//! Data model for `config.json`: accounts and global settings.

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Default sync polling interval in seconds.
pub const DEFAULT_SYNC_INTERVAL_SECS: u64 = 30;

/// Default number of parallel transfers.
pub const DEFAULT_MAX_PARALLEL_TRANSFERS: u32 = 4;

fn default_sync_interval_secs() -> u64 {
    DEFAULT_SYNC_INTERVAL_SECS
}

fn default_max_parallel_transfers() -> u32 {
    DEFAULT_MAX_PARALLEL_TRANSFERS
}

fn default_propfind_pause_ms() -> u64 {
    0
}

fn default_log_level() -> LogLevel {
    LogLevel::Info
}

/// An OpenCloud account (one server + user).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Stable, unique account identifier, referenced by sync jobs.
    pub id: String,
    /// Base URL of the OpenCloud server, e.g. `https://cloud.example.com`.
    pub server_url: String,
    /// User name (login), e.g. `max@example.com`.
    pub username: String,
}

impl Account {
    /// Validates the account: non-empty `id` and `username`, and a usable
    /// HTTP(S) `server_url` with a host name.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.id.trim().is_empty() {
            return Err(ConfigError::invalid("account.id must not be empty"));
        }
        if self.username.trim().is_empty() {
            return Err(ConfigError::invalid(format!(
                "account '{}' has an empty username",
                self.id
            )));
        }
        match url::Url::parse(&self.server_url) {
            Ok(url) => {
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                    return Err(ConfigError::invalid(format!(
                        "account '{}': server_url must be an http(s) URL with a host (got '{}')",
                        self.id, self.server_url
                    )));
                }
            }
            Err(err) => {
                return Err(ConfigError::invalid(format!(
                    "account '{}': invalid server_url '{}': {}",
                    self.id, self.server_url, err
                )));
            }
        }
        Ok(())
    }
}

/// How to resolve sync conflicts (both sides changed the same file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Ask the user (GUI conflict list or CLI prompt). Default.
    #[default]
    Ask,
    /// Silently take the server version.
    KeepServer,
    /// Silently keep the local version.
    KeepLocal,
    /// Keep both: write a conflicted copy locally, take the server version.
    ConflictCopy,
}

/// Log verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Only errors.
    Error,
    /// Errors and warnings.
    Warn,
    /// Informational messages. Default.
    Info,
    /// Verbose debugging output.
    Debug,
    /// Everything.
    Trace,
}

/// Global application settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Interval between periodic sync scans, in seconds.
    #[serde(default = "default_sync_interval_secs")]
    pub sync_interval_secs: u64,
    /// Maximum number of transfers (uploads/downloads) running in parallel.
    #[serde(default = "default_max_parallel_transfers")]
    pub max_parallel_transfers: u32,
    /// Server-load throttle: pause in milliseconds between two folder
    /// PROPFINDs of a remote scan; 0 disables it.
    #[serde(default = "default_propfind_pause_ms")]
    pub propfind_pause_ms: u64,
    /// Conflict resolution policy.
    #[serde(default)]
    pub conflict_policy: ConflictPolicy,
    /// Log verbosity.
    #[serde(default = "default_log_level")]
    pub log_level: LogLevel,
    /// Start the app automatically at login (per-OS mechanism).
    #[serde(default)]
    pub autostart: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sync_interval_secs: DEFAULT_SYNC_INTERVAL_SECS,
            max_parallel_transfers: DEFAULT_MAX_PARALLEL_TRANSFERS,
            propfind_pause_ms: 0,
            conflict_policy: ConflictPolicy::Ask,
            log_level: LogLevel::Info,
            autostart: false,
        }
    }
}

impl Settings {
    /// Validates the settings ranges.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.sync_interval_secs == 0 {
            return Err(ConfigError::invalid(
                "settings.sync_interval_secs must be at least 1",
            ));
        }
        if self.max_parallel_transfers == 0 {
            return Err(ConfigError::invalid(
                "settings.max_parallel_transfers must be at least 1",
            ));
        }
        Ok(())
    }
}

/// Root of `config.json`: schema version, accounts, settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Schema version of the file; written as [`crate::CONFIG_VERSION`] on save.
    #[serde(default)]
    pub version: u64,
    /// Configured accounts.
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// Global settings.
    #[serde(default)]
    pub settings: Settings,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: crate::CONFIG_VERSION,
            accounts: Vec::new(),
            settings: Settings::default(),
        }
    }
}

impl Config {
    /// Validates the whole file: accounts (incl. duplicate ids) and settings.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut ids = std::collections::HashSet::with_capacity(self.accounts.len());
        for account in &self.accounts {
            account.validate()?;
            if !ids.insert(account.id.as_str()) {
                return Err(ConfigError::invalid(format!(
                    "duplicate account id '{}'",
                    account.id
                )));
            }
        }
        self.settings.validate()?;
        Ok(())
    }
}
