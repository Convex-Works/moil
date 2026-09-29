//! Opening a file at a path that someone else may have put anything at:
//! a bundle's files, and the home's files that git can change.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Why [`open_regular`] didn't open a file.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Opening it failed as it can for any file: with
    /// [`NotFound`](io::ErrorKind::NotFound) if nothing is there.
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("it is a symbolic link")]
    SymbolicLink,
    /// Such as a folder, a device or a FIFO.
    #[error("it isn't a regular file")]
    NotRegular,
}

/// Opens the regular file at `path` as `options` say, never through a
/// symbolic link at `path` itself: whoever can change the folder it is in,
/// such as someone pushing to a repository the folder is kept in, can't
/// make moil open another file through a link, read a device without end,
/// or wait on a FIFO. Opening never blocks; the file it returns reads and
/// writes as usual, as does a process that gets it as its stdout.
pub fn open_regular(path: &Path, options: &OpenOptions) -> Result<File, OpenError> {
    let mut options = options.clone();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
    );
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(OpenError::SymbolicLink);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        // Opening fails on a link, and can on what isn't a regular file,
        // such as a FIFO opened only to write.
        Err(error) => {
            return Err(match std::fs::symlink_metadata(path) {
                Ok(m) if m.file_type().is_symlink() => OpenError::SymbolicLink,
                Ok(m) if !m.is_file() => OpenError::NotRegular,
                _ => error.into(),
            });
        }
    };
    if !file.metadata()?.is_file() {
        return Err(OpenError::NotRegular);
    }
    #[cfg(unix)]
    block(&file)?;
    Ok(file)
}

/// Clears `O_NONBLOCK`, which only opening needed.
#[cfg(unix)]
fn block(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let fd = file.as_raw_fd();
    // SAFETY: fcntl on a descriptor `file` owns has no memory-safety
    // preconditions.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    use super::*;

    fn fifo(path: &Path) {
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path` is a valid C string.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    /// Whatever is at the path, to read or to append to, and whether or
    /// not it may be created: nothing but a regular file is opened, and
    /// nothing blocks.
    #[test]
    fn only_a_regular_file_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let theirs = dir.path().join("theirs.txt");
        std::fs::write(&theirs, "theirs\n").unwrap();
        let path = dir.path().join("file");
        let mut read = File::options();
        read.read(true);
        let mut append = File::options();
        append.read(true).append(true).create(true);
        let mut write = File::options();
        write.write(true).create(true);

        type Make<'a> = &'a dyn Fn(&Path);
        let cases: [(Make, &str); 5] = [
            (
                &|path| std::os::unix::fs::symlink("/dev/zero", path).unwrap(),
                "it is a symbolic link",
            ),
            (
                &|path| std::os::unix::fs::symlink(&theirs, path).unwrap(),
                "it is a symbolic link",
            ),
            (
                &|path| std::os::unix::fs::symlink(dir.path().join("none"), path).unwrap(),
                "it is a symbolic link",
            ),
            (&fifo, "it isn't a regular file"),
            (
                &|path| std::fs::create_dir(path).unwrap(),
                "it isn't a regular file",
            ),
        ];
        for (make, why) in cases {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_dir(&path);
            make(&path);
            for options in [&read, &append, &write] {
                let error = open_regular(&path, options).unwrap_err();
                assert_eq!(error.to_string(), why, "{options:?}");
            }
        }
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs\n");
        assert!(
            !dir.path().join("none").exists(),
            "nothing made through a link"
        );

        // A regular file is opened as asked, and made if asked to.
        let _ = std::fs::remove_dir(&path);
        match open_regular(&path, &read) {
            Err(OpenError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
            other => panic!("{other:?}"),
        }
        let mut file = open_regular(&path, &append).unwrap();
        // SAFETY: fcntl on an open descriptor has no memory-safety
        // preconditions.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(flags & libc::O_NONBLOCK, 0, "only opening is non-blocking");
        file.write_all(b"one\n").unwrap();
        let mut text = String::new();
        open_regular(&path, &read)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "one\n");
    }
}
