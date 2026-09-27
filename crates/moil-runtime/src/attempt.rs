//! One attempt, from preparing the bundle to uploading its outputs
//! (spec §8).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moil_proto::{
    ErrorCode, FileInfo, FileName, JobContext, JobError, JobEvent, JobResult, Outcome, Phase,
    SCRIPT_CONTRACT_VERSION, UrlPolicy,
};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;

use crate::limits::Limits;
use crate::orphans::Registration;
use crate::outcome::{self, Accepted, Cut};
use crate::process::Group;
use crate::stderr::{self, Tail};
use crate::stdout::{self, Report};
use crate::workdir::WorkDir;
use crate::{EventSink, Job, Runtime, StopSignal, uv};

/// How long to keep reading the script's output after its process group is
/// gone. Only a process that left the group can hold it open longer.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Runs `job` to its end.
pub(crate) async fn run(
    runtime: &Runtime,
    job: Job,
    events: EventSink,
    stop: StopSignal,
) -> Outcome {
    let mut limits = Limits::new(stop, job.timeout);
    let mut attempt = Attempt {
        runtime,
        job: &job,
        events: &events,
        ran: None,
    };
    let ended = attempt.run(&mut limits).await;
    attempt.with_script_facts(outcome(&limits, ended))
}

/// How an attempt that ended as `ended` turns out (spec §8.5). A stop wins
/// over any other ending, even one that came as a transfer failed or once
/// the outputs were up.
fn outcome(limits: &Limits, ended: Result<JobResult, Ending>) -> Outcome {
    if let Some(reason) = limits.stopped() {
        return Cut::Stopped(reason).outcome();
    }
    match ended {
        Ok(result) => Outcome::Succeeded { result },
        Err(Ending::Cut(cut)) => cut.outcome(),
        Err(Ending::Failed(error)) => Outcome::failed(error),
    }
}

struct Attempt<'a> {
    runtime: &'a Runtime,
    job: &'a Job,
    events: &'a EventSink,
    /// Set once the script has run.
    ran: Option<Ran>,
}

/// What every failure after the script ran reports about it (spec §9).
struct Ran {
    exit_code: Option<i32>,
    stderr_tail: String,
}

/// How an attempt ends other than in success.
enum Ending {
    Cut(Cut),
    Failed(JobError),
}

impl From<Cut> for Ending {
    fn from(cut: Cut) -> Self {
        Self::Cut(cut)
    }
}

impl From<JobError> for Ending {
    fn from(error: JobError) -> Self {
        Self::Failed(error)
    }
}

impl Attempt<'_> {
    async fn run(&mut self, limits: &mut Limits) -> Result<JobResult, Ending> {
        let (runtime, job) = (self.runtime, self.job);
        self.check_urls()?;

        self.phase(Phase::Preparing);
        let script = limits
            .guard(runtime.prepare_bundle(&job.bundle, Some(self.events)))
            .await??;
        let work_path = runtime.work_dir(&job.id, job.attempt);
        // Dropped last: removes the working directory, then the record.
        let mut registration = Registration::new(
            &runtime.config.jobs_dir,
            &job.id,
            job.attempt,
            work_path.clone(),
            runtime.config.keep_work_dirs,
        )
        .map_err(internal("couldn't record the attempt"))?;
        let work = WorkDir::create(&work_path)
            .map_err(internal("couldn't create the working directory"))?;
        let assets = runtime
            .assets
            .link(&job.bundle.manifest().assets, &work.assets())
            .map_err(internal(
                "couldn't link the assets into the working directory",
            ))?;

        self.phase(Phase::Downloading);
        let inputs = limits.guard(self.download_inputs(&work)).await??;

        self.phase(Phase::Running);
        let context = JobContext {
            v: SCRIPT_CONTRACT_VERSION,
            job_id: job.id.clone(),
            attempt: job.attempt,
            params: job.params.clone(),
            work_dir: work.root().to_owned(),
            inputs,
            output_dir: work.outputs(),
            outputs: job.outputs.keys().cloned().collect(),
            assets_dir: work.assets(),
            assets,
        };
        let accepted = self
            .run_script(&script, &work, &context, limits, &mut registration)
            .await?;

        self.phase(Phase::Uploading);
        let files = limits.guard(self.upload(accepted.files)).await??;
        Ok(JobResult {
            meta: accepted.meta,
            files,
        })
    }

    /// Which URLs the job's inputs and outputs may use (spec §3).
    fn url_policy(&self) -> UrlPolicy {
        self.runtime.url_policy(self.job.url_policy)
    }

    /// Refuses inputs and outputs the URL policy doesn't allow before any
    /// work is done (spec §3). No other attempt would do better.
    fn check_urls(&self) -> Result<(), JobError> {
        let policy = self.url_policy();
        for (name, input) in &self.job.inputs {
            policy.check_str(&input.url).map_err(|why| {
                JobError::new(
                    ErrorCode::InputDownload,
                    format!("input {name} can't be downloaded: its URL {why}"),
                )
                .retryable(false)
            })?;
        }
        for (name, output) in &self.job.outputs {
            policy.check_str(&output.url).map_err(|why| {
                JobError::new(
                    ErrorCode::OutputUpload,
                    format!("output {name} can't be uploaded: its URL {why}"),
                )
                .retryable(false)
            })?;
        }
        Ok(())
    }

    async fn download_inputs(
        &self,
        work: &WorkDir,
    ) -> Result<BTreeMap<FileName, PathBuf>, JobError> {
        let mut paths = BTreeMap::new();
        for (name, source) in &self.job.inputs {
            let failed = |why: String| {
                JobError::new(
                    ErrorCode::InputDownload,
                    format!("couldn't download input {name}: {why}"),
                )
            };
            let path = work.inputs().join(name.as_str());
            let mut file = tokio::fs::File::create(&path)
                .await
                .map_err(|e| failed(format!("couldn't create {}: {e}", path.display())))?;
            self.runtime
                .http
                .download(
                    self.url_policy(),
                    &source.url,
                    &source.headers,
                    &mut file,
                    None,
                )
                .await
                .map_err(|e| failed(e.to_string()).retryable(e.is_retryable()))?;
            paths.insert(name.clone(), path);
        }
        Ok(paths)
    }

    /// Runs the script to its end and judges it (rules 1–7 of spec §8.5).
    async fn run_script(
        &mut self,
        script: &uv::Script,
        work: &WorkDir,
        context: &JobContext,
        limits: &mut Limits,
        registration: &mut Registration,
    ) -> Result<Accepted, Ending> {
        let config = &self.runtime.config;
        let uv = self.runtime.uv();
        let mut command = script.command(&uv, "run");
        command
            .env("TMPDIR", work.tmp())
            .env("PYTHONUNBUFFERED", "1")
            .current_dir(work.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut group = Group::spawn(command).map_err(|e| uv::cant_start(&uv, &e))?;
        registration
            .set_group(group.id())
            .map_err(internal("couldn't record the script's process group"))?;

        let child = group.child_mut();
        let (stdin, out, err) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (Some(mut stdin), Some(out), Some(err)) = (stdin, out, err) else {
            unreachable!("the script's stdio is piped");
        };
        let mut line = serde_json::to_vec(context).expect("a job context serializes");
        line.push(b'\n');
        // A script that doesn't read its stdin must not block the attempt.
        tokio::spawn(async move {
            let _ = stdin.write_all(&line).await;
            let _ = stdin.shutdown().await;
        });
        let report = Arc::new(Mutex::new(Report::default()));
        let tail = Arc::new(Mutex::new(Tail::default()));
        let reading = tokio::spawn(stdout::forward(out, self.events.clone(), report.clone()));
        let collecting = tokio::spawn(stderr::collect(err, config.echo_stderr, tail.clone()));

        let cut = tokio::select! {
            () = group.exited() => None,
            cut = limits.reached() => {
                group.terminate(config.kill_grace).await;
                Some(cut)
            }
        };
        let status = group.finish().await;
        drain(reading).await;
        drain(collecting).await;
        self.ran = Some(Ran {
            exit_code: status.as_ref().ok().and_then(|s| s.code()),
            stderr_tail: tail.lock().unwrap_or_else(|e| e.into_inner()).render(),
        });

        if let Some(cut) = limits.decide(cut) {
            return Err(cut.into());
        }
        let status = status.map_err(internal("couldn't get the script's exit status"))?;
        let report = std::mem::take(&mut *report.lock().unwrap_or_else(|e| e.into_inner()));
        Ok(outcome::judge(
            report,
            status,
            &self.job.outputs,
            &work.outputs(),
        )?)
    }

    async fn upload(
        &self,
        files: Vec<(FileName, std::fs::File)>,
    ) -> Result<BTreeMap<FileName, FileInfo>, JobError> {
        let mut uploaded = BTreeMap::new();
        for (name, file) in files {
            let target = &self.job.outputs[&name];
            let info = self
                .runtime
                .http
                .upload(self.url_policy(), target, file)
                .await
                .map_err(|e| {
                    JobError::new(
                        ErrorCode::OutputUpload,
                        format!("couldn't upload {name}: {e}"),
                    )
                    .retryable(e.is_retryable())
                })?;
            uploaded.insert(name, info);
        }
        Ok(uploaded)
    }

    fn phase(&self, phase: Phase) {
        let _ = self.events.send(JobEvent::Phase { phase });
    }

    /// Adds the script's exit code and stderr tail to a failure, if the
    /// script ran and the failure doesn't carry its own.
    fn with_script_facts(&self, outcome: Outcome) -> Outcome {
        match (outcome, &self.ran) {
            (Outcome::Failed { mut error }, Some(ran)) => {
                error.exit_code = error.exit_code.or(ran.exit_code);
                if error.stderr_tail.is_none() {
                    error = error.with_stderr_tail(ran.stderr_tail.clone());
                }
                Outcome::Failed { error }
            }
            (outcome, _) => outcome,
        }
    }
}

/// Waits for a task reading the script's output to finish, but not for long.
async fn drain(task: JoinHandle<()>) {
    let abort = task.abort_handle();
    if tokio::time::timeout(DRAIN_TIMEOUT, task).await.is_err() {
        abort.abort();
        tracing::warn!("a process that left the script's process group kept its output open");
    }
}

fn internal(what: &'static str) -> impl FnOnce(std::io::Error) -> JobError {
    move |error| JobError::new(ErrorCode::Internal, format!("{what}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StopReason, stop_signal};

    #[tokio::test]
    async fn a_stop_wins_over_however_else_the_attempt_ended() {
        let (stopper, stop) = stop_signal();
        let limits = Limits::new(stop, None);
        let failed = || {
            Err(Ending::Failed(JobError::new(
                ErrorCode::InputDownload,
                "the connection was reset",
            )))
        };
        let succeeded = || {
            Ok(JobResult {
                meta: None,
                files: BTreeMap::new(),
            })
        };
        assert!(matches!(
            outcome(&limits, failed()),
            Outcome::Failed { error } if error.code == ErrorCode::InputDownload
        ));

        stopper.stop(StopReason::Cancelled);
        assert_eq!(outcome(&limits, failed()), Outcome::Cancelled);
        assert_eq!(outcome(&limits, succeeded()), Outcome::Cancelled);
    }
}
