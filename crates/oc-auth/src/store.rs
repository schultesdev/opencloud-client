//! Refresh-token storage: OS keyring or in-memory (tests, headless fallback).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::error::AuthError;

/// Storage abstraction for the per-account refresh token.
///
/// The access token lives in memory only; the refresh token must survive
/// process restarts and therefore goes to the OS credential store.
pub trait TokenStore: Send + Sync {
    /// Persists the refresh token for the given account, replacing any
    /// previous value.
    fn save_refresh_token(&self, account_id: &str, token: &str) -> Result<(), AuthError>;

    /// Loads the stored refresh token (`None` if the account is not logged in).
    fn load_refresh_token(&self, account_id: &str) -> Result<Option<String>, AuthError>;

    /// Removes the stored refresh token; deleting a missing entry is not an
    /// error (idempotent).
    fn delete_refresh_token(&self, account_id: &str) -> Result<(), AuthError>;
}

/// Stores refresh tokens in the OS keyring (Windows Credential Manager,
/// macOS Keychain, Linux Secret Service).
///
/// On headless Linux systems without a running Secret Service this fails at
/// runtime; use [`InMemoryTokenStore`] or a file-based store there (see the
/// plan, section "Risks & offene Punkte").
pub struct KeyringStore {
    /// Keyring service name (groups all entries of this app).
    service: String,
}

impl KeyringStore {
    /// Creates a store using the default service name ([`crate::KEYRING_SERVICE`]).
    pub fn new() -> Self {
        Self {
            service: crate::KEYRING_SERVICE.to_string(),
        }
    }

    /// Creates a store with a custom keyring service name.
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// Looks up the keyring entry for an account.
    fn entry(&self, account_id: &str) -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(&self.service, account_id).map_err(map_keyring_error)
    }
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenStore for KeyringStore {
    fn save_refresh_token(&self, account_id: &str, token: &str) -> Result<(), AuthError> {
        self.entry(account_id)?
            .set_password(token)
            .map_err(map_keyring_error)
    }

    fn load_refresh_token(&self, account_id: &str) -> Result<Option<String>, AuthError> {
        match self.entry(account_id)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(map_keyring_error(err)),
        }
    }

    fn delete_refresh_token(&self, account_id: &str) -> Result<(), AuthError> {
        match self.entry(account_id)?.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(map_keyring_error(err)),
        }
    }
}

/// Maps keyring errors to [`AuthError::Store`].
fn map_keyring_error(err: keyring::Error) -> AuthError {
    AuthError::Store(err.to_string())
}

/// In-memory [`TokenStore`] for tests and headless environments without an
/// OS keyring. Cheap to clone — clones share the same state.
#[derive(Debug, Clone, Default)]
pub struct InMemoryTokenStore {
    tokens: Arc<Mutex<HashMap<String, String>>>,
}

impl TokenStore for InMemoryTokenStore {
    fn save_refresh_token(&self, account_id: &str, token: &str) -> Result<(), AuthError> {
        self.tokens
            .lock()
            .map_err(|_| poisoned())?
            .insert(account_id.to_string(), token.to_string());
        Ok(())
    }

    fn load_refresh_token(&self, account_id: &str) -> Result<Option<String>, AuthError> {
        Ok(self
            .tokens
            .lock()
            .map_err(|_| poisoned())?
            .get(account_id)
            .cloned())
    }

    fn delete_refresh_token(&self, account_id: &str) -> Result<(), AuthError> {
        self.tokens
            .lock()
            .map_err(|_| poisoned())?
            .remove(account_id);
        Ok(())
    }
}

/// Maps a poisoned store mutex to an [`AuthError::Store`].
fn poisoned() -> AuthError {
    AuthError::Store("token store lock poisoned".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_store_roundtrip() {
        let store = InMemoryTokenStore::default();
        assert_eq!(store.load_refresh_token("a").expect("read"), None);

        store.save_refresh_token("a", "r1").expect("save");
        assert_eq!(
            store.load_refresh_token("a").expect("read").as_deref(),
            Some("r1")
        );

        store.save_refresh_token("a", "r2").expect("save");
        assert_eq!(
            store.load_refresh_token("a").expect("read").as_deref(),
            Some("r2")
        );

        store.delete_refresh_token("a").expect("delete");
        assert_eq!(store.load_refresh_token("a").expect("read"), None);
        // deleting a missing entry is fine
        store.delete_refresh_token("a").expect("delete again");
    }

    #[test]
    fn in_memory_store_clones_share_state() {
        let store = InMemoryTokenStore::default();
        let clone = store.clone();
        store.save_refresh_token("a", "r1").expect("save");
        assert_eq!(
            clone.load_refresh_token("a").expect("read").as_deref(),
            Some("r1")
        );
    }
}
