//! Shared HTTP plumbing for the Graph and WebDAV clients: authentication
//! (bearer or Basic, set centrally), forced refresh after 401, status
//! classification, retries with exponential backoff (honoring
//! `Retry-After`), and the idle watchdog of uploads.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Builds an HTTP client with the given transfer idle timeout. Without
/// one, the client serves uploads: reqwest's read timeout runs from
/// sending until the response headers arrive and is not reset while the
/// request body streams, so it would cut off any upload that takes
/// longer than the timeout. Uploads are guarded by [`Activity`] instead.
fn build_client(transfer_idle: Option<Duration>) -> Result<reqwest::Client, reqwest::Error> {
    let builder = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT);
    match transfer_idle {
        Some(idle) => builder.read_timeout(idle),
        None => builder,
    }
    .build()
}

/// The HTTP client shared by all API clients with default timeouts — one
/// connection pool (and TLS session cache) for every job and request.
fn shared_client() -> Result<reqwest::Client, ApiError> {
    static SHARED: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = SHARED.get() {
        return Ok(client.clone());
    }
    let client = build_client(Some(DEFAULT_TRANSFER_TIMEOUT))?;
    Ok(SHARED.get_or_init(|| client).clone())
}

/// The shared client for uploads (no read timeout, see [`build_client`]).
fn shared_upload_client() -> Result<reqwest::Client, ApiError> {
    static SHARED: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = SHARED.get() {
        return Ok(client.clone());
    }
    let client = build_client(None)?;
    Ok(SHARED.get_or_init(|| client).clone())
}

/// Longest `Retry-After` hint the clients wait for inside one call; a
/// server asking for more gets the error back, and the engine's next
/// pass (or offline backoff) retries later instead of blocking a task.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// How much longer than the idle timeout an upload may wait for its
/// answer once the whole body was handed to the network stack: the
/// kernel's send buffer (several MiB) still drains over a slow line
/// without any progress the client could observe, and the server may
/// need time to store or assemble the file.
const ANSWER_WAIT_FACTOR: u32 = 5;

/// Progress state of one upload attempt; the watchdog of
/// [`AuthedHttp::send_upload`] aborts the attempt once no data moved for
/// the transfer idle timeout while the body streams, or for
/// [`ANSWER_WAIT_FACTOR`] times that after it was sent.
#[derive(Debug, Clone)]
pub(crate) struct Activity(Arc<Mutex<ActivityState>>);

/// See [`Activity`].
#[derive(Debug, Clone, Copy)]
struct ActivityState {
    /// Last progress.
    last: Instant,
    /// Whether the whole body was handed off.
    body_sent: bool,
}

impl Activity {
    /// A fresh tracker, active now.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(ActivityState {
            last: Instant::now(),
            body_sent: false,
        })))
    }

    fn update(&self, change: impl FnOnce(&mut ActivityState)) -> ActivityState {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        change(&mut state);
        *state
    }

    /// Starts a new attempt: active now, body not yet sent.
    fn restart(&self) {
        self.update(|state| {
            state.last = Instant::now();
            state.body_sent = false;
        });
    }

    /// Records body progress: `sent` of `total` bytes.
    pub fn progress(&self, sent: u64, total: u64) {
        self.update(|state| {
            state.last = Instant::now();
            state.body_sent = sent >= total;
        });
    }

    /// Resolves once nothing happened for the applicable limit.
    async fn stalled(&self, idle: Duration) -> ApiError {
        loop {
            let state = self.update(|_| {});
            let limit = if state.body_sent {
                idle * ANSWER_WAIT_FACTOR
            } else {
                idle
            };
            let quiet = state.last.elapsed();
            if quiet >= limit {
                return ApiError::Stalled { idle: limit };
            }
            tokio::time::sleep(limit - quiet).await;
        }
    }
}

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
    upload_http: reqwest::Client,
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
            upload_http: shared_upload_client()?,
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
        if transfer != self.transfer_timeout
            && let Ok(http) = build_client(Some(transfer))
        {
            self.http = http;
            self.transfer_timeout = transfer;
        }
        self
    }

    /// The underlying `reqwest` client.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The client for requests with a streamed body (uploads); use it
    /// with [`AuthedHttp::send_upload`].
    pub fn upload_http(&self) -> &reqwest::Client {
        &self.upload_http
    }

    /// Timeout for metadata requests.
    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// Sends a request built by `build`, which is called once per attempt
    /// (the `Authorization` header is added here); `timeout` (a limit on the
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
        build: impl Fn() -> RequestBuilder,
    ) -> Result<Response, ApiError> {
        self.send_with(timeout, None, replay, build).await
    }

    /// Like [`AuthedHttp::send`] for uploads built on
    /// [`AuthedHttp::upload_http`]: an attempt fails with
    /// [`ApiError::Stalled`] once `activity` (fed by the body stream)
    /// reports no progress for the transfer idle timeout while sending,
    /// or for a multiple of it while waiting for the answer afterwards
    /// (see [`Activity`]). There is no limit on the total duration.
    pub async fn send_upload(
        &self,
        activity: &Activity,
        replay: Replay,
        build: impl Fn() -> RequestBuilder,
    ) -> Result<Response, ApiError> {
        self.send_with(None, Some(activity), replay, build).await
    }

    /// Shared retry loop of [`AuthedHttp::send`] and
    /// [`AuthedHttp::send_upload`].
    async fn send_with(
        &self,
        timeout: Option<Duration>,
        watch: Option<&Activity>,
        replay: Replay,
        build: impl Fn() -> RequestBuilder,
    ) -> Result<Response, ApiError> {
        let max_attempts = self.retry.max_attempts.max(1);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let outcome = match watch {
                None => self.attempt(timeout, &build).await,
                Some(activity) => {
                    activity.restart();
                    tokio::select! {
                        outcome = self.attempt(timeout, &build) => outcome,
                        stalled = activity.stalled(self.transfer_timeout) => Err(stalled),
                    }
                }
            };
            let (error, hint) = match outcome {
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
        build: &impl Fn() -> RequestBuilder,
    ) -> Result<Response, ApiError> {
        let limited = |request: RequestBuilder| match timeout {
            Some(timeout) => request.timeout(timeout),
            None => request,
        };
        let scheme = self.token.scheme();
        let token = self.token.access_token().await?;
        let response = limited(scheme.apply(build(), &token)).send().await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let refreshed = self.token.refresh(&token).await?;
        Ok(limited(scheme.apply(build(), &refreshed)).send().await?)
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
