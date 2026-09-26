//! Events: what a script writes to stdout, and what the machine forwards to
//! the service (spec §8.3).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An event the machine forwards to the service in an `event` message.
///
/// On the wire each event also carries `"v": 1`; see [`versioned`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEvent {
    /// Emitted by the runtime, never by a script.
    Phase {
        phase: Phase,
    },
    Progress {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fraction: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Log {
        #[serde(default)]
        level: LogLevel,
        message: String,
    },
    Data {
        payload: Value,
    },
}

impl JobEvent {
    pub fn log(level: LogLevel, message: impl Into<String>) -> Self {
        Self::Log {
            level,
            message: message.into(),
        }
    }
}

/// Where an attempt is, as reported by the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Creating the environment and fetching assets.
    Preparing,
    /// Downloading inputs.
    Downloading,
    /// The script is running.
    Running,
    /// Uploading outputs.
    Uploading,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

/// An event a script writes to stdout, in its strict form.
///
/// The runtime is lenient about what it accepts (spec §8.3); this type is
/// what a well-formed line parses into. `v` is ignored when parsing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScriptEvent {
    Progress {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fraction: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Log {
        #[serde(default)]
        level: LogLevel,
        message: String,
    },
    Data {
        payload: Value,
    },
    Result {
        #[serde(default)]
        files: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    Error {
        message: String,
        #[serde(default)]
        retryable: bool,
    },
}

/// Serde helpers that add `"v": 1` when serializing an event and ignore it
/// when deserializing. Use with `#[serde(with = "moil_proto::event::versioned")]`,
/// or call [`versioned::to_value`] directly.
pub mod versioned {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize)]
    struct Versioned<'a, T> {
        v: u32,
        #[serde(flatten)]
        inner: &'a T,
    }

    pub fn serialize<T: Serialize, S: Serializer>(
        value: &T,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        Versioned {
            v: crate::SCRIPT_CONTRACT_VERSION,
            inner: value,
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<T, D::Error> {
        T::deserialize(deserializer)
    }

    /// `value` as a JSON object with `"v": 1` added.
    pub fn to_value<T: Serialize>(value: &T) -> serde_json::Result<serde_json::Value> {
        serde_json::to_value(Versioned {
            v: crate::SCRIPT_CONTRACT_VERSION,
            inner: value,
        })
    }
}
