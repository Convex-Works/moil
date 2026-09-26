//! Test support: a fake service that speaks the service side of the spec
//! over real HTTP and WebSockets, a scripted executor, and helpers.
//!
//! Tests script each conversation: they accept the machine's connections,
//! read exactly what it sends, and answer as a service would.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use moil_agent::{AgentHandle, FileSecrets, Home, Machine, MachineConfig, PairedService, Snapshot};
use moil_bundle::Bundle;
use moil_proto::control::{BundleInfo, Hello, Resume, Welcome};
use moil_proto::http::{
    BundleResponse, BundlesResponse, ErrorBody, InfoResponse, PairRequest, PairResponse,
    TokenRequest, TokenResponse,
};
use moil_proto::{
    Assign, AttemptRef, BundleHash, ErrorCode, JobError, JobEvent, MachineMessage, Offer, Outcome,
    ServiceInfo, ServiceMessage,
};
use moil_runtime::{EventSink, Executor, Job, StopReason, StopSignal};
use tokio::sync::{mpsc, oneshot};

/// How long a test waits for anything before failing.
pub const PATIENCE: Duration = Duration::from_secs(10);

/// Awaits `future`, failing the test if it takes longer than [`PATIENCE`].
pub async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(PATIENCE, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Opens the machine whose home is `dir`, keeping tokens in a file there.
pub fn open_machine(dir: &Path) -> Machine {
    Machine::open(MachineConfig {
        home: Home::new(dir),
        secrets: Arc::new(FileSecrets::new(dir.join("secrets.json"))),
        app_version: "0.1.0-test".into(),
    })
    .unwrap()
}

/// Pairs `machine` with `service`, the owner confirming at once.
pub async fn pair(machine: &Machine, service: &FakeService) -> PairedService {
    service.script_pairing([TokenReply::Grant]);
    let session = machine.pair(&service.base_url).await.unwrap();
    within("pairing", session.wait()).await.unwrap()
}

/// Waits until the agent's snapshot satisfies `condition`.
pub async fn snapshot_where(
    handle: &AgentHandle,
    what: &str,
    condition: impl FnMut(&Snapshot) -> bool,
) -> Snapshot {
    let mut snapshots = handle.subscribe();
    within(what, snapshots.wait_for(condition))
        .await
        .expect("the agent is running")
        .clone()
}

/// A valid bundle; `body` makes versions differ.
pub fn bundle(name: &str, version: &str, body: &str) -> Bundle {
    let script = format!(
        "# /// script\n# requires-python = \">=3.12\"\n# dependencies = []\n# ///\n{body}\n"
    );
    let lockfile = "version = 1\nrevision = 3\nrequires-python = \">=3.12\"\n";
    let manifest = format!(
        "{{\"moil\": 1, \"name\": \"{name}\", \"version\": \"{version}\", \"description\": \"A test bundle.\"}}"
    );
    Bundle::new(script, lockfile, manifest).unwrap()
}

/// How a service lists `bundle`.
pub fn info(bundle: &Bundle) -> BundleInfo {
    BundleInfo {
        hash: bundle.hash(),
        name: bundle.manifest().name.clone(),
        version: bundle.manifest().version.clone(),
        description: bundle.manifest().description.clone(),
    }
}

pub fn attempt(job: &str, attempt: u32) -> AttemptRef {
    AttemptRef {
        job_id: job.parse().unwrap(),
        attempt,
    }
}

pub fn offer(job: &str, bundle: BundleHash) -> ServiceMessage {
    ServiceMessage::Offer(Offer {
        job_id: job.parse().unwrap(),
        bundle_hash: bundle,
        title: None,
        expires_ms: 2000,
    })
}

pub fn assign(job: &str, attempt: u32, bundle: BundleHash, lease_ms: u64) -> ServiceMessage {
    ServiceMessage::Assign(Assign {
        job_id: job.parse().unwrap(),
        attempt,
        bundle_hash: bundle,
        title: Some(format!("{job} title")),
        params: serde_json::json!({"language": "en"}),
        inputs: Default::default(),
        outputs: Default::default(),
        lease_ms,
        timeout_ms: None,
    })
}

pub fn progress(fraction: f64, message: &str) -> JobEvent {
    JobEvent::Progress {
        fraction: Some(fraction),
        message: Some(message.into()),
    }
}

pub fn succeeded() -> Outcome {
    Outcome::Succeeded {
        result: Default::default(),
    }
}

pub fn resume(job: &str, attempt: u32, seq: u64) -> Resume {
    Resume {
        job_id: job.parse().unwrap(),
        attempt,
        seq,
    }
}

/// What the fake service answers to one token poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenReply {
    Pending,
    SlowDown,
    Denied,
    Expired,
    /// The owner confirmed: issue a new machine ID and token.
    Grant,
    /// A 503, as from an overloaded service.
    Unavailable,
}

/// What `GET /v1/info` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InfoReply {
    Moil,
    Protocol(u32),
    NotFound,
    Html,
}

/// A fake service mounted at `/moil`, on a loopback port.
pub struct FakeService {
    /// The base URL, e.g. `http://127.0.0.1:53211/moil`.
    pub base_url: String,
    shared: Arc<Shared>,
    connections: tokio::sync::Mutex<mpsc::UnboundedReceiver<Conn>>,
}

struct Shared {
    name: String,
    base_url: String,
    info: Mutex<InfoReply>,
    pairing: Mutex<Pairing>,
    tokens: Mutex<HashSet<String>>,
    bundles: Mutex<HashMap<BundleHash, BundleResponse>>,
    listed: Mutex<Vec<BundleInfo>>,
    forgotten: Mutex<Vec<String>>,
    refuse_upgrades: Mutex<Option<StatusCode>>,
    /// Open channels, by token, for closing them from the service's side.
    open: Mutex<Vec<(String, mpsc::UnboundedSender<Out>)>>,
    connections: mpsc::UnboundedSender<Conn>,
}

struct Pairing {
    interval: u64,
    expires_in: u64,
    replies: VecDeque<TokenReply>,
    requests: Vec<PairRequest>,
    polls: Vec<Instant>,
    issued: u32,
}

impl FakeService {
    pub async fn start(name: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/moil", listener.local_addr().unwrap());
        let (connections, accepted) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            name: name.to_owned(),
            base_url: base_url.clone(),
            info: Mutex::new(InfoReply::Moil),
            pairing: Mutex::new(Pairing {
                interval: 0,
                expires_in: 600,
                replies: VecDeque::new(),
                requests: Vec::new(),
                polls: Vec::new(),
                issued: 0,
            }),
            tokens: Mutex::default(),
            bundles: Mutex::default(),
            listed: Mutex::default(),
            forgotten: Mutex::default(),
            refuse_upgrades: Mutex::default(),
            open: Mutex::default(),
            connections,
        });
        let app = Router::new()
            .route("/moil/v1/info", get(info_handler))
            .route("/moil/v1/pair", post(pair_handler))
            .route("/moil/v1/pair/token", post(token_handler))
            .route("/moil/v1/bundles", get(bundles_handler))
            .route("/moil/v1/bundles/{hash}", get(bundle_handler))
            .route("/moil/v1/machine", delete(forget_handler))
            .route("/moil/v1/connect", get(connect_handler))
            .with_state(shared.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base_url,
            shared,
            connections: tokio::sync::Mutex::new(accepted),
        }
    }

    /// The next control channel a machine opens.
    pub async fn accept(&self) -> Conn {
        let mut connections = self.connections.lock().await;
        within("the machine to connect", connections.recv())
            .await
            .expect("the service is running")
    }

    /// Whether a machine opened a channel that no test accepted yet.
    pub fn has_pending_connection(&self) -> bool {
        self.connections
            .try_lock()
            .is_ok_and(|connections| !connections.is_empty())
    }

    /// Answers the next token polls with `replies`, then with
    /// `authorization_pending`.
    pub fn script_pairing(&self, replies: impl IntoIterator<Item = TokenReply>) {
        self.shared.pairing.lock().unwrap().replies.extend(replies);
    }

    /// Sets the `interval` and `expires_in` of the next pairing, in seconds.
    pub fn pairing_timing(&self, interval: u64, expires_in: u64) {
        let mut pairing = self.shared.pairing.lock().unwrap();
        pairing.interval = interval;
        pairing.expires_in = expires_in;
    }

    pub fn pair_requests(&self) -> Vec<PairRequest> {
        self.shared.pairing.lock().unwrap().requests.clone()
    }

    /// When each token poll arrived.
    pub fn polls(&self) -> Vec<Instant> {
        self.shared.pairing.lock().unwrap().polls.clone()
    }

    pub fn set_info(&self, reply: InfoReply) {
        *self.shared.info.lock().unwrap() = reply;
    }

    /// Serves `bundle` and lists it.
    pub fn host(&self, bundle: &Bundle) {
        self.serve_bundle(bundle.hash(), bundle.to_response());
        self.shared.listed.lock().unwrap().push(info(bundle));
    }

    /// Answers `GET /v1/bundles/{hash}` with `response`, whatever it holds.
    pub fn serve_bundle(&self, hash: BundleHash, response: BundleResponse) {
        self.shared.bundles.lock().unwrap().insert(hash, response);
    }

    /// Tokens machines sent with `DELETE /v1/machine`.
    pub fn forgotten(&self) -> Vec<String> {
        self.shared.forgotten.lock().unwrap().clone()
    }

    /// Tokens the service currently accepts.
    pub fn tokens(&self) -> HashSet<String> {
        self.shared.tokens.lock().unwrap().clone()
    }

    /// Removes a machine, as its owner would on the service: revokes the
    /// token and closes its channels with 4401.
    pub fn remove_machine(&self, token: &str) {
        self.shared.forget(token);
    }

    /// Answers upgrade requests with `status` instead of opening channels.
    pub fn refuse_upgrades(&self, status: Option<StatusCode>) {
        *self.shared.refuse_upgrades.lock().unwrap() = status;
    }
}

impl Shared {
    fn forget(&self, token: &str) {
        self.tokens.lock().unwrap().remove(token);
        let mut open = self.open.lock().unwrap();
        for (_, channel) in open.iter().filter(|(t, _)| t == token) {
            let _ = channel.send(Out::Close(4401));
        }
        open.retain(|(t, _)| t != token);
    }

    fn authorized(&self, headers: &HeaderMap) -> Option<String> {
        let token = headers
            .get("authorization")?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")?
            .to_owned();
        self.tokens
            .lock()
            .unwrap()
            .contains(&token)
            .then_some(token)
    }
}

fn error(status: StatusCode, code: &str) -> Response {
    let body = ErrorBody {
        error: code.into(),
        message: None,
    };
    (status, Json(body)).into_response()
}

async fn info_handler(State(shared): State<Arc<Shared>>) -> Response {
    let reply = shared.info.lock().unwrap().clone();
    match reply {
        InfoReply::Moil => Json(InfoResponse {
            protocol: 1,
            name: shared.name.clone(),
        })
        .into_response(),
        InfoReply::Protocol(protocol) => Json(InfoResponse {
            protocol,
            name: shared.name.clone(),
        })
        .into_response(),
        InfoReply::NotFound => StatusCode::NOT_FOUND.into_response(),
        InfoReply::Html => "<html>hello</html>".into_response(),
    }
}

async fn pair_handler(
    State(shared): State<Arc<Shared>>,
    Json(request): Json<PairRequest>,
) -> Json<PairResponse> {
    let mut pairing = shared.pairing.lock().unwrap();
    pairing.requests.push(request);
    Json(PairResponse {
        device_code: "device-code-1".into(),
        user_code: "WDJB-MJHT".into(),
        verification_uri: format!("{}/pair", shared.base_url),
        verification_uri_complete: format!("{}/pair?code=WDJB-MJHT", shared.base_url),
        expires_in: pairing.expires_in,
        interval: pairing.interval,
        service: ServiceInfo {
            name: shared.name.clone(),
        },
    })
}

async fn token_handler(
    State(shared): State<Arc<Shared>>,
    Json(request): Json<TokenRequest>,
) -> Response {
    assert_eq!(request.device_code, "device-code-1");
    let mut pairing = shared.pairing.lock().unwrap();
    pairing.polls.push(Instant::now());
    let reply = pairing.replies.pop_front().unwrap_or(TokenReply::Pending);
    let bad_request = |code| error(StatusCode::BAD_REQUEST, code);
    match reply {
        TokenReply::Pending => bad_request("authorization_pending"),
        TokenReply::SlowDown => bad_request("slow_down"),
        TokenReply::Denied => bad_request("access_denied"),
        TokenReply::Expired => bad_request("expired_token"),
        TokenReply::Unavailable => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        TokenReply::Grant => {
            pairing.issued += 1;
            let token = format!("moil_token_{}", pairing.issued);
            shared.tokens.lock().unwrap().insert(token.clone());
            Json(TokenResponse {
                machine_id: format!("m_{}", pairing.issued),
                token,
                service: ServiceInfo {
                    name: shared.name.clone(),
                },
            })
            .into_response()
        }
    }
}

async fn bundles_handler(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    if shared.authorized(&headers).is_none() {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let bundles = shared.listed.lock().unwrap().clone();
    Json(BundlesResponse { bundles }).into_response()
}

async fn bundle_handler(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    UrlPath(hash): UrlPath<String>,
) -> Response {
    if shared.authorized(&headers).is_none() {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let found = hash
        .parse()
        .ok()
        .and_then(|hash| shared.bundles.lock().unwrap().get(&hash).cloned());
    match found {
        Some(response) => Json(response).into_response(),
        None => error(StatusCode::NOT_FOUND, "not_found"),
    }
}

async fn forget_handler(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let Some(token) = shared.authorized(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    shared.forgotten.lock().unwrap().push(token.clone());
    shared.forget(&token);
    StatusCode::NO_CONTENT.into_response()
}

async fn connect_handler(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Some(status) = *shared.refuse_upgrades.lock().unwrap() {
        return status.into_response();
    }
    let Some(token) = shared.authorized(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    upgrade.on_upgrade(move |socket| relay(socket, shared, token))
}

/// Something the machine did on a channel.
enum Frame {
    Message(MachineMessage),
    Invalid(String),
    Closed(Option<u16>),
}

/// Something the test makes the service do on a channel.
enum Out {
    Text(String),
    Ping,
    Close(u16),
    /// Drop the connection without a close frame.
    Drop,
}

async fn relay(socket: WebSocket, shared: Arc<Shared>, token: String) {
    let (frames, from_machine) = mpsc::unbounded_channel();
    let (to_machine, mut outgoing) = mpsc::unbounded_channel();
    shared
        .open
        .lock()
        .unwrap()
        .push((token.clone(), to_machine.clone()));
    let _ = shared.connections.send(Conn {
        token,
        from_machine,
        to_machine,
    });
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    let frame = match serde_json::from_str::<MachineMessage>(&text) {
                        Ok(MachineMessage::Unknown) => Frame::Invalid(format!("unknown type: {text}")),
                        Ok(message) => Frame::Message(message),
                        Err(e) => Frame::Invalid(format!("{e}: {text}")),
                    };
                    let _ = frames.send(frame);
                }
                Some(Ok(Message::Close(frame))) => {
                    let _ = frames.send(Frame::Closed(frame.map(|f| f.code)));
                    return;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => {
                    let _ = frames.send(Frame::Closed(None));
                    return;
                }
            },
            out = outgoing.recv() => match out {
                Some(Out::Text(text)) => {
                    let _ = sink.send(Message::Text(text.into())).await;
                }
                Some(Out::Ping) => {
                    let _ = sink.send(Message::Ping(Default::default())).await;
                }
                Some(Out::Close(code)) => {
                    let close = CloseFrame { code, reason: "".into() };
                    let _ = sink.send(Message::Close(Some(close))).await;
                    let _ = frames.send(Frame::Closed(Some(code)));
                    return;
                }
                Some(Out::Drop) | None => return,
            },
        }
    }
}

/// One control channel, from the service's side.
pub struct Conn {
    /// The token the machine connected with.
    pub token: String,
    from_machine: mpsc::UnboundedReceiver<Frame>,
    to_machine: mpsc::UnboundedSender<Out>,
}

impl Conn {
    /// The next message the machine sends.
    pub async fn recv(&mut self) -> MachineMessage {
        match within("a message from the machine", self.from_machine.recv()).await {
            Some(Frame::Message(message)) => message,
            Some(Frame::Invalid(problem)) => {
                panic!("the machine sent an invalid message: {problem}")
            }
            Some(Frame::Closed(code)) => panic!("the channel closed ({code:?}) instead"),
            None => panic!("the channel is gone"),
        }
    }

    /// The machine's `hello`, which must come first.
    pub async fn hello(&mut self) -> Hello {
        match self.recv().await {
            MachineMessage::Hello(hello) => hello,
            other => panic!("expected hello, got {other:?}"),
        }
    }

    /// Reads `hello` and answers `welcome` with the given bundles and
    /// resumed attempts.
    pub async fn handshake(&mut self, bundles: Vec<BundleInfo>, resume: Vec<Resume>) -> Hello {
        let hello = self.hello().await;
        self.send(ServiceMessage::Welcome(Welcome {
            protocol: 1,
            machine_id: "m_1".into(),
            service: ServiceInfo {
                name: "fake".into(),
            },
            bundles,
            resume,
        }));
        hello
    }

    pub fn send(&self, message: ServiceMessage) {
        self.send_text(&serde_json::to_string(&message).unwrap());
    }

    pub fn send_text(&self, text: &str) {
        let _ = self.to_machine.send(Out::Text(text.to_owned()));
    }

    pub fn ping(&self) {
        let _ = self.to_machine.send(Out::Ping);
    }

    /// Closes the channel with a close code, as the service.
    pub fn close(&self, code: u16) {
        let _ = self.to_machine.send(Out::Close(code));
    }

    /// Drops the connection without a close frame, as a crashed service
    /// or a broken network would.
    pub fn drop_connection(self) {
        let _ = self.to_machine.send(Out::Drop);
    }

    /// Waits for the machine to close the channel, failing if it sends a
    /// message first. Returns the close code.
    pub async fn closed(&mut self) -> Option<u16> {
        match within("the machine to close the channel", self.from_machine.recv()).await {
            Some(Frame::Closed(code)) => code,
            Some(Frame::Message(message)) => {
                panic!("expected the channel to close, got {message:?}")
            }
            Some(Frame::Invalid(problem)) => {
                panic!("the machine sent an invalid message: {problem}")
            }
            None => None,
        }
    }
}

/// An [`Executor`] that tests drive: each run shows up in
/// [`FakeRuns::next`], where the test emits events and ends it.
#[derive(Debug)]
pub struct FakeExecutor {
    runs: mpsc::UnboundedSender<FakeRun>,
    prepared: mpsc::UnboundedSender<BundleHash>,
}

/// The test's end of a [`FakeExecutor`].
pub struct FakeRuns {
    runs: mpsc::UnboundedReceiver<FakeRun>,
    prepared: mpsc::UnboundedReceiver<BundleHash>,
}

pub fn fake_executor() -> (Arc<FakeExecutor>, FakeRuns) {
    let (runs, runs_rx) = mpsc::unbounded_channel();
    let (prepared, prepared_rx) = mpsc::unbounded_channel();
    (
        Arc::new(FakeExecutor { runs, prepared }),
        FakeRuns {
            runs: runs_rx,
            prepared: prepared_rx,
        },
    )
}

impl FakeRuns {
    /// The next attempt the agent runs.
    pub async fn next(&mut self) -> FakeRun {
        within("the agent to run an attempt", self.runs.recv())
            .await
            .expect("the executor is alive")
    }

    /// Whether the agent started an attempt that no test picked up.
    pub fn has_pending_run(&self) -> bool {
        !self.runs.is_empty()
    }

    /// The next bundle the agent prepares.
    pub async fn prepared(&mut self) -> BundleHash {
        within("the agent to prepare a bundle", self.prepared.recv())
            .await
            .expect("the executor is alive")
    }
}

/// One attempt, as the executor sees it.
pub struct FakeRun {
    pub job: Job,
    events: EventSink,
    stop: StopSignal,
    finish: Option<oneshot::Sender<Outcome>>,
}

impl FakeRun {
    pub fn emit(&self, event: JobEvent) {
        self.events.send(event).unwrap();
    }

    /// Ends the attempt with `outcome`.
    pub fn finish(mut self, outcome: Outcome) {
        let _ = self.finish.take().unwrap().send(outcome);
    }

    /// Waits until the agent asks the attempt to stop, and says why. The
    /// run then ends as a real runtime's would.
    pub async fn stopped(&mut self) -> StopReason {
        within("the agent to stop the attempt", self.stop.stopped()).await
    }
}

impl Executor for FakeExecutor {
    fn prepare<'a>(&'a self, bundle: &'a Bundle) -> BoxFuture<'a, Result<(), JobError>> {
        let _ = self.prepared.send(bundle.hash());
        Box::pin(async { Ok(()) })
    }

    fn run(&self, job: Job, events: EventSink, stop: StopSignal) -> BoxFuture<'_, Outcome> {
        let (finish, finished) = oneshot::channel();
        let mut stopped = stop.clone();
        let _ = self.runs.send(FakeRun {
            job,
            events,
            stop,
            finish: Some(finish),
        });
        Box::pin(async move {
            tokio::select! {
                outcome = finished => outcome.unwrap_or_else(|_| {
                    Outcome::failed(JobError::new(ErrorCode::Internal, "the test dropped the run"))
                }),
                reason = stopped.stopped() => match reason {
                    StopReason::Cancelled => Outcome::Cancelled,
                    StopReason::Interrupted => Outcome::failed(JobError::new(
                        ErrorCode::Interrupted,
                        "stopped by the owner",
                    )),
                },
            }
        })
    }
}
