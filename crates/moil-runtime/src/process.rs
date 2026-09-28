//! Process groups: stopping one gracefully, making sure none of it outlives
//! its leader, and cleaning up after a crash (spec §8.6). The runtime runs
//! scripts this way, and the machine agent runs its owner's hooks the same
//! way (spec/machine.md §2.2).

use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::sys::{self, SIGKILL, SIGTERM};

/// A process that leads its own process group, with everything it starts.
///
/// The leader stays unreaped until [`Group::finish`], so the group's ID
/// can't be reused while it's being signalled. Dropping a group before then
/// kills all of it.
#[derive(Debug)]
pub struct Group {
    child: Child,
    pgid: i32,
    /// Resolves once the leader has exited; `None` after that was seen.
    exit: Option<JoinHandle<()>>,
    finished: bool,
}

impl Group {
    /// Starts `command` as the leader of a new process group.
    pub fn spawn(mut command: Command) -> io::Result<Self> {
        command.process_group(0);
        let child = command.spawn()?;
        let pgid = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .ok_or_else(|| io::Error::other("the spawned process has no pid"))?;
        let exit = tokio::task::spawn_blocking(move || {
            if let Err(error) = sys::wait_exited(pgid) {
                tracing::warn!(pgid, %error, "couldn't wait for a process group's leader");
            }
        });
        Ok(Self {
            child,
            pgid,
            exit: Some(exit),
            finished: false,
        })
    }

    /// The group's ID, which is also its leader's pid.
    pub fn id(&self) -> i32 {
        self.pgid
    }

    /// The group's leader, for a record that lets [`Process::kill_group`]
    /// clean up after a crash.
    pub fn leader(&self) -> Process {
        Process {
            pid: self.pgid,
            // Empty if the leader has already exited, which matches no
            // process: the group is then treated like one whose leader is
            // gone.
            started: sys::start_token(self.pgid).unwrap_or_default(),
            boot: sys::boot_id(),
        }
    }

    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Resolves once the leader has exited. Cancel-safe.
    pub async fn exited(&mut self) {
        if let Some(exit) = &mut self.exit {
            let _ = exit.await;
            self.exit = None;
        }
    }

    /// Stops the group: `SIGTERM` to all of it, up to `grace` for the leader
    /// to exit, then `SIGKILL` to whatever is left.
    pub async fn terminate(&mut self, grace: Duration) {
        self.signal(SIGTERM);
        let _ = tokio::time::timeout(grace, self.exited()).await;
        self.signal(SIGKILL);
        self.exited().await;
    }

    /// Waits for the leader to exit, kills anything it left running in the
    /// group, and returns the leader's exit status.
    pub async fn finish(mut self) -> io::Result<ExitStatus> {
        self.exited().await;
        self.signal(SIGKILL);
        self.finished = true;
        self.child.wait().await
    }

    fn signal(&self, signal: i32) {
        if let Err(error) = sys::signal_group(self.pgid, signal) {
            tracing::warn!(pgid = self.pgid, signal, %error, "couldn't signal a process group");
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if !self.finished {
            // The leader isn't reaped yet, so the ID is still ours. tokio
            // reaps the leader in the background once the child is dropped.
            self.signal(SIGKILL);
        }
    }
}

/// A process, told apart from any later process with the same pid, even
/// across reboots: what a record on disk names, so that whoever reads it
/// after a crash never mistakes an unrelated process for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub(crate) pid: i32,
    pub(crate) started: String,
    /// The boot the process ran in, if the platform can tell. A record
    /// without one, such as one an earlier moil wrote, never leads to
    /// killing anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) boot: Option<String>,
}

impl Process {
    /// This process, or `None` if the platform can't tell processes apart.
    pub fn current() -> Option<Self> {
        Self::of(std::process::id() as i32)
    }

    pub(crate) fn of(pid: i32) -> Option<Self> {
        Some(Self {
            pid,
            started: sys::start_token(pid)?,
            boot: sys::boot_id(),
        })
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Whether this very process still runs.
    pub fn is_running(&self) -> bool {
        sys::start_token(self.pid).as_deref() == Some(&self.started)
    }

    /// Kills what's left of the process group this process led, as
    /// [`Group::leader`] names it: all of it if the leader still runs or is
    /// gone, nothing if its pid now belongs to an unrelated process. Kills
    /// nothing either if the computer has restarted since, which ends every
    /// group, or if it can't tell whether it has, or if the pid is one no
    /// group of moil's can have.
    pub fn kill_group(&self) -> io::Result<()> {
        // After a restart, a group with this ID is someone else's, even
        // with its leader gone. Signalling group 0 or 1 would reach moil's
        // own group or every process.
        if self.pid <= 1 || self.boot.is_none() || self.boot != sys::boot_id() {
            return Ok(());
        }
        match sys::start_token(self.pid) {
            // The leader still runs: the group is certainly this one.
            Some(started) if started == self.started => sys::signal_group(self.pid, SIGKILL),
            // The pid now belongs to an unrelated process: leave it alone.
            Some(_) => Ok(()),
            // The leader is gone. The OS doesn't reuse a group ID while any
            // process is still in the group, so whatever is in it is left
            // over from this one, unless all of it ended and the ID went to
            // a new group whose own leader has ended since too.
            None => sys::signal_group(self.pid, SIGKILL),
        }
    }
}
