//! Pairing with a service (spec §4).

use std::fmt;
use std::time::{Duration, Instant};

use moil_proto::http::{PairResponse, TokenRequest, TokenResponse, token_error};
use url::Url;

use crate::{Error, Machine, PairedService, net};

/// Called with the service once a pairing completes.
pub(crate) type OnPaired = Box<dyn FnOnce(&PairedService) + Send + Sync>;

/// A pairing in progress: show [`PairingSession::user_code`] to the owner,
/// open [`PairingSession::verification_uri_complete`] in their browser,
/// then [`wait`](PairingSession::wait).
pub struct PairingSession {
    /// The code the owner should see on the service's confirmation page.
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    /// The name the service gives itself.
    pub service_name: String,
    pub expires_at: Instant,
    machine: Machine,
    base_url: Url,
    device_code: String,
    interval: Duration,
    slow_down: Duration,
    on_paired: Option<OnPaired>,
}

/// One answer to a token poll that doesn't end the pairing.
enum Poll {
    Granted(TokenResponse),
    Pending,
    SlowDown,
}

impl PairingSession {
    /// RFC 8628 §3.5: each `slow_down` lengthens the polling interval by 5
    /// seconds.
    pub const SLOW_DOWN: Duration = Duration::from_secs(5);

    pub(crate) fn new(
        machine: Machine,
        base_url: Url,
        pairing: PairResponse,
        started: Instant,
    ) -> Self {
        Self {
            user_code: pairing.user_code,
            verification_uri: pairing.verification_uri,
            verification_uri_complete: pairing.verification_uri_complete,
            service_name: pairing.service.name,
            expires_at: started + Duration::from_secs(pairing.expires_in),
            machine,
            base_url,
            device_code: pairing.device_code,
            interval: Duration::from_secs(pairing.interval),
            slow_down: Self::SLOW_DOWN,
            on_paired: None,
        }
    }

    /// The service's base URL, as it will be stored.
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Lengthens the polling interval by `step` instead of
    /// [`SLOW_DOWN`](Self::SLOW_DOWN) when the service asks. Only fake
    /// services in tests want this.
    pub fn with_slow_down(mut self, step: Duration) -> Self {
        self.slow_down = step;
        self
    }

    /// Runs `hook` once the pairing completes.
    #[allow(dead_code, reason = "the agent will use it")]
    pub(crate) fn on_paired(mut self, hook: OnPaired) -> Self {
        self.on_paired = Some(hook);
        self
    }

    /// Polls the service until the owner confirms or rejects the pairing, or
    /// the code expires. On confirmation, stores the service and its token.
    /// Dropping the future abandons the pairing.
    pub async fn wait(mut self) -> Result<PairedService, Error> {
        loop {
            tokio::time::sleep(self.interval).await;
            if Instant::now() >= self.expires_at {
                return Err(Error::PairingExpired);
            }
            match self.poll().await? {
                Poll::Granted(token) => return self.complete(token).await,
                Poll::Pending => {}
                Poll::SlowDown => self.interval += self.slow_down,
            }
        }
    }

    /// Asks once for the token. Network trouble and server errors count as
    /// "not yet": the owner may still confirm before the code expires.
    async fn poll(&self) -> Result<Poll, Error> {
        let url = net::endpoint(&self.base_url, "v1/pair/token");
        let request = TokenRequest {
            device_code: self.device_code.clone(),
        };
        let response = match self
            .machine
            .net()
            .http
            .post(&url)
            .json(&request)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                tracing::warn!("couldn't reach {}: {}", self.base_url, net::describe(&e));
                return Ok(Poll::Pending);
            }
        };
        let status = response.status();
        if status.is_success() {
            return net::read_json(&self.service_name, response)
                .await
                .map(Poll::Granted);
        }
        if status.is_server_error() {
            tracing::warn!("{} answered {status} while pairing", self.service_name);
            return Ok(Poll::Pending);
        }
        let body = net::error_body(response).await;
        match body.as_ref().map(|b| b.error.as_str()) {
            Some(token_error::AUTHORIZATION_PENDING) => Ok(Poll::Pending),
            Some(token_error::SLOW_DOWN) => Ok(Poll::SlowDown),
            Some(token_error::ACCESS_DENIED) => Err(Error::PairingDenied),
            Some(token_error::EXPIRED_TOKEN) => Err(Error::PairingExpired),
            _ => Err(Error::Http(format!(
                "{} answered {status} while pairing{}",
                self.service_name,
                body.map(|b| format!(": {}", b.message.unwrap_or(b.error)))
                    .unwrap_or_default()
            ))),
        }
    }

    async fn complete(self, granted: TokenResponse) -> Result<PairedService, Error> {
        if !moil_proto::ids::is_machine_id(&granted.machine_id) {
            return Err(Error::Http(format!(
                "{} gave this machine an invalid ID ({:?})",
                self.service_name, granted.machine_id
            )));
        }
        let (service, replaced) = self.machine.record_pairing(
            &self.base_url,
            granted.service.name,
            granted.machine_id,
            &granted.token,
        )?;
        // Pairing again made the service a new machine record; the old one
        // would otherwise linger there, offline forever.
        if let Some(old_token) = replaced {
            self.machine
                .forget_token_on(&self.base_url, &old_token)
                .await;
        }
        if let Some(hook) = self.on_paired {
            hook(&service);
        }
        Ok(service)
    }
}

impl fmt::Debug for PairingSession {
    /// Leaves out the device code: whoever has it can collect the token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingSession")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("verification_uri_complete", &self.verification_uri_complete)
            .field("service_name", &self.service_name)
            .field("expires_at", &self.expires_at)
            .field("base_url", &self.base_url.as_str())
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}
