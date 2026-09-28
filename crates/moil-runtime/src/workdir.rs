//! An attempt's working directory (spec §8.2).

use std::io;
use std::path::{Path, PathBuf};

use moil_proto::{FileName, JobId};

use crate::sys;

/// A fresh working directory for one attempt:
///
/// ```text
/// <work dir>/
///   in/       inputs, downloaded under their names
///   out/      where the script writes outputs
///   assets/   links to the attempt's assets, under their names
///   tmp/      TMPDIR for the script
/// ```
#[derive(Debug)]
pub(crate) struct WorkDir {
    root: PathBuf,
}

impl WorkDir {
    /// Where attempt `attempt` of job `job_id` works: `<jobs_dir>/<job_id>.<attempt>`.
    pub fn path(jobs_dir: &Path, job_id: &JobId, attempt: u32) -> PathBuf {
        jobs_dir.join(format!("{job_id}.{attempt}"))
    }

    /// Creates an empty working directory at `path`, replacing anything
    /// that was there.
    pub fn create(path: &Path) -> io::Result<Self> {
        remove_tree(path)?;
        for sub in ["in", "out", "assets", "tmp"] {
            std::fs::create_dir_all(path.join(sub))?;
        }
        // The script is promised absolute paths.
        let root = path.canonicalize()?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn inputs(&self) -> PathBuf {
        self.root.join("in")
    }

    pub fn outputs(&self) -> PathBuf {
        self.root.join("out")
    }

    pub fn assets(&self) -> PathBuf {
        self.root.join("assets")
    }

    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }
}

/// Opens output `name` for uploading, if it is a regular file right in
/// `out_dir`, reached through no symbolic link: a script that extracts
/// an archive from an input could otherwise be tricked into uploading any
/// file of the owner's. The error says why not, as in "out/a.txt isn't a
/// regular file".
pub(crate) fn open_output(out_dir: &Path, name: &FileName) -> Result<std::fs::File, String> {
    let path = out_dir.join(name.as_str());
    let is_link =
        |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    let why_not = |error: io::Error| {
        if is_link(out_dir) {
            "out/ is a symbolic link; outputs must be regular files in out/".to_owned()
        } else if is_link(&path) {
            format!("out/{name} is a symbolic link; outputs must be regular files in out/")
        } else if error.kind() == io::ErrorKind::NotFound {
            format!("the script didn't write out/{name}")
        } else {
            format!("out/{name} couldn't be opened: {error}")
        }
    };
    let file = sys::open_in(out_dir, name.as_str()).map_err(why_not)?;
    match file.metadata() {
        Ok(metadata) if metadata.is_file() => Ok(file),
        Ok(_) => Err(format!("out/{name} isn't a regular file")),
        Err(error) => Err(format!("out/{name} couldn't be read: {error}")),
    }
}

/// Removes the directory tree at `path`, if there is one, even if a script
/// made parts of it read-only. Never follows symbolic links, so linked
/// assets stay in the cache.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            make_writable(path)?;
            std::fs::remove_dir_all(path)
        }
        other => other,
    }
}

fn make_writable(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir() {
        return Ok(());
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    for entry in std::fs::read_dir(dir)? {
        make_writable(&entry?.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_trees_are_removed_but_links_are_not_followed() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let precious = root.path().join("cache.bin");
        std::fs::write(&precious, "keep me").unwrap();

        let work = WorkDir::create(&root.path().join("j.1")).unwrap();
        std::fs::write(work.outputs().join("x"), "x").unwrap();
        std::os::unix::fs::symlink(&precious, work.assets().join("model.bin")).unwrap();
        std::fs::set_permissions(work.outputs(), std::fs::Permissions::from_mode(0o500)).unwrap();

        remove_tree(work.root()).unwrap();
        assert!(!work.root().exists());
        assert_eq!(std::fs::read_to_string(&precious).unwrap(), "keep me");
        remove_tree(work.root()).unwrap();
    }
}
