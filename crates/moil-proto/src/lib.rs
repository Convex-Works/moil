//! Wire types for the moil protocol.
//!
//! Everything a machine and a service say to each other, and everything the
//! runtime and a job script say to each other, is defined here. The types
//! mirror [`spec/protocol.md`](https://git.convex.works/ConvexWorks/moil/src/branch/main/spec/protocol.md)
//! and are checked against the shared vectors in `spec/vectors`.

pub mod control;
pub mod error;
pub mod event;
pub mod http;
pub mod ids;
pub mod job;

pub use control::{
    Assign, AttemptRef, BundleInfo, DeclineReason, Done, EventMessage, FileInfo, Gpu, Hello,
    JobResult, MachineInfo, MachineMessage, MachineState, Offer, Outcome, Resume, ServiceInfo,
    ServiceMessage, Welcome,
};
pub use error::{ErrorCode, JobError};
pub use event::{JobEvent, LogLevel, Phase, ScriptEvent};
pub use ids::{BundleHash, FileName, InvalidId, JobId};
pub use job::{Download, JobContext, Upload, UploadMethod};

/// The protocol version this crate speaks: the `v1` in every path and the
/// `protocol` field of `hello` and `welcome`.
pub const PROTOCOL_VERSION: u32 = 1;

/// The version of the script contract: the `v` field of events and the job
/// context.
pub const SCRIPT_CONTRACT_VERSION: u32 = 1;

/// Close codes for the control channel (spec §6.4).
pub mod close_code {
    /// The peer sent something it shouldn't have.
    pub const PROTOCOL_ERROR: u16 = 4400;
    /// The machine was removed or its token revoked. Don't reconnect.
    pub const UNAUTHORIZED: u16 = 4401;
    /// A newer connection with the same token replaced this one. Don't
    /// reconnect.
    pub const REPLACED: u16 = 4409;
}
