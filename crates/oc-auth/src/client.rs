//! HTTP client for the OIDC authorization-code flow with PKCE: discovery,
//! authorize URL, code exchange, refresh, revocation, and the interactive
//! browser login.

use std::time::Duration;

use serde::Deserialize;
use url::Url;

use crate::callback::{Callback, CallbackListener};
use crate::discovery::{self, ProviderEndpoints};
use crate::error::AuthError;
use crate::pkce::{Pkce, random_urlsafe};
use crate::store::TokenStore;
use crate::token::TokenSet;

/// Default request timeout for discovery/token requests.
pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Default time the user has to complete the browser login.
pub const DEFAULT_LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// OIDC client for one OpenCloud server (public desktop client, no secret).
///
/// Token and revocation requests use the discovered endpoints: discovery
/// runs lazily before the first such request (and is shared by clones);
/// until it succeeds, the OpenCloud defaults are used.
#[derive(Debug, Clone)]
pub struct AuthClient {
    http: reqwest::Client,
    server: Url,
    client_id: String,
    scopes: String,
    endpoints: ProviderEndpoints,
    /// Endpoints after a successful discovery (merged over `endpoints`).
    discovered: std::sync::Arc<tokio::sync::OnceCell<ProviderEndpoints>>,
}

impl AuthClient {
    /// Creates a client for the given server with the OpenCloud desktop
    /// `client_id` ([`crate::DEFAULT_CLIENT_ID`]).
    pub fn new(server_url: &str) -> Result<Self, AuthError> {
        Self::with_client_id(server_url, crate::DEFAULT_CLIENT_ID)
    }

    /// Creates a client with a custom OAuth client id.
    pub fn with_client_id(server_url: &str, client_id: &str) -> Result<Self, AuthError> {
        let server = Url::parse(server_url)
            .map_err(|err| AuthError::invalid_server(server_url, err.to_string()))?;
        if !matches!(server.scheme(), "http" | "https") || server.host_str().is_none() {
            return Err(AuthError::invalid_server(
                server_url,
                "must be an http(s) URL with a host",
            ));
        }
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_HTTP_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            endpoints: ProviderEndpoints::fallback(&server),
            server,
            client_id: client_id.to_string(),
            scopes: crate::DEFAULT_SCOPES.to_string(),
            discovered: Default::default(),
        })
    }

    /// Overrides the requested scopes (default: [`crate::DEFAULT_SCOPES`]).
    pub fn with_scopes(mut self, scopes: impl Into<String>) -> Self {
        self.scopes = scopes.into();
        self
    }

    /// Replaces the HTTP client (e.g. to configure timeouts or a proxy).
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// Server base URL.
    pub fn server_url(&self) -> &Url {
        &self.server
    }

    /// Endpoints currently in use.
    pub fn endpoints(&self) -> &ProviderEndpoints {
        &self.endpoints
    }

    /// Best-effort OIDC discovery: fetches
    /// `<server>/.well-known/openid-configuration` and overrides the
    /// OpenCloud default endpoints where the document provides values.
    /// Discovery failures are ignored — the defaults keep working for
    /// OpenCloud servers. Returns the endpoints in use.
    pub async fn discover_endpoints(&mut self) -> ProviderEndpoints {
        if let Ok(discovered) = discovery::discover(&self.http, &self.server).await {
            self.endpoints = self.endpoints.clone().merge(discovered);
            let cell = tokio::sync::OnceCell::new();
            let _ = cell.set(self.endpoints.clone());
            self.discovered = std::sync::Arc::new(cell);
        }
        self.endpoints.clone()
    }

    /// The endpoints for token and revocation requests: discovered ones
    /// once a discovery succeeded (run here on first use), the configured
    /// defaults otherwise. A failed discovery is retried on the next call.
    async fn active_endpoints(&self) -> ProviderEndpoints {
        let discovered = self
            .discovered
            .get_or_try_init(|| async {
                discovery::discover(&self.http, &self.server)
                    .await
                    .map(|found| self.endpoints.clone().merge(found))
            })
            .await;
        match discovered {
            Ok(endpoints) => endpoints.clone(),
            Err(_) => self.endpoints.clone(),
        }
    }

    /// Builds the authorization URL (authorization-code flow with PKCE S256).
    pub fn authorize_url(
        &self,
        redirect_uri: &str,
        pkce: &Pkce,
        state: &str,
    ) -> Result<Url, AuthError> {
        let endpoint = self
            .endpoints
            .authorization_endpoint
            .as_deref()
            .ok_or_else(|| AuthError::missing_endpoint("authorization_endpoint"))?;
        let mut url = Url::parse(endpoint)
            .map_err(|err| AuthError::invalid_server(endpoint, err.to_string()))?;
        url.query_pairs_mut().extend_pairs([
            ("client_id", self.client_id.as_str()),
            ("response_type", "code"),
            ("scope", self.scopes.as_str()),
            ("redirect_uri", redirect_uri),
            ("state", state),
            ("code_challenge", pkce.challenge.as_str()),
            ("code_challenge_method", "S256"),
        ]);
        Ok(url)
    }

    /// Exchanges the authorization code for tokens (with the PKCE verifier).
    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<TokenSet, AuthError> {
        let form = [
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), code.to_string()),
            ("redirect_uri".to_string(), redirect_uri.to_string()),
            ("client_id".to_string(), self.client_id.clone()),
            ("code_verifier".to_string(), code_verifier.to_string()),
        ];
        self.token_request(&form).await
    }

    /// Refreshes the access token with the stored refresh token.
    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenSet, AuthError> {
        let form = [
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
            ("client_id".to_string(), self.client_id.clone()),
        ];
        self.token_request(&form).await
    }

    /// Revokes the refresh token (RFC 7009). A no-op when no revocation
    /// endpoint is known (only discovery provides one).
    pub async fn revoke(&self, refresh_token: &str) -> Result<(), AuthError> {
        let endpoints = self.active_endpoints().await;
        let Some(endpoint) = endpoints.revocation_endpoint.as_deref() else {
            return Ok(());
        };
        let form = [
            ("token".to_string(), refresh_token.to_string()),
            ("token_type_hint".to_string(), "refresh_token".to_string()),
            ("client_id".to_string(), self.client_id.clone()),
        ];
        let response = self.http.post(endpoint).form(&form).send().await?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let error = parse_oauth_error(&body);
            match error {
                Some((code, description)) => Err(AuthError::OAuth {
                    error: code,
                    description,
                }),
                None => Err(AuthError::InvalidTokenResponse(format!(
                    "revocation failed with HTTP {status}"
                ))),
            }
        }
    }

    /// Runs the interactive browser login: discovery, loopback listener,
    /// browser (optional), authorization response, token exchange; then
    /// stores the refresh token in `store`.
    ///
    /// The full flow requires a browser and can only be tested manually;
    /// everything below it (listener, exchange, store) is covered by tests
    /// that drive the steps individually.
    pub async fn login(
        &self,
        store: &dyn TokenStore,
        account_id: &str,
        open_browser: bool,
        timeout: Duration,
    ) -> Result<TokenSet, AuthError> {
        let mut client = self.clone();
        client.discover_endpoints().await;

        let pkce = Pkce::new();
        let state = random_urlsafe(16);
        let listener = CallbackListener::bind().await?;
        let redirect_uri = listener.redirect_uri();
        let authorize_url = client.authorize_url(&redirect_uri, &pkce, &state)?;
        if open_browser {
            webbrowser::open(authorize_url.as_str())
                .map_err(|err| AuthError::Browser(err.to_string()))?;
        }

        let callback = listener.wait_for_state(timeout, &state).await?;
        let code = validate_callback(&callback, &state)?;
        let tokens = client
            .exchange_code(&code, &redirect_uri, &pkce.verifier)
            .await?;
        if let Some(refresh_token) = tokens.refresh_token.as_deref() {
            store.save_refresh_token(account_id, refresh_token)?;
        }
        Ok(tokens)
    }

    /// Posts a form to the token endpoint and parses the response.
    async fn token_request(&self, form: &[(String, String)]) -> Result<TokenSet, AuthError> {
        let endpoints = self.active_endpoints().await;
        let endpoint = endpoints
            .token_endpoint
            .as_deref()
            .ok_or_else(|| AuthError::missing_endpoint("token_endpoint"))?;
        let response = self.http.post(endpoint).form(form).send().await?;
        let status = response.status();
        if status.is_success() {
            let tokens: TokenSet = response
                .json()
                .await
                .map_err(|err| AuthError::InvalidTokenResponse(err.to_string()))?;
            if tokens.access_token.trim().is_empty() {
                return Err(AuthError::InvalidTokenResponse("empty access_token".into()));
            }
            return Ok(tokens);
        }
        let body = response.text().await.unwrap_or_default();
        match parse_oauth_error(&body) {
            Some((code, description)) if code == "invalid_grant" => {
                Err(AuthError::RefreshTokenInvalid {
                    reason: description.unwrap_or_else(|| "invalid_grant".into()),
                })
            }
            Some((error, description)) => Err(AuthError::OAuth { error, description }),
            None => Err(AuthError::InvalidTokenResponse(format!(
                "HTTP {status} without an OAuth error body"
            ))),
        }
    }
}

/// Checks the authorization response: denial, error, state, code.
fn validate_callback(callback: &Callback, expected_state: &str) -> Result<String, AuthError> {
    if let Some(error) = callback.error.as_deref() {
        if error == "access_denied" {
            return Err(AuthError::Denied);
        }
        return Err(AuthError::OAuth {
            error: error.to_string(),
            description: callback.error_description.clone(),
        });
    }
    if callback.state.as_deref() != Some(expected_state) {
        return Err(AuthError::StateMismatch);
    }
    callback.code.clone().ok_or(AuthError::MissingCode)
}

/// OAuth error body (RFC 6749, section 5.2).
#[derive(Debug, Deserialize)]
struct OAuthErrorBody {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// Extracts `error`/`error_description` from an error response body.
fn parse_oauth_error(body: &str) -> Option<(String, Option<String>)> {
    let parsed: OAuthErrorBody = serde_json::from_str(body).ok()?;
    Some((parsed.error, parsed.error_description))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callback(code: Option<&str>, state: Option<&str>, error: Option<&str>) -> Callback {
        Callback {
            code: code.map(String::from),
            state: state.map(String::from),
            error: error.map(String::from),
            error_description: None,
        }
    }

    #[test]
    fn validate_callback_accepts_matching_response() {
        let result = validate_callback(&callback(Some("c"), Some("s"), None), "s");
        assert_eq!(result.expect("valid"), "c");
    }

    #[test]
    fn validate_callback_rejects_state_mismatch() {
        let result = validate_callback(&callback(Some("c"), Some("other"), None), "s");
        assert!(matches!(result, Err(AuthError::StateMismatch)));
    }

    #[test]
    fn validate_callback_maps_access_denied() {
        let result = validate_callback(&callback(None, Some("s"), Some("access_denied")), "s");
        assert!(matches!(result, Err(AuthError::Denied)));
    }

    #[test]
    fn validate_callback_maps_other_oauth_errors() {
        let result = validate_callback(&callback(None, Some("s"), Some("server_error")), "s");
        assert!(matches!(result, Err(AuthError::OAuth { .. })));
    }

    #[test]
    fn validate_callback_rejects_missing_code() {
        let result = validate_callback(&callback(None, Some("s"), None), "s");
        assert!(matches!(result, Err(AuthError::MissingCode)));
    }

    #[test]
    fn parse_oauth_error_extracts_fields() {
        let body = r#"{"error": "invalid_grant", "error_description": "token expired"}"#;
        let (code, description) = parse_oauth_error(body).expect("error body");
        assert_eq!(code, "invalid_grant");
        assert_eq!(description.as_deref(), Some("token expired"));

        assert!(parse_oauth_error("not json").is_none());
    }

    #[test]
    fn rejects_non_http_server_url() {
        assert!(matches!(
            AuthClient::new("ftp://cloud.example.com"),
            Err(AuthError::InvalidServer { .. })
        ));
        assert!(matches!(
            AuthClient::new("not a url"),
            Err(AuthError::InvalidServer { .. })
        ));
    }
}
