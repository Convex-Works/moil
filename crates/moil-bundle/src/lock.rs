//! Reading `job.py.lock`, to show the owner what a bundle installs.

/// A package pinned by the lockfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedPackage {
    pub name: String,
    pub version: Option<String>,
    pub source: PackageSource,
}

/// Where uv installs a package from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    /// A package index, such as `https://pypi.org/simple`.
    Registry(String),
    /// A git repository, pinned to a commit.
    Git(String),
    /// A direct URL to an archive.
    Url(String),
    /// A path on the author's machine. It won't exist on another machine.
    Path(String),
    /// A source this version of moil doesn't know.
    Other(String),
}

impl LockedPackage {
    /// A page about this exact version, when the source has one.
    pub fn link(&self) -> Option<String> {
        match &self.source {
            PackageSource::Registry(index)
                if index.trim_end_matches('/') == "https://pypi.org/simple" =>
            {
                Some(match &self.version {
                    Some(version) => format!("https://pypi.org/project/{}/{version}/", self.name),
                    None => format!("https://pypi.org/project/{}/", self.name),
                })
            }
            PackageSource::Git(url) | PackageSource::Url(url) => Some(url.clone()),
            _ => None,
        }
    }
}

/// Parses the lockfile and lists its packages. Fails only if it isn't TOML;
/// unexpected shapes are shown as they are rather than rejected.
pub(crate) fn parse_lockfile(text: &str) -> Result<Vec<LockedPackage>, String> {
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| e.message().to_owned())?;
    let Some(packages) = table.get("package").and_then(|p| p.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(packages
        .iter()
        .filter_map(|p| p.as_table())
        .map(|p| {
            let str_field = |key: &str| p.get(key).and_then(|v| v.as_str()).map(str::to_owned);
            LockedPackage {
                name: str_field("name").unwrap_or_else(|| "(unnamed)".to_owned()),
                version: str_field("version"),
                source: source(p.get("source").and_then(|s| s.as_table())),
            }
        })
        .collect())
}

fn source(table: Option<&toml::Table>) -> PackageSource {
    let Some(table) = table else {
        return PackageSource::Other("(none)".to_owned());
    };
    let get = |key: &str| table.get(key).and_then(|v| v.as_str()).map(str::to_owned);
    if let Some(index) = get("registry") {
        PackageSource::Registry(index)
    } else if let Some(url) = get("git") {
        PackageSource::Git(url)
    } else if let Some(url) = get("url") {
        PackageSource::Url(url)
    } else if let Some(path) = get("path")
        .or_else(|| get("directory"))
        .or_else(|| get("editable"))
    {
        PackageSource::Path(path)
    } else {
        PackageSource::Other(table.to_string().trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_packages_with_links() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../spec/vectors/bundles/crlf-unicode/job.py.lock"
        ))
        .unwrap();
        let packages = parse_lockfile(&text).unwrap();
        assert_eq!(
            packages,
            [LockedPackage {
                name: "idna".into(),
                version: Some("3.20".into()),
                source: PackageSource::Registry("https://pypi.org/simple".into()),
            }]
        );
        assert_eq!(
            packages[0].link().as_deref(),
            Some("https://pypi.org/project/idna/3.20/")
        );
    }

    #[test]
    fn a_lockfile_without_packages_is_fine() {
        assert_eq!(parse_lockfile("version = 1\nrevision = 3\n").unwrap(), []);
    }

    #[test]
    fn unusual_sources_are_reported_not_rejected() {
        let text = r#"
            [[package]]
            name = "mine"
            version = "0.1.0"
            source = { directory = "../mine" }

            [[package]]
            name = "patched"
            version = "1.0"
            source = { git = "https://github.com/x/patched?rev=abc#abc" }
        "#;
        let packages = parse_lockfile(text).unwrap();
        assert_eq!(packages[0].source, PackageSource::Path("../mine".into()));
        assert_eq!(packages[0].link(), None);
        assert!(matches!(packages[1].source, PackageSource::Git(_)));
    }

    #[test]
    fn invalid_toml_is_an_error() {
        assert!(parse_lockfile("version = = 1").is_err());
    }
}
