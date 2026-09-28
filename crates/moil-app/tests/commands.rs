//! Commands reach the real app through the control socket it serves
//! (spec/machine.md §4, §5): it answers for Open at Login, and quits when
//! `moil quit` asks, as its Quit menu item does. The app runs its agent on
//! a home of its own, but shows an icon in the menu bar for a moment, so
//! these only run when asked:
//! `cargo test -p moil-app --test commands -- --ignored`.

#![cfg(target_os = "macos")]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use moil_agent::Home;
use moil_agent::control::{Client, ClientError};
use moil_agent::report::Host;

/// The app, running on a home in a temporary directory.
struct App {
    process: Child,
    lines: mpsc::Receiver<String>,
    log: Vec<String>,
}

impl App {
    fn start(home: &Path) -> Self {
        let mut process = Command::new(env!("CARGO_BIN_EXE_moil-app"))
            .env("MOIL_HOME", home)
            // Nothing of this machine's owner is touched: not their
            // keychain, and not what is in their home directory.
            .env("MOIL_SECRETS", "file")
            .env("HOME", home.join("owner"))
            .env("RUST_LOG", "info")
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = process.stderr.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            process,
            lines,
            log: Vec::new(),
        }
    }

    /// Waits until the app logs a line containing `text`.
    fn said(&mut self, text: &str) -> bool {
        loop {
            match self.lines.recv_timeout(Duration::from_secs(30)) {
                Ok(line) => {
                    let found = line.contains(text);
                    self.log.push(line);
                    if found {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
    }

    /// Waits up to `limit` for the app to exit.
    fn exited_within(&mut self, limit: Duration) -> Option<ExitStatus> {
        let started = Instant::now();
        while started.elapsed() < limit {
            if let Some(status) = self.process.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    fn log(&mut self) -> String {
        self.log.extend(self.lines.try_iter());
        self.log.join("\n")
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(30), future).await })
        .unwrap_or_else(|_| panic!("{what} took too long"))
}

#[test]
#[ignore = "shows a tray icon; run with --ignored"]
fn commands_reach_the_app_and_quit_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("moil");
    let home = Home::new(&root);
    let mut app = App::start(&root);
    assert!(
        app.said("moil is running in the menu bar"),
        "the app never said it was running:\n{}",
        app.log()
    );

    within("asking the app", async {
        let mut client = Client::connect(&home).await.unwrap();
        assert_eq!(client.agent().host, Host::App);
        let status = client.status().await.unwrap();
        assert_eq!(status.home, root);
        assert_eq!(status.agent.map(|a| a.host), Some(Host::App));

        // Not the installed app, so Open at Login is out of reach, and the
        // app says so, as its menu does.
        for set in [None, Some(true)] {
            match client.login_item(set).await {
                Err(ClientError::Agent(why)) => {
                    assert_eq!(why, "Open at Login works only for the installed app.");
                }
                other => panic!("login-item {set:?}: {other:?}"),
            }
        }
    });

    within("quitting", async {
        let client = Client::connect(&home).await.unwrap();
        client.quit().await.unwrap();
    });
    let status = app.exited_within(Duration::from_secs(10));
    let log = app.log();
    assert!(
        status.is_some_and(|s| s.success()),
        "the app ended with {status:?}:\n{log}"
    );
    assert!(log.contains("quitting, as `moil quit` asks"), "{log}");
    assert!(!home.socket().exists(), "the socket is gone");
    assert!(!log.contains("panicked") && !log.contains("ERROR"), "{log}");
    let kept = std::fs::read_to_string(home.log_file()).unwrap();
    assert!(kept.contains("moil is running in the menu bar"), "{kept}");

    within("asking again", async {
        assert!(matches!(
            Client::connect(&home).await,
            Err(ClientError::NoAgent(_))
        ));
    });
}
