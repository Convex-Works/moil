//! What goes through the socket. Both ends are always the same moil, so
//! none of it is specified or kept stable across versions, except the
//! hellos' `version`: it is how two versions tell that they differ.

use std::collections::BTreeMap;
use std::time::SystemTime;

use moil_bundle::Bundle;
use moil_proto::BundleHash;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::report::{AgentInfo, ListedBundle, ServiceRef};
use crate::status::Clock;
use crate::{Error, Machine, PairingSession, Service};

/// The first line a client sends.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ClientHello {
    pub version: String,
}

/// The agent's answer to [`ClientHello`], whatever the client's version.
/// The agent closes the connection after it unless the versions match.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ServerHello {
    pub version: String,
    pub agent: AgentInfo,
}

/// What a command asks of the agent. Services are named as the owner
/// names them: by ID, host, base URL or name.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub(super) enum Request {
    /// Answered with a [`Status`](crate::report::Status).
    Status,
    /// Answered with a `Status` now and whenever it changes, until the
    /// client leaves or the agent stops.
    Watch,
    /// Answered with whether the machine is paused now.
    Pause,
    Resume,
    /// Answered with the job it stopped, if one ran, as a
    /// [`RunningJob`](crate::report::RunningJob).
    Stop,
    /// Answered with a [`PairingCode`], then with the paired service's
    /// [`ServiceRef`], or why pairing failed. The client leaving abandons
    /// the pairing.
    Pair {
        /// A base URL.
        service: String,
        /// What services should call this machine from now on.
        name: Option<String>,
    },
    /// Answered with the unpaired service's `ServiceRef`.
    Unpair {
        service: String,
    },
    /// Answered with [`ServiceBundles`].
    Bundles {
        service: String,
    },
    /// Answered with [`ReviewFiles`].
    Review {
        service: String,
        hash: BundleHash,
    },
    /// The agent fetches the bundle from the service itself, and approves
    /// it only if it hashes to `hash`.
    Approve {
        service: String,
        hash: BundleHash,
    },
    Revoke {
        service: String,
        hash: BundleHash,
    },
    /// Answered once the host has been asked to quit. The agent closes the
    /// connection once it has stopped.
    Quit,
    /// Answered with whether the app opens at login, once it has been
    /// turned on or off as `set` says.
    LoginItem {
        set: Option<bool>,
    },
}

/// Every answer: the result, or the agent's sentence for why there is
/// none.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Reply<T> {
    Ok(T),
    Error(String),
}

impl<T> From<Result<T, String>> for Reply<T> {
    fn from(result: Result<T, String>) -> Self {
        match result {
            Ok(value) => Self::Ok(value),
            Err(error) => Self::Error(error),
        }
    }
}

/// What a pairing brings, in order.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PairStep {
    Code(PairingCode),
    Paired(ServiceRef),
}

/// A pairing waiting for the owner to confirm its code on the service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCode {
    /// The code the owner should see on the service's confirmation page.
    pub user_code: String,
    pub verification_uri: String,
    /// The confirmation page with the code filled in.
    pub verification_uri_complete: String,
    /// The name the service gives itself.
    pub service_name: String,
    /// The service's base URL, as it will be stored.
    pub base_url: Url,
    /// What the service will call this machine.
    pub machine_name: String,
    #[serde(with = "crate::rfc3339")]
    pub expires_at: SystemTime,
}

impl PairingCode {
    /// Starts pairing as `moil pair` does, whether or not an agent runs:
    /// calls `machine` `name` from now on, for every service, if given
    /// (spec/machine.md §1.1), then starts the pairing with `pair`. Returns
    /// it, with what the owner needs to confirm it.
    pub async fn start(
        machine: &Machine,
        name: Option<String>,
        pair: impl AsyncFnOnce() -> Result<PairingSession, Error>,
    ) -> Result<(PairingSession, Self), Error> {
        if let Some(name) = name {
            machine
                .blocking(|machine| machine.update_settings(|s| s.machine_name = Some(name)))
                .await?;
        }
        let session = pair().await?;
        // The computer's name may take detecting.
        let machine_name = machine.blocking(|machine| Ok(machine.info().name)).await?;
        let code = Self {
            user_code: session.user_code.clone(),
            verification_uri: session.verification_uri.clone(),
            verification_uri_complete: session.verification_uri_complete.clone(),
            service_name: session.service_name.clone(),
            base_url: session.base_url().clone(),
            machine_name,
            expires_at: Clock::now().wall(session.expires_at),
        };
        Ok((session, code))
    }
}

/// The bundles a service offers, and those approved for it that it no
/// longer offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceBundles {
    pub service: ServiceRef,
    pub bundles: Vec<ListedBundle>,
}

/// A [`Review`](crate::Review) as files, for the client to verify against
/// the hash it asked for and rebuild.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ReviewFiles {
    pub service: Service,
    pub approved: bool,
    pub files: BTreeMap<String, String>,
    pub previous: Option<BTreeMap<String, String>>,
}

impl ReviewFiles {
    pub fn files(bundle: &Bundle) -> BTreeMap<String, String> {
        bundle
            .files()
            .map(|(name, text)| (name.to_owned(), text.to_owned()))
            .collect()
    }
}
