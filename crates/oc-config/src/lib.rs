//! JSON configuration for the OpenCloud sync client.
//!
//! Two files live in the per-OS config directory:
//!
//! - [`CONFIG_FILE`]: global settings and accounts ([`Config`])
//! - [`JOBS_FILE`]: the list of sync jobs ([`Jobs`])
//!
//! Files are written atomically (temp file + rename), validated on load, and
//! migrated schema-version-by-schema-version via the `version` field.

mod error;
mod jobs;
mod migrations;
mod model;
mod paths;
mod store;

pub use error::ConfigError;
pub use jobs::{Jobs, SyncJob};
pub use model::{
    Account, Config, ConflictPolicy, LogLevel, MAX_PARALLEL_TRANSFERS, MAX_PROPFIND_PAUSE_MS,
    MAX_SYNC_INTERVAL_SECS, Settings,
};
pub use paths::{
    CONFIG_FILE, CONFIG_VERSION, JOBS_FILE, config_path, default_config_dir, jobs_path,
};
pub use store::{load_config, load_jobs, save_config, save_jobs, validate_references};

/// Checks one `include`/`exclude` filter pattern: it must be non-empty,
/// relative (no leading `/` or `\`), free of NUL bytes, and must not
/// contain `.` or `..` segments. Used for job patterns and the global
/// `exclude` list of the settings alike.
pub fn validate_pattern(pattern: &str) -> Result<(), ConfigError> {
    if pattern.trim().is_empty() {
        return Err(ConfigError::invalid("filter patterns must not be empty"));
    }
    if pattern.starts_with('/') || pattern.starts_with('\\') {
        return Err(ConfigError::invalid(format!(
            "filter pattern '{pattern}' must be relative"
        )));
    }
    if pattern.contains('\0') {
        return Err(ConfigError::invalid(format!(
            "filter pattern '{pattern}' contains a NUL byte"
        )));
    }
    for segment in pattern.split(['/', '\\']) {
        if matches!(segment, "." | "..") {
            return Err(ConfigError::invalid(format!(
                "filter pattern '{pattern}' must not contain '.' or '..' segments"
            )));
        }
    }
    Ok(())
}

/// Name of the crate (used in messages and default keyring entries).
pub const CRATE_NAME: &str = "opencloud-sync";

/// Environment variable that overrides the config directory (used by tests
/// and headless setups).
pub const CONFIG_DIR_ENV: &str = "OPENCLOUD_SYNC_CONFIG_DIR";
