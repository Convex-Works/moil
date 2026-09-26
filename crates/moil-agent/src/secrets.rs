//! Where machine tokens are kept.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::{Error, Home};

/// The secret store to use for `home`: `$MOIL_SECRETS` (`keychain` or
/// `file`) if set, else the keychain on macOS and a file elsewhere.
pub fn default_secrets(_home: &Home) -> Result<Arc<dyn SecretStore>, Error> {
    todo!("moil-agent: default_secrets")
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
}

impl FileSecrets {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl SecretStore for FileSecrets {
    fn get(&self, _key: &str) -> Result<Option<String>, Error> {
        todo!("moil-agent: FileSecrets::get")
    }

    fn set(&self, _key: &str, _secret: &str) -> Result<(), Error> {
        todo!("moil-agent: FileSecrets::set")
    }

    fn delete(&self, _key: &str) -> Result<(), Error> {
        todo!("moil-agent: FileSecrets::delete")
    }
}

/// Secrets in the macOS keychain, as generic passwords of the service
/// `moil`.
#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
pub struct KeychainSecrets;

#[cfg(target_os = "macos")]
impl SecretStore for KeychainSecrets {
    fn get(&self, _key: &str) -> Result<Option<String>, Error> {
        todo!("moil-agent: KeychainSecrets::get")
    }

    fn set(&self, _key: &str, _secret: &str) -> Result<(), Error> {
        todo!("moil-agent: KeychainSecrets::set")
    }

    fn delete(&self, _key: &str) -> Result<(), Error> {
        todo!("moil-agent: KeychainSecrets::delete")
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
