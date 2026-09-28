//! Where a machine keeps its state.

use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use moil_bundle::BundleStore;
use moil_proto::BundleHash;
use serde::{Deserialize, Serialize};

use crate::fs::{PUBLIC, read_strict_json, with_path, write_atomic, write_json};
use crate::report::Problem;
use crate::{Error, ServiceId};

/// A machine's home (spec/machine.md §1): plain files that the owner, an
/// editor, git or an LLM can read, in a folder only the owner can open,
/// since it also holds job inputs and outputs.
///
/// ```text
/// <root>/
///   README.md                      what the folder is (README_TEXT)
///   .gitignore                     written when missing (GITIGNORE)
///   settings.json                  Settings
///   logs/moil.log                  the agent's log
///   services/<id>/                 one folder per service the home knows
///     service.json                 the service, and the bundles approved for it
///     pairing.json                 this machine's pairing with it
///     .gitignore                   written when missing (SERVICE_GITIGNORE)
///     bundles/<name>/<hash>/       each approved bundle's files, read-only
///     hooks/<name>                 the owner's hook for a bundle
///     logs/<name>.log              what that hook printed
///   .machine/                      what only moil reads, in a layout that may change:
///     agent.lock                   held by the process that has the home open
///     agent.sock                   the control socket
///     secrets.json                 tokens, when they aren't in the keychain
///     bundles/<hash>/              the copies the runtime runs (BundleStore)
///     jobs/                        attempts' working directories
///     cache/assets/                assets, by hash
///     cache/environments/          each stored bundle's Python environment
///     hooks/                       hooks waiting to run
///     bin/                         the moil and uv hooks run
/// ```
#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

/// The owner's preferences, kept across restarts in `settings.json`
/// (spec/machine.md §1.1). Every field is optional; the file is strict.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Don't take new jobs.
    pub paused: bool,
    /// What services call this machine. Defaults to the computer's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_name: Option<String>,
    /// Accept `http` services, inputs and outputs on any host. For
    /// development only; the app says so wherever it shows a service.
    pub allow_insecure_http: bool,
    /// Keep attempts' working directories, for debugging bundles.
    pub keep_work_dirs: bool,
}

/// `README.md` at the root of the home, for a person or an LLM opening the
/// folder. moil rewrites it whenever it differs.
pub const README_TEXT: &str = include_str!("home_readme.md");

/// The `.gitignore` moil writes at the root of the home when there is
/// none: it keeps what belongs to this machine alone out of git, so that
/// the whole home can be a repository (spec/machine.md §1.2).
pub const GITIGNORE: &str = "\
# Written by moil because there was none; yours to change. See README.md.
# What belongs to this machine alone:
/.machine/
/settings.json
pairing.json
logs/
";

/// The `.gitignore` moil writes in a service's folder when there is none,
/// so that the folder can be a repository of its own.
pub const SERVICE_GITIGNORE: &str = "\
# Written by moil because there was none; yours to change.
# What belongs to this machine alone:
/pairing.json
/logs/
";

impl Home {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `$MOIL_HOME`, else `~/.moil` (`%USERPROFILE%\.moil` on Windows).
    pub fn default_root() -> Result<PathBuf, Error> {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        if let Some(root) = var("MOIL_HOME") {
            return Ok(root.into());
        }
        let user_home = if cfg!(windows) {
            var("USERPROFILE")
        } else {
            var("HOME")
        };
        let user_home = user_home.map(PathBuf::from).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "couldn't find your home directory; set $MOIL_HOME to where moil should keep its state",
            )
        })?;
        Ok(user_home.join(".moil"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // The layout of spec/machine.md §1.

    /// What the home is, for whoever opens it; moil keeps it current.
    pub fn readme_file(&self) -> PathBuf {
        self.root.join("README.md")
    }

    /// Keeps what belongs to this machine alone out of git.
    pub fn gitignore_file(&self) -> PathBuf {
        self.root.join(".gitignore")
    }

    /// The agent's log, whichever process runs the agent.
    pub fn log_file(&self) -> PathBuf {
        self.root.join("logs").join("moil.log")
    }

    /// A service's folder: `services/<id>`.
    pub fn service_dir(&self, id: &ServiceId) -> PathBuf {
        self.services_dir().join(id.as_str())
    }

    /// The owner's hook for a service's bundle: `services/<id>/hooks/<bundle>`.
    /// `bundle` is a bundle name (protocol §2), so it is a plain file name.
    pub fn hook(&self, id: &ServiceId, bundle: &str) -> PathBuf {
        debug_assert!(is_file_name(bundle), "{bundle:?} isn't a bundle name");
        self.service_dir(id).join("hooks").join(bundle)
    }

    /// What a hook printed: `services/<id>/logs/<bundle>.log`.
    pub fn hook_log(&self, id: &ServiceId, bundle: &str) -> PathBuf {
        debug_assert!(is_file_name(bundle), "{bundle:?} isn't a bundle name");
        self.service_dir(id)
            .join("logs")
            .join(format!("{bundle}.log"))
    }

    /// moil's own: what only moil reads, in a layout that may change.
    pub fn machine_dir(&self) -> PathBuf {
        self.root.join(".machine")
    }

    /// The `moil` and `uv` this machine runs, as links: first on a hook's
    /// `PATH` (spec/machine.md §2.2).
    pub fn bin_dir(&self) -> PathBuf {
        self.machine_dir().join("bin")
    }

    /// Hooks waiting to run, or running, with the outputs they get.
    pub fn hooks_queue_dir(&self) -> PathBuf {
        self.machine_dir().join("hooks")
    }

    /// Where the agent listens for commands (spec/machine.md §4).
    pub fn socket(&self) -> PathBuf {
        self.machine_dir().join("agent.sock")
    }

    pub fn services_dir(&self) -> PathBuf {
        self.root.join("services")
    }

    /// Which service a folder is for, and what the owner approved for it.
    pub fn service_file(&self, id: &ServiceId) -> PathBuf {
        self.service_dir(id).join("service.json")
    }

    /// This machine's pairing with a service.
    pub fn pairing_file(&self, id: &ServiceId) -> PathBuf {
        self.service_dir(id).join("pairing.json")
    }

    /// Keeps what belongs to this machine alone out of a service folder's
    /// repository.
    pub fn service_gitignore_file(&self, id: &ServiceId) -> PathBuf {
        self.service_dir(id).join(".gitignore")
    }

    /// Where the bundles approved for a service are: `services/<id>/bundles`.
    pub fn approved_bundles_dir(&self, id: &ServiceId) -> PathBuf {
        self.service_dir(id).join("bundles")
    }

    /// Where an approved bundle's three files are:
    /// `services/<id>/bundles/<name>/<hash>`. `name` is a bundle name
    /// (protocol §2), so it is a plain file name.
    pub fn approved_bundle_dir(&self, id: &ServiceId, name: &str, hash: &BundleHash) -> PathBuf {
        debug_assert!(is_file_name(name), "{name:?} isn't a bundle name");
        self.approved_bundles_dir(id)
            .join(name)
            .join(hash.to_string())
    }

    /// The copies of approved bundles that the runtime runs, one directory
    /// per hash, each verified against an approval before it was stored.
    pub fn bundle_store(&self) -> BundleStore {
        BundleStore::new(self.machine_dir().join("bundles"))
    }

    pub fn settings_file(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    /// Where [`FileSecrets`](crate::FileSecrets) keeps tokens for this home.
    pub fn secrets_file(&self) -> PathBuf {
        self.machine_dir().join("secrets.json")
    }

    /// The file whose lock marks the home as in use.
    pub fn lock_file(&self) -> PathBuf {
        self.machine_dir().join("agent.lock")
    }

    /// The runtime's configuration for this home, with `uv` as the uv
    /// binary and the runtime's defaults for everything else: everything
    /// it keeps is in `.machine/`. See
    /// [`Machine::runtime_config`](crate::Machine::runtime_config) for one
    /// with the owner's settings applied.
    pub fn runtime_config(&self, uv: PathBuf) -> moil_runtime::RuntimeConfig {
        let config = moil_runtime::RuntimeConfig::new(uv, &self.machine_dir());
        debug_assert_eq!(config.bundles.root(), self.bundle_store().root());
        config
    }

    /// Creates the home's directories and takes its lock, which lasts as
    /// long as the returned file stays open. A home others can read, such
    /// as one an older moil created, is made private.
    pub(crate) fn lock(&self) -> Result<File, Error> {
        create_private(&self.root).map_err(|e| with_path(e, &self.root))?;
        let machine_dir = self.machine_dir();
        create_private(&machine_dir).map_err(|e| with_path(e, &machine_dir))?;
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

    /// The settings, or [`Error::Invalid`] saying what's wrong with them.
    pub(crate) fn load_settings(&self) -> Result<Settings, Error> {
        Ok(read_strict_json(&self.settings_file())?.unwrap_or_default())
    }

    pub(crate) fn save_settings(&self, settings: &Settings) -> Result<(), Error> {
        write_json(&self.settings_file(), settings, PUBLIC)
    }

    /// The problem to report when `settings.json` can't be used, as
    /// [`load_settings`](Self::load_settings) says: `kept` when a running
    /// agent goes on with the settings it read before.
    pub(crate) fn settings_problem(error: &Error, kept: bool) -> Problem {
        let message = match (error, kept) {
            (Error::Invalid { reason, .. }, false) => format!(
                "This file isn't valid ({reason}), and moil won't start until it is. Fix it, or \
                 remove it to go back to the defaults."
            ),
            (Error::Invalid { reason, .. }, true) => format!(
                "This file isn't valid ({reason}), so moil goes on with the settings it read \
                 before, and won't start again until it is. Fix it, or remove it to go back to \
                 the defaults."
            ),
            (error, false) => format!("moil can't read this file ({error})."),
            (error, true) => format!(
                "moil can't read this file ({error}), so it goes on with the settings it read \
                 before."
            ),
        };
        Problem {
            path: "settings.json".into(),
            message,
        }
    }

    /// Writes what the home's readers go by: `README.md`, whenever its text
    /// isn't this version's, and the `.gitignore` files at the root and in
    /// each service folder, where there are none.
    pub(crate) fn write_guides(&self) -> Result<(), Error> {
        let readme = self.readme_file();
        if fs::read(&readme).ok().as_deref() != Some(README_TEXT.as_bytes()) {
            write_atomic(&readme, README_TEXT.as_bytes(), PUBLIC)
                .map_err(|e| with_path(e, &readme))?;
        }
        write_if_missing(&self.gitignore_file(), GITIGNORE)?;
        let services = self.services_dir();
        let entries = match fs::read_dir(&services) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(with_path(e, &services).into()),
        };
        for entry in entries {
            let entry = entry.map_err(|e| with_path(e, &services))?;
            let id = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<ServiceId>().ok());
            if let Some(id) = id
                && entry.path().is_dir()
            {
                self.write_service_gitignore(&id)?;
            }
        }
        Ok(())
    }

    /// Writes a service folder's `.gitignore`, unless it has one.
    pub(crate) fn write_service_gitignore(&self, id: &ServiceId) -> Result<(), Error> {
        write_if_missing(&self.service_gitignore_file(id), SERVICE_GITIGNORE)
    }
}

/// Writes `text` to `path` if nothing is there: once written, the file is
/// the owner's.
fn write_if_missing(path: &Path, text: &str) -> Result<(), Error> {
    if fs::symlink_metadata(path).is_ok() {
        return Ok(());
    }
    Ok(write_atomic(path, text.as_bytes(), PUBLIC).map_err(|e| with_path(e, path))?)
}

/// Whether `name` names a file right in a directory, not a path.
fn is_file_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
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
