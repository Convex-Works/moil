//! The machine owner's commands (spec/machine.md §3), run as the real
//! `moil` binary against a fake service over real HTTP and WebSockets,
//! with `moil agent` running on the home and without one.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;

use moil_agent::report::{
    BundleReview, ConnectionReport, HookListing, Host, ListedBundle, Outcome, ServiceReport, Status,
};
use moil_proto::control::Assign;
use moil_proto::{BundleHash, MachineMessage, Phase, ServiceMessage};
use serde::{Deserialize, Serialize};
use serde_json::json;
use support::*;

/// Whether a test's commands find `moil agent` running on the home, or
/// work on the home itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum With {
    Home,
    Agent,
}

impl With {
    fn start(self, home: &Home) -> Option<Agent> {
        (self == Self::Agent).then(|| home.agent())
    }
}

// The JSON of `Label: value` lines (spec/machine.md §3.1).

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Code {
    code: String,
    url: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Paired {
    paired: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Approved {
    approved: BundleHash,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Revoked {
    revoked: BundleHash,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Unpaired {
    unpaired: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Paused {
    paused: bool,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stopping {
    stopping: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HomePath {
    home: String,
}

/// What a command prints with `--json` when it has no `Label: value` line
/// to print.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Nothing {}

#[test]
fn pairing_reviewing_approving_and_unpairing_work_with_or_without_an_agent() {
    for with in [With::Home, With::Agent] {
        let home = Home::new();
        let service = FakeService::start("klisi");
        let transcribe = bundle("transcribe", "1.0.0", "print('hi')");
        service.host(&transcribe);
        let (hash, text_hash) = (transcribe.hash(), transcribe.hash().to_string());
        let _agent = with.start(&home);

        // The code, then, once the owner confirms it, the service.
        let base_url = service.base_url.as_str();
        let args = [base_url, "--no-browser", "--name", "Ada's Mac", "--json"];
        let mut pairing = Pairing::start(&home, &args);
        let code: Code = parse(&pairing.next_line());
        assert_eq!(code.code, CODE, "{with:?}");
        assert_eq!(code.url, format!("{base_url}/pair?code={CODE}"));
        service.confirm();
        let id = parse::<Paired>(&pairing.next_line()).paired;
        pairing.finish().success();
        assert_eq!(service.names(), ["Ada's Mac"], "{with:?}");
        // The agent connects to the service at once.
        let channel = (with == With::Agent).then(|| service.accept());

        let listed: Vec<ListedBundle> = home.run(&["review", "klisi", "--json"]).success().json();
        assert_eq!(listed.len(), 1, "{with:?}");
        assert_eq!((listed[0].hash, listed[0].approved), (hash, false));
        let review: BundleReview = home
            .run(&["review", &id, &text_hash, "--json"])
            .success()
            .json();
        assert_eq!(review.service.id.as_str(), id);
        assert!(!review.approved);
        let files: Vec<(&str, &str)> = review
            .files
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_str()))
            .collect();
        assert_eq!(files, transcribe.files().collect::<Vec<_>>());

        // As text, the bundle in full before the result.
        let approved = home
            .run(&["approve", "klisi", &text_hash, "--yes"])
            .success();
        let shown = approved.stdout();
        assert!(shown.contains("==> job.py <==\n"), "{shown}");
        assert!(shown.ends_with(&format!("Approved: {hash}\n")), "{shown}");
        let again = home
            .run(&["approve", "klisi", &text_hash, "--yes", "--json"])
            .success();
        assert_eq!(again.json::<Approved>(), Approved { approved: hash });
        assert_label_rule(&shown, &again.stdout());
        if let Some(channel) = &channel {
            channel.received(
                "the approval",
                |m| matches!(m, MachineMessage::Approved { hashes } if hashes == &[hash]),
            );
        }

        let status: Status = home.run(&["status", "--json"]).success().json();
        let reported = &status.services[0];
        assert_eq!(reported.approvals[0].hash, hash, "{with:?}");
        match with {
            With::Agent => {
                assert_eq!(status.agent.map(|a| a.host), Some(Host::Agent));
                let offered = reported.offered.as_ref().expect("offered, while connected");
                assert!(offered[0].approved);
            }
            With::Home => {
                assert_eq!(status.agent, None);
                assert_eq!((&reported.connection, &reported.offered), (&None, &None));
            }
        }

        let revoked = home.run(&["revoke", "klisi", &text_hash]).success();
        assert_eq!(revoked.stdout(), format!("Revoked: {hash}\n"));
        if let Some(channel) = &channel {
            channel.received(
                "the revocation",
                |m| matches!(m, MachineMessage::Approved { hashes } if hashes.is_empty()),
            );
        }
        home.run(&["approve", "klisi", &text_hash, "--yes", "--json"])
            .success();
        let json = home
            .run(&["revoke", "klisi", &text_hash, "--json"])
            .success();
        assert_eq!(json.json::<Revoked>(), Revoked { revoked: hash });
        assert_label_rule(&revoked.stdout(), &json.stdout());
        let run = home
            .run(&["revoke", "klisi", &text_hash, "--json"])
            .failure(1);
        assert!(run.stderr().contains("isn't approved"), "{}", run.stderr());
        assert_eq!(run.stdout(), "");

        let unpaired = home.run(&["unpair", "klisi", "--json"]).success();
        assert_eq!(unpaired.json::<Unpaired>(), Unpaired { unpaired: id });
        assert_eq!(service.forgotten().len(), 1, "{with:?}");
        let services: Vec<ServiceReport> = home.run(&["services", "--json"]).success().json();
        assert!(!services[0].paired, "the folder stays, unpaired");
    }
}

#[test]
fn pausing_works_with_or_without_an_agent() {
    for with in [With::Home, With::Agent] {
        let home = Home::new();
        let _agent = with.start(&home);
        for (command, paused) in [("pause", true), ("resume", false)] {
            let text = home.run(&[command]).success();
            assert_eq!(text.stdout(), format!("Paused: {paused}\n"), "{with:?}");
            let json = home.run(&[command, "--json"]).success();
            assert_eq!(json.json::<Paused>(), Paused { paused });
            assert_label_rule(&text.stdout(), &json.stdout());
            let status: Status = home.run(&["status", "--json"]).success().json();
            assert_eq!(status.paused, paused, "{with:?}");
            // The agent saves it as it goes on.
            let saved = format!("\"paused\": {paused}");
            until("the setting to be saved", || {
                let settings = home.path().join("settings.json");
                std::fs::read_to_string(settings).is_ok_and(|s| s.contains(&saved))
            });
        }
    }
}

#[test]
fn what_needs_an_agent_says_so_and_exits_with_3() {
    let home = Home::new();
    for args in [
        &["stop"][..],
        &["stop", "--json"],
        &["quit"],
        &["login-item"],
        &["login-item", "on"],
        &["status", "--watch"],
        &["status", "--watch", "--json"],
    ] {
        let run = home.run(args).failure(3);
        let said = run.stderr();
        let root = home.path().display();
        assert!(
            said.starts_with(&format!("Error: moil isn't running on {root}, ")),
            "{args:?}: {said}"
        );
        assert_eq!(run.stdout(), "", "{args:?}");
    }
}

#[test]
fn what_needs_an_agent_works_with_one() {
    let home = Home::new();
    let mut agent = home.agent();

    let stopped = home.run(&["stop"]).success();
    assert_eq!(stopped.stdout(), "", "no job runs");
    assert!(stopped.stderr().contains("No job is running."));
    home.run(&["stop", "--json"]).success().json::<Nothing>();

    let run = home.run(&["login-item", "on"]).failure(1);
    assert!(
        run.stderr().contains("only the moil app can open at login"),
        "{}",
        run.stderr()
    );

    // The status, then again whenever it changes.
    let mut watch = home
        .moil(&["status", "--watch", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(watch.stdout.take().unwrap()).lines();
    let mut next = || -> Status { parse(&lines.next().unwrap().unwrap()) };
    let first = next();
    assert!(!first.paused);
    assert_eq!(first.agent.map(|a| a.host), Some(Host::Agent));
    home.run(&["pause"]).success();
    while !next().paused {}

    let quit = home.run(&["quit", "--json"]).success();
    quit.json::<Nothing>();
    assert!(quit.stderr().contains("`moil agent` has stopped."));
    assert!(agent.exited().success(), "{}", agent.log.join("\n"));
    assert!(wait(&mut watch).success(), "the watch ends with the agent");
    let status: Status = home.run(&["status", "--json"]).success().json();
    assert_eq!(status.agent, None);
    assert!(status.paused, "the agent kept it in settings.json");
}

#[test]
fn stop_stops_the_running_job() {
    let home = Home::with_uv();
    let service = FakeService::start("klisi");
    let sleeper = bundle("sleeper", "1.0.0", "import time\ntime.sleep(600)");
    service.host(&sleeper);
    let id = home.pair(&service, &service.base_url);
    let hash = sleeper.hash().to_string();
    home.run(&["approve", &id, &hash, "--yes"]).success();
    let _agent = home.agent();
    let channel = service.accept();
    channel.send(&ServiceMessage::Assign(Assign {
        job_id: "j1".parse().unwrap(),
        attempt: 1,
        bundle_hash: sleeper.hash(),
        title: Some("Weekly sync".into()),
        params: json!({}),
        inputs: Default::default(),
        outputs: Default::default(),
        lease_ms: 60_000,
        timeout_ms: None,
    }));
    until("the job to run", || {
        let status: Status = home.run(&["status", "--json"]).success().json();
        status.job.is_some_and(|job| job.phase == Phase::Running)
    });
    let status = home.run(&["status"]).success().stdout();
    assert!(status.contains("Job: j1 attempt 1\n"), "{status}");
    assert!(status.contains("  Title: Weekly sync\n"), "{status}");

    let stopped = home.run(&["stop", "--json"]).success();
    let stopping = "j1 attempt 1".to_owned();
    assert_eq!(stopped.json::<Stopping>(), Stopping { stopping });
    channel.received("the attempt's end", |m| {
        matches!(m, MachineMessage::Done(_))
    });
    until("the job to have ended", || {
        let status: Status = home.run(&["status", "--json"]).success().json();
        status
            .last_job
            .is_some_and(|job| job.outcome == Outcome::Interrupted)
    });
}

/// A service folder that came from elsewhere, such as a git clone: its
/// approvals and its hooks, but no pairing.
fn cloned_folder(home: &Home, service: &FakeService, bundle: &moil_bundle::Bundle) {
    let approval = json!({
        "hash": bundle.hash(),
        "name": bundle.manifest().name,
        "version": bundle.manifest().version,
        "approved_at": "2026-09-27T22:20:11Z",
    });
    home.service_folder("klisi", &service.base_url, "klisi", json!([approval]));
    let dir = home
        .path()
        .join("services/klisi/bundles")
        .join(&bundle.manifest().name)
        .join(bundle.hash().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    for (name, text) in bundle.files() {
        std::fs::write(dir.join(name), text).unwrap();
    }
    home.hook(
        "klisi",
        &bundle.manifest().name,
        "#!/bin/sh\ncat >/dev/null\n",
    );
}

#[test]
fn pairing_with_a_folder_from_git_keeps_its_approvals_and_hooks() {
    for with in [With::Home, With::Agent] {
        let home = Home::new();
        let service = FakeService::start("klisi");
        let transcribe = bundle("transcribe", "1.0.0", "print('hi')");
        service.host(&transcribe);
        cloned_folder(&home, &service, &transcribe);
        let _agent = with.start(&home);

        // Named as the home knows it, not by its URL.
        assert_eq!(home.pair(&service, "klisi"), "klisi", "{with:?}");
        let services: Vec<ServiceReport> = home.run(&["services", "--json"]).success().json();
        assert_eq!(services.len(), 1, "the folder is adopted: {services:?}");
        assert!(services[0].paired);
        assert_eq!(services[0].approvals[0].hash, transcribe.hash());
        let hooks: Vec<HookListing> = home.run(&["hooks", "--json"]).success().json();
        assert_eq!(hooks.len(), 1, "{with:?}");
        if with == With::Agent {
            // Connected, and telling the service what the folder approves.
            let channel = service.accept();
            let hello = channel.received("hello", |m| matches!(m, MachineMessage::Hello(_)));
            let MachineMessage::Hello(hello) = hello else {
                unreachable!()
            };
            assert_eq!(hello.approved, [transcribe.hash()]);
            until("the service to be connected", || {
                let status: Status = home.run(&["status", "--json"]).success().json();
                status.services[0].connection == Some(ConnectionReport::Connected)
            });
        }
    }
}

/// Before the control socket, `moil agent` held the home's lock, so a
/// change needed the agent stopped first.
#[test]
fn changes_also_work_while_the_agent_runs() {
    let home = Home::new();
    // A service paired before, whose token is gone: the agent gives up on
    // it without reaching for the network. Its approval doesn't count,
    // since its files are gone too.
    let hash = "0".repeat(64);
    let approval = json!({"hash": hash, "name": "transcribe", "version": "1.0.0",
        "approved_at": "2026-09-21T14:13:20Z"});
    let base_url = "https://klisi.example.com/moil";
    home.service_folder("klisi.example.com", base_url, "klisi", json!([approval]));
    let pairing = json!({"base_url": base_url, "machine_id": "m_1",
        "paired_at": "2026-09-21T14:13:20Z"});
    let folder = home.path().join("services/klisi.example.com");
    std::fs::write(folder.join("pairing.json"), pairing.to_string()).unwrap();
    let mut agent = home.agent();
    agent.logged("moil has no token for klisi");

    let listed = home.run(&["services"]).success().stdout();
    assert!(listed.contains("klisi.example.com"), "{listed}");
    assert_eq!(home.run(&["pause"]).success().stdout(), "Paused: true\n");
    let status: Status = home.run(&["status", "--json"]).success().json();
    assert!(status.paused && status.agent.is_some());
    // An approval that doesn't count is the owner's to remove too.
    let revoked = home.run(&["revoke", "klisi", &hash]).success();
    assert_eq!(revoked.stdout(), format!("Revoked: {hash}\n"));
    let file = std::fs::read_to_string(folder.join("service.json")).unwrap();
    assert!(!file.contains(&hash), "{file}");
    assert!(agent.is_running(), "{}", agent.log.join("\n"));
}

/// Whichever of an agent starting and a change made meanwhile takes the
/// home first, both work: the change is made on the home, which the agent
/// then reads, or through the agent once it answers.
#[test]
fn a_change_made_as_an_agent_starts_works_either_way() {
    for _ in 0..3 {
        let home = Home::new();
        let mut agent = Agent::spawn(&home);
        assert_eq!(home.run(&["pause"]).success().stdout(), "Paused: true\n");
        agent.logged("moil commands reach this agent");
        let status: Status = home.run(&["status", "--json"]).success().json();
        assert!(status.paused && status.agent.is_some(), "{status:?}");
    }
}

#[test]
fn approve_asks_only_at_a_terminal() {
    let home = Home::new();
    let service = FakeService::start("klisi");
    let transcribe = bundle("transcribe", "1.0.0", "print('hi')");
    service.host(&transcribe);
    home.pair(&service, &service.base_url);
    let hash = transcribe.hash().to_string();

    // Stdin stays open, and isn't a terminal: moil doesn't wait on it.
    let mut approving = home
        .moil(&["approve", "klisi", &hash])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = approving.stdin.take().unwrap();
    let status = wait(&mut approving);
    let output = approving.wait_with_output().unwrap();
    drop(stdin);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(status.code(), Some(1), "{stdout}{stderr}");
    let way = format!("moil approve klisi {hash} --yes");
    assert!(stderr.contains(&way), "{stderr}");
    assert!(
        stdout.contains("==> job.py <=="),
        "the bundle, to read: {stdout}"
    );

    let run = home.run(&["approve", "klisi", &hash, "--json"]).failure(2);
    assert!(run.stderr().contains("--yes"), "{}", run.stderr());
    let services: Vec<ServiceReport> = home.run(&["services", "--json"]).success().json();
    assert!(services[0].approvals.is_empty(), "nothing was approved");
}

#[test]
fn the_agent_points_at_moil_approve_which_works_while_it_runs() {
    let home = Home::new();
    let service = FakeService::start("klisi");
    let transcribe = bundle("transcribe", "1.0.0", "print('hi')");
    service.host(&transcribe);
    home.pair(&service, &service.base_url);
    let mut agent = home.agent();
    let channel = service.accept();

    let hint = agent.logged("which you haven't approved");
    let command = hint
        .split_once("approve it with `moil ")
        .and_then(|(_, rest)| rest.split_once('`'))
        .map(|(command, _)| command.to_owned())
        .unwrap_or_else(|| panic!("the agent didn't say how to approve: {hint}"));
    let mut args: Vec<&str> = command.split(' ').collect();
    args.push("--yes");
    home.run(&args).success();
    channel.received(
        "the approval",
        |m| matches!(m, MachineMessage::Approved { hashes } if hashes == &[transcribe.hash()]),
    );
}

#[test]
fn the_agent_logs_to_the_home_links_its_own_moil_and_quits_when_asked() {
    let home = Home::new();
    let mut agent = home.agent();
    let log = std::fs::read_to_string(home.path().join("logs/moil.log")).unwrap();
    assert!(log.contains("moil commands reach this agent"), "{log}");
    let linked = std::fs::read_link(home.path().join(".machine/bin/moil")).unwrap();
    let ours = std::path::Path::new(env!("CARGO_BIN_EXE_moil"))
        .canonicalize()
        .unwrap();
    assert_eq!(linked, ours);

    let second = home.run(&["agent"]).failure(1);
    assert!(
        second.stderr().contains("already running"),
        "{}",
        second.stderr()
    );

    home.run(&["quit"]).success();
    assert!(agent.exited().success(), "{}", agent.log.join("\n"));
    agent.logged("shutting down");
}

#[test]
fn a_cleanup_that_fails_is_a_warning_as_in_the_app() {
    let home = Home::new();
    // Where the records of running attempts go, a file is in the way.
    let jobs = home.path().join(".machine/jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(jobs.join(".attempts"), "not a directory").unwrap();

    let mut agent = home.agent();
    agent.logged("couldn't clean up attempts left over from an earlier run");
    agent.logged("no paired services yet");
    assert!(agent.is_running(), "{}", agent.log.join("\n"));
}

#[test]
fn hooks_are_listed_from_the_home_alone() {
    let home = Home::new();
    let base_url = "https://klisi.example.com/moil";
    home.service_folder("klisi", base_url, "klisi", json!([]));
    home.hook("klisi", "transcribe", "#!/bin/sh\nexit 0\n");
    home.hook("klisi", "summarize", "echo nothing says what runs this\n");

    let hooks: Vec<HookListing> = home.run(&["hooks", "--json"]).success().json();
    let summary: Vec<(&str, &str, bool)> = hooks
        .iter()
        .map(|h| (h.service.as_str(), h.hook.bundle.as_str(), h.hook.runnable))
        .collect();
    assert_eq!(
        summary,
        [("klisi", "summarize", false), ("klisi", "transcribe", true)]
    );
    let services = home.path().join("services");
    let hook = &hooks[1].hook;
    assert_eq!(hook.path, services.join("klisi/hooks/transcribe"));
    assert_eq!(hook.log, services.join("klisi/logs/transcribe.log"));

    let text = home.run(&["hooks"]).success().stdout();
    assert!(text.contains("Hook: klisi transcribe\n"), "{text}");
    assert!(
        text.contains("Problem: services/klisi/hooks/summarize: moil can't run this hook"),
        "{text}"
    );
}

#[test]
fn logs_prints_the_last_lines_and_follows_across_rotation() {
    let home = Home::new();
    let log = home.path().join("logs/moil.log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "one\ntwo\nthree\n").unwrap();
    assert_eq!(home.run(&["logs"]).success().stdout(), "one\ntwo\nthree\n");
    assert_eq!(
        home.run(&["logs", "-n", "2"]).success().stdout(),
        "two\nthree\n"
    );

    let mut following = home
        .moil(&["logs", "-f", "-n", "1"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(following.stdout.take().unwrap()).lines();
    let mut next = || lines.next().unwrap().unwrap();
    assert_eq!(next(), "three");
    let append = |text: &str| {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&log)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    };
    append("four\n");
    assert_eq!(next(), "four");
    // Renamed aside as the agent does it, then started afresh.
    append("five\n");
    std::fs::rename(&log, log.with_extension("log.1")).unwrap();
    append("six\n");
    assert_eq!(next(), "five");
    assert_eq!(next(), "six");
    append("\x1b[2Jseven\n");
    assert_eq!(next(), "␛[2Jseven", "hidden characters revealed");
    following.kill().unwrap();
    following.wait().unwrap();

    // A hook's log.
    let base_url = "https://klisi.example.com/moil";
    home.service_folder("klisi", base_url, "klisi", json!([]));
    let hook_log = home.path().join("services/klisi/logs/transcribe.log");
    std::fs::create_dir_all(hook_log.parent().unwrap()).unwrap();
    let written = "== 2026-09-27T22:41:08Z j1 attempt 1\nkept\n== exited 0 after 1s\n";
    std::fs::write(&hook_log, written).unwrap();
    let printed = home.run(&["logs", "klisi", "transcribe"]).success();
    assert_eq!(printed.stdout(), written);
    home.run(&["logs", "nobody", "transcribe"]).failure(1);
}

#[test]
fn a_wrong_command_line_exits_with_2() {
    let home = Home::new();
    for args in [
        &["review"][..],
        &["logs", "klisi"],
        &["logs", "klisi", "Not A Bundle"],
        &["approve", "klisi", "not-a-hash"],
        &["login-item", "maybe"],
        &["status", "--no-such-option"],
    ] {
        home.run(args).failure(2);
    }
    let run = home.run(&["review", "nobody", "--json"]).failure(1);
    assert!(
        run.stderr().contains("no service matches"),
        "{}",
        run.stderr()
    );
    assert_eq!(run.stdout(), "");
}

#[test]
fn home_prints_where_the_home_is() {
    let home = Home::new();
    let text = home.run(&["home"]).success();
    assert_eq!(text.stdout(), format!("Home: {}\n", home.path().display()));
    let json = home.run(&["home", "--json"]).success();
    let path = home.path().display().to_string();
    assert_eq!(json.json::<HomePath>(), HomePath { home: path });
    assert_label_rule(&text.stdout(), &json.stdout());
}

#[test]
fn every_command_explains_itself_with_an_example() {
    let home = Home::new();
    for command in [
        "status",
        "pause",
        "resume",
        "stop",
        "pair",
        "unpair",
        "services",
        "review",
        "approve",
        "revoke",
        "hooks",
        "logs",
        "home",
        "login-item",
        "quit",
        "agent",
        "init",
        "check",
        "hash",
        "run",
    ] {
        let help = home.run(&[command, "--help"]).success().stdout();
        assert!(help.contains("\nExample"), "{command}:\n{help}");
    }
    let top = home.run(&["--help"]).success().stdout();
    let owners = top.find("Your machine:").expect("the owner's commands");
    let authors = top
        .find("Bundle authoring:")
        .expect("the authoring commands");
    assert!(owners < authors, "{top}");
}

/// Whoever reads only `moil hooks --help` can write a hook that works.
#[test]
fn the_hooks_help_gives_the_whole_contract() {
    let home = Home::new();
    let help = home.run(&["hooks", "--help"]).success().stdout();
    for part in [
        "<home>/services/<service ID>/hooks/<bundle name>",
        "after an attempt of a bundle succeeds",
        "one at a time, in the order their attempts ended",
        "#! line",
        "chmod +x",
        "one line of JSON, then end-of-file",
        "\"output_dir\"",
        "\"outputs\"",
        "In output_dir, as its working directory",
        "HOME, USER, LANG, MOIL_HOME",
        "TMPDIR",
        "PATH, which is <home>/.machine/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        "for at most an hour",
        "logs/<bundle name>.log",
        "== exited <status> after <duration>",
        "== stopped: moil stopped unexpectedly",
        "== not run: <reason>",
        "runs again from the start",
        "cope with running twice",
        "Treat them as untrusted",
        "#!/usr/bin/env -S uv run --script",
    ] {
        assert!(help.contains(part), "{part:?} in\n{help}");
    }
}
