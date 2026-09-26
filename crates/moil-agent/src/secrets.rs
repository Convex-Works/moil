//! Where machine tokens are kept.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::fs::{PRIVATE, read_json, write_json};
use crate::{Error, Home};

/// The secret store to use for `home`: `$MOIL_SECRETS` (`keychain` or
/// `file`) if set, else the keychain on macOS and a file elsewhere.
pub fn default_secrets(home: &Home) -> Result<Arc<dyn SecretStore>, Error> {
    let choice = std::env::var("MOIL_SECRETS").ok();
    secrets_for(choice.as_deref().filter(|v| !v.is_empty()), home)
}

/// The store `$MOIL_SECRETS` asks for, if it is set.
fn secrets_for(choice: Option<&str>, home: &Home) -> Result<Arc<dyn SecretStore>, Error> {
    let file = || -> Arc<dyn SecretStore> { Arc::new(FileSecrets::new(home.secrets_file())) };
    match choice {
        Some("file") => Ok(file()),
        Some("keychain") => keychain(),
        Some(other) => Err(Error::Config(format!(
            "$MOIL_SECRETS must be \"keychain\" or \"file\", not {other:?}"
        ))),
        None if cfg!(target_os = "macos") => keychain(),
        None => Ok(file()),
    }
}

#[cfg(target_os = "macos")]
fn keychain() -> Result<Arc<dyn SecretStore>, Error> {
    Ok(Arc::new(KeychainSecrets))
}

#[cfg(not(target_os = "macos"))]
fn keychain() -> Result<Arc<dyn SecretStore>, Error> {
    Err(Error::Config(
        "moil can only keep tokens in the keychain on macOS; set $MOIL_SECRETS=file".into(),
    ))
}

/// Stores secrets (machine tokens) by key.
pub trait SecretStore: Send + Sync + std::fmt::Debug + 'static {
    fn get(&self, key: &str) -> Result<Option<String>, Error>;
    fn set(&self, key: &str, secret: &str) -> Result<(), Error>;
    fn delete(&self, key: &str) -> Result<(), Error>;
}

/// Secrets in a JSON file readable only by the owner (mode 0600). For
/// headless machines and tests; the app uses the keychain.
#[derive(Debug)]
pub struct FileSecrets {
    path: PathBuf,
    /// Serializes read-modify-write cycles within this process; the home's
    /// lock keeps other processes out.
    writing: Mutex<()>,
}

impl FileSecrets {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            writing: Mutex::new(()),
        }
    }

    fn load(&self) -> Result<BTreeMap<String, String>, Error> {
        Ok(read_json(&self.path)?.unwrap_or_default())
    }

    fn update(&self, change: impl FnOnce(&mut BTreeMap<String, String>)) -> Result<(), Error> {
        let _writing = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        let mut secrets = self.load()?;
        change(&mut secrets);
        write_json(&self.path, &secrets, PRIVATE)
    }
}

impl SecretStore for FileSecrets {
    fn get(&self, key: &str) -> Result<Option<String>, Error> {
        Ok(self.load()?.remove(key))
    }

    fn set(&self, key: &str, secret: &str) -> Result<(), Error> {
        self.update(|secrets| {
            secrets.insert(key.to_owned(), secret.to_owned());
        })
    }

    fn delete(&self, key: &str) -> Result<(), Error> {
        if self.load()?.contains_key(key) {
            self.update(|secrets| {
                secrets.remove(key);
            })?;
        }
        Ok(())
    }
}

/// Secrets in the macOS keychain, as generic passwords of the service
/// `moil`.
#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
pub struct KeychainSecrets;

#[cfg(target_os = "macos")]
impl KeychainSecrets {
    const SERVICE: &str = "moil";

    fn entry(key: &str) -> Result<keyring::Entry, Error> {
        keyring::Entry::new(Self::SERVICE, key).map_err(|e| Error::Secrets(e.to_string()))
    }
}

#[cfg(target_os = "macos")]
impl SecretStore for KeychainSecrets {
    fn get(&self, key: &str) -> Result<Option<String>, Error> {
        match Self::entry(key)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Error::Secrets(e.to_string())),
        }
    }

    fn set(&self, key: &str, secret: &str) -> Result<(), Error> {
        Self::entry(key)?
            .set_password(secret)
            .map_err(|e| Error::Secrets(e.to_string()))
    }

    fn delete(&self, key: &str) -> Result<(), Error> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(Error::Secrets(e.to_string())),
        }
    }
}

/// Secrets in memory, for tests.
#[derive(Debug, Default)]
pub struct MemorySecrets {
    secrets: Mutex<HashMap<String, String>>,
}

impl SecretStore for MemorySecrets {
    fn get(&self, key: &str) -> Result<Option<String>, Error> {
        Ok(self.secrets.lock().unwrap().get(key).cloned())
    }

    fn set(&self, key: &str, secret: &str) -> Result<(), Error> {
        self.secrets
            .lock()
            .unwrap()
            .insert(key.to_owned(), secret.to_owned());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), Error> {
        self.secrets.lock().unwrap().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn file_secrets_are_readable_only_by_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        let secrets = FileSecrets::new(&path);
        assert_eq!(secrets.get("a").unwrap(), None);

        secrets.set("a", "one").unwrap();
        secrets.set("b", "two").unwrap();
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);

        // Replacing a secret keeps the others and the permissions, even if
        // someone loosened them in between.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        secrets.set("a", "uno").unwrap();
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
        assert_eq!(secrets.get("a").unwrap().as_deref(), Some("uno"));
        assert_eq!(secrets.get("b").unwrap().as_deref(), Some("two"));

        secrets.delete("a").unwrap();
        secrets.delete("a").unwrap();
        assert_eq!(secrets.get("a").unwrap(), None);
        assert_eq!(
            FileSecrets::new(&path).get("b").unwrap().as_deref(),
            Some("two"),
            "secrets survive a restart"
        );
    }

    #[test]
    fn moil_secrets_selects_the_store() {
        let home = Home::new("/nonexistent/moil");
        let store = secrets_for(Some("file"), &home).unwrap();
        assert!(format!("{store:?}").contains("secrets.json"), "{store:?}");

        let default = format!("{:?}", secrets_for(None, &home).unwrap());
        if cfg!(target_os = "macos") {
            assert!(default.contains("KeychainSecrets"), "{default}");
        } else {
            assert!(default.contains("FileSecrets"), "{default}");
        }

        let err = secrets_for(Some("vault"), &home).unwrap_err();
        assert!(err.to_string().contains("\"vault\""), "{err}");
    }

    /// Talks to the real keychain, which may ask for permission: run it by
    /// hand with `cargo test -p moil-agent -- --ignored keychain`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn keychain_round_trip() {
        let key = format!("moil-test-{}", std::process::id());
        let keychain = KeychainSecrets;
        keychain.set(&key, "one").unwrap();
        keychain.set(&key, "two").unwrap();
        assert_eq!(keychain.get(&key).unwrap().as_deref(), Some("two"));
        keychain.delete(&key).unwrap();
        keychain.delete(&key).unwrap();
        assert_eq!(keychain.get(&key).unwrap(), None);
    }
}
