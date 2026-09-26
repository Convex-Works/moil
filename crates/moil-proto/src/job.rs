//! What a job needs: where its files come from and go to, and what the
//! script is told (spec §6.3, §8.2).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{FileName, JobId};

/// Where the runtime downloads an input from, with `GET`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Download {
    pub url: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

/// Where the runtime uploads an output to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upload {
    pub url: String,
    #[serde(default)]
    pub method: UploadMethod,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum UploadMethod {
    #[default]
    Put,
    Post,
}

impl UploadMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Put => "PUT",
            Self::Post => "POST",
        }
    }
}

/// The job context: the single JSON document a script reads from stdin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobContext {
    pub v: u32,
    pub job_id: JobId,
    pub attempt: u32,
    pub params: Value,
    pub work_dir: PathBuf,
    /// Input name → absolute path of the downloaded file.
    pub inputs: BTreeMap<FileName, PathBuf>,
    pub output_dir: PathBuf,
    /// The outputs the script may produce, by name, inside `output_dir`.
    pub outputs: Vec<FileName>,
    pub assets_dir: PathBuf,
    /// Asset name → absolute path of the asset.
    pub assets: BTreeMap<String, PathBuf>,
}
