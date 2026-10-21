//! Error type for opening and using a status database.

use std::path::PathBuf;

/// Errors that can occur while working with a status database.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// The database file could not be opened, created, or migrated.
    #[error("could not open database {path}: {source}")]
    Open {
        /// Path of the database file (or `<in-memory>`).
        path: PathBuf,
        /// Underlying SQLite error.
        source: rusqlite::Error,
    },
    /// A SQL statement failed.
    #[error("database query failed: {0}")]
    Query(#[from] rusqlite::Error),
    /// A row contains a value that does not map to a known enum variant
    /// (for example an unknown `state` or `status` string).
    #[error("invalid value '{value}' in column '{column}'")]
    InvalidValue {
        /// Name of the offending column.
        column: &'static str,
        /// The raw value found in the database.
        value: String,
    },
    /// A required directory could not be created.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// Path of the offending directory.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// A job id is not usable as a database file name (empty, `..`, or
    /// containing path separators).
    #[error("invalid job id '{id}': must be non-empty and free of path separators")]
    InvalidJobId {
        /// The offending job id.
        id: String,
    },
    /// A required index or row was not found.
    #[error("row not found: {0}")]
    NotFound(String),
}

impl DbError {
    /// Builds an [`DbError::Open`] for the given path.
    pub fn open(path: impl Into<PathBuf>, source: rusqlite::Error) -> Self {
        Self::Open {
            path: path.into(),
            source,
        }
    }
}
