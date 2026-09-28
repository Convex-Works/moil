//! Hooks: the owner's own executables, which moil runs after an attempt of
//! their bundle succeeds here (spec/machine.md §2).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

use moil_proto::{BundleHash, FileName, JobId};
use serde::{Deserialize, Serialize};

use crate::report::ServiceRef;

/// The hook contract's version: the context's `v`.
pub const HOOK_CONTRACT_VERSION: u32 = 1;

/// What a hook reads from stdin, as one line of JSON (spec/machine.md
/// §2.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookContext {
    /// [`HOOK_CONTRACT_VERSION`].
    pub v: u32,
    pub service: ServiceRef,
    pub bundle: BundleRef,
    pub job_id: JobId,
    pub attempt: u32,
    /// The job's title, if the service gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The job's params, as the script got them.
    pub params: serde_json::Value,
    /// The script's `result.meta`, if it sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    #[serde(with = "crate::rfc3339")]
    pub ended_at: SystemTime,
    /// A directory holding the outputs and nothing else; the hook runs in
    /// it.
    pub output_dir: PathBuf,
    /// Each output the attempt uploaded, with its absolute path.
    pub outputs: BTreeMap<FileName, PathBuf>,
}

/// The approved bundle an attempt ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleRef {
    pub name: String,
    pub version: String,
    pub hash: BundleHash,
}
