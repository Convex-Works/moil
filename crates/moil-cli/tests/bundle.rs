//! The bundle authoring commands, run as the real `moil` binary with real
//! uv and Python.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

/// The example bundle every author starts from.
fn echo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/bundles/echo")
}

fn uv() -> PathBuf {
    moil_runtime::find_uv()
        .or_else(|| {
            let installed = PathBuf::from(std::env::var_os("HOME")?).join(".local/bin/uv");
            installed.is_file().then_some(installed)
        })
        .expect("these tests run real uv: install it, or set MOIL_UV to it")
}

/// A place to work in, with its own cache so nothing lands in the
/// developer's.
struct Workspace {
    dir: tempfile::TempDir,
}

impl Workspace {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `moil` with `args`, run in the workspace.
    fn moil(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_moil"));
        command
            .args(args)
            .current_dir(self.path())
            .env("MOIL_UV", uv())
            .env("XDG_CACHE_HOME", self.path().join(".cache"))
            .env(
                "UV_CACHE_DIR",
                Path::new(env!("CARGO_TARGET_TMPDIR")).join("uv-cache"),
            )
            .env_remove("MOIL_HOME");
        command
    }

    fn run(&self, args: &[&str]) -> Run {
        Run(self.moil(args).output().unwrap())
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path().join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }
}

struct Run(Output);

impl Run {
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.0.stdout).into_owned()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.0.stderr).into_owned()
    }

    #[track_caller]
    fn success(self) -> Self {
        assert!(
            self.0.status.success(),
            "moil failed with {}\nstdout:\n{}\nstderr:\n{}",
            self.0.status,
            self.stdout(),
            self.stderr()
        );
        self
    }

    #[track_caller]
    fn failure(self, code: i32) -> Self {
        assert_eq!(
            self.0.status.code(),
            Some(code),
            "stdout:\n{}\nstderr:\n{}",
            self.stdout(),
            self.stderr()
        );
        self
    }
}

#[test]
fn init_makes_a_bundle_that_checks_and_runs_out_of_the_box() {
    let ws = Workspace::new();
    let init = ws.run(&["init", "Hello World"]).success();
    let dir = ws.path().join("Hello World");
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["name"], "hello-world");

    let hash = ws.run(&["hash", "Hello World"]).success().stdout();
    let hash = hash.trim();
    assert_eq!(hash.len(), 64);
    assert!(init.stdout().contains(hash));

    let check = ws.run(&["check", "Hello World"]).success().stdout();
    assert!(check.starts_with("hello-world 0.1.0\n"), "{check}");
    assert!(check.contains(&format!("hash      {hash}")), "{check}");
    assert!(check.contains("ok: "), "{check}");

    let run = Run(ws
        .moil(&["run", "--params", r#"{"name": "Ada"}"#])
        .current_dir(&dir)
        .output()
        .unwrap())
    .success()
    .stdout();
    assert!(run.contains("  info   hello, Ada\n"), "{run}");
    assert!(
        run.contains("succeeded\n  meta   {\"greeting\":\"hello, Ada\"}"),
        "{run}"
    );
}

#[test]
fn init_leaves_existing_bundles_and_bad_names_alone() {
    let ws = Workspace::new();
    ws.run(&["init", "b"]).success();
    let job = std::fs::read_to_string(ws.path().join("b/job.py")).unwrap();
    let again = ws.run(&["init", "b"]).failure(1);
    assert!(
        again.stderr().contains("already exists"),
        "{}",
        again.stderr()
    );
    assert_eq!(
        std::fs::read_to_string(ws.path().join("b/job.py")).unwrap(),
        job
    );

    let bad = ws.run(&["init", "c", "--name", "Not_A_Name"]).failure(1);
    assert!(
        bad.stderr().contains("can't be a bundle name"),
        "{}",
        bad.stderr()
    );
    assert!(!ws.path().join("c").exists());
}

#[test]
fn hash_is_the_sha256sum_listing_the_spec_describes() {
    let ws = Workspace::new();
    let hash = ws
        .run(&["hash", echo().to_str().unwrap()])
        .success()
        .stdout();
    // Spec §5.2: anyone can check a hash with standard tools.
    let listing = Command::new("sh")
        .arg("-c")
        .arg("shasum -a 256 job.py job.py.lock manifest.json | shasum -a 256")
        .current_dir(echo())
        .output()
        .unwrap();
    let expected = String::from_utf8(listing.stdout).unwrap();
    assert_eq!(hash.trim(), expected.split_whitespace().next().unwrap());
}

#[test]
fn check_marks_packages_built_from_source() {
    let ws = Workspace::new();
    let bundle = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/bundles/from-source");
    let check = ws
        .run(&["check", bundle.to_str().unwrap()])
        .success()
        .stdout();
    let index = "from https://packages.example.com/simple";
    assert!(
        check.contains(&format!(
            "  plain 1.0  {index}  (no wheel: built from source on each machine, with build tools it doesn't pin)\n"
        )),
        "{check}"
    );
    assert!(
        check.contains(&format!("  wheeled 2.0  {index}\n")),
        "{check}"
    );
}

#[test]
fn check_explains_a_lockfile_that_no_longer_matches() {
    let ws = Workspace::new();
    ws.run(&["init", "b"]).success();
    let script = ws.path().join("b/job.py");
    let edited = std::fs::read_to_string(&script)
        .unwrap()
        .replace("# dependencies = []", "# dependencies = [\"idna==3.20\"]");
    std::fs::write(&script, edited).unwrap();

    let check = ws.run(&["check", "b"]).failure(1);
    assert!(
        check
            .stderr()
            .contains("job.py.lock doesn't match the dependencies in job.py")
            && check.stderr().contains("uv lock --script job.py"),
        "{}",
        check.stderr()
    );
}

#[test]
fn check_says_what_is_wrong_with_an_invalid_bundle() {
    let ws = Workspace::new();
    ws.run(&["init", "b"]).success();
    std::fs::write(
        ws.path().join("b/manifest.json"),
        r#"{"moil": 1, "name": "b", "version": "1", "licence": "MIT"}"#,
    )
    .unwrap();
    let check = ws.run(&["check", "b"]).failure(1);
    assert!(
        check.stderr().contains("manifest.json") && check.stderr().contains("licence"),
        "{}",
        check.stderr()
    );
}

#[test]
fn run_moves_inputs_and_outputs_like_a_real_job() {
    let ws = Workspace::new();
    ws.write("text.txt", "first line\nsecond line\n");
    let echo = echo();
    let run = ws
        .run(&[
            "run",
            echo.to_str().unwrap(),
            "--input",
            "text.txt=text.txt",
            "--output",
            "echo.txt",
            "--out-dir",
            "results",
            "--params",
            r#"{"shout": true}"#,
        ])
        .success()
        .stdout();
    assert_eq!(
        std::fs::read_to_string(ws.path().join("results/echo.txt")).unwrap(),
        "FIRST LINE\nSECOND LINE\n"
    );
    let expected = "\
preparing
downloading
running
  info   echoing 2 lines
  data   {\"line\":1,\"text\":\"FIRST LINE\"}
  50%    line 1 of 2
  data   {\"line\":2,\"text\":\"SECOND LINE\"}
  100%   line 2 of 2
uploading
succeeded
  meta   {\"lines\":2,\"shouted\":true}
  results/echo.txt  23 bytes  sha256 ";
    assert!(run.starts_with(expected), "{run}");
}

#[test]
fn run_json_prints_events_then_the_outcome() {
    let ws = Workspace::new();
    let params = ws.write("params.json", r#"{"text": "one\n"}"#);
    let echo = echo();
    let run = ws
        .run(&[
            "run",
            echo.to_str().unwrap(),
            "--json",
            "--output",
            "echo.txt",
            "--params",
            &format!("@{}", params.display()),
        ])
        .success()
        .stdout();
    let lines: Vec<Value> = run
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        lines[..3],
        [
            json!({"v": 1, "type": "phase", "phase": "preparing"}),
            json!({"v": 1, "type": "phase", "phase": "downloading"}),
            json!({"v": 1, "type": "phase", "phase": "running"}),
        ]
    );
    let outcome = lines.last().unwrap();
    assert_eq!(outcome["outcome"], "succeeded");
    assert_eq!(
        outcome["result"]["meta"],
        json!({"lines": 1, "shouted": false})
    );
    assert_eq!(
        outcome["result"]["files"]["echo.txt"],
        json!({
            "size_bytes": 4,
            "sha256": "2c8b08da5ce60398e1f19af0e5dccc744df274b826abe585eaba68c525434806"
        })
    );
}

#[test]
fn a_failed_job_fails_the_command_with_its_error() {
    let ws = Workspace::new();
    let echo = echo();
    let run = ws
        .run(&[
            "run",
            echo.to_str().unwrap(),
            "--params",
            r#"{"fail": "the input is empty", "retryable": true}"#,
        ])
        .failure(1)
        .stdout();
    assert!(
        run.contains("failed: script_error: the input is empty\n  exit status 1\n  another attempt could succeed"),
        "{run}"
    );
}

#[test]
fn run_stops_a_job_that_runs_out_of_time() {
    let ws = Workspace::new();
    let echo = echo();
    let run = ws
        .run(&[
            "run",
            echo.to_str().unwrap(),
            "--params",
            r#"{"text": "a\nb\n", "delay_ms": 60000}"#,
            "--timeout",
            "1.5",
        ])
        .failure(1)
        .stdout();
    assert!(
        run.contains("failed: timeout: the attempt ran past its time limit of 1500 ms"),
        "{run}"
    );
}

#[test]
fn ctrl_c_stops_the_job_and_cleans_up() {
    let ws = Workspace::new();
    let home = ws.path().join("home");
    let echo = echo();
    let mut moil = ws
        .moil(&[
            "--home",
            home.to_str().unwrap(),
            "run",
            echo.to_str().unwrap(),
            "--params",
            r#"{"text": "a\nb\n", "delay_ms": 60000}"#,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(moil.stdout.take().unwrap());
    let mut seen = String::new();
    while !seen.contains("line 1 of 2") {
        assert_ne!(
            stdout.read_line(&mut seen).unwrap(),
            0,
            "moil ended early: {seen}"
        );
    }
    Command::new("kill")
        .args(["-INT", &moil.id().to_string()])
        .status()
        .unwrap();
    let status = moil.wait().unwrap();
    std::io::Read::read_to_string(&mut stdout, &mut seen).unwrap();

    assert_eq!(status.code(), Some(130), "{seen}");
    assert!(seen.contains("failed: interrupted"), "{seen}");
    let jobs: Vec<_> = std::fs::read_dir(home.join("jobs"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|name| !name.to_string_lossy().starts_with('.'))
        .collect();
    assert!(jobs.is_empty(), "working directories left: {jobs:?}");
}

#[test]
fn keep_leaves_the_working_directory_for_debugging() {
    let ws = Workspace::new();
    let echo = echo();
    let run = ws
        .run(&[
            "run",
            echo.to_str().unwrap(),
            "--keep",
            "--output",
            "echo.txt",
        ])
        .success();
    let stderr = run.stderr();
    let kept = stderr
        .lines()
        .find_map(|line| line.strip_prefix("moil: kept the working directory "))
        .unwrap_or_else(|| panic!("{stderr}"));
    let kept = Path::new(kept);
    assert_eq!(
        std::fs::read_to_string(kept.join("out/echo.txt")).unwrap(),
        "hello from moil\n"
    );
    std::fs::remove_dir_all(kept.parent().unwrap().parent().unwrap()).unwrap();
}
