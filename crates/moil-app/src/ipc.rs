//! Messages the app's pages send it with `window.ipc.postMessage`.
//!
//! A page shows text the owner didn't write, so the app treats whatever
//! arrives as untrusted: each page has its own closed set of messages, and
//! anything else (another page's message, an extra field, a malformed hash,
//! a huge body) is refused. Messages without fields are written as empty
//! struct variants so that serde refuses extra fields on them too.

use std::fmt;

use moil_proto::BundleHash;
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// From the review window.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewMessage {
    /// Approve the bundle with this hash, which must be the one shown.
    Approve { hash: BundleHash },
    /// Revoke the approval of the bundle with this hash, which must be the
    /// one shown.
    Revoke { hash: BundleHash },
    /// "Not now", or closing the window.
    Close {},
}

/// From the pair window.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PairMessage {
    /// Pair with the service at this address, as the owner typed it.
    Start { url: String },
    /// Open the service's confirmation page in the browser again.
    OpenBrowser {},
    /// Go back to entering an address after a failure.
    Retry {},
    /// Cancel any pairing in progress and close the window.
    Close {},
}

/// From a window that asks the owner to confirm something, such as
/// unpairing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfirmMessage {
    Confirm {},
    Close {},
}

/// A message body that isn't one the page may send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "refused a message from a page: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// No page sends anything near this long.
const MAX_MESSAGE_BYTES: usize = 4096;

/// Parses a message body as one of the messages `T` allows.
pub fn parse<T: DeserializeOwned>(body: &str) -> Result<T, Refused> {
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(Refused(format!("{} bytes is too long", body.len())));
    }
    serde_json::from_str(body).map_err(|e| Refused(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "a00a65c166291e905da7546a67518a81cc27cb498f009c75b824f28a8adfffb2";

    #[test]
    fn each_page_speaks_its_own_messages() {
        assert_eq!(
            parse::<ReviewMessage>(&format!(r#"{{"type":"approve","hash":"{HASH}"}}"#)),
            Ok(ReviewMessage::Approve {
                hash: HASH.parse().unwrap()
            })
        );
        assert_eq!(
            parse::<ReviewMessage>(r#"{"type":"close"}"#),
            Ok(ReviewMessage::Close {})
        );
        assert_eq!(
            parse::<PairMessage>(r#"{"type":"start","url":"klisi.example.com"}"#),
            Ok(PairMessage::Start {
                url: "klisi.example.com".into()
            })
        );
        assert_eq!(
            parse::<PairMessage>(r#"{"type":"open_browser"}"#),
            Ok(PairMessage::OpenBrowser {})
        );
        assert_eq!(
            parse::<ConfirmMessage>(r#"{"type":"confirm"}"#),
            Ok(ConfirmMessage::Confirm {})
        );
    }

    #[test]
    fn a_page_cannot_send_another_pages_messages() {
        let approve = format!(r#"{{"type":"approve","hash":"{HASH}"}}"#);
        assert!(parse::<PairMessage>(&approve).is_err());
        assert!(parse::<ConfirmMessage>(&approve).is_err());
        assert!(parse::<ReviewMessage>(r#"{"type":"confirm"}"#).is_err());
    }

    #[test]
    fn anything_malformed_is_refused() {
        for body in [
            "",
            "approve",
            "null",
            "[]",
            r#"{"hash":"x"}"#,
            r#"{"type":"approve"}"#,
            r#"{"type":"approve","hash":"A00A65C166291E905DA7546A67518A81CC27CB498F009C75B824F28A8ADFFFB2"}"#,
            r#"{"type":"approve","hash":"a00a65c1"}"#,
            r#"{"type":"approve","hash":42}"#,
            r#"{"type":"close","extra":true}"#,
            r#"{"type":"Close"}"#,
            &format!(r#"{{"type":"approve","hash":"{HASH}","hash":"{HASH}"}}"#),
            &format!(r#"{{"type":"approve","hash":"{HASH}","also":"{HASH}"}}"#),
        ] {
            assert!(
                parse::<ReviewMessage>(body).is_err(),
                "{body:?} should be refused"
            );
        }
    }

    #[test]
    fn huge_bodies_are_refused_before_parsing() {
        let url = "a".repeat(MAX_MESSAGE_BYTES);
        let body = format!(r#"{{"type":"start","url":"{url}"}}"#);
        let err = parse::<PairMessage>(&body).unwrap_err();
        assert!(err.to_string().contains("too long"), "{err}");
    }
}
