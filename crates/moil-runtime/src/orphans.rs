//! Cleaning up after a crash (spec §8.6).
//!
//! Every running attempt has a record on disk naming its working directory,
//! the process supervising it and, once the script starts, its process
//! group. Processes are named by pid and start time, so a pid the OS has
//! since handed to an unrelated process is never mistaken for them. When an
//! attempt ends, its record goes; a record whose supervisor is gone belongs
//! to a crashed run, and [`reap`] kills what's left of it.

use std::io;
use std::path::{Path, PathBuf};

use moil_proto::JobId;
use serde::{Deserialize, Serialize};

use crate::sys::{self, SIGKILL};
use crate::workdir::remove_tree;

/// Where records live, inside the jobs directory. Job IDs can't start with
/// a dot, so this never collides with a working directory.
const RECORDS: &str = ".attempts";

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    job_id: String,
    attempt: u32,
    work_dir: PathBuf,
    supervisor: Process,
    group: Option<Process>,
}

/// A process, told apart from any later process with the same pid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Process {
    pid: i32,
    started: String,
}

impl Process {
    fn of(pid: i32) -> Option<Self> {
        Some(Self {
            pid,
            started: sys::start_token(pid)?,
        })
    }

    fn is_running(&self) -> bool {
        sys::start_token(self.pid).as_deref() == Some(&self.started)
    }
}

/// A running attempt's footprint on disk: its record and its working
/// directory. Dropping it removes the working directory (unless it is to be
/// kept) and then the record, so a crash in between still leaves a record
/// for [`reap`].
#[derive(Debug)]
pub(crate) struct Registration {
    path: PathBuf,
    record: Record,
    keep_work_dir: bool,
}

impl Registration {
    /// Records that this process is running an attempt in `work_dir`.
    pub fn new(
        jobs_dir: &Path,
        job_id: &JobId,
        attempt: u32,
        work_dir: PathBuf,
        keep_work_dir: bool,
    ) -> io::Result<Self> {
        let supervisor = Process::of(std::process::id() as i32)
            .ok_or_else(|| io::Error::other("couldn't identify this process"))?;
        let dir = jobs_dir.join(RECORDS);
        std::fs::create_dir_all(&dir)?;
        let registration = Self {
            path: dir.join(format!("{job_id}.{attempt}.json")),
            record: Record {
                job_id: job_id.to_string(),
                attempt,
                work_dir,
                supervisor,
                group: None,
            },
            keep_work_dir,
        };
        registration.save()?;
        Ok(registration)
    }

    /// Records the script's process group, led by `pgid`.
    pub fn set_group(&mut self, pgid: i32) -> io::Result<()> {
        self.record.group = Some(Process {
            pid: pgid,
            // Empty if the leader has already exited, which matches no
            // process: the group is then treated like one whose leader is
            // gone.
            started: sys::start_token(pgid).unwrap_or_default(),
        });
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        let staging = self.path.with_extension("json.tmp");
        std::fs::write(&staging, serde_json::to_vec_pretty(&self.record)?)?;
        std::fs::rename(&staging, &self.path)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if !self.keep_work_dir
            && let Err(error) = remove_tree(&self.record.work_dir)
        {
            tracing::warn!(dir = %self.record.work_dir.display(), %error, "couldn't remove a working directory");
            // Keep the record, so the next reap tries again.
            return;
        }
        if let Err(error) = std::fs::remove_file(&self.path) {
            tracing::warn!(path = %self.path.display(), %error, "couldn't remove an attempt record");
        }
    }
}

/// Kills the processes of attempts whose supervising process is gone and
/// removes their working directories (unless `keep_work_dirs`). Attempts
/// still supervised by a live process, this one included, are left alone.
/// Returns how many attempts it cleaned up. An attempt it can't clean up
/// is logged and kept for the next time, without holding up the others.
pub(crate) fn reap(jobs_dir: &Path, keep_work_dirs: bool) -> io::Result<usize> {
    let dir = jobs_dir.join(RECORDS);
    let entries = match std::fs::read_dir(&dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        other => other?,
    };
    let jobs_dir = jobs_dir.canonicalize()?;
    let mut reaped = 0;
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        match reap_one(&path, &jobs_dir, keep_work_dirs) {
            Ok(true) => reaped += 1,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "couldn't clean up after an attempt of a crashed run");
            }
        }
    }
    Ok(reaped)
}

/// Cleans up after the attempt recorded at `path`, if its supervisor is
/// gone. Returns whether it did.
fn reap_one(path: &Path, jobs_dir: &Path, keep_work_dirs: bool) -> io::Result<bool> {
    let record: Record = match std::fs::read(path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
    {
        Ok(record) => record,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "removing an unreadable attempt record");
            std::fs::remove_file(path)?;
            return Ok(false);
        }
    };
    if record.supervisor.is_running() {
        return Ok(false);
    }
    if let Some(group) = &record.group {
        kill_group(group)?;
    }
    // Only ever delete inside the jobs directory, whatever the record says.
    let inside = record
        .work_dir
        .canonicalize()
        .is_ok_and(|dir| dir.starts_with(jobs_dir) && dir != jobs_dir);
    if inside && !keep_work_dirs {
        remove_tree(&record.work_dir)?;
    }
    std::fs::remove_file(path)?;
    tracing::info!(
        job_id = record.job_id,
        attempt = record.attempt,
        "cleaned up after an attempt of a crashed run"
    );
    Ok(true)
}

/// Kills what's left of the process group `group` led.
fn kill_group(group: &Process) -> io::Result<()> {
    match sys::start_token(group.pid) {
        // The leader still runs: the group is certainly this attempt's.
        Some(started) if started == group.started => sys::signal_group(group.pid, SIGKILL),
        // The pid now belongs to an unrelated process: leave it alone.
        Some(_) => Ok(()),
        // The leader is gone. The OS doesn't reuse a group ID while any
        // process is still in the group, so whatever is in it is left over
        // from this attempt.
        None => sys::signal_group(group.pid, SIGKILL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    /// A `sleep` leading its own process group, like a script would.
    fn sleeper() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap()
    }

    fn write_record(jobs_dir: &Path, name: &str, record: &Record) -> PathBuf {
        let dir = jobs_dir.join(RECORDS);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, serde_json::to_vec(record).unwrap()).unwrap();
        path
    }

    /// A process that has exited, so records naming it look orphaned.
    fn dead_supervisor() -> Process {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let started = sys::start_token(child.id() as i32).unwrap_or_else(|| "gone".into());
        child.wait().unwrap();
        Process {
            pid: child.id() as i32,
            started,
        }
    }

    fn is_alive(child: &mut std::process::Child) -> bool {
        child.try_wait().unwrap().is_none()
    }

    #[test]
    fn a_reused_pid_is_never_killed() {
        let jobs = tempfile::tempdir().unwrap();
        let mut unrelated = sleeper();
        let pid = unrelated.id() as i32;
        let work_dir = jobs.path().join("j.1");
        std::fs::create_dir(&work_dir).unwrap();
        // A record from a crashed run whose script's pid has since been
        // given to `unrelated`.
        write_record(
            jobs.path(),
            "j.1",
            &Record {
                job_id: "j".into(),
                attempt: 1,
                work_dir: work_dir.clone(),
                supervisor: dead_supervisor(),
                group: Some(Process {
                    pid,
                    started: "0.000001".into(),
                }),
            },
        );

        assert_eq!(reap(jobs.path(), false).unwrap(), 1);
        assert!(is_alive(&mut unrelated), "an unrelated process was killed");
        assert!(!work_dir.exists());
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
    }

    #[test]
    fn a_live_supervisors_attempts_are_left_alone() {
        let jobs = tempfile::tempdir().unwrap();
        let mut script = sleeper();
        let work_dir = jobs.path().join("j.1");
        std::fs::create_dir(&work_dir).unwrap();
        let record = write_record(
            jobs.path(),
            "j.1",
            &Record {
                job_id: "j".into(),
                attempt: 1,
                work_dir: work_dir.clone(),
                supervisor: Process::of(std::process::id() as i32).unwrap(),
                group: Process::of(script.id() as i32),
            },
        );

        assert_eq!(reap(jobs.path(), false).unwrap(), 0);
        assert!(is_alive(&mut script));
        assert!(work_dir.exists() && record.exists());
        script.kill().unwrap();
        script.wait().unwrap();
    }

    #[test]
    fn a_record_that_cant_be_cleaned_up_holds_up_no_other() {
        let jobs = tempfile::tempdir().unwrap();
        // Neither readable nor removable as a record.
        let stuck = jobs.path().join(RECORDS).join("stuck.json");
        std::fs::create_dir_all(&stuck).unwrap();
        let mut script = sleeper();
        let work_dir = jobs.path().join("j.1");
        std::fs::create_dir(&work_dir).unwrap();
        write_record(
            jobs.path(),
            "j.1",
            &Record {
                job_id: "j".into(),
                attempt: 1,
                work_dir: work_dir.clone(),
                supervisor: dead_supervisor(),
                group: Process::of(script.id() as i32),
            },
        );

        assert_eq!(reap(jobs.path(), false).unwrap(), 1);
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(script.wait().unwrap().signal(), Some(SIGKILL));
        assert!(!work_dir.exists());
        assert!(stuck.exists(), "kept for the next time");
    }

    #[test]
    fn records_never_lead_to_deleting_outside_the_jobs_directory() {
        let root = tempfile::tempdir().unwrap();
        let jobs = root.path().join("jobs");
        let elsewhere = root.path().join("precious");
        std::fs::create_dir_all(&jobs).unwrap();
        std::fs::create_dir(&elsewhere).unwrap();
        write_record(
            &jobs,
            "j.1",
            &Record {
                job_id: "j".into(),
                attempt: 1,
                work_dir: elsewhere.clone(),
                supervisor: dead_supervisor(),
                group: None,
            },
        );

        assert_eq!(reap(&jobs, false).unwrap(), 1);
        assert!(elsewhere.exists());
    }
}
