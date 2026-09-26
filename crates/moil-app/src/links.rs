//! URLs that reach the app from outside: `moil://` links, the service
//! addresses the owner types, and the links in the app's own pages.
//!
//! All of them can carry text the owner didn't write, so each is parsed
//! strictly into what it is allowed to mean.

use std::fmt;

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
                let url = service_address(&service).map_err(|e| refuse(&e))?;
                Ok(DeepLink::Pair { url })
            }
            other => Err(refuse(&format!(
                "it asks for {other:?}, which moil doesn't do"
            ))),
        }
    }
}

/// The service base URL the owner means by `input`, which they typed or a
/// link supplied: surrounding whitespace is ignored and `https://` is
/// assumed when no scheme is given. Whether plain `http` is acceptable is
/// the agent's decision (spec §3), so it isn't refused here.
pub fn service_address(input: &str) -> Result<Url, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Enter the service's address, such as https://klisi.example.com/moil.".into());
    }
    let with_scheme;
    let input = if input.contains("://") {
        input
    } else {
        with_scheme = format!("https://{input}");
        &with_scheme
    };
    let url = Url::parse(input).map_err(|e| format!("{input} isn't a valid address: {e}."))?;
    if !matches!(url.scheme(), "https" | "http") {
        return Err(format!(
            "moil services have https addresses, not {}:// ones.",
            url.scheme()
        ));
    }
    if url.host().is_none() {
        return Err(format!("{input} doesn't name a host."));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("The address mustn't contain a user name or password.".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("The address can't contain a query (?…) or a fragment (#…).".into());
    }
    Ok(url)
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
    fn typed_addresses_default_to_https() {
        let address = |s| service_address(s).map(|u| u.to_string());
        assert_eq!(
            address("  klisi.example.com/moil \n").unwrap(),
            "https://klisi.example.com/moil"
        );
        assert_eq!(
            address("http://127.0.0.1:8787/moil").unwrap(),
            "http://127.0.0.1:8787/moil"
        );
        assert!(
            address("")
                .unwrap_err()
                .starts_with("Enter the service's address")
        );
        assert!(address("ftp://klisi.example.com").is_err());
        assert!(address("https://klisi.example.com/moil?x=1").is_err());
        assert!(address("https://a@klisi.example.com").is_err());
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
