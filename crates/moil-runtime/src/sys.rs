//! The few Unix calls process supervision needs that std and tokio don't
//! offer.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub(crate) use libc::{SIGKILL, SIGTERM};

/// Sends `signal` to every process in the group `pgid`. A group with nothing
/// left to signal is not an error.
pub(crate) fn signal_group(pgid: i32, signal: i32) -> io::Result<()> {
    // SAFETY: killpg has no memory-safety preconditions.
    if unsafe { libc::killpg(pgid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // No such group, or (on macOS) a group whose only member is a
        // zombie leader we haven't reaped yet.
        Some(libc::ESRCH | libc::EPERM) => Ok(()),
        _ => Err(error),
    }
}

/// Blocks until `pid`, a child of this process, has exited, but leaves it
/// unreaped. Until it is reaped its pid stays taken, so its process group ID
/// can't be reused and signalling the group can only reach its own members.
pub(crate) fn wait_exited(pid: i32) -> io::Result<()> {
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: `info` is valid for writes of a siginfo_t.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Opens the file `name` in the directory `dir` for reading without
/// following a symbolic link on the way: fails if `dir` or `name` is one.
/// Never blocks, even on a FIFO; check what it opened.
pub(crate) fn open_in(dir: &Path, name: &str) -> io::Result<File> {
    open_at(&open_dir(dir)?, name)
}

/// Opens the directory at `path`, failing if it is a symbolic link. What
/// is then done through it happens in that directory, even if something
/// else takes its place at `path`.
pub(crate) fn open_dir(path: &Path) -> io::Result<File> {
    File::options()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// Opens the file `name` in `dir` for reading, failing if it is a symbolic
/// link. Never blocks, even on a FIFO; check what it opened.
pub(crate) fn open_at(dir: &File, name: &str) -> io::Result<File> {
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    let name = c_name(name)?;
    // SAFETY: `name` is a valid C string and `dir` an open directory.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new descriptor that nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Creates the file `name` in `dir` for writing. Fails if anything is
/// there already, a symbolic link included.
pub(crate) fn create_at(dir: &File, name: &str) -> io::Result<File> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let name = c_name(name)?;
    // SAFETY: `name` is a valid C string and `dir` an open directory.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, 0o666 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new descriptor that nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Renames `from_name` in `from` to `to_name` in `to`, replacing whatever
/// is there. A symbolic link is moved as itself, never followed.
pub(crate) fn rename_at(from: &File, from_name: &str, to: &File, to_name: &str) -> io::Result<()> {
    let (from_name, to_name) = (c_name(from_name)?, c_name(to_name)?);
    // SAFETY: both names are valid C strings and both directories open.
    let result = unsafe {
        libc::renameat(
            from.as_raw_fd(),
            from_name.as_ptr(),
            to.as_raw_fd(),
            to_name.as_ptr(),
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Removes the file `name` from `dir`; a symbolic link is removed as
/// itself.
pub(crate) fn unlink_at(dir: &File, name: &str) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: `name` is a valid C string and `dir` an open directory.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Which file something is: its device and inode, which no two files
/// share at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    pub fn of(file: &File) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }

    /// The file `name` in `dir`, or the symbolic link itself if it is one.
    pub fn at(dir: &File, name: &str) -> io::Result<Self> {
        let name = c_name(name)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: `name` is a valid C string, `dir` an open directory and
        // `stat` valid for writes of a stat.
        let result = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstatat filled it in.
        let stat = unsafe { stat.assume_init() };
        // As std's MetadataExt converts it: its type differs between
        // platforms.
        #[allow(clippy::unnecessary_cast)]
        let dev = stat.st_dev as u64;
        Ok(Self {
            dev,
            ino: stat.st_ino,
        })
    }
}

fn c_name(name: &str) -> io::Result<CString> {
    Ok(CString::new(std::ffi::OsStr::new(name).as_bytes())?)
}

/// When process `pid` started, as a token that differs between two
/// processes that happened to get the same pid, even across reboots. `None`
/// if there's no such process or the platform can't tell.
pub(crate) fn start_token(pid: i32) -> Option<String> {
    platform::start_token(pid)
}

/// A token that names the current boot of the computer and differs after
/// every restart. `None` if the platform can't tell.
pub(crate) fn boot_id() -> Option<String> {
    platform::boot_id().filter(|id| !id.is_empty())
}

#[cfg(target_os = "macos")]
mod platform {
    pub(super) fn boot_id() -> Option<String> {
        let name = c"kern.bootsessionuuid";
        let mut buffer = [0u8; 64];
        let mut size = buffer.len();
        // SAFETY: `name` is a C string, and `buffer` is valid for writes of
        // `size` bytes, which sysctlbyname updates to what it wrote.
        let result = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return None;
        }
        let text = buffer.get(..size)?;
        let text = text.strip_suffix(&[0]).unwrap_or(text);
        String::from_utf8(text.to_vec()).ok()
    }

    pub(super) fn start_token(pid: i32) -> Option<String> {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is valid for writes of `size` bytes.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written != size {
            return None;
        }
        // SAFETY: proc_pidinfo filled all `size` bytes.
        let info = unsafe { info.assume_init() };
        // Wall-clock start time, to the microsecond.
        Some(format!(
            "{}.{:06}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    pub(super) fn boot_id() -> Option<String> {
        let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        Some(id.trim().to_owned())
    }

    pub(super) fn start_token(pid: i32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Field 2 is the command name in parentheses, which may itself hold
        // spaces and parentheses; the fields after it are plain. Field 22
        // is the start time in clock ticks since boot.
        let rest = stat.get(stat.rfind(')')? + 2..)?;
        let ticks = rest.split(' ').nth(19)?;
        Some(format!("{}:{ticks}", boot_id()?))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    pub(super) fn boot_id() -> Option<String> {
        None
    }

    pub(super) fn start_token(_pid: i32) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_tokens_tell_processes_apart() {
        let own = start_token(std::process::id() as i32).expect("this process has a start time");
        assert_eq!(start_token(std::process::id() as i32), Some(own.clone()));

        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let theirs = start_token(child.id() as i32).expect("a running child has a start time");
        assert_ne!(theirs, own);
        child.kill().unwrap();
        child.wait().unwrap();
        // Its pid is either free or already someone else's.
        assert_ne!(start_token(child.id() as i32), Some(theirs));
    }

    #[test]
    fn the_boot_is_named_the_same_until_a_restart() {
        let boot = boot_id().expect("this platform names its boots");
        assert!(boot.len() >= 16, "{boot:?}");
        assert_eq!(boot_id(), Some(boot));
    }
}
