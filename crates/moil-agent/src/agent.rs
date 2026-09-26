//! The agent: control channels, offers, attempts.

use std::sync::Arc;
use std::time::Instant;

use moil_proto::control::BundleInfo;
use moil_proto::{BundleHash, JobId, Phase};
use moil_runtime::Executor;
use tokio::sync::watch;
use url::Url;

use crate::{Error, Machine, PairingSession, Review, ServiceId};

/// Starts agents.
#[derive(Debug)]
pub struct Agent;

impl Agent {
    /// Connects to every paired service and starts taking jobs. Must be
    /// called from within a Tokio runtime.
    pub fn start(_machine: Machine, _executor: Arc<dyn Executor>) -> AgentHandle {
        todo!("moil-agent: Agent::start")
    }
}

/// Controls a running agent. Cheap to clone.
#[derive(Debug, Clone)]
pub struct AgentHandle {
    #[allow(dead_code)]
    snapshot: watch::Receiver<Snapshot>,
}

impl AgentHandle {
    pub fn machine(&self) -> &Machine {
        todo!("moil-agent: AgentHandle::machine")
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
    /// connects to the new service.
    pub async fn pair(&self, _base_url: &str) -> Result<PairingSession, Error> {
        todo!("moil-agent: AgentHandle::pair")
    }

    /// Disconnects from a service and forgets it.
    pub async fn unpair(&self, _id: &ServiceId) -> Result<(), Error> {
        todo!("moil-agent: AgentHandle::unpair")
    }

    pub async fn review(&self, _id: &ServiceId, _hash: BundleHash) -> Result<Review, Error> {
        todo!("moil-agent: AgentHandle::review")
    }

    /// Approves the reviewed bundle, tells the service, and prepares the
    /// bundle in the background.
    pub async fn approve(&self, _review: &Review) -> Result<(), Error> {
        todo!("moil-agent: AgentHandle::approve")
    }

    /// Revokes an approval and tells the service.
    pub async fn revoke(&self, _id: &ServiceId, _hash: BundleHash) -> Result<(), Error> {
        todo!("moil-agent: AgentHandle::revoke")
    }

    /// Stops or resumes taking new jobs. A running job carries on.
    pub fn set_paused(&self, _paused: bool) {
        todo!("moil-agent: AgentHandle::set_paused")
    }

    /// Stops the running job, if any. The service may give it to another
    /// machine.
    pub fn stop_job(&self) {
        todo!("moil-agent: AgentHandle::stop_job")
    }

    /// Stops the running job, reports it, and closes every connection.
    pub async fn shutdown(&self) {
        todo!("moil-agent: AgentHandle::shutdown")
    }
}

/// What the agent is doing, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub paused: bool,
    pub services: Vec<ServiceStatus>,
    /// The running attempt, if any.
    pub job: Option<JobStatus>,
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
}
