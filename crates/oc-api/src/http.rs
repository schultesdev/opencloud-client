//! Shared HTTP plumbing for the Graph and WebDAV clients: bearer
//! authentication, forced refresh after 401, status classification, and
//! retries with exponential backoff (honoring `Retry-After`).

use std::fmt;
use std::time::Duration;

use reqwest::{RequestBuilder, Response, StatusCode};
use url::Url;

use crate::error::ApiError;
use crate::retry::RetryConfig;
use crate::token::TokenProvider;

/// Default timeout for metadata requests (Graph, PROPFIND, MKCOL, MOVE, ...).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default timeout for transfers (GET/PUT request bodies).
pub const DEFAULT_TRANSFER_TIMEOUT: Duration = Duration::from_secs(600);

/// HTTP core shared by [`crate::GraphClient`] and [`crate::WebDavClient`].
pub(crate) struct AuthedHttp<P> {
    http: reqwest::Client,
    token: P,
    /// Retry policy applied to every request.
    pub retry: RetryConfig,
    request_timeout: Duration,
    transfer_timeout: Duration,
}

impl<P: TokenProvider> AuthedHttp<P> {
    /// Creates the core with the default timeouts and retry policy.
    pub fn new(token: P) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            token,
            retry: RetryConfig::default(),
            request_timeout: DEFAULT_TIMEOUT,
            transfer_timeout: DEFAULT_TRANSFER_TIMEOUT,
        })
    }

    /// Overrides the retry policy.
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Overrides the metadata-request and transfer timeouts (applied
    /// per request).
    pub fn with_timeouts(mut self, request: Duration, transfer: Duration) -> Self {
        self.request_timeout = request;
        self.transfer_timeout = transfer;
        self
    }

    /// The underlying `reqwest` client.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Timeout for metadata requests.
    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// Timeout for transfers (GET/PUT bodies).
    pub fn transfer_timeout(&self) -> Duration {
        self.transfer_timeout
    }

    /// Sends a request built by `build`, which receives the current bearer
    /// token and is called once per attempt; `timeout` is applied to every
    /// attempt.
    ///
    /// On 401 the token is force-refreshed and the request repeated once
    /// within the same attempt. Retryable failures (network, 5xx, 429,
    /// 408) are retried with backoff until the policy is exhausted; a 429
    /// `Retry-After` hint takes precedence over the computed backoff.
    pub async fn send(
        &self,
        timeout: Duration,
        build: impl Fn(&str) -> RequestBuilder,
    ) -> Result<Response, ApiError> {
        let max_attempts = self.retry.max_attempts.max(1);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let token = self.token.access_token().await?;
            let response = build(&token).timeout(timeout).send().await?;
            let response = if response.status() == StatusCode::UNAUTHORIZED {
                let refreshed = self.token.refresh().await?;
                build(&refreshed).timeout(timeout).send().await?
            } else {
                response
            };
            if response.status().is_success() {
                return Ok(response);
            }
            let error = ApiError::from_status(response.headers(), response.status());
            if attempt >= max_attempts || !error.is_retryable() {
                return Err(error);
            }
            let delay = error
                .retry_after()
                .unwrap_or_else(|| self.retry.backoff(attempt));
            tokio::time::sleep(delay).await;
        }
    }
}

impl<P> fmt::Debug for AuthedHttp<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthedHttp")
            .field("retry", &self.retry)
            .field("request_timeout", &self.request_timeout)
            .field("transfer_timeout", &self.transfer_timeout)
            .finish_non_exhaustive()
    }
}

/// Parses and validates a server base URL (http/https with host).
pub(crate) fn parse_server_url(server_url: &str) -> Result<Url, ApiError> {
    let server =
        Url::parse(server_url).map_err(|err| ApiError::invalid_url(server_url, err.to_string()))?;
    if !matches!(server.scheme(), "http" | "https") || server.host_str().is_none() {
        return Err(ApiError::invalid_url(
            server_url,
            "must be an http(s) URL with a host",
        ));
    }
    Ok(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_url_must_be_http_with_host() {
        assert!(parse_server_url("https://cloud.example.com").is_ok());
        assert!(parse_server_url("http://127.0.0.1:8080").is_ok());
        assert!(parse_server_url("ftp://cloud.example.com").is_err());
        assert!(parse_server_url("not a url").is_err());
    }
}
