//! The machine owner's commands, run as the real `moil` binary.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// How long a test waits for the agent to say something.
const PATIENCE: Duration = Duration::from_secs(10);

/// `moil` with its home at `home`, keeping tokens in a file there. The
/// agent never gets as far as running a job, so uv needn't exist.
fn moil(home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_moil"));
    command
        .args(args)
        .env("MOIL_HOME", home)
        .env("MOIL_SECRETS", "file")
        .env("MOIL_UV", home.join("no-uv-here"))
        .env("RUST_LOG", "info")
        .stdin(Stdio::null());
    command
}

/// A running `moil agent` and what it has logged so far.
struct Agent {
    child: Child,
    lines: mpsc::Receiver<String>,
    log: Vec<String>,
}

impl Agent {
    fn start(home: &Path) -> Self {
        let mut child = moil(home, &["agent"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            log: Vec::new(),
        }
    }

    /// Waits until the agent logs a line containing `text`, failing if it
    /// exits first.
    fn logged(&mut self, text: &str) {
        loop {
            match self.lines.recv_timeout(PATIENCE) {
                Ok(line) => {
                    let found = line.contains(text);
                    self.log.push(line);
                    if found {
                        return;
                    }
                }
                Err(_) => panic!(
                    "the agent never said {text:?}; it said:\n{}",
                    self.log.join("\n")
                ),
            }
        }
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_cleanup_that_fails_is_a_warning_as_in_the_app() {
    let home = tempfile::tempdir().unwrap();
    // Where the records of running attempts go, a file is in the way.
    std::fs::create_dir_all(home.path().join("jobs")).unwrap();
    std::fs::write(home.path().join("jobs/.attempts"), "not a directory").unwrap();

    let mut agent = Agent::start(home.path());
    agent.logged("couldn't clean up attempts left over from a crash");
    agent.logged("no paired services yet");
    assert!(agent.is_running(), "{}", agent.log.join("\n"));
}

#[test]
fn what_only_reads_works_while_the_agent_runs() {
    let home = tempfile::tempdir().unwrap();
    // A service paired before, whose token is gone: the agent gives up on
    // it without reaching for the network.
    let service = r#"{"id": "klisi.example.com", "base_url": "https://klisi.example.com/moil",
        "name": "klisi", "machine_id": "m_1",
        "paired_at": {"secs_since_epoch": 1790000000, "nanos_since_epoch": 0}, "approvals": []}"#;
    std::fs::create_dir_all(home.path().join("services")).unwrap();
    std::fs::write(home.path().join("services/klisi.example.com.json"), service).unwrap();
    let mut agent = Agent::start(home.path());
    agent.logged("klisi removed this machine");

    let listed = moil(home.path(), &["services"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.status.success() && stdout.contains("klisi.example.com"),
        "{stdout}{}",
        String::from_utf8_lossy(&listed.stderr)
    );

    // What changes the machine needs it to itself.
    let hash = "0".repeat(64);
    let revoked = moil(home.path(), &["revoke", "klisi", &hash])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&revoked.stderr);
    assert!(
        !revoked.status.success() && stderr.contains("already running"),
        "{stderr}"
    );
    assert!(agent.is_running(), "{}", agent.log.join("\n"));
}
