//! The one owner of machine-wide state: paused or not, the services it
//! goes by and their connections, the running attempt, and everything each
//! attempt still has to deliver.
//!
//! The coordinator handles one input at a time (a command from the handle,
//! an event from a connection or from the running attempt, or a reading of
//! the home), then reconciles: every welcomed service is told the
//! machine's current state and approvals if they changed, gets the attempt
//! messages it hasn't seen, and gets a `renew` when its running attempt has
//! been quiet for a third of its lease. Deriving what to send from state,
//! rather than sending as things happen, is what makes reconnecting and
//! resuming (spec §7.5) fall out naturally, and what lets a reading of the
//! home be applied however many times it's taken. Inputs that arrive
//! together make one round, after which the owner gets one new
//! [`Snapshot`], however many there were.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use futures::FutureExt;
use moil_bundle::Bundle;
use moil_proto::control::{BundleInfo, EventMessage, Hello, Welcome};
use moil_proto::{
    Assign, AttemptRef, BundleHash, DeclineReason, Done, ErrorCode, JobError, JobEvent, JobResult,
    MachineMessage, MachineState, Offer, Outcome, PROTOCOL_VERSION, Phase, ServiceMessage,
    UrlPolicy,
};
use moil_runtime::{Executor, Job, StopReason, StopSignal, Stopper};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::connection::{self, LinkEvent, Outbox};
use super::watcher::{HomeState, Reading};
use super::{
    AgentOptions, BundleStatus, Connection, JobReport, JobStatus, ServiceStatus, Snapshot,
};
use crate::hooks::{BundleRef, Hooks, PendingHook};
use crate::report::{Problem, ServiceRef};
use crate::{Error, Machine, ServiceId, services};

/// Everything the coordinator reacts to.
#[derive(Debug)]
pub(super) enum Input {
    Command(Command),
    Link {
        service: ServiceId,
        task: u64,
        event: LinkEvent,
    },
    /// The running attempt emitted an event.
    Job {
        key: AttemptKey,
        event: JobEvent,
    },
    /// An attempt's executor returned.
    Finished {
        key: AttemptKey,
        outcome: Outcome,
    },
    /// `settings.json` now says whether the machine is paused, as the
    /// owner asked, or couldn't be made to.
    Paused {
        saved: Result<(), Error>,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// The home was read again, because it changed or the handle asked.
    Home {
        reading: Box<Reading>,
        /// A service the handle disconnected from to unpair it, which the
        /// agent may connect to again from now on.
        left: Option<ServiceId>,
        /// Answered once the agent goes by the reading.
        done: Option<oneshot::Sender<()>>,
    },
    /// Answer a caller, after everything that came before is visible.
    Reply(oneshot::Sender<()>),
}

/// What the handle asks for. Each is answered once its effects show in the
/// snapshot.
#[derive(Debug)]
pub(super) enum Command {
    /// Close the service's channel and forget its attempts, and don't
    /// connect to it again until a reading of the home says it `left`:
    /// the owner is unpairing it.
    Disconnect(ServiceId, oneshot::Sender<()>),
    /// Pause or resume once `settings.json` says so. Answered with why it
    /// can't, if it can't: then nothing changes (spec/machine.md §3.2).
    SetPaused(bool, oneshot::Sender<Result<(), Error>>),
    /// The owner stopped the running job. The sender gets the job that was
    /// running.
    StopJob(oneshot::Sender<Option<JobStatus>>),
    Shutdown(oneshot::Sender<()>),
}

/// Names an attempt across services: job IDs are only unique per service.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct AttemptKey {
    service: ServiceId,
    attempt: AttemptRef,
}

impl fmt::Display for AttemptKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} attempt {} from {}",
            self.attempt.job_id, self.attempt.attempt, self.service
        )
    }
}

/// Renewals come at most this often, however short the lease...
const MIN_RENEW_INTERVAL: Duration = Duration::from_millis(50);
/// ...and at least this often, however long.
const MAX_RENEW_INTERVAL: Duration = Duration::from_secs(3600);
/// The most inputs handled in one round, between two snapshots.
const MAX_ROUND: usize = 256;
/// How long the coordinator waits for a connection to close. The
/// connection gives up on its own after [`connection::CLOSE_TIMEOUT`]; this
/// only guards against a bug there.
const CLOSE_WAIT: Duration = connection::CLOSE_TIMEOUT.saturating_mul(2);

pub(super) struct Coordinator {
    machine: Machine,
    executor: Arc<dyn Executor>,
    options: AgentOptions,
    /// Handed to the tasks the coordinator starts, to report back.
    inputs: mpsc::UnboundedSender<Input>,
    /// Where the owner gets a new [`Snapshot`] after each round.
    snapshot: watch::Sender<Snapshot>,
    /// Runs the owner's hooks, unless they couldn't start: then why.
    hooks: Result<Hooks, Problem>,
    /// Queues the hook of the attempt that ended last, or removes its
    /// entry, off the coordinator, once the one before has been.
    settling: Option<JoinHandle<()>>,
    /// The task that reads the home when it changes, and its stop.
    watcher: Option<(CancellationToken, JoinHandle<()>)>,
    paused: bool,
    /// The home as the agent goes by it.
    home: Arc<HomeState>,
    /// The paired services, and their channels.
    services: BTreeMap<ServiceId, Service>,
    /// Services the owner is unpairing: not connected to again meanwhile,
    /// whatever the home says.
    leaving: BTreeSet<ServiceId>,
    /// Verified copies of the bundles some service's approval names, by
    /// hash: offers and assignments are answered from these, without
    /// reading the store.
    bundles: HashMap<BundleHash, Bundle>,
    /// Attempts whose messages the machine keeps until the service
    /// acknowledges their `done`. Only attempts that started: a refused
    /// one's `done` is sent once and forgotten (spec §7.5).
    attempts: BTreeMap<AttemptKey, Attempt>,
    /// Attempts with messages their service may not have yet, for
    /// [`reconcile`](Self::reconcile) to send.
    unsent: BTreeSet<AttemptKey>,
    /// The one attempt running on this machine, if any.
    running: Option<Running>,
    /// The attempt that ran before, once it ended.
    last_job: Option<JobReport>,
    next_task: u64,
    shutdown: Option<Shutdown>,
    /// Callers to answer at the end of this round.
    replies: Vec<oneshot::Sender<()>>,
}

/// A paired service and its channel.
struct Service {
    paired: services::Service,
    connection: Connection,
    bundles: Vec<BundleInfo>,
    task: u64,
    stop: CancellationToken,
    join: JoinHandle<()>,
    link: Option<Link>,
}

/// An open channel to a service.
struct Link {
    outbox: Outbox,
    /// The outbox was full: nothing more is sent until the connection has
    /// written what it holds.
    held: bool,
    /// Whether the service answered `hello`. Until it has, the machine
    /// sends nothing else.
    welcomed: bool,
    /// The attempts `hello` listed, for `welcome.resume` to settle.
    announced: Vec<AttemptRef>,
    /// What the service was last told.
    state: MachineState,
    approved: Vec<BundleHash>,
}

impl Link {
    /// Queues `message` for the service. Returns whether it was: not while
    /// the outbox is full.
    fn send(&mut self, message: &MachineMessage) -> bool {
        self.queue(|| serde_json::to_string(message).expect("machine messages serialize"))
    }

    fn queue(&mut self, frame: impl FnOnce() -> String) -> bool {
        if !self.held {
            self.held = !self.outbox.queue(frame());
        }
        !self.held
    }

    /// Whether the service takes messages now: it answered `hello` and
    /// its outbox has room.
    fn ready(&self) -> bool {
        self.welcomed && !self.held
    }
}

/// What the machine keeps of an attempt until its `done` is acknowledged.
struct Attempt {
    lease: Duration,
    /// Serialized `event` and `done` messages, by `seq`.
    kept: VecDeque<(u64, String)>,
    kept_bytes: usize,
    next_seq: u64,
    /// The highest `seq` sent over the current channel, or that the service
    /// said it has.
    delivered: u64,
    /// When the machine last sent something for the attempt.
    last_sent: Instant,
    done: bool,
}

impl Attempt {
    fn new(lease: Duration) -> Self {
        Self {
            lease,
            kept: VecDeque::new(),
            kept_bytes: 0,
            next_seq: 1,
            delivered: 0,
            last_sent: Instant::now(),
            done: false,
        }
    }

    fn push_event(&mut self, key: &AttemptKey, event: JobEvent, max_bytes: usize) {
        let message = MachineMessage::Event(EventMessage {
            job_id: key.attempt.job_id.clone(),
            attempt: key.attempt.attempt,
            seq: self.next_seq,
            event,
        });
        self.keep(&message);
        // Oldest first; `done` comes last, so it's never dropped.
        while self.kept_bytes > max_bytes && self.kept.len() > 1 {
            let (_, text) = self.kept.pop_front().expect("kept isn't empty");
            self.kept_bytes -= text.len();
        }
    }

    fn push_done(&mut self, key: &AttemptKey, outcome: Outcome) {
        let message = MachineMessage::Done(Done {
            job_id: key.attempt.job_id.clone(),
            attempt: key.attempt.attempt,
            seq: self.next_seq,
            outcome,
        });
        self.keep(&message);
        self.done = true;
    }

    fn keep(&mut self, message: &MachineMessage) {
        let text = serde_json::to_string(message).expect("machine messages serialize");
        self.kept_bytes += text.len();
        self.kept.push_back((self.next_seq, text));
        self.next_seq += 1;
    }

    /// Sends `link` the kept messages it hasn't had, oldest first. Returns
    /// whether all of them went: not if the outbox filled up.
    fn deliver(&mut self, link: &mut Link, now: Instant) -> bool {
        // Kept messages are in `seq` order, so the unsent ones are the
        // newest: an attempt's every event isn't looked at again each
        // round, however many it sends.
        let unsent = self.kept.partition_point(|(seq, _)| *seq <= self.delivered);
        for (seq, text) in self.kept.range(unsent..) {
            if !link.queue(|| text.clone()) {
                return false;
            }
            self.delivered = *seq;
            self.last_sent = now;
        }
        true
    }

    fn renew_at(&self) -> Instant {
        self.last_sent + (self.lease / 3).clamp(MIN_RENEW_INTERVAL, MAX_RENEW_INTERVAL)
    }
}

struct Running {
    key: AttemptKey,
    /// The approved bundle it runs.
    bundle: BundleHash,
    stopper: Stopper,
    status: JobStatus,
    /// Where its outputs go for its hook, if it succeeds.
    hook: Option<PendingHook>,
}

impl Running {
    /// Asks the attempt to stop; the first reason given wins. It shows as
    /// stopping until the executor returns.
    fn stop(&mut self, reason: StopReason) {
        self.stopper.stop(reason);
        self.status.stopping = true;
    }
}

#[derive(Default)]
struct Shutdown {
    waiters: Vec<oneshot::Sender<()>>,
    /// Once the running attempt has stopped: how long services get to
    /// acknowledge what's left.
    deliver_by: Option<Instant>,
}

/// Why the machine won't take a job now.
enum Unavailable {
    Busy,
    Paused,
    NotApproved(String),
}

impl From<Unavailable> for JobError {
    /// The error of a `done` refusing an assignment (spec §7.2).
    fn from(unavailable: Unavailable) -> Self {
        match unavailable {
            Unavailable::Busy => {
                JobError::new(ErrorCode::Busy, "the machine is running another job")
            }
            Unavailable::Paused => JobError::new(ErrorCode::Busy, "the machine is paused"),
            Unavailable::NotApproved(why) => JobError::new(ErrorCode::NotApproved, why),
        }
    }
}

impl Coordinator {
    /// A coordinator going by nothing yet: see
    /// [`starting_from`](Self::starting_from).
    pub(super) fn new(
        machine: Machine,
        executor: Arc<dyn Executor>,
        options: AgentOptions,
        inputs: mpsc::UnboundedSender<Input>,
        snapshot: watch::Sender<Snapshot>,
        hooks: Result<Hooks, Problem>,
        watcher: (CancellationToken, JoinHandle<()>),
    ) -> Self {
        Self {
            paused: machine.settings().paused,
            machine,
            executor,
            options,
            inputs,
            snapshot,
            hooks,
            settling: None,
            watcher: Some(watcher),
            home: Arc::default(),
            services: BTreeMap::new(),
            leaving: BTreeSet::new(),
            bundles: HashMap::new(),
            attempts: BTreeMap::new(),
            unsent: BTreeSet::new(),
            running: None,
            last_job: None,
            next_task: 0,
            shutdown: None,
            replies: Vec::new(),
        }
    }

    /// Goes by `reading`, the home as the agent starts, and publishes what
    /// it makes of it.
    pub(super) fn starting_from(mut self, reading: Reading) -> Self {
        // A service without its token is connected all the same, off the
        // coordinator: finding none, its channel ends before reaching out.
        self.home_changed(reading, true, None);
        self.publish();
        self
    }

    pub(super) async fn run(mut self, mut inputs: mpsc::UnboundedReceiver<Input>) {
        // `hello` describes the hardware, which may take a moment to
        // detect.
        let detected = self.machine.blocking(|machine| {
            machine.info();
            Ok(())
        });
        let _ = detected.await;
        while !self.shut_down() {
            let renew_at = self.renew_at();
            let deliver_by = self.shutdown.as_ref().and_then(|s| s.deliver_by);
            tokio::select! {
                input = inputs.recv() => match input {
                    Some(input) => self.handle(input),
                    None => break,
                },
                () = sleep_until(renew_at) => {}
                () = sleep_until(deliver_by) => {}
            }
            self.reconcile();
            // What else has arrived meanwhile, such as a burst of progress
            // from a chatty script, makes one round, and one snapshot.
            for _ in 1..MAX_ROUND {
                let Ok(input) = inputs.try_recv() else {
                    break;
                };
                self.handle(input);
                self.reconcile();
            }
            self.publish();
            for reply in self.replies.drain(..) {
                let _ = reply.send(());
            }
        }
        if let Some((stop, watcher)) = self.watcher.take() {
            stop.cancel();
            let _ = watcher.await;
        }
        self.close_all().await;
        // The attempt that ended last gets its hook queued, to run now or
        // when moil next starts.
        if let Some(settling) = self.settling.take() {
            let _ = settling.await;
        }
        if let Ok(hooks) = &self.hooks {
            hooks.shutdown().await;
        }
        for waiter in self.shutdown.take().into_iter().flat_map(|s| s.waiters) {
            let _ = waiter.send(());
        }
    }

    fn handle(&mut self, input: Input) {
        match input {
            Input::Command(command) => self.command(command),
            Input::Link {
                service,
                task,
                event,
            } => {
                // Events from a task that was replaced are stale.
                if self.services.get(&service).is_some_and(|s| s.task == task) {
                    self.link_event(&service, event);
                }
            }
            Input::Job { key, event } => self.job_event(&key, event),
            Input::Finished { key, outcome } => self.finished(key, outcome),
            Input::Paused { saved, reply } => self.paused_saved(saved, reply),
            Input::Home {
                reading,
                left,
                done,
            } => {
                // Shutting down, the agent connects to nothing new.
                if self.shutdown.is_none() {
                    self.home_changed(*reading, false, left);
                }
                self.replies.extend(done);
            }
            Input::Reply(reply) => self.replies.push(reply),
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Disconnect(id, reply) => {
                self.leaving.insert(id.clone());
                match self.disconnect(&id) {
                    Some(join) => {
                        let inputs = self.inputs.clone();
                        tokio::spawn(async move {
                            let _ = tokio::time::timeout(CLOSE_WAIT, join).await;
                            let _ = inputs.send(Input::Reply(reply));
                        });
                    }
                    None => self.replies.push(reply),
                }
            }
            Command::SetPaused(paused, reply) => self.save_paused(paused, reply),
            Command::StopJob(reply) => {
                let stopped = self.running.as_mut().map(|running| {
                    tracing::info!("stopping {}", running.key);
                    running.stop(StopReason::Interrupted);
                    running.status.clone()
                });
                // Answered, like the other commands, once the snapshot
                // shows it.
                let (done, shown) = oneshot::channel();
                self.replies.push(done);
                tokio::spawn(async move {
                    let _ = shown.await;
                    let _ = reply.send(stopped);
                });
            }
            Command::Shutdown(reply) => {
                let deliver_by = Instant::now() + self.options.shutdown_grace;
                let shutdown = self.shutdown.get_or_insert_with(Shutdown::default);
                shutdown.waiters.push(reply);
                match &mut self.running {
                    Some(running) => running.stop(StopReason::Interrupted),
                    None => {
                        shutdown.deliver_by.get_or_insert(deliver_by);
                    }
                }
            }
        }
    }

    /// Goes by `reading`, the home as it is now (spec/machine.md §1.3):
    /// pauses or resumes as `settings.json` says, connects to the services
    /// the home pairs this machine with and disconnects from the others,
    /// follows their approvals, and deletes the tokens of the pairings the
    /// home no longer has. Everything is compared with what the agent goes
    /// by, so a change is applied once, however many readings show it.
    /// The bundles already approved when the agent starts are prepared
    /// only as their jobs come; others as they're approved. `left` is a
    /// service the owner has just unpaired, which is no longer
    /// [`leaving`](Self::leaving).
    fn home_changed(&mut self, mut reading: Reading, starting: bool, left: Option<ServiceId>) {
        reading.keep_from(&self.home);
        let Reading {
            mut home,
            settings,
            bundles,
            ..
        } = reading;
        if let Err(problem) = &self.hooks {
            home.problems.push(problem.clone());
        }
        if let Some(change) = settings
            && change.after.paused != change.before.paused
            && change.after.paused != self.paused
        {
            self.paused = change.after.paused;
            let now = if self.paused { "paused" } else { "resumed" };
            tracing::info!("{now}, as settings.json now says");
        }
        // Unpairing deleted those it was asked to.
        let gone: Vec<ServiceId> = self
            .home
            .pairing_files
            .iter()
            .filter(|id| self.home.services.contains_key(*id))
            .filter(|id| !home.services.contains_key(*id) || !home.pairing_files.contains(*id))
            .filter(|id| !self.leaving.contains(*id) && left.as_ref() != Some(*id))
            .cloned()
            .collect();
        if let Some(left) = &left {
            self.leaving.remove(left);
        }
        self.follow_services(&home, &bundles, starting);
        for id in gone {
            self.forget(id);
        }
        if *self.home != home {
            self.home = Arc::new(home);
        }
    }

    /// Connects to the services `home` pairs this machine with, disconnects
    /// from the others, and follows the approvals of those it stays
    /// connected to, with `bundles`, the verified copies of what they
    /// approve.
    fn follow_services(
        &mut self,
        home: &HomeState,
        bundles: &BTreeMap<ServiceId, Vec<Bundle>>,
        starting: bool,
    ) {
        let connected: Vec<ServiceId> = self.services.keys().cloned().collect();
        for id in connected {
            let Some(service) = home.services.get(&id).filter(|s| s.pairing.is_some()) else {
                let name = &self.services[&id].paired.name;
                tracing::info!(
                    "disconnecting from {name}: this machine is no longer paired with it"
                );
                self.disconnect(&id);
                continue;
            };
            self.follow_approvals(service);
            if self.services[&id].paired.pairing != service.pairing {
                tracing::info!("paired with {} ({})", service.name, service.base_url);
                if let Some(old) = self.services.remove(&id) {
                    old.stop.cancel();
                }
                self.connect(service.clone());
            }
        }
        let new: Vec<&services::Service> = home
            .services
            .values()
            .filter(|s| s.pairing.is_some())
            .filter(|s| !self.services.contains_key(&s.id) && !self.leaving.contains(&s.id))
            .collect();
        for service in new {
            if !starting {
                tracing::info!("paired with {} ({})", service.name, service.base_url);
            }
            self.connect(service.clone());
        }
        // Such as those of a folder cloned from git, once it's paired.
        let mut approved = Vec::new();
        for (id, service) in &self.services {
            let announced: BTreeSet<BundleHash> =
                service.paired.approved_hashes().into_iter().collect();
            approved.extend(
                bundles
                    .get(id)
                    .into_iter()
                    .flatten()
                    .filter(|b| {
                        announced.contains(&b.hash()) && !self.bundles.contains_key(&b.hash())
                    })
                    .cloned(),
            );
        }
        for bundle in approved {
            if self.bundles.insert(bundle.hash(), bundle.clone()).is_none() && !starting {
                self.prepare(bundle);
            }
        }
        self.drop_unapproved_bundles();
    }

    /// Goes by the approvals `service` has now, if the agent is connected
    /// to it: its running attempt stops if its bundle no longer counts as
    /// approved, and the service is told what is (in
    /// [`reconcile`](Self::reconcile)).
    fn follow_approvals(&mut self, service: &services::Service) {
        let Some(connected) = self.services.get_mut(&service.id) else {
            return;
        };
        let before = &connected.paired;
        let name = &service.name;
        for approval in &service.approvals {
            if !before.is_approved(&approval.hash) {
                tracing::info!(
                    "{name}: approved {} {} ({})",
                    approval.name,
                    approval.version,
                    approval.hash
                );
            }
        }
        for approval in &before.approvals {
            if !service.is_approved(&approval.hash) {
                tracing::info!(
                    "{name}: {} {} ({}) is no longer approved",
                    approval.name,
                    approval.version,
                    approval.hash
                );
            }
        }
        if let Some(running) = &mut self.running
            && running.key.service == service.id
            && !service.is_approved(&running.bundle)
            && !running.status.stopping
        {
            // The owner no longer consents to running it.
            tracing::info!("stopping {}: its bundle is no longer approved", running.key);
            running.stop(StopReason::Interrupted);
        }
        connected.paired.name.clone_from(&service.name);
        connected.paired.approvals.clone_from(&service.approvals);
    }

    /// Closes the channel of service `id` and forgets its attempts,
    /// stopping the running one. Returns the channel's task, if there was
    /// one, which ends once the channel has closed.
    fn disconnect(&mut self, id: &ServiceId) -> Option<JoinHandle<()>> {
        self.drop_attempts(id);
        let removed = self.services.remove(id);
        self.drop_unapproved_bundles();
        removed.map(|service| {
            service.stop.cancel();
            service.join
        })
    }

    /// Deletes this machine's token for service `id`, off the coordinator,
    /// if its folder no longer has its pairing (spec/machine.md §1.3). The
    /// service still lists the machine until the owner removes it there.
    fn forget(&self, id: ServiceId) {
        let machine = self.machine.clone();
        tokio::task::spawn_blocking(move || match machine.forget_if_gone(&id) {
            Ok(true) => tracing::info!(
                "deleted this machine's token for {id}, which the home no longer pairs it with"
            ),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                "couldn't delete this machine's token for {id}, which the home no longer pairs \
                 it with: {e}"
            ),
        });
    }

    fn link_event(&mut self, id: &ServiceId, event: LinkEvent) {
        let Some(service) = self.services.get_mut(id) else {
            return;
        };
        match event {
            LinkEvent::Connecting => {
                service.link = None;
                service.connection = Connection::Connecting;
            }
            LinkEvent::Open(outbox) => self.hello(id, outbox),
            LinkEvent::Message(message) => self.message(id, message),
            LinkEvent::Unreadable { attempt, reason } => {
                if service.link.as_ref().is_some_and(|l| l.welcomed) {
                    self.unreadable(id, attempt, reason);
                }
            }
            LinkEvent::Drained => {
                if let Some(link) = &mut service.link {
                    link.held = false;
                }
            }
            LinkEvent::Lost { error, retry_at } => {
                tracing::info!("lost {}: {error}", service.paired.name);
                service.link = None;
                service.connection = Connection::Disconnected { error, retry_at };
            }
            LinkEvent::Unpaired => {
                service.link = None;
                service.connection = Connection::Unpaired;
                self.drop_attempts(id);
            }
            LinkEvent::Replaced => {
                tracing::warn!(
                    "another connection with this machine's token replaced this one at {}",
                    service.paired.name
                );
                service.link = None;
                service.connection = Connection::Replaced;
                self.drop_attempts(id);
            }
        }
    }

    /// Opens the conversation (spec §6.2).
    fn hello(&mut self, id: &ServiceId, outbox: Outbox) {
        let state = self.state();
        let announced: Vec<AttemptRef> = self
            .attempts
            .keys()
            .filter(|key| key.service == *id)
            .map(|key| key.attempt.clone())
            .collect();
        let Some(service) = self.services.get_mut(id) else {
            return;
        };
        let approved = service.paired.approved_hashes();
        warn_unannounced(&service.paired);
        let mut link = Link {
            outbox,
            held: false,
            welcomed: false,
            announced: announced.clone(),
            state,
            approved: approved.clone(),
        };
        link.send(&MachineMessage::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            app_version: self.machine.app_version().to_owned(),
            machine: self.machine.info(),
            state,
            approved,
            attempts: announced,
        }));
        service.link = Some(link);
    }

    fn message(&mut self, id: &ServiceId, message: ServiceMessage) {
        let Some(service) = self.services.get_mut(id) else {
            return;
        };
        let welcomed = service.link.as_ref().is_some_and(|l| l.welcomed);
        if !welcomed && !matches!(message, ServiceMessage::Welcome(_)) {
            tracing::debug!("ignoring {message:?} from {id}, which hasn't answered hello");
            return;
        }
        match message {
            ServiceMessage::Welcome(welcome) => self.welcome(id, welcome),
            ServiceMessage::Bundles { bundles } => service.bundles = bundles,
            ServiceMessage::Offer(offer) => self.offer(id, offer),
            ServiceMessage::Assign(assign) => self.assign(id, assign),
            ServiceMessage::Ack(attempt) => self.ack(id, attempt),
            ServiceMessage::Cancel(attempt) => self.cancel(id, attempt),
            ServiceMessage::Unknown => {}
        }
    }

    /// Settles the attempts `hello` listed (spec §7.5): the ones the service
    /// resumes pick up after the last `seq` it has, the others are
    /// abandoned.
    fn welcome(&mut self, id: &ServiceId, welcome: Welcome) {
        let Some(service) = self.services.get_mut(id) else {
            return;
        };
        let Some(link) = service.link.as_mut().filter(|l| !l.welcomed) else {
            return;
        };
        if welcome.protocol != PROTOCOL_VERSION {
            tracing::warn!("{id} says it speaks protocol {}", welcome.protocol);
        }
        link.welcomed = true;
        let announced = std::mem::take(&mut link.announced);
        service.connection = Connection::Connected;
        service.bundles = welcome.bundles;
        tracing::info!(
            "connected to {} ({})",
            service.paired.name,
            service.paired.base_url
        );
        for attempt in announced {
            let key = AttemptKey {
                service: id.clone(),
                attempt,
            };
            let resumed = welcome
                .resume
                .iter()
                .find(|r| r.job_id == key.attempt.job_id && r.attempt == key.attempt.attempt);
            match (resumed, self.attempts.get_mut(&key)) {
                (Some(resume), Some(attempt)) => {
                    attempt.delivered = resume.seq;
                    self.unsent.insert(key);
                }
                (Some(_), None) => {}
                (None, _) => {
                    tracing::info!("{id} no longer expects {key}; abandoning it");
                    self.abandon(&key);
                }
            }
        }
    }

    fn offer(&mut self, id: &ServiceId, offer: Offer) {
        let answer = match self.availability(id, &offer.bundle_hash) {
            Ok(_) => MachineMessage::Bid {
                job_id: offer.job_id,
            },
            Err(unavailable) => MachineMessage::Decline {
                job_id: offer.job_id,
                reason: match unavailable {
                    Unavailable::Busy => DeclineReason::Busy,
                    Unavailable::Paused => DeclineReason::Paused,
                    Unavailable::NotApproved(_) => DeclineReason::NotApproved,
                },
            },
        };
        if !self.send(id, &answer) {
            // The offer expires unanswered, as if it had been lost.
            tracing::debug!("couldn't answer an offer from {id}: its channel is backed up");
        }
    }

    /// Starts an attempt, or reports at once why it can't start (spec
    /// §7.2).
    fn assign(&mut self, id: &ServiceId, assign: Assign) {
        let key = AttemptKey {
            service: id.clone(),
            attempt: assign.attempt_ref(),
        };
        if self.attempts.contains_key(&key) {
            tracing::warn!("ignoring a second assign for {key}");
            return;
        }
        let bundle = match self.availability(id, &assign.bundle_hash) {
            Ok(bundle) => bundle.clone(),
            Err(unavailable) => return self.refuse(key, unavailable.into()),
        };
        tracing::info!("starting {key}: {}", bundle.manifest().name);
        // Where the attempt leaves its outputs if it succeeds, for the hook
        // there may be by then.
        let hook = self.hooks.as_ref().ok().map(|hooks| {
            let service = ServiceRef::from(&self.services[id].paired);
            hooks.entry(service, BundleRef::from(&bundle), &assign)
        });
        let (stopper, stop) = moil_runtime::stop_signal();
        let status = JobStatus {
            service: id.clone(),
            job_id: assign.job_id.clone(),
            attempt: assign.attempt,
            title: assign.title.clone(),
            bundle_name: bundle.manifest().name.clone(),
            phase: Phase::Preparing,
            fraction: None,
            message: None,
            started_at: Instant::now(),
            stopping: false,
        };
        self.attempts.insert(
            key.clone(),
            Attempt::new(Duration::from_millis(assign.lease_ms)),
        );
        let mut job = Job::from_assign(assign, bundle);
        job.url_policy = self.url_policy(&key.service);
        job.hand_off = hook.as_ref().map(PendingHook::hand_off);
        self.running = Some(Running {
            key: key.clone(),
            bundle: job.bundle.hash(),
            stopper,
            status,
            hook,
        });
        self.spawn_attempt(key, job, stop);
    }

    /// Refuses an assignment the machine can't read (spec §7.2), unless it
    /// names an attempt the machine already holds.
    fn unreadable(&mut self, id: &ServiceId, attempt: AttemptRef, reason: String) {
        let key = AttemptKey {
            service: id.clone(),
            attempt,
        };
        if self.attempts.contains_key(&key) {
            tracing::warn!(
                "ignoring a second assign for {key}, which moil can't read either: {reason}"
            );
            return;
        }
        self.refuse(key, JobError::new(ErrorCode::Rejected, reason));
    }

    /// Answers an assignment the machine won't run with a failed `done`.
    /// The machine doesn't keep it (spec §7.5): if it doesn't reach the
    /// service, the service fails the attempt as `lost`.
    fn refuse(&mut self, key: AttemptKey, error: JobError) {
        tracing::info!("refused {key}: {}", error.message);
        let done = MachineMessage::Done(Done {
            job_id: key.attempt.job_id.clone(),
            attempt: key.attempt.attempt,
            seq: 1,
            outcome: Outcome::failed(error),
        });
        if !self.send(&key.service, &done) {
            tracing::debug!("couldn't tell {} that {key} was refused", key.service);
        }
    }

    /// Which URLs the jobs of service `id` may use for their files (spec
    /// §3).
    fn url_policy(&self, id: &ServiceId) -> UrlPolicy {
        let insecure = self.machine.allows_insecure_http();
        match self.services.get(id) {
            Some(service) => UrlPolicy::for_service(&service.paired.base_url, insecure),
            None => UrlPolicy {
                any_http: insecure,
                ..UrlPolicy::HTTPS_ONLY
            },
        }
    }

    /// Whether the machine takes a job for `hash` from `id` now (spec
    /// §7.1): idle, approved, and holding a verified copy.
    fn availability(&self, id: &ServiceId, hash: &BundleHash) -> Result<&Bundle, Unavailable> {
        if self.running.is_some() {
            return Err(Unavailable::Busy);
        }
        if self.paused || self.shutdown.is_some() {
            return Err(Unavailable::Paused);
        }
        let approved = self
            .services
            .get(id)
            .is_some_and(|s| s.paired.is_approved(hash));
        if !approved {
            return Err(Unavailable::NotApproved(format!(
                "the machine's owner hasn't approved bundle {hash}"
            )));
        }
        self.bundles.get(hash).ok_or_else(|| {
            Unavailable::NotApproved(format!("the machine has no verified copy of bundle {hash}"))
        })
    }

    /// Forgets the verified bundles no service's approval names anymore.
    fn drop_unapproved_bundles(&mut self) {
        let services = &self.services;
        self.bundles
            .retain(|hash, _| services.values().any(|s| s.paired.is_approved(hash)));
    }

    /// Saves in `settings.json` whether the machine is `paused`, off the
    /// coordinator, which goes by it once saved
    /// ([`paused_saved`](Self::paused_saved)).
    fn save_paused(&self, paused: bool, reply: oneshot::Sender<Result<(), Error>>) {
        let machine = self.machine.clone();
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            let saved = machine
                .blocking(move |machine| machine.update_settings(|s| s.paused = paused))
                .await;
            let _ = inputs.send(Input::Paused {
                saved: saved.map(drop),
                reply,
            });
        });
    }

    /// Pauses or resumes as the settings say, now that the owner's choice
    /// is `saved` in them, and answers `reply` once the snapshot shows it.
    /// A choice that couldn't be saved changes nothing.
    fn paused_saved(
        &mut self,
        saved: Result<(), Error>,
        reply: oneshot::Sender<Result<(), Error>>,
    ) {
        match &saved {
            // Whichever choice was saved last.
            Ok(()) => {
                let paused = self.machine.settings().paused;
                if paused != self.paused {
                    self.paused = paused;
                    tracing::info!("{}", if paused { "paused" } else { "resumed" });
                }
            }
            Err(e) => tracing::warn!("couldn't save whether the machine is paused: {e}"),
        }
        let (done, shown) = oneshot::channel();
        self.replies.push(done);
        tokio::spawn(async move {
            let _ = shown.await;
            let _ = reply.send(saved);
        });
    }

    fn spawn_attempt(&self, key: AttemptKey, job: Job, stop: StopSignal) {
        let executor = self.executor.clone();
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            let (events, mut received) = mpsc::unbounded_channel();
            let run = AssertUnwindSafe(executor.run(job, events, stop)).catch_unwind();
            tokio::pin!(run);
            let forward = |event| {
                let key = key.clone();
                let _ = inputs.send(Input::Job { key, event });
            };
            let outcome = loop {
                tokio::select! {
                    biased;
                    Some(event) = received.recv() => forward(event),
                    outcome = &mut run => break outcome,
                }
            };
            while let Ok(event) = received.try_recv() {
                forward(event);
            }
            let outcome = outcome.unwrap_or_else(|panic| {
                let what = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                Outcome::failed(JobError::new(
                    ErrorCode::Internal,
                    format!("the runtime crashed: {what}"),
                ))
            });
            let _ = inputs.send(Input::Finished { key, outcome });
        });
    }

    fn job_event(&mut self, key: &AttemptKey, event: JobEvent) {
        if let Some(running) = self.running.as_mut().filter(|r| r.key == *key) {
            let status = &mut running.status;
            match &event {
                JobEvent::Phase { phase } => status.phase = *phase,
                JobEvent::Progress { fraction, message } => {
                    if fraction.is_some() {
                        status.fraction = *fraction;
                    }
                    if message.is_some() {
                        status.message.clone_from(message);
                    }
                    if let Some(message) = message {
                        tracing::debug!("{key}: {message}");
                    }
                }
                JobEvent::Log { .. } | JobEvent::Data { .. } => {}
            }
        }
        // Abandoned attempts are gone, and so are their late events.
        if let Some(attempt) = self.attempts.get_mut(key) {
            attempt.push_event(key, event, self.options.max_kept_bytes);
            self.unsent.insert(key.clone());
        }
    }

    fn finished(&mut self, key: AttemptKey, outcome: Outcome) {
        let mut hook = None;
        if let Some(running) = self.running.take_if(|r| r.key == key) {
            let report = running.status.ended((&outcome).into());
            hook = running.hook.map(|hook| (hook, report.ended_at));
            self.last_job = Some(report);
            if let Some(shutdown) = &mut self.shutdown {
                shutdown.deliver_by = Some(Instant::now() + self.options.shutdown_grace);
            }
        }
        let mut succeeded = None;
        if let Some(attempt) = self.attempts.get_mut(&key) {
            match &outcome {
                Outcome::Succeeded { result } => {
                    tracing::info!("{key} succeeded");
                    succeeded = Some(result.clone());
                }
                Outcome::Failed { error } => tracing::info!("{key} failed: {error}"),
                Outcome::Cancelled => tracing::info!("{key} was cancelled"),
            }
            attempt.push_done(&key, outcome);
            self.unsent.insert(key.clone());
        }
        // Only once its `done` is queued, so that the hook can't hold up or
        // change what the service hears (spec/machine.md §2.1). An attempt
        // that failed, or whose service no longer hears of it, gets none.
        if let Some((hook, ended_at)) = hook {
            self.settle(hook, succeeded.map(|result| (result, ended_at)));
        }
    }

    /// Queues the hook of an attempt if it `succeeded`, with its result
    /// and end, and removes its entry otherwise: off the coordinator, since
    /// both take the disk a moment, but after the attempt that ended before
    /// it, so that hooks run in the order their attempts ended.
    fn settle(&mut self, hook: PendingHook, succeeded: Option<(JobResult, SystemTime)>) {
        let before = self.settling.take();
        self.settling = Some(tokio::spawn(async move {
            if let Some(before) = before {
                let _ = before.await;
            }
            let settled = tokio::task::spawn_blocking(move || match succeeded {
                Some((result, ended_at)) => hook.ready(result, ended_at),
                None => drop(hook),
            });
            let _ = settled.await;
        }));
    }

    fn ack(&mut self, id: &ServiceId, attempt: AttemptRef) {
        let key = AttemptKey {
            service: id.clone(),
            attempt,
        };
        if self.attempts.get(&key).is_some_and(|a| a.done) {
            self.attempts.remove(&key);
        }
    }

    fn cancel(&mut self, id: &ServiceId, attempt: AttemptRef) {
        let key = AttemptKey {
            service: id.clone(),
            attempt,
        };
        if let Some(running) = self.running.as_mut().filter(|r| r.key == key) {
            tracing::info!("{id} cancelled {key}");
            running.stop(StopReason::Cancelled);
        }
    }

    /// Stops an attempt, if it runs, and forgets it: nothing more about it
    /// is sent.
    fn abandon(&mut self, key: &AttemptKey) {
        self.attempts.remove(key);
        if let Some(running) = self.running.as_mut().filter(|r| r.key == *key) {
            running.stop(StopReason::Cancelled);
        }
    }

    /// Forgets every attempt of a service that no longer takes this
    /// machine's messages, stopping the running one.
    fn drop_attempts(&mut self, id: &ServiceId) {
        self.attempts.retain(|key, _| key.service != *id);
        if let Some(running) = self.running.as_mut().filter(|r| r.key.service == *id) {
            running.stop(StopReason::Interrupted);
        }
    }

    /// Sends each welcomed service what it's missing: attempt messages it
    /// hasn't seen and lease renewals, then the machine's state and
    /// approvals if they changed. An attempt's `done` thus comes before the
    /// `idle` it leads to.
    fn reconcile(&mut self) {
        let now = Instant::now();
        for key in std::mem::take(&mut self.unsent) {
            let Some(attempt) = self.attempts.get_mut(&key) else {
                continue;
            };
            // Until the service answers hello there's nothing to send;
            // `welcome` marks what it's missing.
            let Some(link) = self
                .services
                .get_mut(&key.service)
                .and_then(|s| s.link.as_mut())
                .filter(|l| l.welcomed)
            else {
                continue;
            };
            if !attempt.deliver(link, now) {
                // The rest goes once the outbox has room.
                self.unsent.insert(key);
            }
        }
        if let Some(running) = &self.running
            && let Some(attempt) = self.attempts.get_mut(&running.key)
            && let Some(link) = self
                .services
                .get_mut(&running.key.service)
                .and_then(|s| s.link.as_mut())
                .filter(|l| l.ready())
            && now >= attempt.renew_at()
            && link.send(&MachineMessage::Renew(running.key.attempt.clone()))
        {
            attempt.last_sent = now;
        }

        let state = self.state();
        for service in self.services.values_mut() {
            let Some(link) = service.link.as_mut().filter(|l| l.ready()) else {
                continue;
            };
            if link.state != state && link.send(&MachineMessage::State { state }) {
                link.state = state;
            }
            let approved = service.paired.approved_hashes();
            if link.approved != approved {
                let message = MachineMessage::Approved {
                    hashes: approved.clone(),
                };
                if link.send(&message) {
                    link.approved = approved;
                    warn_unannounced(&service.paired);
                }
            }
        }
    }

    /// When the running attempt next needs a `renew`, if its service takes
    /// messages now.
    fn renew_at(&self) -> Option<Instant> {
        let key = &self.running.as_ref()?.key;
        let link = self.services.get(&key.service)?.link.as_ref()?;
        link.ready()
            .then(|| self.attempts.get(key).map(Attempt::renew_at))
            .flatten()
    }

    /// The state every service is told (spec §6.2).
    fn state(&self) -> MachineState {
        if self.running.is_some() {
            MachineState::Busy
        } else if self.paused || self.shutdown.is_some() {
            MachineState::Paused
        } else {
            MachineState::Idle
        }
    }

    /// Whether shutting down is over: the running attempt has stopped, and
    /// connected services have acknowledged what they were sent, or had
    /// their chance to.
    fn shut_down(&self) -> bool {
        let Some(shutdown) = &self.shutdown else {
            return false;
        };
        if self.running.is_some() {
            return false;
        }
        let grace_over = shutdown.deliver_by.is_some_and(|t| Instant::now() >= t);
        let unacknowledged = self.attempts.iter().any(|(key, attempt)| {
            attempt.done
                && self
                    .services
                    .get(&key.service)
                    .and_then(|s| s.link.as_ref())
                    .is_some_and(|l| l.welcomed)
        });
        grace_over || !unacknowledged
    }

    async fn close_all(&mut self) {
        let closing: Vec<JoinHandle<()>> = std::mem::take(&mut self.services)
            .into_values()
            .map(|service| {
                service.stop.cancel();
                service.join
            })
            .collect();
        let _ = tokio::time::timeout(CLOSE_WAIT, futures::future::join_all(closing)).await;
    }

    fn connect(&mut self, paired: services::Service) {
        let task = self.next_task;
        self.next_task += 1;
        let stop = CancellationToken::new();
        let join = connection::spawn(
            self.machine.clone(),
            &paired,
            task,
            self.options.clone(),
            self.inputs.clone(),
            stop.clone(),
        );
        self.services.insert(
            paired.id.clone(),
            Service {
                paired,
                connection: Connection::Connecting,
                bundles: Vec::new(),
                task,
                stop,
                join,
                link: None,
            },
        );
    }

    /// Gets a newly approved bundle ready in the background, so its first
    /// job starts sooner.
    fn prepare(&self, bundle: Bundle) {
        let executor = self.executor.clone();
        tokio::spawn(async move {
            let name = &bundle.manifest().name;
            match executor.prepare(&bundle).await {
                Ok(()) => tracing::info!("prepared {name} ({})", bundle.hash()),
                Err(e) => tracing::warn!("couldn't prepare {name} ({}): {e}", bundle.hash()),
            }
        });
    }

    /// Answers service `id` right away, such as with a bid; what follows
    /// from the machine's state goes out in [`reconcile`](Self::reconcile).
    /// Returns whether the message went: not while the service's outbox is
    /// full.
    fn send(&mut self, id: &ServiceId, message: &MachineMessage) -> bool {
        self.services
            .get_mut(id)
            .and_then(|s| s.link.as_mut())
            .is_some_and(|link| link.send(message))
    }

    fn publish(&self) {
        let snapshot = Snapshot {
            paused: self.paused,
            services: self
                .services
                .values()
                .map(|s| ServiceStatus {
                    id: s.paired.id.clone(),
                    name: s.paired.name.clone(),
                    base_url: s.paired.base_url.clone(),
                    connection: s.connection.clone(),
                    bundles: s
                        .bundles
                        .iter()
                        .map(|info| BundleStatus {
                            info: info.clone(),
                            approved: s.paired.is_approved(&info.hash),
                        })
                        .collect(),
                    insecure: s.paired.is_insecure(),
                })
                .collect(),
            job: self.running.as_ref().map(|r| r.status.clone()),
            last_job: self.last_job.clone(),
            home: self.home.clone(),
        };
        self.snapshot
            .send_if_modified(|current| replace(current, snapshot));
    }
}

/// Replaces `current` with `new` if they differ, and says whether they did.
fn replace<T: PartialEq>(current: &mut T, new: T) -> bool {
    let changed = *current != new;
    if changed {
        *current = new;
    }
    changed
}

/// Says so when `service` isn't told about every bundle approved for it.
fn warn_unannounced(service: &services::Service) {
    let older = service.unannounced_approvals();
    if older > 0 {
        tracing::warn!(
            "{} is told only of the {} bundles approved for it most recently; {older} older \
             approvals aren't announced, so it won't offer their jobs",
            service.name,
            services::Service::MAX_ANNOUNCED
        );
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use futures::future::BoxFuture;
    use moil_proto::control::{ServiceInfo, Welcome};
    use moil_runtime::EventSink;
    use url::Url;

    use super::*;
    use crate::{Home, MachineConfig, MemorySecrets, Review};

    /// Hands every job it's given to the test.
    #[derive(Debug)]
    struct Jobs(mpsc::UnboundedSender<Job>);

    impl Executor for Jobs {
        fn prepare<'a>(&'a self, _: &'a Bundle) -> BoxFuture<'a, Result<(), JobError>> {
            Box::pin(async { Ok(()) })
        }

        fn run(&self, job: Job, _: EventSink, mut stop: StopSignal) -> BoxFuture<'_, Outcome> {
            let _ = self.0.send(job);
            Box::pin(async move {
                stop.stopped().await;
                Outcome::Cancelled
            })
        }
    }

    /// The job an assignment from the service at `base_url` becomes. The
    /// test plays the connection, so the service needn't exist: no remote
    /// service does in tests.
    async fn job_from(base_url: &str) -> Job {
        let dir = tempfile::tempdir().unwrap();
        let machine = Machine::open(MachineConfig {
            home: Home::new(dir.path()),
            secrets: Arc::new(MemorySecrets::default()),
            app_version: "0.1.0-test".into(),
        })
        .unwrap();
        let base_url = Url::parse(base_url).unwrap();
        let (paired, _) = machine
            .record_pairing(&base_url, "tide".into(), "m_1".into(), "moil_token")
            .unwrap();
        let bundle = Bundle::new(
            "# /// script\n# requires-python = \">=3.12\"\n# dependencies = []\n# ///\n",
            "version = 1\nrevision = 3\nrequires-python = \">=3.12\"\n",
            r#"{"moil": 1, "name": "echo", "version": "1.0.0"}"#,
        )
        .unwrap();
        let hash = bundle.hash();
        let review = Review {
            service: paired,
            bundle,
            previous: None,
            approved: false,
        };
        let id = machine.approve(&review).unwrap().id;

        let (jobs, mut ran) = mpsc::unbounded_channel();
        let (inputs, _) = mpsc::unbounded_channel();
        let reading = crate::agent::read_home(&machine);
        let watcher = (CancellationToken::new(), tokio::spawn(async {}));
        let hooks = Err(Problem {
            path: "hooks".into(),
            message: "not in this test".into(),
        });
        let mut coordinator = Coordinator::new(
            machine,
            Arc::new(Jobs(jobs)),
            AgentOptions::default(),
            inputs,
            watch::Sender::default(),
            hooks,
            watcher,
        )
        .starting_from(reading);
        coordinator.services[&id].stop.cancel();
        let (outbox, _frames) = connection::outbox();
        let welcome = Welcome {
            protocol: PROTOCOL_VERSION,
            machine_id: "m_1".into(),
            service: ServiceInfo {
                name: "tide".into(),
            },
            bundles: vec![],
            resume: vec![],
        };
        let assign = Assign {
            job_id: "j1".parse().unwrap(),
            attempt: 1,
            bundle_hash: hash,
            title: None,
            params: serde_json::Value::Null,
            inputs: Default::default(),
            outputs: Default::default(),
            lease_ms: 120_000,
            timeout_ms: None,
        };
        for event in [
            LinkEvent::Open(outbox),
            LinkEvent::Message(ServiceMessage::Welcome(welcome)),
            LinkEvent::Message(ServiceMessage::Assign(assign)),
        ] {
            let service = id.clone();
            coordinator.handle(Input::Link {
                service,
                task: 0,
                event,
            });
        }
        ran.recv().await.expect("the attempt runs")
    }

    fn allows(job: &Job, url: &str) -> bool {
        job.url_policy.check(&Url::parse(url).unwrap()).is_ok()
    }

    #[tokio::test]
    async fn a_remote_services_jobs_cant_use_http_to_this_machine() {
        let job = job_from("https://tide.example.com/moil").await;
        assert!(allows(&job, "https://s3.example.com/meeting.ogg"));
        for refused in [
            "http://127.0.0.1:6379/",
            "http://localhost:8080/admin",
            "http://[::1]/",
            "http://s3.example.com/meeting.ogg",
        ] {
            assert!(!allows(&job, refused), "{refused}");
        }
    }

    #[tokio::test]
    async fn a_service_on_this_machine_may_use_http_to_it() {
        let job = job_from("http://127.0.0.1:8787/moil").await;
        assert!(allows(&job, "http://127.0.0.1:8787/files/meeting.ogg"));
        assert!(allows(&job, "http://localhost:9000/bucket/out.json"));
        assert!(allows(&job, "https://s3.example.com/meeting.ogg"));
        assert!(!allows(&job, "http://s3.example.com/meeting.ogg"));
    }
}
