//! The control socket (spec/machine.md §4): how commands reach the agent
//! running on a home.
//!
//! The process running the agent, the app or `moil agent`, [`serve`]s it
//! while it holds the home's lock, and commands reach the agent through a
//! [`Client`], which tells them when no agent runs. Both ends are always
//! the same moil, so the messages are moil's own and aren't specified:
//! lines of JSON, after one exchange of hellos that carries both sides'
//! versions. A client that reaches an agent of another version fails,
//! pointing the owner to the agent's own `moil`.
//!
//! Only the owner's user can connect: the socket is in the home's private
//! `.machine/`, only its owner may use it, and each end checks the user ID
//! of the other.

mod client;
mod messages;
mod server;
mod socket;

pub use client::{Client, ClientError, Pairing, Watch};
pub use messages::{PairingCode, ServiceBundles};
pub use server::{HostControls, ServerHandle, serve};

/// This moil's version, which both ends must share.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
