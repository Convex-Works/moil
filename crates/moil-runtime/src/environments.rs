//! Bundles' uv environments: one per bundle, named by its hash, so that
//! each can go when its bundle does. uv would otherwise keep them in its
//! cache, where nothing removes them.

use std::fs::{File, TryLockError};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use moil_bundle::{BundleHash, BundleStore};
use moil_proto::{ErrorCode, JobError};

use crate::uv::{self, Script, Uv};
use crate::workdir::remove_tree;

/// Held while an environment is made or removed. uv locks an environment
/// while it syncs one, but runtimes in other processes may share the
/// directory, as two `moil run`s do.
const LOCK: &str = ".lock";

/// A directory of environments, `<dir>/<bundle hash>`.
#[derive(Debug)]
pub(crate) struct Environments {
    dir: PathBuf,
}

impl Environments {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Where the environment of the bundle with this hash is, or will be.
    pub fn path(&self, hash: &BundleHash) -> PathBuf {
        self.dir.join(hash.to_string())
    }

    /// Gives `script` an environment with its dependencies installed,
    /// making it first if it has none that works.
    pub async fn sync(&self, uv: &Uv, script: &Script, echo_stderr: bool) -> Result<(), JobError> {
        if !script.has_environment() {
            let _lock = self.lock().await.map_err(|error| {
                JobError::new(
                    ErrorCode::Internal,
                    format!("couldn't lock {}: {error}", self.dir.display()),
                )
            })?;
            // Another process may have made it meanwhile.
            if !script.has_environment() {
                uv::create_environment(uv, script, echo_stderr).await?;
            }
        }
        uv::sync(uv, script, echo_stderr).await
    }

    /// Removes the environments of bundles that `store` no longer holds,
    /// and returns how many it removed. One it can't remove is logged and
    /// left for next time.
    pub fn prune(&self, store: &BundleStore) -> io::Result<usize> {
        let entries = match std::fs::read_dir(&self.dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            other => other?,
        };
        let lock = self.lock_file()?;
        lock.lock()?;
        let mut removed = 0;
        for entry in entries {
            let path = entry?.path();
            let Some(hash) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.parse::<BundleHash>().ok())
            else {
                continue;
            };
            if store.dir(&hash).is_dir() {
                continue;
            }
            match remove_tree(&path) {
                Ok(()) => removed += 1,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "couldn't remove an unused environment");
                }
            }
        }
        Ok(removed)
    }

    /// Waits until no other process makes or removes environments, then
    /// holds that off until the file returned is dropped. Cancel-safe.
    async fn lock(&self) -> io::Result<File> {
        let file = self.lock_file()?;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
    }

    fn lock_file(&self) -> io::Result<File> {
        std::fs::create_dir_all(&self.dir)?;
        File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(LOCK))
    }
}
