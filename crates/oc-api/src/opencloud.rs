//! The OpenCloud storage provider: OIDC login via [`TokenManager`], spaces
//! via the Graph API, one WebDAV storage per space
//! (`/dav/spaces/{id}/`).

use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use oc_auth::{AuthClient, TokenManager, TokenStore};
use oc_storage::{
    BoxFuture, RemoteStorage, StorageError, StorageProvider, StorageQuota, StorageResult,
    StorageRoot,
};

use crate::dav::WebDavClient;
use crate::error::ApiError;
use crate::graph::{GraphClient, Space};

/// One OpenCloud account. All clients it hands out share its token
/// manager, so refreshes stay single-flight and a rotated refresh token
/// is never used twice.
pub struct OpenCloudProvider<S: TokenStore + 'static> {
    server_url: String,
    manager: Arc<TokenManager<S>>,
}

impl<S: TokenStore + 'static> OpenCloudProvider<S> {
    /// Creates the provider for `account_id` at `server_url`; the refresh
    /// token lives in `store`.
    pub fn new(server_url: &str, store: S, account_id: &str) -> Result<Self, ApiError> {
        let auth = AuthClient::new(server_url)?;
        Ok(Self {
            server_url: server_url.to_owned(),
            manager: Arc::new(TokenManager::new(auth, store, account_id)),
        })
    }

    /// The shared token manager.
    pub fn manager(&self) -> &Arc<TokenManager<S>> {
        &self.manager
    }
}

impl<S: TokenStore + 'static> std::fmt::Debug for OpenCloudProvider<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenCloudProvider")
            .field("server_url", &self.server_url)
            .finish_non_exhaustive()
    }
}

/// Converts a Graph space into a provider-neutral root.
impl From<Space> for StorageRoot {
    fn from(space: Space) -> Self {
        Self {
            id: space.id,
            name: space.name,
            kind: space.drive_type,
            quota: space.quota.map(|quota| StorageQuota {
                used: quota.used,
                total: quota.total,
            }),
        }
    }
}

impl<S: TokenStore + 'static> StorageProvider for OpenCloudProvider<S> {
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
            let graph = GraphClient::new(&self.server_url, self.manager.clone())?;
            let spaces = graph.list_spaces().await?;
            Ok(spaces.into_iter().map(StorageRoot::from).collect())
        }
        .boxed()
    }

    fn storage(&self, root_id: &str) -> StorageResult<Arc<dyn RemoteStorage>> {
        let dav = WebDavClient::new(&self.server_url, root_id, self.manager.clone())?;
        Ok(Arc::new(dav))
    }
}
