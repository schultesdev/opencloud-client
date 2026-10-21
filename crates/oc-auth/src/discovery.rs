//! OIDC discovery via `.well-known/openid-configuration`, with the OpenCloud
//! default endpoints as fallback.

use serde::Deserialize;
use url::Url;

use crate::error::AuthError;

/// OIDC endpoints needed by the client.
///
/// [`ProviderEndpoints::fallback`] provides the OpenCloud defaults (Konnect
/// token endpoint, signin authorize endpoint); fields discovered via
/// `.well-known/openid-configuration` override them (see
/// [`ProviderEndpoints::merge`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderEndpoints {
    /// OIDC issuer, informational.
    pub issuer: Option<String>,
    /// Authorization endpoint (authorization-code flow).
    pub authorization_endpoint: Option<String>,
    /// Token endpoint (code exchange and refresh).
    pub token_endpoint: Option<String>,
    /// Token revocation endpoint (RFC 7009), if the server provides one.
    pub revocation_endpoint: Option<String>,
}

impl ProviderEndpoints {
    /// OpenCloud default endpoints for the given server base URL:
    /// authorize at `/signin/v1/identifier/_/authorize`, tokens at
    /// `/konnect/v1/token`. No revocation fallback — only used when
    /// discovery provides it.
    pub fn fallback(server: &Url) -> Self {
        Self {
            issuer: None,
            authorization_endpoint: Some(format!("{server}signin/v1/identifier/_/authorize")),
            token_endpoint: Some(format!("{server}konnect/v1/token")),
            revocation_endpoint: None,
        }
    }

    /// Merges discovery results over `self`: only fields that are `Some` in
    /// `discovered` override the current values.
    pub fn merge(mut self, discovered: ProviderEndpoints) -> Self {
        if discovered.issuer.is_some() {
            self.issuer = discovered.issuer;
        }
        if discovered.authorization_endpoint.is_some() {
            self.authorization_endpoint = discovered.authorization_endpoint;
        }
        if discovered.token_endpoint.is_some() {
            self.token_endpoint = discovered.token_endpoint;
        }
        if discovered.revocation_endpoint.is_some() {
            self.revocation_endpoint = discovered.revocation_endpoint;
        }
        self
    }
}

/// Subset of the discovery document relevant to this client.
#[derive(Debug, Deserialize)]
struct DiscoveryDocument {
    #[serde(default)]
    issuer: Option<String>,
    #[serde(default)]
    authorization_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint: Option<String>,
    #[serde(default)]
    revocation_endpoint: Option<String>,
}

impl DiscoveryDocument {
    fn endpoints(self) -> ProviderEndpoints {
        ProviderEndpoints {
            issuer: self.issuer.non_empty(),
            authorization_endpoint: self.authorization_endpoint.non_empty(),
            token_endpoint: self.token_endpoint.non_empty(),
            revocation_endpoint: self.revocation_endpoint.non_empty(),
        }
    }
}

/// Treats empty strings as absent.
trait NonEmpty {
    fn non_empty(self) -> Option<String>;
}

impl NonEmpty for Option<String> {
    fn non_empty(self) -> Option<String> {
        self.filter(|s| !s.trim().is_empty())
    }
}

/// Fetches `<server>/.well-known/openid-configuration` and extracts the
/// endpoints. Errors are always [`AuthError::Discovery`]; callers fall back
/// to the OpenCloud defaults.
pub async fn discover(
    http: &reqwest::Client,
    server: &Url,
) -> Result<ProviderEndpoints, AuthError> {
    let url = format!("{server}.well-known/openid-configuration");
    let response = http
        .get(&url)
        .send()
        .await
        .map_err(|err| AuthError::Discovery {
            url: url.clone(),
            message: err.to_string(),
        })?;
    if !response.status().is_success() {
        return Err(AuthError::Discovery {
            url: url.clone(),
            message: format!("HTTP {}", response.status()),
        });
    }
    let document: DiscoveryDocument =
        response.json().await.map_err(|err| AuthError::Discovery {
            url,
            message: format!("invalid document: {err}"),
        })?;
    Ok(document.endpoints())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Url {
        Url::parse("https://cloud.example.com").expect("valid URL")
    }

    #[test]
    fn fallback_uses_opencloud_paths() {
        let endpoints = ProviderEndpoints::fallback(&server());
        assert_eq!(
            endpoints.authorization_endpoint.as_deref(),
            Some("https://cloud.example.com/signin/v1/identifier/_/authorize")
        );
        assert_eq!(
            endpoints.token_endpoint.as_deref(),
            Some("https://cloud.example.com/konnect/v1/token")
        );
        assert_eq!(endpoints.revocation_endpoint, None);
    }

    #[test]
    fn merge_only_overrides_present_fields() {
        let base = ProviderEndpoints::fallback(&server());
        let merged = base.merge(ProviderEndpoints {
            issuer: Some("https://cloud.example.com".into()),
            token_endpoint: Some("https://cloud.example.com/custom/token".into()),
            authorization_endpoint: None,
            revocation_endpoint: None,
        });
        assert_eq!(
            merged.token_endpoint.as_deref(),
            Some("https://cloud.example.com/custom/token")
        );
        assert_eq!(
            merged.authorization_endpoint.as_deref(),
            Some("https://cloud.example.com/signin/v1/identifier/_/authorize")
        );
        assert!(merged.issuer.is_some());
    }
}
