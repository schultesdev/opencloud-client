//! The Nextcloud storage provider: app-password login, one root per
//! account (the user's file tree), quota from the OCS user endpoint.

use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use oc_auth::{AppPasswordManager, TokenStore};
use oc_storage::{
    BoxFuture, RemoteStorage, StorageError, StorageProvider, StorageQuota, StorageResult,
    StorageRoot,
};
use url::Url;

use super::ocs::current_user;
use super::storage::NextcloudStorage;
use crate::error::ApiError;
use crate::http::{AuthedHttp, parse_server_url};

/// One Nextcloud account. All clients it hands out share its credential
/// manager, so a rejected app password is noticed everywhere at once.
pub struct NextcloudProvider<S: TokenStore + 'static> {
    server_url: String,
    server: Url,
    manager: Arc<AppPasswordManager<S>>,
}

impl<S: TokenStore + 'static> NextcloudProvider<S> {
    /// Creates the provider for `account_id` at `server_url`; the app
    /// password lives in `store`.
    pub fn new(server_url: &str, store: S, account_id: &str) -> Result<Self, ApiError> {
        let server = parse_server_url(server_url)?;
        Ok(Self {
            server_url: server_url.to_owned(),
            server,
            manager: Arc::new(AppPasswordManager::new(server_url, store, account_id)?),
        })
    }

    /// The shared credential manager.
    pub fn manager(&self) -> &Arc<AppPasswordManager<S>> {
        &self.manager
    }
}

impl<S: TokenStore + 'static> std::fmt::Debug for NextcloudProvider<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NextcloudProvider")
            .field("server_url", &self.server_url)
            .finish_non_exhaustive()
    }
}

impl<S: TokenStore + 'static> StorageProvider for NextcloudProvider<S> {
    fn has_stored_login(&self) -> bool {
        self.manager.has_stored_login()
    }

    fn login<'a>(
        &'a self,
        open_browser: bool,
        timeout: Duration,
    ) -> BoxFuture<'a, StorageResult<()>> {
        async move {
            self.manager
                .login(open_browser, timeout)
                .await
                .map(|_| ())
                .map_err(|error| StorageError::from(ApiError::Auth(error)))
        }
        .boxed()
    }

    fn logout<'a>(&'a self) -> BoxFuture<'a, StorageResult<()>> {
        async move {
            self.manager
                .logout()
                .await
                .map_err(|error| StorageError::from(ApiError::Auth(error)))
        }
        .boxed()
    }

    fn roots<'a>(&'a self) -> BoxFuture<'a, StorageResult<Vec<StorageRoot>>> {
        async move {
            let http = AuthedHttp::new(self.manager.clone())?;
            let user = current_user(&http, &self.server).await?;
            Ok(vec![StorageRoot {
                id: user.id,
                name: user.display_name,
                kind: "personal".into(),
                quota: Some(StorageQuota {
                    used: user.quota_used,
                    total: user.quota_total,
                }),
            }])
        }
        .boxed()
    }

    fn storage(&self, root_id: &str) -> StorageResult<Arc<dyn RemoteStorage>> {
        let storage = NextcloudStorage::new(&self.server_url, root_id, self.manager.clone())?;
        Ok(Arc::new(storage))
    }
}
