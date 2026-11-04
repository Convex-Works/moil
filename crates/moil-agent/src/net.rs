//! Reaching services: which URLs the machine may use, and one TLS setup
//! shared by HTTP requests and control channels.

use std::sync::Arc;
use std::time::Duration;

use moil_proto::http::ErrorBody;
use moil_proto::{UrlPolicy, UrlRefused};
use rustls_platform_verifier::BuilderVerifierExt;
use serde::de::DeserializeOwned;
use url::Url;

use crate::Error;

/// The most the machine reads of one HTTP response: a bundle's 8 MiB, with
/// room for JSON escaping.
const MAX_RESPONSE_BYTES: usize = 24 << 20;

/// HTTP and TLS clients, built once per [`Machine`](crate::Machine).
#[derive(Debug)]
pub(crate) struct Net {
    pub http: reqwest::Client,
    /// Verifies servers with the operating system's trust store, so
    /// certificates the owner trusts (e.g. a company CA) work.
    pub tls: Arc<rustls::ClientConfig>,
}

impl Net {
    pub fn new(app_version: &str) -> Result<Self, Error> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .and_then(|builder| builder.with_platform_verifier())
            .map(|builder| builder.with_no_client_auth())
            .map_err(|e| Error::Http(format!("couldn't set up TLS: {e}")))?;
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls.clone())
            .user_agent(format!("moil/{app_version}"))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            // A redirect could lead anywhere, including to plain http.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::Http(format!("couldn't set up HTTP: {}", describe(&e))))?;
        Ok(Self {
            http,
            tls: Arc::new(tls),
        })
    }
}

/// Fails unless `url` may be a service's base URL (spec §3): `https`, or
/// `http` to a loopback host, or `http` anywhere when the owner allowed it.
pub(crate) fn check_url(url: &Url, allow_insecure_http: bool) -> Result<(), Error> {
    match UrlPolicy::for_base_urls(allow_insecure_http).check(url) {
        Ok(()) => Ok(()),
        Err(UrlRefused::Http(_) | UrlRefused::LoopbackHttp(_)) => {
            Err(Error::InsecureUrl(url.to_string()))
        }
        Err(refused) => Err(Error::NotAService {
            url: url.to_string(),
            reason: format!("moil services are reached over https, and this URL {refused}"),
        }),
    }
}

/// The URL of an endpoint, `path` being relative to the base URL
/// (`v1/info`).
pub(crate) fn endpoint(base: &Url, path: &str) -> String {
    format!("{}/{path}", base.as_str().trim_end_matches('/'))
}

/// Decodes a successful JSON response, or turns a failed one into an error
/// naming `who` answered.
pub(crate) async fn read_json<T: DeserializeOwned>(
    who: &str,
    response: reqwest::Response,
) -> Result<T, Error> {
    if !response.status().is_success() {
        return Err(failure(who, response).await);
    }
    let body = read_body(who, response).await?;
    serde_json::from_slice(&body)
        .map_err(|e| Error::Http(format!("{who} sent an answer moil doesn't understand: {e}")))
}

/// The error a failed response stands for.
pub(crate) async fn failure(who: &str, response: reqwest::Response) -> Error {
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Error::Unauthorized;
    }
    let detail = error_body(response)
        .await
        .map(|body| format!(": {}", body.message.unwrap_or(body.error)))
        .unwrap_or_default();
    Error::Http(format!("{who} answered {status}{detail}"))
}

/// The `{"error": …}` body of a failed response, if it has one.
pub(crate) async fn error_body(response: reqwest::Response) -> Option<ErrorBody> {
    let body = read_body("", response).await.ok()?;
    serde_json::from_slice(&body).ok()
}

async fn read_body(who: &str, mut response: reqwest::Response) -> Result<Vec<u8>, Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Error::Http(format!("{who} didn't finish answering: {}", describe(&e))))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(Error::Http(format!("{who} sent more than moil accepts")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The error for a request that got no answer.
pub(crate) fn unreachable(url: &str, error: &reqwest::Error) -> Error {
    Error::Http(format!("couldn't reach {url}: {}", describe(error)))
}

/// An error with its causes, which is where the useful part usually is
/// ("connection refused", "certificate expired").
pub(crate) fn describe(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_is_only_for_loopback_unless_allowed() {
        let check = |s: &str, allow| check_url(&Url::parse(s).unwrap(), allow);
        for ok in [
            "https://tide.example.com",
            "http://localhost:8787",
            "http://LOCALHOST",
            "http://127.0.0.1:1",
            "http://127.9.9.9",
            "http://[::1]:80",
        ] {
            assert!(check(ok, false).is_ok(), "{ok}");
        }
        for insecure in [
            "http://tide.example.com",
            "http://10.0.0.2",
            "http://localhost.evil.com",
        ] {
            assert!(
                matches!(check(insecure, false), Err(Error::InsecureUrl(_))),
                "{insecure}"
            );
            assert!(check(insecure, true).is_ok(), "{insecure} when allowed");
        }
    }

    #[test]
    fn endpoints_are_relative_to_the_base_url() {
        let base = crate::services::parse_base_url("https://a.example/moil").unwrap();
        assert_eq!(endpoint(&base, "v1/info"), "https://a.example/moil/v1/info");
        let root = crate::services::parse_base_url("https://a.example").unwrap();
        assert_eq!(endpoint(&root, "v1/info"), "https://a.example/v1/info");
    }
}
