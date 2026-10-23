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
/// Default idle timeout for transfers (GET/PUT bodies): a transfer fails
/// when no data moves for this long. There is no limit on the total
/// duration — a large file over a slow line may take hours.
pub const DEFAULT_TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);
/// Timeout for establishing a connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Builds an HTTP client with the given transfer idle timeout.
fn build_client(transfer_idle: Duration) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(transfer_idle)
        .build()
}

/// The HTTP client shared by all API clients with default timeouts — one
/// connection pool (and TLS session cache) for every job and request.
fn shared_client() -> Result<reqwest::Client, ApiError> {
    static SHARED: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = SHARED.get() {
        return Ok(client.clone());
    }
    let client = build_client(DEFAULT_TRANSFER_TIMEOUT)?;
    Ok(SHARED.get_or_init(|| client).clone())
}
/// Longest `Retry-After` hint the clients wait for inside one call; a
/// server asking for more gets the error back, and the engine's next
/// pass (or offline backoff) retries later instead of blocking a task.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Whether a request may be sent again after a failure that leaves open
/// whether the server already applied it (5xx, timeout, broken
/// connection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Replay {
    /// Reads and writes whose repetition is harmless (GET, PROPFIND,
    /// DELETE, MKCOL, unconditional PUT): every retryable failure is
    /// retried.
    Safe,
    /// Conditional writes and MOVE/COPY: if the first attempt was applied,
    /// a repetition fails its precondition (412/404) and would look like
    /// a conflict. Only failures that prove the request was not processed
    /// are retried — no connection, 429, 503, or a token failure before
    /// sending.
    OnlyUnprocessed,
}

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
        Ok(Self {
            http: shared_client()?,
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

    /// Overrides the metadata-request timeout (total, per request) and
    /// the transfer idle timeout. A custom idle timeout needs its own
    /// HTTP client; if that cannot be built, the shared one is kept.
    pub fn with_timeouts(mut self, request: Duration, transfer: Duration) -> Self {
        self.request_timeout = request;
        if transfer != self.transfer_timeout {
            if let Ok(http) = build_client(transfer) {
                self.http = http;
                self.transfer_timeout = transfer;
            }
        }
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

    /// Sends a request built by `build`, which receives the current bearer
    /// token and is called once per attempt; `timeout` (a limit on the
    /// whole request) is applied to every attempt — `None` for transfers,
    /// which are bounded by the client's idle timeout instead.
    ///
    /// On 401 the token is force-refreshed and the request repeated once
    /// within the same attempt. Retryable failures (network, 5xx, 429,
    /// 408) are retried with backoff until the policy is exhausted, as far
    /// as `replay` allows. A `Retry-After` hint (429/503) takes precedence
    /// over the computed backoff; hints above [`MAX_RETRY_AFTER`] end the
    /// call with the error.
    pub async fn send(
        &self,
        timeout: Option<Duration>,
        replay: Replay,
        build: impl Fn(&str) -> RequestBuilder,
    ) -> Result<Response, ApiError> {
        let max_attempts = self.retry.max_attempts.max(1);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (error, hint) = match self.attempt(timeout, &build).await {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    let hint = match status {
                        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                            crate::error::retry_after_hint(response.headers())
                        }
                        _ => None,
                    };
                    (ApiError::from_status(response.headers(), status), hint)
                }
                Err(error) => (error, None),
            };
            let may_retry = match replay {
                Replay::Safe => error.is_retryable(),
                Replay::OnlyUnprocessed => error.is_unprocessed(),
            };
            if attempt >= max_attempts || !may_retry {
                return Err(error);
            }
            let delay = match hint {
                Some(hint) if hint > MAX_RETRY_AFTER => return Err(error),
                Some(hint) => hint,
                None => self.retry.backoff(attempt),
            };
            tokio::time::sleep(delay).await;
        }
    }

    /// One attempt: send with the current token; on 401 refresh the token
    /// and send once more.
    async fn attempt(
        &self,
        timeout: Option<Duration>,
        build: &impl Fn(&str) -> RequestBuilder,
    ) -> Result<Response, ApiError> {
        let limited = |request: RequestBuilder| match timeout {
            Some(timeout) => request.timeout(timeout),
            None => request,
        };
        let token = self.token.access_token().await?;
        let response = limited(build(&token)).send().await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let refreshed = self.token.refresh(&token).await?;
        Ok(limited(build(&refreshed)).send().await?)
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
