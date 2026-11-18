//! Storage providers per account: the one place that turns an
//! [`Account`] from `config.json` into a [`StorageProvider`] — shared by
//! the CLI and the GUI backend.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use oc_auth::KeyringStore;
use oc_config::{Account, ProviderKind};
use oc_storage::{StorageError, StorageProvider};

/// A provider instance as handed out by [`create`] and [`Providers`].
pub type Provider = Arc<dyn StorageProvider>;

/// Builds a fresh provider for an account (credentials in the OS
/// keyring). Long-lived users should go through [`Providers`] instead.
pub fn create(account: &Account) -> Result<Provider, StorageError> {
    match account.provider {
        ProviderKind::OpenCloud => {
            let provider = oc_api::OpenCloudProvider::new(
                &account.server_url,
                KeyringStore::new(),
                &account.id,
            )?;
            Ok(Arc::new(provider))
        }
        ProviderKind::Nextcloud => {
            let provider = oc_api::NextcloudProvider::new(
                &account.server_url,
                KeyringStore::new(),
                &account.id,
            )?;
            Ok(Arc::new(provider))
        }
    }
}

/// What a cached provider was built from; any change rebuilds it.
fn cache_key(account: &Account) -> String {
    format!("{:?}\n{}", account.provider, account.server_url)
}

/// One provider per account, shared by the engine's jobs and all one-off
/// requests (login, roots, folder tree, conflicts). OpenCloud rotates the
/// refresh token on every refresh: two token managers of one account
/// would refresh with the same token, the second would be rejected and
/// the account's login cleared. A login also leaves its access token in
/// the provider that ran it, so later requests must reuse that instance.
#[derive(Clone, Default)]
pub struct Providers(Arc<Mutex<HashMap<String, (String, Provider)>>>);

impl Providers {
    /// The shared provider of `account`; a changed server URL (or
    /// provider kind) replaces the cached instance.
    pub fn get(&self, account: &Account) -> Result<Provider, StorageError> {
        let key = cache_key(account);
        let mut cache = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some((cached_key, provider)) = cache.get(&account.id)
            && *cached_key == key
        {
            return Ok(provider.clone());
        }
        let provider = create(account)?;
        cache.insert(account.id.clone(), (key, provider.clone()));
        Ok(provider)
    }
}

impl std::fmt::Debug for Providers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Providers").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(server: &str) -> Account {
        Account {
            id: "konto".into(),
            provider: oc_config::ProviderKind::OpenCloud,
            server_url: server.into(),
            username: "max".into(),
        }
    }

    #[test]
    fn providers_are_shared_per_account_and_rebuilt_on_change() {
        let providers = Providers::default();
        let first = providers.get(&account("https://a.example")).expect("first");
        let again = providers.get(&account("https://a.example")).expect("again");
        assert!(Arc::ptr_eq(&first, &again));
        let moved = providers.get(&account("https://b.example")).expect("moved");
        assert!(!Arc::ptr_eq(&first, &moved));
        let mut nextcloud = account("https://b.example");
        nextcloud.provider = ProviderKind::Nextcloud;
        let switched = providers.get(&nextcloud).expect("switched");
        assert!(!Arc::ptr_eq(&moved, &switched));
    }
}
