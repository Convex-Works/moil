//! Test support for the owner's commands: homes that the real `moil`
//! binary works on, `moil agent` running on them, and a fake service that
//! speaks the service side of the protocol over real HTTP and WebSockets.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use moil_bundle::Bundle;
use moil_proto::control::{BundleInfo, Welcome};
use moil_proto::http::{
    BundlesResponse, ErrorBody, InfoResponse, PairRequest, PairResponse, TokenResponse,
};
use moil_proto::{MachineMessage, ServiceInfo, ServiceMessage};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// How long a test waits for anything before failing.
pub const PATIENCE: Duration = Duration::from_secs(20);

/// The code the fake service asks owners to confirm.
pub const CODE: &str = "WDJB-MJHT";

/// Waits until `condition` holds, checking every few milliseconds, for
/// what moil does without telling: files it writes, processes it ends.
#[track_caller]
pub fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let give_up_at = Instant::now() + PATIENCE;
    while !condition() {
        assert!(Instant::now() < give_up_at, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// uv, for the tests that run a job.
pub fn uv() -> PathBuf {
    moil_runtime::find_uv()
        .or_else(|| {
            let installed = PathBuf::from(std::env::var_os("HOME")?).join(".local/bin/uv");
            installed.is_file().then_some(installed)
        })
        .expect("these tests run real uv: install it, or set MOIL_UV to it")
}

/// A home of its own, for `moil` to work on, keeping tokens in a file.
pub struct Home {
    dir: tempfile::TempDir,
    uv: PathBuf,
}

impl Home {
    /// A home whose agent never gets as far as running a job, so uv can
    /// be any executable.
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            uv: "/usr/bin/false".into(),
        }
    }

    /// A home whose folder's name starts with `name`, or `None` where the
    /// file system refuses it, as APFS refuses names that aren't UTF-8.
    pub fn named(name: &std::ffi::OsStr) -> Option<Self> {
        let dir = tempfile::Builder::new().prefix(name).tempdir().ok()?;
        let uv = "/usr/bin/false".into();
        Some(Self { dir, uv })
    }

    /// A home whose agent runs jobs with real uv.
    pub fn with_uv() -> Self {
        Self {
            uv: uv(),
            ..Self::new()
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `moil` with `args`, working on this home, with nothing on stdin.
    pub fn moil(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_moil"));
        command
            .args(args)
            .env("MOIL_HOME", self.path())
            .env("MOIL_SECRETS", "file")
            .env("MOIL_UV", &self.uv)
            .env("RUST_LOG", "info")
            .env_remove("RUST_BACKTRACE")
            .stdin(Stdio::null());
        command
    }

    /// Runs `moil` with `args` to its end.
    pub fn run(&self, args: &[&str]) -> Run {
        let output = self.moil(args).output().unwrap();
        Run {
            args: args.join(" "),
            output,
        }
    }

    /// Starts `moil agent` on this home, and waits until commands can
    /// reach it.
    pub fn agent(&self) -> Agent {
        let mut agent = Agent::spawn(self);
        agent.logged("moil commands reach this agent");
        agent
    }

    /// Puts a service folder in the home, as a copy or a git clone would:
    /// a `service.json` and nothing else.
    pub fn service_folder(&self, id: &str, base_url: &str, name: &str, approvals: Value) {
        let folder = self.path().join("services").join(id);
        std::fs::create_dir_all(&folder).unwrap();
        let file = serde_json::json!({"base_url": base_url, "name": name, "approvals": approvals});
        std::fs::write(folder.join("service.json"), file.to_string()).unwrap();
    }

    /// Writes an executable hook for `bundle` in service `id`'s folder.
    pub fn hook(&self, id: &str, bundle: &str, text: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.path().join("services").join(id).join("hooks");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(bundle);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Pairs with `service` through `moil pair`, the owner confirming the
    /// code, and returns the service's ID.
    pub fn pair(&self, service: &FakeService, target: &str) -> String {
        let mut pairing = Pairing::start(self, &[target, "--no-browser"]);
        assert_eq!(pairing.next_line(), format!("Code: {CODE}"));
        pairing.next_line();
        service.confirm();
        let paired = pairing.next_line();
        pairing.finish().success();
        paired.strip_prefix("Paired: ").unwrap().to_owned()
    }
}

/// A `moil` command that has run to its end.
pub struct Run {
    args: String,
    pub output: Output,
}

impl Run {
    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    pub fn code(&self) -> Option<i32> {
        self.output.status.code()
    }

    #[track_caller]
    pub fn success(self) -> Self {
        assert!(
            self.output.status.success(),
            "moil {} failed with {}\nstdout:\n{}\nstderr:\n{}",
            self.args,
            self.output.status,
            self.stdout(),
            self.stderr()
        );
        self
    }

    #[track_caller]
    pub fn failure(self, code: i32) -> Self {
        assert_eq!(
            self.code(),
            Some(code),
            "moil {}\nstdout:\n{}\nstderr:\n{}",
            self.args,
            self.stdout(),
            self.stderr()
        );
        self
    }

    /// Stdout, which must be one line of JSON of a report type.
    #[track_caller]
    pub fn json<T: Serialize + DeserializeOwned>(&self) -> T {
        let stdout = self.stdout();
        assert!(
            stdout.ends_with('\n') && stdout.matches('\n').count() == 1,
            "moil {} printed more than one line:\n{stdout}",
            self.args
        );
        parse(stdout.trim_end())
    }
}

/// One line of JSON, which must be exactly a `T`: every field it has is
/// one of `T`'s, and `T` reads every field it has.
#[track_caller]
pub fn parse<T: Serialize + DeserializeOwned>(line: &str) -> T {
    let value: Value = serde_json::from_str(line)
        .unwrap_or_else(|e| panic!("moil printed something that isn't JSON ({e}): {line}"));
    let parsed: T = serde_json::from_value(value.clone())
        .unwrap_or_else(|e| panic!("{line} isn't a {}: {e}", std::any::type_name::<T>()));
    assert_eq!(
        serde_json::to_value(&parsed).unwrap(),
        value,
        "{line} has fields a {} doesn't",
        std::any::type_name::<T>()
    );
    parsed
}

/// The `Label: value` lines in `text`, keyed as their JSON form is
/// (spec/machine.md §3.1): the labels in lowercase, with underscores for
/// spaces.
pub fn labels(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|line| !line.starts_with(' '))
        .filter_map(|line| line.split_once(": "))
        .map(|(label, value)| (label.to_lowercase().replace(' ', "_"), value.to_owned()))
        .collect()
}

/// Checks the Label rule: `json` is an object holding what the `Label:
/// value` lines in `text` say, under the keys the rule gives them.
#[track_caller]
pub fn assert_label_rule(text: &str, json: &str) {
    let json: BTreeMap<String, Value> = serde_json::from_str(json.trim_end())
        .unwrap_or_else(|e| panic!("{json} isn't a JSON object: {e}"));
    let text = labels(text);
    for (key, value) in &json {
        let shown = match value {
            Value::String(value) => value.clone(),
            other => other.to_string(),
        };
        assert_eq!(
            text.get(key),
            Some(&shown),
            "{key} in {json:?} and in the lines {text:?}"
        );
    }
}

/// `moil pair`, running, whose stdout the test reads line by line.
pub struct Pairing {
    child: Child,
    lines: mpsc::Receiver<String>,
    printed: Vec<String>,
}

impl Pairing {
    pub fn start(home: &Home, args: &[&str]) -> Self {
        let args: Vec<&str> = ["pair"].iter().chain(args).copied().collect();
        let mut child = home
            .moil(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            lines: read_lines(child.stdout.take().unwrap()),
            child,
            printed: Vec::new(),
        }
    }

    /// The next line `moil pair` prints on stdout.
    #[track_caller]
    pub fn next_line(&mut self) -> String {
        match self.lines.recv_timeout(PATIENCE) {
            Ok(line) => {
                self.printed.push(line.clone());
                line
            }
            Err(_) => {
                let _ = self.child.kill();
                let printed = self.printed.clone();
                let run = self.finish_ref();
                panic!(
                    "moil pair ended ({}) after printing {printed:?}:\n{}",
                    run.output.status,
                    run.stderr()
                );
            }
        }
    }

    /// Waits for `moil pair` to end.
    pub fn finish(mut self) -> Run {
        self.finish_ref()
    }

    fn finish_ref(&mut self) -> Run {
        use std::io::Read;
        let status = wait(&mut self.child);
        let mut stderr = Vec::new();
        if let Some(mut err) = self.child.stderr.take() {
            err.read_to_end(&mut stderr).unwrap();
        }
        let printed = self.printed.iter().cloned().chain(self.lines.iter());
        let stdout = printed.map(|line| line + "\n").collect::<String>();
        Run {
            args: "pair".into(),
            output: Output {
                status,
                stdout: stdout.into_bytes(),
                stderr,
            },
        }
    }
}

/// The lines `from` gives, as they come.
fn read_lines(from: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (sender, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(from).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    lines
}

/// Waits for `child` to exit, failing the test if it takes too long.
#[track_caller]
pub fn wait(child: &mut Child) -> ExitStatus {
    let give_up_at = Instant::now() + PATIENCE;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= give_up_at {
            let _ = child.kill();
            panic!("the process was still running after {PATIENCE:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A running `moil agent` and what it has logged so far.
pub struct Agent {
    child: Child,
    lines: mpsc::Receiver<String>,
    pub log: Vec<String>,
}

impl Agent {
    /// Starts `moil agent` without waiting for anything.
    pub fn spawn(home: &Home) -> Self {
        let mut child = home
            .moil(&["agent"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            lines: read_lines(child.stderr.take().unwrap()),
            child,
            log: Vec::new(),
        }
    }

    /// Waits until the agent logs a line containing `text`, failing if it
    /// exits first, and returns the line.
    #[track_caller]
    pub fn logged(&mut self, text: &str) -> String {
        if let Some(line) = self.log.iter().find(|line| line.contains(text)) {
            return line.clone();
        }
        loop {
            match self.lines.recv_timeout(PATIENCE) {
                Ok(line) => {
                    self.log.push(line.clone());
                    if line.contains(text) {
                        return line;
                    }
                }
                Err(_) => panic!(
                    "the agent never said {text:?}; it said:\n{}",
                    self.log.join("\n")
                ),
            }
        }
    }

    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Waits for the agent to exit, and returns how it did.
    pub fn exited(&mut self) -> ExitStatus {
        let status = wait(&mut self.child);
        self.log.extend(self.lines.try_iter());
        status
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A fake service mounted at `/moil` on a loopback port: machines pair
/// with it (the test confirming the code), fetch the bundles it hosts, and
/// open control channels, which the test takes with [`accept`] to talk to
/// the machine.
///
/// [`accept`]: FakeService::accept
pub struct FakeService {
    /// The base URL, e.g. `http://127.0.0.1:53211/moil`.
    pub base_url: String,
    shared: Arc<Shared>,
    channels: Mutex<mpsc::Receiver<Channel>>,
    _runtime: tokio::runtime::Runtime,
}

struct Shared {
    name: String,
    base_url: String,
    /// Whether the owner confirmed the pairing waiting to be confirmed.
    confirmed: tokio::sync::watch::Sender<bool>,
    issued: Mutex<u32>,
    tokens: Mutex<HashSet<String>>,
    /// The names machines asked to be called when pairing.
    names: Mutex<Vec<String>>,
    bundles: Mutex<Vec<Bundle>>,
    forgotten: Mutex<Vec<String>>,
    channels: mpsc::Sender<Channel>,
}

impl FakeService {
    pub fn start(name: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let base_url = format!("http://{}/moil", listener.local_addr().unwrap());
        let (channels, accepted) = mpsc::channel();
        let shared = Arc::new(Shared {
            name: name.to_owned(),
            base_url: base_url.clone(),
            confirmed: tokio::sync::watch::Sender::new(false),
            issued: Mutex::new(0),
            tokens: Mutex::default(),
            names: Mutex::default(),
            bundles: Mutex::default(),
            forgotten: Mutex::default(),
            channels,
        });
        let app = Router::new()
            .route("/moil/v1/info", get(info))
            .route("/moil/v1/pair", post(pair))
            .route("/moil/v1/pair/token", post(token))
            .route("/moil/v1/bundles", get(list_bundles))
            .route("/moil/v1/bundles/{hash}", get(fetch_bundle))
            .route("/moil/v1/machine", delete(forget))
            .route("/moil/v1/connect", get(connect))
            .with_state(shared.clone());
        runtime.spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base_url,
            shared,
            channels: Mutex::new(accepted),
            _runtime: runtime,
        }
    }

    /// Serves `bundle` and offers it.
    pub fn host(&self, bundle: &Bundle) {
        self.shared.bundles.lock().unwrap().push(bundle.clone());
    }

    /// The owner confirms the code on the service's page: the pairing
    /// waiting for it completes.
    pub fn confirm(&self) {
        self.shared.confirmed.send_replace(true);
    }

    /// The tokens the service accepts.
    pub fn tokens(&self) -> HashSet<String> {
        self.shared.tokens.lock().unwrap().clone()
    }

    /// The names machines asked to be called, in the order they paired.
    pub fn names(&self) -> Vec<String> {
        self.shared.names.lock().unwrap().clone()
    }

    /// The tokens machines asked the service to forget.
    pub fn forgotten(&self) -> Vec<String> {
        self.shared.forgotten.lock().unwrap().clone()
    }

    /// The next control channel a machine opens, once it has said hello
    /// and the service has welcomed it, offering what it hosts.
    #[track_caller]
    pub fn accept(&self) -> Channel {
        self.channels
            .lock()
            .unwrap()
            .recv_timeout(PATIENCE)
            .expect("the machine to connect")
    }
}

fn error(status: StatusCode, code: &str) -> Response {
    let body = ErrorBody {
        error: code.into(),
        message: None,
    };
    (status, Json(body)).into_response()
}

impl Shared {
    fn authorized(&self, headers: &HeaderMap) -> Option<String> {
        let token = headers
            .get("authorization")?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")?
            .to_owned();
        let tokens = self.tokens.lock().unwrap();
        tokens.contains(&token).then_some(token)
    }

    fn infos(&self) -> Vec<BundleInfo> {
        let bundles = self.bundles.lock().unwrap();
        bundles
            .iter()
            .map(|b| BundleInfo {
                hash: b.hash(),
                name: b.manifest().name.clone(),
                version: b.manifest().version.clone(),
                description: b.manifest().description.clone(),
            })
            .collect()
    }
}

async fn info(State(shared): State<Arc<Shared>>) -> Json<InfoResponse> {
    Json(InfoResponse {
        protocol: 1,
        name: shared.name.clone(),
    })
}

async fn pair(
    State(shared): State<Arc<Shared>>,
    Json(request): Json<PairRequest>,
) -> Json<PairResponse> {
    shared.names.lock().unwrap().push(request.name);
    Json(PairResponse {
        device_code: "device-code".into(),
        user_code: CODE.into(),
        verification_uri: format!("{}/pair", shared.base_url),
        verification_uri_complete: format!("{}/pair?code={CODE}", shared.base_url),
        expires_in: 600,
        interval: 0,
        service: ServiceInfo {
            name: shared.name.clone(),
        },
    })
}

/// Grants a token once the owner has confirmed, waiting a moment for them
/// first, so that the machine, which polls without pause, needn't ask
/// again and again.
async fn token(State(shared): State<Arc<Shared>>) -> Response {
    let mut confirmed = shared.confirmed.subscribe();
    let waited = tokio::time::timeout(Duration::from_secs(2), confirmed.wait_for(|c| *c));
    // What `wait_for` returns holds the value's lock, until dropped here.
    if !waited.await.is_ok_and(|confirmed| confirmed.is_ok()) {
        return error(StatusCode::BAD_REQUEST, "authorization_pending");
    }
    shared.confirmed.send_replace(false);
    let issued = {
        let mut issued = shared.issued.lock().unwrap();
        *issued += 1;
        *issued
    };
    let token = format!("moil_token_{issued}");
    shared.tokens.lock().unwrap().insert(token.clone());
    Json(TokenResponse {
        machine_id: format!("m_{issued}"),
        token,
        service: ServiceInfo {
            name: shared.name.clone(),
        },
    })
    .into_response()
}

async fn list_bundles(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    if shared.authorized(&headers).is_none() {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    Json(BundlesResponse {
        bundles: shared.infos(),
    })
    .into_response()
}

async fn fetch_bundle(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    UrlPath(hash): UrlPath<String>,
) -> Response {
    if shared.authorized(&headers).is_none() {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let bundles = shared.bundles.lock().unwrap();
    match bundles.iter().find(|b| b.hash().to_string() == hash) {
        Some(bundle) => Json(bundle.to_response()).into_response(),
        None => error(StatusCode::NOT_FOUND, "not_found"),
    }
}

async fn forget(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let Some(token) = shared.authorized(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    shared.tokens.lock().unwrap().remove(&token);
    shared.forgotten.lock().unwrap().push(token);
    StatusCode::NO_CONTENT.into_response()
}

async fn connect(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(token) = shared.authorized(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    upgrade.on_upgrade(move |socket| relay(socket, shared, token))
}

/// Welcomes the machine, then hands the channel to the test.
async fn relay(socket: WebSocket, shared: Arc<Shared>, token: String) {
    let (mut sink, mut stream) = socket.split();
    // The machine says hello first.
    let Some(Ok(Message::Text(hello))) = stream.next().await else {
        return;
    };
    let welcome = ServiceMessage::Welcome(Welcome {
        protocol: 1,
        machine_id: format!("m_{}", token.trim_start_matches("moil_token_")),
        service: ServiceInfo {
            name: shared.name.clone(),
        },
        bundles: shared.infos(),
        resume: Vec::new(),
    });
    let text = serde_json::to_string(&welcome).unwrap();
    if sink.send(Message::Text(text.into())).await.is_err() {
        return;
    }
    let (from_machine, received) = mpsc::channel();
    let hello = serde_json::from_str::<MachineMessage>(&hello)
        .unwrap_or_else(|e| panic!("the machine said hello with {hello}: {e}"));
    let _ = from_machine.send(hello);
    let (to_machine, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<String>();
    let _ = shared.channels.send(Channel {
        received,
        to_machine,
    });
    loop {
        tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    let message = serde_json::from_str::<MachineMessage>(&text)
                        .unwrap_or_else(|e| panic!("the machine sent {text}: {e}"));
                    let _ = from_machine.send(message);
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
            out = outgoing.recv() => match out {
                Some(text) => {
                    let _ = sink.send(Message::Text(text.into())).await;
                }
                None => return,
            },
        }
    }
}

/// A machine's control channel, from the service's side: what the machine
/// sends, from its hello on, and what the test sends it after the welcome.
pub struct Channel {
    received: mpsc::Receiver<MachineMessage>,
    to_machine: tokio::sync::mpsc::UnboundedSender<String>,
}

impl Channel {
    pub fn send(&self, message: &ServiceMessage) {
        let _ = self
            .to_machine
            .send(serde_json::to_string(message).unwrap());
    }

    /// The next message the machine sends for which `wanted` holds,
    /// passing over the others.
    #[track_caller]
    pub fn received(
        &self,
        what: &str,
        mut wanted: impl FnMut(&MachineMessage) -> bool,
    ) -> MachineMessage {
        let give_up_at = Instant::now() + PATIENCE;
        loop {
            let left = give_up_at.saturating_duration_since(Instant::now());
            match self.received.recv_timeout(left) {
                Ok(message) if wanted(&message) => return message,
                Ok(_) => {}
                Err(_) => panic!("the machine never sent {what}"),
            }
        }
    }
}

/// A valid bundle with no dependencies; `body` is its script's code, and
/// makes versions differ.
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
