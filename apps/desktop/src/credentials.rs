//! Where the desktop app keeps the API key the user typed.
//!
//! OWNER OVERRIDE for this phase: no OS keychain yet (it would raise OS permission prompts during
//! development). The key lives in process memory for the session behind [`CredentialStore`] and is
//! never written to disk, logs, events or argv.
//!
//! TODO(keyring), after the app is complete (docs/specs/desktop.md section 5): a second
//! implementation over `keyring-core` + `apple-native-keyring-store` / `windows-native-keyring-store`
//! / `zbus-secret-service-keyring-store` (service `Synthesis`, account = provider id), written only
//! on explicit Save / Replace / Delete, with backend errors mapped to fixed [`StoreError`] texts.

use std::collections::HashMap;
use std::sync::Mutex;

/// An API key. `Debug` and `Display` never show it.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The raw value: only for building the provider client and for tests.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("‹redacted›")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("‹redacted›")
    }
}

/// A fixed message: backend texts can carry secrets, so they are never passed through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreError(pub &'static str);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// Keyed by provider id. Only the app's Save / Replace / Delete actions call `set` / `delete`.
pub trait CredentialStore: Send + Sync {
    fn set(&self, provider: &str, key: Secret) -> Result<(), StoreError>;
    fn get(&self, provider: &str) -> Result<Option<Secret>, StoreError>;
    fn delete(&self, provider: &str) -> Result<(), StoreError>;
}

/// Session-only store: gone when the process ends.
#[derive(Default)]
pub struct InMemoryStore(Mutex<HashMap<String, Secret>>);

impl InMemoryStore {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Secret>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl CredentialStore for InMemoryStore {
    fn set(&self, provider: &str, key: Secret) -> Result<(), StoreError> {
        self.map().insert(provider.to_string(), key);
        Ok(())
    }

    fn get(&self, provider: &str) -> Result<Option<Secret>, StoreError> {
        Ok(self.map().get(provider).cloned())
    }

    fn delete(&self, provider: &str) -> Result<(), StoreError> {
        self.map().remove(provider);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_replace_delete() {
        let s = InMemoryStore::default();
        assert!(s.get("a").unwrap().is_none());
        s.set("a", Secret::new("k1")).unwrap();
        s.set("b", Secret::new("kb")).unwrap();
        assert_eq!(s.get("a").unwrap().unwrap().expose(), "k1");
        s.set("a", Secret::new("k2")).unwrap();
        assert_eq!(s.get("a").unwrap().unwrap().expose(), "k2");
        s.delete("a").unwrap();
        assert!(s.get("a").unwrap().is_none());
        assert_eq!(s.get("b").unwrap().unwrap().expose(), "kb");
    }

    #[test]
    fn secret_never_prints() {
        let k = Secret::new("sk-sentinel-123456789");
        for t in [format!("{k:?}"), format!("{k}"), format!("{:?}", Some(&k))] {
            assert!(!t.contains("sentinel"), "{t}");
        }
    }
}
