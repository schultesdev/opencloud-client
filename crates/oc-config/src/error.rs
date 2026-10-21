//! Error type for configuration loading, saving, and validation.

use std::path::PathBuf;

/// Errors that can occur while handling configuration files.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A file could not be read or written.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// Path of the offending file.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// A file does not contain valid JSON.
    #[error("invalid JSON in {path}: {source}")]
    Parse {
        /// Path of the offending file.
        path: PathBuf,
        /// Underlying serde error.
        source: serde_json::Error,
    },
    /// A value failed validation.
    #[error("invalid configuration: {0}")]
    Invalid(String),
    /// The file's schema version is newer than this binary understands.
    #[error("unsupported config version {found} (this build supports up to {supported})")]
    UnsupportedVersion {
        /// Version found in the file.
        found: u64,
        /// Highest supported version.
        supported: u64,
    },
    /// The per-OS config directory could not be determined.
    #[error("could not determine the per-OS config directory (no HOME/APPDATA available)")]
    NoConfigDir,
}

impl ConfigError {
    /// Builds an [`ConfigError::Io`] for the given path.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Builds a [`ConfigError::Parse`] for the given path.
    pub fn parse(path: impl Into<PathBuf>, source: serde_json::Error) -> Self {
        Self::Parse {
            path: path.into(),
            source,
        }
    }

    /// Builds a [`ConfigError::Invalid`] with a formatted message.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}
