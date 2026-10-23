//! Graph API client (LibreGraph / MS Graph `drives` subset): listing the
//! spaces of the logged-in user.

use serde::Deserialize;
use std::time::Duration;
use url::Url;

use crate::error::ApiError;
use crate::http::{AuthedHttp, Replay, parse_server_url};
use crate::retry::RetryConfig;
use crate::token::TokenProvider;

/// Path prefix of the Graph API on an OpenCloud server.
pub const GRAPH_API_PREFIX: &str = "/graph/v1.0";

/// Client for the Graph API of one OpenCloud server.
#[derive(Debug)]
pub struct GraphClient<P: TokenProvider> {
    http: AuthedHttp<P>,
    base: Url,
}

impl<P: TokenProvider> GraphClient<P> {
    /// Creates a client for the given server (e.g. `https://cloud.example.com`).
    pub fn new(server_url: &str, token: P) -> Result<Self, ApiError> {
        let server = parse_server_url(server_url)?;
        let base = Url::parse(&format!(
            "{}{}/",
            server.as_str().trim_end_matches('/'),
            GRAPH_API_PREFIX
        ))
        .map_err(|err| ApiError::invalid_url(server_url, err.to_string()))?;
        Ok(Self {
            http: AuthedHttp::new(token)?,
            base,
        })
    }

    /// Overrides the retry policy (default: [`RetryConfig::default`]).
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.http = self.http.with_retry(retry);
        self
    }

    /// Overrides the metadata-request and transfer timeouts.
    pub fn with_timeouts(mut self, request: Duration, transfer: Duration) -> Self {
        self.http = self.http.with_timeouts(request, transfer);
        self
    }

    /// Lists the spaces the user is a member of
    /// (`GET /graph/v1.0/me/drives`).
    pub async fn list_spaces(&self) -> Result<Vec<Space>, ApiError> {
        let url = self
            .base
            .join("me/drives")
            .map_err(|err| ApiError::invalid_url(self.base.as_str(), err.to_string()))?;
        let response = self
            .http
            .send(self.http.request_timeout(), Replay::Safe, |token| {
                self.http.http().get(url.clone()).bearer_auth(token)
            })
            .await?;
        let body = response.text().await?;
        let parsed: DriveList = serde_json::from_str(&body)
            .map_err(|err| ApiError::protocol(format!("invalid drives response: {err}")))?;
        Ok(parsed.value)
    }
}

/// Wrapper of the `value` member of a drives response.
#[derive(Debug, Deserialize)]
struct DriveList {
    /// The drives (spaces).
    value: Vec<Space>,
}

/// One space (drive) of the user.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Space {
    /// Stable space id, e.g. `storage-users-1$89ad5ad2-...`.
    pub id: String,
    /// Display name of the space.
    pub name: String,
    /// Space type: `personal`, `project`, `share`, ...
    #[serde(default, rename = "driveType")]
    pub drive_type: String,
    /// Alias identifying the space, e.g. `personal/admin` (optional).
    #[serde(default, rename = "driveAlias")]
    pub drive_alias: Option<String>,
    /// The space root item.
    #[serde(default)]
    pub root: Option<SpaceRoot>,
    /// Quota information (optional).
    #[serde(default)]
    pub quota: Option<Quota>,
}

impl Space {
    /// The WebDAV URL of the space root (`root.webDavUrl`), if provided —
    /// the base URL for [`crate::WebDavClient::from_webdav_url`].
    pub fn web_dav_url(&self) -> Option<&str> {
        self.root
            .as_ref()
            .and_then(|root| root.web_dav_url.as_deref())
    }
}

/// The root item of a space.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SpaceRoot {
    /// Item id of the root.
    #[serde(default)]
    pub id: Option<String>,
    /// WebDAV URL of the space.
    #[serde(default, rename = "webDavUrl")]
    pub web_dav_url: Option<String>,
}

/// Quota of a space in bytes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Quota {
    /// Total capacity in bytes, if reported.
    #[serde(default)]
    pub total: Option<u64>,
    /// Used bytes, if reported.
    #[serde(default)]
    pub used: Option<u64>,
    /// Remaining bytes, if reported.
    #[serde(default)]
    pub remaining: Option<u64>,
}
