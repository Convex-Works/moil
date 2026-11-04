//! Which URLs a machine uses for a service (spec §3).

use url::{Host, Url};

/// Which URLs a machine accepts for one service: `https` always, plain
/// `http` only as far as the policy allows.
///
/// A service's base URL may be `http` to a loopback host. Its jobs' inputs
/// and outputs may use loopback `http` only if the service itself is on
/// loopback, so that a remote service can't aim requests at services that
/// listen only on the owner's machine. The owner can allow `http` anywhere,
/// for development.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct UrlPolicy {
    /// Allow `http` to loopback hosts.
    pub loopback_http: bool,
    /// Allow `http` to any host.
    pub any_http: bool,
}

/// Why a URL was refused. Its message completes the sentence "the URL …".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlRefused {
    #[error("isn't valid: {0}")]
    Invalid(String),
    #[error("uses {0}; only https is allowed")]
    Scheme(String),
    #[error("has no host")]
    NoHost,
    #[error("uses http to {0}; only https is allowed")]
    Http(String),
    #[error("uses http to {0}, on this machine, for a service that isn't; only https is allowed")]
    LoopbackHttp(String),
}

impl UrlPolicy {
    /// `https` only.
    pub const HTTPS_ONLY: Self = Self {
        loopback_http: false,
        any_http: false,
    };

    /// The policy for base URLs, which may be `http` to loopback.
    pub fn for_base_urls(allow_insecure_http: bool) -> Self {
        Self {
            loopback_http: true,
            any_http: allow_insecure_http,
        }
    }

    /// The policy for the inputs and outputs of the jobs of the service at
    /// `base_url`.
    pub fn for_service(base_url: &Url, allow_insecure_http: bool) -> Self {
        Self {
            loopback_http: is_loopback(base_url),
            any_http: allow_insecure_http,
        }
    }

    /// Fails unless the policy allows `url`.
    pub fn check(&self, url: &Url) -> Result<(), UrlRefused> {
        if !url.has_host() || url.host_str() == Some("") {
            return Err(UrlRefused::NoHost);
        }
        match url.scheme() {
            "https" => Ok(()),
            "http" if self.any_http => Ok(()),
            "http" if is_loopback(url) => {
                if self.loopback_http {
                    Ok(())
                } else {
                    Err(UrlRefused::LoopbackHttp(host(url)))
                }
            }
            "http" => Err(UrlRefused::Http(host(url))),
            scheme => Err(UrlRefused::Scheme(scheme.to_owned())),
        }
    }

    /// Parses `url` and fails unless the policy allows it.
    pub fn check_str(&self, url: &str) -> Result<Url, UrlRefused> {
        let parsed = Url::parse(url).map_err(|e| UrlRefused::Invalid(e.to_string()))?;
        self.check(&parsed)?;
        Ok(parsed)
    }
}

/// Whether `url`'s host is `localhost`, in `127.0.0.0/8`, or `::1`.
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain
            .strip_suffix('.')
            .unwrap_or(domain)
            .eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

fn host(url: &Url) -> String {
    url.host_str().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn a_remote_service_gets_https_only() {
        let policy = UrlPolicy::for_service(&url("https://acme.example.com/moil"), false);
        assert!(policy.check(&url("https://s3.example.com/a")).is_ok());
        for refused in [
            "http://s3.example.com/a",
            "http://127.0.0.1:9000/a",
            "http://localhost/a",
            "http://[::1]/a",
            "ftp://example.com/a",
            "file:///etc/passwd",
        ] {
            assert!(
                policy.check(&url(refused)).is_err(),
                "{refused} should be refused"
            );
        }
        assert_eq!(
            policy.check(&url("http://localhost:6379/")),
            Err(UrlRefused::LoopbackHttp("localhost".into()))
        );
    }

    #[test]
    fn a_service_on_loopback_may_use_loopback_http() {
        let policy = UrlPolicy::for_service(&url("http://127.0.0.1:8787/moil"), false);
        for allowed in [
            "http://127.0.0.1:8787/files/x",
            "http://localhost./a",
            "http://[::1]:1/a",
            "https://e.com/a",
        ] {
            assert!(
                policy.check(&url(allowed)).is_ok(),
                "{allowed} should be allowed"
            );
        }
        assert_eq!(
            policy.check(&url("http://example.com/a")),
            Err(UrlRefused::Http("example.com".into()))
        );
    }

    #[test]
    fn base_urls_may_be_loopback_http() {
        let policy = UrlPolicy::for_base_urls(false);
        assert!(policy.check(&url("http://localhost:8787/moil")).is_ok());
        assert!(policy.check(&url("http://acme.example.com/moil")).is_err());
    }

    #[test]
    fn the_owner_can_allow_http_anywhere_but_nothing_else() {
        let policy = UrlPolicy::for_service(&url("https://acme.example.com/moil"), true);
        assert!(policy.check(&url("http://minio:9000/a")).is_ok());
        assert!(policy.check(&url("file:///etc/passwd")).is_err());
    }

    #[test]
    fn check_str_reports_unparseable_urls() {
        assert!(matches!(
            UrlPolicy::HTTPS_ONLY.check_str("not a url"),
            Err(UrlRefused::Invalid(_))
        ));
    }
}
