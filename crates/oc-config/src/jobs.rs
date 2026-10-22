//! Data model for `jobs.json`: the list of sync jobs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// One sync job: a space of an account mirrored into a local directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncJob {
    /// Stable, unique job identifier.
    pub id: String,
    /// Id of the account this job belongs to (see [`crate::Account`]).
    pub account_id: String,
    /// Id of the OpenCloud space to sync (e.g. `storage-users-1$89ad5ad2-...`).
    pub space_id: String,
    /// Display name of the space (filled by the GUI; informational only).
    #[serde(default)]
    pub space_name: Option<String>,
    /// Absolute local directory that mirrors the space.
    pub local_path: PathBuf,
    /// Whether the job is active.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Folder whitelist (selective sync); empty list = sync everything.
    #[serde(default)]
    pub include: Vec<String>,
    /// Exclude patterns (glob-style), e.g. `tmp`, `*.tmp`, `.git`.
    #[serde(default)]
    pub exclude: Vec<String>,
}

fn default_enabled() -> bool {
    true
}

impl SyncJob {
    /// Validates the job: non-empty ids, absolute `local_path`, and valid
    /// `include`/`exclude` patterns.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.id.trim().is_empty() {
            return Err(ConfigError::invalid("job.id must not be empty"));
        }
        if self.account_id.trim().is_empty() {
            return Err(ConfigError::invalid(format!(
                "job '{}' has an empty account_id",
                self.id
            )));
        }
        if self.space_id.trim().is_empty() {
            return Err(ConfigError::invalid(format!(
                "job '{}' has an empty space_id",
                self.id
            )));
        }
        if !self.local_path.is_absolute() {
            return Err(ConfigError::invalid(format!(
                "job '{}': local_path must be absolute (got '{}')",
                self.id,
                self.local_path.display()
            )));
        }
        for pattern in self.include.iter().chain(&self.exclude) {
            validate_pattern(&self.id, pattern)?;
        }
        Ok(())
    }
}

/// Checks a single `include`/`exclude` pattern.
fn validate_pattern(job_id: &str, pattern: &str) -> Result<(), ConfigError> {
    if pattern.trim().is_empty() {
        return Err(ConfigError::invalid(format!(
            "job '{job_id}': filter patterns must not be empty"
        )));
    }
    if pattern.starts_with('/') || pattern.starts_with('\\') {
        return Err(ConfigError::invalid(format!(
            "job '{job_id}': filter pattern '{pattern}' must be relative"
        )));
    }
    if pattern.contains('\0') {
        return Err(ConfigError::invalid(format!(
            "job '{job_id}': filter pattern '{pattern}' contains a NUL byte"
        )));
    }
    for segment in pattern.split(['/', '\\']) {
        if matches!(segment, "." | "..") {
            return Err(ConfigError::invalid(format!(
                "job '{job_id}': filter pattern '{pattern}' must not contain '.' or '..' segments"
            )));
        }
    }
    Ok(())
}

/// Root of `jobs.json`: schema version and the job list.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Jobs {
    /// Schema version of the file; written as [`crate::CONFIG_VERSION`] on save.
    #[serde(default)]
    pub version: u64,
    /// All configured sync jobs.
    #[serde(default)]
    pub jobs: Vec<SyncJob>,
}

impl Jobs {
    /// Validates all jobs (incl. duplicate ids). Does **not** check that
    /// `account_id` exists; use [`crate::validate_references`] for that.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut ids = std::collections::HashSet::with_capacity(self.jobs.len());
        for job in &self.jobs {
            job.validate()?;
            if !ids.insert(job.id.as_str()) {
                return Err(ConfigError::invalid(format!(
                    "duplicate job id '{}'",
                    job.id
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_job() -> SyncJob {
        SyncJob {
            id: "job-1".into(),
            account_id: "konto-1".into(),
            space_id: "storage-users-1$abc".into(),
            space_name: None,
            local_path: PathBuf::from("/tmp/sync"),
            enabled: false,
            include: vec![],
            exclude: vec![],
        }
    }

    #[test]
    fn rejects_relative_local_path() {
        let mut job = sample_job();
        job.local_path = PathBuf::from("relative/path");
        let err = job.validate().unwrap_err();
        assert!(err.to_string().contains("absolute"), "got: {err}");
    }

    #[test]
    fn rejects_absolute_patterns() {
        let mut job = sample_job();
        job.exclude = vec!["/etc".into()];
        assert!(job.validate().is_err());
    }

    #[test]
    fn rejects_parent_segments_in_patterns() {
        let mut job = sample_job();
        job.include = vec!["../Dokumente".into()];
        assert!(job.validate().is_err());
    }

    #[test]
    fn accepts_relative_patterns() {
        let mut job = sample_job();
        job.include = vec!["Dokumente".into(), "Bilder/2026".into()];
        job.exclude = vec!["tmp".into(), "*.tmp".into(), ".git".into()];
        job.validate().unwrap();
    }

    #[test]
    fn rejects_empty_pattern() {
        let mut job = sample_job();
        job.exclude = vec!["  ".into()];
        assert!(job.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_job_ids() {
        let jobs = Jobs {
            version: 1,
            jobs: vec![sample_job(), sample_job()],
        };
        assert!(jobs.validate().is_err());
    }
}
