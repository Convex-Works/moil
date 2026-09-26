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
