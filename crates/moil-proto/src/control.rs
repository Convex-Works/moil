//! Messages on the control channel (spec §6).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::JobError;
use crate::event::JobEvent;
use crate::ids::{BundleHash, FileName, JobId};
use crate::job::{Download, Upload};

/// A message from a machine to a service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MachineMessage {
    Hello(Hello),
    State {
        state: MachineState,
    },
    Approved {
        hashes: Vec<BundleHash>,
    },
    Bid {
        job_id: JobId,
    },
    Decline {
        job_id: JobId,
        reason: DeclineReason,
    },
    Event(EventMessage),
    Renew(AttemptRef),
    Done(Done),
    /// A message type from a newer machine. Receivers ignore it.
    #[serde(other)]
    Unknown,
}

/// A message from a service to a machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServiceMessage {
    Welcome(Welcome),
    Bundles {
        bundles: Vec<BundleInfo>,
    },
    Offer(Offer),
    Assign(Assign),
    Ack(AttemptRef),
    Cancel(AttemptRef),
    /// A message type from a newer service. Receivers ignore it.
    #[serde(other)]
    Unknown,
}

/// The machine's first message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub app_version: String,
    pub machine: MachineInfo,
    pub state: MachineState,
    /// Every bundle hash the owner approved for this service.
    pub approved: Vec<BundleHash>,
    /// Attempts for this service the machine still holds (spec §7.5).
    pub attempts: Vec<AttemptRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineInfo {
    pub name: String,
    /// `std::env::consts::OS`: `macos`, `linux`, `windows`.
    pub os: String,
    /// `std::env::consts::ARCH`: `aarch64`, `x86_64`.
    pub arch: String,
    pub cpus: u32,
    pub memory_bytes: u64,
    pub gpus: Vec<Gpu>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gpu {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineState {
    /// Can take a job.
    Idle,
    /// Running a job, for this service or another.
    Busy,
    /// The owner paused the machine.
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclineReason {
    Busy,
    Paused,
    NotApproved,
}

/// Names one attempt of one job.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AttemptRef {
    pub job_id: JobId,
    pub attempt: u32,
}

/// One forwarded job event, with its sequence number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventMessage {
    pub job_id: JobId,
    pub attempt: u32,
    pub seq: u64,
    #[serde(with = "crate::event::versioned")]
    pub event: JobEvent,
}

/// The end of an attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Done {
    pub job_id: JobId,
    pub attempt: u32,
    pub seq: u64,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Succeeded {
        result: JobResult,
    },
    Failed {
        error: JobError,
    },
    /// The service asked for the attempt to stop.
    Cancelled,
}

impl Outcome {
    pub fn failed(error: JobError) -> Self {
        Self::Failed { error }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    /// Every uploaded output.
    pub files: BTreeMap<FileName, FileInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInfo {
    pub size_bytes: u64,
    pub sha256: String,
}

/// The service's answer to `hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol: u32,
    pub machine_id: String,
    pub service: ServiceInfo,
    pub bundles: Vec<BundleInfo>,
    pub resume: Vec<Resume>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub name: String,
}

/// A bundle the service may send jobs for, as the service describes it.
/// Never trust these fields for approval: show what you fetch and verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleInfo {
    pub hash: BundleHash,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// An attempt the service still considers assigned to the machine, and the
/// last `seq` it processed for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    pub job_id: JobId,
    pub attempt: u32,
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub job_id: JobId,
    pub bundle_hash: BundleHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub expires_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assign {
    pub job_id: JobId,
    pub attempt: u32,
    pub bundle_hash: BundleHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub params: Value,
    #[serde(default)]
    pub inputs: BTreeMap<FileName, Download>,
    #[serde(default)]
    pub outputs: BTreeMap<FileName, Upload>,
    pub lease_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl Assign {
    pub fn attempt_ref(&self) -> AttemptRef {
        AttemptRef {
            job_id: self.job_id.clone(),
            attempt: self.attempt,
        }
    }
}
