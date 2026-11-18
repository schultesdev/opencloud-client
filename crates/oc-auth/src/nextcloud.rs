//! Nextcloud login: Login Flow v2 yields an app password for this device,
//! which is sent with HTTP Basic auth and kept in the [`TokenStore`] (OS
//! keyring) — never in JSON files.
//!
//! Flow: `POST <server>/index.php/login/v2` returns a login URL for the
//! browser and a poll endpoint with a token; polling answers 404 until the
//! user granted access, then the login name and the app password. Logout
//! deletes the app password at the server
//! (`DELETE <server>/ocs/v2.php/core/apppassword`).

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::discovery::below;
use crate::error::AuthError;
use crate::store::TokenStore;

/// Device name shown in the user's Nextcloud security settings (sent as
/// `User-Agent` when the login flow starts).
pub const NEXTCLOUD_DEVICE_NAME: &str = "OpenCloud Sync";

/// Default interval between two polls of the login flow.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The credential of a Nextcloud login.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppPassword {
    /// The login name the app password belongs to.
    pub login_name: String,
    /// The app password.
    pub app_password: String,
}

impl AppPassword {
    /// `login:password`, the credential form of HTTP Basic auth.
    pub fn basic_credential(&self) -> String {
        format!("{}:{}", self.login_name, self.app_password)
    }

    /// Parses a stored credential; `None` for anything else (e.g. an
    /// OpenCloud refresh token left under the same account id).
    fn parse(stored: &str) -> Option<Self> {
        serde_json::from_str::<Self>(stored)
            .ok()
            .filter(|credential| {
                !credential.login_name.is_empty() && !credential.app_password.is_empty()
            })
    }
}

impl fmt::Debug for AppPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppPassword")
            .field("login_name", &self.login_name)
            .finish_non_exhaustive()
    }
}

/// Answer of the login flow start.
#[derive(Debug, Deserialize)]
struct FlowStart {
    poll: FlowPoll,
    login: String,
}

/// Poll endpoint and token of a started login flow.
#[derive(Debug, Deserialize)]
struct FlowPoll {
    token: String,
    endpoint: String,
}

/// Answer of a successful poll.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlowResult {
    login_name: String,
    app_password: String,
}

/// What the manager knows about the credential.
#[derive(Debug, Clone)]
enum CredentialState {
    /// Not loaded yet (read from the store on demand).
    Unknown,
    /// Loaded and believed valid.
    Loaded(AppPassword),
    /// The server rejected it (401); a new login is required. Kept in
    /// memory so the rejected password is not sent again and again
    /// (Nextcloud throttles repeated failed logins).
    Rejected,
}

/// Manages the app password of one Nextcloud account.
pub struct AppPasswordManager<S: TokenStore> {
    server: Url,
    http: reqwest::Client,
    store: Arc<S>,
    account_id: String,
    poll_interval: Duration,
    state: Mutex<CredentialState>,
}

impl<S: TokenStore + 'static> AppPasswordManager<S> {
    /// Creates the manager for `account_id` at `server_url`.
    pub fn new(server_url: &str, store: S, account_id: &str) -> Result<Self, AuthError> {
        let server = Url::parse(server_url)
            .map_err(|err| AuthError::invalid_server(server_url, err.to_string()))?;
        if !matches!(server.scheme(), "http" | "https") || server.host_str().is_none() {
            return Err(AuthError::invalid_server(
                server_url,
                "must be an http(s) URL with a host",
            ));
        }
        let http = reqwest::Client::builder()
            .timeout(crate::DEFAULT_HTTP_TIMEOUT)
            .user_agent(NEXTCLOUD_DEVICE_NAME)
            .build()?;
        Ok(Self {
            server,
            http,
            store: Arc::new(store),
            account_id: account_id.to_owned(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            state: Mutex::new(CredentialState::Unknown),
        })
    }

    /// Overrides the poll interval of the login flow (mainly for tests).
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// The account id used as key in the store.
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// The credential store.
    pub fn store(&self) -> &S {
        &self.store
    }

    fn state(&self) -> std::sync::MutexGuard<'_, CredentialState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Runs a store operation on a blocking thread (keyring calls may
    /// block).
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

    /// Whether an app password is stored. Blocks on the keyring.
    pub fn has_stored_login(&self) -> bool {
        self.store
            .load_refresh_token(&self.account_id)
            .ok()
            .flatten()
            .and_then(|stored| AppPassword::parse(&stored))
            .is_some()
    }

    /// Interactive login via Login Flow v2: opens the login page in the
    /// browser (with `open_browser`), waits until the user granted access
    /// (at most `timeout`), and stores the app password.
    pub async fn login(
        &self,
        open_browser: bool,
        timeout: Duration,
    ) -> Result<AppPassword, AuthError> {
        let start_url = below(&self.server, "index.php/login/v2");
        let response = self.http.post(&start_url).send().await?;
        if !response.status().is_success() {
            return Err(AuthError::LoginFlow(format!(
                "starting the login flow failed with HTTP {}",
                response.status().as_u16()
            )));
        }
        let start: FlowStart = response
            .json()
            .await
            .map_err(|err| AuthError::LoginFlow(format!("invalid login flow answer: {err}")))?;
        let login_url = self.checked_url(&start.login)?;
        let poll_url = self.checked_url(&start.poll.endpoint)?;
        if open_browser {
            webbrowser::open(login_url.as_str())
                .map_err(|err| AuthError::Browser(err.to_string()))?;
        }

        let deadline = Instant::now() + timeout;
        let result = loop {
            let response = self
                .http
                .post(poll_url.clone())
                .form(&[("token", start.poll.token.as_str())])
                .send()
                .await?;
            match response.status() {
                status if status.is_success() => {
                    break response.json::<FlowResult>().await.map_err(|err| {
                        AuthError::LoginFlow(format!("invalid login flow result: {err}"))
                    })?;
                }
                // Not granted yet.
                StatusCode::NOT_FOUND => {}
                status => {
                    return Err(AuthError::LoginFlow(format!(
                        "polling the login flow failed with HTTP {}",
                        status.as_u16()
                    )));
                }
            }
            if Instant::now() + self.poll_interval > deadline {
                return Err(AuthError::Timeout {
                    timeout_secs: timeout.as_secs(),
                });
            }
            tokio::time::sleep(self.poll_interval).await;
        };

        let credential = AppPassword {
            login_name: result.login_name,
            app_password: result.app_password,
        };
        if credential.login_name.is_empty() || credential.app_password.is_empty() {
            return Err(AuthError::LoginFlow(
                "the login flow returned an empty credential".into(),
            ));
        }
        let stored = serde_json::to_string(&credential)
            .map_err(|err| AuthError::Store(format!("cannot serialize credential: {err}")))?;
        self.with_store(move |store, account| store.save_refresh_token(account, &stored))
            .await?;
        *self.state() = CredentialState::Loaded(credential.clone());
        Ok(credential)
    }

    /// Parses a URL from the login flow; it must not downgrade an https
    /// server to plain http (the app password would travel in clear text).
    fn checked_url(&self, raw: &str) -> Result<Url, AuthError> {
        let url = Url::parse(raw)
            .map_err(|err| AuthError::LoginFlow(format!("invalid URL '{raw}': {err}")))?;
        let allowed = match self.server.scheme() {
            "https" => url.scheme() == "https",
            _ => matches!(url.scheme(), "http" | "https"),
        };
        if !allowed {
            return Err(AuthError::LoginFlow(format!(
                "the server sent an insecure URL '{raw}'"
            )));
        }
        Ok(url)
    }

    /// The stored app password; fails when none is stored or the server
    /// rejected it.
    pub async fn credential(&self) -> Result<AppPassword, AuthError> {
        match self.state().clone() {
            CredentialState::Loaded(credential) => return Ok(credential),
            CredentialState::Rejected => return Err(AuthError::AppPasswordRejected),
            CredentialState::Unknown => {}
        }
        let stored = self
            .with_store(|store, account| store.load_refresh_token(account))
            .await?;
        let credential = stored
            .as_deref()
            .and_then(AppPassword::parse)
            .ok_or(AuthError::NoAppPassword)?;
        let mut state = self.state();
        // A concurrent rejection wins over the stored value.
        if matches!(*state, CredentialState::Rejected) {
            return Err(AuthError::AppPasswordRejected);
        }
        *state = CredentialState::Loaded(credential.clone());
        Ok(credential)
    }

    /// Records that the server rejected `rejected` (401): it is not sent
    /// again until the next login. A credential that changed in the
    /// meantime (new login) is kept.
    pub fn mark_rejected(&self, rejected: &str) {
        let mut state = self.state();
        let current = match &*state {
            CredentialState::Loaded(credential) => Some(credential.basic_credential()),
            _ => None,
        };
        if current.as_deref().is_none_or(|current| current == rejected) {
            *state = CredentialState::Rejected;
        }
    }

    /// Deletes the app password at the server (best effort) and removes
    /// it from the store.
    pub async fn logout(&self) -> Result<(), AuthError> {
        let stored = self
            .with_store(|store, account| store.load_refresh_token(account))
            .await?;
        if let Some(credential) = stored.as_deref().and_then(AppPassword::parse) {
            let url = below(&self.server, "ocs/v2.php/core/apppassword");
            // Best effort: an unreachable server must not keep the
            // credential on this device.
            let _ = self
                .http
                .delete(url)
                .basic_auth(&credential.login_name, Some(&credential.app_password))
                .header("OCS-APIRequest", "true")
                .send()
                .await;
        }
        self.with_store(|store, account| store.delete_refresh_token(account))
            .await?;
        *self.state() = CredentialState::Unknown;
        Ok(())
    }
}

impl<S: TokenStore> fmt::Debug for AppPasswordManager<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppPasswordManager")
            .field("server", &self.server.as_str())
            .field("account_id", &self.account_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryTokenStore;

    #[test]
    fn stored_credentials_must_be_complete_json() {
        assert!(AppPassword::parse(r#"{"login_name":"a","app_password":"b"}"#).is_some());
        assert!(AppPassword::parse(r#"{"login_name":"a","app_password":""}"#).is_none());
        // An OpenCloud refresh token under the same account id.
        assert!(AppPassword::parse("eyJhbGciOi.refresh").is_none());
    }

    #[test]
    fn https_servers_reject_insecure_flow_urls() {
        let manager =
            AppPasswordManager::new("https://nc.example.com", InMemoryTokenStore::default(), "a")
                .expect("manager");
        assert!(manager.checked_url("https://nc.example.com/login").is_ok());
        assert!(matches!(
            manager.checked_url("http://nc.example.com/login"),
            Err(AuthError::LoginFlow(_))
        ));
    }

    #[test]
    fn debug_hides_the_password() {
        let credential = AppPassword {
            login_name: "alice".into(),
            app_password: "secret".into(),
        };
        assert!(!format!("{credential:?}").contains("secret"));
    }
}
