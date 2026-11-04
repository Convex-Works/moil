//! Hooks end to end: shell scripts run as the owner's hooks, with real
//! files, logs and process groups (spec/machine.md §2).

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use moil_agent::hooks::{BundleRef, HookOptions, Hooks, PendingHook, scan};
use moil_agent::report::{HookReport, Problem, ServiceRef};
use moil_agent::{Home, ServiceId};
use moil_proto::{Assign, FileInfo, FileName, JobResult};
use serde_json::{Value, json};

const PATIENCE: Duration = Duration::from_secs(10);

/// Waits until `condition` holds, for what hooks do in the background.
async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let waiting = async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(PATIENCE, waiting)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// The uv hooks run: `find_uv`, or where uv's installer puts it.
fn uv() -> PathBuf {
    moil_runtime::find_uv()
        .or_else(|| {
            let installed = PathBuf::from(std::env::var_os("HOME")?).join(".local/bin/uv");
            installed.is_file().then_some(installed)
        })
        .expect("these tests run real uv: install it, or set MOIL_UV to it")
}

fn service() -> ServiceId {
    ServiceId::new("acme.example.com").unwrap()
}

/// Short limits, so that tests of them are quick.
fn options() -> HookOptions {
    HookOptions {
        time_limit: PATIENCE,
        kill_grace: Duration::from_millis(200),
    }
}

/// A home in a temporary directory, and a place for hooks to report to.
struct Test {
    _dir: tempfile::TempDir,
    home: Home,
    report: PathBuf,
}

impl Test {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Canonical, as hooks are told it.
        let root = dir.path().canonicalize().unwrap();
        let (home, report) = (root.join("home"), root.join("report"));
        fs::create_dir(&home).unwrap();
        fs::create_dir(&report).unwrap();
        Self {
            _dir: dir,
            home: Home::new(home),
            report,
        }
    }

    fn start(&self) -> Hooks {
        self.start_with(options())
    }

    fn start_with(&self, options: HookOptions) -> Hooks {
        Hooks::start(&self.home, options).unwrap()
    }

    /// Writes `script` as the hook for `bundle`, executable, with `REPORT`
    /// in it replaced by where hooks report to.
    fn hook(&self, bundle: &str, script: &str) -> PathBuf {
        let path = self.home.hook(&service(), bundle);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let script = script
            .replace("REPORT", self.report.to_str().unwrap())
            .replace("WARM_UP", "[ -n \"$MOIL_TEST_WARM_UP\" ] && exit 0");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn log(&self, bundle: &str) -> String {
        fs::read_to_string(self.home.hook_log(&service(), bundle)).unwrap_or_default()
    }

    /// Waits for the log of `bundle` to have `closing` lines starting with
    /// `==` after its headers, and returns it.
    async fn closed(&self, bundle: &str, closing: usize) -> String {
        until(&format!("{bundle}'s hook to end {closing} times"), || {
            closing_lines(&self.log(bundle)).len() >= closing
        })
        .await;
        self.log(bundle)
    }

    fn reported(&self, name: &str) -> String {
        fs::read_to_string(self.report.join(name))
            .unwrap_or_else(|e| panic!("the hook didn't report {name}: {e}"))
    }

    /// The queue's entries.
    fn queued(&self) -> Vec<PathBuf> {
        fs::read_dir(self.home.hooks_queue_dir())
            .map(|entries| entries.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default()
    }
}

/// The lines moil wrote to a log that aren't headers.
fn closing_lines(log: &str) -> Vec<&str> {
    log.lines()
        .filter_map(|line| line.strip_prefix("== "))
        .filter(|line| !line.starts_with("20"))
        .collect()
}

/// The entry the agent makes as the service assigns attempt 2 of job
/// `job_id`, of `bundle`.
fn entry(hooks: &Hooks, bundle: &str, job_id: &str) -> PendingHook {
    let service = ServiceRef {
        id: service(),
        name: "acme".into(),
        base_url: "https://acme.example.com/moil".parse().unwrap(),
    };
    let bundle = BundleRef {
        name: bundle.into(),
        version: "0.3.0".into(),
        hash: "42d30e3fb23030627d993330b1e2917a0ac2f4abef567fc6e82328d8acedeae6"
            .parse()
            .unwrap(),
    };
    let assign: Assign = serde_json::from_value(json!({
        "job_id": job_id,
        "attempt": 2,
        "bundle_hash": bundle.hash,
        "title": "Weekly sync",
        "params": {"language": "en"},
        "lease_ms": 30_000,
    }))
    .unwrap();
    hooks.entry(service, bundle, &assign)
}

/// Does what the runtime does for an attempt that succeeded with
/// `outputs`: hands them off to the entry.
fn hand_off(pending: &PendingHook, outputs: &[(&str, &str)]) {
    let hand_off = pending.hand_off();
    fs::create_dir_all(hand_off.dir()).unwrap();
    for (name, contents) in outputs {
        fs::write(hand_off.dir().join(name), contents).unwrap();
    }
    hand_off.set_result(Ok(()));
}

/// Marks the entry ready as the agent does once the attempt succeeded,
/// uploading `outputs`.
fn ready(pending: PendingHook, outputs: &[&str]) {
    let files = outputs
        .iter()
        .map(|name| {
            let info = FileInfo {
                size_bytes: 0,
                sha256: "0".repeat(64),
            };
            (FileName::new(*name).unwrap(), info)
        })
        .collect();
    let result = JobResult {
        meta: Some(json!({"speakers": 3})),
        files,
    };
    pending.ready(
        result,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000),
    );
}

/// Does what the agent and the runtime do for an attempt of `bundle` that
/// succeeded with `outputs`: makes an entry, hands the outputs off to it,
/// and marks it ready.
fn queue(hooks: &Hooks, bundle: &str, job_id: &str, outputs: &[(&str, &str)]) {
    let pending = entry(hooks, bundle, job_id);
    hand_off(&pending, outputs);
    let names: Vec<&str> = outputs.iter().map(|(name, _)| *name).collect();
    ready(pending, &names);
}

/// Runs a hook written with `WARM_UP` in it just far enough to get it past
/// the check macOS makes the first time a new executable file runs, which
/// can take longer than the time limits tests give hooks.
fn warm_up(hook: &Path) {
    let status = Command::new(hook)
        .env("MOIL_TEST_WARM_UP", "1")
        .status()
        .unwrap();
    assert!(status.success());
}

/// Whether the closing line `line` is `exited <status> after <duration>`.
fn exited(line: &str, status: i32) -> bool {
    line.strip_prefix(&format!("exited {status} after "))
        .and_then(|after| humantime::parse_duration(after).ok())
        .is_some_and(|after| after < PATIENCE)
}

fn is_running(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[tokio::test]
async fn a_hook_gets_its_context_on_stdin_and_runs_in_an_empty_directory() {
    let t = Test::new();
    t.hook(
        "transcribe",
        r#"#!/bin/sh
cat > REPORT/stdin
pwd -P > REPORT/cwd
ls -A > REPORT/cwd-listing
echo "$TMPDIR" > REPORT/tmpdir
touch scratch && echo yes > REPORT/tmp-usable
out=$(sed -E 's/.*"output_dir":"([^"]*)".*/\1/' REPORT/stdin)
ls -A "$out" > REPORT/listing
cat "$out/transcript.txt" > REPORT/transcript.txt
echo to stdout
echo to stderr >&2
printf 'without a newline'
exit 3
"#,
    );
    let hooks = t.start();
    queue(
        &hooks,
        "transcribe",
        "recording-1",
        &[("transcript.txt", "hello"), ("transcript.vtt", "WEBVTT")],
    );
    let log = t.closed("transcribe", 1).await;

    // One line of JSON, then end-of-file.
    let stdin = t.reported("stdin");
    assert_eq!(stdin.matches('\n').count(), 1, "{stdin}");
    assert!(stdin.ends_with('\n'));
    let context: Value = serde_json::from_str(&stdin).unwrap();
    let output_dir = PathBuf::from(context["output_dir"].as_str().unwrap());
    assert!(
        output_dir.starts_with(t.home.hooks_queue_dir()),
        "{context}"
    );
    let out = |name: &str| output_dir.join(name).to_str().unwrap().to_owned();
    assert_eq!(
        context,
        json!({
            "v": 1,
            "service": {
                "id": "acme.example.com",
                "name": "acme",
                "base_url": "https://acme.example.com/moil"
            },
            "bundle": {
                "name": "transcribe",
                "version": "0.3.0",
                "hash": "42d30e3fb23030627d993330b1e2917a0ac2f4abef567fc6e82328d8acedeae6"
            },
            "job_id": "recording-1",
            "attempt": 2,
            "title": "Weekly sync",
            "params": {"language": "en"},
            "meta": {"speakers": 3},
            "ended_at": "2026-09-21T14:13:20Z",
            "output_dir": output_dir,
            "outputs": {
                "transcript.txt": out("transcript.txt"),
                "transcript.vtt": out("transcript.vtt"),
            }
        })
    );

    // It ran in its TMPDIR, which was empty, and found the outputs, and
    // nothing else, in output_dir.
    let tmpdir = t.reported("tmpdir");
    assert!(
        Path::new(tmpdir.trim_end()).starts_with(t.home.hooks_queue_dir()),
        "{tmpdir}"
    );
    assert_eq!(t.reported("cwd"), tmpdir);
    assert_eq!(t.reported("cwd-listing"), "");
    assert_eq!(t.reported("tmp-usable"), "yes\n");
    assert_eq!(t.reported("listing"), "transcript.txt\ntranscript.vtt\n");
    assert_eq!(t.reported("transcript.txt"), "hello");

    // Between moil's lines, what it printed, as it printed it.
    let lines: Vec<&str> = log.lines().collect();
    let [
        header,
        "to stdout",
        "to stderr",
        "without a newline",
        closing,
    ] = lines[..]
    else {
        panic!("{log}");
    };
    let header = header.strip_prefix("== ").unwrap();
    let (time, rest) = header.split_once(' ').unwrap();
    moil_agent::rfc3339::parse(time).unwrap();
    assert_eq!(rest, "recording-1 attempt 2");
    assert!(exited(closing.strip_prefix("== ").unwrap(), 3), "{log}");

    // Nothing is left: its TMPDIR, its outputs or its entry.
    until("the entry to go", || t.queued().is_empty()).await;
    assert!(!Path::new(tmpdir.trim_end()).exists());
    assert!(!output_dir.exists());
}

/// The outputs' names come from the service, and a file such as uv.toml
/// where a `uv run --script` hook runs would change what it runs.
#[tokio::test]
async fn outputs_cant_configure_what_runs_a_hook() {
    let t = Test::new();
    t.home.link_bin(Path::new("/usr/bin/true"), &uv()).unwrap();
    // A cache of the tests' own, as the runtime's tests have.
    let cache = Path::new(env!("CARGO_TARGET_TMPDIR")).join("uv-cache");
    t.hook(
        "transcribe",
        &format!(
            r#"#!/usr/bin/env -S UV_CACHE_DIR={} uv run --script
# /// script
# dependencies = []
# ///
import json, sys

context = json.load(sys.stdin)
print("read", open(context["outputs"]["uv.toml"]).read())
"#,
            cache.display()
        ),
    );
    let hooks = t.start();
    // uv refuses to run anything where it finds a uv.toml it can't read.
    queue(
        &hooks,
        "transcribe",
        "recording-1",
        &[("uv.toml", "index-url = [\n")],
    );
    let log = t.closed("transcribe", 1).await;
    assert!(log.contains("\nread index-url = [\n"), "{log}");
    assert!(exited(closing_lines(&log)[0], 0), "{log}");
}

#[tokio::test]
async fn a_hook_gets_only_the_environment_the_spec_lists() {
    let t = Test::new();
    // A symbolic link to a program is a hook too.
    let hook = t.home.hook(&service(), "transcribe");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/usr/bin/env", &hook).unwrap();
    let hooks = t.start();
    queue(&hooks, "transcribe", "recording-1", &[]);
    let log = t.closed("transcribe", 1).await;

    let env: BTreeMap<&str, &str> = log
        .lines()
        .filter(|line| !line.starts_with("== "))
        .map(|line| line.split_once('=').unwrap())
        .collect();
    let root = t.home.root().to_str().unwrap();
    let mut expected = BTreeMap::from([
        (
            "PATH",
            format!("{root}/.machine/bin:/usr/bin:/bin:/usr/sbin:/sbin"),
        ),
        ("MOIL_HOME", root.to_owned()),
        (
            "LANG",
            std::env::var("LANG")
                .ok()
                .filter(|lang| !lang.is_empty())
                .unwrap_or("C.UTF-8".into()),
        ),
    ]);
    for name in ["HOME", "USER"] {
        if let Ok(value) = std::env::var(name) {
            expected.insert(name, value);
        }
    }
    let tmpdir = env.get("TMPDIR").copied().unwrap_or_default();
    assert!(
        Path::new(tmpdir).starts_with(t.home.hooks_queue_dir()),
        "{log}"
    );
    expected.insert("TMPDIR", tmpdir.into());
    let expected: BTreeMap<&str, &str> = expected.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert_eq!(env, expected);
}

#[tokio::test]
async fn hooks_find_this_machines_moil_and_uv_first_on_their_path() {
    let t = Test::new();
    let programs = t.report.join("programs");
    fs::create_dir(&programs).unwrap();
    let program = |name: &str| {
        let path = programs.join(name);
        fs::write(&path, format!("#!/bin/sh\necho \"{name} $*\"\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let (moil, uv) = (program("moil"), program("uv"));
    // Links that lead elsewhere, or anything else in their place, go.
    let bin = t.home.bin_dir();
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", bin.join("moil")).unwrap();
    fs::write(bin.join("uv"), "stale").unwrap();
    // Links point to the files themselves, not to other links.
    let linked_uv = t.report.join("uv-link");
    std::os::unix::fs::symlink(&uv, &linked_uv).unwrap();

    t.home.link_bin(&moil, &linked_uv).unwrap();
    t.home.link_bin(&moil, &linked_uv).unwrap();
    assert_eq!(fs::read_link(bin.join("moil")).unwrap(), moil);
    assert_eq!(fs::read_link(bin.join("uv")).unwrap(), uv);
    let mut names: Vec<_> = fs::read_dir(&bin)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["moil", "uv"], "nothing else is left behind");

    t.hook("transcribe", "#!/bin/sh\nmoil status\nuv run\n");
    let hooks = t.start();
    queue(&hooks, "transcribe", "recording-1", &[]);
    let log = t.closed("transcribe", 1).await;
    assert!(log.contains("\nmoil status\nuv run\n== exited 0"), "{log}");
}

#[tokio::test]
async fn logs_are_renamed_once_they_pass_a_mebibyte() {
    let t = Test::new();
    t.hook("transcribe", "#!/bin/sh\necho fresh\n");
    let log = t.home.hook_log(&service(), "transcribe");
    let old = log.with_file_name("transcribe.log.1");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    let full = "x".repeat((1 << 20) + 1);
    fs::write(&log, &full).unwrap();
    fs::write(&old, "older still").unwrap();

    let hooks = t.start();
    queue(&hooks, "transcribe", "recording-1", &[]);
    t.closed("transcribe", 1).await;
    assert_eq!(fs::read_to_string(&old).unwrap(), full);
    queue(&hooks, "transcribe", "recording-2", &[]);
    let current = t.closed("transcribe", 2).await;

    // The new log holds both runs; a log under the limit stays put.
    assert_eq!(current.matches("\nfresh\n").count(), 2, "{current}");
    assert!(current.starts_with("== "), "{current}");
    assert_eq!(fs::read_to_string(&old).unwrap(), full);
}

#[tokio::test]
async fn a_log_that_is_a_symbolic_link_isnt_written_through() {
    let t = Test::new();
    t.hook("transcribe", "#!/bin/sh\necho printed\ntouch REPORT/ran\n");
    // As a commit could leave it, pointing at another of the owner's files.
    let theirs = t.report.join("notes.md");
    fs::write(&theirs, "mine\n").unwrap();
    let log = t.home.hook_log(&service(), "transcribe");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&theirs, &log).unwrap();

    let hooks = t.start();
    queue(&hooks, "transcribe", "recording-1", &[]);
    until("the hook to run", || t.report.join("ran").exists()).await;
    until("the entry to go", || t.queued().is_empty()).await;
    assert_eq!(fs::read_to_string(&theirs).unwrap(), "mine\n");
    assert!(fs::symlink_metadata(&log).unwrap().is_symlink());
}

/// As a commit could leave it: the service's logs folder a link to a
/// folder of the owner's, or a hook's log a FIFO.
#[tokio::test]
async fn a_log_moil_would_have_to_write_through_a_link_or_to_a_fifo_isnt_written() {
    let t = Test::new();
    t.hook(
        "transcribe",
        "#!/bin/sh\necho printed\ntouch REPORT/transcribed\n",
    );
    let theirs = t.report.join("notes");
    fs::create_dir(&theirs).unwrap();
    // A log past the size limit, which moil would otherwise rename.
    let full = "x".repeat((1 << 20) + 1);
    fs::write(theirs.join("transcribe.log"), &full).unwrap();
    let logs = t.home.hook_log(&service(), "transcribe");
    let logs = logs.parent().unwrap();
    fs::create_dir_all(logs.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&theirs, logs).unwrap();

    let hooks = t.start();
    queue(&hooks, "transcribe", "recording-1", &[]);
    until("the hook to run", || t.report.join("transcribed").exists()).await;
    until("the entry to go", || t.queued().is_empty()).await;
    let mut names: Vec<_> = fs::read_dir(&theirs)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["transcribe.log"]);
    assert_eq!(
        fs::read_to_string(theirs.join("transcribe.log")).unwrap(),
        full
    );

    // A FIFO nobody reads would hold up every hook after this one.
    fs::remove_file(logs).unwrap();
    fs::create_dir(logs).unwrap();
    let fifo = std::ffi::CString::new(logs.join("summarize.log").to_str().unwrap()).unwrap();
    // SAFETY: `fifo` is a valid C string.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    t.hook("summarize", "#!/bin/sh\nhead -c 1000000 /dev/zero\n");
    queue(&hooks, "summarize", "recording-2", &[]);
    queue(&hooks, "transcribe", "recording-3", &[]);
    let log = t.closed("transcribe", 1).await;
    assert!(exited(closing_lines(&log)[0], 0), "{log}");
    until("the entries to go", || t.queued().is_empty()).await;
}

#[tokio::test]
async fn a_hook_that_runs_too_long_is_stopped_as_a_script_is() {
    let t = Test::new();
    // One goes when asked; the other ignores SIGTERM and is killed.
    let polite = t.hook(
        "polite",
        "#!/bin/sh\nWARM_UP\ntrap 'echo asked to stop; exit 0' TERM\nsleep 30 &\nwait\n",
    );
    let stubborn = t.hook(
        "stubborn",
        "#!/bin/sh\nWARM_UP\ntrap '' TERM\necho $$ > REPORT/stubborn\nsleep 30\n",
    );
    warm_up(&polite);
    warm_up(&stubborn);
    let hooks = t.start_with(HookOptions {
        time_limit: Duration::from_millis(300),
        ..options()
    });
    queue(&hooks, "polite", "recording-1", &[]);
    queue(&hooks, "stubborn", "recording-2", &[]);

    let polite = t.closed("polite", 1).await;
    assert!(
        polite.ends_with("\nasked to stop\n== stopped after 300ms\n"),
        "{polite}"
    );
    let stubborn = t.closed("stubborn", 1).await;
    assert!(
        stubborn.ends_with("\n== stopped after 300ms\n"),
        "{stubborn}"
    );
    assert!(!is_running(&t.reported("stubborn")));
}

#[tokio::test]
async fn nothing_a_hook_starts_outlives_it() {
    let t = Test::new();
    t.hook(
        "transcribe",
        "#!/bin/sh\nsleep 30 &\necho $! > REPORT/sleeper\n",
    );
    let hooks = t.start();
    queue(&hooks, "transcribe", "recording-1", &[]);
    let log = t.closed("transcribe", 1).await;
    assert!(exited(closing_lines(&log)[0], 0), "{log}");
    let sleeper = t.reported("sleeper");
    until("the hook's child to be killed", || !is_running(&sleeper)).await;
}

#[tokio::test]
async fn hooks_moil_cant_run_are_logged_and_reported() {
    let t = Test::new();
    let not_executable = t.hook("not-executable", "#!/bin/sh\necho ran\n");
    fs::set_permissions(&not_executable, fs::Permissions::from_mode(0o644)).unwrap();
    t.hook("no-interpreter", "echo ran\n");
    let folder = t.home.hook(&service(), "folder");
    fs::create_dir_all(&folder).unwrap();
    t.hook("outputs-lost", "#!/bin/sh\necho ran\n");
    let hooks = t.start();
    for bundle in ["not-executable", "no-interpreter", "folder"] {
        queue(&hooks, bundle, "recording-1", &[("a.txt", "a")]);
    }
    // The runtime couldn't hand the outputs off.
    let pending = entry(&hooks, "outputs-lost", "recording-1");
    pending
        .hand_off()
        .set_result(Err("couldn't move output a.txt: disk full".into()));
    ready(pending, &["a.txt"]);

    let reason = |log: &str| {
        let [closing] = closing_lines(log)[..] else {
            panic!("{log}");
        };
        closing.strip_prefix("not run: ").unwrap().to_owned()
    };
    assert_eq!(
        reason(&t.closed("not-executable", 1).await),
        "it isn't executable. Run chmod +x on it."
    );
    // Which libc would run with /bin/sh.
    assert_eq!(
        reason(&t.closed("no-interpreter", 1).await),
        "it doesn't start with a #! line, such as #!/bin/sh, and isn't a program"
    );
    assert_eq!(
        reason(&t.closed("folder", 1).await),
        "it's a folder, not a file"
    );
    assert_eq!(
        reason(&t.closed("outputs-lost", 1).await),
        "moil couldn't keep the attempt's outputs: couldn't move output a.txt: disk full"
    );
    until("the entries to go", || t.queued().is_empty()).await;

    // The same, as `moil hooks` and `moil status` tell it.
    let scan = scan(&t.home);
    let runnable: Vec<(&str, bool)> = scan
        .hooks
        .iter()
        .map(|listing| (listing.hook.bundle.as_str(), listing.hook.runnable))
        .collect();
    assert_eq!(
        runnable,
        [
            ("folder", false),
            ("no-interpreter", false),
            ("not-executable", false),
            ("outputs-lost", true),
        ]
    );
    assert_eq!(
        scan.problems[2],
        Problem {
            path: "services/acme.example.com/hooks/not-executable".into(),
            message: "moil can't run this hook: it isn't executable. Run chmod +x on it.".into(),
        }
    );
}

#[tokio::test]
async fn the_scan_lists_every_hook_by_service_and_bundle() {
    let t = Test::new();
    let hook = t.hook("transcribe", "#!/bin/sh\n");
    let other = ServiceId::new("localhost-8787").unwrap();
    let elsewhere = t.home.hook(&other, "summarize");
    fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
    // A link to an executable file counts.
    std::os::unix::fs::symlink(&hook, &elsewhere).unwrap();
    let hooks_dir = hook.parent().unwrap();
    fs::write(hooks_dir.join(".DS_Store"), "").unwrap();
    fs::write(hooks_dir.join("transcribe.tar.gz"), "").unwrap();
    std::os::unix::fs::symlink("missing", hooks_dir.join("broken")).unwrap();
    fs::create_dir_all(t.home.services_dir().join("Not A Service/hooks")).unwrap();

    let scan = scan(&t.home);

    let listed: Vec<(&str, &str, bool)> = scan
        .hooks
        .iter()
        .map(|l| (l.service.as_str(), l.hook.bundle.as_str(), l.hook.runnable))
        .collect();
    assert_eq!(
        listed,
        [
            ("acme.example.com", "broken", false),
            ("acme.example.com", "transcribe", true),
            ("localhost-8787", "summarize", true),
        ]
    );
    assert_eq!(
        scan.hooks[2].hook,
        HookReport {
            bundle: "summarize".into(),
            path: elsewhere,
            log: t.home.hook_log(&other, "summarize"),
            runnable: true,
        }
    );
    let problems: Vec<(&str, &str)> = scan
        .problems
        .iter()
        .map(|p| (p.path.to_str().unwrap(), p.message.as_str()))
        .collect();
    assert_eq!(
        problems,
        [
            (
                "services/acme.example.com/hooks/broken",
                "moil can't run this hook: it's a symbolic link to a file that doesn't exist"
            ),
            (
                "services/acme.example.com/hooks/transcribe.tar.gz",
                "moil never runs this file: a hook is named after its bundle, in lowercase \
                 letters, digits and hyphens, with at most one extension of letters and \
                 digits, such as hooks/transcribe or hooks/transcribe.py"
            ),
        ]
    );
}

#[tokio::test]
async fn a_hook_may_have_an_extension() {
    let t = Test::new();
    let hook = t.hook("transcribe.sh", "#!/bin/sh\necho ran\ntouch REPORT/ran\n");
    let hooks = t.start();
    queue(&hooks, "transcribe", "recording-1", &[]);

    // Its log is named after the bundle alone.
    let log = t.closed("transcribe", 1).await;
    assert!(log.contains("\nran\n"), "{log}");
    assert!(exited(closing_lines(&log)[0], 0), "{log}");
    t.reported("ran");
    let scan = scan(&t.home);
    assert_eq!(
        scan.hooks[0].hook,
        HookReport {
            bundle: "transcribe".into(),
            path: hook,
            log: t.home.hook_log(&service(), "transcribe"),
            runnable: true,
        }
    );
    assert_eq!(scan.problems, []);
}

#[tokio::test]
async fn a_bundle_with_more_than_one_hook_runs_none() {
    let t = Test::new();
    t.hook("transcribe", "#!/bin/sh\ntouch REPORT/plain\n");
    t.hook("transcribe.py", "#!/bin/sh\ntouch REPORT/python\n");
    let hooks = t.start();
    queue(&hooks, "transcribe", "recording-1", &[]);

    let why = "transcribe has more than one hook, transcribe and transcribe.py, so moil runs \
               none of them. Remove all but one.";
    let log = t.closed("transcribe", 1).await;
    assert_eq!(closing_lines(&log), [format!("not run: {why}")], "{log}");
    until("the entry to go", || t.queued().is_empty()).await;
    assert!(
        fs::read_dir(&t.report).unwrap().next().is_none(),
        "a hook ran"
    );

    let scan = scan(&t.home);
    let listed: Vec<(&str, &Path, bool)> = scan
        .hooks
        .iter()
        .map(|l| {
            (
                l.hook.bundle.as_str(),
                l.hook.path.as_path(),
                l.hook.runnable,
            )
        })
        .collect();
    let hooks_dir = t.home.hooks_dir(&service());
    assert_eq!(
        listed,
        [
            ("transcribe", hooks_dir.join("transcribe").as_path(), false),
            (
                "transcribe",
                hooks_dir.join("transcribe.py").as_path(),
                false
            ),
        ]
    );
    let problems: Vec<(&str, &str)> = scan
        .problems
        .iter()
        .map(|p| (p.path.to_str().unwrap(), p.message.as_str()))
        .collect();
    assert_eq!(
        problems,
        [
            ("services/acme.example.com/hooks/transcribe", why),
            ("services/acme.example.com/hooks/transcribe.py", why),
        ]
    );
}

#[tokio::test]
async fn a_file_with_two_extensions_isnt_a_hook() {
    let t = Test::new();
    t.hook("transcribe.tar.gz", "#!/bin/sh\ntouch REPORT/ran\n");
    let hooks = t.start();
    queue(&hooks, "transcribe", "recording-1", &[]);

    until("the entry to go", || t.queued().is_empty()).await;
    assert!(!t.report.join("ran").exists());
    assert_eq!(t.log("transcribe"), "");
    let scan = scan(&t.home);
    assert_eq!(scan.hooks, []);
    assert_eq!(
        scan.problems[0].path,
        Path::new("services/acme.example.com/hooks/transcribe.tar.gz")
    );
}

#[test]
fn a_hooks_folder_moil_cant_read_is_a_problem() {
    let t = Test::new();
    let hook = t.hook("transcribe", "#!/bin/sh\n");
    let hooks_dir = hook.parent().unwrap();
    fs::set_permissions(hooks_dir, fs::Permissions::from_mode(0o000)).unwrap();
    // Root reads it all the same.
    let unreadable = fs::read_dir(hooks_dir).is_err();
    let scan = scan(&t.home);
    fs::set_permissions(hooks_dir, fs::Permissions::from_mode(0o755)).unwrap();
    if !unreadable {
        return;
    }

    assert_eq!(scan.hooks, []);
    let [problem] = &scan.problems[..] else {
        panic!("{:?}", scan.problems);
    };
    assert_eq!(problem.path, Path::new("services/acme.example.com/hooks"));
    assert!(
        problem
            .message
            .starts_with("moil can't list the hooks in this folder ("),
        "{}",
        problem.message
    );
}

/// A hook for `bundle` that records when it starts and ends, and waits for
/// `REPORT/release` in between.
fn recording_hook(t: &Test, bundle: &str) {
    t.hook(
        bundle,
        &format!(
            "#!/bin/sh\necho start {bundle} >> REPORT/order\n\
             while [ ! -e REPORT/release ]; do sleep 0.01; done\n\
             echo end {bundle} >> REPORT/order\n"
        ),
    );
}

#[tokio::test]
async fn hooks_run_one_at_a_time_in_the_order_their_attempts_ended() {
    let t = Test::new();
    for bundle in ["a", "b", "gone", "c"] {
        recording_hook(&t, bundle);
    }
    let hooks = t.start();
    queue(&hooks, "a", "recording-1", &[]);
    until("the first hook to start", || {
        fs::read_to_string(t.report.join("order")).is_ok()
    })
    .await;
    // Attempts that end meanwhile wait their turn.
    queue(&hooks, "b", "recording-2", &[]);
    queue(&hooks, "gone", "recording-3", &[]);
    queue(&hooks, "c", "recording-4", &[]);
    // Removing its file turns a hook off, for attempts that already ended
    // too.
    fs::remove_file(t.home.hook(&service(), "gone")).unwrap();
    fs::write(t.report.join("release"), "").unwrap();

    t.closed("c", 1).await;
    assert_eq!(
        t.reported("order"),
        "start a\nend a\nstart b\nend b\nstart c\nend c\n"
    );
    until("the entries to go", || t.queued().is_empty()).await;
    assert_eq!(t.log("gone"), "");
}

#[tokio::test]
async fn attempts_without_a_hook_leave_nothing_queued() {
    let t = Test::new();
    let hooks = t.start();
    let failed = entry(&hooks, "transcribe", "recording-1");
    let without_hook = entry(&hooks, "transcribe", "recording-2");
    // One that moil crashed during, as far as the queue can tell.
    let crashed = entry(&hooks, "transcribe", "recording-3");
    for pending in [&failed, &without_hook, &crashed] {
        hand_off(pending, &[("a.txt", "a")]);
    }
    assert_eq!(t.queued().len(), 3);

    drop(failed);
    ready(without_hook, &["a.txt"]);
    std::mem::forget(crashed);
    assert_eq!(t.queued().len(), 1);
    hooks.shutdown().await;

    let _hooks = t.start();
    assert_eq!(t.queued(), [] as [PathBuf; 0]);
}

/// A hook that moves its output away, as a hook may, and waits for SIGTERM
/// the first time it runs.
const MOVING_HOOK: &str = r#"#!/bin/sh
echo run >> REPORT/runs
out=$(sed -E 's/.*"output_dir":"([^"]*)".*/\1/')
mv "$out/transcript.txt" REPORT/kept.txt
if [ "$(wc -l < REPORT/runs)" -eq 1 ]; then
  echo $$ > REPORT/pid.tmp && mv REPORT/pid.tmp REPORT/pid
  exec sleep 30
fi
"#;

const CRASH_HOME: &str = "MOIL_TEST_HOOKS_CRASH_HOME";

/// Not a test of its own: the process that
/// [`a_hook_left_running_by_a_crash_is_killed_and_runs_again`] starts and
/// kills. Queues the moving hook's attempt and runs hooks until killed.
#[tokio::test]
#[ignore = "run by another test in this file, in a process of its own"]
async fn run_hooks_until_killed() {
    let Some(root) = std::env::var_os(CRASH_HOME) else {
        return;
    };
    let hooks = Hooks::start(&Home::new(root), options()).unwrap();
    queue(
        &hooks,
        "transcribe",
        "recording-1",
        &[("transcript.txt", "hello")],
    );
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn a_hook_left_running_by_a_crash_is_killed_and_runs_again() {
    let t = Test::new();
    t.hook("transcribe", MOVING_HOOK);
    let mut crashing = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "run_hooks_until_killed", "--ignored"])
        .env(CRASH_HOME, t.home.root())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    until("the hook to start", || t.report.join("pid").exists()).await;
    crashing.kill().unwrap();
    crashing.wait().unwrap();
    let pid = t.reported("pid");
    assert!(is_running(&pid), "a crash leaves the hook running");
    fs::remove_file(t.report.join("kept.txt")).unwrap();

    let _hooks = t.start();
    until("the leftover hook to be killed", || !is_running(&pid)).await;
    let log = t.closed("transcribe", 2).await;
    // The crashed run gets its closing line before the next one starts.
    let lines: Vec<&str> = log.lines().collect();
    let [
        first,
        "== stopped: moil stopped unexpectedly",
        second,
        ..,
        last,
    ] = lines[..]
    else {
        panic!("{log}");
    };
    assert!(
        first.starts_with("== 20") && second.starts_with("== 20"),
        "{log}"
    );
    assert!(exited(last.strip_prefix("== ").unwrap(), 0), "{log}");
    assert_eq!(t.reported("runs"), "run\nrun\n");
    assert_eq!(t.reported("kept.txt"), "hello");
    until("the entry to go", || t.queued().is_empty()).await;
}
