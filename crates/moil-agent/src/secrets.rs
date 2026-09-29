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
    Ok(Arc::new(KeychainSecrets::new(KeychainSecrets::TOKENS)))
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

#[cfg(target_os = "macos")]
pub use keychain::KeychainSecrets;

#[cfg(target_os = "macos")]
mod keychain {
    use core_foundation::base::TCFType;
    use security_framework::base::Error as OsError;
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit, Reference, SearchResult};
    use security_framework::os::macos::keychain::{SecKeychain, SecPreferencesDomain};
    use security_framework::os::macos::keychain_item::SecKeychainItem;
    use security_framework::os::macos::passwords::find_generic_password;
    use security_framework_sys::base::errSecItemNotFound;
    use security_framework_sys::keychain_item::SecKeychainItemDelete;

    use super::SecretStore;
    use crate::Error;

    /// Secrets in the login keychain, as generic passwords of one service,
    /// each keyed by its account.
    ///
    /// macOS ties a keychain item to the program that stored it, and moil
    /// is signed ad hoc, so the app, `moil`, and each new build of either
    /// are different programs to it (spec/machine.md §1.2). One program
    /// reads what another stored only once the owner allows it.
    /// `SecItemDelete`, which the `keyring` crate used, refuses outright to
    /// delete it (errSecInvalidOwnerEdit), and changing it in place leaves
    /// an item that neither program may read without asking. Deleting it
    /// through its reference works, whoever stored it, without asking; so
    /// this store deletes items that way, and replaces an item rather than
    /// changing it.
    #[derive(Debug)]
    pub struct KeychainSecrets {
        service: String,
    }

    impl KeychainSecrets {
        /// The service whose items hold machine tokens.
        pub const TOKENS: &str = "moil";

        /// The items of `service`, which is [`TOKENS`](Self::TOKENS) but in
        /// tests.
        pub fn new(service: impl Into<String>) -> Self {
            Self {
                service: service.into(),
            }
        }

        fn keychain() -> Result<SecKeychain, Error> {
            SecKeychain::default_for_domain(SecPreferencesDomain::User).map_err(failed)
        }

        /// The items for `key`, found without reading their secrets.
        fn items(&self, key: &str) -> Result<Vec<SecKeychainItem>, Error> {
            let key = named(key)?;
            let found = ItemSearchOptions::new()
                .keychains(&[Self::keychain()?])
                .class(ItemClass::generic_password())
                .service(&self.service)
                .account(key)
                .load_refs(true)
                .limit(Limit::All)
                .search();
            match found {
                Ok(found) => Ok(found
                    .into_iter()
                    .filter_map(|found| match found {
                        SearchResult::Ref(Reference::KeychainItem(item)) => Some(item),
                        _ => None,
                    })
                    .collect()),
                Err(e) if e.code() == errSecItemNotFound => Ok(Vec::new()),
                Err(e) => Err(failed(e)),
            }
        }
    }

    impl SecretStore for KeychainSecrets {
        fn get(&self, key: &str) -> Result<Option<String>, Error> {
            let key = named(key)?;
            match find_generic_password(Some(&[Self::keychain()?]), &self.service, key) {
                Ok((secret, _)) => String::from_utf8(secret.to_owned()).map(Some).map_err(|_| {
                    Error::Secrets(format!("the item {key} of {} isn't text", self.service))
                }),
                Err(e) if e.code() == errSecItemNotFound => Ok(None),
                Err(e) if REFUSED.contains(&e.code()) => Err(Error::Secrets(
                    "macOS didn't let moil read its token. To macOS, the moil app, `moil` and \
                     each new version of either are different programs, and it asks before one \
                     reads what another stored: try again, and allow moil when macOS asks"
                        .into(),
                )),
                Err(e) => Err(failed(e)),
            }
        }

        /// If adding the new item fails, the old one is gone all the same.
        fn set(&self, key: &str, secret: &str) -> Result<(), Error> {
            self.delete(key)?;
            Self::keychain()?
                .add_generic_password(&self.service, key, secret.as_bytes())
                .map_err(failed)
        }

        fn delete(&self, key: &str) -> Result<(), Error> {
            for item in self.items(key)? {
                // SAFETY: `item` is a live keychain item reference.
                let status = unsafe { SecKeychainItemDelete(item.as_concrete_TypeRef()) };
                if status != 0 && status != errSecItemNotFound {
                    return Err(failed(OsError::from_code(status)));
                }
            }
            Ok(())
        }
    }

    /// What reading an item gives when the owner said no, or when macOS
    /// couldn't ask: errSecAuthFailed, errSecInteractionNotAllowed and
    /// userCanceledErr.
    const REFUSED: [i32; 3] = [-25293, -25308, -128];

    /// `key`, unless it's empty: the keychain takes an empty account for
    /// any, and deleting would delete every item of the service.
    fn named(key: &str) -> Result<&str, Error> {
        match key {
            "" => Err(Error::Secrets(
                "a keychain item's name can't be empty".into(),
            )),
            key => Ok(key),
        }
    }

    fn failed(error: OsError) -> Error {
        Error::Secrets(error.to_string())
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

    /// A service of throwaway items in the real keychain, apart from moil's.
    #[cfg(target_os = "macos")]
    fn throwaway_service(name: &str) -> String {
        format!("moil-test-{name}-{}", std::process::id())
    }

    /// Talks to the real keychain: run it by hand with
    /// `cargo test -p moil-agent -- --ignored keychain`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn keychain_round_trip() {
        let keychain = KeychainSecrets::new(throwaway_service("round-trip"));
        keychain.set("token", "one").unwrap();
        keychain.set("token", "two").unwrap();
        assert_eq!(keychain.get("token").unwrap().as_deref(), Some("two"));
        keychain.delete("token").unwrap();
        keychain.delete("token").unwrap();
        assert_eq!(keychain.get("token").unwrap(), None);
    }

    /// The app and `moil` are different programs to the keychain, as is
    /// each new build of either. moil replaces and deletes a token another
    /// program stored, here the `security` command, and macOS doesn't ask.
    /// Talks to the real keychain, as `keychain_round_trip` does.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn keychain_items_another_program_stored_are_replaced_and_deleted() {
        use std::process::{Command, Stdio};

        // Anything that would ask fails instead.
        let _no_prompts =
            security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()
                .unwrap();
        let service = throwaway_service("another-program");
        let security = |args: &[&str]| {
            Command::new("/usr/bin/security")
                .args(args)
                .args(["-s", &service])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let keychain = KeychainSecrets::new(&service);

        assert!(security(&[
            "add-generic-password",
            "-a",
            "replaced",
            "-w",
            "theirs"
        ]));
        keychain.set("replaced", "ours").unwrap();
        assert_eq!(keychain.get("replaced").unwrap().as_deref(), Some("ours"));

        assert!(security(&[
            "add-generic-password",
            "-a",
            "deleted",
            "-w",
            "theirs"
        ]));
        keychain.delete("deleted").unwrap();
        assert!(!security(&["find-generic-password", "-a", "deleted"]));

        keychain.delete("replaced").unwrap();
        assert!(!security(&["find-generic-password", "-a", "replaced"]));

        // Reading it is what asks, and here fails instead.
        assert!(security(&[
            "add-generic-password",
            "-a",
            "read",
            "-w",
            "theirs"
        ]));
        let refused = keychain.get("read").unwrap_err().to_string();
        keychain.delete("read").unwrap();
        assert!(refused.contains("allow moil when macOS asks"), "{refused}");
    }
}
