//! The agent: control channels, offers, attempts.
//!
//! One task per paired service keeps its control channel open
//! ([`connection`]); one coordinator task owns everything machine-wide:
//! whether the machine is paused or busy, the running attempt, and what
//! each attempt still has to deliver ([`coordinator`]). They talk over
//! channels, and the handle talks to the coordinator the same way.

mod connection;
mod coordinator;

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use moil_proto::control::BundleInfo;
use moil_proto::{BundleHash, ErrorCode, JobId, Outcome, Phase};
use moil_runtime::Executor;
use tokio::sync::{mpsc, oneshot, watch};
use url::Url;

use crate::{Error, Machine, PairedService, PairingSession, Review, ServiceId};
use coordinator::{Command, Coordinator, Input};

/// Starts agents.
#[derive(Debug)]
pub struct Agent;

impl Agent {
    /// Connects to every paired service and starts taking jobs. Must be
    /// called from within a Tokio runtime.
    pub fn start(machine: Machine, executor: Arc<dyn Executor>) -> AgentHandle {
        Self::start_with(machine, executor, AgentOptions::default())
    }

    /// Like [`start`](Self::start), with the agent's timings changed.
    pub fn start_with(
        machine: Machine,
        executor: Arc<dyn Executor>,
        options: AgentOptions,
    ) -> AgentHandle {
        // Detecting the hardware runs a few commands; better here than when
        // the first hello is due.
        let detect = machine.clone();
        tokio::task::spawn_blocking(move || detect.info());
        let (inputs, receiver) = mpsc::unbounded_channel();
        let (snapshot, snapshot_receiver) = watch::channel(Snapshot::default());
        let coordinator =
            Coordinator::new(machine.clone(), executor, options, inputs.clone(), snapshot);
        tokio::spawn(coordinator.run(receiver));
        AgentHandle {
            machine,
            inputs,
            snapshot: snapshot_receiver,
        }
    }
}

/// How the agent paces itself: how long it waits for things and how much
/// it keeps. The defaults follow the spec; tests shrink them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOptions {
    /// The first delay before reconnecting to a service (spec §6.1: 1 s).
    /// It doubles after each failed attempt, with jitter.
    pub reconnect_min: Duration,
    /// The longest delay before reconnecting (spec §6.1: 60 s).
    pub reconnect_max: Duration,
    /// How long a control channel may stay silent, pings included, before
    /// the agent treats it as dead (spec §6.1: 60 s). Also bounds how long
    /// opening one may take.
    pub receive_timeout: Duration,
    /// How long [`AgentHandle::shutdown`] waits for services to acknowledge
    /// the attempts it reports, once the running attempt has stopped.
    pub shutdown_grace: Duration,
    /// How many bytes of events the agent keeps per attempt for services
    /// that are out of reach. Older events are dropped first; the final
    /// `done` is always kept (spec §7.5).
    pub max_kept_bytes: usize,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            reconnect_min: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(60),
            receive_timeout: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(5),
            max_kept_bytes: 16 << 20,
        }
    }
}

/// Controls a running agent. Cheap to clone.
#[derive(Debug, Clone)]
pub struct AgentHandle {
    machine: Machine,
    inputs: mpsc::UnboundedSender<Input>,
    snapshot: watch::Receiver<Snapshot>,
}

impl AgentHandle {
    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    /// The agent's state right now.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.borrow().clone()
    }

    /// Watches the agent's state; there's a new [`Snapshot`] whenever
    /// anything the owner might see changes.
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.snapshot.clone()
    }

    /// Like [`Machine::pair`]; once the pairing completes, the agent
    /// connects to the new service (to the same one again, with the new
    /// token, if it was paired before).
    pub async fn pair(&self, base_url: &str) -> Result<PairingSession, Error> {
        let session = self.machine.pair(base_url).await?;
        let inputs = self.inputs.clone();
        Ok(session.on_paired(Box::new(move |service: &PairedService| {
            let _ = inputs.send(Input::Command(Command::Paired(Box::new(service.clone()))));
        })))
    }

    /// Disconnects from a service and forgets it, stopping its running
    /// attempt, if any.
    pub async fn unpair(&self, id: &ServiceId) -> Result<(), Error> {
        self.machine.service(id)?;
        self.call(|done| Command::Disconnect(id.clone(), done))
            .await;
        self.machine.unpair(id).await
    }

    pub async fn review(&self, id: &ServiceId, hash: BundleHash) -> Result<Review, Error> {
        self.machine.review(id, hash).await
    }

    /// Approves the reviewed bundle, tells the service, and prepares the
    /// bundle in the background.
    pub async fn approve(&self, review: &Review) -> Result<(), Error> {
        let service = Box::new(self.machine.approve(review)?);
        let bundle = Box::new(review.bundle.clone());
        self.call(|done| Command::Approvals(service, Some(bundle), done))
            .await;
        Ok(())
    }

    /// Revokes an approval and tells the service. If an attempt of that
    /// bundle for that service is running, it is stopped as
    /// `interrupted`, which the service may retry elsewhere.
    pub async fn revoke(&self, id: &ServiceId, hash: BundleHash) -> Result<(), Error> {
        let service = Box::new(self.machine.revoke(id, hash)?);
        self.call(|done| Command::Approvals(service, None, done))
            .await;
        Ok(())
    }

    /// Stops or resumes taking new jobs. A running job carries on.
    pub fn set_paused(&self, paused: bool) {
        self.send(Command::SetPaused(paused));
    }

    /// Stops the running job, if any; the snapshot shows it
    /// [`stopping`](JobStatus::stopping) until it has. The service may give
    /// it to another machine.
    pub fn stop_job(&self) {
        self.send(Command::StopJob);
    }

    /// Stops the running job, reports it, and closes every connection.
    ///
    /// Waits for the executor to return (which it must do promptly once
    /// stopped), then up to [`AgentOptions::shutdown_grace`] for services
    /// to acknowledge what they were sent. Afterwards the handle's other
    /// methods do nothing, or only what [`Machine`] does.
    pub async fn shutdown(&self) {
        self.call(Command::Shutdown).await;
    }

    fn send(&self, command: Command) {
        // Fails only once the agent has shut down, when there's nothing
        // left to tell.
        let _ = self.inputs.send(Input::Command(command));
    }

    /// Sends a command and waits until the agent has carried it out.
    async fn call(&self, command: impl FnOnce(oneshot::Sender<()>) -> Command) {
        let (done, carried_out) = oneshot::channel();
        self.send(command(done));
        let _ = carried_out.await;
    }
}

/// What the agent is doing, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub paused: bool,
    pub services: Vec<ServiceStatus>,
    /// The running attempt, if any.
    pub job: Option<JobStatus>,
    /// The attempt that ran here last, since the agent started, once it
    /// has ended. Attempts refused without running don't count.
    pub last_job: Option<JobReport>,
}

impl Snapshot {
    /// Bundles services offer that the owner hasn't approved yet.
    pub fn pending_reviews(&self) -> impl Iterator<Item = (&ServiceStatus, &BundleStatus)> {
        self.services.iter().flat_map(|s| {
            s.bundles
                .iter()
                .filter(|b| !b.approved)
                .map(move |b| (s, b))
        })
    }

    /// The status of one service.
    pub fn service(&self, id: &ServiceId) -> Option<&ServiceStatus> {
        self.services.iter().find(|s| s.id == *id)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceStatus {
    pub id: ServiceId,
    pub name: String,
    pub base_url: Url,
    pub connection: Connection,
    /// The bundles the service offers, as it describes them.
    pub bundles: Vec<BundleStatus>,
    /// Whether this service is reached over plain `http`.
    pub insecure: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Connection {
    Connecting,
    Connected,
    /// Lost or refused; the agent retries at `retry_at`.
    Disconnected {
        error: String,
        retry_at: Instant,
    },
    /// The service removed this machine (close code 4401). Pair again.
    Unpaired,
    /// Another connection with this machine's token replaced this one
    /// (close code 4409).
    Replaced,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BundleStatus {
    /// As the service describes it: never trust it for approval.
    pub info: BundleInfo,
    pub approved: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobStatus {
    pub service: ServiceId,
    pub job_id: JobId,
    pub attempt: u32,
    /// The service's label for the job, e.g. a meeting's name.
    pub title: Option<String>,
    /// From the approved bundle's manifest.
    pub bundle_name: String,
    pub phase: Phase,
    /// The script's latest `progress.fraction`.
    pub fraction: Option<f64>,
    /// The script's latest `progress.message`.
    pub message: Option<String>,
    pub started_at: Instant,
    /// Whether the attempt was asked to stop (by the owner, its service, a
    /// withdrawn approval or shutting down) and is winding down. It stays
    /// the running job until the runtime has stopped it.
    pub stopping: bool,
}

impl JobStatus {
    /// This attempt's report, as it ends now with `outcome`.
    pub fn ended(self, outcome: JobOutcome) -> JobReport {
        JobReport {
            service: self.service,
            job_id: self.job_id,
            attempt: self.attempt,
            title: self.title,
            bundle_name: self.bundle_name,
            outcome,
            ended_at: SystemTime::now(),
        }
    }
}

/// An attempt that ran on this machine and has ended.
#[derive(Debug, Clone, PartialEq)]
pub struct JobReport {
    pub service: ServiceId,
    pub job_id: JobId,
    pub attempt: u32,
    /// The service's label for the job, e.g. a meeting's name.
    pub title: Option<String>,
    /// From the approved bundle's manifest.
    pub bundle_name: String,
    pub outcome: JobOutcome,
    pub ended_at: SystemTime,
}

/// How an attempt ended, as the machine's owner sees it: its [`Outcome`],
/// with the attempts stopped on this machine told apart from failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    Succeeded,
    /// `message` says why, for people.
    Failed {
        code: ErrorCode,
        message: String,
    },
    /// The service cancelled it, or stopped expecting it.
    Cancelled,
    /// Stopped here: by the owner, by revoking its bundle or unpairing its
    /// service, or by quitting. The service may retry it elsewhere.
    Interrupted,
}

impl From<&Outcome> for JobOutcome {
    fn from(outcome: &Outcome) -> Self {
        match outcome {
            Outcome::Succeeded { .. } => Self::Succeeded,
            Outcome::Cancelled => Self::Cancelled,
            Outcome::Failed { error } if error.code == ErrorCode::Interrupted => Self::Interrupted,
            Outcome::Failed { error } => Self::Failed {
                code: error.code,
                message: error.message.clone(),
            },
        }
    }
}
