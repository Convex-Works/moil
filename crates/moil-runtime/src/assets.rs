//! The asset cache (spec §8.1, §8.4): files named by their SHA-256,
//! downloaded once, verified, and shared by every bundle.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use moil_bundle::{Asset, Manifest};
use moil_proto::{ErrorCode, JobError, JobEvent, LogLevel};

use crate::EventSink;
use crate::http::{DownloadError, Http};
use crate::locks::KeyedLocks;

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
    /// it if it isn't. Logs to `events`, if given, when it downloads.
    pub async fn fetch(
        &self,
        http: &Http,
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
        self.download(http, name, asset, &path).await
    }

    async fn download(
        &self,
        http: &Http,
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
            .prefix(".partial-")
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
            .download(&asset.url, &BTreeMap::new(), &mut file, asset.size_bytes)
            .await
            .map_err(|e| match e {
                DownloadError::TooLarge(expected) => mismatch(format!(
                    "it is larger than the {expected} bytes the manifest says"
                )),
                DownloadError::Failed(why) => failed(why),
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

/// Two assets that can't both be laid out in an attempt's `assets/`, because
/// one would have to be a file and a directory at once (`a` and `a/b`).
pub fn asset_layout_conflict(manifest: &Manifest) -> Option<(&str, &str)> {
    // In sorted order, a name's descendants come right after it, possibly
    // after names that merely share a prefix, like `a.bin` or `a-b`.
    let names: Vec<&str> = manifest.assets.keys().map(String::as_str).collect();
    names.iter().enumerate().find_map(|(i, parent)| {
        names[i + 1..]
            .iter()
            .take_while(|other| other.starts_with(parent))
            .find(|other| other.as_bytes().get(parent.len()) == Some(&b'/'))
            .map(|child| (*parent, *child))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(names: &[&str]) -> Manifest {
        let assets: serde_json::Map<String, serde_json::Value> = names
            .iter()
            .map(|name| {
                let asset =
                    serde_json::json!({"url": "https://example.com/a", "sha256": "0".repeat(64)});
                ((*name).to_owned(), asset)
            })
            .collect();
        let text = serde_json::json!({"moil": 1, "name": "x", "version": "1", "assets": assets});
        Manifest::parse(&text.to_string()).unwrap()
    }

    #[test]
    fn a_name_cant_be_both_a_file_and_a_directory() {
        let conflicting = manifest(&["whisper", "whisper-x", "whisper.bin", "whisper/model.bin"]);
        assert_eq!(
            asset_layout_conflict(&conflicting),
            Some(("whisper", "whisper/model.bin"))
        );
        let fine = manifest(&["whisper/model.bin", "whisper/vocab.txt", "whisper.bin", "w"]);
        assert_eq!(asset_layout_conflict(&fine), None);
    }
}
