use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::Path;

use moil_proto::BundleHash;
use moil_proto::http::BundleResponse;
use sha2::{Digest, Sha256};

use crate::lock::{LockedPackage, parse_lockfile};
use crate::manifest::{Manifest, ManifestError};
use crate::{FILE_NAMES, LOCKFILE, MANIFEST, MAX_BUNDLE_BYTES, SCRIPT};

/// A valid bundle and its hash.
#[derive(Clone, PartialEq, Eq)]
pub struct Bundle {
    script: String,
    lockfile: String,
    manifest_text: String,
    manifest: Manifest,
    requires_python: Option<String>,
    packages: Vec<LockedPackage>,
    hash: BundleHash,
}

/// Why contents don't make a valid bundle.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BundleError {
    #[error("the bundle has no {0}")]
    MissingFile(&'static str),
    #[error("a bundle holds only job.py, job.py.lock and manifest.json, not {0:?}")]
    UnexpectedFile(String),
    #[error("the bundle is {0} bytes; the limit is 8 MiB")]
    TooLarge(usize),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("job.py.lock isn't valid TOML: {0}")]
    Lockfile(String),
    #[error("the bundle hashes to {actual}, not the expected {expected}")]
    HashMismatch {
        expected: BundleHash,
        actual: BundleHash,
    },
}

/// Why a bundle couldn't be read from a directory.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("couldn't read {path}: {source}")]
    Io { path: String, source: io::Error },
    #[error("{0} isn't UTF-8 text")]
    NotUtf8(&'static str),
    #[error(transparent)]
    Invalid(#[from] BundleError),
}

impl Bundle {
    /// A bundle from its three files' contents.
    pub fn new(
        script: impl Into<String>,
        lockfile: impl Into<String>,
        manifest: impl Into<String>,
    ) -> Result<Self, BundleError> {
        let (script, lockfile, manifest_text) = (script.into(), lockfile.into(), manifest.into());
        let size = script.len() + lockfile.len() + manifest_text.len();
        if size > MAX_BUNDLE_BYTES {
            return Err(BundleError::TooLarge(size));
        }
        let manifest = Manifest::parse(&manifest_text)?;
        let locked = parse_lockfile(&lockfile).map_err(BundleError::Lockfile)?;
        let hash = hash_files([
            (SCRIPT, &*script),
            (LOCKFILE, &*lockfile),
            (MANIFEST, &*manifest_text),
        ]);
        Ok(Self {
            script,
            lockfile,
            manifest_text,
            manifest,
            requires_python: locked.requires_python,
            packages: locked.packages,
            hash,
        })
    }

    /// A bundle from a map of file name to contents, which must hold exactly
    /// the three bundle files.
    pub fn from_files<K, V>(files: impl IntoIterator<Item = (K, V)>) -> Result<Self, BundleError>
    where
        K: Into<String>,
        V: Into<String>,
    {
        let mut files: BTreeMap<String, String> = files
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        let mut take =
            |name: &'static str| files.remove(name).ok_or(BundleError::MissingFile(name));
        let (script, lockfile, manifest) = (take(SCRIPT)?, take(LOCKFILE)?, take(MANIFEST)?);
        if let Some(extra) = files.into_keys().next() {
            return Err(BundleError::UnexpectedFile(extra));
        }
        Self::new(script, lockfile, manifest)
    }

    /// Reads the three bundle files from `dir`. Other files in `dir` are
    /// ignored, so a bundle's source directory can hold a README or tests.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, LoadError> {
        let dir = dir.as_ref();
        let read = |name: &'static str| {
            let path = dir.join(name);
            let bytes = std::fs::read(&path).map_err(|source| LoadError::Io {
                path: path.display().to_string(),
                source,
            })?;
            String::from_utf8(bytes).map_err(|_| LoadError::NotUtf8(name))
        };
        Ok(Self::new(read(SCRIPT)?, read(LOCKFILE)?, read(MANIFEST)?)?)
    }

    /// A bundle a service sent in answer to `GET /v1/bundles/{requested}`.
    /// Fails unless the files hash to `requested`.
    pub fn from_response(
        response: BundleResponse,
        requested: &BundleHash,
    ) -> Result<Self, BundleError> {
        let bundle = Self::from_files(response.files)?;
        bundle.verify(requested)?;
        Ok(bundle)
    }

    /// Fails unless this bundle's hash is `expected`.
    pub fn verify(&self, expected: &BundleHash) -> Result<(), BundleError> {
        if self.hash == *expected {
            Ok(())
        } else {
            Err(BundleError::HashMismatch {
                expected: *expected,
                actual: self.hash,
            })
        }
    }

    /// The body of `GET /v1/bundles/{hash}` for this bundle.
    pub fn to_response(&self) -> BundleResponse {
        BundleResponse {
            hash: self.hash,
            files: self
                .files()
                .map(|(name, text)| (name.to_owned(), text.to_owned()))
                .collect(),
        }
    }

    /// Writes the three files into `dir`, creating it if needed.
    pub fn write_to(&self, dir: impl AsRef<Path>) -> io::Result<()> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        for (name, text) in self.files() {
            std::fs::write(dir.join(name), text)?;
        }
        Ok(())
    }

    pub fn hash(&self) -> BundleHash {
        self.hash
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The contents of `job.py`.
    pub fn script(&self) -> &str {
        &self.script
    }

    /// The contents of `job.py.lock`.
    pub fn lockfile(&self) -> &str {
        &self.lockfile
    }

    /// The contents of `manifest.json`, exactly as hashed.
    pub fn manifest_text(&self) -> &str {
        &self.manifest_text
    }

    /// The Python versions the lockfile was made for (its
    /// `requires-python`), such as `>=3.12`.
    pub fn requires_python(&self) -> Option<&str> {
        self.requires_python.as_deref()
    }

    /// The packages the lockfile pins.
    pub fn packages(&self) -> &[LockedPackage] {
        &self.packages
    }

    /// `(name, contents)` of each file, in hash order.
    pub fn files(&self) -> impl ExactSizeIterator<Item = (&'static str, &str)> {
        FILE_NAMES
            .into_iter()
            .zip([&*self.script, &*self.lockfile, &*self.manifest_text])
    }

    /// The contents of the named bundle file.
    pub fn file(&self, name: &str) -> Option<&str> {
        self.files().find(|(n, _)| *n == name).map(|(_, text)| text)
    }
}

impl fmt::Debug for Bundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bundle")
            .field("name", &self.manifest.name)
            .field("version", &self.manifest.version)
            .field("hash", &self.hash)
            .finish_non_exhaustive()
    }
}

/// The bundle hash of `files` (spec §5.2): the SHA-256 of their `sha256sum`
/// listing, in bytewise order of name.
pub fn hash_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> BundleHash {
    let mut files: Vec<_> = files.into_iter().collect();
    files.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut listing = Sha256::new();
    for (name, contents) in files {
        let digest = Sha256::digest(contents.as_bytes());
        let mut hex = String::with_capacity(64);
        for byte in digest.iter() {
            hex.push_str(&format!("{byte:02x}"));
        }
        listing.update(hex.as_bytes());
        listing.update(b"  ");
        listing.update(name.as_bytes());
        listing.update(b"\n");
    }
    BundleHash::from_bytes(listing.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;

    fn vectors_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors")
    }

    fn vectors() -> Value {
        serde_json::from_str(&std::fs::read_to_string(vectors_dir().join("bundles.json")).unwrap())
            .unwrap()
    }

    #[test]
    fn valid_vectors_hash_as_specified() {
        for case in vectors()["valid"].as_array().unwrap() {
            let dir = vectors_dir()
                .join("bundles")
                .join(case["dir"].as_str().unwrap());
            let bundle = Bundle::from_dir(&dir).unwrap();
            assert_eq!(
                bundle.hash().to_string(),
                case["hash"].as_str().unwrap(),
                "{}",
                dir.display()
            );
        }
    }

    #[test]
    fn invalid_vectors_are_rejected() {
        let cases = vectors()["invalid"].as_array().unwrap().clone();
        assert!(!cases.is_empty());
        for case in cases {
            let base = Bundle::from_dir(
                vectors_dir()
                    .join("bundles")
                    .join(case["base"].as_str().unwrap()),
            )
            .unwrap();
            let mut files: BTreeMap<String, String> = base
                .files()
                .map(|(n, t)| (n.to_owned(), t.to_owned()))
                .collect();
            for (name, text) in case["set"].as_object().into_iter().flatten() {
                files.insert(name.clone(), text.as_str().unwrap().to_owned());
            }
            for name in case["remove"].as_array().into_iter().flatten() {
                files.remove(name.as_str().unwrap());
            }
            let result = Bundle::from_files(files);
            assert!(
                result.is_err(),
                "{}: expected rejection, got {:?}",
                case["case"],
                result.unwrap()
            );
        }
    }

    #[test]
    fn crlf_and_unicode_are_hashed_as_raw_bytes() {
        let bundle = Bundle::from_dir(vectors_dir().join("bundles/crlf-unicode")).unwrap();
        assert!(bundle.script().contains("\r\n"));
        let normalized = Bundle::new(
            bundle.script().replace("\r\n", "\n"),
            bundle.lockfile(),
            bundle.manifest_text(),
        )
        .unwrap();
        assert_ne!(normalized.hash(), bundle.hash());
    }

    #[test]
    fn response_must_match_the_requested_hash() {
        let bundle = Bundle::from_dir(vectors_dir().join("bundles/minimal")).unwrap();
        let other = Bundle::from_dir(vectors_dir().join("bundles/crlf-unicode")).unwrap();

        let ok = Bundle::from_response(bundle.to_response(), &bundle.hash()).unwrap();
        assert_eq!(ok, bundle);

        // A service that answers with different files, even under the right
        // `hash` field, is caught: only the contents count.
        let mut swapped = other.to_response();
        swapped.hash = bundle.hash();
        let err = Bundle::from_response(swapped, &bundle.hash()).unwrap_err();
        assert_eq!(
            err,
            BundleError::HashMismatch {
                expected: bundle.hash(),
                actual: other.hash()
            }
        );
    }

    #[test]
    fn write_to_then_from_dir_round_trips() {
        let bundle = Bundle::from_dir(vectors_dir().join("bundles/crlf-unicode")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        bundle.write_to(dir.path().join("b")).unwrap();
        assert_eq!(Bundle::from_dir(dir.path().join("b")).unwrap(), bundle);
    }

    #[test]
    fn from_dir_reports_what_is_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let err = Bundle::from_dir(dir.path()).unwrap_err();
        assert!(matches!(err, LoadError::Io { .. }), "{err}");

        let bundle = Bundle::from_dir(vectors_dir().join("bundles/minimal")).unwrap();
        bundle.write_to(dir.path()).unwrap();
        std::fs::write(dir.path().join("job.py"), [0xff, 0xfe]).unwrap();
        assert!(matches!(
            Bundle::from_dir(dir.path()),
            Err(LoadError::NotUtf8("job.py"))
        ));
    }

    #[test]
    fn oversized_bundles_are_rejected() {
        let bundle = Bundle::from_dir(vectors_dir().join("bundles/minimal")).unwrap();
        let huge = format!("{}#{}\n", bundle.script(), "x".repeat(MAX_BUNDLE_BYTES));
        assert!(matches!(
            Bundle::new(huge, bundle.lockfile(), bundle.manifest_text()),
            Err(BundleError::TooLarge(_))
        ));
    }
}
