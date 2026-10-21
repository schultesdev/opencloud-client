//! Loading and saving of the configuration files, including atomic writes.

use std::fs;
use std::path::Path;

use serde::Serialize;

use crate::error::ConfigError;
use crate::jobs::Jobs;
use crate::migrations;
use crate::model::Config;
use crate::paths::{config_path, jobs_path};

/// Loads `config.json` from the given directory.
///
/// A missing file yields [`Config::default`] (fresh install); an existing
/// file is migrated, deserialized, and validated.
pub fn load_config(config_dir: &Path) -> Result<Config, ConfigError> {
    let path = config_path(config_dir);
    if !path.exists() {
        return Ok(Config::default());
    }
    let mut value = read_json(&path)?;
    migrations::migrate_config(&mut value)?;
    let config: Config =
        serde_json::from_value(value).map_err(|err| ConfigError::parse(&path, err))?;
    config.validate()?;
    Ok(config)
}

/// Loads `jobs.json` from the given directory.
///
/// A missing file yields an empty [`Jobs`] (fresh install); an existing file
/// is migrated, deserialized, and validated.
pub fn load_jobs(config_dir: &Path) -> Result<Jobs, ConfigError> {
    let path = jobs_path(config_dir);
    if !path.exists() {
        return Ok(Jobs {
            version: crate::CONFIG_VERSION,
            jobs: Vec::new(),
        });
    }
    let mut value = read_json(&path)?;
    migrations::migrate_jobs(&mut value)?;
    let jobs: Jobs = serde_json::from_value(value).map_err(|err| ConfigError::parse(&path, err))?;
    jobs.validate()?;
    Ok(jobs)
}

/// Saves `config.json` atomically into the given directory.
pub fn save_config(config_dir: &Path, config: &Config) -> Result<(), ConfigError> {
    config.validate()?;
    let mut config = config.clone();
    config.version = crate::CONFIG_VERSION;
    write_json(&config_path(config_dir), &config)
}

/// Saves `jobs.json` atomically into the given directory.
pub fn save_jobs(config_dir: &Path, jobs: &Jobs) -> Result<(), ConfigError> {
    jobs.validate()?;
    let mut jobs = jobs.clone();
    jobs.version = crate::CONFIG_VERSION;
    write_json(&jobs_path(config_dir), &jobs)
}

/// Cross-validates config and jobs: every job must reference an existing
/// account id.
pub fn validate_references(config: &Config, jobs: &Jobs) -> Result<(), ConfigError> {
    config.validate()?;
    jobs.validate()?;
    for job in &jobs.jobs {
        if !config.accounts.iter().any(|a| a.id == job.account_id) {
            return Err(ConfigError::invalid(format!(
                "job '{}' references unknown account_id '{}'",
                job.id, job.account_id
            )));
        }
    }
    Ok(())
}

fn read_json(path: &Path) -> Result<serde_json::Value, ConfigError> {
    let text = fs::read_to_string(path).map_err(|err| ConfigError::io(path, err))?;
    serde_json::from_str(&text).map_err(|err| ConfigError::parse(path, err))
}

/// Serializes `value` pretty-printed and writes it atomically: write to a
/// temp file next to the target, then rename over the target.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), ConfigError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|err| ConfigError::invalid(format!("serialization failed: {err}")))?;
    atomic_write(path, text.as_bytes())
}

/// Atomically replaces `path` with `contents`.
///
/// Writes to `<path>.tmp` and renames. On Windows, `rename` fails when the
/// target exists, so the target is removed and the rename retried once.
pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|err| ConfigError::io(parent, err))?;

    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, contents).map_err(|err| ConfigError::io(&tmp_path, err))?;

    match fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            // Windows: rename onto an existing file fails; remove and retry.
            if path.exists() {
                fs::remove_file(path).map_err(|err| ConfigError::io(path, err))?;
                fs::rename(&tmp_path, path).map_err(|err| ConfigError::io(path, err))?;
                Ok(())
            } else {
                Err(ConfigError::io(path, rename_err))
            }
        }
    }
}
