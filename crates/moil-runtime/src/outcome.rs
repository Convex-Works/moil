//! How an attempt ends (spec §8.5).
//!
//! The rules apply in order and the first that applies decides. Rules 1–3
//! (stopped, interrupted, timed out) are a [`Cut`]; rules 4–7 look at what
//! the script did ([`judge`]); rules 8–9 are the uploads.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use moil_proto::{ErrorCode, FileName, JobError, Outcome, Upload};
use serde_json::Value;

use crate::stdout::{Report, truncate};
use crate::workdir::open_output;
use crate::{StopReason, units};

/// How much of a name the result gives a failure quotes: more than any
/// output's name takes.
const MAX_QUOTED_NAME_BYTES: usize = 200;

/// Why an attempt ended before running its course.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cut {
    /// Rules 1 and 2.
    Stopped(StopReason),
    /// Rule 3: it ran past this time limit.
    TimedOut(Duration),
}

impl Cut {
    pub fn outcome(self) -> Outcome {
        match self {
            Cut::Stopped(StopReason::Cancelled) => Outcome::Cancelled,
            Cut::Stopped(StopReason::Interrupted) => Outcome::failed(JobError::new(
                ErrorCode::Interrupted,
                "the attempt was stopped on the machine",
            )),
            Cut::TimedOut(limit) => Outcome::failed(JobError::new(
                ErrorCode::Timeout,
                format!(
                    "the attempt ran past its time limit of {}",
                    units::duration(limit)
                ),
            )),
        }
    }
}

/// What the script produced, once it's accepted as a success.
#[derive(Debug)]
pub(crate) struct Accepted {
    /// The outputs to upload, open: what was judged is what gets uploaded.
    pub files: Vec<(FileName, std::fs::File)>,
    pub meta: Option<Value>,
}

/// Rules 4–7: judges a script that ran its course, from what it reported,
/// how it exited, which outputs it may produce and what it left in
/// `out_dir`.
pub(crate) fn judge(
    report: Report,
    status: ExitStatus,
    declared: &BTreeMap<FileName, Upload>,
    out_dir: &Path,
) -> Result<Accepted, JobError> {
    if let Some(error) = report.error {
        return Err(JobError::new(ErrorCode::ScriptError, error.message).retryable(error.retryable));
    }
    if !status.success() {
        return Err(JobError::new(
            ErrorCode::Exit,
            format!("the script {}", describe(status)),
        ));
    }
    let Some(result) = report.result else {
        return Err(JobError::new(
            ErrorCode::NoResult,
            report
                .refused_result
                .unwrap_or_else(|| "the script exited without sending a result".into()),
        ));
    };
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for name in result.files {
        let missing = |why: String| JobError::new(ErrorCode::OutputMissing, why);
        let Some(output) = FileName::new(name.as_str())
            .ok()
            .filter(|output| declared.contains_key(output))
        else {
            // The name is the script's, of any length; outputs' names are
            // short.
            let name = truncate(&name, MAX_QUOTED_NAME_BYTES);
            return Err(missing(format!(
                "the result names {name:?}, which isn't one of the job's outputs"
            )));
        };
        if !seen.insert(output.clone()) {
            continue;
        }
        let file = open_output(out_dir, &output)
            .map_err(|why| missing(format!("the result names {name:?}, but {why}")))?;
        files.push((output, file));
    }
    Ok(Accepted {
        files,
        meta: result.meta,
    })
}

/// How a process ended, completing "the script …".
fn describe(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        // uv reports a script killed by signal N as exit status 128 + N.
        (Some(code), _) if code > 128 && code < 128 + 32 => format!(
            "exited with status {code}; it was probably killed by signal {}",
            code - 128
        ),
        (Some(code), _) => format!("exited with status {code}"),
        (None, Some(signal)) => format!("was killed by signal {signal}"),
        (None, None) => "ended in an unknown way".into(),
    }
}
