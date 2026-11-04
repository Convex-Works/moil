//! The control socket, end to end: commands reach a real agent over a real
//! Unix socket, as `moil` reaches the app, while the agent works with a
//! fake service and a fake executor.

mod support;

use std::io::{BufRead, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use futures::FutureExt;
use futures::future::BoxFuture;
use moil_agent::control::{self, Client, ClientError, HostControls, ServerHandle};
use moil_agent::report::{
    AgentInfo, ConnectionReport, EndedJob, Host, ListedBundle, OfferedBundle, Outcome, Status,
};
use moil_agent::{Agent, AgentHandle, AgentOptions, Connection, Home, Machine, Service, status};
use moil_bundle::Bundle;
use moil_proto::{ErrorCode, MachineMessage, MachineState, ServiceMessage};
use moil_runtime::StopReason;
use support::*;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// The app, as far as commands can tell: it quits when asked, and opens at
/// login if the owner says so.
struct AppHost {
    quits: mpsc::UnboundedSender<()>,
    login_item: Mutex<bool>,
}

impl HostControls for AppHost {
    fn quit(&self) {
        let _ = self.quits.send(());
    }

    fn login_item(&self, set: Option<bool>) -> BoxFuture<'_, Result<bool, String>> {
        let mut login_item = self.login_item.lock().unwrap();
        if let Some(set) = set {
            *login_item = set;
        }
        let enabled = *login_item;
        async move { Ok(enabled) }.boxed()
    }
}

/// `moil agent`, which only quits.
struct AgentCommand {
    quits: mpsc::UnboundedSender<()>,
}

impl HostControls for AgentCommand {
    fn quit(&self) {
        let _ = self.quits.send(());
    }
}

/// An agent serving its control socket, on a machine paired with `acme`
/// with `transcribe` approved, unless it was set up unpaired.
struct Setup {
    _dir: TempDir,
    home: Home,
    service: FakeService,
    paired: Option<Service>,
    bundle: Bundle,
    agent: AgentHandle,
    runs: FakeRuns,
    server: Option<ServerHandle>,
    quits: mpsc::UnboundedReceiver<()>,
}

impl Setup {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_owned();
        Self::build(dir, &root, true, Host::App).await
    }

    async fn unpaired() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_owned();
        Self::build(dir, &root, false, Host::App).await
    }

    /// Paired, with an agent that looks at the home only once an hour, so
    /// that it sees a change there only when asked to read it.
    async fn slow_to_notice() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_owned();
        let options = AgentOptions {
            home_poll: Duration::from_secs(3600),
            ..fast()
        };
        Self::build_with(dir, &root, true, Host::App, options).await
    }

    async fn build(dir: TempDir, root: &Path, paired: bool, host: Host) -> Self {
        Self::build_with(dir, root, paired, host, fast()).await
    }

    async fn build_with(
        dir: TempDir,
        root: &Path,
        paired: bool,
        host: Host,
        options: AgentOptions,
    ) -> Self {
        let machine = open_machine(root);
        let service = FakeService::start("acme").await;
        let bundle = bundle("transcribe", "1.0.0", "print('hi')");
        let paired = if paired {
            let paired = pair(&machine, &service).await;
            service.host(&bundle);
            let review = machine.review(&paired.id, bundle.hash()).await.unwrap();
            Some(machine.approve(&review).unwrap())
        } else {
            None
        };
        let (executor, runs) = fake_executor();
        // No channel goes quiet for long enough to time out during a test,
        // so what a watcher sees is what the test did.
        let options = AgentOptions {
            channel_timeout: PATIENCE * 2,
            ..options
        };
        let agent = Agent::start_with(machine, executor, options);
        let (sender, quits) = mpsc::unbounded_channel();
        let controls: Arc<dyn HostControls> = match host {
            Host::App => Arc::new(AppHost {
                quits: sender,
                login_item: Mutex::new(false),
            }),
            Host::Agent => Arc::new(AgentCommand { quits: sender }),
        };
        let server = control::serve(agent.clone(), agent_info(host), controls).unwrap();
        Self {
            _dir: dir,
            home: Home::new(root),
            service,
            paired,
            bundle,
            agent,
            runs,
            server: Some(server),
            quits,
        }
    }

    fn paired(&self) -> &Service {
        self.paired.as_ref().expect("set up paired")
    }

    async fn client(&self) -> Client {
        within("the client to connect", Client::connect(&self.home))
            .await
            .unwrap()
    }

    /// Accepts the machine's channel to the service, which offers
    /// `bundles`, and waits until the agent shows it connected.
    async fn connect(&self, bundles: &[&Bundle]) -> Conn {
        let mut conn = self.service.accept().await;
        conn.handshake(bundles.iter().map(|b| info(b)).collect(), vec![])
            .await;
        let id = self.paired().id.clone();
        snapshot_where(&self.agent, "the service to be connected", |s| {
            s.service(&id)
                .is_some_and(|s| s.connection == Connection::Connected)
        })
        .await;
        conn
    }

    /// Quits as a host does: the agent, then the server.
    async fn shut_down(&mut self) {
        within("the agent to shut down", self.agent.shutdown()).await;
        let server = self.server.take().expect("serving");
        within("the server to shut down", server.shutdown()).await;
    }
}

fn agent_info(host: Host) -> AgentInfo {
    AgentInfo {
        host,
        version: control::VERSION.to_owned(),
    }
}

fn state(state: MachineState) -> MachineMessage {
    MachineMessage::State { state }
}

fn approved(hashes: &[moil_proto::BundleHash]) -> MachineMessage {
    MachineMessage::Approved {
        hashes: hashes.to_vec(),
    }
}

/// Waits for the next status of `watch` that satisfies `condition`.
async fn next_where(
    watch: &mut control::Watch,
    what: &str,
    mut condition: impl FnMut(&Status) -> bool,
) -> Status {
    within(what, async {
        loop {
            let status = watch.next().await.unwrap().expect("the agent is running");
            if condition(&status) {
                return status;
            }
        }
    })
    .await
}

async fn connect_to(home: &Home) -> Result<Client, ClientError> {
    within("the client to connect", Client::connect(home)).await
}

#[tokio::test]
async fn the_status_over_the_socket_is_the_agents() {
    let mut setup = Setup::new().await;
    let v2 = bundle("transcribe", "2.0.0", "print('two')");
    let mut conn = setup.connect(&[&setup.bundle, &v2]).await;
    let mut client = setup.client().await;
    assert_eq!(client.agent(), &agent_info(Host::App));

    let status = client.status().await.unwrap();
    assert_eq!(status.home, setup.home.root());
    assert_eq!(status.agent, Some(agent_info(Host::App)));
    assert!(!status.paused && status.job.is_none() && status.last_job.is_none());
    let [service] = &status.services[..] else {
        panic!("{:?}", status.services);
    };
    assert_eq!(
        (&service.id, service.name.as_str(), service.paired),
        (&setup.paired().id, "acme", true)
    );
    assert_eq!(service.connection, Some(ConnectionReport::Connected));
    assert_eq!(service.approvals, setup.paired().approvals);
    let offered = |b: &Bundle, approved| OfferedBundle {
        hash: b.hash(),
        name: b.manifest().name.clone(),
        version: b.manifest().version.clone(),
        description: b.manifest().description.clone(),
        approved,
    };
    assert_eq!(
        service.offered,
        Some(vec![offered(&setup.bundle, true), offered(&v2, false)])
    );

    let assigned_at = SystemTime::now();
    conn.send(assign("j1", 1, setup.bundle.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let run = setup.runs.next().await;
    run.emit(progress(0.25, "diarizing"));
    snapshot_where(&setup.agent, "progress to show", |s| {
        s.job.as_ref().is_some_and(|j| j.fraction == Some(0.25))
    })
    .await;
    let job = client.status().await.unwrap().job.expect("a job runs");
    assert_eq!(
        (
            &job.service,
            job.job_id.as_str(),
            job.attempt,
            job.bundle.as_str()
        ),
        (&setup.paired().id, "j1", 1, "transcribe")
    );
    assert_eq!(job.title.as_deref(), Some("j1 title"));
    assert_eq!(
        (job.fraction, job.message.as_deref(), job.stopping),
        (Some(0.25), Some("diarizing"), false)
    );
    let since_assigned = job
        .started_at
        .duration_since(assigned_at - Duration::from_secs(1))
        .expect("started after it was assigned, to the second");
    assert!(since_assigned < Duration::from_secs(10), "{job:?}");

    let error = moil_proto::JobError::new(ErrorCode::ScriptError, "CUDA out of memory");
    run.finish(moil_proto::Outcome::failed(error));
    snapshot_where(&setup.agent, "the job to end", |s| s.job.is_none()).await;
    let status = client.status().await.unwrap();
    let EndedJob {
        job_id,
        outcome,
        error,
        ..
    } = status.last_job.expect("a job ran");
    assert_eq!((job_id.as_str(), outcome), ("j1", Outcome::Failed));
    let error = error.expect("a failure says why");
    assert_eq!(
        (error.code, error.message.as_str()),
        (ErrorCode::ScriptError, "CUDA out of memory")
    );
}

#[tokio::test]
async fn the_status_over_the_socket_shows_the_home_as_it_is_now() {
    let setup = Setup::slow_to_notice().await;
    let mut client = setup.client().await;
    let id = &setup.paired().id;
    assert!(client.status().await.unwrap().services[0].hooks.is_empty());

    // What an owner, or their agent, writes and then asks moil about.
    let hook = setup.home.hook(id, "transcribe");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let status = client.status().await.unwrap();
    let hooks = &status.services[0].hooks;
    assert_eq!(hooks.len(), 1, "{status:?}");
    assert!(hooks[0].runnable && hooks[0].bundle == "transcribe");

    // The same for the first status of a watch.
    std::fs::remove_file(&hook).unwrap();
    let mut watch = setup.client().await.watch().await.unwrap();
    let first = watch.next().await.unwrap().unwrap();
    assert_eq!(first.services[0].hooks, [], "{first:?}");
}

/// A status asked for while a checkout is under way, here with the
/// service's service.json gone for the moment, isn't taken from the home
/// half done: the service stays connected, and its job running.
#[tokio::test]
async fn a_status_asked_for_during_a_checkout_waits_for_it_to_end() {
    let mut setup = Setup::slow_to_notice().await;
    let mut conn = setup.connect(&[&setup.bundle]).await;
    let mut client = setup.client().await;
    conn.send(assign("j1", 1, setup.bundle.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let run = setup.runs.next().await;

    let id = &setup.paired().id;
    let service_file = setup.home.service_file(id);
    let kept = std::fs::read(&service_file).unwrap();
    std::fs::remove_file(&service_file).unwrap();
    // The rest of the checkout, which takes its time.
    let checking_out = tokio_util::sync::CancellationToken::new();
    let checkout = tokio::spawn({
        let notes = setup.home.service_dir(id).join("notes.md");
        let checking_out = checking_out.clone();
        async move {
            for line in 0.. {
                std::fs::write(&notes, format!("line {line}\n")).unwrap();
                tokio::select! {
                    () = checking_out.cancelled() => return,
                    () = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
        }
    });
    let during = client.status().await.unwrap();
    checking_out.cancel();
    checkout.await.unwrap();
    std::fs::write(&service_file, kept).unwrap();
    let after = client.status().await.unwrap();

    for status in [&during, &after] {
        let [service] = &status.services[..] else {
            panic!("{status:?}");
        };
        assert_eq!(service.connection, Some(ConnectionReport::Connected));
        let job = status.job.as_ref().expect("the job runs");
        assert!(!job.stopping, "{status:?}");
    }
    run.finish(succeeded());
    loop {
        if let MachineMessage::Done(done) = conn.recv().await {
            assert!(
                matches!(done.outcome, moil_proto::Outcome::Succeeded { .. }),
                "{done:?}"
            );
            break;
        }
    }
}

#[tokio::test]
async fn the_status_is_the_same_whether_or_not_an_agent_runs() {
    let dir = tempfile::tempdir().unwrap();
    let service = FakeService::start("acme").await;
    let machine = open_machine(dir.path());
    let paired = pair(&machine, &service).await;
    let bundle = bundle("transcribe", "1.0.0", "print('hi')");
    service.host(&bundle);
    let review = machine.review(&paired.id, bundle.hash()).await.unwrap();
    let approved = machine.approve(&review).unwrap();
    machine.update_settings(|s| s.paused = true).unwrap();
    drop(machine);

    let config = moil_agent::MachineConfig {
        home: Home::new(dir.path()),
        secrets: Arc::new(moil_agent::FileSecrets::new(
            dir.path().join("secrets.json"),
        )),
        app_version: "0.1.0-test".into(),
    };
    let from_home = status::from_home(&Machine::open_read_only(config).unwrap());
    assert_eq!((from_home.agent.as_ref(), from_home.paused), (None, true));
    assert_eq!((&from_home.job, &from_home.last_job), (&None, &None));
    let [service] = &from_home.services[..] else {
        panic!("{:?}", from_home.services);
    };
    assert_eq!((&service.id, service.paired), (&paired.id, true));
    assert_eq!(service.approvals, approved.approvals);
    assert_eq!((&service.connection, &service.offered), (&None, &None));

    let (executor, _runs) = fake_executor();
    let agent = Agent::start_with(open_machine(dir.path()), executor, fast());
    let info = agent_info(Host::Agent);
    let from_agent = status::from_agent(&agent, &info);
    assert_eq!(from_agent.agent, Some(info));
    let mut only_the_homes = from_agent.clone();
    only_the_homes.agent = None;
    for service in &mut only_the_homes.services {
        assert!(service.connection.is_some() && service.offered.is_some());
        service.connection = None;
        service.offered = None;
    }
    assert_eq!(only_the_homes, from_home);
}

#[tokio::test]
async fn pausing_resuming_and_stopping_over_the_socket() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect(&[&setup.bundle]).await;
    let mut client = setup.client().await;

    let saved = || std::fs::read_to_string(setup.home.settings_file()).unwrap();
    assert!(client.set_paused(true).await.unwrap());
    assert!(saved().contains("\"paused\": true"), "{}", saved());
    assert_eq!(conn.recv().await, state(MachineState::Paused));
    assert!(client.status().await.unwrap().paused, "at once");
    assert!(!client.set_paused(false).await.unwrap());
    assert!(saved().contains("\"paused\": false"), "{}", saved());
    assert_eq!(conn.recv().await, state(MachineState::Idle));
    assert!(!client.status().await.unwrap().paused);

    assert_eq!(client.stop().await.unwrap(), None, "no job runs");
    conn.send(assign("j1", 1, setup.bundle.hash(), 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let mut run = setup.runs.next().await;
    run.linger();
    let stopped = client.stop().await.unwrap().expect("a job ran");
    assert_eq!((stopped.job_id.as_str(), stopped.attempt), ("j1", 1));
    assert!(stopped.stopping);
    assert_eq!(run.stopped().await, StopReason::Interrupted);
    let job = client
        .status()
        .await
        .unwrap()
        .job
        .expect("still winding down");
    assert!(job.stopping);
}

#[tokio::test]
async fn a_pause_moil_cant_keep_fails_and_changes_nothing() {
    let setup = Setup::new().await;
    let mut conn = setup.connect(&[&setup.bundle]).await;
    let mut client = setup.client().await;
    let settings = setup.home.settings_file();
    std::fs::write(&settings, "{\"paused\": maybe}").unwrap();

    // As without an agent, which won't open the home.
    let error = client.set_paused(true).await.unwrap_err().to_string();
    let invalid = format!("{} isn't valid (", settings.display());
    assert!(
        error.starts_with(&invalid) && error.ends_with("; fix it or move it out of the way"),
        "{error}"
    );
    assert!(!client.status().await.unwrap().paused);
    conn.send(offer("j1", setup.bundle.hash()));
    let bid = MachineMessage::Bid {
        job_id: "j1".parse().unwrap(),
    };
    assert_eq!(conn.recv().await, bid, "still taking jobs");

    // Once the file is fixed, pausing works, and lasts.
    std::fs::write(&settings, "{}").unwrap();
    assert!(client.set_paused(true).await.unwrap());
    assert_eq!(conn.recv().await, state(MachineState::Paused));
    let saved = std::fs::read_to_string(&settings).unwrap();
    assert!(saved.contains("\"paused\": true"), "{saved}");
}

/// `moil pause` says what `settings.json` says once it returns, even
/// over an edit the agent hasn't noticed yet, and the agent goes by it.
#[tokio::test]
async fn a_pause_is_saved_over_an_edit_moil_hasnt_seen_yet() {
    let setup = Setup::slow_to_notice().await;
    let mut client = setup.client().await;
    let settings = setup.home.settings_file();
    assert!(client.set_paused(true).await.unwrap());

    // Someone resumes in the file, and pauses again before the agent looks.
    std::fs::write(&settings, "{\"paused\": false}\n").unwrap();
    assert!(client.set_paused(true).await.unwrap());
    let saved = std::fs::read_to_string(&settings).unwrap();
    assert!(saved.contains("\"paused\": true"), "{saved}");
    // Reading the home now finds nothing to undo.
    assert!(client.status().await.unwrap().paused);
    assert!(setup.agent.snapshot().paused);
}

#[tokio::test]
async fn approving_over_the_socket_reaches_the_service_and_the_agent() {
    let mut setup = Setup::new().await;
    let v1 = setup.bundle.clone();
    let v2 = bundle("transcribe", "2.0.0", "print('two')");
    setup.service.host(&v2);
    let mut conn = setup.connect(&[&v1, &v2]).await;
    let mut client = setup.client().await;

    let listing = client.bundles("acme").await.unwrap();
    assert_eq!(listing.service.id, setup.paired().id);
    let listed = |b: &Bundle, approved| ListedBundle {
        hash: b.hash(),
        name: b.manifest().name.clone(),
        version: b.manifest().version.clone(),
        description: b.manifest().description.clone(),
        approved,
        offered: true,
    };
    assert_eq!(listing.bundles, [listed(&v1, true), listed(&v2, false)]);

    let review = client.review("acme", v2.hash()).await.unwrap();
    assert_eq!(review.bundle, v2);
    assert_eq!(review.previous.as_ref(), Some(&v1));
    assert_eq!(review.service.id, setup.paired().id);
    assert!(!review.approved);

    client.approve("acme", v2.hash()).await.unwrap();
    assert_eq!(conn.recv().await, approved(&[v1.hash(), v2.hash()]));
    assert_eq!(setup.runs.prepared().await, v2.hash());
    assert_eq!(setup.agent.snapshot().pending_reviews().count(), 0);
    assert!(client.review("acme", v2.hash()).await.unwrap().approved);

    // The agent approves only what hashes to the hash it was given,
    // whatever the service sends for it.
    let v3 = bundle("transcribe", "3.0.0", "print('three')");
    let claimed = bundle("transcribe", "3.0.0", "print('something else')").hash();
    setup.service.serve_bundle(claimed, v3.to_response());
    let error = client.approve("acme", claimed).await.unwrap_err();
    assert!(
        matches!(&error, ClientError::Agent(e) if e.contains(&claimed.to_string())),
        "{error}"
    );
    let error = client.review("acme", claimed).await.unwrap_err();
    assert!(matches!(error, ClientError::Agent(_)), "{error}");
    let status = client.status().await.unwrap();
    assert_eq!(status.services[0].approvals.len(), 2);

    let error = client.approve("nowhere", v3.hash()).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "no service matches \"nowhere\"",
        "the agent's own sentence"
    );
}

#[tokio::test]
async fn revoking_over_the_socket_stops_a_running_attempt_of_that_bundle() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect(&[&setup.bundle]).await;
    let mut client = setup.client().await;
    let hash = setup.bundle.hash();
    conn.send(assign("j1", 1, hash, 30_000));
    assert_eq!(conn.recv().await, state(MachineState::Busy));
    let mut run = setup.runs.next().await;

    let id = setup.paired().id.to_string();
    client.revoke(&id, hash).await.unwrap();
    assert_eq!(run.stopped().await, StopReason::Interrupted);
    assert_eq!(conn.recv().await, approved(&[]));
    match conn.recv().await {
        MachineMessage::Done(done) => assert!(
            matches!(&done.outcome, moil_proto::Outcome::Failed { error } if error.code == ErrorCode::Interrupted),
            "{done:?}"
        ),
        other => panic!("expected the attempt's done, got {other:?}"),
    }
    assert!(
        client.status().await.unwrap().services[0]
            .approvals
            .is_empty()
    );

    let error = client.revoke(&id, hash).await.unwrap_err();
    assert_eq!(error.to_string(), format!("{hash} isn't approved for {id}"));
}

#[tokio::test]
async fn unpairing_over_the_socket_disconnects_and_forgets() {
    let setup = Setup::new().await;
    let mut conn = setup.connect(&[&setup.bundle]).await;
    let mut client = setup.client().await;

    let unpaired = client.unpair(&setup.service.base_url).await.unwrap();
    assert_eq!(unpaired.id, setup.paired().id);
    assert_eq!(conn.closed().await, Some(1000));
    assert_eq!(setup.service.forgotten(), [conn.token.clone()]);
    // The folder stays, with its approvals, for pairing again
    // (spec/machine.md §3.2).
    let services = client.status().await.unwrap().services;
    assert_eq!(services.len(), 1);
    assert!(!services[0].paired && services[0].connection.is_none());
    assert_eq!(services[0].approvals.len(), 1);
}

#[tokio::test]
async fn pairing_over_the_socket_sends_the_code_then_the_service() {
    let setup = Setup::unpaired().await;
    let service = &setup.service;
    service.script_pairing([TokenReply::Pending, TokenReply::Grant]);

    let client = setup.client().await;
    let pairing = client
        .pair(&service.base_url, Some("Ada's test box"))
        .await
        .unwrap();
    let code = &pairing.code;
    assert_eq!(code.user_code, "WDJB-MJHT");
    assert_eq!(
        code.verification_uri_complete,
        format!("{}/pair?code=WDJB-MJHT", service.base_url)
    );
    assert_eq!(code.base_url.as_str(), service.base_url);
    assert_eq!(
        (code.service_name.as_str(), code.machine_name.as_str()),
        ("acme", "Ada's test box")
    );
    assert!(code.expires_at > SystemTime::now());

    let paired = within("the pairing", pairing.confirmed()).await.unwrap();
    assert_eq!(paired.name, "acme");
    assert_eq!(service.pair_requests()[0].name, "Ada's test box");
    let conn = service.accept().await;
    assert_eq!(conn.token, "moil_token_1");
    let status = setup.client().await.status().await.unwrap();
    assert_eq!(status.services[0].id, paired.id);
    assert!(status.services[0].connection.is_some(), "{status:?}");

    // A rejected pairing says so.
    service.script_pairing([TokenReply::Denied]);
    let pairing = setup.client().await.pair(&service.base_url, None).await;
    let error = pairing.unwrap().confirmed().await.unwrap_err();
    assert_eq!(error.to_string(), "the pairing was rejected on the service");
    // And so does one that can't start.
    let error = setup
        .client()
        .await
        .pair("ftp://acme.example.com", None)
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Agent(_)), "{error}");
}

#[tokio::test]
async fn a_client_that_leaves_abandons_the_pairing() {
    let setup = Setup::unpaired().await;
    let service = &setup.service;
    let pairing = setup
        .client()
        .await
        .pair(&service.base_url, None)
        .await
        .unwrap();
    until("the agent to poll", || !service.polls().is_empty()).await;

    drop(pairing);
    // The service answers at once, so an agent still pairing would poll
    // many times in the meantime.
    within("the agent to stop polling", async {
        loop {
            let polls = service.polls().len();
            tokio::time::sleep(Duration::from_millis(50)).await;
            if service.polls().len() == polls {
                return;
            }
        }
    })
    .await;
    service.script_pairing([TokenReply::Grant]);
    assert!(setup.agent.machine().services().unwrap().is_empty());
    assert!(service.tokens().is_empty());
}

#[tokio::test]
async fn watching_sends_each_change_until_the_agent_stops() {
    let mut setup = Setup::new().await;
    let mut conn = setup.connect(&[&setup.bundle]).await;
    let mut watch = setup.client().await.watch().await.unwrap();
    let first = watch.next().await.unwrap().unwrap();
    assert!(!first.paused && first.job.is_none());

    // Changes made in the app...
    setup.agent.set_paused(true).await.unwrap();
    next_where(&mut watch, "the pause", |s| s.paused).await;
    setup.agent.set_paused(false).await.unwrap();
    next_where(&mut watch, "the resumption", |s| !s.paused).await;

    // ...by services...
    conn.send(assign("j1", 1, setup.bundle.hash(), 30_000));
    let run = setup.runs.next().await;
    next_where(&mut watch, "the job", |s| s.job.is_some()).await;
    run.emit(progress(0.5, "halfway"));
    next_where(&mut watch, "the progress", |s| {
        s.job.as_ref().is_some_and(|j| j.fraction == Some(0.5))
    })
    .await;
    run.finish(succeeded());
    let ended = next_where(&mut watch, "the job to end", |s| s.job.is_none()).await;
    assert_eq!(ended.last_job.unwrap().outcome, Outcome::Succeeded);
    while !matches!(conn.recv().await, MachineMessage::Done(_)) {}
    conn.send(ServiceMessage::Ack(attempt("j1", 1)));

    // ...by other commands, such as approving a bundle the service doesn't
    // offer...
    let unlisted = bundle("transcribe", "2.0.0", "print('two')");
    setup
        .service
        .serve_bundle(unlisted.hash(), unlisted.to_response());
    let id = setup.paired().id.to_string();
    setup
        .client()
        .await
        .approve(&id, unlisted.hash())
        .await
        .unwrap();
    next_where(&mut watch, "the approval", |s| {
        s.services[0]
            .approvals
            .iter()
            .any(|a| a.hash == unlisted.hash())
    })
    .await;

    // ...through the agent's handle, as the app does...
    let v3 = bundle("transcribe", "3.0.0", "print('three')");
    setup.service.serve_bundle(v3.hash(), v3.to_response());
    let review = setup.agent.review(&setup.paired().id, v3.hash());
    setup.agent.approve(&review.await.unwrap()).await.unwrap();
    next_where(&mut watch, "the app's approval", |s| {
        s.services[0].approvals.iter().any(|a| a.hash == v3.hash())
    })
    .await;

    // ...and by other programs, in the home's files.
    let hook = setup.home.hook(&setup.paired().id, "transcribe");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    next_where(&mut watch, "the hook", |s| {
        s.services[0].hooks.iter().any(|h| h.runnable)
    })
    .await;
    std::fs::write(setup.home.settings_file(), "{\"paused\": true}\n").unwrap();
    next_where(&mut watch, "the pause", |s| s.paused).await;
    std::fs::write(setup.home.services_dir().join("notes.txt"), "").unwrap();
    next_where(&mut watch, "the problem", |s| !s.problems.is_empty()).await;

    setup.shut_down().await;
    let end = within("the watch to end", async {
        loop {
            if watch.next().await.unwrap().is_none() {
                return;
            }
        }
    });
    end.await;
}

#[tokio::test]
async fn quitting_asks_the_host_and_returns_once_the_agent_has_stopped() {
    let mut setup = Setup::new().await;
    let quit = tokio::spawn(setup.client().await.quit());
    within("the host to be asked to quit", setup.quits.recv())
        .await
        .unwrap();
    assert!(!quit.is_finished(), "the agent hasn't stopped yet");

    setup.shut_down().await;
    within("quit to return", quit).await.unwrap().unwrap();
    let error = connect_to(&setup.home).await.unwrap_err();
    assert!(matches!(error, ClientError::NoAgent(_)), "{error}");
}

#[tokio::test]
async fn the_login_item_is_the_apps_alone() {
    let setup = Setup::new().await;
    let mut client = setup.client().await;
    assert!(!client.login_item(None).await.unwrap());
    assert!(client.login_item(Some(true)).await.unwrap());
    assert!(client.login_item(None).await.unwrap());

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    let setup = Setup::build(dir, &root, false, Host::Agent).await;
    let mut client = setup.client().await;
    assert_eq!(client.agent().host, Host::Agent);
    let error = client.login_item(Some(true)).await.unwrap_err();
    assert!(
        matches!(&error, ClientError::Agent(e) if e.contains("only the moil app")),
        "{error}"
    );
}

#[tokio::test]
async fn an_agent_of_another_version_is_refused_both_ways() {
    // An older command reaching this agent learns its version, and nothing
    // more.
    let setup = Setup::new().await;
    let mut stream = tokio::net::UnixStream::connect(setup.home.socket())
        .await
        .unwrap();
    stream
        .write_all(b"{\"version\":\"0.0.1\"}\n")
        .await
        .unwrap();
    let mut reader = tokio::io::BufReader::new(stream);
    let mut line = String::new();
    within("the agent's hello", reader.read_line(&mut line))
        .await
        .unwrap();
    let hello: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(hello["version"], control::VERSION);
    line.clear();
    let read = within("the agent to hang up", reader.read_line(&mut line)).await;
    assert_eq!(read.unwrap(), 0, "and says nothing more");

    // A command reaching an agent of another version is told to use the
    // agent's own moil.
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path());
    fake_agent(
        &home,
        "{\"version\":\"9.9.9\",\"agent\":{\"host\":\"app\",\"version\":\"9.9.9\"},\"new\":1}\n",
    );
    let error = connect_to(&home).await.unwrap_err();
    assert!(
        matches!(&error, ClientError::OtherVersion { agent, .. } if agent == "9.9.9"),
        "{error}"
    );
    let bin = dir.path().join(".machine/bin/moil");
    assert!(
        error.to_string().contains(&bin.display().to_string()),
        "{error}"
    );
}

/// Listens on `home`'s socket as an agent would, answering the first
/// client's hello with `hello` and nothing more.
fn fake_agent(home: &Home, hello: &'static str) {
    std::fs::create_dir_all(home.machine_dir()).unwrap();
    let listener = UnixListener::bind(home.socket()).unwrap();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut line = String::new();
        let _ = std::io::BufReader::new(&stream).read_line(&mut line);
        let _ = stream.write_all(hello.as_bytes());
    });
}

#[tokio::test]
async fn no_agent_is_told_apart_from_failures() {
    let dir = tempfile::tempdir().unwrap();
    let nowhere = Home::new(dir.path().join("nowhere"));
    let error = connect_to(&nowhere).await.unwrap_err();
    assert!(matches!(error, ClientError::NoAgent(_)), "{error}");
    assert_eq!(
        error.to_string(),
        format!(
            "moil isn't running on {}; open the moil app or run `moil agent`",
            nowhere.root().display()
        )
    );
    let unused = Home::new(dir.path());
    open_machine(dir.path());
    let error = connect_to(&unused).await.unwrap_err();
    assert!(matches!(error, ClientError::NoAgent(_)), "{error}");

    // Something that listens but isn't an agent is a failure to report.
    let other = tempfile::tempdir().unwrap();
    let home = Home::new(other.path());
    fake_agent(&home, "hello\n");
    let error = connect_to(&home).await.unwrap_err();
    assert!(matches!(error, ClientError::Broken { .. }), "{error}");
}

#[tokio::test]
async fn a_stale_socket_is_replaced_and_the_socket_removed_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    let home = Home::new(&root);
    std::fs::create_dir_all(home.machine_dir()).unwrap();
    // Left by an agent that crashed: nothing listens.
    drop(UnixListener::bind(home.socket()).unwrap());
    let error = connect_to(&home).await.unwrap_err();
    assert!(matches!(error, ClientError::NoAgent(_)), "{error}");

    let mut setup = Setup::build(dir, &root, false, Host::App).await;
    setup.client().await.status().await.unwrap();
    // Only the owner can reach it.
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&home.socket()), 0o600);
    assert_eq!(mode(&home.machine_dir()), 0o700);

    setup.shut_down().await;
    assert!(!home.socket().exists());
    let error = connect_to(&home).await.unwrap_err();
    assert!(matches!(error, ClientError::NoAgent(_)), "{error}");
}

#[tokio::test]
async fn a_home_too_deep_for_a_socket_address_works() {
    let dir = tempfile::tempdir().unwrap();
    let root: PathBuf = dir
        .path()
        .join("a directory with a rather long name".repeat(2))
        .join("and another one inside it, to make sure");
    let home = Home::new(&root);
    assert!(home.socket().as_os_str().len() > 108, "{:?}", home.socket());

    let mut setup = Setup::build(dir, &root, false, Host::App).await;
    let mut client = setup.client().await;
    assert_eq!(client.status().await.unwrap().home, root);
    let socket = std::fs::symlink_metadata(home.socket()).unwrap();
    assert!(socket.file_type().is_socket(), "where the home keeps it");

    drop(client);
    setup.shut_down().await;
    let error = connect_to(&home).await.unwrap_err();
    assert!(matches!(error, ClientError::NoAgent(_)), "{error}");
}
