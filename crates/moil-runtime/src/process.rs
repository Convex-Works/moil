//! The script's process group: stopping it gracefully and making sure none
//! of it outlives the attempt (spec §8.6).

use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::sys::{self, SIGKILL, SIGTERM};

/// A process that leads its own process group, with everything it starts.
///
/// The leader stays unreaped until [`Group::finish`], so the group's ID
/// can't be reused while it's being signalled. Dropping a group before then
/// kills all of it.
#[derive(Debug)]
pub(crate) struct Group {
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
                tracing::warn!(pgid, %error, "couldn't wait for the script's process");
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
            tracing::warn!(pgid = self.pgid, signal, %error, "couldn't signal the script's process group");
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
