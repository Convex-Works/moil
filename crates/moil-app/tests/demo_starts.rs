//! The demo starts: the event loop runs, the tray icon appears and nothing
//! panics; and it quits as a signal asks. It shows an icon in the menu bar
//! for a moment, so it only runs when asked:
//! `cargo test -p moil-app --test demo_starts -- --ignored`.

#![cfg(target_os = "macos")]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[test]
#[ignore = "shows a tray icon; run with --ignored"]
fn the_demo_starts() {
    let mut app = Command::new(env!("CARGO_BIN_EXE_moil-app"))
        .arg("--demo")
        .env("RUST_LOG", "info")
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = app.stderr.take().unwrap();
    let (lines, received) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });

    let mut log = Vec::new();
    let ready = loop {
        match received.recv_timeout(Duration::from_secs(30)) {
            Ok(line) => {
                let ready = line.contains("moil is running in the menu bar");
                log.push(line);
                if ready {
                    break true;
                }
            }
            Err(_) => break false,
        }
    };
    let exited = app.try_wait().unwrap();
    let _ = app.kill();
    let _ = app.wait();
    assert!(
        ready,
        "the app never said it was running:\n{}",
        log.join("\n")
    );
    assert_eq!(exited, None, "the app exited:\n{}", log.join("\n"));
    assert!(
        !log.iter()
            .any(|l| l.contains("panicked") || l.contains("ERROR")),
        "{}",
        log.join("\n")
    );
}

#[test]
#[ignore = "shows a tray icon; run with --ignored"]
fn the_demo_shuts_its_agent_down_when_a_signal_asks() {
    let mut app = Command::new(env!("CARGO_BIN_EXE_moil-app"))
        .arg("--demo")
        .env("RUST_LOG", "info")
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = app.stderr.take().unwrap();
    let (lines, received) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let mut log = Vec::new();
    let mut said = |text: &str| loop {
        match received.recv_timeout(Duration::from_secs(30)) {
            Ok(line) => {
                let found = line.contains(text);
                log.push(line);
                if found {
                    return true;
                }
            }
            Err(_) => return false,
        }
    };
    assert!(said("moil is running in the menu bar"));

    let sent = Command::new("kill")
        .args(["-TERM", &app.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
    let heard = said("quitting, as a signal asks");
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = app.try_wait().unwrap() {
            break Some(status);
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = app.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = app.wait();
    assert!(heard, "the app didn't hear the signal");
    assert!(
        status.is_some_and(|s| s.success()),
        "the app ended with {status:?} instead of shutting down"
    );
}
