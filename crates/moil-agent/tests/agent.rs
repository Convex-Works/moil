//! The agent against fake services over real WebSockets: the handshake,
//! offers, attempts, leases, reconnecting and resuming, close codes, and
//! what the owner sees and does through the handle.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use moil_agent::{
    Agent, AgentHandle, AgentOptions, Connection, Home, JobOutcome, JobReport, Machine,
    MachineConfig, PairedService, Settings,
};
use moil_bundle::Bundle;
use moil_proto::control::{EventMessage, Hello, Welcome};
use moil_proto::{
    DeclineReason, Done, ErrorCode, JobEvent, JobResult, LogLevel, MachineMessage, MachineState,
    Outcome, Phase, ServiceInfo, ServiceMessage,
};
use moil_runtime::StopReason;
use support::*;
use tempfile::TempDir;

/// A machine with `transcribe` approved for each of its services, running
/// an agent with a fake executor.
struct Setup {
    dir: TempDir,
    services: Vec<(FakeService, PairedService)>,
    bundle: Bundle,
    agent: AgentHandle,
    runs: FakeRuns,
}

impl Setup {
    async fn new() -> Self {
        Self::build(&["klisi"], fast(), |_, _| {}).await
    }

    async fn build(
        names: &[&str],
        options: AgentOptions,
        before_start: impl FnOnce(&Machine, &[(FakeService, PairedService)]),
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let machine = open_machine(dir.path());
        let bundle = bundle("transcribe", "1.0.0", "print('hi')");
        let mut services = Vec::new();
        for name in names {
            let service = FakeService::start(name).await;
            let paired = pair(&machine, &service).await;
            service.host(&bundle);
            let review = machine.review(&paired.id, bundle.hash()).await.unwrap();
            let paired = machine.approve(&review).unwrap();
            services.push((service, paired));
        }
        before_start(&machine, &services);
        let (executor, runs) = fake_executor();
        let agent = Agent::start_with(machine, executor, options);
        Self {
            dir,
            services,
            bundle,
            agent,
            runs,
        }
    }

    fn service(&self) -> &FakeService {
        &self.services[0].0
    }

    fn paired(&self) -> &PairedService {
        &self.services[0].1
    }

    fn hash(&self) -> moil_proto::BundleHash {
        self.bundle.hash()
    }

    /// Accepts the machine's next channel to the first service and
    /// welcomes it.
    async fn connect(&self) -> Conn {
        connect(self.service(), &self.bundle).await
    }

    async fn wait_until_connected(&self) {
        let id = self.paired().id.clone();
        snapshot_where(&self.agent, "the service to be connected", |s| {
            s.service(&id)
                .is_some_and(|s| s.connection == Connection::Connected)
        })
        .await;
    }

    async fn wait_until_disconnected(&self) {
        let id = self.paired().id.clone();
        snapshot_where(&self.agent, "the service to be disconnected", |s| {
            s.service(&id)
                .is_some_and(|s| matches!(s.connection, Connection::Disconnected { .. }))
        })
        .await;
    }
}

async fn connect(service: &FakeService, bundle: &Bundle) -> Conn {
    let mut conn = service.accept().await;
    conn.handshake(vec![info(bundle)], vec![]).await;
    conn
}

fn welcome(resume: Vec<moil_proto::Resume>) -> ServiceMessage {
    ServiceMessage::Welcome(Welcome {
        protocol: 1,
        machine_id: "m_1".into(),
        service: ServiceInfo {
            name: "klisi".into(),
        },
        bundles: vec![],
        resume,
    })
}

fn state(state: MachineState) -> MachineMessage {
    MachineMessage::State { state }
}

fn bid(job: &str) -> MachineMessage {
    MachineMessage::Bid {
        job_id: job.parse().unwrap(),
    }
}

fn decline(job: &str, reason: DeclineReason) -> MachineMessage {
    MachineMessage::Decline {
        job_id: job.parse().unwrap(),
        reason,
    }
}

fn event(job: &str, attempt: u32, seq: u64, event: JobEvent) -> MachineMessage {
    MachineMessage::Event(EventMessage {
        job_id: job.parse().unwrap(),
        attempt,
        seq,
        event,
    })
}

fn done(job: &str, attempt: u32, seq: u64, outcome: Outcome) -> MachineMessage {
    MachineMessage::Done(Done {
        job_id: job.parse().unwrap(),
        attempt,
        seq,
        outcome,
    })
}

fn log(message: &str) -> JobEvent {
    JobEvent::log(LogLevel::Info, message)
}

/// How the job that just ran ended, once it has.
async fn last_outcome(agent: &AgentHandle) -> JobOutcome {
    let snapshot = snapshot_where(agent, "the job to end", |s| s.job.is_none()).await;
    snapshot.last_job.expect("a job ran").outcome
}

/// How an attempt that was stopped by the owner or a shutdown ends.
fn interrupted() -> Outcome {
    Outcome::failed(moil_proto::JobError::new(
        ErrorCode::Interrupted,
        "stopped by the owner",
    ))
}

/// The error of a failed `done`.
fn failure(message: MachineMessage) -> (u64, moil_proto::JobError) {
    match message {
        MachineMessage::Done(Done {
            seq,
            outcome: Outcome::Failed { error },
            ..
        }) => (seq, error),
        other => panic!("expected a failed done, got {other:?}"),
    }
}

#[tokio::test]
async fn hello_describes_the_machine_and_what_it_approved_for_that_service() {
    let setup = Setup::build(&["klisi", "notes"], fast(), |machine, services| {
        machine
            .update_settings(|s| {
                s.machine_name = Some("Ada's test box".into());
                s.paused = true;
            })
            .unwrap();
        let hash = machine.services().unwrap()[0].approvals[0].hash;
        machine.revoke(&services[1].1.id, hash).unwrap();
    })
    .await;
    let notes = &setup.services[1].0;

    let hello = setup.service().accept().await.hello().await;
    assert_eq!(
        hello,
        Hello {
            protocol: 1,
            app_version: "0.1.0-test".into(),
            machine: setup.agent.machine().info(),
            state: MachineState::Paused,
            approved: vec![setup.hash()],
            attempts: vec![],
        }
    );
    assert_eq!(hello.machine.name, "Ada's test box");

    let hello = notes.accept().await.hello().await;
    assert_eq!(hello.approved, [], "approvals are per service");
    assert_eq!(hello.state, MachineState::Paused);
}

#[tokio::test]
async fn offers_get_a_bid_only_when_the_machine_can_take_the_job() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    let hash = setup.hash();

    conn.send(offer("j1", hash));
    assert_eq!(conn.recv().await, bid("j1"));

    let unapproved = bundle("summarize", "1.0.0", "print('?')");
    conn.send(offer("j2", unapproved.hash()));
    assert_eq!(conn.recv().await, decline("j2", DeclineReason::NotApproved));

    setup.agent.set_paused(true);
    assert_eq!(conn.recv().await, state(MachineState::Paused));
    conn.send(offer("j5", hash));
    assert_eq!(conn.recv().await, decline("j5", DeclineReason::Paused));
    setup.agent.set_paused(false);
    assert_eq!(conn.recv().await, state(MachineState::Idle));

    conn.send(assign("j6", 1, hash, 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let _run = setup.runs.next().await;
    conn.send(offer("j7", hash));
    assert_eq!(conn.recv().await, decline("j7", DeclineReason::Busy));
}

#[tokio::test]
async fn jobs_run_the_copy_verified_at_approval_whatever_happens_to_the_store() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    let hash = setup.hash();

    // Offers are answered from memory: the store isn't read, let alone
    // hashed, for each one...
    let store = setup.agent.machine().home().bundles();
    std::fs::write(store.dir(&hash).join("job.py"), "import os\n").unwrap();
    conn.send(offer("j1", hash));
    assert_eq!(conn.recv().await, bid("j1"));

    // ...and the job gets the approved bundle, which the runtime checks
    // the stored copy against before it runs.
    conn.send(assign("j1", 1, hash, 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    assert_eq!(setup.runs.next().await.job.bundle, setup.bundle);
}

#[tokio::test]
async fn a_stored_copy_that_fails_verification_takes_no_jobs_until_approved_again() {
    let mut setup = Setup::build(&["klisi"], fast(), |machine, services| {
        let hash = services[0].1.approvals[0].hash;
        let dir = machine.home().bundles().dir(&hash);
        std::fs::write(dir.join("job.py"), "import os\n").unwrap();
    })
    .await;
    let mut conn = setup.connect().await;
    let hash = setup.hash();
    conn.send(offer("j1", hash));
    assert_eq!(conn.recv().await, decline("j1", DeclineReason::NotApproved));
    conn.send(assign("j1", 1, hash, 30_000));
    let (_, error) = failure(conn.recv().await);
    assert_eq!(error.code, ErrorCode::NotApproved);

    let review = setup.agent.review(&setup.paired().id, hash).await.unwrap();
    setup.agent.approve(&review).await.unwrap();
    conn.send(offer("j2", hash));
    assert_eq!(conn.recv().await, bid("j2"));
    conn.send(assign("j2", 1, hash, 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    assert_eq!(setup.runs.next().await.job.bundle, setup.bundle);
}

#[tokio::test]
async fn a_keychain_that_keeps_moil_waiting_doesnt_hold_up_the_agent() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let keychain = Arc::new(SlowSecrets::new(dir.path().join("secrets.json")));
    let machine = Machine::open(MachineConfig {
        home: Home::new(dir.path()),
        secrets: keychain.clone(),
        app_version: "0.1.0-test".into(),
    })
    .unwrap();
    pair(&machine, &service).await;
    let (executor, _runs) = fake_executor();

    // Like a keychain asking the owner for permission, every read takes
    // its time: connecting, then unpairing, read the token.
    keychain.slow_down(Duration::from_secs(1));
    let agent = Agent::start_with(machine, executor, fast());
    let started = Instant::now();
    agent.set_paused(true);
    snapshot_where(&agent, "the pause to show", |s| s.paused).await;
    let id = agent.snapshot().services[0].id.clone();
    let unpairing = tokio::spawn({
        let agent = agent.clone();
        async move { agent.unpair(&id).await }
    });
    agent.set_paused(false);
    snapshot_where(&agent, "the resumption to show", |s| !s.paused).await;
    let took = started.elapsed();
    assert!(
        took < Duration::from_millis(500),
        "the agent was held up for {took:?}"
    );
    within("unpairing", unpairing).await.unwrap().unwrap();
}

#[tokio::test]
async fn an_assignment_the_machine_cant_take_fails_at_once() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;

    let unapproved = bundle("summarize", "1.0.0", "print('?')");
    conn.send(assign("j1", 1, unapproved.hash(), 30_000));
    let (seq, error) = failure(conn.recv().await);
    assert_eq!(
        (seq, error.code, error.retryable),
        (1, ErrorCode::NotApproved, true)
    );
    assert_eq!(setup.agent.snapshot().last_job, None, "it never ran");

    conn.send(assign("j2", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let _run = setup.runs.next().await;
    conn.send(assign("j3", 1, setup.hash(), 30_000));
    let (seq, error) = failure(conn.recv().await);
    assert_eq!(
        (seq, error.code, error.retryable),
        (1, ErrorCode::Busy, true)
    );
    assert!(!setup.runs.has_pending_run(), "only one attempt ever ran");
}

#[tokio::test]
async fn a_refused_assignment_is_answered_once_and_not_kept() {
    let setup = Setup::new().await;
    let mut conn = setup.connect().await;
    let unapproved = bundle("summarize", "1.0.0", "print('?')");
    conn.send(assign("j1", 1, unapproved.hash(), 30_000));
    failure(conn.recv().await);

    // Never acknowledged, it isn't offered again: the service fails the
    // attempt as lost (spec §7.5).
    conn.drop_connection();
    let hello = setup.service().accept().await.hello().await;
    assert_eq!(hello.attempts, []);
}

#[tokio::test]
async fn a_flood_of_refused_assignments_costs_the_machine_nothing() {
    let setup = Setup::new().await;
    let conn = setup.connect().await;
    let unapproved = bundle("summarize", "1.0.0", "print('?')").hash();

    // Kept until acknowledged, and all looked at again after each
    // message, 40,000 of these took seconds and memory without end.
    const ASSIGNS: u32 = 20_000;
    let started = Instant::now();
    for attempt in 1..=ASSIGNS {
        conn.send(assign("j1", attempt, unapproved, 30_000));
    }
    // The machine reads them all before it sees the connection drop.
    conn.drop_connection();
    let mut conn = setup.service().accept().await;
    assert_eq!(conn.hello().await.attempts, [], "none of them was kept");
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "{ASSIGNS} refusals took {took:?}"
    );
}

#[tokio::test]
async fn an_assignment_the_machine_cant_read_is_rejected() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    let hash = setup.hash();
    let assign = |attempt: u32, params: &str, more: &str| {
        format!(
            r#"{{"type": "assign", "job_id": "j1", "attempt": {attempt}, "bundle_hash": "{hash}",
                "params": {params}, "lease_ms": 30000{more}}}"#
        )
    };
    let nested = |levels| format!("{}{}", "[".repeat(levels), "]".repeat(levels));
    let unreadable = [
        // Deeper than any JSON parser need follow...
        assign(1, &nested(10_000), ""),
        // ...or than the spec allows (§10).
        assign(2, &nested(65), ""),
        // No double holds it.
        assign(3, r#"{"x": 1e400}"#, ""),
        // An upload method from a later protocol.
        assign(
            4,
            "null",
            r#", "outputs": {"out.json": {"url": "https://s3.example/o", "method": "PATCH"}}"#,
        ),
        // A time limit of nothing (§6.3).
        assign(5, "null", r#", "timeout_ms": 0"#),
    ];
    for (attempt, text) in (1..).zip(&unreadable) {
        conn.send_text(text);
        match conn.recv().await {
            MachineMessage::Done(Done {
                job_id,
                attempt: done_attempt,
                seq: 1,
                outcome: Outcome::Failed { error },
            }) => {
                assert_eq!((job_id.as_str(), done_attempt), ("j1", attempt));
                assert_eq!(
                    (error.code, error.retryable),
                    (ErrorCode::Rejected, false),
                    "{error}"
                );
            }
            other => panic!("expected a rejected done for attempt {attempt}, got {other:?}"),
        }
    }
    assert!(!setup.runs.has_pending_run(), "none of them ran");

    // At the spec's limits, an attempt runs...
    conn.send_text(&assign(6, &nested(64), r#", "timeout_ms": 1"#));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let run = setup.runs.next().await;
    assert_eq!(run.job.attempt, 6);
    // ...and an unreadable assign for it changes nothing.
    conn.send_text(&assign(6, "1e400", ""));
    conn.send(offer("j2", hash));
    assert_eq!(conn.recv().await, decline("j2", DeclineReason::Busy));
}

#[tokio::test]
async fn an_attempt_reports_its_events_and_outcome_in_order() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;

    conn.send(assign("j_42", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let run = setup.runs.next().await;
    assert_eq!(run.job.id, "j_42");
    assert_eq!(run.job.attempt, 1);
    assert_eq!(run.job.bundle, setup.bundle);
    assert_eq!(run.job.params, serde_json::json!({"language": "en"}));

    let events = [
        JobEvent::Phase {
            phase: Phase::Running,
        },
        progress(0.5, "halfway"),
        JobEvent::log(LogLevel::Warn, "falling back to CPU"),
        JobEvent::Data {
            payload: serde_json::json!({"segment": {"text": "hi"}}),
        },
    ];
    for e in &events {
        run.emit(e.clone());
    }
    for (seq, e) in (1..).zip(&events) {
        assert_eq!(conn.recv().await, event("j_42", 1, seq, e.clone()));
    }

    let snapshot = snapshot_where(&setup.agent, "progress to show", |s| {
        s.job.as_ref().is_some_and(|j| j.fraction == Some(0.5))
    })
    .await;
    let job = snapshot.job.unwrap();
    assert_eq!(job.service, setup.paired().id);
    assert_eq!((job.job_id.as_str(), job.attempt), ("j_42", 1));
    assert_eq!(job.title.as_deref(), Some("j_42 title"));
    assert_eq!(job.bundle_name, "transcribe");
    assert_eq!(
        (job.phase, job.message.as_deref()),
        (Phase::Running, Some("halfway"))
    );

    let outcome = Outcome::Succeeded {
        result: JobResult {
            meta: Some(serde_json::json!({"speakers": 3})),
            files: Default::default(),
        },
    };
    let before = std::time::SystemTime::now();
    run.finish(outcome.clone());
    assert_eq!(conn.recv().await, done("j_42", 1, 5, outcome));
    assert_eq!(conn.recv().await, state(MachineState::Idle));
    let snapshot = snapshot_where(&setup.agent, "the job to end", |s| s.job.is_none()).await;
    let report = snapshot.last_job.unwrap();
    assert!(report.ended_at >= before, "{report:?}");
    assert_eq!(
        report,
        JobReport {
            service: setup.paired().id.clone(),
            job_id: "j_42".parse().unwrap(),
            attempt: 1,
            title: Some("j_42 title".into()),
            bundle_name: "transcribe".into(),
            outcome: JobOutcome::Succeeded,
            ended_at: report.ended_at,
        }
    );

    // Acknowledged, the attempt is forgotten.
    conn.send(ServiceMessage::Ack(attempt("j_42", 1)));
    conn.drop_connection();
    let hello = setup.service().accept().await.hello().await;
    assert_eq!(hello.attempts, []);
    assert_eq!(hello.state, MachineState::Idle);
}

#[tokio::test]
async fn the_last_job_says_how_it_ended_until_another_ends() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let run = setup.runs.next().await;
    let error = moil_proto::JobError::new(ErrorCode::ScriptError, "CUDA out of memory");
    run.finish(Outcome::failed(error));
    failure(conn.recv().await);
    let failed = JobOutcome::Failed {
        code: ErrorCode::ScriptError,
        message: "CUDA out of memory".into(),
    };
    assert_eq!(last_outcome(&setup.agent).await, failed);

    // Still the last job while the next one runs...
    conn.send(assign("j2", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Idle));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let mut run = setup.runs.next().await;
    let snapshot = snapshot_where(&setup.agent, "the next job", |s| s.job.is_some()).await;
    let last = snapshot.last_job.unwrap();
    assert_eq!((last.job_id.as_str(), last.outcome), ("j1", failed));

    // ...until it ends in turn.
    conn.send(ServiceMessage::Cancel(attempt("j2", 1)));
    run.stopped().await;
    let snapshot = snapshot_where(&setup.agent, "the job to end", |s| s.job.is_none()).await;
    let last = snapshot.last_job.unwrap();
    assert_eq!(
        (last.job_id.as_str(), last.outcome),
        ("j2", JobOutcome::Cancelled)
    );
}

#[tokio::test]
async fn a_quiet_attempt_renews_its_lease_every_third_of_it() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    let lease = Duration::from_millis(600);
    let (third, slack) = (lease / 3, Duration::from_millis(20));
    let renew = MachineMessage::Renew(attempt("j1", 1));

    let assigned = Instant::now();
    conn.send(assign("j1", 1, setup.hash(), 600));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let run = setup.runs.next().await;
    assert_eq!(conn.recv().await, renew);
    let first = Instant::now();
    assert_eq!(conn.recv().await, renew);
    let second = Instant::now();
    for gap in [first - assigned, second - first] {
        assert!(gap + slack >= third && gap < lease, "renewed after {gap:?}");
    }

    // Any message for the attempt renews the lease, so the clock restarts.
    run.emit(log("working"));
    assert_eq!(conn.recv().await, event("j1", 1, 1, log("working")));
    let evented = Instant::now();
    assert_eq!(conn.recv().await, renew);
    let gap = evented.elapsed();
    assert!(
        gap + slack >= third && gap < lease,
        "renewed {gap:?} after an event"
    );

    // A finished attempt has no lease left to renew.
    run.finish(succeeded());
    assert_eq!(conn.recv().await, done("j1", 1, 2, succeeded()));
    assert_eq!(conn.recv().await, state(MachineState::Idle));
    tokio::time::sleep(lease).await;
    conn.send(offer("j2", setup.hash()));
    assert_eq!(conn.recv().await, bid("j2"));
}

#[tokio::test]
async fn cancelling_stops_the_attempt() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let mut run = setup.runs.next().await;

    // Cancels for other attempts change nothing.
    conn.send(ServiceMessage::Cancel(attempt("j1", 2)));
    conn.send(ServiceMessage::Cancel(attempt("j9", 1)));
    run.emit(log("still here"));
    assert_eq!(conn.recv().await, event("j1", 1, 1, log("still here")));

    conn.send(ServiceMessage::Cancel(attempt("j1", 1)));
    assert_eq!(run.stopped().await, StopReason::Cancelled);
    assert_eq!(conn.recv().await, done("j1", 1, 2, Outcome::Cancelled));
    assert_eq!(conn.recv().await, state(MachineState::Idle));
}

#[tokio::test]
async fn the_owner_stopping_a_job_makes_it_retryable_elsewhere() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let mut run = setup.runs.next().await;

    setup.agent.stop_job();
    assert_eq!(run.stopped().await, StopReason::Interrupted);
    let (seq, error) = failure(conn.recv().await);
    assert_eq!(
        (seq, error.code, error.retryable),
        (1, ErrorCode::Interrupted, true)
    );
    assert_eq!(conn.recv().await, state(MachineState::Idle));
}

#[tokio::test]
async fn a_stopping_job_shows_as_stopping_until_it_ends() {
    for cause in ["the owner", "the service", "shutting down"] {
        let mut setup = Setup::new().await;
        let mut conn = setup.connect().await;
        conn.send(assign("j1", 1, setup.hash(), 30_000));
        assert_eq!(conn.recv().await, state(MachineState::Busy));
        let mut run = setup.runs.next().await;
        run.linger();
        let job = snapshot_where(&setup.agent, "the job to show", |s| s.job.is_some()).await;
        assert!(!job.job.unwrap().stopping, "{cause}");

        let (reason, outcome) = match cause {
            "the owner" => {
                setup.agent.stop_job();
                (StopReason::Interrupted, interrupted())
            }
            "the service" => {
                conn.send(ServiceMessage::Cancel(attempt("j1", 1)));
                (StopReason::Cancelled, Outcome::Cancelled)
            }
            _ => {
                let agent = setup.agent.clone();
                tokio::spawn(async move { agent.shutdown().await });
                (StopReason::Interrupted, interrupted())
            }
        };
        assert_eq!(run.stopped().await, reason, "{cause}");
        snapshot_where(&setup.agent, "the job to show it's stopping", |s| {
            s.job.as_ref().is_some_and(|j| j.stopping)
        })
        .await;

        // The script takes its time; the job is still the running one.
        run.emit(log("cleaning up"));
        assert_eq!(conn.recv().await, event("j1", 1, 1, log("cleaning up")));
        let job = setup.agent.snapshot().job;
        assert!(job.is_some_and(|j| j.stopping), "{cause}");

        run.finish(outcome.clone());
        assert_eq!(conn.recv().await, done("j1", 1, 2, outcome), "{cause}");
        let expected = match reason {
            StopReason::Cancelled => JobOutcome::Cancelled,
            StopReason::Interrupted => JobOutcome::Interrupted,
        };
        assert_eq!(last_outcome(&setup.agent).await, expected, "{cause}");
    }
}

#[tokio::test]
async fn one_attempt_runs_at_a_time_across_services() {
    let mut setup = Setup::build(&["klisi", "notes"], fast(), |_, _| {}).await;
    let mut klisi = connect(&setup.services[0].0, &setup.bundle).await;
    let mut notes = connect(&setup.services[1].0, &setup.bundle).await;
    let hash = setup.hash();

    klisi.send(assign("k1", 1, hash, 30_000));
    assert_eq!(klisi.recv().await, state(MachineState::Busy));
    assert_eq!(
        notes.recv().await,
        state(MachineState::Busy),
        "every service hears"
    );
    let run = setup.runs.next().await;

    notes.send(offer("n1", hash));
    assert_eq!(notes.recv().await, decline("n1", DeclineReason::Busy));
    notes.send(assign("n1", 1, hash, 30_000));
    let (_, error) = failure(notes.recv().await);
    assert_eq!(error.code, ErrorCode::Busy);

    run.finish(succeeded());
    assert_eq!(klisi.recv().await, done("k1", 1, 1, succeeded()));
    assert_eq!(klisi.recv().await, state(MachineState::Idle));
    assert_eq!(notes.recv().await, state(MachineState::Idle));
    notes.send(offer("n2", hash));
    assert_eq!(notes.recv().await, bid("n2"));
}

#[tokio::test]
async fn after_reconnecting_only_unseen_messages_are_replayed() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let run = setup.runs.next().await;
    run.emit(log("one"));
    run.emit(log("two"));
    assert_eq!(conn.recv().await, event("j1", 1, 1, log("one")));
    assert_eq!(conn.recv().await, event("j1", 1, 2, log("two")));

    conn.drop_connection();
    setup.wait_until_disconnected().await;
    run.emit(log("three"));

    let mut conn = setup.service().accept().await;
    let hello = conn.hello().await;
    assert_eq!(hello.attempts, [attempt("j1", 1)]);
    assert_eq!(hello.state, MachineState::Busy);
    // The service had processed seq 1 only.
    conn.send(welcome(vec![resume("j1", 1, 1)]));
    assert_eq!(conn.recv().await, event("j1", 1, 2, log("two")));
    assert_eq!(conn.recv().await, event("j1", 1, 3, log("three")));

    run.emit(log("four"));
    assert_eq!(conn.recv().await, event("j1", 1, 4, log("four")));
    run.finish(succeeded());
    assert_eq!(conn.recv().await, done("j1", 1, 5, succeeded()));
    assert_eq!(conn.recv().await, state(MachineState::Idle));
}

#[tokio::test]
async fn attempts_the_service_no_longer_expects_are_abandoned() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let mut run = setup.runs.next().await;
    run.emit(log("one"));
    assert_eq!(conn.recv().await, event("j1", 1, 1, log("one")));

    conn.drop_connection();
    let mut conn = setup.service().accept().await;
    assert_eq!(conn.hello().await.attempts, [attempt("j1", 1)]);
    conn.send(welcome(vec![]));

    assert_eq!(run.stopped().await, StopReason::Cancelled);
    // No done for an abandoned attempt: the machine just becomes idle.
    assert_eq!(conn.recv().await, state(MachineState::Idle));
    conn.send(offer("j2", setup.hash()));
    assert_eq!(conn.recv().await, bid("j2"));
    conn.drop_connection();
    assert_eq!(setup.service().accept().await.hello().await.attempts, []);
}

#[tokio::test]
async fn a_done_is_kept_until_the_service_acknowledges_it() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    setup.runs.next().await.finish(succeeded());
    assert_eq!(conn.recv().await, done("j1", 1, 1, succeeded()));
    assert_eq!(conn.recv().await, state(MachineState::Idle));

    // No ack, then the connection drops: the done is offered again.
    conn.drop_connection();
    let mut conn = setup.service().accept().await;
    let hello = conn.hello().await;
    assert_eq!(
        (hello.attempts, hello.state),
        (vec![attempt("j1", 1)], MachineState::Idle)
    );
    conn.send(welcome(vec![resume("j1", 1, 0)]));
    assert_eq!(conn.recv().await, done("j1", 1, 1, succeeded()));

    // The service did have it: nothing is replayed, but it's still kept.
    conn.drop_connection();
    let mut conn = setup.service().accept().await;
    assert_eq!(conn.hello().await.attempts, [attempt("j1", 1)]);
    conn.send(welcome(vec![resume("j1", 1, 1)]));
    conn.send(offer("j2", setup.hash()));
    assert_eq!(conn.recv().await, bid("j2"));

    conn.send(ServiceMessage::Ack(attempt("j1", 1)));
    conn.drop_connection();
    assert_eq!(setup.service().accept().await.hello().await.attempts, []);
}

#[tokio::test]
async fn an_offline_attempt_keeps_its_newest_events_and_always_its_done() {
    let options = AgentOptions {
        max_kept_bytes: 2_000,
        ..fast()
    };
    let mut setup = Setup::build(&["klisi"], options, |_, _| {}).await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let run = setup.runs.next().await;

    conn.drop_connection();
    setup.wait_until_disconnected().await;
    for i in 1..=50 {
        run.emit(log(&format!("line {i:02} {}", "x".repeat(60))));
    }
    run.finish(succeeded());
    snapshot_where(&setup.agent, "the job to end", |s| s.job.is_none()).await;

    let mut conn = setup.service().accept().await;
    conn.hello().await;
    conn.send(welcome(vec![resume("j1", 1, 0)]));
    let mut seqs = Vec::new();
    let mut bytes = 0;
    loop {
        let message = conn.recv().await;
        match &message {
            MachineMessage::Event(e) => {
                seqs.push(e.seq);
                bytes += serde_json::to_string(&message).unwrap().len();
            }
            MachineMessage::Done(d) => {
                assert_eq!(d.seq, 51);
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(seqs[0] > 1, "the oldest events were dropped: {seqs:?}");
    assert_eq!(
        seqs,
        (seqs[0]..=50).collect::<Vec<_>>(),
        "the rest in order"
    );
    assert!(bytes <= 2_000, "{bytes} bytes of events replayed");
}

#[tokio::test]
async fn a_chatty_attempt_is_relayed_as_briskly_as_a_quiet_one() {
    // The fake service doesn't ping; a slow relay should fail the
    // assertion below, not look like a dead channel.
    let options = AgentOptions {
        channel_timeout: Duration::from_secs(60),
        ..fast()
    };
    let mut setup = Setup::build(&["klisi"], options, |_, _| {}).await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let run = setup.runs.next().await;

    // The machine keeps an attempt's events until its done is
    // acknowledged. When it looked through all of them for each one it
    // sent, relaying these took about twenty seconds.
    const EVENTS: u64 = 100_000;
    let started = Instant::now();
    for i in 1..=EVENTS {
        run.emit(log(&format!("line {i}")));
    }
    run.finish(succeeded());
    let mut seqs = 0;
    loop {
        match conn.recv().await {
            MachineMessage::Event(e) => {
                seqs += 1;
                assert_eq!(e.seq, seqs, "events arrive in order");
            }
            MachineMessage::Done(d) => {
                assert_eq!(d.seq, EVENTS + 1);
                break;
            }
            _ => {}
        }
    }
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(10),
        "relaying {EVENTS} events took {took:?}"
    );
}

#[tokio::test]
async fn a_removed_machine_stops_reconnecting() {
    let setup = Setup::new().await;
    let conn = setup.connect().await;
    setup.service().remove_machine(&conn.token);
    snapshot_where(&setup.agent, "the service to be marked unpaired", |s| {
        s.services[0].connection == Connection::Unpaired
    })
    .await;
    // Many times the longest backoff: a reconnect would have happened.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!setup.service().has_pending_connection());
}

#[tokio::test]
async fn a_replaced_connection_stops_reconnecting() {
    let setup = Setup::new().await;
    let conn = setup.connect().await;
    conn.close(4409);
    snapshot_where(&setup.agent, "the connection to be marked replaced", |s| {
        s.services[0].connection == Connection::Replaced
    })
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!setup.service().has_pending_connection());
}

#[tokio::test]
async fn a_401_when_connecting_means_the_machine_was_removed() {
    let setup = Setup::build(&["klisi"], fast(), |_, _| {}).await;
    setup.service().remove_machine("moil_token_1");
    snapshot_where(&setup.agent, "the service to be marked unpaired", |s| {
        s.services[0].connection == Connection::Unpaired
    })
    .await;
}

#[tokio::test]
async fn other_failures_are_retried() {
    let setup = Setup::new().await;
    let conn = setup.connect().await;
    let before = Instant::now();
    conn.close(4400);
    let snapshot = snapshot_where(&setup.agent, "the protocol error to show", |s| {
        matches!(&s.services[0].connection, Connection::Disconnected { error, .. } if error.contains("4400"))
    })
    .await;
    let Connection::Disconnected { retry_at, .. } = snapshot.services[0].connection else {
        unreachable!()
    };
    assert!(retry_at > before && retry_at < before + Duration::from_secs(1));
    let conn = setup.connect().await;
    setup.wait_until_connected().await;

    setup
        .service()
        .refuse_upgrades(Some(axum::http::StatusCode::SERVICE_UNAVAILABLE));
    conn.drop_connection();
    snapshot_where(&setup.agent, "the refusal to show", |s| {
        matches!(&s.services[0].connection, Connection::Disconnected { error, .. } if error.contains("503"))
    })
    .await;
    setup.service().refuse_upgrades(None);
    let _conn = setup.connect().await;
    setup.wait_until_connected().await;
}

#[tokio::test]
async fn a_silent_service_is_given_up_on_but_pings_keep_it_alive() {
    let options = AgentOptions {
        channel_timeout: Duration::from_millis(300),
        ..fast()
    };
    let setup = Setup::build(&["klisi"], options, |_, _| {}).await;
    let conn = setup.connect().await;
    setup.wait_until_connected().await;

    for _ in 0..6 {
        conn.ping();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !setup.service().has_pending_connection(),
        "pings count as traffic"
    );

    // Now silence: the machine gives up and connects again.
    let _conn = setup.connect().await;
}

/// A `data` event of `kib` KiB.
fn bulky(i: usize, kib: usize) -> JobEvent {
    JobEvent::Data {
        payload: serde_json::json!({"i": i, "text": "x".repeat(kib << 10)}),
    }
}

/// Sends `events` bulky events, then waits until the agent has handled
/// them: sent them on, or held them back for a service that isn't reading.
async fn flood(agent: &AgentHandle, run: &FakeRun, events: usize, kib: usize) {
    for i in 0..events {
        run.emit(bulky(i, kib));
    }
    run.emit(progress(0.5, "flooded"));
    snapshot_where(agent, "the agent to handle the flood", |s| {
        s.job.as_ref().is_some_and(|j| j.fraction == Some(0.5))
    })
    .await;
}

#[tokio::test]
async fn a_service_that_stops_reading_is_given_up_on_even_while_it_pings() {
    let options = AgentOptions {
        channel_timeout: Duration::from_millis(500),
        ..fast()
    };
    let mut setup = Setup::build(&["klisi"], options, |_, _| {}).await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let run = setup.runs.next().await;

    // The service stops reading, but it keeps pinging: the machine hears
    // from it all along, and only its writes stall.
    conn.stop_reading();
    flood(&setup.agent, &run, 100, 200).await;
    let pinging = async {
        loop {
            conn.ping();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let mut conn = tokio::select! {
        conn = setup.service().accept() => conn,
        () = pinging => unreachable!(),
    };
    assert_eq!(conn.hello().await.attempts, [attempt("j1", 1)]);
}

#[tokio::test]
async fn unpairing_doesnt_wait_on_a_service_that_stopped_reading() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let mut run = setup.runs.next().await;
    conn.stop_reading();
    flood(&setup.agent, &run, 100, 200).await;

    within("unpairing", setup.agent.unpair(&setup.paired().id))
        .await
        .unwrap();
    assert_eq!(run.stopped().await, StopReason::Interrupted);
    assert_eq!(setup.agent.snapshot().services, []);
}

#[tokio::test]
async fn a_slow_service_gets_every_message_in_order_once_it_reads_again() {
    // Neither silence nor stalled writes run out while the service rests.
    let options = AgentOptions {
        channel_timeout: Duration::from_secs(60),
        max_kept_bytes: 64 << 20,
        ..fast()
    };
    let mut setup = Setup::build(&["klisi"], options, |_, _| {}).await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let run = setup.runs.next().await;

    // More than the outbox and the network hold together: the machine
    // holds the rest back until there's room.
    conn.stop_reading();
    const EVENTS: u64 = 800;
    flood(&setup.agent, &run, EVENTS as usize, 32).await;
    run.finish(succeeded());
    snapshot_where(&setup.agent, "the job to end", |s| s.job.is_none()).await;

    conn.resume_reading();
    for (i, seq) in (1..=EVENTS).enumerate() {
        assert_eq!(conn.recv().await, event("j1", 1, seq, bulky(i, 32)));
    }
    let marker = progress(0.5, "flooded");
    assert_eq!(conn.recv().await, event("j1", 1, EVENTS + 1, marker));
    assert_eq!(conn.recv().await, done("j1", 1, EVENTS + 2, succeeded()));
    assert_eq!(conn.recv().await, state(MachineState::Idle));
}

#[tokio::test]
async fn pausing_is_told_to_every_service_and_remembered() {
    let setup = Setup::build(&["klisi", "notes"], fast(), |_, _| {}).await;
    let mut klisi = connect(&setup.services[0].0, &setup.bundle).await;
    let mut notes = connect(&setup.services[1].0, &setup.bundle).await;

    setup.agent.set_paused(true);
    assert_eq!(klisi.recv().await, state(MachineState::Paused));
    assert_eq!(notes.recv().await, state(MachineState::Paused));
    assert!(setup.agent.snapshot().paused);
    let saved = || -> Option<Settings> {
        let text = std::fs::read_to_string(setup.dir.path().join("settings.json")).ok()?;
        serde_json::from_str(&text).ok()
    };
    until("the pause to be saved", || {
        saved().is_some_and(|s| s.paused)
    })
    .await;
    klisi.send(offer("j1", setup.hash()));
    assert_eq!(klisi.recv().await, decline("j1", DeclineReason::Paused));

    setup.agent.set_paused(false);
    assert_eq!(klisi.recv().await, state(MachineState::Idle));
    assert_eq!(notes.recv().await, state(MachineState::Idle));
    until("the resumption to be saved", || {
        saved().is_some_and(|s| !s.paused)
    })
    .await;
    assert!(!setup.agent.machine().settings().paused);
}

#[tokio::test]
async fn a_running_job_carries_on_when_the_machine_is_paused() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let run = setup.runs.next().await;

    setup.agent.set_paused(true);
    run.emit(log("working"));
    assert_eq!(conn.recv().await, event("j1", 1, 1, log("working")));
    run.finish(succeeded());
    assert_eq!(conn.recv().await, done("j1", 1, 2, succeeded()));
    assert_eq!(conn.recv().await, state(MachineState::Paused));
}

#[tokio::test]
async fn approvals_through_the_agent_reach_the_service() {
    let mut setup = Setup::new().await;
    let v2 = bundle("transcribe", "2.0.0", "print('two')");
    setup.service().host(&v2);
    let mut conn = setup.service().accept().await;
    conn.handshake(vec![info(&setup.bundle), info(&v2)], vec![])
        .await;
    let snapshot = snapshot_where(&setup.agent, "the bundles to show", |s| {
        !s.services[0].bundles.is_empty()
    })
    .await;
    let pending: Vec<_> = snapshot
        .pending_reviews()
        .map(|(_, b)| b.info.hash)
        .collect();
    assert_eq!(pending, [v2.hash()]);

    let id = setup.paired().id.clone();
    let review = setup.agent.review(&id, v2.hash()).await.unwrap();
    assert_eq!(review.previous.as_ref(), Some(&setup.bundle));
    setup.agent.approve(&review).await.unwrap();
    assert_eq!(
        conn.recv().await,
        MachineMessage::Approved {
            hashes: vec![setup.hash(), v2.hash()]
        }
    );
    assert_eq!(
        setup.runs.prepared().await,
        v2.hash(),
        "prepared ahead of any job"
    );
    assert_eq!(setup.agent.snapshot().pending_reviews().count(), 0);

    setup.agent.revoke(&id, setup.hash()).await.unwrap();
    assert_eq!(
        conn.recv().await,
        MachineMessage::Approved {
            hashes: vec![v2.hash()]
        }
    );
    let pending: Vec<_> = setup
        .agent
        .snapshot()
        .pending_reviews()
        .map(|(_, b)| b.info.hash)
        .collect();
    assert_eq!(pending, [setup.hash()]);
}

#[tokio::test]
async fn revoking_a_bundle_stops_its_running_attempt() {
    let mut setup = Setup::build(&["klisi", "notes"], fast(), |_, _| {}).await;
    let mut klisi = connect(&setup.services[0].0, &setup.bundle).await;
    let mut notes = connect(&setup.services[1].0, &setup.bundle).await;
    let (klisi_id, notes_id) = (
        setup.services[0].1.id.clone(),
        setup.services[1].1.id.clone(),
    );
    let hash = setup.hash();
    klisi.send(assign("j1", 1, hash, 30_000));
    assert_eq!(klisi.recv().await, state(MachineState::Busy));
    assert_eq!(notes.recv().await, state(MachineState::Busy));
    let mut run = setup.runs.next().await;

    // Approving another bundle, or revoking this one for another service,
    // leaves the attempt alone: approvals are per bundle and per service.
    let v2 = bundle("transcribe", "2.0.0", "print('two')");
    setup.services[0].0.host(&v2);
    let review = setup.agent.review(&klisi_id, v2.hash()).await.unwrap();
    setup.agent.approve(&review).await.unwrap();
    let approved = |hashes: &[moil_proto::BundleHash]| MachineMessage::Approved {
        hashes: hashes.to_vec(),
    };
    assert_eq!(klisi.recv().await, approved(&[hash, v2.hash()]));
    setup.agent.revoke(&notes_id, hash).await.unwrap();
    assert_eq!(notes.recv().await, approved(&[]));
    run.emit(log("still running"));
    assert_eq!(klisi.recv().await, event("j1", 1, 1, log("still running")));

    setup.agent.revoke(&klisi_id, hash).await.unwrap();
    assert_eq!(run.stopped().await, StopReason::Interrupted);
    assert_eq!(klisi.recv().await, approved(&[v2.hash()]));
    let (seq, error) = failure(klisi.recv().await);
    assert_eq!(
        (seq, error.code, error.retryable),
        (2, ErrorCode::Interrupted, true),
        "another machine may take the job"
    );
    assert_eq!(klisi.recv().await, state(MachineState::Idle));
    assert_eq!(last_outcome(&setup.agent).await, JobOutcome::Interrupted);
}

#[tokio::test]
async fn unpairing_stops_the_services_running_attempt() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let mut run = setup.runs.next().await;

    setup.agent.unpair(&setup.paired().id).await.unwrap();
    assert_eq!(run.stopped().await, StopReason::Interrupted);
    // Nothing more is said: the service fails the attempt as `lost` when
    // it removes the machine.
    assert_eq!(conn.closed().await, Some(1000));
    assert_eq!(last_outcome(&setup.agent).await, JobOutcome::Interrupted);
}

#[tokio::test]
async fn pairing_through_the_agent_connects_to_the_new_service() {
    let dir = tempfile::tempdir().unwrap();
    let (executor, _runs) = fake_executor();
    let agent = Agent::start_with(open_machine(dir.path()), executor, fast());
    assert_eq!(agent.snapshot().services, []);

    let service = FakeService::start("klisi").await;
    service.script_pairing([TokenReply::Grant]);
    let session = agent.pair(&service.base_url).await.unwrap();
    let paired = within("pairing", session.wait()).await.unwrap();

    let mut conn = service.accept().await;
    assert_eq!(conn.token, "moil_token_1");
    conn.handshake(vec![], vec![]).await;
    snapshot_where(&agent, "the new service to be connected", |s| {
        s.service(&paired.id)
            .is_some_and(|s| s.connection == Connection::Connected && s.insecure)
    })
    .await;
}

#[tokio::test]
async fn pairing_again_through_the_agent_reconnects_with_the_new_token() {
    let setup = Setup::new().await;
    let mut conn = setup.connect().await;
    setup.service().script_pairing([TokenReply::Grant]);
    let session = setup.agent.pair(&setup.service().base_url).await.unwrap();
    within("pairing", session.wait()).await.unwrap();

    // The service forgets the old machine, closing its channel...
    assert_eq!(conn.closed().await, Some(4401));
    // ...and the agent connects with the new token.
    let conn = setup.service().accept().await;
    assert_eq!(conn.token, "moil_token_2");
}

#[tokio::test]
async fn unpairing_through_the_agent_disconnects_and_forgets() {
    let setup = Setup::new().await;
    let mut conn = setup.connect().await;
    let id = setup.paired().id.clone();

    setup.agent.unpair(&id).await.unwrap();
    assert_eq!(conn.closed().await, Some(1000));
    assert_eq!(setup.service().forgotten(), ["moil_token_1"]);
    assert_eq!(setup.agent.snapshot().services, []);
    assert_eq!(setup.agent.machine().services().unwrap(), []);
}

#[tokio::test]
async fn shutting_down_interrupts_the_job_and_delivers_its_done() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let mut run = setup.runs.next().await;

    let agent = setup.agent.clone();
    let shutdown = tokio::spawn(async move { agent.shutdown().await });
    assert_eq!(run.stopped().await, StopReason::Interrupted);
    let (_, error) = failure(conn.recv().await);
    assert_eq!(
        (error.code, error.retryable),
        (ErrorCode::Interrupted, true)
    );
    assert_eq!(conn.recv().await, state(MachineState::Paused));
    assert!(
        !shutdown.is_finished(),
        "waiting for the service to acknowledge"
    );

    conn.send(ServiceMessage::Ack(attempt("j1", 1)));
    within("shutdown", shutdown).await.unwrap();
    assert_eq!(conn.closed().await, Some(1000));
}

#[tokio::test]
async fn shutting_down_gives_a_silent_service_only_so_long() {
    let options = AgentOptions {
        shutdown_grace: Duration::from_millis(200),
        ..fast()
    };
    let mut setup = Setup::build(&["klisi"], options, |_, _| {}).await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let _run = setup.runs.next().await;

    let started = Instant::now();
    within("shutdown", setup.agent.shutdown()).await;
    assert!(started.elapsed() >= Duration::from_millis(200));
    failure(conn.recv().await);
    assert_eq!(conn.recv().await, state(MachineState::Paused));
    assert_eq!(conn.closed().await, Some(1000));
}

#[tokio::test]
async fn the_snapshot_follows_the_connection() {
    let setup = Setup::new().await;
    let status = &setup.agent.snapshot().services[0];
    assert_eq!(status.connection, Connection::Connecting);
    assert_eq!((status.name.as_str(), status.insecure), ("klisi", true));

    let mut conn = setup.service().accept().await;
    conn.hello().await;
    assert_eq!(
        setup.agent.snapshot().services[0].connection,
        Connection::Connecting
    );
    conn.send(ServiceMessage::Welcome(Welcome {
        protocol: 1,
        machine_id: "m_1".into(),
        service: ServiceInfo {
            name: "klisi".into(),
        },
        bundles: vec![info(&setup.bundle)],
        resume: vec![],
    }));
    let snapshot = snapshot_where(&setup.agent, "connected", |s| {
        s.services[0].connection == Connection::Connected
    })
    .await;
    let bundles = &snapshot.services[0].bundles;
    assert_eq!(bundles.len(), 1);
    assert!(bundles[0].approved && bundles[0].info == info(&setup.bundle));

    let v2 = bundle("transcribe", "2.0.0", "print('two')");
    conn.send(ServiceMessage::Bundles {
        bundles: vec![info(&v2)],
    });
    snapshot_where(&setup.agent, "the new bundle list", |s| {
        s.pending_reviews()
            .map(|(_, b)| b.info.hash)
            .eq([v2.hash()])
    })
    .await;

    conn.drop_connection();
    setup.wait_until_disconnected().await;
    let _conn = setup.connect().await;
    setup.wait_until_connected().await;
}

#[tokio::test]
async fn messages_the_machine_cant_use_are_ignored() {
    let setup = Setup::new().await;
    let mut conn = setup.connect().await;
    conn.send_text(r#"{"type": "poll", "question": "lunch?"}"#);
    conn.send_text("not json");
    conn.send_text(r#"{"type": "offer", "job_id": "../../etc", "bundle_hash": "x"}"#);
    // An assign whose attempt can't be named can't be answered either.
    conn.send_text(r#"{"type": "assign", "job_id": "../../etc", "attempt": 1}"#);
    conn.send_text(r#"{"type": "assign", "job_id": "j1", "attempt": -1}"#);
    conn.send(offer("j1", setup.hash()));
    assert_eq!(conn.recv().await, bid("j1"));
}

#[tokio::test]
async fn an_absurd_lease_is_survived() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect().await;
    conn.send(assign("j1", 1, setup.hash(), u64::MAX));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let run = setup.runs.next().await;
    run.emit(log("fine"));
    assert_eq!(conn.recv().await, event("j1", 1, 1, log("fine")));
    run.finish(succeeded());
    assert_eq!(conn.recv().await, done("j1", 1, 2, succeeded()));
}

/// An executor with a bug.
#[derive(Debug)]
struct Panicking;

impl moil_runtime::Executor for Panicking {
    fn prepare<'a>(
        &'a self,
        _: &'a Bundle,
    ) -> futures::future::BoxFuture<'a, Result<(), moil_proto::JobError>> {
        Box::pin(async { Ok(()) })
    }

    fn run(
        &self,
        _: moil_runtime::Job,
        _: moil_runtime::EventSink,
        _: moil_runtime::StopSignal,
    ) -> futures::future::BoxFuture<'_, Outcome> {
        Box::pin(async { panic!("index out of bounds") })
    }
}

#[tokio::test]
async fn a_crashing_runtime_fails_the_attempt_instead_of_hanging() {
    let service = FakeService::start("klisi").await;
    let dir = tempfile::tempdir().unwrap();
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    service.host(&bundle);
    let review = machine.review(&paired.id, bundle.hash()).await.unwrap();
    machine.approve(&review).unwrap();
    let _agent = Agent::start_with(machine, std::sync::Arc::new(Panicking), fast());
    let mut conn = connect(&service, &bundle).await;

    conn.send(assign("j1", 1, bundle.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let (_, error) = failure(conn.recv().await);
    assert_eq!((error.code, error.retryable), (ErrorCode::Internal, true));
    assert!(
        error.message.contains("index out of bounds"),
        "{}",
        error.message
    );
    assert_eq!(conn.recv().await, state(MachineState::Idle));
}
