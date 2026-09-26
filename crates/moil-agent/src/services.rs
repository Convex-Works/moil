//! Paired services and what this machine's owner approved for each.

use std::fmt;
use std::str::FromStr;
use std::time::SystemTime;

use moil_proto::BundleHash;
use serde::{Deserialize, Serialize};
use url::Url;

/// This machine's name for a paired service: derived from the service's
/// host and port (`klisi.example.com`, `localhost-8787`), unique among the
/// machine's services, and safe as a file name. An ID is 1 to 128
/// lowercase ASCII letters, digits, dots and hyphens, starting and ending
/// with a letter or digit, so it can't name a hidden file or leave a
/// directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServiceId(String);

impl ServiceId {
    /// The longest an ID can be, in characters.
    pub const MAX_LEN: usize = 128;

    /// An ID written out, such as one read back from a file. Fails unless
    /// it follows the rules above; [`derive`](Self::derive) makes new ones.
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidServiceId> {
        let id = id.into();
        if is_service_id(&id) {
            Ok(Self(id))
        } else {
            Err(InvalidServiceId(id))
        }
    }

    /// The ID for a new service at `base_url`, skipping IDs `taken` says are
    /// in use: `klisi.example.com`, then `klisi.example.com-2`, and so on.
    pub fn derive(base_url: &Url, taken: impl Fn(&str) -> bool) -> Self {
        let host = base_url.host_str().unwrap_or_default();
        let mut stem = match base_url.port() {
            Some(port) => format!("{host}-{port}"),
            None => host.to_owned(),
        };
        stem = file_safe(&stem);
        if stem.is_empty() {
            stem = "service".to_owned();
        }
        let id = (1..)
            .map(|n| match n {
                1 => stem.clone(),
                n => format!("{stem}-{n}"),
            })
            .find(|id| !taken(id))
            .expect("some suffix is free");
        debug_assert!(is_service_id(&id), "derived an invalid ID {id:?}");
        Self(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_service_id(s: &str) -> bool {
    let edge = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let bytes = s.as_bytes();
    (1..=ServiceId::MAX_LEN).contains(&bytes.len())
        && bytes.first().is_some_and(edge)
        && bytes.last().is_some_and(edge)
        && bytes.iter().all(|b| edge(b) || matches!(b, b'.' | b'-'))
}

/// `s` lowercased, with anything but letters, digits, `.` and `-` turned
/// into single hyphens, at most 100 characters (leaving room for a suffix),
/// and no leading or trailing punctuation.
fn file_safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().map(|c| c.to_ascii_lowercase()) {
        let c = if c.is_ascii_alphanumeric() || c == '.' {
            c
        } else {
            '-'
        };
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    let out: String = out.trim_matches(['-', '.']).chars().take(100).collect();
    out.trim_end_matches(['-', '.']).to_owned()
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ServiceId {
    type Err = InvalidServiceId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for ServiceId {
    type Error = InvalidServiceId;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        Self::new(id)
    }
}

impl From<ServiceId> for String {
    fn from(id: ServiceId) -> Self {
        id.0
    }
}

/// A string that breaks the rules for a [`ServiceId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{0:?} isn't a service ID: IDs are 1 to 128 lowercase letters, digits, dots and hyphens, starting and ending with a letter or digit"
)]
pub struct InvalidServiceId(String);

/// A service this machine is paired with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedService {
    pub id: ServiceId,
    /// Where the service's moil endpoints live, e.g. `https://klisi.example.com/moil`.
    pub base_url: Url,
    /// The name the service gave itself, e.g. `klisi`.
    pub name: String,
    /// The service's ID for this machine.
    pub machine_id: String,
    pub paired_at: SystemTime,
    /// The bundles the owner approved for this service, oldest first.
    pub approvals: Vec<Approval>,
}

impl PairedService {
    pub fn is_approved(&self, hash: &BundleHash) -> bool {
        self.approvals.iter().any(|a| a.hash == *hash)
    }

    /// Every approved hash, oldest first, as `hello` and `approved` carry
    /// them.
    pub fn approved_hashes(&self) -> Vec<BundleHash> {
        self.approvals.iter().map(|a| a.hash).collect()
    }

    /// Whether the machine reaches this service over plain `http`.
    pub fn is_insecure(&self) -> bool {
        self.base_url.scheme() == "http"
    }
}

/// One approved bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub hash: BundleHash,
    /// The bundle's name and version, from its verified manifest.
    pub name: String,
    pub version: String,
    pub approved_at: SystemTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(url: &str, taken: &[&str]) -> String {
        ServiceId::derive(&Url::parse(url).unwrap(), |c| taken.contains(&c)).0
    }

    #[test]
    fn ids_come_from_host_and_port() {
        assert_eq!(
            id("https://klisi.example.com/moil", &[]),
            "klisi.example.com"
        );
        assert_eq!(
            id("https://Klisi.Example.com:443/moil", &[]),
            "klisi.example.com"
        );
        assert_eq!(id("http://localhost:8787/moil", &[]), "localhost-8787");
        assert_eq!(id("http://127.0.0.1:9000", &[]), "127.0.0.1-9000");
        assert_eq!(id("http://[::1]:9000/", &[]), "1-9000");
    }

    #[test]
    fn ids_are_unique_among_the_machines_services() {
        let taken = ["klisi.example.com", "klisi.example.com-2"];
        assert_eq!(
            id("https://klisi.example.com/other", &taken),
            "klisi.example.com-3"
        );
    }

    #[test]
    fn derived_ids_are_valid_whatever_the_host() {
        let long = format!("https://{}.{}.example.com/", "a".repeat(60), "b".repeat(38));
        assert_eq!(
            id(&long, &[]),
            format!("{}.{}", "a".repeat(60), "b".repeat(38)),
            "cut to 100 characters without a dot at the end"
        );
        for url in [
            long.as_str(),
            "https://xn--bcher-kva.example/",
            "http://[::1]/",
            "http://[fe80::1]:8080/",
            "file:///tmp/moil",
            "https://-.example.com./",
        ] {
            for taken in [&[][..], &[id(url, &[]).as_str()][..]] {
                let derived = id(url, taken);
                assert_eq!(
                    ServiceId::new(derived.clone()).map(String::from),
                    Ok(derived.clone()),
                    "{url} gave {derived:?}"
                );
            }
        }
    }

    #[test]
    fn written_ids_must_be_safe_file_names() {
        for good in ["klisi.example.com", "localhost-8787", "1-9000", "a", "a..b"] {
            assert_eq!(good.parse::<ServiceId>().unwrap().as_str(), good);
        }
        let longest = "x".repeat(ServiceId::MAX_LEN);
        assert!(ServiceId::new(longest.clone()).is_ok());
        for bad in [
            "",
            ".",
            "..",
            "../etc",
            ".hidden",
            "a/b",
            "a\\b",
            "Klisi.example.com",
            "klisi.example.com.",
            "-klisi",
            "klisi-",
            "kl isi",
            "bücher.example",
            &format!("{longest}x"),
        ] {
            let err = ServiceId::new(bad).unwrap_err();
            assert!(err.to_string().contains("isn't a service ID"), "{err}");
        }
    }

    #[test]
    fn ids_are_checked_when_read_back() {
        let id: ServiceId = serde_json::from_str("\"klisi.example.com\"").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"klisi.example.com\"");
        assert!(serde_json::from_str::<ServiceId>("\"../../etc/passwd\"").is_err());
    }
}
