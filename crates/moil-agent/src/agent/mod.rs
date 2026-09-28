//! The agent: control channels, offers, attempts, hooks, and the home.
//!
//! One task per paired service keeps its control channel open
//! ([`connection`]); one coordinator task owns everything machine-wide:
//! whether the machine is paused or busy, the services it goes by, the
//! running attempt, and what each attempt still has to deliver
//! ([`coordinator`]); one task reads the home again whenever it changes
//! ([`watcher`]). They talk over channels, and the handle talks to the
//! coordinator the same way. The owner's hooks run in a task of their own
//! ([`Hooks`]).

mod connection;
mod coordinator;
mod watcher;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use moil_proto::control::BundleInfo;
use moil_proto::{BundleHash, ErrorCode, JobId, Outcome, Phase};
use moil_runtime::Executor;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::hooks::{HookOptions, Hooks};
use crate::report::Problem;
use crate::{Error, Machine, PairingSession, Review, Service, ServiceId};
use coordinator::{Command, Coordinator, Input, Outputs};
pub(crate) use watcher::HomeState;
use watcher::{Reading, Refresh, Watcher};

/// Starts agents.
#[derive(Debug)]
pub struct Agent;

impl Agent {
    /// Starts the owner's hooks, connects to every paired service, starts
    /// taking jobs, and follows the home as it changes. Must be called from
    /// within a Tokio runtime, and before anything runs attempts on the
    /// machine.
    pub fn start(machine: Machine, executor: Arc<dyn Executor>) -> AgentHandle {
        Self::start_with(machine, executor, AgentOptions::default())
    }

    /// Like [`start`](Self::start), with the agent's timings changed.
    pub fn start_with(
        machine: Machine,
        executor: Arc<dyn Executor>,
        options: AgentOptions,
    ) -> AgentHandle {
        let (inputs, receiver) = mpsc::unbounded_channel();
        let home = machine.home();
        let hooks = Hooks::start(home, options.hooks.clone()).map_err(|e| {
            tracing::error!("hooks won't run: {e}");
            let queue = home.hooks_queue_dir();
            Problem {
                path: queue.strip_prefix(home.root()).unwrap_or(&queue).to_owned(),
                message: format!(
                    "moil can't run hooks ({e}). Check that moil can write to this folder, then \
                     restart moil."
                ),
            }
        });
        // Taken first, so that a change made while reading is noticed.
        let fingerprint = watcher::fingerprint(home);
        let reading = Reading::take(&machine);
        let (refreshes, refresh_receiver) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let watcher = tokio::spawn(
            Watcher {
                machine: machine.clone(),
                interval: options.home_poll,
                inputs: inputs.clone(),
                refreshes: refresh_receiver,
                stop: stop.clone(),
                read: fingerprint,
            }
            .run(),
        );
        let (snapshot, snapshot_receiver) = watch::channel(Snapshot::default());
        let (published, published_receiver) = watch::channel(Published::default());
        let (changes, changes_receiver) = watch::channel(());
        let outputs = Outputs {
            snapshot,
            published,
            changes,
        };
        let coordinator = Coordinator::new(
            machine.clone(),
            executor,
            options,
            inputs.clone(),
            outputs,
            hooks,
            (stop, watcher),
        )
        .starting_from(reading);
        tokio::spawn(coordinator.run(receiver));
        AgentHandle {
            machine,
            inputs,
            snapshot: snapshot_receiver,
            published: published_receiver,
            changes: changes_receiver,
            refreshes,
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
    /// How long a control channel may go without receiving anything (pings
    /// included), or without taking what the machine writes, before the
    /// agent treats it as dead (spec §6.1: 45 s). Also bounds how long
    /// opening one may take.
    pub channel_timeout: Duration,
    /// How long [`AgentHandle::shutdown`] waits for services to acknowledge
    /// the attempts it reports, once the running attempt has stopped.
    pub shutdown_grace: Duration,
    /// How many bytes of events the agent keeps per attempt for services
    /// that are out of reach. Older events are dropped first; the final
    /// `done` is always kept (spec §7.5).
    pub max_kept_bytes: usize,
    /// How often the agent looks for changes that other programs made to
    /// the home. It goes by one within three of these (spec/machine.md
    /// §1.3: five seconds), once the files have stopped changing.
    pub home_poll: Duration,
    /// How the owner's hooks run (spec/machine.md §2).
    pub hooks: HookOptions,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            reconnect_min: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(60),
            channel_timeout: Duration::from_secs(45),
            shutdown_grace: Duration::from_secs(5),
            max_kept_bytes: 16 << 20,
            home_poll: Duration::from_secs(1),
            hooks: HookOptions::default(),
        }
    }
}

/// Controls a running agent. Cheap to clone.
#[derive(Debug, Clone)]
pub struct AgentHandle {
    machine: Machine,
    inputs: mpsc::UnboundedSender<Input>,
    snapshot: watch::Receiver<Snapshot>,
    published: watch::Receiver<Published>,
    changes: watch::Receiver<()>,
    refreshes: mpsc::UnboundedSender<Refresh>,
}

/// What the agent publishes after each round, for
/// [`status`](crate::status): its snapshot, the home as it goes by it, and
/// the services it found no token for, all as of the same moment.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Published {
    pub snapshot: Snapshot,
    pub home: Arc<HomeState>,
    pub tokenless: BTreeSet<ServiceId>,
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
    /// anything it holds changes. What else `moil status` shows, such as
    /// hooks and problems, [`changes`](Self::changes) tells of.
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.snapshot.clone()
    }

    /// Tells when anything [`status`](crate::status) shows may have
    /// changed: the snapshot, or the home as the agent goes by it (its
    /// services, approvals and pairings, settings, hooks and problems),
    /// whether through this handle, a command, a service or another
    /// program. Each change is marked once the status shows it; changes
    /// made before this was called aren't. `changed()` fails once the agent
    /// has stopped.
    pub fn changes(&self) -> watch::Receiver<()> {
        let mut changes = self.changes.clone();
        changes.mark_unchanged();
        changes
    }

    /// What the agent published last.
    pub(crate) fn published(&self) -> Published {
        self.published.borrow().clone()
    }

    /// Like [`Machine::pair`]; once the pairing completes, the agent
    /// connects to the service (to the same one again, with the new token,
    /// if it was paired before), ready to take the jobs of the bundles its
    /// folder already approves.
    pub async fn pair(&self, base_url: &str) -> Result<PairingSession, Error> {
        let session = self.machine.pair(base_url).await?;
        let refreshes = self.refreshes.clone();
        Ok(session.on_paired(Box::new(move |_: &Service| {
            // Called where waiting is fine: the pairing completes once the
            // agent goes by it.
            let (done, applied) = oneshot::channel();
            if refreshes.send(Refresh { left: None, done }).is_ok() {
                let _ = applied.blocking_recv();
            }
        })))
    }

    /// Disconnects from a service and unpairs from it, stopping its running
    /// attempt, if any. Its folder stays (spec/machine.md §3.2).
    pub async fn unpair(&self, id: &ServiceId) -> Result<(), Error> {
        let known = id.clone();
        self.machine
            .blocking(move |machine| machine.service(&known))
            .await?;
        self.call(|done| Command::Disconnect(id.clone(), done))
            .await;
        let unpaired = self.machine.unpair(id).await;
        // Still paired if unpairing failed, the service is connected again.
        self.refresh(Some(id.clone())).await;
        unpaired
    }

    pub async fn review(&self, id: &ServiceId, hash: BundleHash) -> Result<Review, Error> {
        self.machine.review(id, hash).await
    }

    /// Approves the reviewed bundle, tells the service, and prepares the
    /// bundle in the background.
    pub async fn approve(&self, review: &Review) -> Result<(), Error> {
        let reviewed = review.clone();
        self.machine
            .blocking(move |machine| machine.approve(&reviewed))
            .await?;
        self.refresh(None).await;
        Ok(())
    }

    /// Revokes an approval and tells the service. If an attempt of that
    /// bundle for that service is running, it is stopped as
    /// `interrupted`, which the service may retry elsewhere.
    pub async fn revoke(&self, id: &ServiceId, hash: BundleHash) -> Result<(), Error> {
        let revoked = id.clone();
        self.machine
            .blocking(move |machine| machine.revoke(&revoked, hash))
            .await?;
        self.refresh(None).await;
        Ok(())
    }

    /// Has the home read again at once, and waits until the agent goes by
    /// what it says, as it does with any change to the home: after the
    /// owner changed it through the handle. `left` is a service the agent
    /// disconnected from to unpair it, which it may connect to again.
    async fn refresh(&self, left: Option<ServiceId>) {
        let (done, applied) = oneshot::channel();
        // Fails only once the agent has shut down.
        if self.refreshes.send(Refresh { left, done }).is_ok() {
            let _ = applied.await;
        }
    }

    /// Stops or resumes taking new jobs. A running job carries on.
    pub fn set_paused(&self, paused: bool) {
        self.send(Command::SetPaused(paused));
    }

    /// Stops the running job, if any; the snapshot shows it
    /// [`stopping`](JobStatus::stopping) until it has. The service may give
    /// it to another machine.
    pub fn stop_job(&self) {
        self.send(Command::StopJob(None));
    }

    /// Like [`stop_job`](Self::stop_job), and says which job it stopped,
    /// once the snapshot shows it stopping: `None` if no job was running.
    pub async fn stop_running_job(&self) -> Option<JobStatus> {
        let (reply, stopped) = oneshot::channel();
        self.send(Command::StopJob(Some(reply)));
        stopped.await.ok().flatten()
    }

    /// Waits until the agent has carried out everything asked of it
    /// before, such as [`set_paused`](Self::set_paused), and its snapshot
    /// shows it.
    pub async fn settled(&self) {
        let (done, settled) = oneshot::channel();
        // Fails only once the agent has shut down, when nothing is left to
        // carry out.
        let _ = self.inputs.send(Input::Reply(done));
        let _ = settled.await;
    }

    /// Stops the running job, reports it, closes every connection, and
    /// stops the running hook, which runs again when the agent next starts
    /// (spec/machine.md §2.3).
    ///
    /// Waits for the executor to return (which it must do promptly once
    /// stopped), then up to [`AgentOptions::shutdown_grace`] for services
    /// to acknowledge what they were sent, then for the hook to stop.
    /// Afterwards the handle's other methods do nothing, or only what
    /// [`Machine`] does.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_timings_are_the_specs() {
        let options = AgentOptions::default();
        // Spec §6.1.
        assert_eq!(options.channel_timeout, Duration::from_secs(45));
        assert_eq!(options.reconnect_min, Duration::from_secs(1));
        assert_eq!(options.reconnect_max, Duration::from_secs(60));
        // Spec §7.3: with the shortest lease services should use, renewed
        // every third of it, a machine whose channel dies right after a
        // renewal notices and connects again before the lease runs out.
        let lease = Duration::from_secs(90);
        assert!(lease / 3 + options.channel_timeout + options.reconnect_min < lease);
        // Spec/machine.md §1.3: changes made from outside within five
        // seconds, with a second to read them.
        assert!(options.home_poll * 3 + Duration::from_secs(1) <= Duration::from_secs(5));
        // Spec/machine.md §2.2: an hour, then stopped as a script is
        // (spec §8.6).
        assert_eq!(options.hooks.time_limit, Duration::from_secs(3600));
        assert_eq!(options.hooks.kill_grace, Duration::from_secs(10));
    }
}
