//! Database path resolution: one file per sync job under
//! `<config-dir>/db/<job-id>.sqlite`.

use std::path::PathBuf;

use crate::error::DbError;

/// Directory holding all per-job databases, relative to the config dir.
pub const DB_DIR: &str = "db";

/// File extension of the per-job databases.
pub const DB_EXTENSION: &str = "sqlite";

/// Returns the database file for a sync job inside the given config
/// directory: `<config-dir>/db/<job-id>.sqlite`.
///
/// The job id must be a single, safe path segment (non-empty, no path
/// separators, no `.`/`..`), because it comes from the user-editable
/// `jobs.json`.
pub fn db_path(config_dir: &std::path::Path, job_id: &str) -> Result<PathBuf, DbError> {
    validate_job_id(job_id)?;
    Ok(config_dir
        .join(DB_DIR)
        .join(format!("{job_id}.{DB_EXTENSION}")))
}

/// Rejects job ids that would escape the `db/` directory or produce
/// invalid file names.
fn validate_job_id(job_id: &str) -> Result<(), DbError> {
    let invalid = job_id.is_empty()
        || job_id == "."
        || job_id == ".."
        || job_id.contains('/')
        || job_id.contains('\\')
        || job_id.contains('\0');
    if invalid {
        return Err(DbError::InvalidJobId {
            id: job_id.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_is_nested_under_db_dir() {
        let path = db_path(std::path::Path::new("/cfg"), "job-1").unwrap();
        assert_eq!(path, PathBuf::from("/cfg/db/job-1.sqlite"));
    }

    #[test]
    fn path_separators_are_rejected() {
        assert!(db_path(std::path::Path::new("/cfg"), "a/b").is_err());
        assert!(db_path(std::path::Path::new("/cfg"), "a\\b").is_err());
        assert!(db_path(std::path::Path::new("/cfg"), "..").is_err());
        assert!(db_path(std::path::Path::new("/cfg"), "").is_err());
        assert!(db_path(std::path::Path::new("/cfg"), "job-1").is_ok());
    }
}
