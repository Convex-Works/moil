//! Where a machine keeps its state.

use std::path::{Path, PathBuf};

use moil_bundle::BundleStore;
use serde::{Deserialize, Serialize};

use crate::Error;

/// A machine's state directory:
///
/// ```text
/// <root>/
///   settings.json        Settings
///   services/<id>.json   one PairedService per paired service
///   bundles/<hash>/      approved bundles (BundleStore)
///   jobs/                attempts' working directories
///   cache/assets/        assets, by hash
///   agent.lock           held by the process that has the home open
/// ```
#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

/// The owner's preferences, kept across restarts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Don't take new jobs.
    pub paused: bool,
    /// What services call this machine. Defaults to the computer's name.
    pub machine_name: Option<String>,
    /// Accept `http` services, inputs and outputs on any host. For
    /// development only; the app says so wherever it shows a service.
    pub allow_insecure_http: bool,
    /// Keep attempts' working directories, for debugging bundles.
    pub keep_work_dirs: bool,
}

impl Home {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `$MOIL_HOME`, else the platform's application data directory:
    /// `~/Library/Application Support/moil` on macOS,
    /// `$XDG_DATA_HOME/moil` (`~/.local/share/moil`) elsewhere.
    pub fn default_root() -> Result<PathBuf, Error> {
        todo!("moil-agent: Home::default_root")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn services_dir(&self) -> PathBuf {
        self.root.join("services")
    }

    pub fn bundles(&self) -> BundleStore {
        BundleStore::new(self.root.join("bundles"))
    }

    pub fn settings_file(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    /// The runtime's configuration for this home, with `uv` as the uv binary.
    pub fn runtime_config(&self, uv: PathBuf) -> moil_runtime::RuntimeConfig {
        moil_runtime::RuntimeConfig::new(uv, &self.root)
    }
}
