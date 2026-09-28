//! Hooks end to end: shell scripts run as the owner's hooks, with real
//! files, logs and process groups (spec/machine.md §2).

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use moil_agent::hooks::{BundleRef, HookOptions, Hooks, Succeeded, link_bin, scan};
use moil_agent::report::{HookReport, Problem, ServiceRef};
use moil_agent::{Home, ServiceId};
use moil_proto::{FileInfo, FileName, JobId, JobResult};
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

fn service() -> ServiceId {
    ServiceId::new("klisi.example.com").unwrap()
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

/// An attempt of `bundle` that succeeded with `outputs`.
fn attempt(bundle: &str, job_id: &str, outputs: &[&str]) -> Succeeded {
    Succeeded {
        service: ServiceRef {
            id: service(),
            name: "klisi".into(),
            base_url: "https://klisi.example.com/moil".parse().unwrap(),
        },
        bundle: BundleRef {
            name: bundle.into(),
            version: "0.3.0".into(),
            hash: "42d30e3fb23030627d993330b1e2917a0ac2f4abef567fc6e82328d8acedeae6"
                .parse()
                .unwrap(),
        },
        job_id: JobId::new(job_id).unwrap(),
        attempt: 2,
        title: Some("Weekly sync".into()),
        params: json!({"language": "en"}),
        result: JobResult {
            meta: Some(json!({"speakers": 3})),
            files: outputs
                .iter()
                .map(|name| {
                    let info = FileInfo {
                        size_bytes: 0,
                        sha256: "0".repeat(64),
                    };
                    (FileName::new(*name).unwrap(), info)
                })
                .collect(),
        },
        ended_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000),
    }
}

/// Does what the agent and the runtime do for an attempt that succeeded:
/// makes an entry, hands the outputs off to it, and marks it ready.
fn queue(hooks: &Hooks, attempt: Succeeded, outputs: &[(&str, &str)]) {
    let pending = hooks.entry().unwrap();
    let hand_off = pending.hand_off();
    fs::create_dir_all(hand_off.dir()).unwrap();
    for (name, contents) in outputs {
        fs::write(hand_off.dir().join(name), contents).unwrap();
    }
    hand_off.set_result(Ok(()));
    pending.ready(attempt);
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
async fn a_hook_gets_its_context_on_stdin_and_runs_among_its_outputs() {
    let t = Test::new();
    t.hook(
        "transcribe",
        r#"#!/bin/sh
cat > REPORT/stdin
pwd -P > REPORT/cwd
ls -A > REPORT/listing
cat transcript.txt > REPORT/transcript.txt
echo "$TMPDIR" > REPORT/tmpdir
test -d "$TMPDIR" && touch "$TMPDIR/scratch" && echo yes > REPORT/tmp-usable
echo to stdout
echo to stderr >&2
printf 'without a newline'
exit 3
"#,
    );
    let hooks = t.start();
    queue(
        &hooks,
        attempt(
            "transcribe",
            "recording-1",
            &["transcript.txt", "transcript.vtt"],
        ),
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
                "id": "klisi.example.com",
                "name": "klisi",
                "base_url": "https://klisi.example.com/moil"
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

    // It ran in a directory holding the outputs and nothing else.
    assert_eq!(t.reported("cwd").trim_end(), output_dir.to_str().unwrap());
    assert_eq!(t.reported("listing"), "transcript.txt\ntranscript.vtt\n");
    assert_eq!(t.reported("transcript.txt"), "hello");
    assert_eq!(t.reported("tmp-usable"), "yes\n");

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
    assert!(!Path::new(t.reported("tmpdir").trim_end()).exists());
    assert!(!output_dir.exists());
}

#[tokio::test]
async fn a_hook_gets_only_the_environment_the_spec_lists() {
    let t = Test::new();
    // A symbolic link to a program is a hook too.
    let hook = t.home.hook(&service(), "transcribe");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/usr/bin/env", &hook).unwrap();
    let hooks = t.start();
    queue(&hooks, attempt("transcribe", "recording-1", &[]), &[]);
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

    link_bin(&t.home, &moil, &linked_uv).unwrap();
    link_bin(&t.home, &moil, &linked_uv).unwrap();
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
    queue(&hooks, attempt("transcribe", "recording-1", &[]), &[]);
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
    queue(&hooks, attempt("transcribe", "recording-1", &[]), &[]);
    t.closed("transcribe", 1).await;
    assert_eq!(fs::read_to_string(&old).unwrap(), full);
    queue(&hooks, attempt("transcribe", "recording-2", &[]), &[]);
    let current = t.closed("transcribe", 2).await;

    // The new log holds both runs; a log under the limit stays put.
    assert_eq!(current.matches("\nfresh\n").count(), 2, "{current}");
    assert!(current.starts_with("== "), "{current}");
    assert_eq!(fs::read_to_string(&old).unwrap(), full);
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
    queue(&hooks, attempt("polite", "recording-1", &[]), &[]);
    queue(&hooks, attempt("stubborn", "recording-2", &[]), &[]);

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
    queue(&hooks, attempt("transcribe", "recording-1", &[]), &[]);
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
        queue(
            &hooks,
            attempt(bundle, "recording-1", &["a.txt"]),
            &[("a.txt", "a")],
        );
    }
    // The runtime couldn't hand the outputs off.
    let pending = hooks.entry().unwrap();
    pending
        .hand_off()
        .set_result(Err("couldn't move output a.txt: disk full".into()));
    pending.ready(attempt("outputs-lost", "recording-1", &["a.txt"]));

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
            path: "services/klisi.example.com/hooks/not-executable".into(),
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
    fs::write(hooks_dir.join("transcribe.sh"), "").unwrap();
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
            ("klisi.example.com", "broken", false),
            ("klisi.example.com", "transcribe", true),
            ("localhost-8787", "summarize", true),
        ]
    );
    assert_eq!(
        scan.for_service(&other),
        [HookReport {
            bundle: "summarize".into(),
            path: elsewhere,
            log: t.home.hook_log(&other, "summarize"),
            runnable: true,
        }]
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
                "services/klisi.example.com/hooks/broken",
                "moil can't run this hook: it's a symbolic link to a file that doesn't exist"
            ),
            (
                "services/klisi.example.com/hooks/transcribe.sh",
                "moil never runs this file: a hook is named exactly after its bundle, in \
                 lowercase letters, digits and hyphens, such as hooks/transcribe"
            ),
        ]
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
    for bundle in ["first", "a", "b", "c", "gone"] {
        recording_hook(&t, bundle);
    }
    let hooks = t.start();
    // Entries are made as attempts start, and marked ready as they end.
    let [a, b, c, gone] = [(); 4].map(|()| hooks.entry().unwrap());
    queue(&hooks, attempt("first", "recording-0", &[]), &[]);
    until("the first hook to start", || {
        fs::read_to_string(t.report.join("order")).is_ok()
    })
    .await;
    c.ready(attempt("c", "recording-3", &[]));
    gone.ready(attempt("gone", "recording-4", &[]));
    a.ready(attempt("a", "recording-1", &[]));
    b.ready(attempt("b", "recording-2", &[]));
    // Removing its file turns a hook off, for attempts that already ended
    // too.
    fs::remove_file(t.home.hook(&service(), "gone")).unwrap();
    fs::write(t.report.join("release"), "").unwrap();

    t.closed("b", 1).await;
    assert_eq!(
        t.reported("order"),
        "start first\nend first\nstart c\nend c\nstart a\nend a\nstart b\nend b\n"
    );
    until("the entries to go", || t.queued().is_empty()).await;
    assert_eq!(t.log("gone"), "");
}

#[tokio::test]
async fn attempts_without_a_hook_leave_nothing_queued() {
    let t = Test::new();
    let hooks = t.start();
    let failed = hooks.entry().unwrap();
    let without_hook = hooks.entry().unwrap();
    // One that moil crashed during, as far as the queue can tell.
    std::mem::forget(hooks.entry().unwrap());
    assert_eq!(t.queued().len(), 3);

    drop(failed);
    without_hook.ready(attempt("transcribe", "recording-1", &[]));
    assert_eq!(t.queued().len(), 1);
    hooks.shutdown().await;

    let _hooks = t.start();
    assert_eq!(t.queued(), [] as [PathBuf; 0]);
}

/// A hook that moves its output away, as a hook may, and waits for SIGTERM
/// the first time it runs.
const MOVING_HOOK: &str = r#"#!/bin/sh
echo run >> REPORT/runs
mv transcript.txt REPORT/kept.txt
if [ "$(wc -l < REPORT/runs)" -eq 1 ]; then
  echo $$ > REPORT/pid.tmp && mv REPORT/pid.tmp REPORT/pid
  exec sleep 30
fi
"#;

#[tokio::test]
async fn a_hook_moil_quits_during_runs_again_with_the_same_outputs() {
    let t = Test::new();
    t.hook("transcribe", MOVING_HOOK);
    let hooks = t.start();
    queue(
        &hooks,
        attempt("transcribe", "recording-1", &["transcript.txt"]),
        &[("transcript.txt", "hello")],
    );
    until("the hook to start", || t.report.join("pid").exists()).await;
    fs::remove_file(t.report.join("kept.txt")).unwrap();

    hooks.shutdown().await;
    let log = t.log("transcribe");
    assert_eq!(closing_lines(&log), ["stopped: moil quit"], "{log}");
    assert!(!is_running(&t.reported("pid")));
    let [entry] = &t.queued()[..] else {
        panic!("the entry is kept");
    };
    let mut kept: Vec<_> = fs::read_dir(entry)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    kept.sort();
    assert_eq!(kept, ["hook.json", "outputs"]);

    let _hooks = t.start();
    let log = t.closed("transcribe", 2).await;
    let ["stopped: moil quit", second] = closing_lines(&log)[..] else {
        panic!("{log}");
    };
    assert!(exited(second, 0), "{log}");
    assert_eq!(t.reported("runs"), "run\nrun\n");
    assert_eq!(t.reported("kept.txt"), "hello");
    until("the entry to go", || t.queued().is_empty()).await;
}

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
        attempt("transcribe", "recording-1", &["transcript.txt"]),
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
    let log = t.closed("transcribe", 1).await;
    // The crashed run never got its closing line.
    let headers = log.lines().filter(|l| l.starts_with("== 20")).count();
    assert_eq!(headers, 2, "{log}");
    let [closing] = closing_lines(&log)[..] else {
        panic!("{log}");
    };
    assert!(exited(closing, 0), "{log}");
    assert_eq!(t.reported("runs"), "run\nrun\n");
    assert_eq!(t.reported("kept.txt"), "hello");
    until("the entry to go", || t.queued().is_empty()).await;
}
