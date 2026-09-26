//! Bodies of the HTTP endpoints (spec §3, §4, §5.4).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::control::{BundleInfo, ServiceInfo};
use crate::ids::BundleHash;

/// `GET /v1/info`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfoResponse {
    pub protocol: u32,
    pub name: String,
}

/// `POST /v1/pair`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub app_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    /// Seconds until the codes expire.
    pub expires_in: u64,
    /// Seconds to wait between polls.
    pub interval: u64,
    pub service: ServiceInfo,
}

/// `POST /v1/pair/token`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRequest {
    pub device_code: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenResponse {
    pub machine_id: String,
    pub token: String,
    pub service: ServiceInfo,
}

impl fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenResponse")
            .field("machine_id", &self.machine_id)
            .field("token", &"<redacted>")
            .field("service", &self.service)
            .finish()
    }
}

/// `GET /v1/bundles`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundlesResponse {
    pub bundles: Vec<BundleInfo>,
}

/// `GET /v1/bundles/{hash}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleResponse {
    pub hash: BundleHash,
    /// File name → the file's exact contents.
    pub files: BTreeMap<String, String>,
}

/// The body of every error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The `error` values of `POST /v1/pair/token` (RFC 8628 §3.5).
pub mod token_error {
    pub const AUTHORIZATION_PENDING: &str = "authorization_pending";
    pub const SLOW_DOWN: &str = "slow_down";
    pub const ACCESS_DENIED: &str = "access_denied";
    pub const EXPIRED_TOKEN: &str = "expired_token";
}
