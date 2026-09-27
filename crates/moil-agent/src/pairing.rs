//! Pairing with a service (spec §4).

use std::fmt;
use std::time::{Duration, Instant};

use moil_proto::http::{PairResponse, TokenRequest, TokenResponse, token_error};
use url::Url;

use crate::{Error, Machine, PairedService, net};

/// Called with the service once a pairing completes.
pub(crate) type OnPaired = Box<dyn FnOnce(&PairedService) + Send + Sync>;

/// Fails unless `page`, a confirmation page from the service at
/// `base_url`, is a web page on the service's own origin, with no user
/// name or password to disguise it (spec §4). A service could otherwise
/// relay another service's pairing, and the owner, confirming on that
/// service's page, would pair the relay's machine with their account there.
pub(crate) fn check_confirmation_page(base_url: &Url, page: &str) -> Result<(), Error> {
    let refuse = |reason: &str| {
        Err(Error::NotAService {
            url: base_url.to_string(),
            reason: format!("its confirmation page {page:?} {reason}"),
        })
    };
    let url = match Url::parse(page) {
        Ok(url) if matches!(url.scheme(), "https" | "http") => url,
        _ => return refuse("isn't a web address"),
    };
    if url.origin() != base_url.origin() {
        return Err(Error::ForeignPairingPage {
            url: base_url.to_string(),
            site: url.origin().ascii_serialization(),
        });
    }
    if !url.username().is_empty() || url.password().is_some() {
        return refuse("has a user name or password in it");
    }
    Ok(())
}

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
    pub(crate) fn on_paired(mut self, hook: OnPaired) -> Self {
        self.on_paired = Some(hook);
        self
    }

    /// Polls the service until the owner confirms or rejects the pairing, or
    /// the code expires. On confirmation, stores the service and its token.
    ///
    /// Dropping the future abandons the pairing until the service has sent
    /// the token. From then on the pairing completes (the service and token
    /// are stored, and an agent pairing through
    /// [`AgentHandle::pair`](crate::AgentHandle::pair) connects) whether
    /// the future is polled to the end or not.
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
        if let Err(reason) = crate::services::check_name(&granted.service.name) {
            let url = self.base_url.to_string();
            return Err(Error::NotAService { url, reason });
        }
        let (base_url, on_paired) = (self.base_url, self.on_paired);
        // One task that runs to the end, whoever waits for it.
        self.machine
            .blocking(move |machine| {
                let (service, replaced) = machine.record_pairing(
                    &base_url,
                    granted.service.name,
                    granted.machine_id,
                    &granted.token,
                )?;
                if let Some(hook) = on_paired {
                    hook(&service);
                }
                // Pairing again made the service a new machine record; the
                // old one would otherwise linger there, offline forever.
                // Nothing waits for the service to answer.
                if let Some(old_token) = replaced {
                    let machine = machine.clone();
                    tokio::spawn(
                        async move { machine.forget_token_on(&base_url, &old_token).await },
                    );
                }
                Ok(service)
            })
            .await
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

#[cfg(test)]
mod tests {
    use super::*;

    fn check(base: &str, page: &str) -> Result<(), Error> {
        check_confirmation_page(&Url::parse(base).unwrap(), page)
    }

    #[test]
    fn the_confirmation_page_may_be_anywhere_on_the_services_origin() {
        for (base, page) in [
            (
                "https://klisi.example.com/moil",
                "https://klisi.example.com/machines/pair?code=WDJB-MJHT",
            ),
            (
                "https://klisi.example.com/moil",
                "https://KLISI.example.com:443/machines",
            ),
            ("https://klisi.example.com/", "https://klisi.example.com"),
            ("http://127.0.0.1:8787/moil", "http://127.0.0.1:8787/pair"),
            ("http://[::1]:8787/moil", "http://[0:0::1]:8787/pair"),
        ] {
            assert!(check(base, page).is_ok(), "{page} for {base}");
        }
    }

    #[test]
    fn a_page_on_another_origin_is_refused() {
        let klisi = "https://klisi.example.com/moil";
        let local = "http://127.0.0.1:8787/moil";
        for (base, page, site) in [
            // A relay, passing on the page of the service it relays to.
            (
                "https://evil.example/moil",
                "https://klisi.example.com/machines/pair?code=WDJB-MJHT",
                "https://klisi.example.com",
            ),
            // Other hosts, however related.
            (
                klisi,
                "https://machines.klisi.example.com/pair",
                "https://machines.klisi.example.com",
            ),
            (klisi, "https://example.com/pair", "https://example.com"),
            (
                "http://localhost:8787/moil",
                "http://127.0.0.1:8787/pair",
                "http://127.0.0.1:8787",
            ),
            // Other ports.
            (
                klisi,
                "https://klisi.example.com:8443/pair",
                "https://klisi.example.com:8443",
            ),
            (local, "http://127.0.0.1:8788/pair", "http://127.0.0.1:8788"),
            // Other schemes.
            (
                klisi,
                "http://klisi.example.com/pair",
                "http://klisi.example.com",
            ),
            (
                local,
                "https://127.0.0.1:8787/pair",
                "https://127.0.0.1:8787",
            ),
            // A user name that reads like the service's host.
            (
                klisi,
                "https://klisi.example.com@evil.example/pair",
                "https://evil.example",
            ),
            (
                klisi,
                "https://klisi.example.com:443@evil.example/pair",
                "https://evil.example",
            ),
        ] {
            match check(base, page) {
                Err(Error::ForeignPairingPage { site: refused, .. }) => {
                    assert_eq!(refused, site, "{page} for {base}")
                }
                other => panic!("{page} for {base}: {other:?}"),
            }
        }
    }

    #[test]
    fn the_owner_learns_where_they_were_sent() {
        let err = check(
            "https://evil.example/moil",
            "https://klisi.example.com/machines/pair?code=WDJB-MJHT",
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "https://evil.example/moil wants you to confirm on https://klisi.example.com, \
             another site, which is how a service would get you to pair its own machine with \
             your account there; to pair with https://klisi.example.com, use the address it \
             shows for moil"
        );
    }

    #[test]
    fn a_disguised_page_or_one_that_isnt_on_the_web_is_refused() {
        for (page, why) in [
            (
                "https://klisi.example.com@evil.example/pair",
                "has a user name or password in it",
            ),
            (
                "https://owner:secret@evil.example/pair",
                "has a user name or password in it",
            ),
            ("file:///Applications/Calculator.app", "isn't a web address"),
            ("javascript:alert(1)", "isn't a web address"),
            ("/machines/pair", "isn't a web address"),
            ("", "isn't a web address"),
        ] {
            match check("https://evil.example/moil", page) {
                Err(Error::NotAService { reason, .. }) => {
                    assert!(reason.contains(why), "{page:?}: {reason}")
                }
                other => panic!("{page:?}: {other:?}"),
            }
        }
    }
}
