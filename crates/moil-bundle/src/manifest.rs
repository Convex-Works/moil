use std::collections::BTreeMap;

use moil_proto::ids::{is_asset_name, is_bundle_name, is_sha256_hex};
use serde::{Deserialize, Serialize};

/// A bundle's `manifest.json` (spec §5.3). Parsing is strict: unknown fields
/// anywhere make the manifest invalid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub moil: u32,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Asset name → where to fetch it and its hash.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub assets: BTreeMap<String, Asset>,
    #[serde(default, skip_serializing_if = "Resources::is_empty")]
    pub resources: Resources,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub url: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

/// What the bundle needs. Informational in v1.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuRequirement>,
}

impl Resources {
    pub fn is_empty(&self) -> bool {
        self.memory_bytes.is_none() && self.gpu.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuRequirement {
    None,
    Optional,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest.json isn't valid: {0}")]
    Json(String),
    #[error("manifest.json must be a JSON object")]
    NotAnObject,
    #[error("manifest format {0} isn't supported (expected \"moil\": 1)")]
    UnsupportedVersion(u32),
    #[error(
        "bundle name {0:?} must be 1–64 characters of a-z, 0-9 and '-', starting with a letter or digit"
    )]
    InvalidName(String),
    #[error("version must be 1–32 characters")]
    InvalidVersion,
    #[error("description must be at most 500 characters")]
    DescriptionTooLong,
    #[error(
        "asset name {0:?} must be a relative path of segments made of A-Z, a-z, 0-9, '.', '_' and '-'"
    )]
    InvalidAssetName(String),
    #[error("asset {name:?} must be fetched over https, not {url:?}")]
    InvalidAssetUrl { name: String, url: String },
    #[error("asset {name:?} has an invalid sha256 (expected 64 lowercase hex characters)")]
    InvalidAssetHash { name: String },
    #[error(
        "assets {file:?} and {nested:?} can't both exist: {file:?} would have to be a file and a directory"
    )]
    AssetLayoutConflict { file: String, nested: String },
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        // Duplicate keys would let the manifest say one thing to the owner
        // reading it and another to a parser that keeps the last value.
        serde_json::from_str::<NoDuplicateKeys>(text)
            .map_err(|e| ManifestError::Json(e.to_string()))?;
        // Go through a Value: serde would otherwise accept a JSON array as a
        // struct, field by field.
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| ManifestError::Json(e.to_string()))?;
        if !value.is_object() {
            return Err(ManifestError::NotAnObject);
        }
        let manifest: Self =
            serde_json::from_value(value).map_err(|e| ManifestError::Json(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        if self.moil != 1 {
            return Err(ManifestError::UnsupportedVersion(self.moil));
        }
        if !is_bundle_name(&self.name) {
            return Err(ManifestError::InvalidName(self.name.clone()));
        }
        if !(1..=32).contains(&self.version.chars().count()) {
            return Err(ManifestError::InvalidVersion);
        }
        if self
            .description
            .as_ref()
            .is_some_and(|d| d.chars().count() > 500)
        {
            return Err(ManifestError::DescriptionTooLong);
        }
        for (name, asset) in &self.assets {
            if !is_asset_name(name) {
                return Err(ManifestError::InvalidAssetName(name.clone()));
            }
            let https = url::Url::parse(&asset.url)
                .is_ok_and(|u| u.scheme() == "https" && u.host().is_some());
            if !https {
                return Err(ManifestError::InvalidAssetUrl {
                    name: name.clone(),
                    url: asset.url.clone(),
                });
            }
            if !is_sha256_hex(&asset.sha256) {
                return Err(ManifestError::InvalidAssetHash { name: name.clone() });
            }
        }
        if let Some((file, nested)) = layout_conflict(self.assets.keys().map(String::as_str)) {
            return Err(ManifestError::AssetLayoutConflict {
                file: file.to_owned(),
                nested: nested.to_owned(),
            });
        }
        Ok(())
    }
}

/// Two asset names that can't both be laid out on disk, because one would
/// have to be a file and a directory at once (`a` and `a/b`). `names` must
/// be sorted, as a `BTreeMap`'s keys are.
fn layout_conflict<'a>(names: impl Iterator<Item = &'a str>) -> Option<(&'a str, &'a str)> {
    let names: Vec<&str> = names.collect();
    // In sorted order a name's descendants come after it, possibly after
    // names that merely share a prefix, like `a.bin` or `a-b`.
    names.iter().enumerate().find_map(|(i, parent)| {
        names[i + 1..]
            .iter()
            .take_while(|other| other.starts_with(parent))
            .find(|other| other.as_bytes().get(parent.len()) == Some(&b'/'))
            .map(|child| (*parent, *child))
    })
}

/// Accepts any JSON without duplicate object keys, at any depth.
struct NoDuplicateKeys;

impl<'de> Deserialize<'de> for NoDuplicateKeys {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(NoDuplicateKeysVisitor)
    }
}

struct NoDuplicateKeysVisitor;

impl<'de> serde::de::Visitor<'de> for NoDuplicateKeysVisitor {
    type Value = NoDuplicateKeys;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JSON")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut seen = std::collections::HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!("duplicate key {key:?}")));
            }
            map.next_value::<NoDuplicateKeys>()?;
        }
        Ok(NoDuplicateKeys)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<NoDuplicateKeys>()?.is_some() {}
        Ok(NoDuplicateKeys)
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(NoDuplicateKeys)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(NoDuplicateKeys)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(NoDuplicateKeys)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(NoDuplicateKeys)
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(NoDuplicateKeys)
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(NoDuplicateKeys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `spec/vectors/manifests.json`
    #[test]
    fn manifest_vectors() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../spec/vectors/manifests.json"
        );
        let vectors: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let cases = vectors["cases"].as_array().unwrap();
        assert!(cases.len() > 20);
        for case in cases {
            let name = case["case"].as_str().unwrap();
            let text = case["manifest"].as_str().unwrap();
            let result = Manifest::parse(text);
            if case["valid"].as_bool().unwrap() {
                assert!(
                    result.is_ok(),
                    "{name}: expected valid, got {:?}",
                    result.unwrap_err()
                );
            } else {
                assert!(
                    result.is_err(),
                    "{name}: expected invalid, got {:?}",
                    result.unwrap()
                );
            }
        }
    }

    #[test]
    fn an_asset_name_cant_be_both_a_file_and_a_directory() {
        let conflicting = ["whisper", "whisper-x", "whisper.bin", "whisper/model.bin"];
        assert_eq!(
            layout_conflict(conflicting.into_iter()),
            Some(("whisper", "whisper/model.bin"))
        );
        let fine = ["w", "whisper.bin", "whisper/model.bin", "whisper/vocab.txt"];
        assert_eq!(layout_conflict(fine.into_iter()), None);
    }

    #[test]
    fn a_json_array_is_not_a_manifest() {
        assert_eq!(
            Manifest::parse(r#"[1, "echo", "1"]"#),
            Err(ManifestError::NotAnObject)
        );
    }

    #[test]
    fn duplicate_keys_are_rejected_at_any_depth() {
        let top = r#"{"moil": 1, "name": "echo", "name": "other", "version": "1"}"#;
        let err = Manifest::parse(top).unwrap_err();
        assert!(err.to_string().contains("duplicate key \"name\""), "{err}");

        let asset = r#"{"url": "https://e.com/a", "sha256": "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}"#;
        let nested = format!(
            r#"{{"moil": 1, "name": "echo", "version": "1", "assets": {{"a": {asset}, "a": {asset}}}}}"#
        );
        assert!(matches!(
            Manifest::parse(&nested),
            Err(ManifestError::Json(_))
        ));
    }
}
