//! Error model of the sync engine.

use std::path::Path;
use std::path::PathBuf;

use oc_api::ApiError;
use oc_db::DbError;

/// Everything the sync engine can fail with.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SyncError {
    /// Server paths differ only in case and cannot coexist on the local,
    /// case-folding filesystem; they are left alone until one is renamed.
    #[error(
        "'{paths}' differ only in upper/lower case and cannot be synced side by side \
         on this filesystem; rename one of them on the server"
    )]
    CaseCollision {
        /// The colliding paths, joined for display.
        paths: String,
    },
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
    /// A conflict to resolve does not exist or is no longer pending.
    #[error("no pending conflict with id {id}")]
    UnknownConflict {
        /// The requested conflict row id.
        id: i64,
    },
    /// The conflict has no automatic resolution (e.g. a folder/file
    /// mismatch); one side must be removed manually.
    #[error("conflict at '{path}' cannot be resolved automatically: {reason}")]
    UnresolvableConflict {
        /// The conflicting path.
        path: String,
        /// Why no automatic resolution exists.
        reason: &'static str,
    },
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

    /// Whether the server could not be reached at all (DNS/connect
    /// failure, see [`ApiError::is_connectivity`]): the job should
    /// switch to offline backoff instead of its normal interval.
    pub fn is_connectivity(&self) -> bool {
        matches!(self, Self::Api(error) if error.is_connectivity())
    }

    /// Whether the server is throttling this client (425/429, see
    /// [`ApiError::is_throttled`]): continuing at full rate only
    /// prolongs the throttling.
    pub fn is_throttled(&self) -> bool {
        matches!(self, Self::Api(error) if error.is_throttled())
    }

    /// Whether an aborted pass should switch the job to offline
    /// backoff: an unreachable server, or one that is throttling —
    /// both call for fewer, later attempts, not the normal interval.
    pub fn should_back_off(&self) -> bool {
        matches!(self, Self::Api(error) if error.is_connectivity() || error.is_throttled())
    }
}
