//! Token manager: keeps the access token in memory, tracks its expiry, and
//! refreshes it (single-flight) when needed. Refresh tokens live in a
//! [`TokenStore`] (OS keyring).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::client::AuthClient;
use crate::error::AuthError;
use crate::store::TokenStore;
use crate::token::TokenSet;

/// An access token with its expiry instant.
#[derive(Debug, Clone)]
struct ActiveToken {
    access_token: String,
    expires_at: Instant,
}

impl ActiveToken {
    /// Wraps a fresh token response.
    fn new(tokens: &TokenSet) -> Self {
        Self {
            access_token: tokens.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(tokens.expires_in),
        }
    }

    /// True when the token is expired or about to expire within `skew`.
    fn is_expired(&self, skew: Duration) -> bool {
        match self.expires_at.checked_duration_since(Instant::now()) {
            Some(remaining) => remaining <= skew,
            None => true,
        }
    }
}

/// Manages tokens for one account: in-memory access token with expiry
/// tracking, refresh tokens in the [`TokenStore`].
///
/// Refreshes are single-flight: concurrent callers share one refresh request
/// (the tokio mutex is held across the HTTP call); on failure the refresh
/// token stays intact so a later call can retry.
pub struct TokenManager<S: TokenStore> {
    client: AuthClient,
    store: Arc<S>,
    account_id: String,
    state: Mutex<Option<ActiveToken>>,
    /// Refresh this many seconds before the access token actually expires.
    refresh_skew: Duration,
}

impl<S: TokenStore + 'static> TokenManager<S> {
    /// Creates a manager for the given client, store, and account.
    pub fn new(client: AuthClient, store: S, account_id: &str) -> Self {
        Self {
            client,
            store: Arc::new(store),
            account_id: account_id.to_string(),
            state: Mutex::new(None),
            refresh_skew: Duration::from_secs(crate::DEFAULT_REFRESH_SKEW_SECS),
        }
    }

    /// Overrides the refresh skew (mainly for tests).
    pub fn with_refresh_skew(mut self, skew: Duration) -> Self {
        self.refresh_skew = skew;
        self
    }

    /// The underlying HTTP client.
    pub fn client(&self) -> &AuthClient {
        &self.client
    }

    /// The refresh-token store.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Runs a store operation on a blocking thread: keyring calls may
    /// block (macOS unlock dialog, D-Bus round trips) and must not stall
    /// a runtime worker — least of all while the refresh lock is held.
    async fn with_store<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&S, &str) -> Result<T, AuthError> + Send + 'static,
    ) -> Result<T, AuthError> {
        let store = self.store.clone();
        let account_id = self.account_id.clone();
        tokio::task::spawn_blocking(move || operation(&store, &account_id))
            .await
            .map_err(|error| AuthError::Store(format!("token store task failed: {error}")))?
    }

    /// The account id used as key in the store.
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// Whether a refresh token is stored (i.e. the account can refresh).
    /// Blocks on the keyring; call it outside of hot async paths.
    pub fn has_stored_login(&self) -> bool {
        self.store
            .load_refresh_token(&self.account_id)
            .map(|token| token.is_some())
            .unwrap_or(false)
    }

    /// Applies the tokens of a completed login: persists the refresh token
    /// and keeps the access token (with expiry) in memory.
    pub async fn apply_login(&self, tokens: &TokenSet) -> Result<(), AuthError> {
        if let Some(refresh_token) = tokens.refresh_token.clone() {
            self.with_store(move |store, account| {
                store.save_refresh_token(account, &refresh_token)
            })
            .await?;
        }
        *self.state.lock().await = Some(ActiveToken::new(tokens));
        Ok(())
    }

    /// Interactive browser login (see [`AuthClient::login`]); stores the
    /// refresh token and applies the received access token.
    pub async fn login(
        &self,
        open_browser: bool,
        timeout: Duration,
    ) -> Result<TokenSet, AuthError> {
        let tokens = self
            .client
            .login(self.store.as_ref(), &self.account_id, open_browser, timeout)
            .await?;
        *self.state.lock().await = Some(ActiveToken::new(&tokens));
        Ok(tokens)
    }

    /// Returns a valid access token; refreshes first when the current one is
    /// expired (or about to expire). Concurrent calls share one refresh.
    pub async fn access_token(&self) -> Result<String, AuthError> {
        let mut guard = self.state.lock().await;
        if let Some(active) = guard.as_ref()
            && !active.is_expired(self.refresh_skew)
        {
            return Ok(active.access_token.clone());
        }
        self.refresh_locked(&mut guard).await
    }

    /// Forces a refresh, even if the current access token looks valid.
    pub async fn force_refresh(&self) -> Result<String, AuthError> {
        let mut guard = self.state.lock().await;
        self.refresh_locked(&mut guard).await
    }

    /// Replaces an access token the server rejected (401). When several
    /// requests are rejected at once, only the first refreshes; the
    /// others find a newer token already in place and use it, instead of
    /// each rotating the refresh token again.
    pub async fn refresh_rejected(&self, rejected: &str) -> Result<String, AuthError> {
        let mut guard = self.state.lock().await;
        if let Some(active) = guard.as_ref()
            && active.access_token != rejected
            && !active.is_expired(self.refresh_skew)
        {
            return Ok(active.access_token.clone());
        }
        self.refresh_locked(&mut guard).await
    }

    /// Refreshes while holding the lock (single-flight) and updates the
    /// stored refresh token when the server rotates it.
    ///
    /// A rejected refresh token only clears the stored login when the
    /// store still holds that very token. If another manager or process
    /// rotated it in the meantime (the rejected one was stale), the refresh
    /// is retried once with the current token instead of logging out.
    async fn refresh_locked(&self, guard: &mut Option<ActiveToken>) -> Result<String, AuthError> {
        let mut refresh_token = self
            .with_store(|store, account| store.load_refresh_token(account))
            .await?
            .ok_or(AuthError::NoRefreshToken)?;
        let mut retried = false;
        loop {
            match self.client.refresh(&refresh_token).await {
                Ok(tokens) => {
                    if let Some(rotated) = tokens.refresh_token.clone() {
                        self.with_store(move |store, account| {
                            store.save_refresh_token(account, &rotated)
                        })
                        .await?;
                    }
                    let access_token = tokens.access_token.clone();
                    *guard = Some(ActiveToken::new(&tokens));
                    return Ok(access_token);
                }
                Err(AuthError::RefreshTokenInvalid { reason }) => {
                    let current = self
                        .with_store(|store, account| store.load_refresh_token(account))
                        .await?;
                    match current {
                        Some(current) if current != refresh_token && !retried => {
                            // Rotated concurrently: ours was stale.
                            refresh_token = current;
                            retried = true;
                        }
                        Some(current) if current != refresh_token => {
                            // Rotated again; keep the newer login intact.
                            *guard = None;
                            return Err(AuthError::RefreshTokenInvalid { reason });
                        }
                        _ => {
                            // The server rejected the stored token for
                            // good: drop the login; the user must
                            // authenticate again.
                            *guard = None;
                            let _ = self
                                .with_store(|store, account| store.delete_refresh_token(account))
                                .await;
                            return Err(AuthError::RefreshTokenInvalid { reason });
                        }
                    }
                }
                // Transient failure: keep the refresh token for a retry.
                Err(err) => return Err(err),
            }
        }
    }

    /// Logout: revokes the refresh token at the server (best effort), then
    /// clears the store entry and the in-memory token.
    pub async fn logout(&self) -> Result<(), AuthError> {
        let mut guard = self.state.lock().await;
        *guard = None;
        let stored = self
            .with_store(|store, account| store.load_refresh_token(account))
            .await;
        if let Ok(Some(refresh_token)) = stored {
            // Revocation is best effort (offline server, unknown endpoint):
            // the local state is cleared regardless.
            let _ = self.client.revoke(&refresh_token).await;
        }
        self.with_store(|store, account| store.delete_refresh_token(account))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(expires_in: u64, refresh: Option<&str>) -> TokenSet {
        TokenSet {
            access_token: "acc".into(),
            token_type: Some("Bearer".into()),
            expires_in,
            refresh_token: refresh.map(String::from),
            scope: None,
            id_token: None,
        }
    }

    #[test]
    fn fresh_token_is_not_expired() {
        let active = ActiveToken::new(&tokens(3600, Some("r")));
        assert!(!active.is_expired(Duration::from_secs(30)));
    }

    #[test]
    fn short_lived_token_counts_as_expired_within_skew() {
        // A token with 1 s left is "expired" given the default 30 s skew.
        let active = ActiveToken::new(&tokens(1, Some("r")));
        assert!(active.is_expired(Duration::from_secs(30)));
        assert!(!active.is_expired(Duration::ZERO));
    }
}
