//! Hooks: the owner's own executables, which moil runs after an attempt of
//! their bundle succeeds here (spec/machine.md §2).
//!
//! [`Hooks`] keeps the hooks waiting to run in a queue on disk, under
//! [`Home::hooks_queue_dir`], and runs them in the background, one at a
//! time, in the order their attempts ended. For each attempt it runs, the
//! agent:
//!
//! 1. makes a [`PendingHook`] with [`Hooks::entry`] before the attempt
//!    starts, and gives the runtime its [`hand_off`](PendingHook::hand_off)
//!    as the attempt's [`Job::hand_off`](moil_runtime::Job::hand_off);
//! 2. once the attempt has ended and its `done` is queued for the service,
//!    calls [`PendingHook::ready`] if it succeeded, and drops the entry
//!    otherwise.
//!
//! A hook that didn't get to exit because moil quit or crashed runs again
//! from the start, with the same outputs, the next time [`Hooks::start`]
//! runs on the home. [`scan`] lists a home's hooks and what's wrong with
//! them, whether or not an agent runs, and [`link_bin`] links the `moil`
//! and `uv` that hooks find first on their `PATH`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::os::unix::fs::{FileExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use moil_proto::{BundleHash, FileName, JobId, JobResult};
use moil_runtime::HandOff;
use moil_runtime::process::{Group, Process};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::fs::{PRIVATE, with_path, write_atomic};
use crate::report::{HookListing, HookReport, Problem, ServiceRef};
use crate::{Home, ServiceId};

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
    /// A directory holding the outputs and nothing else; the hook runs in
    /// it.
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

/// An attempt that succeeded, as its hook is told about it
/// (spec/machine.md §2.2): the [`HookContext`] but for where the outputs
/// are, which its [`PendingHook`] knows.
#[derive(Debug, Clone, PartialEq)]
pub struct Succeeded {
    pub service: ServiceRef,
    /// The approved bundle that ran.
    pub bundle: BundleRef,
    pub job_id: JobId,
    pub attempt: u32,
    /// The job's title, if the service gave one.
    pub title: Option<String>,
    /// The job's params, as the script got them.
    pub params: serde_json::Value,
    /// What the attempt reported: its `meta` goes to the hook, and its
    /// `files` name the outputs.
    pub result: JobResult,
    pub ended_at: SystemTime,
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
// entries were made:
//
//   outputs/      the outputs as the runtime handed them off, untouched
//   hook.json     once the attempt succeeded: a `Queued`
//   running.json  while the hook runs: a `Running`
//   out/          the hook's output_dir, made afresh from outputs/ for
//                 every run
//   tmp/          the hook's TMPDIR while it runs

const OUTPUTS: &str = "outputs";
const QUEUED: &str = "hook.json";
const RUNNING: &str = "running.json";
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
    /// Hooks run in this order, which is the order their attempts ended in.
    order: u64,
    /// Why the hook can't run, when that was known as it was queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    not_run: Option<String>,
    context: HookContext,
}

/// `running.json`: which process runs the hook, and the hook's process
/// group once it has started, so that the processes a crash leaves behind
/// can be found.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Running {
    supervisor: Process,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<Process>,
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
    queue_dir: PathBuf,
    options: HookOptions,
    state: Mutex<State>,
    /// Told whenever an entry becomes ready.
    wake: Notify,
    quit: CancellationToken,
}

#[derive(Debug, Default)]
struct State {
    /// The entries ready to run, by order and then number.
    ready: BTreeMap<(u64, u64), Entry>,
    next_order: u64,
    next_number: u64,
}

/// A queue entry whose hook is ready to run.
#[derive(Debug)]
struct Entry {
    dir: PathBuf,
    queued: Queued,
}

impl Hooks {
    /// Starts running the hooks queued in `home`: those that were waiting
    /// when moil last stopped, and those that were running then, after
    /// killing what's left of them. Entries of attempts that never ended
    /// are removed.
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
        let state = recover(&queue_dir).map_err(|e| with_path(e, &queue_dir))?;
        let inner = Arc::new(Inner {
            home,
            queue_dir,
            options,
            state: Mutex::new(state),
            wake: Notify::new(),
            quit: CancellationToken::new(),
        });
        let worker = tokio::spawn(inner.clone().work());
        Ok(Self {
            inner,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// A queue entry for an attempt about to start, whose hook runs if the
    /// attempt succeeds. Dropping it removes it.
    pub fn entry(&self) -> io::Result<PendingHook> {
        let number = {
            let mut state = self.inner.state();
            state.next_number += 1;
            state.next_number - 1
        };
        let dir = self.inner.queue_dir.join(number.to_string());
        fs::create_dir(&dir).map_err(|e| with_path(e, &dir))?;
        Ok(PendingHook {
            inner: self.inner.clone(),
            hand_off: HandOff::new(dir.join(OUTPUTS)),
            dir,
            number,
            queued: false,
        })
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
    dir: PathBuf,
    number: u64,
    hand_off: HandOff,
    queued: bool,
}

impl PendingHook {
    /// Where the runtime leaves the attempt's outputs if it succeeds: the
    /// attempt's [`Job::hand_off`](moil_runtime::Job::hand_off).
    pub fn hand_off(&self) -> HandOff {
        self.hand_off.clone()
    }

    /// The context a hook gets for `attempt`, with this entry's outputs.
    pub fn context(&self, attempt: &Succeeded) -> HookContext {
        let mut context = HookContext {
            v: HOOK_CONTRACT_VERSION,
            service: attempt.service.clone(),
            bundle: attempt.bundle.clone(),
            job_id: attempt.job_id.clone(),
            attempt: attempt.attempt,
            title: attempt.title.clone(),
            params: attempt.params.clone(),
            meta: attempt.result.meta.clone(),
            ended_at: attempt.ended_at,
            output_dir: PathBuf::new(),
            outputs: attempt
                .result
                .files
                .keys()
                .map(|name| (name.clone(), PathBuf::new()))
                .collect(),
        };
        locate_outputs(&mut context, &self.dir);
        context
    }

    /// Queues the hook of `attempt`, which succeeded, to run once the hooks
    /// of attempts that ended before it have. If there is no hook for its
    /// service and bundle by now, the entry is removed instead.
    ///
    /// Call it once the attempt's `done` is queued for its service, so that
    /// the hook never runs before the service hears how the attempt ended
    /// (spec/machine.md §2.1).
    pub fn ready(mut self, attempt: Succeeded) {
        let context = self.context(&attempt);
        let hook = self
            .inner
            .home
            .hook(&context.service.id, &context.bundle.name);
        // The file is the whole setup: without it, there is nothing to run.
        if is_absent(&hook) {
            return;
        }
        let not_run = match self.hand_off.result() {
            Some(Err(why)) => Some(format!("moil couldn't keep the attempt's outputs: {why}")),
            // Otherwise the outputs are checked as the hook starts.
            Some(Ok(())) | None => None,
        };
        let mut state = self.inner.state();
        let queued = Queued {
            order: state.next_order,
            not_run,
            context,
        };
        let path = self.dir.join(QUEUED);
        let mut json = serde_json::to_vec_pretty(&queued).expect("a queued hook serializes");
        json.push(b'\n');
        if let Err(error) = write_atomic(&path, &json, PRIVATE) {
            tracing::warn!(
                "couldn't queue the hook for {} attempt {}, so it won't run: {}",
                queued.context.job_id,
                queued.context.attempt,
                with_path(error, &path)
            );
            return;
        }
        state.next_order += 1;
        let entry = Entry {
            dir: self.dir.clone(),
            queued,
        };
        state.ready.insert((entry.queued.order, self.number), entry);
        drop(state);
        self.queued = true;
        self.inner.wake.notify_one();
    }
}

impl Drop for PendingHook {
    fn drop(&mut self) {
        if !self.queued {
            remove_entry(&self.dir);
        }
    }
}

impl Inner {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs ready hooks, one at a time, until moil quits.
    async fn work(self: Arc<Self>) {
        let supervisor = Process::current();
        loop {
            let entry = tokio::select! {
                biased;
                () = self.quit.cancelled() => return,
                entry = self.next() => entry,
            };
            let ending = self.run(&entry, supervisor.as_ref()).await;
            if matches!(ending, Some(Ending::Quit)) {
                // It runs again from the start next time.
                for leftover in [OUT, TMP] {
                    remove_quietly(&entry.dir.join(leftover));
                }
                return;
            }
            remove_entry(&entry.dir);
        }
    }

    /// The first entry ready to run, once there is one.
    async fn next(&self) -> Entry {
        loop {
            if let Some((_, entry)) = self.state().ready.pop_first() {
                return entry;
            }
            self.wake.notified().await;
        }
    }

    /// Runs the hook of `entry`, logging how it went, unless its file is
    /// gone. Returns how it ended.
    async fn run(&self, entry: &Entry, supervisor: Option<&Process>) -> Option<Ending> {
        let context = &entry.queued.context;
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
        log.line(&format!(
            "{} {} attempt {}",
            crate::rfc3339::format(SystemTime::now()),
            context.job_id,
            context.attempt
        ));
        let ending = match &entry.queued.not_run {
            Some(why) => Ending::NotRun(why.clone()),
            None => self.run_hook(entry, &hook, &log, supervisor).await,
        };
        log.line(&ending.to_string());
        Some(ending)
    }

    async fn run_hook(
        &self,
        entry: &Entry,
        hook: &Path,
        log: &Log,
        supervisor: Option<&Process>,
    ) -> Ending {
        if let Err(why) = runnable(hook) {
            return Ending::NotRun(why.to_string());
        }
        let context = &entry.queued.context;
        let out = &context.output_dir;
        if let Err(why) = give_outputs(&entry.dir.join(OUTPUTS), out, context.outputs.keys()) {
            return Ending::NotRun(why);
        }
        let tmp = entry.dir.join(TMP);
        if let Err(error) = moil_runtime::remove_tree(&tmp).and_then(|()| fs::create_dir(&tmp)) {
            return Ending::NotRun(format!(
                "moil couldn't make its TMPDIR, {}: {error}",
                tmp.display()
            ));
        }

        let mut command = Command::new(hook);
        command
            .env_clear()
            .envs(self.environment(&tmp))
            .current_dir(out)
            .stdin(Stdio::piped())
            .stdout(log.stdio())
            .stderr(log.stdio());
        let mut group = match Group::spawn(command) {
            Ok(group) => group,
            Err(error) => return Ending::NotRun(cant_start(&error)),
        };
        let started = Instant::now();
        let running = entry.dir.join(RUNNING);
        if let Some(supervisor) = supervisor {
            let record = Running {
                supervisor: supervisor.clone(),
                group: Some(group.leader()),
            };
            let json = serde_json::to_vec(&record).expect("a record serializes");
            if let Err(error) = write_atomic(&running, &json, PRIVATE) {
                tracing::warn!(
                    "couldn't record a running hook, so moil can't stop it if it crashes: {}",
                    with_path(error, &running)
                );
            }
        }
        let mut line = serde_json::to_vec(context).expect("a hook context serializes");
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
        remove_quietly(&running);
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
    NotRun(String),
    /// Should never happen: its exit status couldn't be read.
    Unknown {
        error: String,
        after: Duration,
    },
}

impl Ending {
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
    /// Opens the log at `path`, first renaming it to `<name>.1` if it has
    /// passed [`MAX_LOG_BYTES`] (spec/machine.md §2.3).
    fn open(path: PathBuf) -> Self {
        let file = open_log(&path)
            .map_err(|error| {
                tracing::warn!(
                    "couldn't open a hook's log, so what the hook prints is lost: {}",
                    with_path(error, &path)
                );
            })
            .ok();
        Self { path, file }
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

fn open_log(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        fs::rename(path, old)?;
    }
    File::options()
        .read(true)
        .append(true)
        .create(true)
        .open(path)
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

/// Why moil can't run what is at a hook's path (spec/machine.md §2.1).
#[derive(Debug)]
enum Unrunnable {
    Folder,
    BrokenLink,
    NotAFile,
    NotExecutable,
    NoInterpreter,
    Unreadable(io::Error),
}

impl fmt::Display for Unrunnable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Folder => f.write_str("it's a folder, not a file"),
            Self::BrokenLink => f.write_str("it's a symbolic link to a file that doesn't exist"),
            Self::NotAFile => f.write_str("it isn't a regular file"),
            Self::NotExecutable => f.write_str("it isn't executable. Run chmod +x on it."),
            Self::NoInterpreter => f.write_str(
                "it doesn't start with a #! line, such as #!/bin/sh, and isn't a program",
            ),
            Self::Unreadable(error) => write!(f, "moil can't look at it: {error}"),
        }
    }
}

/// Whether moil can run the hook at `path`: a regular file, or a symbolic
/// link to one, that its owner can execute, and that says what runs it.
fn runnable(path: &Path) -> Result<(), Unrunnable> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound && fs::symlink_metadata(path).is_ok() => {
            return Err(Unrunnable::BrokenLink);
        }
        Err(e) => return Err(Unrunnable::Unreadable(e)),
    };
    if metadata.is_dir() {
        Err(Unrunnable::Folder)
    } else if !metadata.is_file() {
        Err(Unrunnable::NotAFile)
    } else if metadata.permissions().mode() & 0o100 == 0 {
        Err(Unrunnable::NotExecutable)
    } else if !is_executable_format(path) {
        Err(Unrunnable::NoInterpreter)
    } else {
        Ok(())
    }
}

/// Whether the file at `path` starts as one the system can execute does:
/// with `#!`, or as a Mach-O or ELF program. libc runs any other file with
/// `/bin/sh` instead, but a hook's file must say what runs it
/// (spec/machine.md §2.1). A file moil can't read is left to the system.
fn is_executable_format(path: &Path) -> bool {
    const PROGRAMS: [[u8; 4]; 9] = [
        *b"\x7fELF",
        // Mach-O, 32 and 64 bits, either byte order.
        [0xfe, 0xed, 0xfa, 0xce],
        [0xce, 0xfa, 0xed, 0xfe],
        [0xfe, 0xed, 0xfa, 0xcf],
        [0xcf, 0xfa, 0xed, 0xfe],
        // Universal binaries.
        [0xca, 0xfe, 0xba, 0xbe],
        [0xbe, 0xba, 0xfe, 0xca],
        [0xca, 0xfe, 0xba, 0xbf],
        [0xbf, 0xba, 0xfe, 0xca],
    ];
    let mut start = [0; 4];
    let read = File::open(path).and_then(|file| {
        let mut read = 0;
        while read < start.len() {
            match file.read_at(&mut start[read..], read as u64)? {
                0 => break,
                n => read += n,
            }
        }
        Ok(read)
    });
    match read {
        Ok(read) => start[..read].starts_with(b"#!") || PROGRAMS.contains(&start),
        Err(_) => true,
    }
}

/// Whether nothing at all is at `path`, not even a broken link.
fn is_absent(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// The queue in `queue_dir` as a crash or quit left it: kills what's left
/// of hooks that were running, and removes entries that aren't ready.
fn recover(queue_dir: &Path) -> io::Result<State> {
    let mut state = State::default();
    for item in fs::read_dir(queue_dir)? {
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
        state.next_number = state.next_number.max(number + 1);
        match recover_entry(&dir) {
            Ok(Some(queued)) => {
                state.next_order = state.next_order.max(queued.order + 1);
                state
                    .ready
                    .insert((queued.order, number), Entry { dir, queued });
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                "couldn't clean up after a hook that was running when moil stopped; \
                 moil tries again when it next starts: {}",
                with_path(error, &dir)
            ),
        }
    }
    Ok(state)
}

/// The hook to run for the entry at `dir`, if it is to run.
fn recover_entry(dir: &Path) -> io::Result<Option<Queued>> {
    let running = dir.join(RUNNING);
    if let Some(record) = read_record::<Running>(&running) {
        // Another process runs it; with the home's lock, that can only be
        // a moil that is about to exit.
        if record.supervisor.is_running() {
            return Ok(None);
        }
        if let Some(group) = &record.group {
            group.kill_group()?;
        }
        fs::remove_file(&running)?;
        tracing::info!(
            "a hook was running when moil stopped; it runs again from the start: {}",
            dir.display()
        );
    }
    match read_record::<Queued>(&dir.join(QUEUED)) {
        // Wherever the home is now.
        Some(mut queued) => {
            locate_outputs(&mut queued.context, dir);
            Ok(Some(queued))
        }
        // The attempt never ended, or its record is damaged.
        None => {
            moil_runtime::remove_tree(dir)?;
            Ok(None)
        }
    }
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
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!("couldn't read {}: {error}", path.display());
            return None;
        }
    };
    serde_json::from_slice(&bytes)
        .map_err(|error| tracing::warn!("ignoring {}, which is damaged: {error}", path.display()))
        .ok()
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

/// A home's hooks, and what's wrong with them: for `moil hooks`, `moil
/// status` and the app.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookScan {
    /// Every hook, by service and then bundle, whether or not moil can run
    /// it.
    pub hooks: Vec<HookListing>,
    /// One for each hook moil can't run, and for each file among a
    /// service's hooks that can't be one.
    pub problems: Vec<Problem>,
}

impl HookScan {
    /// The hooks of service `id`, as its report lists them.
    pub fn for_service(&self, id: &ServiceId) -> Vec<HookReport> {
        self.hooks
            .iter()
            .filter(|listing| listing.service == *id)
            .map(|listing| listing.hook.clone())
            .collect()
    }
}

/// Looks for hooks in every service folder of `home` (spec/machine.md §2).
/// Hidden files are passed over. What can't be read counts as missing.
pub fn scan(home: &Home) -> HookScan {
    let mut scan = HookScan::default();
    for id in sorted_names(&home.services_dir())
        .into_iter()
        .filter_map(|name| ServiceId::new(name).ok())
        .filter(|id| home.service_dir(id).is_dir())
    {
        let hooks_dir = home.service_dir(&id).join("hooks");
        if fs::metadata(&hooks_dir).is_ok_and(|m| !m.is_dir()) {
            scan.problems.push(Problem {
                path: relative(home, &hooks_dir),
                message: "moil looks for this service's hooks in a folder named hooks, \
                          but this is a file"
                    .into(),
            });
            continue;
        }
        for name in sorted_names(&hooks_dir) {
            if name.starts_with('.') {
                continue;
            }
            if !moil_proto::ids::is_bundle_name(&name) {
                scan.problems.push(Problem {
                    path: relative(home, &hooks_dir.join(&name)),
                    message: "moil never runs this file: a hook is named exactly after its \
                              bundle, in lowercase letters, digits and hyphens, such as \
                              hooks/transcribe"
                        .into(),
                });
                continue;
            }
            let path = home.hook(&id, &name);
            let runnable = runnable(&path);
            if let Err(why) = &runnable {
                scan.problems.push(Problem {
                    path: relative(home, &path),
                    message: format!("moil can't run this hook: {why}"),
                });
            }
            scan.hooks.push(HookListing {
                service: id.clone(),
                hook: HookReport {
                    log: home.hook_log(&id, &name),
                    bundle: name,
                    path,
                    runnable: runnable.is_ok(),
                },
            });
        }
    }
    scan
}

/// The names in `dir`, sorted, or none if it can't be read.
fn sorted_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|item| item.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// `path` relative to the home, as problems name it.
fn relative(home: &Home, path: &Path) -> PathBuf {
    path.strip_prefix(home.root()).unwrap_or(path).to_owned()
}

/// Links `.machine/bin/moil` and `.machine/bin/uv` to `moil` and `uv`,
/// replacing whatever is there, so that hooks find this machine's own
/// first on their `PATH` (spec/machine.md §2.2) and a command can point to
/// the agent's own moil (§4).
///
/// The process running the agent calls it when it starts, with its own
/// `moil` and the uv it runs jobs with. The links point to where the files
/// really are, whatever links lead there.
pub fn link_bin(home: &Home, moil: &Path, uv: &Path) -> io::Result<()> {
    let bin = home.bin_dir();
    fs::create_dir_all(&bin).map_err(|e| with_path(e, &bin))?;
    for (name, target) in [("moil", moil), ("uv", uv)] {
        let target = target.canonicalize().map_err(|e| with_path(e, target))?;
        let link = bin.join(name);
        if fs::read_link(&link).is_ok_and(|current| current == target) {
            continue;
        }
        let failed = |error: io::Error| {
            io::Error::new(
                error.kind(),
                format!(
                    "couldn't link {} to {}: {error}",
                    link.display(),
                    target.display()
                ),
            )
        };
        let staging = bin.join(format!(".{name}.{}.tmp", std::process::id()));
        remove_quietly(&staging);
        std::os::unix::fs::symlink(&target, &staging).map_err(failed)?;
        if fs::symlink_metadata(&link).is_ok_and(|m| m.is_dir()) {
            moil_runtime::remove_tree(&link).map_err(failed)?;
        }
        fs::rename(&staging, &link).map_err(failed)?;
    }
    Ok(())
}
