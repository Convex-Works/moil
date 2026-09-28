//! The socket's file, who may use it, and the lines of JSON it carries.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The longest path a socket address holds: `sun_path` has 104 bytes on
/// macOS and the BSDs (108 on Linux), one of them for the NUL at the end.
const MAX_PATH_BYTES: usize = 103;

/// This process's user ID, which the other end must share.
pub(super) fn user_id() -> u32 {
    // SAFETY: geteuid has no preconditions and can't fail.
    unsafe { libc::geteuid() }
}

/// A path to a socket that fits in a socket address.
///
/// A home deep in the file system, such as a test's under `$TMPDIR`, has a
/// socket path too long to bind or connect to. Such a socket is reached
/// through a symbolic link to its directory, which this makes in a private
/// directory of the user's under `/tmp`, for this process alone, and
/// removes when dropped. Binding or connecting needs the path only for a
/// moment, and the socket itself stays where it is.
#[derive(Debug)]
pub(super) struct ShortPath {
    path: PathBuf,
    link: Option<PathBuf>,
}

impl ShortPath {
    pub(super) fn to(socket: &Path) -> io::Result<Self> {
        if socket.as_os_str().len() <= MAX_PATH_BYTES {
            return Ok(Self {
                path: socket.to_owned(),
                link: None,
            });
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (Some(dir), Some(name)) = (socket.parent(), socket.file_name()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} can't be a socket", socket.display()),
            ));
        };
        let link = links_dir()?.join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // Left by a process that had this one's ID and crashed.
        remove_if_present(&link)?;
        std::os::unix::fs::symlink(std::path::absolute(dir)?, &link)?;
        Ok(Self {
            path: link.join(name),
            link: Some(link),
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ShortPath {
    fn drop(&mut self) {
        if let Some(link) = &self.link {
            let _ = fs::remove_file(link);
        }
    }
}

/// `/tmp/moil-<uid>`, where [`ShortPath`]s are made: a directory only this
/// user can use, as tmux keeps its sockets. Anyone can make a directory in
/// `/tmp`, so one that someone else made first is refused.
fn links_dir() -> io::Result<PathBuf> {
    let dir = PathBuf::from(format!("/tmp/moil-{}", user_id()));
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != user_id() || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} isn't a directory that only you can use, which moil needs to reach the \
                 control socket of a home this deep; remove it and try again",
                dir.display()
            ),
        ));
    }
    Ok(dir)
}

/// Creates `dir` readable only by the owner, or takes other users' access
/// to it if it exists.
pub(super) fn private_dir(dir: &Path) -> io::Result<()> {
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

pub(super) fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Reads one line of JSON, of at most `max` bytes. `None` at the end of
/// the stream.
pub(super) async fn read<T: DeserializeOwned>(
    reader: &mut (impl AsyncBufRead + Unpin),
    max: usize,
) -> io::Result<Option<T>> {
    let mut line = Vec::new();
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    if (&mut *reader)
        .take(limit)
        .read_until(b'\n', &mut line)
        .await?
        == 0
    {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(if line.len() > max {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a message was longer than {max} bytes"),
            )
        } else {
            io::ErrorKind::UnexpectedEof.into()
        });
    }
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes `message` as one line of JSON.
pub(super) async fn write(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &impl Serialize,
) -> io::Result<()> {
    let mut line = serde_json::to_vec(message).expect("control messages serialize to JSON");
    line.push(b'\n');
    writer.write_all(&line).await
}

/// Waits until the other end closes the connection, or sends something
/// where it should be listening, which ends the conversation all the same.
/// Nothing is read, so this can be raced against other work.
pub(super) async fn closed(reader: &mut (impl AsyncBufRead + Unpin)) {
    let _ = reader.fill_buf().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lines_are_bounded() {
        let mut input: &[u8] = b"{\"a\":1}\n[1,2,3,4,5,6]\n";
        let first: serde_json::Value = read(&mut input, 8).await.unwrap().unwrap();
        assert_eq!(first, serde_json::json!({"a": 1}));
        let error = read::<serde_json::Value>(&mut input, 8).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");

        let mut cut: &[u8] = b"{\"a\":";
        let error = read::<serde_json::Value>(&mut cut, 8).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        let mut empty: &[u8] = b"";
        assert!(
            read::<serde_json::Value>(&mut empty, 8)
                .await
                .unwrap()
                .is_none()
        );
    }
}
