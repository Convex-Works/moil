//! Runs job bundles (spec §8).
//!
//! The [`Executor`] trait is the contract between the machine agent and
//! whatever runs attempts. [`Runtime`] is the real implementation: it
//! prepares the bundle's uv environment, fetches and verifies assets,
//! downloads inputs, runs `job.py`, turns its stdout into events, uploads
//! outputs and decides the [`Outcome`].

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use futures::future::BoxFuture;
use moil_bundle::{Bundle, BundleStore};
use moil_proto::control::Assign;
use moil_proto::job::{Download, Upload};
use moil_proto::{FileName, JobError, JobEvent, JobId, Outcome};
use tokio::sync::{mpsc, watch};

/// Where a running attempt's events go. Unbounded, so a slow consumer never
/// stalls the script.
pub type EventSink = mpsc::UnboundedSender<JobEvent>;

/// Runs attempts. Implemented by [`Runtime`], and by fakes in tests.
pub trait Executor: Send + Sync + 'static {
    /// Gets a bundle ready to run: its environment and assets. Running an
    /// attempt prepares the bundle too, so this is only an optimization,
    /// for example right after the owner approves a bundle.
    fn prepare<'a>(&'a self, bundle: &'a Bundle) -> BoxFuture<'a, Result<(), JobError>>;

    /// Runs one attempt to its end and returns how it ended. Emits
    /// [`JobEvent`]s to `events` as it goes; never emits `result` or
    /// `error`, which end up in the [`Outcome`]. Must return promptly after
    /// `stop` fires, with [`Outcome::Cancelled`] or an `interrupted` failure.
    fn run(&self, job: Job, events: EventSink, stop: StopSignal) -> BoxFuture<'_, Outcome>;
}

/// One attempt to run.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: JobId,
    pub attempt: u32,
    pub bundle: Bundle,
    pub params: serde_json::Value,
    pub inputs: BTreeMap<FileName, Download>,
    pub outputs: BTreeMap<FileName, Upload>,
    pub timeout: Option<Duration>,
}

impl Job {
    /// The attempt an `assign` describes, to run with `bundle`, which the
    /// caller has checked is the approved bundle `assign.bundle_hash` names.
    pub fn from_assign(assign: Assign, bundle: Bundle) -> Self {
        debug_assert_eq!(assign.bundle_hash, bundle.hash());
        Self {
            id: assign.job_id,
            attempt: assign.attempt,
            bundle,
            params: assign.params,
            inputs: assign.inputs,
            outputs: assign.outputs,
            timeout: assign.timeout_ms.map(Duration::from_millis),
        }
    }
}

/// Why an attempt is being stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The service cancelled it: the outcome is [`Outcome::Cancelled`].
    Cancelled,
    /// The owner stopped it or the app is quitting: the outcome is a
    /// retryable `interrupted` failure.
    Interrupted,
}

/// Creates a connected [`Stopper`] and [`StopSignal`].
pub fn stop_signal() -> (Stopper, StopSignal) {
    let (tx, rx) = watch::channel(None);
    (Stopper(tx), StopSignal(rx))
}

/// Asks a running attempt to stop. The first reason given wins.
#[derive(Debug)]
pub struct Stopper(watch::Sender<Option<StopReason>>);

impl Stopper {
    pub fn stop(&self, reason: StopReason) {
        self.0.send_if_modified(|current| {
            if current.is_none() {
                *current = Some(reason);
                true
            } else {
                false
            }
        });
    }
}

/// Tells a running attempt whether, and why, to stop.
#[derive(Debug, Clone)]
pub struct StopSignal(watch::Receiver<Option<StopReason>>);

impl StopSignal {
    /// Why the attempt should stop, if it should.
    pub fn reason(&self) -> Option<StopReason> {
        *self.0.borrow()
    }

    /// Resolves once the attempt should stop. Never resolves if the
    /// [`Stopper`] is dropped without stopping.
    pub async fn stopped(&mut self) -> StopReason {
        loop {
            if let Some(reason) = *self.0.borrow_and_update() {
                return reason;
            }
            if self.0.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }
}

/// How the [`Runtime`] runs attempts.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// The uv binary. See [`find_uv`].
    pub uv: PathBuf,
    /// Where bundles are stored, one directory per hash. Attempts run the
    /// stored copy, and the runtime stores a bundle there if it's missing.
    pub bundles: BundleStore,
    /// Where attempts get their working directories.
    pub jobs_dir: PathBuf,
    /// Where assets are cached, by hash.
    pub assets_dir: PathBuf,
    /// Accept `http` URLs for inputs and outputs on any host, not just
    /// loopback. For development only.
    pub allow_insecure_http: bool,
    /// Keep working directories after attempts end, for debugging.
    pub keep_work_dirs: bool,
    /// How long a stopped script gets between `SIGTERM` and `SIGKILL`.
    pub kill_grace: Duration,
    /// Extra environment variables for uv and the script, such as
    /// `UV_CACHE_DIR` in tests.
    pub extra_env: Vec<(OsString, OsString)>,
}

impl RuntimeConfig {
    /// A config with the spec's defaults, keeping everything under `home`.
    pub fn new(uv: PathBuf, home: &std::path::Path) -> Self {
        Self {
            uv,
            bundles: BundleStore::new(home.join("bundles")),
            jobs_dir: home.join("jobs"),
            assets_dir: home.join("cache").join("assets"),
            allow_insecure_http: false,
            keep_work_dirs: false,
            kill_grace: Duration::from_secs(10),
            extra_env: Vec::new(),
        }
    }
}

/// Runs attempts for real, with uv.
#[derive(Debug)]
pub struct Runtime {
    config: RuntimeConfig,
}

impl Runtime {
    pub fn new(config: RuntimeConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Kills processes left running by attempts of an earlier run of the
    /// app that crashed, and removes their working directories. Returns how
    /// many attempts it cleaned up. Call once at startup, before running
    /// anything.
    pub fn reap_orphans(&self) -> std::io::Result<usize> {
        todo!("moil-runtime: reap_orphans")
    }
}

impl Executor for Runtime {
    fn prepare<'a>(&'a self, _bundle: &'a Bundle) -> BoxFuture<'a, Result<(), JobError>> {
        todo!("moil-runtime: prepare")
    }

    fn run(&self, _job: Job, _events: EventSink, _stop: StopSignal) -> BoxFuture<'_, Outcome> {
        todo!("moil-runtime: run")
    }
}

/// Finds the uv binary: `$MOIL_UV`, then next to the current executable (as
/// in the macOS app bundle), then on `PATH`.
pub fn find_uv() -> Option<PathBuf> {
    todo!("moil-runtime: find_uv")
}
