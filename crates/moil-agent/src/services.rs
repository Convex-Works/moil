//! The services a home knows, this machine's pairings with them, and what
//! the owner approved for each (spec/machine.md §1.2).

use std::fmt;
use std::str::FromStr;
use std::time::SystemTime;

use moil_proto::BundleHash;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::Error;
use crate::report::ServiceRef;

/// The home's name for a service, which names its folder: derived from the
/// service's host and port (`klisi.example.com`, `localhost-8787`) when
/// the machine first pairs with it, unique among the home's services, and
/// safe as a file name. An ID is 1 to 128
/// lowercase ASCII letters, digits, dots and hyphens, starting and ending
/// with a letter or digit, so it can't name a hidden file or leave a
/// directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServiceId(String);

impl ServiceId {
    /// The longest an ID can be, in characters.
    pub const MAX_LEN: usize = 128;

    /// An ID written out, such as one read back from a file. Fails unless
    /// it follows the rules above; [`derive`](Self::derive) makes new ones.
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidServiceId> {
        let id = id.into();
        if is_service_id(&id) {
            Ok(Self(id))
        } else {
            Err(InvalidServiceId(id))
        }
    }

    /// The ID for a new service at `base_url`, skipping IDs `taken` says are
    /// in use: `klisi.example.com`, then `klisi.example.com-2`, and so on.
    pub fn derive(base_url: &Url, taken: impl Fn(&str) -> bool) -> Self {
        let host = base_url.host_str().unwrap_or_default();
        let mut stem = match base_url.port() {
            Some(port) => format!("{host}-{port}"),
            None => host.to_owned(),
        };
        stem = file_safe(&stem);
        if stem.is_empty() {
            stem = "service".to_owned();
        }
        let id = (1..)
            .map(|n| match n {
                1 => stem.clone(),
                n => format!("{stem}-{n}"),
            })
            .find(|id| !taken(id))
            .expect("some suffix is free");
        debug_assert!(is_service_id(&id), "derived an invalid ID {id:?}");
        Self(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_service_id(s: &str) -> bool {
    let edge = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let bytes = s.as_bytes();
    (1..=ServiceId::MAX_LEN).contains(&bytes.len())
        && bytes.first().is_some_and(edge)
        && bytes.last().is_some_and(edge)
        && bytes.iter().all(|b| edge(b) || matches!(b, b'.' | b'-'))
}

/// `s` lowercased, with anything but letters, digits, `.` and `-` turned
/// into single hyphens, at most 100 characters (leaving room for a suffix),
/// and no leading or trailing punctuation.
fn file_safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().map(|c| c.to_ascii_lowercase()) {
        let c = if c.is_ascii_alphanumeric() || c == '.' {
            c
        } else {
            '-'
        };
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    let out: String = out.trim_matches(['-', '.']).chars().take(100).collect();
    out.trim_end_matches(['-', '.']).to_owned()
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ServiceId {
    type Err = InvalidServiceId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for ServiceId {
    type Error = InvalidServiceId;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        Self::new(id)
    }
}

impl From<ServiceId> for String {
    fn from(id: ServiceId) -> Self {
        id.0
    }
}

/// A string that breaks the rules for a [`ServiceId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{0:?} isn't a service ID: IDs are 1 to 128 lowercase letters, digits, dots and hyphens, starting and ending with a letter or digit"
)]
pub struct InvalidServiceId(String);

/// A service's base URL from what the owner typed, or a link supplied:
/// surrounding whitespace is ignored, `https://` is assumed when there's no
/// scheme, and a trailing slash is dropped. It must be an `http` or `https`
/// URL with a host, and without a user name, password, query or fragment,
/// which a base URL has no use for. Whether plain `http` is acceptable is
/// checked when pairing (spec §3).
pub fn parse_base_url(input: &str) -> Result<Url, Error> {
    let input = input.trim();
    let refuse = |why: String| Err(Error::InvalidAddress(why));
    if input.is_empty() {
        return refuse(
            "Enter the service's address, such as https://klisi.example.com/moil.".into(),
        );
    }
    let with_scheme;
    let input = if input.contains("://") {
        input
    } else {
        with_scheme = format!("https://{input}");
        &with_scheme
    };
    let mut url = match Url::parse(input) {
        Ok(url) => url,
        Err(e) => return refuse(format!("{input} isn't a valid address: {e}.")),
    };
    if !matches!(url.scheme(), "https" | "http") {
        return refuse(format!(
            "moil services have https addresses, not {}:// ones.",
            url.scheme()
        ));
    }
    if url.host().is_none() {
        return refuse(format!("{input} doesn't name a host."));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return refuse("The address mustn't contain a user name or password.".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return refuse("The address can't contain a query (?…) or a fragment (#…).".into());
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url)
}

/// Fails unless `name` may be what a service calls itself (spec §4): 1 to
/// 64 characters, none of them a control character. Says why, as a reason
/// for [`Error::NotAService`](crate::Error::NotAService).
pub(crate) fn check_name(name: &str) -> Result<(), String> {
    let chars = name.chars().count();
    if name.chars().any(char::is_control) {
        Err(format!(
            "it calls itself {name:?}, which has control characters"
        ))
    } else if !(1..=MAX_NAME_CHARS).contains(&chars) {
        Err(format!(
            "it calls itself {name:?}, which isn't 1 to {MAX_NAME_CHARS} characters long"
        ))
    } else {
        Ok(())
    }
}

/// The longest name a service may give itself, in characters (spec §10).
const MAX_NAME_CHARS: usize = 64;

/// A service the home knows: a folder `services/<id>/` whose
/// `service.json` is valid (spec/machine.md §1.2), whether or not this
/// machine is paired with it. It serializes only for the control socket
/// (spec/machine.md §4); the home keeps it as `service.json` and
/// `pairing.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Service {
    /// The name of the service's folder.
    pub id: ServiceId,
    /// Where the service's moil endpoints live, e.g. `https://klisi.example.com/moil`.
    pub base_url: Url,
    /// The name the service gave itself when pairing, e.g. `klisi`.
    pub name: String,
    /// The approvals that count (spec/machine.md §1.2), oldest first.
    /// Those that don't are left out, and reported as problems.
    pub approvals: Vec<Approval>,
    /// This machine's pairing with the service, if its `pairing.json` names
    /// [`base_url`](Self::base_url). The service is paired if moil also
    /// holds its token: see [`Machine::is_paired`](crate::Machine::is_paired).
    pub pairing: Option<Pairing>,
}

impl Service {
    pub fn is_approved(&self, hash: &BundleHash) -> bool {
        self.approvals.iter().any(|a| a.hash == *hash)
    }

    /// The most approved hashes a service is told about (spec §10).
    pub const MAX_ANNOUNCED: usize = 1024;

    /// The approved hashes `hello` and `approved` carry, oldest first: all
    /// of them, or the [`MAX_ANNOUNCED`](Self::MAX_ANNOUNCED) approved most
    /// recently.
    pub fn approved_hashes(&self) -> Vec<BundleHash> {
        let older = self.unannounced_approvals();
        self.approvals[older..].iter().map(|a| a.hash).collect()
    }

    /// How many approvals are too old to be among those the service is told
    /// about.
    pub fn unannounced_approvals(&self) -> usize {
        self.approvals.len().saturating_sub(Self::MAX_ANNOUNCED)
    }

    /// Whether the machine reaches this service over plain `http`.
    pub fn is_insecure(&self) -> bool {
        self.base_url.scheme() == "http"
    }
}

impl From<&Service> for ServiceRef {
    fn from(service: &Service) -> Self {
        Self {
            id: service.id.clone(),
            name: service.name.clone(),
            base_url: service.base_url.clone(),
        }
    }
}

/// This machine's pairing with a service: its `pairing.json`, which
/// belongs to this machine alone. The token that goes with it is kept in
/// the [`SecretStore`](crate::SecretStore), never in the service's folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pairing {
    /// The base URL the machine paired with: the only one its token is
    /// ever sent to.
    #[serde(with = "base_url")]
    pub base_url: Url,
    /// The service's ID for this machine.
    pub machine_id: String,
    #[serde(with = "crate::rfc3339")]
    pub paired_at: SystemTime,
}

/// One approved bundle, as `service.json` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub hash: BundleHash,
    /// The bundle's name and version, from its verified manifest.
    pub name: String,
    pub version: String,
    #[serde(with = "crate::rfc3339")]
    pub approved_at: SystemTime,
}

/// `service.json`: which service a folder is for, and what the owner
/// approved for it, including approvals that don't count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceFile {
    #[serde(with = "base_url")]
    pub base_url: Url,
    pub name: String,
    #[serde(default)]
    pub approvals: Vec<Approval>,
}

impl ServiceFile {
    /// Why the file is invalid beyond what serde checks, if it is.
    pub fn check(&self) -> Result<(), String> {
        let chars = self.name.chars().count();
        if (1..=MAX_NAME_CHARS).contains(&chars) && !self.name.chars().any(char::is_control) {
            Ok(())
        } else {
            Err(format!(
                "name must be 1 to {MAX_NAME_CHARS} characters, none of them control characters"
            ))
        }
    }
}

impl Pairing {
    /// Why the file is invalid beyond what serde checks, if it is.
    pub(crate) fn check(&self) -> Result<(), String> {
        if moil_proto::ids::is_machine_id(&self.machine_id) {
            Ok(())
        } else {
            Err(format!("{:?} isn't a machine ID", self.machine_id))
        }
    }
}

/// Base URLs in the home's files, read as [`parse_base_url`] reads what the
/// owner types.
mod base_url {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::borrow::Cow;
    use url::Url;

    pub fn serialize<S: Serializer>(url: &Url, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(url.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Url, D::Error> {
        let text = Cow::<'de, str>::deserialize(deserializer)?;
        super::parse_base_url(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(url: &str, taken: &[&str]) -> String {
        ServiceId::derive(&Url::parse(url).unwrap(), |c| taken.contains(&c)).0
    }

    #[test]
    fn ids_come_from_host_and_port() {
        assert_eq!(
            id("https://klisi.example.com/moil", &[]),
            "klisi.example.com"
        );
        assert_eq!(
            id("https://Klisi.Example.com:443/moil", &[]),
            "klisi.example.com"
        );
        assert_eq!(id("http://localhost:8787/moil", &[]), "localhost-8787");
        assert_eq!(id("http://127.0.0.1:9000", &[]), "127.0.0.1-9000");
        assert_eq!(id("http://[::1]:9000/", &[]), "1-9000");
    }

    #[test]
    fn ids_are_unique_among_the_machines_services() {
        let taken = ["klisi.example.com", "klisi.example.com-2"];
        assert_eq!(
            id("https://klisi.example.com/other", &taken),
            "klisi.example.com-3"
        );
    }

    #[test]
    fn derived_ids_are_valid_whatever_the_host() {
        let long = format!("https://{}.{}.example.com/", "a".repeat(60), "b".repeat(38));
        assert_eq!(
            id(&long, &[]),
            format!("{}.{}", "a".repeat(60), "b".repeat(38)),
            "cut to 100 characters without a dot at the end"
        );
        for url in [
            long.as_str(),
            "https://xn--bcher-kva.example/",
            "http://[::1]/",
            "http://[fe80::1]:8080/",
            "file:///tmp/moil",
            "https://-.example.com./",
        ] {
            for taken in [&[][..], &[id(url, &[]).as_str()][..]] {
                let derived = id(url, taken);
                assert_eq!(
                    ServiceId::new(derived.clone()).map(String::from),
                    Ok(derived.clone()),
                    "{url} gave {derived:?}"
                );
            }
        }
    }

    #[test]
    fn written_ids_must_be_safe_file_names() {
        for good in ["klisi.example.com", "localhost-8787", "1-9000", "a", "a..b"] {
            assert_eq!(good.parse::<ServiceId>().unwrap().as_str(), good);
        }
        let longest = "x".repeat(ServiceId::MAX_LEN);
        assert!(ServiceId::new(longest.clone()).is_ok());
        for bad in [
            "",
            ".",
            "..",
            "../etc",
            ".hidden",
            "a/b",
            "a\\b",
            "Klisi.example.com",
            "klisi.example.com.",
            "-klisi",
            "klisi-",
            "kl isi",
            "bücher.example",
            &format!("{longest}x"),
        ] {
            let err = ServiceId::new(bad).unwrap_err();
            assert!(err.to_string().contains("isn't a service ID"), "{err}");
        }
    }

    #[test]
    fn base_urls_come_from_what_the_owner_typed() {
        let url = |s| parse_base_url(s).unwrap().to_string();
        assert_eq!(
            url("https://klisi.example.com/moil/"),
            "https://klisi.example.com/moil"
        );
        assert_eq!(
            url("  klisi.example.com/moil \n"),
            "https://klisi.example.com/moil"
        );
        assert_eq!(
            url("http://127.0.0.1:8787/moil"),
            "http://127.0.0.1:8787/moil"
        );
        assert_eq!(url("https://a.example"), "https://a.example/");
        for (input, why) in [
            ("", "Enter the service's address"),
            ("ftp://a.example", "not ftp:// ones"),
            ("https://", "isn't a valid address"),
            ("https://u:p@a.example/", "user name or password"),
            ("https://a.example/moil?x=1", "query"),
            ("https://a.example/moil#top", "fragment"),
        ] {
            let error = parse_base_url(input).unwrap_err().to_string();
            assert!(error.contains(why), "{input:?}: {error}");
        }
    }

    #[test]
    fn ids_are_checked_when_read_back() {
        let id: ServiceId = serde_json::from_str("\"klisi.example.com\"").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"klisi.example.com\"");
        assert!(serde_json::from_str::<ServiceId>("\"../../etc/passwd\"").is_err());
    }
}
