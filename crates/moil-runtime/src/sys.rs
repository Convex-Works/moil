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
    let dir = File::options()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)?;
    let name = CString::new(std::ffi::OsStr::new(name).as_bytes())?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: `name` is a valid C string and `dir` an open directory.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new descriptor that nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// When process `pid` started, as a token that differs between two
/// processes that happened to get the same pid, even across reboots. `None`
/// if there's no such process or the platform can't tell.
pub(crate) fn start_token(pid: i32) -> Option<String> {
    platform::start_token(pid)
}

#[cfg(target_os = "macos")]
mod platform {
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
    pub(super) fn start_token(pid: i32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Field 2 is the command name in parentheses, which may itself hold
        // spaces and parentheses; the fields after it are plain. Field 22
        // is the start time in clock ticks since boot.
        let rest = stat.get(stat.rfind(')')? + 2..)?;
        let ticks = rest.split(' ').nth(19)?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        Some(format!("{}:{ticks}", boot.trim()))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
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
}
