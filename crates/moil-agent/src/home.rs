//! Where a machine keeps its state.

use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use moil_bundle::BundleStore;
use serde::{Deserialize, Serialize};

use crate::fs::{PUBLIC, read_json, with_path, write_json};
use crate::{Error, ServiceId};

/// A machine's state directory, readable only by the owner: it holds job
/// inputs and outputs, which other users of the computer mustn't read.
///
/// ```text
/// <root>/
///   settings.json        Settings
///   services/<id>.json   one PairedService per paired service
///   bundles/<hash>/      approved bundles (BundleStore)
///   jobs/                attempts' working directories
///   cache/assets/        assets, by hash
///   secrets.json         tokens, when they aren't in the keychain
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
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        if let Some(root) = var("MOIL_HOME") {
            return Ok(root.into());
        }
        if cfg!(windows)
            && let Some(app_data) = var("APPDATA")
        {
            return Ok(PathBuf::from(app_data).join("moil"));
        }
        let user_home = var("HOME").map(PathBuf::from).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "couldn't find your home directory; set $MOIL_HOME to where moil should keep its state",
            )
        })?;
        if cfg!(target_os = "macos") {
            return Ok(user_home.join("Library/Application Support/moil"));
        }
        let data = var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| user_home.join(".local/share"));
        Ok(data.join("moil"))
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

    /// Where [`FileSecrets`](crate::FileSecrets) keeps tokens for this home.
    pub fn secrets_file(&self) -> PathBuf {
        self.root.join("secrets.json")
    }

    /// The file whose lock marks the home as in use.
    pub fn lock_file(&self) -> PathBuf {
        self.root.join("agent.lock")
    }

    /// The runtime's configuration for this home, with `uv` as the uv
    /// binary and the runtime's defaults for everything else. See
    /// [`Machine::runtime_config`](crate::Machine::runtime_config) for one
    /// with the owner's settings applied.
    pub fn runtime_config(&self, uv: PathBuf) -> moil_runtime::RuntimeConfig {
        moil_runtime::RuntimeConfig::new(uv, &self.root)
    }

    pub(crate) fn service_file(&self, id: &ServiceId) -> PathBuf {
        self.services_dir().join(format!("{id}.json"))
    }

    /// Creates the home's directories and takes its lock, which lasts as
    /// long as the returned file stays open. A home others can read, such
    /// as one an older moil created, is made private.
    pub(crate) fn lock(&self) -> Result<File, Error> {
        create_private(&self.root).map_err(|e| with_path(e, &self.root))?;
        fs::create_dir_all(self.services_dir()).map_err(|e| with_path(e, &self.root))?;
        let path = self.lock_file();
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| with_path(e, &path))?;
        // A process that this one (or another) spawns shares the lock from
        // fork to exec, so the lock can look taken for a moment after its
        // holder let go. A running moil holds it for good.
        let give_up_at = Instant::now() + Duration::from_millis(250);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) if Instant::now() < give_up_at => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(Error::AlreadyRunning(self.root.clone()));
                }
                Err(TryLockError::Error(e)) => return Err(with_path(e, &path).into()),
            }
        }
    }

    pub(crate) fn load_settings(&self) -> Result<Settings, Error> {
        Ok(read_json(&self.settings_file())?.unwrap_or_default())
    }

    pub(crate) fn save_settings(&self, settings: &Settings) -> Result<(), Error> {
        write_json(&self.settings_file(), settings, PUBLIC)
    }
}

/// Creates `dir`, and any missing parents, readable only by the owner, and
/// takes other users' access to it if it already exists.
fn create_private(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let mode = fs::metadata(dir)?.permissions().mode();
        if mode & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(mode & !0o077))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    fs::create_dir_all(dir)
}
