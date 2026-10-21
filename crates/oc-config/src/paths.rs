//! Config directory resolution and well-known file names.

use std::path::PathBuf;

use crate::CONFIG_DIR_ENV;
use crate::error::ConfigError;

/// File name of the global configuration: `config.json`.
pub const CONFIG_FILE: &str = "config.json";

/// File name of the sync job list: `jobs.json`.
pub const JOBS_FILE: &str = "jobs.json";

/// Current schema version of both configuration files.
pub const CONFIG_VERSION: u64 = 1;

/// Returns the per-OS config directory:
///
/// - Linux: `~/.config/opencloud-sync/`
/// - macOS: `~/Library/Application Support/opencloud-sync/`
/// - Windows: `%APPDATA%\opencloud-sync\`
///
/// The environment variable `OPENCLOUD_SYNC_CONFIG_DIR` takes precedence and
/// is mainly used by tests and headless setups.
pub fn default_config_dir() -> Result<PathBuf, ConfigError> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV) {
        return Ok(PathBuf::from(dir));
    }
    dirs::config_dir()
        .map(|base| base.join(crate::CRATE_NAME))
        .ok_or(ConfigError::NoConfigDir)
}

/// Full path of `config.json` inside the given config directory.
pub fn config_path(config_dir: &std::path::Path) -> PathBuf {
    config_dir.join(CONFIG_FILE)
}

/// Full path of `jobs.json` inside the given config directory.
pub fn jobs_path(config_dir: &std::path::Path) -> PathBuf {
    config_dir.join(JOBS_FILE)
}
