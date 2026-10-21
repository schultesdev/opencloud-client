//! Token response model (RFC 6749, section 5.1).

use serde::{Deserialize, Serialize};

/// A token response from the token endpoint.
///
/// The access token lives in memory only; the refresh token goes to the
/// OS keyring (see [`crate::store`]) and is never written to JSON files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSet {
    /// Bearer token for API calls.
    pub access_token: String,
    /// Token type, usually `Bearer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    /// Lifetime of the access token in seconds.
    pub expires_in: u64,
    /// Refresh token (present with the `offline_access` scope).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Granted scopes, if the server echoes them back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// OpenID Connect ID token, if issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_response() {
        let set: TokenSet = serde_json::from_str(r#"{"access_token": "acc", "expires_in": 300}"#)
            .expect("valid token response");
        assert_eq!(set.access_token, "acc");
        assert_eq!(set.expires_in, 300);
        assert_eq!(set.refresh_token, None);
        assert_eq!(set.token_type, None);
    }

    #[test]
    fn parses_full_response() {
        let set: TokenSet = serde_json::from_str(
            r#"{"access_token": "acc", "token_type": "Bearer", "expires_in": 300,
                "refresh_token": "ref", "scope": "openid profile email"}"#,
        )
        .expect("valid token response");
        assert_eq!(set.refresh_token.as_deref(), Some("ref"));
        assert_eq!(set.scope.as_deref(), Some("openid profile email"));
    }

    #[test]
    fn rejects_response_without_access_token() {
        assert!(serde_json::from_str::<TokenSet>(r#"{"expires_in": 300}"#).is_err());
    }
}
