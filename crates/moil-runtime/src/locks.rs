//! Locks by key, for work this process must not do twice at once for the
//! same thing, such as fetching one asset or preparing one bundle.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use tokio::sync::OwnedMutexGuard;

/// An async lock per key. Entries are never removed: there are only as many
/// as the distinct keys this process has locked, such as bundles and
/// assets, which is few.
#[derive(Debug)]
pub(crate) struct KeyedLocks<K> {
    locks: Mutex<HashMap<K, Arc<tokio::sync::Mutex<()>>>>,
}

impl<K> Default for KeyedLocks<K> {
    fn default() -> Self {
        Self {
            locks: Mutex::default(),
        }
    }
}

impl<K: Eq + Hash + Clone> KeyedLocks<K> {
    /// Waits until nothing else holds the lock for `key`, then holds it
    /// until the guard is dropped. Cancel-safe.
    pub async fn lock(&self, key: &K) -> OwnedMutexGuard<()> {
        let lock = self
            .locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.clone())
            .or_default()
            .clone();
        lock.lock_owned().await
    }
}
