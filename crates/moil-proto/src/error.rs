//! How attempts fail (spec §9).

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why an attempt failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The script sent an `error` event.
    ScriptError,
    /// The script exited with a nonzero status or was killed by a signal.
    Exit,
    /// The script exited without sending `result`.
    NoResult,
    /// `result.files` named an undeclared or absent output.
    OutputMissing,
    /// The attempt ran past `timeout_ms`.
    Timeout,
    /// `uv sync` couldn't create the environment.
    Environment,
    /// An asset couldn't be downloaded.
    AssetDownload,
    /// An asset didn't match the hash in the manifest.
    AssetMismatch,
    /// An input couldn't be downloaded.
    InputDownload,
    /// An output couldn't be uploaded.
    OutputUpload,
    /// The owner stopped the attempt, or the app quit.
    Interrupted,
    /// The machine was assigned the attempt while not idle; it never started.
    Busy,
    /// The bundle isn't approved or stored on the machine; it never started.
    NotApproved,
    /// A bug in the runtime.
    Internal,
    /// The service's lease on the attempt ran out.
    LeaseExpired,
    /// The machine reconnected without the attempt.
    Lost,
    /// A code from a newer peer.
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    /// Whether another attempt could succeed, for codes where the spec fixes
    /// the answer. `script_error` defaults to `false`; the script decides.
    pub const fn default_retryable(self) -> bool {
        match self {
            Self::ScriptError
            | Self::Exit
            | Self::NoResult
            | Self::OutputMissing
            | Self::Timeout
            | Self::Environment
            | Self::AssetMismatch => false,
            Self::AssetDownload
            | Self::InputDownload
            | Self::OutputUpload
            | Self::Interrupted
            | Self::Busy
            | Self::NotApproved
            | Self::Internal
            | Self::LeaseExpired
            | Self::Lost
            | Self::Unknown => true,
        }
    }

    /// Whether the attempt never started, so it shouldn't count toward a
    /// retry limit (spec §7.2).
    pub const fn never_started(self) -> bool {
        matches!(self, Self::Busy | Self::NotApproved)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ScriptError => "script_error",
            Self::Exit => "exit",
            Self::NoResult => "no_result",
            Self::OutputMissing => "output_missing",
            Self::Timeout => "timeout",
            Self::Environment => "environment",
            Self::AssetDownload => "asset_download",
            Self::AssetMismatch => "asset_mismatch",
            Self::InputDownload => "input_download",
            Self::OutputUpload => "output_upload",
            Self::Interrupted => "interrupted",
            Self::Busy => "busy",
            Self::NotApproved => "not_approved",
            Self::Internal => "internal",
            Self::LeaseExpired => "lease_expired",
            Self::Lost => "lost",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A failed attempt's error, as carried by a failed `done`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
}

impl JobError {
    /// An error with the code's default retryability.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: code.default_retryable(),
            exit_code: None,
            stderr_tail: None,
        }
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.exit_code = Some(exit_code);
        self
    }

    pub fn with_stderr_tail(mut self, tail: impl Into<String>) -> Self {
        let tail = tail.into();
        if !tail.is_empty() {
            self.stderr_tail = Some(tail);
        }
        self
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for JobError {}
