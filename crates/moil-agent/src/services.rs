//! Paired services and what this machine's owner approved for each.

use std::fmt;
use std::time::SystemTime;

use moil_proto::BundleHash;
use serde::{Deserialize, Serialize};
use url::Url;

/// This machine's name for a paired service: derived from the service's
/// host and port (`klisi.example.com`, `localhost-8787`), unique among the
/// machine's services, and safe as a file name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServiceId(pub(crate) String);

impl ServiceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The ID for a new service at `base_url`, skipping IDs `taken` says are
    /// in use: `klisi.example.com`, then `klisi.example.com-2`, and so on.
    pub(crate) fn derive(base_url: &Url, taken: impl Fn(&str) -> bool) -> Self {
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
        Self(id)
    }
}

/// `s` lowercased, with anything but letters, digits, `.` and `-` turned
/// into single hyphens, and no leading or trailing punctuation, so that it
/// can't name a hidden file or leave the directory.
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
    out.trim_matches(['-', '.']).chars().take(100).collect()
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

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
    fn ids_are_safe_file_names() {
        for raw in ["../..", ".hidden", "a/b", "a\\b", "", "---", "é.example"] {
            let safe = file_safe(raw);
            assert!(
                !safe.starts_with('.') && !safe.contains(['/', '\\']),
                "{raw:?} -> {safe:?}"
            );
            assert!(
                safe.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            );
        }
    }
}
