//! Identifiers and the rules for their formats (spec §2).

use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A value that doesn't have the format its kind requires.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind}: {value:?}")]
pub struct InvalidId {
    kind: &'static str,
    value: String,
}

impl InvalidId {
    fn new(kind: &'static str, value: impl Into<String>) -> Self {
        let mut value = value.into();
        if value.len() > 80 {
            let mut end = 80;
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            value.truncate(end);
            value.push('…');
        }
        Self { kind, value }
    }
}

/// The SHA-256 of a bundle (spec §5.2), written as 64 lowercase hex
/// characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BundleHash([u8; 32]);

impl BundleHash {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The first 12 hex characters, for places where the full hash doesn't
    /// fit. Never use it to identify a bundle.
    pub fn short(&self) -> String {
        self.to_string()[..12].to_owned()
    }
}

impl fmt::Display for BundleHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for BundleHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BundleHash({self})")
    }
}

impl FromStr for BundleHash {
    type Err = InvalidId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidId::new("bundle hash", s);
        if !is_sha256_hex(s) {
            return Err(invalid());
        }
        let mut bytes = [0u8; 32];
        for (i, pair) in s.as_bytes().chunks_exact(2).enumerate() {
            let hi = hex_value(pair[0]).ok_or_else(invalid)?;
            let lo = hex_value(pair[1]).ok_or_else(invalid)?;
            bytes[i] = hi << 4 | lo;
        }
        Ok(Self(bytes))
    }
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

impl Serialize for BundleHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for BundleHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal, $check:path) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidId> {
                let value = value.into();
                if $check(&value) {
                    Ok(Self(value))
                } else {
                    Err(InvalidId::new($kind, value))
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:?})", stringify!($name), self.0)
            }
        }

        impl FromStr for $name {
            type Err = InvalidId;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidId;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = InvalidId;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

string_id!(
    /// A job ID, chosen by the service: `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`.
    JobId,
    "job ID",
    is_job_id
);

string_id!(
    /// The name of an input or output file: `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`.
    /// The format keeps names safe to use as a single path component.
    FileName,
    "file name",
    is_file_name
);

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`
pub fn is_job_id(s: &str) -> bool {
    is_segment(s) && s.len() <= 128
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`
pub fn is_file_name(s: &str) -> bool {
    is_segment(s) && s.len() <= 128
}

/// 1–64 characters of `[A-Za-z0-9_-]`.
pub fn is_machine_id(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `/`-separated segments, each `^[A-Za-z0-9][A-Za-z0-9._-]*$`, at most 256
/// characters in total.
pub fn is_asset_name(s: &str) -> bool {
    s.len() <= 256 && s.split('/').all(is_segment)
}

/// `^[a-z0-9][a-z0-9-]{0,63}$`
pub fn is_bundle_name(s: &str) -> bool {
    let bytes = s.as_bytes();
    (1..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// 64 lowercase hex characters.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_segment(s: &str) -> bool {
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_hash_round_trips_and_rejects_other_formats() {
        let hex = "a00a65c166291e905da7546a67518a81cc27cb498f009c75b824f28a8adfffb2";
        let hash: BundleHash = hex.parse().unwrap();
        assert_eq!(hash.to_string(), hex);
        assert_eq!(hash.short(), "a00a65c16629");
        assert_eq!(serde_json::to_string(&hash).unwrap(), format!("\"{hex}\""));

        for bad in [
            &hex[..63],
            &hex.to_uppercase(),
            &format!("{hex}0"),
            "",
            &hex.replace('a', "g"),
        ] {
            assert!(
                bad.parse::<BundleHash>().is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn file_names_are_single_safe_path_components() {
        for good in [
            "a",
            "transcript.json",
            "audio.ogg",
            "A-b_c.d",
            &"x".repeat(128),
        ] {
            assert!(FileName::new(good).is_ok(), "{good:?} should be accepted");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "-x",
            "a/b",
            "a\\b",
            "é.txt",
            "a b",
            &"x".repeat(129),
        ] {
            assert!(FileName::new(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn asset_names_are_relative_paths_without_escapes() {
        for good in ["model.bin", "whisper/model.bin", "a/b/c.d"] {
            assert!(is_asset_name(good), "{good:?} should be accepted");
        }
        for bad in [
            "",
            "/a",
            "a/",
            "a//b",
            "../a",
            "a/../b",
            "./a",
            "a/.b",
            &"a".repeat(257),
        ] {
            assert!(!is_asset_name(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn deserializing_an_invalid_id_fails() {
        let err = serde_json::from_str::<JobId>("\"../etc\"").unwrap_err();
        assert!(err.to_string().contains("invalid job ID"), "{err}");
    }
}
