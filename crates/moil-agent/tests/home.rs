//! A real agent and its home (spec/machine.md §1.3 and §2): the owner's
//! hooks after attempts, changes other programs make to the home while the
//! agent runs, and what `moil status` makes of both.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use moil_agent::report::{AgentInfo, HookReport, Host, Problem, Status};
use moil_agent::{Agent, AgentHandle, Connection, Home, Machine, Service, status};
use moil_bundle::Bundle;
use moil_proto::{
    DeclineReason, ErrorCode, JobError, MachineMessage, MachineState, Outcome, ServiceMessage,
};
use moil_runtime::StopReason;
use serde_json::{Value, json};
use support::*;
use tempfile::TempDir;

/// A machine paired with `klisi` with `transcribe` approved, running an
/// agent with a fake executor, and a place for hooks to report to.
struct Setup {
    dir: TempDir,
    report: TempDir,
    service: FakeService,
    paired: Service,
    bundle: Bundle,
    agent: AgentHandle,
    runs: FakeRuns,
}

impl Setup {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let machine = open_machine(dir.path());
        let service = FakeService::start("klisi").await;
        let paired = pair(&machine, &service).await;
        let bundle = bundle("transcribe", "1.0.0", "print('hi')");
        service.host(&bundle);
        let review = machine.review(&paired.id, bundle.hash()).await.unwrap();
        let paired = machine.approve(&review).unwrap();
        let (agent, runs) = start(machine);
        Self {
            dir,
            report: tempfile::tempdir().unwrap(),
            service,
            paired,
            bundle,
            agent,
            runs,
        }
    }

    fn home(&self) -> Home {
        Home::new(self.dir.path())
    }

    fn hash(&self) -> moil_proto::BundleHash {
        self.bundle.hash()
    }

    /// Accepts the machine's next channel and welcomes it, offering the
    /// setup's bundle.
    async fn connect(&self) -> Conn {
        let mut conn = self.service.accept().await;
        conn.handshake(vec![info(&self.bundle)], vec![]).await;
        conn
    }

    /// Writes `script` as the hook for `transcribe`, executable, with
    /// `REPORT` in it replaced by where hooks report to.
    fn hook(&self, script: &str) -> PathBuf {
        let path = self.home().hook(&self.paired.id, "transcribe");
        let script = script.replace("REPORT", self.report.path().to_str().unwrap());
        write(&path, &script);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn reported(&self, name: &str) -> String {
        fs::read_to_string(self.report.path().join(name))
            .unwrap_or_else(|e| panic!("the hook didn't report {name}: {e}"))
    }

    fn log(&self) -> String {
        let log = self.home().hook_log(&self.paired.id, "transcribe");
        fs::read_to_string(log).unwrap_or_default()
    }

    /// Waits for the hook's log to have `closing` closing lines, and
    /// returns them.
    async fn closed(&self, closing: usize) -> Vec<String> {
        until("the hook to end", || {
            closing_lines(&self.log()).len() >= closing
        })
        .await;
        closing_lines(&self.log())
    }

    /// Assigns attempt 1 of `job`, and returns its run once it has
    /// started.
    async fn run(&mut self, conn: &mut Conn, job: &str) -> FakeRun {
        conn.send(assign(job, 1, self.hash(), 30_000));
        while conn.recv().await != state(MachineState::Busy) {}
        self.runs.next().await
    }

    /// Stops the agent as quitting does, and starts another on the home.
    async fn restart(self) -> Self {
        within("the agent to shut down", self.agent.shutdown()).await;
        let Self {
            dir,
            report,
            service,
            paired,
            bundle,
            agent,
            runs: _,
        } = self;
        // It holds the home's lock.
        drop(agent);
        let (agent, runs) = start(open_machine(dir.path()));
        Self {
            dir,
            report,
            service,
            paired,
            bundle,
            agent,
            runs,
        }
    }

    async fn status(&self) -> Status {
        let info = AgentInfo {
            host: Host::Agent,
            version: "test".into(),
        };
        status::from_agent(&self.agent, &info).await.unwrap()
    }
}

fn start(machine: Machine) -> (AgentHandle, FakeRuns) {
    let (executor, runs) = fake_executor();
    (Agent::start_with(machine, executor, fast()), runs)
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// Changes a JSON file as an editor would, in place.
fn edit_json(path: &Path, edit: impl FnOnce(&mut Value)) {
    let mut value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    edit(&mut value);
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

/// Makes `to` hold every file `tree` holds, as a checkout does: each one
/// removed and written anew.
fn checkout(tree: &Path, to: &Path) {
    for entry in fs::read_dir(tree).unwrap() {
        let entry = entry.unwrap();
        let (from, into) = (entry.path(), to.join(entry.file_name()));
        if from.is_dir() {
            fs::create_dir_all(&into).unwrap();
            checkout(&from, &into);
        } else {
            let _ = fs::remove_file(&into);
            fs::write(&into, fs::read(&from).unwrap()).unwrap();
        }
    }
}

/// The lines moil wrote to a hook's log that aren't headers.
fn closing_lines(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|line| line.strip_prefix("== "))
        .filter(|line| !line.starts_with("20"))
        .map(str::to_owned)
        .collect()
}

fn state(state: MachineState) -> MachineMessage {
    MachineMessage::State { state }
}

fn approved(hashes: &[moil_proto::BundleHash]) -> MachineMessage {
    MachineMessage::Approved {
        hashes: hashes.to_vec(),
    }
}

/// The attempt's `done`, once the machine has sent it.
async fn done(conn: &mut Conn) -> Outcome {
    loop {
        if let MachineMessage::Done(done) = conn.recv().await {
            return done.outcome;
        }
    }
}

fn interrupted() -> Outcome {
    Outcome::failed(JobError::new(
        ErrorCode::Interrupted,
        "stopped by the owner",
    ))
}

/// The hook entries waiting to run.
fn queued(home: &Home) -> usize {
    fs::read_dir(home.hooks_queue_dir()).map_or(0, |entries| entries.count())
}

#[tokio::test]
async fn a_hook_runs_after_a_succeeded_attempt_with_its_context_and_outputs() {
    let mut setup = Setup::new().await;
    setup.hook(
        r#"#!/bin/sh
cat > REPORT/context.json
ls > REPORT/listing
cat transcript.txt > REPORT/transcript.txt
echo "transcribed $(wc -c < transcript.txt | tr -d ' ') bytes"
"#,
    );
    let mut conn = setup.connect().await;
    let run = setup.run(&mut conn, "j1").await;
    run.succeed_with(
        Some(json!({"speakers": 3})),
        &[("transcript.txt", "hello"), ("transcript.vtt", "WEBVTT")],
    );
    let outcome = done(&mut conn).await;
    assert!(matches!(outcome, Outcome::Succeeded { .. }), "{outcome:?}");
    let closing = setup.closed(1).await;
    assert!(closing[0].starts_with("exited 0 after "), "{closing:?}");

    let context: Value = serde_json::from_str(&setup.reported("context.json")).unwrap();
    let output_dir = PathBuf::from(context["output_dir"].as_str().unwrap());
    let out = |name: &str| output_dir.join(name).to_str().unwrap().to_owned();
    let ended_at = setup.agent.snapshot().last_job.unwrap().ended_at;
    assert_eq!(
        context,
        json!({
            "v": 1,
            "service": {
                "id": setup.paired.id,
                "name": "klisi",
                "base_url": setup.service.base_url,
            },
            "bundle": {
                "name": "transcribe",
                "version": "1.0.0",
                "hash": setup.hash(),
            },
            "job_id": "j1",
            "attempt": 1,
            "title": "j1 title",
            "params": {"language": "en"},
            "meta": {"speakers": 3},
            "ended_at": moil_agent::rfc3339::format(ended_at),
            "output_dir": output_dir,
            "outputs": {
                "transcript.txt": out("transcript.txt"),
                "transcript.vtt": out("transcript.vtt"),
            },
        })
    );
    assert_eq!(
        setup.reported("listing"),
        "transcript.txt\ntranscript.vtt\n"
    );
    assert_eq!(setup.reported("transcript.txt"), "hello");
    let log = setup.log();
    let lines: Vec<&str> = log.lines().collect();
    let [header, "transcribed 5 bytes", _] = lines[..] else {
        panic!("{log}");
    };
    assert!(header.ends_with(" j1 attempt 1"), "{log}");
    until("the entry to go", || queued(&setup.home()) == 0).await;
}

#[tokio::test]
async fn no_hook_runs_after_an_attempt_that_didnt_succeed() {
    let mut setup = Setup::new().await;
    setup.hook("#!/bin/sh\necho ran\n");
    let mut conn = setup.connect().await;

    let run = setup.run(&mut conn, "failed").await;
    run.finish(Outcome::failed(JobError::new(
        ErrorCode::ScriptError,
        "CUDA out of memory",
    )));
    done(&mut conn).await;
    let mut run = setup.run(&mut conn, "stopped").await;
    setup.agent.stop_job();
    assert_eq!(run.stopped().await, StopReason::Interrupted);
    assert_eq!(done(&mut conn).await, interrupted());
    let run = setup.run(&mut conn, "succeeded").await;
    run.succeed_with(None, &[]);
    done(&mut conn).await;

    // Hooks run in the order their attempts ended, so any other would
    // have run first.
    setup.closed(1).await;
    let log = setup.log();
    let headers: Vec<&str> = log.lines().filter(|l| l.starts_with("== 20")).collect();
    let [header] = headers[..] else {
        panic!("{log}");
    };
    assert!(header.ends_with(" succeeded attempt 1"), "{log}");
    until("the entries to go", || queued(&setup.home()) == 0).await;
}

#[tokio::test]
async fn a_hook_moil_quits_during_runs_again_when_it_next_starts() {
    let mut setup = Setup::new().await;
    // It waits to be stopped the first time, and keeps its output.
    setup.hook(
        r#"#!/bin/sh
echo run >> REPORT/runs
cp transcript.txt REPORT/transcript.txt
if [ "$(wc -l < REPORT/runs)" -eq 1 ]; then
  touch REPORT/started
  exec sleep 30
fi
"#,
    );
    let mut conn = setup.connect().await;
    let run = setup.run(&mut conn, "j1").await;
    run.succeed_with(None, &[("transcript.txt", "hello")]);
    done(&mut conn).await;
    conn.send(ServiceMessage::Ack(attempt("j1", 1)));
    until("the hook to start", || {
        setup.report.path().join("started").exists()
    })
    .await;

    let setup = setup.restart().await;
    // The service's channel closed with the first agent.
    setup.connect().await;
    let closing = setup.closed(2).await;
    assert_eq!(closing[0], "stopped: moil quit");
    assert!(closing[1].starts_with("exited 0 after "), "{closing:?}");
    assert_eq!(setup.reported("runs"), "run\nrun\n");
    assert_eq!(
        setup.reported("transcript.txt"),
        "hello",
        "the same outputs"
    );
    until("the entry to go", || queued(&setup.home()) == 0).await;
}

#[tokio::test]
async fn an_approval_that_git_brings_is_announced_and_its_jobs_taken() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    let v2 = bundle("transcribe", "2.0.0", "print('two')");

    // Another machine's commit: the bundle's files, and service.json
    // approving it.
    let tree = tempfile::tempdir().unwrap();
    let folder = setup.home().service_dir(&setup.paired.id);
    fs::copy(
        folder.join("service.json"),
        tree.path().join("service.json"),
    )
    .unwrap();
    edit_json(&tree.path().join("service.json"), |file| {
        let approvals = file["approvals"].as_array_mut().unwrap();
        let mut approval = approvals[0].clone();
        approval["hash"] = v2.hash().to_string().into();
        approval["version"] = "2.0.0".into();
        approvals.push(approval);
    });
    let dir = tree
        .path()
        .join("bundles/transcribe")
        .join(v2.hash().to_string());
    v2.write_to(&dir).unwrap();
    checkout(tree.path(), &folder);

    assert_eq!(conn.recv().await, approved(&[setup.hash(), v2.hash()]));
    assert_eq!(
        setup.runs.prepared().await,
        v2.hash(),
        "prepared ahead of any job"
    );
    conn.send(offer("j1", v2.hash()));
    assert_eq!(
        conn.recv().await,
        MachineMessage::Bid {
            job_id: "j1".parse().unwrap()
        }
    );
    conn.send(assign("j1", 1, v2.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    assert_eq!(setup.runs.next().await.job.bundle, v2);
}

#[tokio::test]
async fn an_approval_removed_on_disk_stops_its_running_attempt() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    let mut run = setup.run(&mut conn, "j1").await;

    let file = setup.home().service_file(&setup.paired.id);
    edit_json(&file, |file| file["approvals"] = json!([]));
    assert_eq!(run.stopped().await, StopReason::Interrupted);
    assert_eq!(conn.recv().await, approved(&[]));
    assert_eq!(done(&mut conn).await, interrupted());
    assert_eq!(conn.recv().await, state(MachineState::Idle));
    conn.send(offer("j2", setup.hash()));
    assert_eq!(
        conn.recv().await,
        MachineMessage::Decline {
            job_id: "j2".parse().unwrap(),
            reason: DeclineReason::NotApproved
        }
    );
}

#[tokio::test]
async fn a_pairing_deleted_on_disk_disconnects_and_deletes_the_token() {
    let setup = Setup::new().await;
    let mut conn = setup.connect().await;
    assert_eq!(tokens(setup.dir.path()).len(), 1);

    fs::remove_file(setup.home().pairing_file(&setup.paired.id)).unwrap();
    assert_eq!(conn.closed().await, Some(1000));
    until("the token to be deleted", || {
        tokens(setup.dir.path()).is_empty()
    })
    .await;
    snapshot_where(&setup.agent, "the service to be gone", |s| {
        s.services.is_empty()
    })
    .await;
    // The folder stays, with its approvals, and the service isn't told:
    // it lists the machine until the owner removes it there.
    let status = setup.status().await;
    let [service] = &status.services[..] else {
        panic!("{status:?}");
    };
    assert!(!service.paired && service.connection.is_none());
    assert_eq!(service.approvals, setup.paired.approvals);
    assert_eq!(setup.service.forgotten(), [] as [String; 0]);
    assert!(!setup.service.has_pending_connection());
}

#[tokio::test]
async fn a_service_folder_deleted_on_disk_is_disconnected_and_forgotten() {
    let setup = Setup::new().await;
    let mut conn = setup.connect().await;

    // As `git rm` leaves it: what git doesn't track stays.
    let folder = setup.home().service_dir(&setup.paired.id);
    fs::remove_file(folder.join("service.json")).unwrap();
    fs::remove_dir_all(folder.join("bundles")).unwrap();
    assert_eq!(conn.closed().await, Some(1000));
    until("the pairing to be deleted", || {
        tokens(setup.dir.path()).is_empty() && !folder.join("pairing.json").exists()
    })
    .await;
    let status = setup.status().await;
    assert_eq!(status.services, []);
}

#[tokio::test]
async fn pausing_in_settings_json_takes_effect() {
    let setup = Setup::new().await;
    let mut conn = setup.connect().await;
    let settings = setup.home().settings_file();

    // Written as the owner likes it, which moil leaves alone.
    let paused = "{\"paused\":true}";
    fs::write(&settings, paused).unwrap();
    assert_eq!(conn.recv().await, state(MachineState::Paused));
    assert!(setup.agent.snapshot().paused);
    conn.send(offer("j1", setup.hash()));
    assert_eq!(
        conn.recv().await,
        MachineMessage::Decline {
            job_id: "j1".parse().unwrap(),
            reason: DeclineReason::Paused
        }
    );
    assert_eq!(fs::read_to_string(&settings).unwrap(), paused);

    // Pausing through the agent changes only what it changes.
    edit_json(&settings, |file| {
        file["machine_name"] = "Ada's test box".into();
    });
    until("the name to be read", || {
        setup.agent.machine().settings().machine_name.is_some()
    })
    .await;
    setup.agent.set_paused(false);
    assert_eq!(conn.recv().await, state(MachineState::Idle));
    let saved =
        || -> Value { serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap() };
    until("the resumption to be saved", || saved()["paused"] == false).await;
    assert_eq!(saved()["machine_name"], "Ada's test box");

    // The agent's own write isn't applied again, once read back.
    edit_json(&settings, |file| file["machine_name"] = "build box".into());
    until("the next change to be read", || {
        setup.agent.machine().settings().machine_name.as_deref() == Some("build box")
    })
    .await;
    assert!(!setup.agent.snapshot().paused);
    assert_eq!(saved()["paused"], false);
}

#[tokio::test]
async fn a_file_that_turns_invalid_keeps_what_was_read_from_it_and_is_a_problem() {
    let setup = Setup::new().await;
    let mut conn = setup.connect().await;
    let file = setup.home().service_file(&setup.paired.id);
    let text = fs::read_to_string(&file).unwrap();

    fs::write(&file, text.replace("\"approvals\"", "\"approvls\"")).unwrap();
    fs::write(setup.home().settings_file(), "{\"paused\": maybe}").unwrap();
    let problem_with = |status: &Status, path: &str| -> Option<Problem> {
        status
            .problems
            .iter()
            .find(|p| p.path == Path::new(path))
            .cloned()
    };
    let service_json = format!("services/{}/service.json", setup.paired.id);
    until_status(&setup, "the problems to show", |status| {
        problem_with(status, &service_json).is_some()
            && problem_with(status, "settings.json").is_some()
    })
    .await;
    let status = setup.status().await;
    let problem = problem_with(&status, &service_json).unwrap();
    assert!(
        problem.message.contains("approvls")
            && problem
                .message
                .contains("goes on with what it read from it before"),
        "{problem:?}"
    );
    let problem = problem_with(&status, "settings.json").unwrap();
    assert!(
        problem
            .message
            .contains("goes on with the settings it read before"),
        "{problem:?}"
    );
    // The service is as it was: connected, with its approval.
    assert_eq!(status.services[0].approvals, setup.paired.approvals);
    conn.send(offer("j1", setup.hash()));
    assert_eq!(
        conn.recv().await,
        MachineMessage::Bid {
            job_id: "j1".parse().unwrap()
        }
    );

    // Fixed, the file is read again.
    let mut fixed: Value = serde_json::from_str(&text).unwrap();
    fixed["approvals"] = json!([]);
    fs::write(&file, fixed.to_string()).unwrap();
    assert_eq!(conn.recv().await, approved(&[]));
    let status = setup.status().await;
    assert!(problem_with(&status, &service_json).is_none(), "{status:?}");
}

#[tokio::test]
async fn status_shows_the_hooks_and_what_is_wrong_with_the_home() {
    let setup = Setup::new().await;
    setup.connect().await;
    let runnable = setup.hook("#!/bin/sh\n");
    let not_executable = setup.home().hook(&setup.paired.id, "summarize");
    write(&not_executable, "#!/bin/sh\n");
    write(&setup.home().services_dir().join("notes.txt"), "");
    let id = &setup.paired.id;
    let hook = |bundle: &str, path: &Path, runnable| HookReport {
        bundle: bundle.into(),
        path: path.to_owned(),
        log: setup.home().hook_log(id, bundle),
        runnable,
    };
    let hooks = [
        hook("summarize", &not_executable, false),
        hook("transcribe", &runnable, true),
    ];
    let problems = [
        Problem {
            path: "services/notes.txt".into(),
            message: "moil keeps only service folders in services/, so it ignores this file. \
                      Move it elsewhere."
                .into(),
        },
        Problem {
            path: format!("services/{id}/hooks/summarize").into(),
            message: "moil can't run this hook: it isn't executable. Run chmod +x on it.".into(),
        },
    ];

    let status = until_status(&setup, "the hooks to show", |status| {
        status.services[0].hooks.len() == 2
    })
    .await;
    assert_eq!(status.services[0].hooks, hooks);
    assert_eq!(status.problems, problems);

    // The same, whether or not an agent runs.
    let reader = Machine::open_read_only(config(setup.dir.path())).unwrap();
    let from_home = status::from_home(&reader).unwrap();
    assert_eq!(from_home.services[0].hooks, hooks);
    assert_eq!(from_home.problems, problems);
}

#[tokio::test]
async fn a_damaged_settings_json_is_a_problem_in_status_not_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    open_machine(dir.path());
    let home = Home::new(dir.path());
    fs::write(home.settings_file(), "{\"pause\": true}").unwrap();

    let reader = Machine::open_read_only(config(dir.path())).unwrap();
    let status = status::from_home(&reader).unwrap();
    assert!(!status.paused, "the defaults");
    let [problem] = &status.problems[..] else {
        panic!("{status:?}");
    };
    assert_eq!(problem.path, Path::new("settings.json"));
    assert!(
        problem.message.contains("unknown field `pause`")
            && problem.message.contains("won't start until it is"),
        "{problem:?}"
    );
}

#[tokio::test]
async fn a_service_whose_token_is_gone_isnt_paired() {
    let setup = Setup::new().await;
    let secrets = setup.home().secrets_file();
    within("the agent to shut down", setup.agent.shutdown()).await;
    drop(setup.agent);
    fs::write(&secrets, "{}").unwrap();

    let (agent, _runs) = start(open_machine(setup.dir.path()));
    let id = setup.paired.id.clone();
    snapshot_where(&agent, "the agent to find no token", |s| {
        s.service(&id)
            .is_some_and(|s| s.connection == Connection::Unpaired)
    })
    .await;
    let info = AgentInfo {
        host: Host::Agent,
        version: "test".into(),
    };
    let status = status::from_agent(&agent, &info).await.unwrap();
    assert!(!status.services[0].paired, "{status:?}");
    // Without an agent, only the files are looked at.
    within("the agent to shut down", agent.shutdown()).await;
    let reader = Machine::open_read_only(config(setup.dir.path())).unwrap();
    assert!(status::from_home(&reader).unwrap().services[0].paired);
}

/// Waits until the agent's status satisfies `condition`, and returns it.
async fn until_status(
    setup: &Setup,
    what: &str,
    mut condition: impl FnMut(&Status) -> bool,
) -> Status {
    let mut changes = setup.agent.changes();
    within(what, async {
        loop {
            let status = setup.status().await;
            if condition(&status) {
                return status;
            }
            changes.changed().await.expect("the agent is running");
        }
    })
    .await
}
