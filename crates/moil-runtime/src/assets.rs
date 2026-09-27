//! The asset cache (spec §8.1, §8.4): files named by their SHA-256,
//! downloaded once, verified, and shared by every bundle.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use moil_bundle::Asset;
use moil_proto::{ErrorCode, JobError, JobEvent, LogLevel, UrlPolicy};

use crate::EventSink;
use crate::http::{Http, TransferError};
use crate::locks::KeyedLocks;

/// How the names of files still being downloaded start.
const PARTIAL: &str = ".partial-";

/// A directory of verified assets, each named by its SHA-256.
///
/// A file only ever appears under its hash once its contents have been
/// checked against it, so the cache can be shared by concurrent attempts
/// and processes, and trusted without hashing gigabytes on every run.
#[derive(Debug)]
pub(crate) struct AssetCache {
    dir: PathBuf,
    /// Held while an asset is fetched, by hash, so concurrent attempts in
    /// this process download each asset once.
    fetching: KeyedLocks<String>,
}

impl AssetCache {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            fetching: KeyedLocks::default(),
        }
    }

    /// Where the asset with this hash is, or will be, cached.
    pub fn path(&self, sha256: &str) -> PathBuf {
        self.dir.join(sha256)
    }

    /// Makes sure asset `name` is in the cache, downloading and verifying
    /// it if it isn't, from where `policy` allows. Logs to `events`, if
    /// given, when it downloads.
    pub async fn fetch(
        &self,
        http: &Http,
        policy: UrlPolicy,
        name: &str,
        asset: &Asset,
        events: Option<&EventSink>,
    ) -> Result<(), JobError> {
        let path = self.path(&asset.sha256);
        if is_cached(&path, asset) {
            return Ok(());
        }
        let _fetching = self.fetching.lock(&asset.sha256).await;
        if is_cached(&path, asset) {
            return Ok(());
        }
        if let Some(events) = events {
            let _ = events.send(JobEvent::log(
                LogLevel::Info,
                format!("downloading asset {name}"),
            ));
        }
        self.download(http, policy, name, asset, &path).await
    }

    async fn download(
        &self,
        http: &Http,
        policy: UrlPolicy,
        name: &str,
        asset: &Asset,
        path: &Path,
    ) -> Result<(), JobError> {
        let failed = |why: String| {
            JobError::new(
                ErrorCode::AssetDownload,
                format!("couldn't download asset {name}: {why}"),
            )
        };
        let mismatch = |why: String| {
            JobError::new(
                ErrorCode::AssetMismatch,
                format!("asset {name} doesn't match the manifest: {why}"),
            )
        };
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| failed(format!("couldn't create {}: {e}", self.dir.display())))?;
        // Downloaded next to its final place, so that the rename that
        // publishes it is atomic. Deleted if anything goes wrong, including
        // the attempt being dropped mid-download.
        let partial = tempfile::Builder::new()
            .prefix(PARTIAL)
            .tempfile_in(&self.dir)
            .map_err(|e| {
                failed(format!(
                    "couldn't create a file in {}: {e}",
                    self.dir.display()
                ))
            })?;
        let (file, partial) = partial.into_parts();
        let mut file = tokio::fs::File::from_std(file);
        let got = http
            .download(
                policy,
                &asset.url,
                &BTreeMap::new(),
                &mut file,
                asset.size_bytes,
            )
            .await
            .map_err(|e| match e {
                TransferError::TooLarge(expected) => mismatch(format!(
                    "it is larger than the {expected} bytes the manifest says"
                )),
                other => failed(other.to_string()),
            })?;
        if let Some(expected) = asset.size_bytes
            && got.size_bytes != expected
        {
            return Err(mismatch(format!(
                "it is {} bytes, not the {expected} the manifest says",
                got.size_bytes
            )));
        }
        if got.sha256 != asset.sha256 {
            return Err(mismatch(format!(
                "its sha256 is {}, not {}",
                got.sha256, asset.sha256
            )));
        }
        file.sync_all()
            .await
            .map_err(|e| failed(format!("couldn't write it to disk: {e}")))?;
        drop(file);
        // Read-only, since attempts see it through links: a script writing
        // to its asset would otherwise change it for every bundle.
        set_read_only(&partial).map_err(|e| failed(format!("couldn't protect it: {e}")))?;
        partial
            .persist(path)
            .map_err(|e| failed(format!("couldn't move it into the cache: {}", e.error)))?;
        Ok(())
    }

    /// Removes downloads that a crashed process left unfinished: partial
    /// files nothing has written to for `idle`. Returns how many it removed.
    pub fn remove_abandoned(&self, idle: Duration) -> io::Result<usize> {
        let entries = match std::fs::read_dir(&self.dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            other => other?,
        };
        let mut removed = 0;
        for entry in entries {
            let entry = entry?;
            let abandoned = entry.file_name().to_string_lossy().starts_with(PARTIAL)
                && entry
                    .metadata()?
                    .modified()?
                    .elapsed()
                    .is_ok_and(|age| age >= idle);
            if abandoned {
                std::fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Links every asset in `assets` into `dir` under its name, pointing at
    /// its cached file, and returns each asset's path.
    pub fn link(
        &self,
        assets: &BTreeMap<String, Asset>,
        dir: &Path,
    ) -> io::Result<BTreeMap<String, PathBuf>> {
        let mut paths = BTreeMap::new();
        for (name, asset) in assets {
            let link = dir.join(name);
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::os::unix::fs::symlink(self.path(&asset.sha256), &link)?;
            paths.insert(name.clone(), link);
        }
        Ok(paths)
    }
}

/// Whether `path` holds the asset: it exists and has the expected size.
/// Its hash was checked before it got there.
fn is_cached(path: &Path, asset: &Asset) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|m| m.is_file() && asset.size_bytes.is_none_or(|size| m.len() == size))
}

fn set_read_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_abandoned_downloads_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AssetCache::new(dir.path().to_owned());
        let write = |name: &str, age: Duration| {
            let file = std::fs::File::create(dir.path().join(name)).unwrap();
            file.set_modified(std::time::SystemTime::now() - age)
                .unwrap();
        };
        let hour = Duration::from_secs(3600);
        write(".partial-crashed", 2 * hour);
        write(".partial-downloading", Duration::ZERO);
        write(&"0".repeat(64), 2 * hour);

        assert_eq!(cache.remove_abandoned(hour).unwrap(), 1);
        let mut left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, [".partial-downloading".to_owned(), "0".repeat(64)]);
    }
}
