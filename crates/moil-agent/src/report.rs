//! What the command line prints with `--json` (spec/machine.md §3.3), and
//! what the control socket carries to it. The command line, the socket
//! and the app share these types, so that there is one shape to learn.
//!
//! Optional fields are left out when absent, as on the wire (spec §2).
//! Print a report with [`json_line`], which also escapes the characters
//! text output would reveal.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::SystemTime;

use moil_proto::control::BundleInfo;
use moil_proto::{BundleHash, ErrorCode, JobId, Phase};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::display::is_hidden;
use crate::{Approval, ServiceId};

/// `moil status`: the machine as the owner would see it in the app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// The home's absolute path.
    pub home: PathBuf,
    /// The running agent, if one runs on the home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentInfo>,
    pub paused: bool,
    /// The running attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<RunningJob>,
    /// The attempt that ran here last since the agent started, once ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_job: Option<EndedJob>,
    /// Every service the home knows, by ID.
    pub services: Vec<ServiceReport>,
    pub problems: Vec<Problem>,
}

/// The process running the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub host: Host,
    /// The running moil's version.
    pub version: String,
}

/// What runs the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Host {
    /// The tray app.
    App,
    /// `moil agent`.
    Agent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunningJob {
    pub service: ServiceId,
    pub job_id: JobId,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The bundle's name, from its approved manifest.
    pub bundle: String,
    pub phase: Phase,
    /// The script's latest `progress.fraction`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fraction: Option<f64>,
    /// The script's latest `progress.message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(with = "crate::rfc3339")]
    pub started_at: SystemTime,
    /// Asked to stop, and winding down.
    pub stopping: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndedJob {
    pub service: ServiceId,
    pub job_id: JobId,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub bundle: String,
    pub outcome: Outcome,
    /// Why it failed, with [`Outcome::Failed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorReport>,
    #[serde(with = "crate::rfc3339")]
    pub ended_at: SystemTime,
}

/// How an attempt ended, as [`JobOutcome`](crate::JobOutcome) tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorReport {
    pub code: ErrorCode,
    pub message: String,
}

/// A service the home knows: in `status`, and in `moil services`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceReport {
    pub id: ServiceId,
    pub name: String,
    pub base_url: Url,
    /// Whether this machine is paired with the service (spec/machine.md
    /// §1.2): its `pairing.json` names the base URL in its `service.json`,
    /// and, while an agent runs, the agent hasn't found its token missing
    /// as it connected. Without an agent the token isn't looked for, since
    /// reading the keychain may ask the owner first.
    pub paired: bool,
    /// While an agent runs and the service is paired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ConnectionReport>,
    /// The approvals that count (spec/machine.md §1.2), oldest first.
    pub approvals: Vec<Approval>,
    /// What the service offers, as it describes it, while an agent runs
    /// and the service is paired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offered: Option<Vec<OfferedBundle>>,
    /// The owner's hooks for the service's bundles, by bundle.
    pub hooks: Vec<HookReport>,
}

/// A service's control channel, as [`Connection`](crate::Connection)
/// tells it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum ConnectionReport {
    Connecting,
    Connected,
    Offline {
        error: String,
        #[serde(with = "crate::rfc3339")]
        retry_at: SystemTime,
    },
    /// The service removed this machine: pair again.
    Unpaired,
    /// Another connection with this machine's token took over.
    Replaced,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferedBundle {
    pub hash: BundleHash,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `false`: waiting for the owner's review.
    pub approved: bool,
}

/// A hook (spec/machine.md §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookReport {
    /// The bundle whose attempts it follows.
    pub bundle: String,
    pub path: PathBuf,
    pub log: PathBuf,
    /// Whether moil can run it: a regular file the owner can execute, that
    /// starts with a `#!` line or is a program.
    pub runnable: bool,
}

/// An element of `moil hooks --json`: a hook, with its service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookListing {
    pub service: ServiceId,
    #[serde(flatten)]
    pub hook: HookReport,
}

/// Something wrong with the home, for the owner to fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// The file or directory concerned, relative to the home.
    pub path: PathBuf,
    /// A sentence the owner can act on.
    pub message: String,
}

/// A service as reviews and hook contexts name it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceRef {
    pub id: ServiceId,
    pub name: String,
    pub base_url: Url,
}

/// An element of `moil review SERVICE --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedBundle {
    pub hash: BundleHash,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub approved: bool,
    /// `false` for an approved bundle the service no longer offers.
    pub offered: bool,
}

impl ListedBundle {
    /// A service's bundles: those it offers, as it describes them, then
    /// those approved for it that it no longer offers, oldest first.
    pub fn listing(offered: &[BundleInfo], approvals: &[Approval]) -> Vec<Self> {
        let approved = |hash: &BundleHash| approvals.iter().any(|a| a.hash == *hash);
        let is_offered = |hash: &BundleHash| offered.iter().any(|b| b.hash == *hash);
        let offered = offered.iter().map(|b| Self {
            hash: b.hash,
            name: b.name.clone(),
            version: b.version.clone(),
            description: b.description.clone(),
            approved: approved(&b.hash),
            offered: true,
        });
        let withdrawn = approvals
            .iter()
            .filter(|a| !is_offered(&a.hash))
            .map(|a| Self {
                hash: a.hash,
                name: a.name.clone(),
                version: a.version.clone(),
                description: None,
                approved: true,
                offered: false,
            });
        offered.chain(withdrawn).collect()
    }
}

/// `moil review SERVICE HASH --json`: one bundle in full.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleReview {
    pub service: ServiceRef,
    pub hash: BundleHash,
    pub approved: bool,
    /// The three files, by name, exactly as they are.
    pub files: BTreeMap<String, String>,
    pub packages: Vec<PackageReport>,
    /// The approved version this bundle would replace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<PreviousVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageReport {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Where it comes from: the artifact's URL, or the index, repository
    /// or URL the lockfile names.
    pub source: String,
    /// Built on the machine, with build tools the lockfile doesn't pin.
    pub builds_from_source: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviousVersion {
    pub hash: BundleHash,
    pub version: String,
    /// A unified diff for each file that changed, by file name.
    pub changes: BTreeMap<String, String>,
}

/// `value` as one line of JSON, with every character that text output
/// would reveal (spec §5.5) written as a `\u` escape, so that nothing in
/// it can make the JSON read differently from what it is.
pub fn json_line(value: &impl Serialize) -> String {
    let json = serde_json::to_string(value).expect("reports serialize to JSON");
    if !json.chars().any(is_hidden) {
        return json;
    }
    let mut escaped = String::with_capacity(json.len() + 16);
    for c in json.chars() {
        if is_hidden(c) {
            // serde_json has already escaped C0 controls; the rest can
            // only be inside strings, where escapes mean the same.
            let mut units = [0; 2];
            for unit in c.encode_utf16(&mut units) {
                let _ = write!(escaped, "\\u{unit:04x}");
            }
        } else {
            escaped.push(c);
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_characters_are_escaped_and_mean_the_same() {
        let message = "fine\u{202e}txt.exe \u{e0041}";
        let line = json_line(&Problem {
            path: "services".into(),
            message: message.into(),
        });
        assert!(line.is_ascii(), "{line}");
        assert!(
            line.contains(r"\u202e") && line.contains(r"\udb40\udc41"),
            "{line}"
        );
        let read: Problem = serde_json::from_str(&line).unwrap();
        assert_eq!(read.message, message);
    }

    #[test]
    fn absent_fields_are_left_out() {
        let line = json_line(&Status {
            home: "/h".into(),
            agent: None,
            paused: false,
            job: None,
            last_job: None,
            services: Vec::new(),
            problems: Vec::new(),
        });
        assert_eq!(
            line,
            r#"{"home":"/h","paused":false,"services":[],"problems":[]}"#
        );
    }

    #[test]
    fn a_listing_keeps_approved_bundles_the_service_withdrew() {
        let hash = |byte| BundleHash::from_bytes([byte; 32]);
        let offered = |byte, version: &str| BundleInfo {
            hash: hash(byte),
            name: "transcribe".into(),
            version: version.into(),
            description: Some(format!("version {version}")),
        };
        let approval = |byte, version: &str| Approval {
            hash: hash(byte),
            name: "transcribe".into(),
            version: version.into(),
            approved_at: SystemTime::UNIX_EPOCH,
        };
        let listing = ListedBundle::listing(
            &[offered(3, "3.0.0"), offered(2, "2.0.0")],
            &[approval(1, "1.0.0"), approval(2, "2.0.0")],
        );
        let summary: Vec<_> = listing
            .iter()
            .map(|b| (b.version.as_str(), b.approved, b.offered))
            .collect();
        assert_eq!(
            summary,
            [
                ("3.0.0", false, true),
                ("2.0.0", true, true),
                ("1.0.0", true, false)
            ]
        );
        assert_eq!(listing[1].description.as_deref(), Some("version 2.0.0"));
        assert_eq!(listing[2].description, None, "the service no longer says");
    }
}
