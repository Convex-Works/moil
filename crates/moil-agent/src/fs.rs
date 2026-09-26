//! State files: written so that a crash never leaves one half-written.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Error;

/// Permissions for files anyone on the computer may read.
pub(crate) const PUBLIC: u32 = 0o644;
/// Permissions for files only the owner may read, such as tokens.
pub(crate) const PRIVATE: u32 = 0o600;

/// Replaces `path` with `contents`: readers see either the old file or the
/// new one, never a mix. On Unix the new file gets `mode`.
pub(crate) fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let temp = dir.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
        #[cfg(not(unix))]
        let _ = mode;
        let mut file = options.open(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

/// Writes `value` to `path` as pretty JSON, atomically.
pub(crate) fn write_json(path: &Path, value: &impl Serialize, mode: u32) -> Result<(), Error> {
    let mut json = serde_json::to_vec_pretty(value).expect("state serializes to JSON");
    json.push(b'\n');
    Ok(write_atomic(path, &json, mode).map_err(|e| with_path(e, path))?)
}

/// Reads JSON from `path`, or `None` if there is no such file.
pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(with_path(e, path).into()),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| Error::Damaged {
            path: path.to_owned(),
            reason: e.to_string(),
        })
}

/// Adds the path to an I/O error, which otherwise only says what failed.
pub(crate) fn with_path(error: io::Error, path: &Path) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_writes_replace_the_file_and_leave_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state.json");
        write_json(&path, &serde_json::json!({"a": 1}), PUBLIC).unwrap();
        write_json(&path, &serde_json::json!({"a": 2}), PUBLIC).unwrap();

        let value: serde_json::Value = read_json(&path).unwrap().unwrap();
        assert_eq!(value, serde_json::json!({"a": 2}));
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["state.json"], "no temporary files left behind");
    }

    #[test]
    fn a_damaged_file_names_itself() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "{not json").unwrap();
        let err = read_json::<serde_json::Value>(&path).unwrap_err();
        assert!(matches!(err, Error::Damaged { .. }), "{err}");
        assert!(err.to_string().contains("settings.json"), "{err}");
        assert!(
            read_json::<serde_json::Value>(&dir.path().join("missing"))
                .unwrap()
                .is_none()
        );
    }
}
