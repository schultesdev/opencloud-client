//! Access-token provisioning for the API clients.
//!
//! [`TokenProvider`] decouples the Graph and WebDAV clients from the auth
//! layer: implementors return a credential (a bearer token, or
//! `login:password` for HTTP Basic, see [`AuthScheme`]), and on a 401 the
//! clients ask
//! for a forced refresh ([`TokenProvider::refresh`]) and repeat the request
//! once. [`StaticToken`] serves tests and CLI usage; the app's
//! [`TokenManager`](oc_auth::TokenManager) implements the trait out of the
//! box.

use std::future::Future;

use crate::error::ApiError;
use oc_auth::{AppPasswordManager, TokenManager, TokenStore};

/// How the credential of a [`TokenProvider`] is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthScheme {
    /// `Authorization: Bearer <token>` (OIDC access tokens).
    #[default]
    Bearer,
    /// HTTP Basic; the credential is `login:password` (app passwords).
    /// The password is everything after the last `:`.
    Basic,
}

impl AuthScheme {
    /// Adds the `Authorization` header for `credential` to a request.
    pub(crate) fn apply(
        self,
        request: reqwest::RequestBuilder,
        credential: &str,
    ) -> reqwest::RequestBuilder {
        match self {
            Self::Bearer => request.bearer_auth(credential),
            Self::Basic => match credential.rsplit_once(':') {
                Some((login, password)) => request.basic_auth(login, Some(password)),
                None => request.basic_auth(credential, None::<&str>),
            },
        }
    }
}

/// Supplies access credentials for Graph and WebDAV requests.
pub trait TokenProvider: Send + Sync {
    /// Returns a valid access token, refreshing first when necessary.
    fn access_token(&self) -> impl Future<Output = Result<String, ApiError>> + Send;

    /// Returns a replacement for the access token `rejected`, which the
    /// server answered with 401. Providers that share tokens between
    /// requests refresh only if `rejected` is still current. Defaults to
    /// [`TokenProvider::access_token`].
    fn refresh(&self, rejected: &str) -> impl Future<Output = Result<String, ApiError>> + Send {
        let _ = rejected;
        self.access_token()
    }

    /// How the credential is sent; defaults to [`AuthScheme::Bearer`].
    fn scheme(&self) -> AuthScheme {
        AuthScheme::Bearer
    }
}

/// Fixed bearer token for tests, CLI tools, and static-token setups.
#[derive(Debug, Clone)]
pub struct StaticToken(String);

impl StaticToken {
    /// Wraps the given token.
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }
}

impl TokenProvider for StaticToken {
    async fn access_token(&self) -> Result<String, ApiError> {
        Ok(self.0.clone())
    }
}

/// A shared provider: all clients of one account use the same
/// [`TokenManager`] behind an `Arc`, so refreshes stay single-flight per
/// account and a rotated refresh token is never sent twice.
impl<P: TokenProvider> TokenProvider for std::sync::Arc<P> {
    fn access_token(&self) -> impl Future<Output = Result<String, ApiError>> + Send {
        P::access_token(self)
    }

    fn refresh(&self, rejected: &str) -> impl Future<Output = Result<String, ApiError>> + Send {
        P::refresh(self, rejected)
    }

    fn scheme(&self) -> AuthScheme {
        P::scheme(self)
    }
}

impl<S: TokenStore + 'static> TokenProvider for TokenManager<S> {
    async fn access_token(&self) -> Result<String, ApiError> {
        TokenManager::access_token(self)
            .await
            .map_err(ApiError::Auth)
    }

    async fn refresh(&self, rejected: &str) -> Result<String, ApiError> {
        TokenManager::refresh_rejected(self, rejected)
            .await
            .map_err(ApiError::Auth)
    }
}

/// Nextcloud app passwords: HTTP Basic. A 401 means the password was
/// revoked or is wrong — there is nothing to refresh, so the request is
/// not repeated and the password is not sent again until the next login.
impl<S: TokenStore + 'static> TokenProvider for AppPasswordManager<S> {
    async fn access_token(&self) -> Result<String, ApiError> {
        let credential = AppPasswordManager::credential(self)
            .await
            .map_err(ApiError::Auth)?;
        Ok(credential.basic_credential())
    }

    async fn refresh(&self, rejected: &str) -> Result<String, ApiError> {
        self.mark_rejected(rejected);
        Err(ApiError::Unauthorized)
    }

    fn scheme(&self) -> AuthScheme {
        AuthScheme::Basic
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_auth::{AuthClient, InMemoryTokenStore, TokenSet};

    #[tokio::test]
    async fn static_token_is_returned_as_is() {
        let provider = StaticToken::new("tok");
        let token = provider.access_token().await.expect("token");
        assert_eq!(token, "tok");
        assert_eq!(provider.refresh("tok").await.expect("refresh"), "tok");
    }

    #[tokio::test]
    async fn token_manager_provides_access_tokens() {
        let client = AuthClient::new("https://cloud.example.com").expect("client");
        let manager = TokenManager::new(client, InMemoryTokenStore::default(), "konto-1");
        let tokens = TokenSet {
            access_token: "acc".into(),
            token_type: Some("Bearer".into()),
            expires_in: 3600,
            refresh_token: Some("ref".into()),
            scope: None,
            id_token: None,
        };
        manager.apply_login(&tokens).await.expect("login applied");
        assert_eq!(manager.access_token().await.expect("token"), "acc");
    }
}
