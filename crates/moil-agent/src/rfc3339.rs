//! Times as the home's files and the command line's JSON write them: RFC
//! 3339 strings in UTC, to the second, such as `2026-09-27T22:41:07Z`
//! (spec/machine.md §1). Use as `#[serde(with = "crate::rfc3339")]`, or
//! `crate::rfc3339::option` for an `Option<SystemTime>`.

use std::borrow::Cow;
use std::time::SystemTime;

use serde::{Deserialize, Deserializer, Serializer};

/// The time now, to the second, as the home's files keep it: a time
/// written and read back is the same time.
pub fn now() -> SystemTime {
    let since_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(since_epoch.as_secs())
}

/// `time` in UTC, to the second: `2026-09-27T22:41:07Z`.
pub fn format(time: SystemTime) -> String {
    humantime::format_rfc3339_seconds(time).to_string()
}

/// Reads a time [`format`] wrote, or any RFC 3339 time in UTC (`Z`), with
/// or without fractions of a second.
pub fn parse(text: &str) -> Result<SystemTime, String> {
    humantime::parse_rfc3339(text).map_err(|_| {
        format!("{text:?} isn't a time in UTC as RFC 3339 writes it, such as 2026-09-27T22:41:07Z")
    })
}

pub fn serialize<S: Serializer>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format(*time))
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SystemTime, D::Error> {
    let text = Cow::<'de, str>::deserialize(deserializer)?;
    parse(&text).map_err(serde::de::Error::custom)
}

/// For optional times, skipped when absent: pair with
/// `#[serde(default, skip_serializing_if = "Option::is_none")]`.
pub mod option {
    use super::*;

    pub fn serialize<S: Serializer>(
        time: &Option<SystemTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match time {
            Some(time) => super::serialize(time, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<SystemTime>, D::Error> {
        match Option::<Cow<'de, str>>::deserialize(deserializer)? {
            Some(text) => parse(&text).map(Some).map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    #[test]
    fn times_round_trip_to_the_second() {
        let time = UNIX_EPOCH + Duration::from_millis(1_790_000_000_750);
        let text = format(time);
        assert_eq!(text, "2026-09-21T14:13:20Z");
        assert_eq!(
            parse(&text).unwrap(),
            UNIX_EPOCH + Duration::from_secs(1_790_000_000)
        );
    }

    #[test]
    fn only_utc_is_read() {
        assert!(parse("2026-09-21T14:13:20.5Z").is_ok());
        let error = parse("2026-09-21T16:13:20+02:00").unwrap_err();
        assert!(error.contains("2026-09-27T22:41:07Z"), "{error}");
    }
}
