//! Hooks: the owner's own executables, which moil runs after an attempt of
//! their bundle succeeds here (spec/machine.md §2).
//!
//! [`Hooks`] keeps the hooks waiting to run in a queue on disk, under
//! [`Home::hooks_queue_dir`], and runs them in the background, one at a
//! time, in the order their attempts ended. For each attempt it runs, the
//! agent:
//!
//! 1. makes a [`PendingHook`] with [`Hooks::entry`] as the attempt is
//!    assigned, and gives the runtime its [`hand_off`](PendingHook::hand_off)
//!    as the attempt's [`Job::hand_off`](moil_runtime::Job::hand_off);
//! 2. once the attempt has ended and its `done` is queued for the service,
//!    calls [`PendingHook::ready`] if it succeeded, and drops the entry
//!    otherwise.
//!
//! A hook that didn't get to exit because moil quit or crashed runs again
//! from the start, with the same outputs, the next time [`Hooks::start`]
//! runs on the home; one that exited never runs again. Wherever moil
//! crashes, each run it logged gets one closing line. [`scan`] lists a
//! home's hooks and what's wrong with them, whether or not an agent runs.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use moil_bundle::{Bundle, OpenError, open_regular};
use moil_proto::{Assign, BundleHash, FileName, JobId, JobResult};
use moil_runtime::HandOff;
use moil_runtime::process::{Group, Process};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::Home;
use crate::folders::runnable;
pub use crate::folders::{HookScan, scan_hooks as scan};
use crate::fs::{PRIVATE, read_json, with_path, write_atomic};
use crate::report::ServiceRef;

/// The hook contract's version: the context's `v`.
pub const HOOK_CONTRACT_VERSION: u32 = 1;

/// What a hook reads from stdin, as one line of JSON (spec/machine.md
/// §2.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookContext {
    /// [`HOOK_CONTRACT_VERSION`].
    pub v: u32,
    pub service: ServiceRef,
    pub bundle: BundleRef,
    pub job_id: JobId,
    pub attempt: u32,
    /// The job's title, if the service gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The job's params, as the script got them.
    pub params: serde_json::Value,
    /// The script's `result.meta`, if it sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    #[serde(with = "crate::rfc3339")]
    pub ended_at: SystemTime,
    /// A directory holding the outputs and nothing else.
    pub output_dir: PathBuf,
    /// Each output the attempt uploaded, with its absolute path.
    pub outputs: BTreeMap<FileName, PathBuf>,
}

/// The approved bundle an attempt ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleRef {
    pub name: String,
    pub version: String,
    pub hash: BundleHash,
}

impl From<&Bundle> for BundleRef {
    fn from(bundle: &Bundle) -> Self {
        Self {
            name: bundle.manifest().name.clone(),
            version: bundle.manifest().version.clone(),
            hash: bundle.hash(),
        }
    }
}

/// How hooks run. The defaults are the spec's; tests shrink them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOptions {
    /// How long a hook may run before moil stops it (spec/machine.md §2.2:
    /// an hour).
    pub time_limit: Duration,
    /// How long a hook being stopped gets between `SIGTERM` and `SIGKILL`
    /// (protocol §8.6: 10 s).
    pub kill_grace: Duration,
}

impl Default for HookOptions {
    fn default() -> Self {
        Self {
            time_limit: Duration::from_secs(60 * 60),
            kill_grace: Duration::from_secs(10),
        }
    }
}

// A queue entry is a directory, `<queue>/<number>`, numbered in the order
// entries were made. One attempt runs at a time, so that is also the order
// the attempts ended in.
//
//   outputs/      the outputs as the runtime handed them off, untouched
//   hook.json     once the attempt succeeded, until its hook has run: a
//                 `Queued`
//   running.json  from before the run's first line in the log: a `Running`
//   group.json    once the hook has started: the leader of its process
//                 group
//   ended.json    once the hook has ended, until moil is done with the
//                 run: an `Ended`
//   out/          the hook's output_dir, made afresh from outputs/ for
//                 every run
//   tmp/          the hook's working directory and TMPDIR while it runs
//
// Each record is written before what it lets moil finish after a crash,
// and removed after it: `recover_entry` then knows whether the log has the
// run's first line, whether it has its closing line, and whether the hook
// runs again.

const OUTPUTS: &str = "outputs";
const QUEUED: &str = "hook.json";
const RUNNING: &str = "running.json";
const GROUP: &str = "group.json";
const ENDED: &str = "ended.json";
const OUT: &str = "out";
const TMP: &str = "tmp";

/// A log that has passed this size is renamed before a hook next writes
/// to it (spec/machine.md §2.3).
const MAX_LOG_BYTES: u64 = 1 << 20;

/// What follows `.machine/bin` on a hook's `PATH`.
const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// A hook's `LANG` when moil has none, as for scripts.
const DEFAULT_LANG: &str = "C.UTF-8";

/// `hook.json`: a hook ready to run.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Queued {
    /// Why the hook can't run, when that was known as it was queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    not_run: Option<String>,
    context: HookContext,
}

/// `running.json`: a hook's run under way.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Running {
    /// The process running it.
    supervisor: Process,
    /// How long the log was before the run's first line: if it's longer,
    /// the log has that line, and needs a closing one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    log_length: Option<u64>,
}

/// `ended.json`: how a hook's run ended, recorded before the log's closing
/// line is written.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ended {
    /// The closing line, without its `== `.
    closing: String,
    /// How long the log was before the closing line: if it's longer, the
    /// log has that line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    log_length: Option<u64>,
    /// Whether the hook runs again from the start: it didn't get to exit.
    again: bool,
}

/// A home's queue of hooks, and the task that runs them. Dropping it stops
/// them as [`Hooks::shutdown`] does, without waiting.
#[derive(Debug)]
pub struct Hooks {
    inner: Arc<Inner>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug)]
struct Inner {
    /// With its root made absolute and canonical, as hooks are told it.
    home: Home,
    options: HookOptions,
    /// The entries ready to run, by number.
    ready: Mutex<BTreeMap<u64, Queued>>,
    next_number: AtomicU64,
    /// Told whenever an entry becomes ready.
    wake: Notify,
    quit: CancellationToken,
}

impl Hooks {
    /// Starts running the hooks queued in `home`: those that were waiting
    /// when moil last stopped, and those that were running then, after
    /// killing what's left of them and closing their logs with `==
    /// stopped: moil stopped unexpectedly` if moil crashed. A hook that had
    /// exited doesn't run again, and its log gets the closing line it
    /// missed, if it missed it. Entries of attempts that never ended are
    /// removed.
    ///
    /// Call it once, from within a Tokio runtime, in the process that holds
    /// the home's lock, before it runs any attempt.
    pub fn start(home: &Home, options: HookOptions) -> io::Result<Self> {
        let root = home
            .root()
            .canonicalize()
            .map_err(|e| with_path(e, home.root()))?;
        let home = Home::new(root);
        let queue_dir = home.hooks_queue_dir();
        fs::create_dir_all(&queue_dir).map_err(|e| with_path(e, &queue_dir))?;
        let (ready, next_number) = recover(&home).map_err(|e| with_path(e, &queue_dir))?;
        let inner = Arc::new(Inner {
            home,
            options,
            ready: Mutex::new(ready),
            next_number: AtomicU64::new(next_number),
            wake: Notify::new(),
            quit: CancellationToken::new(),
        });
        let worker = tokio::spawn(inner.clone().work());
        Ok(Self {
            inner,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// A queue entry for the attempt that `assign` starts, of `bundle` for
    /// `service`: if the attempt succeeds, its hook runs with the outputs
    /// the runtime leaves in the entry. Touches no disk; dropping the entry
    /// removes what the runtime left there.
    pub fn entry(&self, service: ServiceRef, bundle: BundleRef, assign: &Assign) -> PendingHook {
        let number = self.inner.next_number.fetch_add(1, Ordering::Relaxed);
        let dir = self.inner.entry_dir(number);
        let mut context = HookContext {
            v: HOOK_CONTRACT_VERSION,
            service,
            bundle,
            job_id: assign.job_id.clone(),
            attempt: assign.attempt,
            title: assign.title.clone(),
            params: assign.params.clone(),
            // Known once the attempt has ended.
            meta: None,
            ended_at: SystemTime::UNIX_EPOCH,
            output_dir: PathBuf::new(),
            outputs: BTreeMap::new(),
        };
        locate_outputs(&mut context, &dir);
        PendingHook {
            inner: self.inner.clone(),
            number,
            hand_off: HandOff::new(dir.join(OUTPUTS)),
            context: Some(context),
        }
    }

    /// Stops the hook that is running, if any, writing `== stopped: moil
    /// quit` in its log, and runs no more. Hooks that didn't get to exit
    /// stay queued, to run again from the start when moil next starts.
    pub async fn shutdown(&self) {
        self.inner.quit.cancel();
        let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(worker) = worker {
            let _ = worker.await;
        }
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        self.inner.quit.cancel();
    }
}

/// A queue entry for an attempt that hasn't ended yet. Dropping it removes
/// the entry, with any outputs handed off to it.
#[derive(Debug)]
pub struct PendingHook {
    inner: Arc<Inner>,
    number: u64,
    hand_off: HandOff,
    /// What the hook is told, so far; taken once the entry is queued.
    context: Option<HookContext>,
}

impl PendingHook {
    /// Where the runtime leaves the attempt's outputs if it succeeds: the
    /// attempt's [`Job::hand_off`](moil_runtime::Job::hand_off).
    pub fn hand_off(&self) -> HandOff {
        self.hand_off.clone()
    }

    /// Queues the hook of the attempt, which succeeded with `result` at
    /// `ended_at`, to run once the hooks of the entries made before it
    /// have. If there is no hook for its service and bundle by now, the
    /// entry is removed instead. Blocks while it writes to the disk.
    ///
    /// Call it once the attempt's `done` is queued for its service, so that
    /// the hook never runs before the service hears how the attempt ended
    /// (spec/machine.md §2.1), and only after the entries made before this
    /// one were readied or dropped, so that no hook runs before those of
    /// attempts that ended earlier.
    pub fn ready(mut self, result: JobResult, ended_at: SystemTime) {
        let dir = self.inner.entry_dir(self.number);
        let mut context = self.context.take().expect("an entry is readied once");
        let hook = self
            .inner
            .home
            .hook(&context.service.id, &context.bundle.name);
        // The file is the whole setup: without it, there is nothing to run.
        if is_absent(&hook) {
            return remove_entry(&dir);
        }
        context.meta = result.meta;
        context.ended_at = ended_at;
        context.outputs = result
            .files
            .into_keys()
            .map(|name| (name, PathBuf::new()))
            .collect();
        locate_outputs(&mut context, &dir);
        let not_run = match self.hand_off.result() {
            Some(Err(why)) => Some(format!("moil couldn't keep the attempt's outputs: {why}")),
            // Otherwise the outputs are checked as the hook starts.
            Some(Ok(())) | None => None,
        };
        let queued = Queued { not_run, context };
        let path = dir.join(QUEUED);
        let written = serde_json::to_vec_pretty(&queued)
            .map_err(io::Error::from)
            .and_then(|mut json| {
                json.push(b'\n');
                write_atomic(&path, &json, PRIVATE)
            });
        if let Err(error) = written {
            tracing::warn!(
                "couldn't queue the hook for {} attempt {}, so it won't run: {}",
                queued.context.job_id,
                queued.context.attempt,
                with_path(error, &path)
            );
            return remove_entry(&dir);
        }
        self.inner.ready().insert(self.number, queued);
        self.inner.wake.notify_one();
    }
}

impl Drop for PendingHook {
    fn drop(&mut self) {
        // Not queued: the attempt didn't succeed, or its service no longer
        // hears of it.
        if self.context.is_some() {
            remove_entry(&self.inner.entry_dir(self.number));
        }
    }
}

impl Inner {
    fn ready(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Queued>> {
        self.ready.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn entry_dir(&self, number: u64) -> PathBuf {
        self.home.hooks_queue_dir().join(number.to_string())
    }

    /// Runs ready hooks, one at a time, until moil quits.
    async fn work(self: Arc<Self>) {
        let supervisor = Process::current();
        loop {
            let (number, queued) = tokio::select! {
                biased;
                () = self.quit.cancelled() => return,
                next = self.next() => next,
            };
            let dir = self.entry_dir(number);
            let ending = self.run(&dir, &queued, supervisor.as_ref()).await;
            if matches!(ending, Some(Ending::Quit)) {
                // It runs again from the start next time.
                for leftover in [OUT, TMP] {
                    remove_quietly(&dir.join(leftover));
                }
                return;
            }
            remove_entry(&dir);
        }
    }

    /// The first entry ready to run, once there is one.
    async fn next(&self) -> (u64, Queued) {
        loop {
            if let Some(next) = self.ready().pop_first() {
                return next;
            }
            self.wake.notified().await;
        }
    }

    /// Runs the hook of the entry in `dir`, logging how it went, unless its
    /// file is gone. Returns how it ended.
    async fn run(
        &self,
        dir: &Path,
        queued: &Queued,
        supervisor: Option<&Process>,
    ) -> Option<Ending> {
        let context = &queued.context;
        let hook = self.home.hook(&context.service.id, &context.bundle.name);
        // Removing the file turns the hook off, for attempts that already
        // ended too.
        if is_absent(&hook) {
            return None;
        }
        let mut log = Log::open(
            self.home
                .hook_log(&context.service.id, &context.bundle.name),
        );
        // Recorded before the run's first line, so that a crash from here
        // on leaves it, for moil to close the log and kill what's left of
        // the hook when it next starts.
        let recorded = supervisor.is_some_and(|supervisor| {
            let running = Running {
                supervisor: supervisor.clone(),
                log_length: log.length(),
            };
            let otherwise = "moil can't stop the hook, or close its log, if moil crashes";
            record(&dir.join(RUNNING), &running, otherwise)
        });
        log.line(&format!(
            "{} {} attempt {}",
            crate::rfc3339::format(SystemTime::now()),
            context.job_id,
            context.attempt
        ));
        let ending = match &queued.not_run {
            Some(why) => Ending::NotRun(why.clone()),
            None => self.run_hook(dir, context, &hook, &log, recorded).await,
        };
        end_run(dir, &mut log, &ending);
        Some(ending)
    }

    /// Runs `hook` with `context`, as the entry in `dir` has it, and
    /// records its process group if the run is `recorded`.
    async fn run_hook(
        &self,
        dir: &Path,
        context: &HookContext,
        hook: &Path,
        log: &Log,
        recorded: bool,
    ) -> Ending {
        if let Err(why) = runnable(hook) {
            return Ending::NotRun(why.to_string());
        }
        let out = &context.output_dir;
        if let Err(why) = give_outputs(&dir.join(OUTPUTS), out, context.outputs.keys()) {
            return Ending::NotRun(why);
        }
        let tmp = dir.join(TMP);
        if let Err(error) = moil_runtime::remove_tree(&tmp).and_then(|()| fs::create_dir(&tmp)) {
            return Ending::NotRun(format!(
                "moil couldn't make the directory it runs in, {}: {error}",
                tmp.display()
            ));
        }

        let mut command = Command::new(hook);
        command
            .env_clear()
            .envs(self.environment(&tmp))
            // Not among the outputs, whose names come from the service: a
            // file such as uv.toml there could change what the hook runs.
            .current_dir(&tmp)
            .stdin(Stdio::piped())
            .stdout(log.stdio())
            .stderr(log.stdio());
        let mut group = match Group::spawn(command) {
            Ok(group) => group,
            Err(error) => return Ending::NotRun(cant_start(&error)),
        };
        let started = Instant::now();
        if recorded {
            let otherwise = "moil can't stop what's left of the hook if moil crashes";
            record(&dir.join(GROUP), &group.leader(), otherwise);
        }
        let mut line = serde_json::to_vec(context).expect("a queued context serializes");
        line.push(b'\n');
        let stdin = group.child_mut().stdin.take();
        // A hook that doesn't read its stdin must not hold anything up.
        tokio::spawn(async move {
            if let Some(mut stdin) = stdin {
                let _ = stdin.write_all(&line).await;
                let _ = stdin.shutdown().await;
            }
        });

        let limit = self.options.time_limit;
        let stopped = tokio::select! {
            () = group.exited() => None,
            () = tokio::time::sleep(limit) => Some(Ending::TimedOut(limit)),
            () = self.quit.cancelled() => Some(Ending::Quit),
        };
        if stopped.is_some() {
            group.terminate(self.options.kill_grace).await;
        }
        // Nothing the hook started outlives it.
        let status = group.finish().await;
        let after = started.elapsed();
        remove_quietly(&tmp);
        stopped.unwrap_or_else(|| Ending::from_status(status, after))
    }

    /// A hook's whole environment (spec/machine.md §2.2).
    fn environment(&self, tmp: &Path) -> Vec<(&'static str, OsString)> {
        let mut env = Vec::new();
        for name in ["HOME", "USER"] {
            if let Some(value) = std::env::var_os(name) {
                env.push((name, value));
            }
        }
        let lang = std::env::var_os("LANG").filter(|lang| !lang.is_empty());
        env.push(("LANG", lang.unwrap_or_else(|| DEFAULT_LANG.into())));
        env.push(("MOIL_HOME", self.home.root().into()));
        env.push(("TMPDIR", tmp.into()));
        let mut path = OsString::from(self.home.bin_dir());
        path.push(":");
        path.push(SYSTEM_PATH);
        env.push(("PATH", path));
        env
    }
}

/// Writes `value` to the record at `path`. Returns whether it did; if not,
/// warns that `otherwise`.
fn record(path: &Path, value: &impl Serialize, otherwise: &str) -> bool {
    let json = serde_json::to_vec(value).expect("a record serializes");
    match write_atomic(path, &json, PRIVATE) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                "couldn't record a hook's run, so {otherwise}: {}",
                with_path(error, path)
            );
            false
        }
    }
}

/// Ends the run of the entry in `dir`, which ended as `ending`: records
/// that, then closes `log` with it and forgets the run, as
/// [`Ended::close`] does.
fn end_run(dir: &Path, log: &mut Log, ending: &Ending) {
    let ended = Ended {
        closing: ending.to_string(),
        log_length: log.length(),
        again: ending.runs_again(),
    };
    let otherwise = "a crash now would run it again, or close its log twice";
    record(&dir.join(ENDED), &ended, otherwise);
    ended.close(dir, log);
}

impl Ended {
    /// Finishes the run it records, of the entry in `dir`: closes `log`
    /// with the closing line, unless the log has that line already, then
    /// forgets the run, removing first what queues the hook, unless it
    /// runs again. Once this is recorded, a crash neither runs a hook that
    /// exited again nor closes its log twice: [`recover`] calls this again.
    fn close(&self, dir: &Path, log: &mut Log) {
        // Once the hook has ended, only this line makes the log grow.
        if self.log_length.is_some() && !log.grew_since(self.log_length) {
            log.line(&self.closing);
        }
        if !self.again {
            remove_quietly(&dir.join(QUEUED));
        }
        // This record last: a run recorded as running and not as ended is
        // one moil lost track of.
        for record in [GROUP, RUNNING, ENDED] {
            remove_quietly(&dir.join(record));
        }
    }
}

/// How a hook's run ended: its log's closing line (spec/machine.md §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ending {
    Exited {
        status: i32,
        after: Duration,
    },
    Killed {
        signal: i32,
        after: Duration,
    },
    /// Stopped once it had run for this long.
    TimedOut(Duration),
    Quit,
    /// moil crashed, or was killed, while it ran.
    Crashed,
    NotRun(String),
    /// Should never happen: its exit status couldn't be read.
    Unknown {
        error: String,
        after: Duration,
    },
}

impl Ending {
    /// Whether the hook runs again from the start when moil next starts,
    /// because it didn't get to exit.
    fn runs_again(&self) -> bool {
        matches!(self, Self::Quit | Self::Crashed)
    }

    fn from_status(status: io::Result<std::process::ExitStatus>, after: Duration) -> Self {
        use std::os::unix::process::ExitStatusExt as _;
        match status {
            Ok(status) => match (status.code(), status.signal()) {
                (Some(status), _) => Self::Exited { status, after },
                (None, Some(signal)) => Self::Killed { signal, after },
                (None, None) => Self::Unknown {
                    error: "it ended in an unknown way".into(),
                    after,
                },
            },
            Err(error) => Self::Unknown {
                error: error.to_string(),
                after,
            },
        }
    }
}

impl fmt::Display for Ending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exited { status, after } => {
                write!(f, "exited {status} after {}", seconds(*after))
            }
            Self::Killed { signal, after } => {
                write!(f, "killed by signal {signal} after {}", seconds(*after))
            }
            Self::TimedOut(limit) if *limit == Duration::from_secs(60 * 60) => {
                f.write_str("stopped after an hour")
            }
            Self::TimedOut(limit) => {
                write!(f, "stopped after {}", humantime::format_duration(*limit))
            }
            Self::Quit => f.write_str("stopped: moil quit"),
            Self::Crashed => f.write_str("stopped: moil stopped unexpectedly"),
            Self::NotRun(why) => write!(f, "not run: {why}"),
            Self::Unknown { error, after } => write!(
                f,
                "exited after {}, but moil couldn't tell how: {error}",
                seconds(*after)
            ),
        }
    }
}

/// `duration` to the second, as in `42s` or `1h 2m 3s`.
fn seconds(duration: Duration) -> String {
    humantime::format_duration(Duration::from_secs(duration.as_secs())).to_string()
}

/// A hook's log, open for appending, or nowhere if it can't be opened.
#[derive(Debug)]
struct Log {
    path: PathBuf,
    file: Option<File>,
}

impl Log {
    /// Opens the log at `path` as a hook starts, making it if it isn't
    /// there, first renaming it to `<name>.1` if it has passed
    /// [`MAX_LOG_BYTES`] (spec/machine.md §2.3).
    fn open(path: PathBuf) -> Self {
        let file = open_log(&path, true).unwrap_or_else(|error| {
            tracing::warn!("couldn't open a hook's log, so what the hook prints is lost: {error}");
            None
        });
        Self { path, file }
    }

    /// The log at `path`, to add to it, if it's there: a log that's gone,
    /// or a service folder, isn't made again just for a closing line.
    fn existing(path: PathBuf) -> Self {
        let file = open_log(&path, false).unwrap_or_else(|error| {
            tracing::warn!("couldn't write to a hook's log: {error}");
            None
        });
        Self { path, file }
    }

    /// How long the log is, if it's open.
    fn length(&self) -> Option<u64> {
        Some(self.file.as_ref()?.metadata().ok()?.len())
    }

    /// Whether the log is open, and longer than it was `before`.
    fn grew_since(&self, before: Option<u64>) -> bool {
        matches!((before, self.length()), (Some(before), Some(now)) if now > before)
    }

    /// Appends `== <text>` as a line of its own.
    fn line(&mut self, text: &str) {
        let Some(file) = &mut self.file else {
            return;
        };
        // Whatever the hook printed last may lack its newline.
        let mut last = *b"\n";
        if let Ok(length @ 1..) = file.metadata().map(|m| m.len()) {
            let _ = file.read_at(&mut last, length - 1);
        }
        let newline = if last[0] == b'\n' { "" } else { "\n" };
        if let Err(error) = writeln!(file, "{newline}== {text}") {
            tracing::warn!(
                "couldn't write to a hook's log: {}",
                with_path(error, &self.path)
            );
        }
    }

    /// Where the hook's stdout and stderr go: the log, after what moil has
    /// written.
    fn stdio(&self) -> Stdio {
        match self.file.as_ref().map(File::try_clone) {
            Some(Ok(file)) => file.into(),
            _ => Stdio::null(),
        }
    }
}

/// Opens the log at `path` to add to it, if it's there, or with `start`,
/// as a hook starts, making it and its folder if they aren't, and first
/// renaming it to `<name>.1` if it has passed [`MAX_LOG_BYTES`]. Only a
/// regular file in a folder that isn't a symbolic link: a commit could put
/// a link at either path to have moil write to another of the owner's
/// files.
fn open_log(path: &Path, start: bool) -> io::Result<Option<File>> {
    let dir = path
        .parent()
        .expect("a hook's log is in its service's folder");
    match fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(refused(dir, &OpenError::SymbolicLink));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound && !start => return Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(dir).map_err(|e| with_path(e, dir))?;
        }
        // Anything else wrong with it, opening the log reports.
        _ => {}
    }
    if start && fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() > MAX_LOG_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        fs::rename(path, old).map_err(|e| with_path(e, path))?;
    }
    let mut options = File::options();
    options.read(true).append(true).create(start);
    match open_regular(path, &options) {
        Ok(file) => Ok(Some(file)),
        Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound && !start => Ok(None),
        Err(OpenError::Io(e)) => Err(with_path(e, path)),
        Err(why) => Err(refused(path, &why)),
    }
}

/// Why moil doesn't write a log at `path`, or in the folder at `path`, as
/// a sentence.
fn refused(path: &Path, why: &OpenError) -> io::Error {
    let why = match why {
        OpenError::SymbolicLink => "it's a symbolic link, which moil doesn't write through",
        _ => "it isn't a regular file, which is all moil writes a log to",
    };
    io::Error::other(format!("{}: {why}. Remove it", path.display()))
}

/// Makes `out` afresh with copies of the outputs `names` from `kept`, so
/// that every run of a hook gets them as the attempt left them, whatever
/// an earlier run did to its own. Copying is cheap where it matters: on
/// APFS a copy is a clone.
fn give_outputs<'a>(
    kept: &Path,
    out: &Path,
    names: impl Iterator<Item = &'a FileName>,
) -> Result<(), String> {
    let failed = |error: io::Error| format!("moil couldn't give it the outputs: {error}");
    moil_runtime::remove_tree(out).map_err(failed)?;
    fs::create_dir(out).map_err(failed)?;
    for name in names {
        let from = kept.join(name.as_str());
        if !fs::symlink_metadata(&from).is_ok_and(|m| m.is_file()) {
            return Err(format!("moil no longer has output {name} to give it"));
        }
        fs::copy(&from, out.join(name.as_str())).map_err(failed)?;
    }
    Ok(())
}

/// What moil says when it can't start a hook.
fn cant_start(error: &io::Error) -> String {
    // The hook itself is there: what's missing is what runs it.
    let hint = if error.kind() == io::ErrorKind::NotFound {
        ". Check the program its #! line names"
    } else {
        ""
    };
    format!("moil couldn't start it: {error}{hint}")
}

/// Whether nothing at all is at `path`, not even a broken link.
fn is_absent(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// The queue of `home` as a crash or quit left it: the entries ready to
/// run, by number, and the number the next entry takes. Kills what's left
/// of hooks that were running, and removes entries that aren't ready.
fn recover(home: &Home) -> io::Result<(BTreeMap<u64, Queued>, u64)> {
    let (mut ready, mut next_number) = (BTreeMap::new(), 0);
    for item in fs::read_dir(home.hooks_queue_dir())? {
        let item = item?;
        let dir = item.path();
        let Some(number) = item
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u64>().ok())
        else {
            // moil's own directory: nothing else belongs in it.
            remove_quietly(&dir);
            continue;
        };
        next_number = next_number.max(number + 1);
        match recover_entry(home, &dir) {
            Ok(Some(queued)) => {
                ready.insert(number, queued);
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                "couldn't clean up after a hook that was running when moil stopped; \
                 moil tries again when it next starts: {}",
                with_path(error, &dir)
            ),
        }
    }
    Ok((ready, next_number))
}

/// The hook to run for the entry at `dir`, if it is to run, once moil has
/// finished what a crash left of its last run.
fn recover_entry(home: &Home, dir: &Path) -> io::Result<Option<Queued>> {
    let queued = read_record::<Queued>(&dir.join(QUEUED));
    let running = read_record::<Running>(&dir.join(RUNNING));
    // Another process runs it; with the home's lock, that can only be a
    // moil that is about to exit.
    if running.as_ref().is_some_and(|r| r.supervisor.is_running()) {
        return Ok(None);
    }
    let ended = read_record::<Ended>(&dir.join(ENDED));
    if running.is_some() && ended.is_none() {
        // moil lost track of the hook as it ran.
        if let Some(leader) = read_record::<Process>(&dir.join(GROUP)) {
            leader.kill_group()?;
        }
        tracing::info!(
            "a hook was running when moil stopped; it runs again from the start: {}",
            dir.display()
        );
    }
    // The attempt never ended, its hook has run, or its record is damaged.
    let Some(mut queued) = queued else {
        moil_runtime::remove_tree(dir)?;
        return Ok(None);
    };
    let context = &queued.context;
    let mut log = Log::existing(home.hook_log(&context.service.id, &context.bundle.name));
    let again = match (ended, running) {
        (Some(ended), _) => {
            ended.close(dir, &mut log);
            ended.again
        }
        // The run moil lost track of gets its closing line before the
        // next one starts (spec/machine.md §2.3), if its first line made
        // it to the log.
        (None, Some(running)) => {
            if log.grew_since(running.log_length) {
                end_run(dir, &mut log, &Ending::Crashed);
            } else {
                for record in [GROUP, RUNNING] {
                    remove_quietly(&dir.join(record));
                }
            }
            true
        }
        (None, None) => true,
    };
    if !again {
        moil_runtime::remove_tree(dir)?;
        return Ok(None);
    }
    // Wherever the home is now.
    locate_outputs(&mut queued.context, dir);
    Ok(Some(queued))
}

/// Points `context` at the outputs of the queue entry in `dir`.
fn locate_outputs(context: &mut HookContext, dir: &Path) {
    let output_dir = dir.join(OUT);
    for (name, path) in &mut context.outputs {
        *path = output_dir.join(name.as_str());
    }
    context.output_dir = output_dir;
}

/// The record at `path`, or `None` if there is none or it's damaged.
fn read_record<T: DeserializeOwned>(path: &Path) -> Option<T> {
    read_json(path).unwrap_or_else(|error| {
        tracing::warn!("ignoring a hook's record: {error}");
        None
    })
}

/// Removes a queue entry, with its outputs.
fn remove_entry(dir: &Path) {
    if let Err(error) = moil_runtime::remove_tree(dir) {
        tracing::warn!(
            "couldn't remove a hook's outputs; moil tries again when it next starts: {}",
            with_path(error, dir)
        );
    }
}

/// Removes what's at `path`, if anything, whatever it is.
fn remove_quietly(path: &Path) {
    let removed = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => moil_runtime::remove_tree(path),
        Ok(_) => fs::remove_file(path),
        Err(_) => Ok(()),
    };
    if let Err(error) = removed {
        tracing::warn!("couldn't remove {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a run does to its queue entry and its log, one step at a time.
    #[derive(Debug, Clone, Copy)]
    enum Step {
        /// Records that it runs, as [`Running`].
        Running,
        /// Writes this line to the log.
        Line(&'static str),
        /// Records how it ended, as [`Ended`]: the closing line, and
        /// whether the hook runs again.
        Ended(&'static str, bool),
        Remove(&'static str),
    }

    const EARLIER: &str =
        "== 2026-09-27T22:41:08Z recording-0 attempt 1\nearlier\n== exited 0 after 1s\n";
    const FIRST: &str = "2026-09-27T22:42:00Z recording-1 attempt 1";
    const CRASHED: &str = "stopped: moil stopped unexpectedly";

    /// The steps of a run that ends with `closing`, and runs again if
    /// `again`, in the order `run` and [`end_run`] take them: each is a
    /// point moil can crash after.
    fn run(closing: &'static str, again: bool) -> Vec<Step> {
        let mut steps = vec![
            Step::Running,
            Step::Line(FIRST),
            Step::Line("printed"),
            Step::Ended(closing, again),
            Step::Line(closing),
        ];
        if !again {
            steps.push(Step::Remove(QUEUED));
        }
        steps.extend([Step::Remove(RUNNING), Step::Remove(ENDED)]);
        steps
    }

    fn context(home: &Home) -> HookContext {
        let mut context = HookContext {
            v: HOOK_CONTRACT_VERSION,
            service: ServiceRef {
                id: crate::ServiceId::new("klisi.example.com").unwrap(),
                name: "klisi".into(),
                base_url: "https://klisi.example.com/moil".parse().unwrap(),
            },
            bundle: BundleRef {
                name: "transcribe".into(),
                version: "0.3.0".into(),
                hash: "42d30e3fb23030627d993330b1e2917a0ac2f4abef567fc6e82328d8acedeae6"
                    .parse()
                    .unwrap(),
            },
            job_id: JobId::new("recording-1").unwrap(),
            attempt: 1,
            title: None,
            params: serde_json::json!({}),
            meta: None,
            ended_at: SystemTime::UNIX_EPOCH,
            output_dir: PathBuf::new(),
            outputs: BTreeMap::from([(FileName::new("a.txt").unwrap(), PathBuf::new())]),
        };
        locate_outputs(&mut context, &home.hooks_queue_dir().join("0"));
        context
    }

    /// Wherever moil crashes in a run, the run's first line in the log gets
    /// one closing line, and the hook runs again only if it didn't get to
    /// exit: when moil next starts, and when it starts once more after
    /// crashing again right away.
    #[tokio::test]
    async fn a_crash_anywhere_in_a_run_closes_its_log_once_and_reruns_only_what_didnt_exit() {
        // What a crash leaves of moil: a process that has exited.
        let group = Group::spawn(Command::new("/usr/bin/true")).unwrap();
        let crashed = group.leader();
        group.finish().await.unwrap();

        // An exited hook; one stopped as moil quit; and one that moil
        // crashed during, then crashed again as it closed its log.
        let runs = [
            run("exited 0 after 1s", false),
            run("stopped: moil quit", true),
            run(CRASHED, true),
        ];
        for steps in runs {
            for taken in 0..=steps.len() {
                let dir = tempfile::tempdir().unwrap();
                let home = Home::new(dir.path());
                let entry = home.hooks_queue_dir().join("0");
                fs::create_dir_all(entry.join(OUTPUTS)).unwrap();
                fs::write(entry.join(OUTPUTS).join("a.txt"), "a").unwrap();
                let queued = Queued {
                    not_run: None,
                    context: context(&home),
                };
                let context = &queued.context;
                let log = home.hook_log(&context.service.id, &context.bundle.name);
                fs::create_dir_all(log.parent().unwrap()).unwrap();
                fs::write(&log, EARLIER).unwrap();
                fs::write(entry.join(QUEUED), serde_json::to_vec(&queued).unwrap()).unwrap();
                let length = || Some(fs::metadata(&log).unwrap().len());

                // What the log holds, whether the run's first line is
                // there, and how the run ended, if moil recorded it.
                let mut written = EARLIER.to_owned();
                let (mut started, mut ended) = (false, None);
                for step in &steps[..taken] {
                    match *step {
                        Step::Running => {
                            let running = Running {
                                supervisor: crashed.clone(),
                                log_length: length(),
                            };
                            assert!(record(&entry.join(RUNNING), &running, ""));
                        }
                        Step::Line(line) => {
                            let line = match line {
                                "printed" => "printed\n".to_owned(),
                                line => format!("== {line}\n"),
                            };
                            let mut file = File::options().append(true).open(&log).unwrap();
                            file.write_all(line.as_bytes()).unwrap();
                            written.push_str(&line);
                            started = true;
                        }
                        Step::Ended(closing, again) => {
                            let how = Ended {
                                closing: closing.into(),
                                log_length: length(),
                                again,
                            };
                            assert!(record(&entry.join(ENDED), &how, ""));
                            ended = Some((closing, again));
                        }
                        Step::Remove(record) => fs::remove_file(entry.join(record)).unwrap(),
                    }
                }
                // The run, if logged, closed once: as it ended, or as moil
                // crashed if moil lost track of it.
                let closing = format!("== {}\n", ended.map_or(CRASHED, |(closing, _)| closing));
                let mut logged = written;
                if started && !logged.ends_with(&closing) {
                    logged.push_str(&closing);
                }
                let runs_again = ended.is_none_or(|(_, again)| again);

                for start in ["moil starts", "moil starts again"] {
                    let (ready, _) = recover(&home).unwrap();
                    let at = format!("{steps:?} crashed after {taken} steps, then {start}");
                    assert_eq!(fs::read_to_string(&log).unwrap(), logged, "{at}");
                    assert_eq!(ready.contains_key(&0), runs_again, "{at}");
                    let mut left: Vec<String> = match fs::read_dir(&entry) {
                        Ok(names) => names
                            .map(|n| n.unwrap().file_name().into_string().unwrap())
                            .collect(),
                        Err(_) => Vec::new(),
                    };
                    left.sort();
                    match runs_again {
                        true => assert_eq!(left, [QUEUED, OUTPUTS], "{at}"),
                        false => assert_eq!(left, [] as [String; 0], "{at}"),
                    }
                }
            }
        }
    }
}
