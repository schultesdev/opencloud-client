//! OpenCloud authentication (Phase 2): OIDC authorization-code flow with
//! PKCE, automatic token refresh, and refresh-token storage in the OS
//! keyring.
//!
//! Building blocks:
//!
//! - [`AuthClient`]: OIDC HTTP client — discovery, authorize URL (code flow
//!   + PKCE), token exchange, refresh, revocation.
//! - [`CallbackListener`]: loopback HTTP listener on `127.0.0.1:<free port>`
//!   that catches the browser redirect.
//! - [`TokenStore`] / [`KeyringStore`]: refresh-token storage in the OS
//!   keyring (never in JSON files); [`InMemoryTokenStore`] for tests.
//! - [`TokenManager`]: wraps it all — in-memory access token with expiry
//!   tracking, single-flight refresh (parallel callers share one refresh),
//!   forced refresh after 401, logout.
//!
//! The interactive browser login itself is only manually testable; refresh
//! logic, expiry handling, PKCE, the callback listener, and the store are
//! covered by tests against mock servers.

mod callback;
mod client;
mod discovery;
mod error;
mod manager;
mod pkce;
mod store;
mod token;

pub use callback::{Callback, CallbackListener};
pub use client::{AuthClient, DEFAULT_HTTP_TIMEOUT, DEFAULT_LOGIN_TIMEOUT};
pub use discovery::ProviderEndpoints;
pub use error::AuthError;
pub use manager::TokenManager;
pub use pkce::{Pkce, generate_verifier, s256};
pub use store::{InMemoryTokenStore, KeyringStore, TokenStore};
pub use token::TokenSet;

/// OAuth client id of the OpenCloud desktop client.
pub const DEFAULT_CLIENT_ID: &str = "OpenCloudDesktop";

/// Default OIDC scopes; `offline_access` yields a refresh token.
pub const DEFAULT_SCOPES: &str = "openid profile email offline_access";

/// Refresh this many seconds before the access token actually expires
/// (expiry tracking safety margin).
pub const DEFAULT_REFRESH_SKEW_SECS: u64 = 30;

/// Keyring service name for refresh tokens; the account id is the user.
pub const KEYRING_SERVICE: &str = "opencloud-sync";
