//! Pairing with a service (spec §4).

use std::time::Instant;

use crate::{Error, PairedService};

/// A pairing in progress: show [`PairingSession::user_code`] to the owner,
/// open [`PairingSession::verification_uri_complete`] in their browser,
/// then [`wait`](PairingSession::wait).
#[derive(Debug)]
pub struct PairingSession {
    /// The code the owner should see on the service's confirmation page.
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    /// The name the service gives itself.
    pub service_name: String,
    pub expires_at: Instant,
}

impl PairingSession {
    /// Polls the service until the owner confirms or rejects the pairing, or
    /// the code expires. On confirmation, stores the service and its token.
    /// Dropping the future abandons the pairing.
    pub async fn wait(self) -> Result<PairedService, Error> {
        todo!("moil-agent: PairingSession::wait")
    }
}
