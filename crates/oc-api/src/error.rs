//! Error model for the OpenCloud API clients.
//!
//! [`ApiError`] differentiates the failure classes the sync engine needs to
//! react to: transport-level network errors, the HTTP statuses relevant for
//! optimistic writes and conflicts (401, 403, 404, 412, 423, 429, 5xx), and
//! protocol errors from malformed server responses. [`ApiError::is_retryable`]
//! drives the retry logic; [`ApiError::retry_after`] honors `Retry-After`.

use std::time::Duration;

use oc_auth::AuthError;
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, RETRY_AFTER};

/// Errors from the Graph and WebDAV clients.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApiError {
    /// Transport-level failure: DNS, connect, timeout, TLS, broken body.
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    /// A URL could not be parsed or built.
    #[error("invalid URL '{url}': {reason}")]
    InvalidUrl {
        /// The offending URL.
        url: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A remote path is invalid (dot/dot-dot segments, NUL bytes).
    #[error("invalid WebDAV path '{path}': {reason}")]
    InvalidPath {
        /// The offending path.
        path: String,
        /// Why it was rejected.
        reason: String,
    },
    /// The request was not authenticated (401); a forced token refresh did
    /// not help. Login may be required.
    #[error("authentication failed (401 Unauthorized)")]
    Unauthorized,
    /// The authenticated user lacks the required permission (403).
    #[error("access forbidden (403)")]
    Forbidden,
    /// The remote resource does not exist (404).
    #[error("resource not found (404)")]
    NotFound,
    /// The write precondition failed (412): the remote resource changed
    /// since the recorded ETag — basis for conflict detection.
    #[error("precondition failed (412): remote resource changed")]
    PreconditionFailed,
    /// The remote resource is locked (423).
    #[error("resource locked (423)")]
    Locked,
    /// The server rate-limited the request (429).
    #[error("rate limited (429)")]
    RateLimited {
        /// Server-provided `Retry-After` hint in seconds, if present.
        retry_after: Option<Duration>,
    },
    /// The server reported an internal problem (5xx).
    #[error("server error ({code})")]
    Server {
        /// The HTTP status code.
        code: u16,
    },
    /// Any other unexpected HTTP status.
    #[error("unexpected HTTP status {code}")]
    Status {
        /// The HTTP status code.
        code: u16,
    },
    /// The server response could not be parsed (bad XML/JSON, inconsistent
    /// hrefs).
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The authentication layer failed (token endpoint, keyring, missing
    /// login).
    #[error("authentication error: {0}")]
    Auth(#[from] AuthError),
}

impl ApiError {
    /// Builds an [`ApiError::InvalidUrl`].
    pub fn invalid_url(url: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::InvalidUrl {
            url: url.into(),
            reason: reason.into(),
        }
    }

    /// Builds an [`ApiError::InvalidPath`].
    pub fn invalid_path(path: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::InvalidPath {
            path: path.into(),
            reason: reason.into(),
        }
    }

    /// Builds an [`ApiError::Protocol`].
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    /// Classifies an HTTP error status (with its headers) into the error
    /// model; 2xx statuses must not be passed in.
    pub(crate) fn from_status(headers: &HeaderMap, status: StatusCode) -> Self {
        match status {
            StatusCode::UNAUTHORIZED => Self::Unauthorized,
            StatusCode::FORBIDDEN => Self::Forbidden,
            StatusCode::NOT_FOUND => Self::NotFound,
            StatusCode::PRECONDITION_FAILED => Self::PreconditionFailed,
            StatusCode::LOCKED => Self::Locked,
            StatusCode::TOO_MANY_REQUESTS => {
                let retry_after = headers
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                Self::RateLimited { retry_after }
            }
            code if code.is_server_error() => Self::Server {
                code: code.as_u16(),
            },
            code => Self::Status {
                code: code.as_u16(),
            },
        }
    }

    /// Whether another attempt may succeed: transport failures, server
    /// errors (5xx), rate limiting (429), and request timeouts (408) are
    /// considered transient.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Network(_) | Self::Server { .. } | Self::RateLimited { .. } => true,
            Self::Status { code } => *code == 408,
            _ => false,
        }
    }

    /// Whether the server could not be reached at all (DNS or connect
    /// failure): the caller is likely offline and should back off rather
    /// than retry at full rate. Timeouts do not count — they usually
    /// mean a slow server, not a missing network.
    pub fn is_connectivity(&self) -> bool {
        match self {
            Self::Network(source) => source.is_connect(),
            _ => false,
        }
    }

    /// A server-provided delay hint (`Retry-After` on 429), if present.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .expect("valid header name");
            let value = reqwest::header::HeaderValue::from_str(value).expect("valid header value");
            map.insert(name, value);
        }
        map
    }

    #[test]
    fn status_mapping_covers_sync_relevant_codes() {
        let empty = HeaderMap::new();
        assert!(matches!(
            ApiError::from_status(&empty, StatusCode::UNAUTHORIZED),
            ApiError::Unauthorized
        ));
        assert!(matches!(
            ApiError::from_status(&empty, StatusCode::FORBIDDEN),
            ApiError::Forbidden
        ));
        assert!(matches!(
            ApiError::from_status(&empty, StatusCode::NOT_FOUND),
            ApiError::NotFound
        ));
        assert!(matches!(
            ApiError::from_status(&empty, StatusCode::PRECONDITION_FAILED),
            ApiError::PreconditionFailed
        ));
        assert!(matches!(
            ApiError::from_status(&empty, StatusCode::LOCKED),
            ApiError::Locked
        ));
        assert!(matches!(
            ApiError::from_status(&empty, StatusCode::INTERNAL_SERVER_ERROR),
            ApiError::Server { code: 500 }
        ));
        assert!(matches!(
            ApiError::from_status(&empty, StatusCode::BAD_REQUEST),
            ApiError::Status { code: 400 }
        ));
    }

    #[test]
    fn retry_after_header_is_parsed_in_seconds() {
        let headers = headers(&[("retry-after", "120")]);
        let err = ApiError::from_status(&headers, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(err.retry_after(), Some(Duration::from_secs(120)));
        assert!(err.is_retryable());
    }

    #[test]
    fn retry_after_ignores_http_dates() {
        let headers = headers(&[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")]);
        let err = ApiError::from_status(&headers, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn only_transient_errors_are_retryable() {
        assert!(!ApiError::Unauthorized.is_retryable());
        assert!(!ApiError::Forbidden.is_retryable());
        assert!(!ApiError::NotFound.is_retryable());
        assert!(!ApiError::PreconditionFailed.is_retryable());
        assert!(!ApiError::Locked.is_retryable());
        assert!(ApiError::Server { code: 502 }.is_retryable());
        assert!(ApiError::Status { code: 408 }.is_retryable());
        assert!(!ApiError::Status { code: 400 }.is_retryable());
    }
}
