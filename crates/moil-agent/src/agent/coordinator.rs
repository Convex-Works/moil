//! The one owner of machine-wide state: paused or not, the running attempt,
//! and everything each attempt still has to deliver.
//!
//! The coordinator handles one input at a time (a command from the handle,
//! an event from a connection or from the running attempt), then
//! reconciles: every welcomed service is told the machine's current state
//! and approvals if they changed, gets the attempt messages it hasn't seen,
//! and gets a `renew` when its running attempt has been quiet for a third
//! of its lease. Deriving what to send from state, rather than sending as
//! things happen, is what makes reconnecting and resuming (spec §7.5) fall
//! out naturally.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt;
use moil_bundle::Bundle;
use moil_proto::control::{BundleInfo, EventMessage, Hello, Welcome};
use moil_proto::{
    Assign, AttemptRef, BundleHash, DeclineReason, Done, ErrorCode, JobError, JobEvent,
    MachineMessage, MachineState, Offer, Outcome, PROTOCOL_VERSION, Phase, ServiceMessage,
};
use moil_runtime::{Executor, Job, StopReason, StopSignal, Stopper};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::connection::{self, LinkEvent};
use super::{AgentOptions, BundleStatus, Connection, JobStatus, ServiceStatus, Snapshot};
use crate::{Machine, PairedService, ServiceId};

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
    /// Answer a caller, after everything that came before is visible.
    Reply(oneshot::Sender<()>),
}

/// What the handle asks for. Commands carrying a sender are answered once
/// their effects show in the snapshot.
#[derive(Debug)]
pub(super) enum Command {
    /// A pairing completed: connect to the service, again if it was paired
    /// before.
    Paired(Box<PairedService>),
    /// Close the service's channel and forget its attempts.
    Disconnect(ServiceId, oneshot::Sender<()>),
    /// The service's approvals changed; prepare the bundle if one was
    /// approved.
    Approvals(Box<PairedService>, Option<Box<Bundle>>, oneshot::Sender<()>),
    SetPaused(bool),
    /// The owner stopped the running job.
    StopJob,
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
/// How long shutting down waits for connections to close.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct Coordinator {
    machine: Machine,
    executor: Arc<dyn Executor>,
    options: AgentOptions,
    /// Handed to the tasks the coordinator starts, to report back.
    inputs: mpsc::UnboundedSender<Input>,
    snapshot: watch::Sender<Snapshot>,
    paused: bool,
    services: BTreeMap<ServiceId, Service>,
    /// Attempts whose messages the machine keeps until the service
    /// acknowledges their `done`.
    attempts: BTreeMap<AttemptKey, Attempt>,
    /// The one attempt running on this machine, if any.
    running: Option<Running>,
    next_task: u64,
    shutdown: Option<Shutdown>,
    /// Callers to answer at the end of this round.
    replies: Vec<oneshot::Sender<()>>,
}

/// A paired service and its channel.
struct Service {
    paired: PairedService,
    connection: Connection,
    bundles: Vec<BundleInfo>,
    task: u64,
    stop: CancellationToken,
    join: JoinHandle<()>,
    link: Option<Link>,
}

/// An open channel to a service.
struct Link {
    outbox: mpsc::UnboundedSender<String>,
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
    fn send(&self, message: &MachineMessage) {
        let text = serde_json::to_string(message).expect("machine messages serialize");
        let _ = self.outbox.send(text);
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

    fn renew_at(&self) -> Instant {
        self.last_sent + (self.lease / 3).clamp(MIN_RENEW_INTERVAL, MAX_RENEW_INTERVAL)
    }
}

struct Running {
    key: AttemptKey,
    stopper: Stopper,
    status: JobStatus,
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

impl Coordinator {
    pub(super) fn new(
        machine: Machine,
        executor: Arc<dyn Executor>,
        options: AgentOptions,
        inputs: mpsc::UnboundedSender<Input>,
        snapshot: watch::Sender<Snapshot>,
    ) -> Self {
        let mut coordinator = Self {
            paused: machine.settings().paused,
            machine,
            executor,
            options,
            inputs,
            snapshot,
            services: BTreeMap::new(),
            attempts: BTreeMap::new(),
            running: None,
            next_task: 0,
            shutdown: None,
            replies: Vec::new(),
        };
        match coordinator.machine.services() {
            Ok(services) => services.into_iter().for_each(|s| coordinator.connect(s)),
            Err(e) => tracing::error!("couldn't read the paired services: {e}"),
        }
        coordinator.publish();
        coordinator
    }

    pub(super) async fn run(mut self, mut inputs: mpsc::UnboundedReceiver<Input>) {
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
            self.publish();
            for reply in self.replies.drain(..) {
                let _ = reply.send(());
            }
        }
        self.close_all().await;
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
            Input::Reply(reply) => self.replies.push(reply),
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Paired(service) => {
                tracing::info!("paired with {} ({})", service.name, service.base_url);
                if let Some(old) = self.services.remove(&service.id) {
                    old.stop.cancel();
                }
                self.connect(*service);
            }
            Command::Disconnect(id, reply) => {
                self.drop_attempts(&id);
                match self.services.remove(&id) {
                    Some(service) => {
                        service.stop.cancel();
                        let inputs = self.inputs.clone();
                        tokio::spawn(async move {
                            let _ = service.join.await;
                            let _ = inputs.send(Input::Reply(reply));
                        });
                    }
                    None => self.replies.push(reply),
                }
            }
            Command::Approvals(paired, approved, reply) => {
                if let Some(service) = self.services.get_mut(&paired.id) {
                    service.paired = *paired;
                }
                if let Some(bundle) = approved {
                    self.prepare(*bundle);
                }
                self.replies.push(reply);
            }
            Command::SetPaused(paused) => {
                if paused != self.paused {
                    self.paused = paused;
                    tracing::info!("{}", if paused { "paused" } else { "resumed" });
                    if let Err(e) = self.machine.update_settings(|s| s.paused = paused) {
                        tracing::warn!("couldn't remember that the machine is paused: {e}");
                    }
                }
            }
            Command::StopJob => {
                if let Some(running) = &mut self.running {
                    tracing::info!("stopping {}", running.key);
                    running.stop(StopReason::Interrupted);
                }
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
            LinkEvent::Lost { error, retry_at } => {
                tracing::info!("lost {}: {error}", service.paired.name);
                service.link = None;
                service.connection = Connection::Disconnected { error, retry_at };
            }
            LinkEvent::Unpaired => {
                tracing::warn!(
                    "{} removed this machine; pair it again",
                    service.paired.name
                );
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
    fn hello(&mut self, id: &ServiceId, outbox: mpsc::UnboundedSender<String>) {
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
        let link = Link {
            outbox,
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
                (Some(resume), Some(attempt)) => attempt.delivered = resume.seq,
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
        self.send(id, &answer);
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
        let mut attempt = Attempt::new(Duration::from_millis(assign.lease_ms));
        let bundle = match self.availability(id, &assign.bundle_hash) {
            Ok(bundle) => bundle,
            Err(unavailable) => {
                let error = match unavailable {
                    Unavailable::Busy => {
                        JobError::new(ErrorCode::Busy, "the machine is running another job")
                    }
                    Unavailable::Paused => JobError::new(ErrorCode::Busy, "the machine is paused"),
                    Unavailable::NotApproved(why) => JobError::new(ErrorCode::NotApproved, why),
                };
                tracing::info!("refused {key}: {}", error.message);
                attempt.push_done(&key, Outcome::failed(error));
                self.attempts.insert(key, attempt);
                return;
            }
        };
        tracing::info!("starting {key}: {}", bundle.manifest().name);
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
        self.attempts.insert(key.clone(), attempt);
        self.running = Some(Running {
            key: key.clone(),
            stopper,
            status,
        });
        self.spawn_attempt(key, Job::from_assign(assign, bundle), stop);
    }

    /// Whether the machine takes a job for `hash` from `id` now (spec
    /// §7.1): idle, approved, and holding a verified copy.
    fn availability(&self, id: &ServiceId, hash: &BundleHash) -> Result<Bundle, Unavailable> {
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
        match self.machine.approved_bundle(id, *hash) {
            Ok(Some(bundle)) => Ok(bundle),
            Ok(None) => Err(Unavailable::NotApproved(format!(
                "the machine has no approved copy of bundle {hash}"
            ))),
            Err(e) => {
                tracing::error!("the approved copy of bundle {hash} failed verification: {e}");
                Err(Unavailable::NotApproved(format!(
                    "the machine's copy of bundle {hash} failed verification"
                )))
            }
        }
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
                        tracing::info!("{key}: {message}");
                    }
                }
                JobEvent::Log { .. } | JobEvent::Data { .. } => {}
            }
        }
        // Abandoned attempts are gone, and so are their late events.
        if let Some(attempt) = self.attempts.get_mut(key) {
            attempt.push_event(key, event, self.options.max_kept_bytes);
        }
    }

    fn finished(&mut self, key: AttemptKey, outcome: Outcome) {
        if self.running.as_ref().is_some_and(|r| r.key == key) {
            self.running = None;
            if let Some(shutdown) = &mut self.shutdown {
                shutdown.deliver_by = Some(Instant::now() + self.options.shutdown_grace);
            }
        }
        if let Some(attempt) = self.attempts.get_mut(&key) {
            match &outcome {
                Outcome::Succeeded { .. } => tracing::info!("{key} succeeded"),
                Outcome::Failed { error } => tracing::info!("{key} failed: {error}"),
                Outcome::Cancelled => tracing::info!("{key} was cancelled"),
            }
            attempt.push_done(&key, outcome);
        }
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
        let running = self.running.as_ref().map(|r| &r.key);
        for (key, attempt) in &mut self.attempts {
            let Some(link) = self
                .services
                .get(&key.service)
                .and_then(|s| s.link.as_ref())
                .filter(|l| l.welcomed)
            else {
                continue;
            };
            let mut sent = false;
            for (seq, text) in &attempt.kept {
                if *seq > attempt.delivered {
                    let _ = link.outbox.send(text.clone());
                    sent = true;
                }
            }
            if sent {
                attempt.delivered = attempt.kept.back().map_or(0, |(seq, _)| *seq);
                attempt.last_sent = now;
            }
            if running == Some(key) && now >= attempt.renew_at() {
                link.send(&MachineMessage::Renew(key.attempt.clone()));
                attempt.last_sent = now;
            }
        }

        let state = self.state();
        for service in self.services.values_mut() {
            let Some(link) = service.link.as_mut().filter(|l| l.welcomed) else {
                continue;
            };
            if link.state != state {
                link.send(&MachineMessage::State { state });
                link.state = state;
            }
            let approved = service.paired.approved_hashes();
            if link.approved != approved {
                link.send(&MachineMessage::Approved {
                    hashes: approved.clone(),
                });
                link.approved = approved;
            }
        }
    }

    /// When the running attempt next needs a `renew`, if its service is
    /// connected.
    fn renew_at(&self) -> Option<Instant> {
        let key = &self.running.as_ref()?.key;
        let link = self.services.get(&key.service)?.link.as_ref()?;
        link.welcomed
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
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, futures::future::join_all(closing)).await;
    }

    fn connect(&mut self, paired: PairedService) {
        let task = self.next_task;
        self.next_task += 1;
        let stop = CancellationToken::new();
        let join = connection::spawn(
            self.machine.clone(),
            paired.id.clone(),
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

    fn send(&self, id: &ServiceId, message: &MachineMessage) {
        if let Some(link) = self.services.get(id).and_then(|s| s.link.as_ref()) {
            link.send(message);
        }
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
        };
        self.snapshot.send_if_modified(|current| {
            let changed = *current != snapshot;
            if changed {
                *current = snapshot;
            }
            changed
        });
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}
