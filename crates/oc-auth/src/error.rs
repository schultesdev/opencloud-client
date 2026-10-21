//! Error type for authentication: discovery, login, refresh, keyring.

/// Errors that can occur during OIDC authentication.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The server URL is not usable (unparseable, wrong scheme, or no host).
    #[error("invalid server URL '{url}': {reason}")]
    InvalidServer {
        /// The offending URL.
        url: String,
        /// Why it was rejected.
        reason: String,
    },
    /// The OIDC discovery document could not be fetched or parsed.
    #[error("OIDC discovery failed for {url}: {message}")]
    Discovery {
        /// URL of the discovery document.
        url: String,
        /// What went wrong.
        message: String,
    },
    /// An HTTP or network failure while talking to the authorization server.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    /// The authorization server returned an OAuth error response.
    #[error("authorization server error '{error}'")]
    OAuth {
        /// OAuth error code (e.g. `invalid_request`, `access_denied`).
        error: String,
        /// Human-readable description, if the server provided one.
        description: Option<String>,
    },
    /// The refresh token was rejected (e.g. expired or revoked); a new
    /// interactive login is required.
    #[error("refresh token rejected ({reason}); login required")]
    RefreshTokenInvalid {
        /// Why the server rejected the token.
        reason: String,
    },
    /// No refresh token is stored; an interactive login is required first.
    #[error("no refresh token stored; login required")]
    NoRefreshToken,
    /// The token endpoint responded with something we could not parse.
    #[error("invalid token response: {0}")]
    InvalidTokenResponse(String),
    /// The user denied the authorization request (`access_denied`).
    #[error("authorization denied by the user")]
    Denied,
    /// The loopback listener saw no authorization response in time.
    #[error("no authorization response within {timeout_secs} s")]
    Timeout {
        /// How long was waited, in seconds.
        timeout_secs: u64,
    },
    /// The `state` of the authorization response did not match the request
    /// (possible CSRF or foreign response).
    #[error("state mismatch in the authorization response")]
    StateMismatch,
    /// A required OIDC endpoint is not known (neither from discovery nor from
    /// the OpenCloud defaults).
    #[error("missing OIDC endpoint: {0}")]
    MissingEndpoint(String),
    /// The authorization response contained neither code nor error.
    #[error("authorization response without code")]
    MissingCode,
    /// The token store (OS keyring) failed.
    #[error("token store error: {0}")]
    Store(String),
    /// The system browser could not be opened.
    #[error("could not open the browser: {0}")]
    Browser(String),
    /// I/O error on the loopback listener.
    #[error("I/O error on the loopback listener: {0}")]
    Io(#[from] std::io::Error),
}

impl AuthError {
    /// Builds an [`AuthError::InvalidServer`].
    pub fn invalid_server(url: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::InvalidServer {
            url: url.into(),
            reason: reason.into(),
        }
    }

    /// Builds an [`AuthError::MissingEndpoint`] for the given endpoint name.
    pub fn missing_endpoint(endpoint: impl Into<String>) -> Self {
        Self::MissingEndpoint(endpoint.into())
    }
}
