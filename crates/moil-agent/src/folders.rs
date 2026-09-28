//! Service folders (spec/machine.md §1.2): what moil makes of what the
//! owner, git and moil itself wrote there, the owner's hooks included
//! (§2), and what's wrong with it.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{FileExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use moil_bundle::{Bundle, BundleError, LoadError};
use moil_proto::ids::is_bundle_name;
use serde::de::DeserializeOwned;

use crate::fs::{read_strict_json, visible_names, with_path};
use crate::report::{HookListing, HookReport, Problem};
use crate::services::{Approval, Pairing, Service, ServiceFile, ServiceId};
use crate::{Error, Home};

/// What the home's `services/` folder holds.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    /// Every service the home knows, by ID.
    pub services: Vec<Service>,
    /// Verified copies of the bundles that each service's approvals that
    /// count name, oldest approval first.
    pub bundles: BTreeMap<ServiceId, Vec<Bundle>>,
    /// The service folders that have a `pairing.json`, whether moil can
    /// use it or not.
    pub pairing_files: BTreeSet<ServiceId>,
    /// The files moil couldn't use, although they're there.
    pub unusable: Vec<Unusable>,
    /// What's wrong, the unusable files included.
    pub problems: Vec<Problem>,
}

/// Something in the home that is there, but that moil can't use: it can't
/// be read, or it isn't valid. A running agent goes on with what it read
/// from it before (spec/machine.md §1.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unusable {
    /// `services/`, which can't be listed.
    Services,
    /// A service's `service.json`.
    ServiceFile(ServiceId),
    /// A service's `pairing.json`.
    PairingFile(ServiceId),
}

/// Reads every service folder. Fails only if `services/` can't be listed;
/// what's wrong inside it is a problem.
pub(crate) fn scan(home: &Home) -> Result<Scan, Error> {
    let dir = home.services_dir();
    let mut scan = Scan::default();
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
                if folder.has_pairing_file {
                    scan.pairing_files.insert(id.clone());
                }
                if let Some(service) = folder.service {
                    scan.bundles.insert(id, folder.bundles);
                    scan.services.push(service);
                }
                scan.unusable.extend(folder.unusable);
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
    /// Verified copies of the bundles its approvals that count name.
    pub bundles: Vec<Bundle>,
    /// Whether the folder has a `pairing.json`, whether moil can use it or
    /// not.
    pub has_pairing_file: bool,
    /// Its `service.json` or `pairing.json`, if either is there but moil
    /// can't use it.
    pub unusable: Vec<Unusable>,
    pub problems: Vec<Problem>,
}

/// Reads the folder of service `id`: its `service.json`, which approvals
/// count, whether this machine is paired with it, and what's wrong.
pub(crate) fn read(home: &Home, id: &ServiceId) -> Folder {
    let mut folder = Folder {
        service: None,
        bundles: Vec::new(),
        has_pairing_file: fs::symlink_metadata(home.pairing_file(id)).is_ok(),
        unusable: Vec::new(),
        problems: Vec::new(),
    };
    let relative = Path::new("services").join(id.as_str());
    let file = match read_valid(&home.service_file(id), ServiceFile::check) {
        Ok(Some(file)) => file,
        Ok(None) => {
            if home.service_dir(id).is_dir() {
                folder.problems.push(Problem {
                    path: relative.clone(),
                    message: "This folder has no service.json, so moil ignores it. Pair with the \
                              service to make one, or remove the folder."
                        .into(),
                });
            }
            return folder;
        }
        Err(what) => {
            folder.unusable.push(Unusable::ServiceFile(id.clone()));
            folder.problems.push(Problem {
                path: relative.join("service.json"),
                message: format!("{what}. Fix it, or remove the folder."),
            });
            return folder;
        }
    };

    let mut approvals = Vec::new();
    let mut seen = HashSet::new();
    for approval in &file.approvals {
        if !seen.insert(approval.hash) {
            folder.problems.push(Problem {
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
            Ok(bundle) => {
                approvals.push(approval.clone());
                folder.bundles.push(bundle);
            }
            Err(not_counted) => folder.problems.push(not_counted.problem(id, approval)),
        }
    }
    folder
        .problems
        .extend(stray_bundle_problems(home, id, &file.approvals));

    let pairing = read_valid(&home.pairing_file(id), Pairing::check).unwrap_or_else(|what| {
        folder.unusable.push(Unusable::PairingFile(id.clone()));
        folder.problems.push(Problem {
            path: relative.join("pairing.json"),
            message: format!(
                "{what}. Pair with the service again with `moil pair {id}` to write it anew."
            ),
        });
        None
    });
    // Both files are plain files, which git can change, so neither says
    // where the token goes: only the URL kept with the token does.
    let pairing = pairing.filter(|pairing| {
        let same = pairing.base_url == file.base_url;
        if !same {
            folder.problems.push(Problem {
                path: relative.join("pairing.json"),
                message: format!(
                    "This file names {}, but service.json names {}, so this machine isn't paired \
                     with the service and moil doesn't connect to it. To use {}, pair with it \
                     with `moil pair {id}`. moil sends this machine's token only to the address \
                     it was issued for, whatever these files say.",
                    pairing.base_url, file.base_url, file.base_url
                ),
            });
        }
        same
    });

    folder.service = Some(Service {
        id: id.clone(),
        base_url: file.base_url,
        name: file.name,
        approvals,
        pairing,
    });
    folder
}

/// The file at `path`, if it's there and valid by serde and `check`, or
/// why moil can't use it, as a sentence to go on.
fn read_valid<T: DeserializeOwned>(
    path: &Path,
    check: impl FnOnce(&T) -> Result<(), String>,
) -> Result<Option<T>, String> {
    let invalid = |reason| format!("This file isn't valid ({reason})");
    match read_strict_json::<T>(path) {
        Ok(Some(value)) => check(&value).map(|()| Some(value)).map_err(invalid),
        Ok(None) => Ok(None),
        Err(Error::Invalid { reason, .. }) => Err(invalid(reason)),
        Err(e) => Err(format!("moil can't read this file ({e})")),
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
/// don't count, which [`read`] reports: anything but the folders
/// `<name>/<hash>/` that its approvals name. What's in those is for
/// [`check`] to say.
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
    for name in visible_names(&dir).unwrap_or_default() {
        let path = dir.join(&name);
        if !path.is_dir() || !is_bundle_name(&name) {
            problems.push(not_a_bundle(
                relative.join(&name),
                "this isn't a bundle's folder",
            ));
            continue;
        }
        for hash in visible_names(&path).unwrap_or_default() {
            if !approved.contains(&(name.as_str(), hash.clone())) {
                problems.push(not_a_bundle(
                    relative.join(&name).join(&hash),
                    "no approval in service.json names this folder",
                ));
            }
        }
    }
    problems
}

/// A home's hooks, and what's wrong with them: for `moil hooks`, `moil
/// status` and the app.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookScan {
    /// Every hook, by service and then bundle, whether or not moil can run
    /// it.
    pub hooks: Vec<HookListing>,
    /// One for each hook moil can't run, for each file among a service's
    /// hooks that can't be one, and for each `hooks/` moil can't list.
    pub problems: Vec<Problem>,
}

/// Looks for hooks in every service folder of `home` (spec/machine.md §2).
/// Hidden files are passed over.
pub fn scan_hooks(home: &Home) -> HookScan {
    let mut scan = HookScan::default();
    let problem = |path: &Path, message: String| Problem {
        path: path.strip_prefix(home.root()).unwrap_or(path).to_owned(),
        message,
    };
    // What's wrong with services/ itself, `scan` reports.
    let ids = visible_names(&home.services_dir()).unwrap_or_default();
    for id in ids
        .into_iter()
        .filter_map(|name| ServiceId::new(name).ok())
        .filter(|id| home.service_dir(id).is_dir())
    {
        let hooks_dir = home.service_dir(&id).join("hooks");
        let names = match visible_names(&hooks_dir) {
            Ok(names) => names,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) if e.kind() == io::ErrorKind::NotADirectory => {
                let message = "moil looks for this service's hooks in a folder named hooks, but \
                               this is a file";
                scan.problems.push(problem(&hooks_dir, message.into()));
                continue;
            }
            Err(e) => {
                let message = format!(
                    "moil can't list the hooks in this folder ({e}). Make it readable, with \
                     chmod u+rx, say."
                );
                scan.problems.push(problem(&hooks_dir, message));
                continue;
            }
        };
        for name in names {
            if !is_bundle_name(&name) {
                let message = "moil never runs this file: a hook is named exactly after its \
                               bundle, in lowercase letters, digits and hyphens, such as \
                               hooks/transcribe";
                scan.problems
                    .push(problem(&hooks_dir.join(&name), message.into()));
                continue;
            }
            let path = home.hook(&id, &name);
            let runnable = runnable(&path);
            if let Err(why) = &runnable {
                let message = format!("moil can't run this hook: {why}");
                scan.problems.push(problem(&path, message));
            }
            scan.hooks.push(HookListing {
                service: id.clone(),
                hook: HookReport {
                    log: home.hook_log(&id, &name),
                    bundle: name,
                    path,
                    runnable: runnable.is_ok(),
                },
            });
        }
    }
    scan
}

/// Why moil can't run what is at a hook's path (spec/machine.md §2.1).
#[derive(Debug)]
pub(crate) enum Unrunnable {
    Folder,
    BrokenLink,
    NotAFile,
    NotExecutable,
    NoInterpreter,
    Unreadable(io::Error),
}

impl fmt::Display for Unrunnable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Folder => f.write_str("it's a folder, not a file"),
            Self::BrokenLink => f.write_str("it's a symbolic link to a file that doesn't exist"),
            Self::NotAFile => f.write_str("it isn't a regular file"),
            Self::NotExecutable => f.write_str("it isn't executable. Run chmod +x on it."),
            Self::NoInterpreter => f.write_str(
                "it doesn't start with a #! line, such as #!/bin/sh, and isn't a program",
            ),
            Self::Unreadable(error) => write!(f, "moil can't look at it: {error}"),
        }
    }
}

/// Whether moil can run the hook at `path`: a regular file, or a symbolic
/// link to one, that its owner can execute, and that says what runs it.
pub(crate) fn runnable(path: &Path) -> Result<(), Unrunnable> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound && fs::symlink_metadata(path).is_ok() => {
            return Err(Unrunnable::BrokenLink);
        }
        Err(e) => return Err(Unrunnable::Unreadable(e)),
    };
    if metadata.is_dir() {
        Err(Unrunnable::Folder)
    } else if !metadata.is_file() {
        Err(Unrunnable::NotAFile)
    } else if metadata.permissions().mode() & 0o100 == 0 {
        Err(Unrunnable::NotExecutable)
    } else if !is_executable_format(path) {
        Err(Unrunnable::NoInterpreter)
    } else {
        Ok(())
    }
}

/// Whether the file at `path` starts as one the system can execute does:
/// with `#!`, or as a Mach-O or ELF program. libc runs any other file with
/// `/bin/sh` instead, but a hook's file must say what runs it
/// (spec/machine.md §2.1). A file moil can't read is left to the system.
fn is_executable_format(path: &Path) -> bool {
    const PROGRAMS: [[u8; 4]; 9] = [
        *b"\x7fELF",
        // Mach-O, 32 and 64 bits, either byte order.
        [0xfe, 0xed, 0xfa, 0xce],
        [0xce, 0xfa, 0xed, 0xfe],
        [0xfe, 0xed, 0xfa, 0xcf],
        [0xcf, 0xfa, 0xed, 0xfe],
        // Universal binaries.
        [0xca, 0xfe, 0xba, 0xbe],
        [0xbe, 0xba, 0xfe, 0xca],
        [0xca, 0xfe, 0xba, 0xbf],
        [0xbf, 0xba, 0xfe, 0xca],
    ];
    let mut start = [0; 4];
    let read = File::open(path).and_then(|file| {
        let mut read = 0;
        while read < start.len() {
            match file.read_at(&mut start[read..], read as u64)? {
                0 => break,
                n => read += n,
            }
        }
        Ok(read)
    });
    match read {
        Ok(read) => start[..read].starts_with(b"#!") || PROGRAMS.contains(&start),
        Err(_) => true,
    }
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
