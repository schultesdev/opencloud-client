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
pub use model::{Account, Config, ConflictPolicy, LogLevel, Settings};
pub use paths::{
    CONFIG_FILE, CONFIG_VERSION, JOBS_FILE, config_path, default_config_dir, jobs_path,
};
pub use store::{load_config, load_jobs, save_config, save_jobs, validate_references};

/// Name of the crate (used in messages and default keyring entries).
pub const CRATE_NAME: &str = "opencloud-sync";

/// Environment variable that overrides the config directory (used by tests
/// and headless setups).
pub const CONFIG_DIR_ENV: &str = "OPENCLOUD_SYNC_CONFIG_DIR";
