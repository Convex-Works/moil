//! Reading and checking `job.py.lock` (spec §5.1): the owner approves what
//! the lockfile names, so it must pin everything it can.

use moil_proto::ids::is_sha256_hex;

/// A package pinned by the lockfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedPackage {
    pub name: String,
    pub version: Option<String>,
    pub source: PackageSource,
    /// Whether the machine builds it from source, because the lockfile has
    /// no wheel for it. Its build requirements are resolved when it's built,
    /// not pinned by the lockfile, so the owner should know.
    pub builds_from_source: bool,
}

/// Where uv installs a package from. Every source is `https`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    /// A package index, such as `https://pypi.org/simple`.
    Registry(String),
    /// A git repository, pinned to a full commit.
    Git(String),
    /// A direct URL to an archive or a wheel.
    Url(String),
}

impl LockedPackage {
    /// A page about this exact version, when the source has one.
    pub fn link(&self) -> Option<String> {
        match &self.source {
            PackageSource::Registry(index) if is_pypi(index) => Some(match &self.version {
                Some(version) => format!("https://pypi.org/project/{}/{version}/", self.name),
                None => format!("https://pypi.org/project/{}/", self.name),
            }),
            PackageSource::Git(url) | PackageSource::Url(url) => Some(url.clone()),
            PackageSource::Registry(_) => None,
        }
    }
}

/// Whether `index` is PyPI's.
pub fn is_pypi(index: &str) -> bool {
    index.trim_end_matches('/') == "https://pypi.org/simple"
}

/// Parses the lockfile, checks that it pins everything it can, and lists
/// its packages. The error says what's wrong, for the bundle's author.
pub(crate) fn parse_lockfile(text: &str) -> Result<Vec<LockedPackage>, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        // The full error says where; keep it on one line.
        let full = e.to_string();
        let lines: Vec<&str> = full
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        lines.join(" ")
    })?;
    let Some(packages) = table.get("package") else {
        return Ok(Vec::new());
    };
    let packages = packages
        .as_array()
        .ok_or("`package` must be an array of tables")?;
    packages.iter().map(package).collect()
}

fn package(value: &toml::Value) -> Result<LockedPackage, String> {
    let table = value
        .as_table()
        .ok_or("every `[[package]]` must be a table")?;
    let name = table
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("a package has no name")?
        .to_owned();
    let version = table
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let fail = |why: String| format!("package {name}: {why}");

    let source = table
        .get("source")
        .and_then(|s| s.as_table())
        .ok_or_else(|| fail("it has no source".into()))?;
    let source = package_source(source).map_err(fail)?;

    let sdist = table.get("sdist");
    let wheels = match table.get("wheels") {
        Some(wheels) => wheels
            .as_array()
            .ok_or_else(|| fail("`wheels` must be an array".into()))?
            .as_slice(),
        None => &[],
    };
    for artifact in sdist.into_iter().chain(wheels) {
        let fields = artifact.as_table();
        let hash = fields.and_then(|a| a.get("hash")).and_then(|h| h.as_str());
        if !hash
            .and_then(|h| h.strip_prefix("sha256:"))
            .is_some_and(is_sha256_hex)
        {
            let default = if sdist == Some(artifact) {
                "its sdist"
            } else {
                "a wheel"
            };
            let what = fields
                .and_then(|a| a.get("url"))
                .and_then(|u| u.as_str())
                .unwrap_or(default);
            return Err(fail(format!(
                "{what} has no SHA-256 hash, so it isn't pinned"
            )));
        }
    }
    let has_artifacts = sdist.is_some() || !wheels.is_empty();
    if !has_artifacts && !matches!(source, PackageSource::Git(_)) {
        return Err(fail("it lists nothing to install".into()));
    }

    Ok(LockedPackage {
        name,
        version,
        source,
        builds_from_source: wheels.is_empty(),
    })
}

fn package_source(source: &toml::Table) -> Result<PackageSource, String> {
    let get = |key: &str| source.get(key).and_then(|v| v.as_str());
    if let Some(index) = get("registry") {
        https(index, "its package index")?;
        Ok(PackageSource::Registry(index.to_owned()))
    } else if let Some(url) = get("url") {
        https(url, "its URL")?;
        Ok(PackageSource::Url(url.to_owned()))
    } else if let Some(url) = get("git") {
        https(url, "its git repository")?;
        let commit = url
            .rsplit_once('#')
            .map(|(_, commit)| commit)
            .unwrap_or_default();
        if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "its git source {url} isn't pinned to a full commit"
            ));
        }
        Ok(PackageSource::Git(url.to_owned()))
    } else if let Some(path) = ["path", "directory", "editable", "virtual"]
        .into_iter()
        .find_map(get)
    {
        Err(format!(
            "it comes from {path}, a path on the author's machine that other machines don't have"
        ))
    } else {
        Err(format!(
            "its source {} isn't one moil knows",
            source.to_string().trim()
        ))
    }
}

fn https(url: &str, what: &str) -> Result<(), String> {
    let ok = url::Url::parse(url).is_ok_and(|u| u.scheme() == "https" && u.has_host());
    if ok {
        Ok(())
    } else {
        Err(format!("{what}, {url}, isn't https"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `spec/vectors/lockfiles.json`
    #[test]
    fn lockfile_vectors() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../spec/vectors/lockfiles.json"
        );
        let vectors: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let cases = vectors["cases"].as_array().unwrap();
        assert!(cases.len() > 15);
        for case in cases {
            let name = case["case"].as_str().unwrap();
            let result = parse_lockfile(case["lockfile"].as_str().unwrap());
            if case["valid"].as_bool().unwrap() {
                let packages = result.unwrap_or_else(|e| panic!("{name}: expected valid, got {e}"));
                let built: Vec<&str> = packages
                    .iter()
                    .filter(|p| p.builds_from_source)
                    .map(|p| p.name.as_str())
                    .collect();
                let expected: Vec<&str> = case["builds_from_source"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect();
                assert_eq!(built, expected, "{name}");
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
                builds_from_source: false,
            }]
        );
        assert_eq!(
            packages[0].link().as_deref(),
            Some("https://pypi.org/project/idna/3.20/")
        );
    }

    #[test]
    fn errors_say_which_package_and_why() {
        // The attack the policy closes: an index the service controls serves
        // an unhashed sdist, which it could change after approval.
        let text = r#"
            [[package]]
            name = "evilpkg"
            version = "0.1"
            source = { registry = "https://evil.example.com/simple" }
            sdist = { url = "https://evil.example.com/evilpkg-0.1.tar.gz", size = 900 }
        "#;
        assert_eq!(
            parse_lockfile(text).unwrap_err(),
            "package evilpkg: https://evil.example.com/evilpkg-0.1.tar.gz has no SHA-256 hash, so it isn't pinned"
        );
    }

    #[test]
    fn toml_errors_say_where() {
        let error = parse_lockfile("version = 1\nrevision = = 3\n").unwrap_err();
        assert!(error.contains("line 2"), "{error}");
        assert!(!error.contains('\n'), "{error}");
    }
}
