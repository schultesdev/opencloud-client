//! Provider-neutral error model of remote storage operations.

use std::time::Duration;

/// Result alias for storage operations.
pub type StorageResult<T> = Result<T, StorageError>;

/// Errors of [`crate::RemoteStorage`] and [`crate::StorageProvider`]
/// operations, classified the way the sync engine reacts to them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StorageError {
    /// Transport-level failure: DNS, connect, timeout, TLS, broken body.
    #[error("network error: {message}")]
    Network {
        /// Description of the failure.
        message: String,
        /// Whether the server could not be reached at all (offline).
        connectivity: bool,
    },
    /// The authentication layer failed (token endpoint, keyring, missing
    /// login).
    #[error("authentication error: {message}")]
    Auth {
        /// Description of the failure.
        message: String,
        /// Whether the identity provider could not be reached at all.
        connectivity: bool,
    },
    /// The request was not authenticated (401); login may be required.
    #[error("authentication failed (401 Unauthorized)")]
    Unauthorized,
    /// The authenticated user lacks the required permission (403).
    #[error("access forbidden (403)")]
    Forbidden,
    /// The remote resource does not exist (404).
    #[error("resource not found (404)")]
    NotFound,
    /// A write precondition failed: the remote resource changed since the
    /// recorded ETag, or it exists although it should be created.
    #[error("precondition failed (412): remote resource changed")]
    PreconditionFailed,
    /// The remote resource is locked (423).
    #[error("resource locked (423)")]
    Locked,
    /// The server is limiting this client (429, 425); continuing at full
    /// rate only prolongs the throttling.
    #[error("server is throttling requests ({code})")]
    Throttled {
        /// The HTTP status code.
        code: u16,
        /// Server-provided delay hint, if present.
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
    /// The server response could not be interpreted, or the operation is
    /// not supported.
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl StorageError {
    /// Builds a [`StorageError::Protocol`].
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    /// Whether the server could not be reached at all (DNS or connect
    /// failure): the caller is likely offline and should back off.
    pub fn is_connectivity(&self) -> bool {
        match self {
            Self::Network { connectivity, .. } | Self::Auth { connectivity, .. } => *connectivity,
            _ => false,
        }
    }

    /// Whether the server is throttling this client.
    pub fn is_throttled(&self) -> bool {
        matches!(self, Self::Throttled { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        let offline = StorageError::Network {
            message: "connect".into(),
            connectivity: true,
        };
        assert!(offline.is_connectivity());
        assert!(!offline.is_throttled());
        let timeout = StorageError::Network {
            message: "timeout".into(),
            connectivity: false,
        };
        assert!(!timeout.is_connectivity());
        let throttled = StorageError::Throttled {
            code: 425,
            retry_after: None,
        };
        assert!(throttled.is_throttled());
        assert!(!StorageError::Server { code: 500 }.is_throttled());
    }

    #[test]
    fn messages_match_the_api_layer() {
        assert_eq!(
            StorageError::Server { code: 500 }.to_string(),
            "server error (500)"
        );
        assert_eq!(StorageError::protocol("x").to_string(), "protocol error: x");
    }
}
