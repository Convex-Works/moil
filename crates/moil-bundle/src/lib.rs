//! Job bundles (spec §5): the three files a machine runs, identified by the
//! hash of their contents.
//!
//! A [`Bundle`] can only be constructed from valid contents, so holding one
//! means the files, the manifest and the hash have been checked.

mod bundle;
mod diff;
mod lock;
mod manifest;
mod regular;
mod store;

pub use bundle::{Bundle, BundleError, LoadError, hash_files};
pub use diff::{DiffLine, FileDiff, Hunk, LineKind, diff};
pub use lock::{LockedPackage, PackageSource};
pub use manifest::{Asset, GpuRequirement, Manifest, ManifestError, Resources};
pub use moil_proto::BundleHash;
pub use regular::{OpenError, open_regular};
pub use store::BundleStore;

/// The script, with PEP 723 inline metadata.
pub const SCRIPT: &str = "job.py";
/// The lockfile `uv lock --script job.py` writes.
pub const LOCKFILE: &str = "job.py.lock";
/// The bundle's metadata.
pub const MANIFEST: &str = "manifest.json";
/// Every file of a bundle, in the order they're hashed.
pub const FILE_NAMES: [&str; 3] = [SCRIPT, LOCKFILE, MANIFEST];
/// The most the three files may add up to.
pub const MAX_BUNDLE_BYTES: usize = 8 << 20;
