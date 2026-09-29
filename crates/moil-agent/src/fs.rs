//! Files and folders: state files written so that a crash never leaves one
//! half-written, links replaced in one step, private folders, and the names
//! the owner keeps in a folder.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use moil_bundle::{OpenError, open_regular};
use serde::de::{DeserializeOwned, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

use crate::Error;

/// Permissions for files anyone on the computer may read.
pub(crate) const PUBLIC: u32 = 0o644;
/// Permissions for files only the owner may read, such as tokens.
pub(crate) const PRIVATE: u32 = 0o600;

/// Replaces `path` with `contents`: readers see either the old file or the
/// new one, never a mix. On Unix the new file gets `mode`.
pub(crate) fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let temp = staging(path);
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

/// Makes `link` a symbolic link to `original`, replacing the file or link
/// at `link`, if any, in one step: whoever looks at `link` finds what was
/// there or the new link, never nothing.
#[cfg(unix)]
pub fn replace_symlink(original: &Path, link: &Path) -> io::Result<()> {
    let staging = staging(link);
    let _ = fs::remove_file(&staging);
    std::os::unix::fs::symlink(original, &staging)?;
    fs::rename(&staging, link).inspect_err(|_| {
        let _ = fs::remove_file(&staging);
    })
}

/// A hidden name beside `path` that no other writer, in this process or
/// another, picks: where to make what then replaces `path` by a rename.
fn staging(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .unwrap_or("file".as_ref())
        .to_string_lossy();
    path.with_file_name(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Creates `dir`, and any missing parents, readable only by the owner, and
/// takes other users' access to it if it already exists.
pub(crate) fn create_private(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let mode = fs::metadata(dir)?.permissions().mode();
        if mode & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(mode & !0o077))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    fs::create_dir_all(dir)
}

/// The names in `dir` that don't start with a dot, sorted: what the owner
/// keeps there, without the likes of `.git` and `.DS_Store`. Names that
/// aren't UTF-8 are left out, since none of moil's are.
pub(crate) fn visible_names(dir: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        if let Ok(name) = entry?.file_name().into_string()
            && !name.starts_with('.')
        {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Writes `value` to `path` as pretty JSON, atomically.
pub(crate) fn write_json(path: &Path, value: &impl Serialize, mode: u32) -> Result<(), Error> {
    let mut json = serde_json::to_vec_pretty(value).expect("state serializes to JSON");
    json.push(b'\n');
    Ok(write_atomic(path, &json, mode).map_err(|e| with_path(e, path))?)
}

/// The most moil reads of one of the home's JSON files, 1 MiB: far more
/// than any of them holds.
const MAX_JSON_BYTES: u64 = 1 << 20;

/// Reads JSON from `path`, or `None` if there is no such file. It must be
/// a regular file of at most 1 MiB, as [`read_small`] reads it.
pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    let damaged = |reason| Error::Damaged {
        path: path.to_owned(),
        reason,
    };
    let bytes = match read_small(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(Unreadable::Io(e)) => return Err(e.into()),
        Err(Unreadable::Refused(reason)) => return Err(damaged(reason)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| damaged(e.to_string()))
}

/// Reads a file the owner may edit (spec/machine.md §1), or `None` if there
/// is no such file. Such files are strict, like manifests: a file that
/// isn't one JSON object, or that has unknown fields (`T` denies them) or
/// duplicate keys, fails with [`Error::Invalid`], saying why, so that a
/// typo is reported rather than ignored. So does one that isn't a regular
/// file of at most 1 MiB, as [`read_small`] reads it.
pub(crate) fn read_strict_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    let invalid = |reason| Error::Invalid {
        path: path.to_owned(),
        reason,
    };
    let bytes = match read_small(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(Unreadable::Io(e)) => return Err(e.into()),
        Err(Unreadable::Refused(reason)) => return Err(invalid(reason)),
    };
    parse_strict(&bytes).map(Some).map_err(invalid)
}

/// Why [`read_small`] didn't read a file that is there.
pub(crate) enum Unreadable {
    /// Reading it failed; the error names the file.
    Io(io::Error),
    /// moil won't read it, for the reason given.
    Refused(String),
}

/// The contents of `path`, or `None` if there is no such file. Only a
/// regular file of at most 1 MiB is read, not through a symbolic link:
/// git, or anyone who can push to a repository the home is kept in, can
/// put anything at the home's paths, and mustn't make moil read a file
/// through a link, a device or without end, or wait on a FIFO.
pub(crate) fn read_small(path: &Path) -> Result<Option<Vec<u8>>, Unreadable> {
    use std::io::Read;
    let failed = |e| Unreadable::Io(with_path(e, path));
    let file = match open_regular(path, OpenOptions::new().read(true)) {
        Ok(file) => file,
        Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(OpenError::Io(e)) => return Err(failed(e)),
        Err(OpenError::SymbolicLink) => return Err(Unreadable::Refused(SYMBOLIC_LINK.into())),
        Err(refused) => return Err(Unreadable::Refused(refused.to_string())),
    };
    let metadata = file.metadata().map_err(failed)?;
    let too_large = || Unreadable::Refused("it is larger than 1 MiB".into());
    if metadata.len() > MAX_JSON_BYTES {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(failed)?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return Err(too_large());
    }
    Ok(Some(bytes))
}

const SYMBOLIC_LINK: &str = "it is a symbolic link, and moil reads only regular files here";

/// `bytes` as a `T`, read as [`read_strict_json`] reads files, or why not.
pub(crate) fn parse_strict<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    // Duplicate keys would let a file say one thing to whoever reads it and
    // another to moil, which keeps the last value.
    serde_json::from_slice::<NoDuplicateKeys>(bytes).map_err(|e| e.to_string())?;
    // serde would otherwise read a JSON array as a struct, field by field.
    if bytes.trim_ascii_start().first() != Some(&b'{') {
        return Err("it must hold one JSON object".into());
    }
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

/// Accepts any JSON without duplicate object keys, at any depth.
struct NoDuplicateKeys;

impl<'de> Deserialize<'de> for NoDuplicateKeys {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(NoDuplicateKeysVisitor)
    }
}

struct NoDuplicateKeysVisitor;

impl<'de> Visitor<'de> for NoDuplicateKeysVisitor {
    type Value = NoDuplicateKeys;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JSON")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!("duplicate key {key:?}")));
            }
            map.next_value::<NoDuplicateKeys>()?;
        }
        Ok(NoDuplicateKeys)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<NoDuplicateKeys>()?.is_some() {}
        Ok(NoDuplicateKeys)
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(NoDuplicateKeys)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(NoDuplicateKeys)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(NoDuplicateKeys)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(NoDuplicateKeys)
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(NoDuplicateKeys)
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(NoDuplicateKeys)
    }
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
    fn strict_files_refuse_what_could_hide_a_typo() {
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            #[serde(default)]
            paused: bool,
        }
        let parse = |text: &str| parse_strict::<File>(text.as_bytes());
        assert!(parse(" {\"paused\": true}\n").unwrap().paused);
        for (text, why) in [
            (r#"{"pausd": true}"#, "unknown field `pausd`"),
            (
                r#"{"paused": true, "paused": false}"#,
                "duplicate key \"paused\"",
            ),
            ("[true]", "one JSON object"),
            ("{", "EOF"),
        ] {
            let error = parse(text).unwrap_err();
            assert!(error.contains(why), "{text}: {error}");
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert!(read_strict_json::<File>(&path).unwrap().is_none());
        fs::write(&path, r#"{"pausd": true}"#).unwrap();
        let err = read_strict_json::<File>(&path).unwrap_err();
        assert!(matches!(err, Error::Invalid { .. }), "{err}");
        assert!(err.to_string().contains("settings.json"), "{err}");
    }

    /// What git could put at the path of one of the home's files: nothing
    /// moil reads there hangs, reads another file, or reads without end.
    #[cfg(unix)]
    #[test]
    fn only_small_regular_files_are_read() {
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere.json");
        fs::write(&elsewhere, "{}").unwrap();
        let path = dir.path().join("service.json");
        let fifo = |path: &Path| {
            let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: `path` is a valid C string.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        };
        type Make<'a> = &'a dyn Fn(&Path);
        let cases: [(Make, &str); 5] = [
            (
                &|path| std::os::unix::fs::symlink("/dev/zero", path).unwrap(),
                "symbolic link",
            ),
            (
                &|path| std::os::unix::fs::symlink(&elsewhere, path).unwrap(),
                "symbolic link",
            ),
            (&fifo, "isn't a regular file"),
            (
                &|path| fs::create_dir(path).unwrap(),
                "isn't a regular file",
            ),
            (
                &|path| {
                    let file = fs::File::create(path).unwrap();
                    file.set_len(MAX_JSON_BYTES + 1).unwrap();
                },
                "larger than 1 MiB",
            ),
        ];
        for (make, why) in cases {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_dir(&path);
            make(&path);
            match read_strict_json::<serde_json::Value>(&path) {
                Err(Error::Invalid { reason, .. }) => assert!(reason.contains(why), "{reason}"),
                other => panic!("{other:?}"),
            }
            match read_json::<serde_json::Value>(&path) {
                Err(Error::Damaged { reason, .. }) => assert!(reason.contains(why), "{reason}"),
                other => panic!("{other:?}"),
            }
        }
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
