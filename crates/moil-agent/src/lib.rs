//! The machine side of moil.
//!
//! - [`Machine`] is this computer's moil state: settings, paired services,
//!   approved bundles and tokens. The command line uses it directly.
//! - [`Agent`] runs on top of a [`Machine`]: it keeps a control channel
//!   open to every paired service, bids on offers, runs assigned attempts
//!   through an [`Executor`](moil_runtime::Executor) and reports back. The
//!   tray app and `moil agent` run one, and watch it through a [`Snapshot`].
//!
//! Only one process at a time may open a machine's [`Home`]; the second
//! gets [`Error::AlreadyRunning`].

mod agent;
mod error;
mod fs;
mod hardware;
pub mod home;
pub mod machine;
mod net;
pub mod pairing;
pub mod review;
pub mod secrets;
pub mod services;

pub use agent::{
    Agent, AgentHandle, AgentOptions, BundleStatus, Connection, JobStatus, ServiceStatus, Snapshot,
};
pub use error::Error;
pub use home::{Home, Settings};
pub use machine::{Machine, MachineConfig};
pub use pairing::PairingSession;
pub use review::Review;
#[cfg(target_os = "macos")]
pub use secrets::KeychainSecrets;
pub use secrets::{FileSecrets, MemorySecrets, SecretStore, default_secrets};
pub use services::{Approval, InvalidServiceId, PairedService, ServiceId};
