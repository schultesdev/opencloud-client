//! Error model of the sync engine.

use std::path::Path;
use std::path::PathBuf;

use oc_api::ApiError;
use oc_db::DbError;

/// Everything the sync engine can fail with.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SyncError {
    /// A request against the server failed (network, auth, protocol).
    #[error("server request failed: {0}")]
    Api(#[from] ApiError),
    /// The status database failed.
    #[error("status database error: {0}")]
    Db(#[from] DbError),
    /// A local filesystem operation failed.
    #[error("io error at {path}: {source}")]
    Io {
        /// The affected local path.
        path: PathBuf,
        /// The underlying io error.
        #[source]
        source: std::io::Error,
    },
    /// A relative path cannot be mapped to the local filesystem
    /// (`.`/`..` or empty segments, NUL bytes).
    #[error("invalid relative path '{path}'")]
    InvalidPath {
        /// The rejected path.
        path: String,
    },
    /// An `include`/`exclude` pattern is invalid.
    #[error("invalid filter pattern '{pattern}': {reason}")]
    InvalidPattern {
        /// The rejected pattern.
        pattern: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// The filesystem watcher could not be started or failed.
    #[error("fs watcher error: {0}")]
    Watch(String),
}

impl SyncError {
    /// Wraps an io error with the affected path.
    pub fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().to_path_buf(),
            source,
        }
    }

    /// Rejects a relative path that cannot be mapped to the local filesystem.
    pub fn invalid_path(path: impl Into<String>) -> Self {
        Self::InvalidPath { path: path.into() }
    }
}
