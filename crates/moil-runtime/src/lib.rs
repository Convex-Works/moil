//! Runs job bundles (spec §8).
//!
//! The [`Executor`] trait is the contract between the machine agent and
//! whatever runs attempts. [`Runtime`] is the real implementation: it
//! prepares the bundle's uv environment, fetches and verifies assets,
//! downloads inputs, runs `job.py`, turns its stdout into events, uploads
//! outputs and decides the [`Outcome`].
//!
//! Where the spec leaves room, the runtime decides:
//!
//! - A job's time limit covers the whole attempt, preparation and transfers
//!   included, since that is what bounds how long the service waits.
//! - A `data` event whose payload is over 256 KiB is dropped with a
//!   warning; a `result` whose meta is, is refused, so the attempt fails as
//!   `no_result` rather than succeed without the meta the service expects.
//! - Nothing the script starts outlives the attempt: when the script ends,
//!   whatever is left in its process group is killed.
//! - A redirect is followed only to a URL the policy of spec §3 allows.
//!
//! Unix only: scripts run in their own process group.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use futures::future::BoxFuture;
use moil_bundle::{Bundle, BundleHash, BundleStore};
use moil_proto::control::Assign;
use moil_proto::job::{Download, Upload};
use moil_proto::{ErrorCode, FileName, JobError, JobEvent, JobId, Outcome};
use tokio::sync::{mpsc, watch};

mod assets;
mod attempt;
mod http;
mod locks;
mod orphans;
mod outcome;
mod process;
mod stderr;
mod stdout;
mod sys;
pub mod uv;
mod workdir;

pub use assets::asset_layout_conflict;
pub use uv::Uv;

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
    /// The approved bundle. The attempt runs the copy in
    /// [`RuntimeConfig::bundles`], storing it there first if needed.
    pub bundle: Bundle,
    /// Handed to the script untouched.
    pub params: serde_json::Value,
    /// Downloaded into `in/` before the script starts.
    pub inputs: BTreeMap<FileName, Download>,
    /// The outputs the script may produce; those its result names are
    /// uploaded from `out/` after it succeeds.
    pub outputs: BTreeMap<FileName, Upload>,
    /// The attempt's time limit, counted from when [`Executor::run`] starts
    /// until the outcome is known: preparation and transfers count too.
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
    /// `UV_CACHE_DIR` in tests. They apply after the runtime's own, except
    /// that the script's `TMPDIR` and `PYTHONUNBUFFERED` are always the
    /// runtime's.
    pub extra_env: Vec<(OsString, OsString)>,
    /// Root certificates to trust for HTTPS on top of the platform's, as
    /// PEM text; each entry may hold several. For networks with a CA of
    /// their own that the platform doesn't know, and for tests.
    pub extra_root_certificates: Vec<String>,
    /// Copy what uv and the script write to stderr to this process's stderr
    /// as it arrives, as `moil run` does for bundle authors. The runtime
    /// keeps the tail for error reports either way.
    pub echo_stderr: bool,
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
            extra_root_certificates: Vec::new(),
            echo_stderr: false,
        }
    }
}

/// A [`RuntimeConfig`] that can't work.
#[derive(Debug, Clone, thiserror::Error)]
#[error("couldn't set up HTTPS: {0}")]
pub struct ConfigError(String);

/// Runs attempts for real, with uv.
#[derive(Debug)]
pub struct Runtime {
    config: RuntimeConfig,
    /// The client, or why it couldn't be built, reported by every attempt
    /// that needs it.
    http: Result<http::Http, ConfigError>,
    assets: assets::AssetCache,
    /// Held while a bundle is stored and its environment synced, by hash:
    /// preparing it after approval and for an attempt may overlap.
    preparing: locks::KeyedLocks<BundleHash>,
}

impl Runtime {
    /// A runtime for `config`. If the config can't work (see
    /// [`Runtime::try_new`]), attempts that move files fail as `internal`.
    pub fn new(config: RuntimeConfig) -> Self {
        let http = http::Http::new(&config.extra_root_certificates, config.allow_insecure_http)
            .map_err(ConfigError);
        Self {
            assets: assets::AssetCache::new(config.assets_dir.clone()),
            http,
            preparing: locks::KeyedLocks::default(),
            config,
        }
    }

    /// A runtime for `config`, or why it can't work, such as an extra root
    /// certificate that isn't valid PEM.
    pub fn try_new(config: RuntimeConfig) -> Result<Self, ConfigError> {
        let runtime = Self::new(config);
        match &runtime.http {
            Ok(_) => Ok(runtime),
            Err(error) => Err(error.clone()),
        }
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Where attempt `attempt` of job `job_id` works, while it runs (and
    /// after, with [`RuntimeConfig::keep_work_dirs`]).
    pub fn work_dir(&self, job_id: &JobId, attempt: u32) -> PathBuf {
        workdir::WorkDir::path(&self.config.jobs_dir, job_id, attempt)
    }

    /// Kills processes left running by attempts of an earlier run of the
    /// app that crashed, and removes their working directories. Returns how
    /// many attempts it cleaned up. Call once at startup, before running
    /// anything.
    ///
    /// Attempts whose supervising process is still alive, including this
    /// one's, are left alone, so calling it while attempts run is safe. It
    /// also removes asset downloads that were abandoned mid-way, going by
    /// files nothing has written to for an hour.
    pub fn reap_orphans(&self) -> std::io::Result<usize> {
        let reaped = orphans::reap(&self.config.jobs_dir, self.config.keep_work_dirs)?;
        self.assets.remove_abandoned(Duration::from_secs(60 * 60))?;
        Ok(reaped)
    }

    /// How this runtime runs uv.
    pub fn uv(&self) -> Uv {
        Uv::new(&self.config.uv).with_env(self.config.extra_env.iter().cloned())
    }

    fn http(&self) -> Result<&http::Http, JobError> {
        self.http
            .as_ref()
            .map_err(|error| JobError::new(ErrorCode::Internal, error.to_string()))
    }

    /// Stores `bundle`, creates its environment and fetches its assets.
    /// Returns the stored bundle's directory.
    async fn prepare_bundle(
        &self,
        bundle: &Bundle,
        events: Option<&EventSink>,
    ) -> Result<PathBuf, JobError> {
        let _preparing = self.preparing.lock(&bundle.hash()).await;
        let dir = self.config.bundles.put(bundle).map_err(|e| {
            JobError::new(
                ErrorCode::Internal,
                format!("couldn't store the bundle: {e}"),
            )
        })?;
        let manifest = bundle.manifest();
        if let Some((file, nested)) = asset_layout_conflict(manifest) {
            return Err(JobError::new(
                ErrorCode::Environment,
                format!(
                    "the bundle's assets {file:?} and {nested:?} can't both exist: {file:?} would have to be a file and a directory"
                ),
            ));
        }
        let fetch_assets = async {
            if manifest.assets.is_empty() {
                return Ok(());
            }
            let http = self.http()?;
            for (name, asset) in &manifest.assets {
                self.assets.fetch(http, name, asset, events).await?;
            }
            Ok(())
        };
        let (uv, script) = (self.uv(), dir.join(moil_bundle::SCRIPT));
        let sync = uv::sync(&uv, &script, self.config.echo_stderr);
        tokio::try_join!(sync, fetch_assets)?;
        Ok(dir)
    }
}

impl Executor for Runtime {
    fn prepare<'a>(&'a self, bundle: &'a Bundle) -> BoxFuture<'a, Result<(), JobError>> {
        Box::pin(async move { self.prepare_bundle(bundle, None).await.map(drop) })
    }

    fn run(&self, job: Job, events: EventSink, stop: StopSignal) -> BoxFuture<'_, Outcome> {
        Box::pin(attempt::run(self, job, events, stop))
    }
}

/// Finds the uv binary: `$MOIL_UV`, then next to the current executable (as
/// in the macOS app bundle), then on `PATH`.
pub fn find_uv() -> Option<PathBuf> {
    uv::locate(
        std::env::var_os("MOIL_UV"),
        std::env::current_exe().ok(),
        std::env::var_os("PATH"),
    )
}
