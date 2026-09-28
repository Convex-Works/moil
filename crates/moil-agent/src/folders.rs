//! Service folders (spec/machine.md §1.2): what moil makes of what the
//! owner, git and moil itself wrote there, and what's wrong with it.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use moil_bundle::{Bundle, BundleError, LoadError};
use moil_proto::BundleHash;
use moil_proto::ids::is_bundle_name;

use crate::fs::{read_strict_json, with_path};
use crate::report::Problem;
use crate::services::{Approval, Pairing, Service, ServiceFile, ServiceId};
use crate::{Error, Home};

/// What the home's `services/` folder holds.
pub(crate) struct Scan {
    /// Every service the home knows, by ID.
    pub services: Vec<Service>,
    pub problems: Vec<Problem>,
}

/// Reads every service folder. Fails only if `services/` can't be listed;
/// what's wrong inside it is a problem.
pub(crate) fn scan(home: &Home) -> Result<Scan, Error> {
    let dir = home.services_dir();
    let mut scan = Scan {
        services: Vec::new(),
        problems: Vec::new(),
    };
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(scan),
        Err(e) => return Err(with_path(e, &dir).into()),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| with_path(e, &dir))?;
        names.push(entry.file_name());
    }
    names.sort();
    for name in names {
        let text = name.to_string_lossy();
        // Such as .git or .DS_Store.
        if text.starts_with('.') {
            continue;
        }
        let relative = Path::new("services").join(&name);
        let is_dir = dir.join(&name).is_dir();
        match name.to_str().map(ServiceId::new) {
            Some(Ok(id)) if is_dir => {
                let folder = read(home, &id);
                scan.services.extend(folder.service);
                scan.problems.extend(folder.problems);
            }
            _ if !is_dir => scan.problems.push(Problem {
                path: relative,
                message: "moil keeps only service folders in services/, so it ignores this file. \
                          Move it elsewhere."
                    .into(),
            }),
            _ => scan.problems.push(Problem {
                path: relative,
                message: format!(
                    "moil ignores this folder: a service's folder is named by its ID, and {text:?} \
                     isn't one. IDs are 1 to {} lowercase letters, digits, dots and hyphens, \
                     starting and ending with a letter or digit. Rename the folder.",
                    ServiceId::MAX_LEN
                ),
            }),
        }
    }
    Ok(scan)
}

/// A service folder, as moil reads it.
pub(crate) struct Folder {
    /// The service, if the folder has a valid `service.json`.
    pub service: Option<Service>,
    pub problems: Vec<Problem>,
}

/// Reads the folder of service `id`: its `service.json`, which approvals
/// count, whether this machine is paired with it, and what's wrong.
pub(crate) fn read(home: &Home, id: &ServiceId) -> Folder {
    let mut problems = Vec::new();
    let relative = Path::new("services").join(id.as_str());
    let file = match read_strict_json::<ServiceFile>(&home.service_file(id)) {
        Ok(Some(file)) => match file.check() {
            Ok(()) => Some(file),
            Err(reason) => {
                problems.push(invalid_service_file(&relative, &reason));
                None
            }
        },
        Ok(None) => {
            if home.service_dir(id).is_dir() {
                problems.push(Problem {
                    path: relative.clone(),
                    message: "This folder has no service.json, so moil ignores it. Pair with the \
                              service to make one, or remove the folder."
                        .into(),
                });
            }
            None
        }
        Err(Error::Invalid { reason, .. }) => {
            problems.push(invalid_service_file(&relative, &reason));
            None
        }
        Err(e) => {
            problems.push(Problem {
                path: relative.join("service.json"),
                message: format!("moil can't read this file ({e}), so it ignores the service."),
            });
            None
        }
    };
    let Some(file) = file else {
        return Folder {
            service: None,
            problems,
        };
    };

    let mut approvals = Vec::new();
    let mut seen = HashSet::new();
    for approval in &file.approvals {
        if !seen.insert(approval.hash) {
            problems.push(Problem {
                path: relative.join("service.json"),
                message: format!(
                    "This file approves {} more than once; moil counts the first approval. \
                     Remove the others.",
                    approval.hash
                ),
            });
            continue;
        }
        match check(home, id, approval) {
            Ok(_) => approvals.push(approval.clone()),
            Err(not_counted) => problems.push(not_counted.problem(id, approval)),
        }
    }
    problems.extend(stray_bundle_problems(home, id, &file.approvals));

    let pairing = match read_strict_json::<Pairing>(&home.pairing_file(id)) {
        Ok(Some(pairing)) => match pairing.check() {
            Ok(()) => Some(pairing),
            Err(reason) => {
                problems.push(invalid_pairing_file(&relative, id, &reason));
                None
            }
        },
        Ok(None) => None,
        Err(Error::Invalid { reason, .. }) => {
            problems.push(invalid_pairing_file(&relative, id, &reason));
            None
        }
        Err(e) => {
            problems.push(Problem {
                path: relative.join("pairing.json"),
                message: format!(
                    "moil can't read this file ({e}), so this machine isn't paired with the \
                     service. Pair with it again."
                ),
            });
            None
        }
    };
    // The token was issued for the base URL the machine paired with, and
    // is never sent anywhere else.
    let pairing = pairing.filter(|pairing| {
        let same = pairing.base_url == file.base_url;
        if !same {
            problems.push(Problem {
                path: relative.join("pairing.json"),
                message: format!(
                    "This machine paired with {}, but service.json names {}, so moil connects to \
                     neither. To use {}, pair with it; to go back, set base_url in service.json \
                     to {}.",
                    pairing.base_url, file.base_url, file.base_url, pairing.base_url
                ),
            });
        }
        same
    });

    // The hooks track adds each hook's problems (spec/machine.md §2.1)
    // here.

    Folder {
        service: Some(Service {
            id: id.clone(),
            base_url: file.base_url,
            name: file.name,
            approvals,
            pairing,
        }),
        problems,
    }
}

fn invalid_service_file(folder: &Path, reason: &str) -> Problem {
    Problem {
        path: folder.join("service.json"),
        message: format!(
            "This file isn't valid ({reason}), so moil ignores the service. Fix it, or remove the \
             folder."
        ),
    }
}

fn invalid_pairing_file(folder: &Path, id: &ServiceId, reason: &str) -> Problem {
    Problem {
        path: folder.join("pairing.json"),
        message: format!(
            "This file isn't valid ({reason}), so this machine isn't paired with the service. \
             Pair with it again with `moil pair {id}`."
        ),
    }
}

/// Why an approval doesn't count (spec/machine.md §1.2).
#[derive(Debug)]
pub(crate) enum NotCounted {
    /// Its name can't be a bundle's, so it names no folder.
    Unnamed,
    /// Its folder, or a file in it, isn't there.
    Missing,
    /// The files aren't a valid bundle, or don't hash to the approval's
    /// hash.
    Invalid(LoadError),
    /// The manifest names another bundle, or another version of it.
    Mismatch { name: String, version: String },
}

impl NotCounted {
    /// The problem to report, for the owner to fix.
    fn problem(&self, id: &ServiceId, approval: &Approval) -> Problem {
        let (name, hash) = (&approval.name, approval.hash);
        let folder = Path::new("services").join(id.as_str());
        let why = match self {
            Self::Unnamed => {
                return Problem {
                    path: folder.join("service.json"),
                    message: format!(
                        "moil ignores the approval of {hash}: {name:?} isn't a bundle's name. \
                         Correct it, or remove the approval."
                    ),
                };
            }
            Self::Missing => format!(
                "the bundle's files aren't all here. Approve it again with `moil approve {id} \
                 {hash}`, or remove the approval from service.json"
            ),
            Self::Invalid(LoadError::Invalid(BundleError::HashMismatch { .. })) => format!(
                "these files don't hash to {hash}: they changed after it was approved. Restore \
                 them (with git, say), or approve the bundle again"
            ),
            Self::Invalid(e) => format!(
                "these files aren't a valid bundle ({e}). Restore them (with git, say), or \
                 approve the bundle again"
            ),
            Self::Mismatch { name, version } => format!(
                "the bundle here is {name:?} {version:?}. Correct the approval in service.json, \
                 or remove it"
            ),
        };
        Problem {
            path: folder.join("bundles").join(name).join(hash.to_string()),
            message: format!(
                "moil ignores the approval of {name:?} {:?}: {why}.",
                approval.version
            ),
        }
    }
}

impl fmt::Display for NotCounted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unnamed => f.write_str("its name can't be a bundle's"),
            Self::Missing => f.write_str("the bundle's files aren't all there"),
            Self::Invalid(e) => write!(f, "{e}"),
            Self::Mismatch { name, version } => {
                write!(f, "the bundle there is {name:?} {version:?}")
            }
        }
    }
}

/// The bundle `approval` names, from its folder, if the approval counts:
/// the folder holds the three files, they hash to the approval's hash,
/// and the manifest's name and version are the approval's.
pub(crate) fn check(
    home: &Home,
    id: &ServiceId,
    approval: &Approval,
) -> Result<Bundle, NotCounted> {
    if !is_bundle_name(&approval.name) {
        return Err(NotCounted::Unnamed);
    }
    let dir = home.approved_bundle_dir(id, &approval.name, &approval.hash);
    let bundle = match Bundle::from_dir(&dir) {
        Ok(bundle) => bundle,
        Err(LoadError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Err(NotCounted::Missing);
        }
        Err(e) => return Err(NotCounted::Invalid(e)),
    };
    bundle
        .verify(&approval.hash)
        .map_err(|e| NotCounted::Invalid(LoadError::Invalid(e)))?;
    let manifest = bundle.manifest();
    if manifest.name != approval.name || manifest.version != approval.version {
        return Err(NotCounted::Mismatch {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
        });
    }
    Ok(bundle)
}

/// What's wrong under a service's `bundles/`, besides the approvals that
/// don't count, which [`read`] reports: anything that isn't a folder
/// `<name>/<hash>/` holding that bundle.
fn stray_bundle_problems(home: &Home, id: &ServiceId, approvals: &[Approval]) -> Vec<Problem> {
    let approved: HashSet<(&str, String)> = approvals
        .iter()
        .map(|a| (a.name.as_str(), a.hash.to_string()))
        .collect();
    let dir = home.approved_bundles_dir(id);
    let relative = Path::new("services").join(id.as_str()).join("bundles");
    let mut problems = Vec::new();
    let not_a_bundle = |path: PathBuf, why: &str| Problem {
        path,
        message: format!(
            "moil keeps only approved bundles in bundles/<name>/<hash>/, and {why}. Remove it."
        ),
    };
    for name in visible_entries(&dir) {
        let path = dir.join(&name);
        if !path.is_dir() || !is_bundle_name(&name) {
            problems.push(not_a_bundle(
                relative.join(&name),
                "this isn't a bundle's folder",
            ));
            continue;
        }
        for hash in visible_entries(&path) {
            if approved.contains(&(name.as_str(), hash.clone())) {
                continue;
            }
            let bundle_dir = path.join(&hash);
            let intact = hash.parse::<BundleHash>().is_ok_and(|hash| {
                Bundle::from_dir(&bundle_dir)
                    .is_ok_and(|b| b.hash() == hash && b.manifest().name == name)
            });
            if !intact {
                problems.push(not_a_bundle(
                    relative.join(&name).join(&hash),
                    "this folder doesn't hold the bundle it's named for",
                ));
            }
        }
    }
    problems
}

/// The names in `dir` that don't start with a dot, sorted; none if it
/// can't be read.
fn visible_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// Writes `bundle`'s three files, read-only, into `dir`, unless it already
/// holds them. Never leaves a partial folder: the files are written aside,
/// then moved into place.
pub(crate) fn write_bundle(dir: &Path, bundle: &Bundle) -> io::Result<()> {
    if Bundle::from_dir(dir).is_ok_and(|stored| stored.hash() == bundle.hash()) {
        return Ok(());
    }
    let parent = dir.parent().expect("a bundle's folder is in its name's");
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".{}.{}.tmp", bundle.hash(), std::process::id()));
    let _ = remove_bundle(&staging);
    fs::create_dir(&staging)?;
    let written = (|| {
        for (name, text) in bundle.files() {
            write_read_only(&staging.join(name), text.as_bytes())?;
        }
        remove_bundle(dir)?;
        fs::rename(&staging, dir)
    })();
    if written.is_err() {
        let _ = remove_bundle(&staging);
    }
    written
}

fn write_read_only(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o444);
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    #[cfg(not(unix))]
    {
        let mut permissions = file.metadata()?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

/// Removes a bundle's folder, read-only files and all, if it's there.
pub(crate) fn remove_bundle(dir: &Path) -> io::Result<()> {
    // Deleting a file takes write access to its folder, which the owner
    // may have taken away along with the files'.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::symlink_metadata(dir) {
            Ok(metadata) if metadata.is_dir() => {
                let mode = metadata.permissions().mode() | 0o700;
                fs::set_permissions(dir, fs::Permissions::from_mode(mode))?;
            }
            Ok(_) => return fs::remove_file(dir),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    // Elsewhere, read-only files can't be deleted at all.
    #[cfg(not(unix))]
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let mut permissions = entry.metadata()?.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        let _ = fs::set_permissions(entry.path(), permissions);
    }
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Removes `dir` if it's empty, as a bundle name's folder is once its
/// last approved version goes.
pub(crate) fn remove_if_empty(dir: &Path) {
    // Fails, harmlessly, unless the folder is empty.
    let _ = fs::remove_dir(dir);
}
