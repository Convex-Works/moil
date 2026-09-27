//! URLs that reach the app from outside: `moil://` links and the links in
//! the app's own pages. The service addresses the owner types are the
//! agent's to read ([`moil_agent::services::parse_base_url`]).
//!
//! All of them can carry text the owner didn't write, so each is parsed
//! strictly into what it is allowed to mean.

use std::fmt;

use moil_agent::services::parse_base_url;
use url::Url;

/// What a `moil://` link asks the app to do. Every link only ever leads to
/// a screen that asks the owner first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepLink {
    /// `moil://pair?url=<base URL>`: offer to pair with the service at `url`.
    Pair { url: Url },
}

/// Why a `moil://` link was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkError(String);

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LinkError {}

/// Links longer than this are refused outright.
const MAX_LINK_LEN: usize = 2048;

impl DeepLink {
    pub fn parse(link: &str) -> Result<Self, LinkError> {
        let refuse = |why: &str| LinkError(format!("moil can't open this link: {why}"));
        if link.len() > MAX_LINK_LEN {
            return Err(refuse("it is too long"));
        }
        let url = Url::parse(link).map_err(|_| refuse("it isn't a valid URL"))?;
        if url.scheme() != "moil" {
            return Err(refuse("it isn't a moil:// link"));
        }
        // `moil://pair?…` has `pair` as its host; `moil:pair?…` as its path.
        let action = match url.host_str() {
            Some(host) if matches!(url.path(), "" | "/") => host,
            None => url.path(),
            Some(_) => return Err(refuse("it has an unexpected path")),
        };
        match action {
            "pair" => {
                let mut urls = url.query_pairs().filter(|(key, _)| key == "url");
                let (Some((_, service)), None) = (urls.next(), urls.next()) else {
                    return Err(refuse("it must name exactly one service URL"));
                };
                let url = parse_base_url(&service).map_err(|e| refuse(&e.to_string()))?;
                Ok(DeepLink::Pair { url })
            }
            other => Err(refuse(&format!(
                "it asks for {other:?}, which moil doesn't do"
            ))),
        }
    }
}

/// A link that may be opened in the owner's browser: `http` or `https`,
/// with a host and no credentials. Anything else (`file:`, `javascript:`,
/// custom schemes, including `moil:`) is refused.
pub fn browser_url(link: &str) -> Option<Url> {
    let url = Url::parse(link).ok()?;
    let web = matches!(url.scheme(), "https" | "http") && url.host().is_some();
    let credentials = !url.username().is_empty() || url.password().is_some();
    (web && !credentials).then_some(url)
}

/// What to do when one of the app's pages tries to navigate to a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Navigation {
    /// The page itself, or a place within it.
    Stay,
    /// A web link: open it in the browser and keep the page as it is.
    Browser(Url),
    /// Anything else: ignore it.
    Block,
}

impl Navigation {
    /// Pages are loaded from strings, so their own URL is `about:blank`.
    pub fn of(url: &str) -> Self {
        if url == "about:blank" || url.starts_with("about:blank#") {
            Navigation::Stay
        } else if let Some(url) = browser_url(url) {
            Navigation::Browser(url)
        } else {
            Navigation::Block
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair_url(link: &str) -> String {
        match DeepLink::parse(link) {
            Ok(DeepLink::Pair { url }) => url.to_string(),
            Err(e) => panic!("{link}: {e}"),
        }
    }

    #[test]
    fn pair_links_name_the_service() {
        assert_eq!(
            pair_url("moil://pair?url=https%3A%2F%2Fklisi.example.com%2Fmoil"),
            "https://klisi.example.com/moil"
        );
        assert_eq!(
            pair_url("moil://pair/?url=https://klisi.example.com/moil&utm_source=mail"),
            "https://klisi.example.com/moil"
        );
        assert_eq!(
            pair_url("moil:pair?url=http://localhost:8787/moil"),
            "http://localhost:8787/moil"
        );
    }

    #[test]
    fn malformed_links_are_refused() {
        for link in [
            "https://pair?url=https://klisi.example.com",
            "moil://pair",
            "moil://pair?url=",
            "moil://pair?url=https://a.example&url=https://b.example",
            "moil://pair?url=file:///etc/passwd",
            "moil://pair?url=javascript:alert(1)",
            "moil://pair?url=https://user:pw@klisi.example.com",
            "moil://pair/extra?url=https://klisi.example.com",
            "moil://approve?hash=a00a65c1",
            "not a url",
        ] {
            assert!(DeepLink::parse(link).is_err(), "{link} should be refused");
        }
        let long = format!("moil://pair?url=https://{}.example", "a".repeat(3000));
        assert!(DeepLink::parse(&long).is_err());
    }

    #[test]
    fn refusals_explain_themselves() {
        let err = DeepLink::parse("moil://approve?hash=x").unwrap_err();
        assert_eq!(
            err.to_string(),
            "moil can't open this link: it asks for \"approve\", which moil doesn't do"
        );
    }

    #[test]
    fn only_web_links_leave_for_the_browser() {
        assert_eq!(Navigation::of("about:blank"), Navigation::Stay);
        assert_eq!(Navigation::of("about:blank#job-py"), Navigation::Stay);
        assert_eq!(
            Navigation::of("https://pypi.org/project/av/15.1.0/"),
            Navigation::Browser(Url::parse("https://pypi.org/project/av/15.1.0/").unwrap())
        );
        for blocked in [
            "file:///Users/ada/.ssh/id_ed25519",
            "javascript:window.ipc.postMessage('{}')",
            "moil://pair?url=https://evil.example",
            "data:text/html,<script>alert(1)</script>",
            "about:srcdoc",
            "https://user:secret@example.com/",
        ] {
            assert_eq!(Navigation::of(blocked), Navigation::Block, "{blocked}");
        }
    }
}
