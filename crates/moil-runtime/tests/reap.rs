//! Cleaning up after a crash: the process supervising an attempt is killed
//! mid-attempt, and a new runtime on the same home cleans up what it left.

mod common;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use common::*;
use moil_proto::{JobEvent, JobId};
use moil_runtime::{Executor, Job, Runtime, RuntimeConfig, stop_signal};
use serde_json::{Value, json};

const HOME_VAR: &str = "MOIL_TEST_CRASH_HOME";
const STEPS_VAR: &str = "MOIL_TEST_CRASH_STEPS";
const MARK: &str = "moil-test-crash: ";

fn config(home: &Path) -> RuntimeConfig {
    let mut config = RuntimeConfig::new(uv(), home);
    config.extra_env = uv_cache_env();
    config
}

/// Not a test of its own: the supervising process that [`crash_after`]
/// starts and kills. Runs the probe with the steps it's given and prints
/// the pids the script reports.
#[tokio::test]
#[ignore = "run by the other tests in this file, in a process of its own"]
async fn supervise_until_killed() {
    let (Some(home), Ok(steps)) = (std::env::var_os(HOME_VAR), std::env::var(STEPS_VAR)) else {
        return;
    };
    let runtime = Runtime::try_new(config(Path::new(&home))).unwrap();
    let job = Job {
        id: JobId::new("crashing").unwrap(),
        attempt: 1,
        bundle: probe(),
        params: json!({"steps": serde_json::from_str::<Value>(&steps).unwrap()}),
        inputs: Default::default(),
        outputs: Default::default(),
        timeout: None,
        url_policy: Default::default(),
    };
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (_stopper, stop) = stop_signal();
    let attempt = tokio::spawn(async move { runtime.run(job, events, stop).await });
    while let Some(event) = received.recv().await {
        if let JobEvent::Data { payload } = event
            && payload.get("pid").is_some()
        {
            println!("{MARK}{payload}");
        }
    }
    attempt.await.unwrap();
}

/// Runs an attempt of the probe with `steps` in a separate process and
/// kills that process with `SIGKILL` once the script has reported its pids,
/// which it returns.
fn crash_after(home: &Path, steps: Value) -> (i32, i32, i32) {
    let mut supervisor = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "supervise_until_killed",
            "--ignored",
            "--nocapture",
        ])
        .env(HOME_VAR, home)
        .env(STEPS_VAR, steps.to_string())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = BufReader::new(supervisor.stdout.take().unwrap());
    let pids: Value = stdout
        .lines()
        .find_map(|line| Some(serde_json::from_str(line.unwrap().strip_prefix(MARK)?).unwrap()))
        .expect("the supervisor printed the script's pids");
    supervisor.kill().unwrap();
    supervisor.wait().unwrap();
    let pid = |key: &str| pids[key].as_i64().unwrap() as i32;
    (pid("uv"), pid("script"), pid("pid"))
}

/// Working directories left in `jobs_dir`.
fn work_dirs(jobs_dir: &Path) -> Vec<String> {
    std::fs::read_dir(jobs_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect()
}

#[tokio::test]
async fn a_crashed_runs_script_is_killed_and_its_directory_removed() {
    let home = tempfile::tempdir().unwrap();
    let (uv, script, stubborn) = crash_after(home.path(), json!([{"spawn": true}, {"sleep": 600}]));
    assert!(process_exists(uv) && process_exists(script) && process_exists(stubborn));
    let runtime = Runtime::new(config(home.path()));
    assert_eq!(work_dirs(&runtime.config().jobs_dir), ["crashing.1"]);

    assert_eq!(runtime.reap_orphans().unwrap(), 1);

    for pid in [uv, script, stubborn] {
        assert!(gone(pid).await, "process {pid} survived");
    }
    assert!(work_dirs(&runtime.config().jobs_dir).is_empty());
    assert_eq!(
        runtime.reap_orphans().unwrap(),
        0,
        "there's nothing left to do"
    );
}

#[tokio::test]
async fn leftovers_are_killed_after_the_script_itself_has_exited() {
    let home = tempfile::tempdir().unwrap();
    // Once nobody reads its output, the script exits, and uv with it; the
    // grandchild it started stays behind in their process group.
    let (uv, script, stubborn) = crash_after(
        home.path(),
        json!([{"spawn": true}, {"until_orphaned": true}]),
    );
    assert!(gone(script).await && gone(uv).await);
    assert!(process_exists(stubborn));

    let runtime = Runtime::new(config(home.path()));
    assert_eq!(runtime.reap_orphans().unwrap(), 1);

    assert!(gone(stubborn).await, "the leftover grandchild survived");
    assert!(work_dirs(&runtime.config().jobs_dir).is_empty());
}

#[tokio::test]
async fn kept_working_directories_are_kept_by_reaping_too() {
    let home = tempfile::tempdir().unwrap();
    let (_, script, _) = crash_after(home.path(), json!([{"spawn": true}, {"sleep": 600}]));
    let mut config = config(home.path());
    config.keep_work_dirs = true;
    let runtime = Runtime::new(config);

    assert_eq!(runtime.reap_orphans().unwrap(), 1);

    assert!(gone(script).await);
    assert_eq!(work_dirs(&runtime.config().jobs_dir), ["crashing.1"]);
}
