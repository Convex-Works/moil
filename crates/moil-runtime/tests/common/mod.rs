//! What the runtime's integration tests share: a real HTTP and HTTPS server
//! for inputs, outputs and assets, test bundles, and a way to run attempts
//! and look at what they did.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use moil_bundle::Bundle;
use moil_proto::{
    FileName, JobError, JobEvent, JobId, JobResult, LogLevel, Outcome, Phase, Upload, UploadMethod,
    UrlPolicy,
};
use moil_runtime::{Executor, Job, Runtime, RuntimeConfig, StopReason, Stopper, stop_signal};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;

/// The uv these tests run: `find_uv`, or where uv's installer puts it.
pub fn uv() -> PathBuf {
    moil_runtime::find_uv()
        .or_else(|| {
            let installed = PathBuf::from(std::env::var_os("HOME")?).join(".local/bin/uv");
            installed.is_file().then_some(installed)
        })
        .expect("these tests run real uv: install it, or set MOIL_UV to it")
}

/// A uv cache under `target/`: every test home stores its bundles at a new
/// path, and uv keeps an environment per script path, which shouldn't pile
/// up in the developer's own cache. Python installs are shared regardless.
pub fn uv_cache_env() -> Vec<(OsString, OsString)> {
    let cache = Path::new(env!("CARGO_TARGET_TMPDIR")).join("uv-cache");
    vec![("UV_CACHE_DIR".into(), cache.into())]
}

pub fn bundle_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/bundles")
        .join(name)
}

/// The bundle in `tests/bundles/<name>`.
pub fn bundle(name: &str) -> Bundle {
    Bundle::from_dir(bundle_dir(name)).unwrap()
}

/// The probe bundle: does whatever its `steps` say (see its job.py).
pub fn probe() -> Bundle {
    bundle("probe")
}

/// The probe's script with a different manifest, e.g. one with assets.
pub fn probe_with_manifest(manifest: Value) -> Bundle {
    let probe = probe();
    Bundle::new(probe.script(), probe.lockfile(), manifest.to_string()).unwrap()
}

/// Params that make the probe take `steps`.
pub fn steps(steps: Value) -> Value {
    json!({ "steps": steps })
}

pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A runtime in a fresh home, next to a server it can reach.
pub struct Harness {
    pub home: tempfile::TempDir,
    pub runtime: Arc<Runtime>,
    pub server: Server,
}

impl Harness {
    pub async fn new() -> Self {
        Self::with(|_| {}).await
    }

    /// A harness whose runtime config `configure` adjusts. The server's CA
    /// is trusted and stopped scripts get half a second of grace unless
    /// `configure` says otherwise.
    pub async fn with(configure: impl FnOnce(&mut RuntimeConfig)) -> Self {
        let server = Server::start().await;
        let home = tempfile::tempdir().unwrap();
        let mut config = RuntimeConfig::new(uv(), home.path());
        config.kill_grace = Duration::from_millis(500);
        config.extra_root_certificates = vec![server.ca_pem.clone()];
        config.extra_env = uv_cache_env();
        configure(&mut config);
        let runtime = Arc::new(Runtime::try_new(config).unwrap());
        Self {
            home,
            runtime,
            server,
        }
    }

    /// An attempt of a new job running `bundle` with `params`, without
    /// inputs, outputs or a time limit.
    pub fn job(&self, bundle: &Bundle, params: Value) -> Job {
        static NEXT: AtomicU32 = AtomicU32::new(1);
        Job {
            id: JobId::new(format!("j{}", NEXT.fetch_add(1, Ordering::Relaxed))).unwrap(),
            attempt: 1,
            bundle: bundle.clone(),
            params,
            inputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            timeout: None,
            url_policy: UrlPolicy {
                loopback_http: true,
                any_http: false,
            },
        }
    }

    /// Runs `job` to its end.
    pub async fn run(&self, job: Job) -> Run {
        self.start(job).finish().await
    }

    /// Starts `job`, to watch and steer it while it runs.
    pub fn start(&self, job: Job) -> Running {
        let (tx, events) = mpsc::unbounded_channel();
        let (stopper, stop) = stop_signal();
        let runtime = self.runtime.clone();
        let work_dir = runtime.work_dir(&job.id, job.attempt);
        let task = tokio::spawn(async move { runtime.run(job, tx, stop).await });
        Running {
            events,
            seen: Vec::new(),
            stopper,
            task,
            work_dir,
        }
    }

    pub fn jobs_dir(&self) -> PathBuf {
        self.runtime.config().jobs_dir.clone()
    }
}

/// An attempt in progress.
pub struct Running {
    events: mpsc::UnboundedReceiver<JobEvent>,
    seen: Vec<JobEvent>,
    pub stopper: Stopper,
    task: tokio::task::JoinHandle<Outcome>,
    pub work_dir: PathBuf,
}

impl Running {
    /// Waits for the script to send a `data` event whose payload has `key`,
    /// and returns that value.
    pub async fn data(&mut self, key: &str) -> Value {
        loop {
            let event = self
                .events
                .recv()
                .await
                .unwrap_or_else(|| panic!("the attempt ended without sending data {key:?}"));
            self.seen.push(event.clone());
            if let JobEvent::Data { payload } = event
                && let Some(value) = payload.get(key)
            {
                return value.clone();
            }
        }
    }

    /// Waits for `event`.
    pub async fn event(&mut self, event: JobEvent) {
        loop {
            let next = self
                .events
                .recv()
                .await
                .unwrap_or_else(|| panic!("the attempt ended before {event:?}"));
            self.seen.push(next.clone());
            if next == event {
                return;
            }
        }
    }

    /// Waits for the attempt to reach `phase`.
    pub async fn phase(&mut self, phase: Phase) {
        self.event(JobEvent::Phase { phase }).await;
    }

    /// Waits for a `log` event at level `info` with `message`.
    pub async fn log(&mut self, message: &str) {
        self.event(JobEvent::log(LogLevel::Info, message)).await;
    }

    pub fn stop(&self, reason: StopReason) {
        self.stopper.stop(reason);
    }

    pub async fn finish(mut self) -> Run {
        let outcome = self.task.await.unwrap();
        while let Ok(event) = self.events.try_recv() {
            self.seen.push(event);
        }
        Run {
            outcome,
            events: self.seen,
        }
    }
}

/// A finished attempt.
#[derive(Debug)]
pub struct Run {
    pub outcome: Outcome,
    pub events: Vec<JobEvent>,
}

impl Run {
    #[track_caller]
    pub fn result(&self) -> &JobResult {
        match &self.outcome {
            Outcome::Succeeded { result } => result,
            other => panic!(
                "expected success, got {other:?}\nevents: {:#?}",
                self.events
            ),
        }
    }

    #[track_caller]
    pub fn error(&self) -> &JobError {
        match &self.outcome {
            Outcome::Failed { error } => error,
            other => panic!(
                "expected failure, got {other:?}\nevents: {:#?}",
                self.events
            ),
        }
    }

    pub fn phases(&self) -> Vec<Phase> {
        self.events
            .iter()
            .filter_map(|e| match e {
                JobEvent::Phase { phase } => Some(*phase),
                _ => None,
            })
            .collect()
    }

    pub fn logs(&self, level: LogLevel) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                JobEvent::Log { level: l, message } if *l == level => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// The value under `key` in the first `data` payload that has it.
    #[track_caller]
    pub fn data(&self, key: &str) -> Value {
        self.events
            .iter()
            .find_map(|e| match e {
                JobEvent::Data { payload } => payload.get(key).cloned(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no data {key:?} in {:#?}", self.events))
    }

    /// The events the script sent, without the runtime's phases.
    pub fn script_events(&self) -> Vec<JobEvent> {
        self.events
            .iter()
            .filter(|e| !matches!(e, JobEvent::Phase { .. }))
            .cloned()
            .collect()
    }
}

/// Whether process `pid` still exists (zombies included).
pub fn process_exists(pid: i32) -> bool {
    // SAFETY: kill with signal 0 only checks for the process.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Waits until process `pid` is gone. Processes killed by the runtime are
/// reaped by init, which takes a moment.
pub async fn gone(pid: i32) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while process_exists(pid) {
        if tokio::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// Whether anything is left in `dir`.
pub fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).map_or(true, |mut entries| entries.next().is_none())
}

/// How the server answers requests for one path.
#[derive(Debug, Clone)]
pub enum Reply {
    /// `200` with this body.
    Body(Vec<u8>),
    /// This status, with a short body.
    Status(u16),
    /// `302` to this URL.
    Redirect(String),
    /// Never answers.
    Hang,
}

/// A request the server received.
#[derive(Debug, Clone)]
pub struct Received {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Received {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

#[derive(Default)]
struct ServerState {
    replies: Mutex<HashMap<String, Reply>>,
    received: Mutex<Vec<Received>>,
}

/// An HTTP and an HTTPS server on loopback, answering the same routes. By
/// default `GET` gets `404` and anything else `200`, so uploads succeed.
/// The HTTPS side uses a certificate from a throwaway CA, [`Server::ca_pem`].
pub struct Server {
    http: SocketAddr,
    https: SocketAddr,
    pub ca_pem: String,
    state: Arc<ServerState>,
}

impl Server {
    pub async fn start() -> Self {
        let state = Arc::new(ServerState::default());
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(state.clone());

        let plain = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = plain.local_addr().unwrap();
        tokio::spawn(axum::serve(plain, app.clone()).into_future());

        let (ca_pem, tls) = tls_config();
        let secure = TlsListener {
            tcp: TcpListener::bind("127.0.0.1:0").await.unwrap(),
            acceptor: TlsAcceptor::from(Arc::new(tls)),
        };
        let https = secure.tcp.local_addr().unwrap();
        tokio::spawn(axum::serve(secure, app).into_future());

        Self {
            http,
            https,
            ca_pem,
            state,
        }
    }

    /// `http://127.0.0.1:<port><path>`
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.http)
    }

    /// `https://127.0.0.1:<port><path>`
    pub fn https_url(&self, path: &str) -> String {
        format!("https://{}{path}", self.https)
    }

    pub fn reply(&self, path: &str, reply: Reply) {
        self.state
            .replies
            .lock()
            .unwrap()
            .insert(path.to_owned(), reply);
    }

    /// Serves `body` at `path`.
    pub fn serve(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.reply(path, Reply::Body(body.into()));
    }

    /// Every request received for `path`, in order.
    pub fn received(&self, path: &str) -> Vec<Received> {
        self.state
            .received
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == path)
            .cloned()
            .collect()
    }

    /// An upload target at `path` on the plain HTTP side.
    pub fn upload(&self, path: &str) -> Upload {
        Upload {
            url: self.url(path),
            method: UploadMethod::Put,
            headers: BTreeMap::new(),
        }
    }
}

async fn handle(
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_owned();
    state.received.lock().unwrap().push(Received {
        method: method.clone(),
        path: path.clone(),
        headers,
        body: body.to_vec(),
    });
    let reply = state.replies.lock().unwrap().get(&path).cloned();
    match reply {
        Some(Reply::Body(body)) => Body::from(body).into_response(),
        Some(Reply::Status(code)) => {
            (StatusCode::from_u16(code).unwrap(), "no luck").into_response()
        }
        Some(Reply::Redirect(to)) => {
            (StatusCode::FOUND, [(axum::http::header::LOCATION, to)]).into_response()
        }
        Some(Reply::Hang) => std::future::pending().await,
        None if method == Method::GET => StatusCode::NOT_FOUND.into_response(),
        None => StatusCode::OK.into_response(),
    }
}

/// A CA made up for this test run, and a server config with a certificate
/// it issued for `127.0.0.1` and `localhost`.
fn tls_config() -> (String, rustls::ServerConfig) {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };
    // Validity periods short enough for every platform's verifier.
    let now = time::OffsetDateTime::now_utc();
    let (not_before, not_after) = (now - time::Duration::days(1), now + time::Duration::days(7));

    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca.distinguished_name
        .push(DnType::CommonName, "moil test CA");
    (ca.not_before, ca.not_after) = (not_before, not_after);
    let ca_cert = ca.self_signed(&ca_key).unwrap();

    let key = KeyPair::generate().unwrap();
    let mut leaf = CertificateParams::new(vec!["127.0.0.1".into(), "localhost".into()]).unwrap();
    leaf.distinguished_name
        .push(DnType::CommonName, "moil test server");
    leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    (leaf.not_before, leaf.not_after) = (not_before, not_after);
    let leaf_cert = leaf.signed_by(&key, &Issuer::new(ca, ca_key)).unwrap();

    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf_cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .unwrap();
    (ca_cert.pem(), config)
}

struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok((tcp, addr)) = self.tcp.accept().await else {
                continue;
            };
            // Clients that don't trust the CA give up here.
            if let Ok(tls) = self.acceptor.accept(tcp).await {
                return (tls, addr);
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// An output of `name`, uploaded to `/out/<name>` on the server.
pub fn output(server: &Server, name: &str) -> (FileName, Upload) {
    (
        FileName::new(name).unwrap(),
        server.upload(&format!("/out/{name}")),
    )
}

/// An input `name` downloaded from `/in/<name>`, serving `contents`.
pub fn input(server: &Server, name: &str, contents: &str) -> (FileName, moil_proto::Download) {
    let path = format!("/in/{name}");
    server.serve(&path, contents);
    (
        FileName::new(name).unwrap(),
        moil_proto::Download {
            url: server.url(&path),
            headers: BTreeMap::new(),
        },
    )
}
