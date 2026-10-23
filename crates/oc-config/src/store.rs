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
    let Some(mut value) = read_json(&path)? else {
        return Ok(Config::default());
    };
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
    let Some(mut value) = read_json(&path)? else {
        return Ok(Jobs {
            version: crate::CONFIG_VERSION,
            jobs: Vec::new(),
        });
    };
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
    jobs.validate_paths()?;
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

/// Reads and parses a JSON file; `None` only when the file does not exist.
/// Any other read failure (permissions, locks) is an error — falling back
/// to defaults there would let the next save overwrite the real file.
fn read_json(path: &Path) -> Result<Option<serde_json::Value>, ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(ConfigError::io(path, err)),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| ConfigError::parse(path, err))
}

/// Serializes `value` pretty-printed and writes it atomically: write to a
/// temp file next to the target, then rename over the target.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), ConfigError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|err| ConfigError::invalid(format!("serialization failed: {err}")))?;
    atomic_write(path, text.as_bytes())
}

/// How often a rename blocked by another process (Windows: virus
/// scanner, indexer) is retried before giving up.
const RENAME_ATTEMPTS: u32 = 5;

/// Atomically replaces `path` with `contents`.
///
/// Writes a uniquely named temp file next to the target (so concurrent
/// writers — GUI and CLI — never share one), flushes it to disk, and
/// renames it over the target. `std::fs::rename` replaces an existing
/// target on every platform (Windows: `MOVEFILE_REPLACE_EXISTING`), so the
/// original is never deleted first: if the rename fails, the old file is
/// still intact. A rename refused with `PermissionDenied` (a file briefly
/// locked by another process on Windows) is retried a few times.
pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|err| ConfigError::io(parent, err))?;

    let tmp_path = temp_path(path);
    let result = write_synced(&tmp_path, contents)
        .map_err(|err| ConfigError::io(&tmp_path, err))
        .and_then(|()| rename_with_retry(&tmp_path, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result?;
    sync_dir(parent);
    Ok(())
}

/// A temp file name unique to this process and call:
/// `<name>.<pid>.<nanos>.tmp`.
fn temp_path(path: &Path) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{}.{nanos}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Writes `contents` and flushes them to the disk before returning.
fn write_synced(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = fs::File::create(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// Renames `from` onto `to`, retrying briefly while another process holds
/// the target open (Windows reports that as `PermissionDenied`).
fn rename_with_retry(from: &Path, to: &Path) -> Result<(), ConfigError> {
    let mut attempt = 1;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(err)
                if err.kind() == std::io::ErrorKind::PermissionDenied
                    && attempt < RENAME_ATTEMPTS =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50 * u64::from(attempt)));
                attempt += 1;
            }
            Err(err) => return Err(ConfigError::io(to, err)),
        }
    }
}

/// Persists the rename itself (the directory entry) on Unix; best effort.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}
