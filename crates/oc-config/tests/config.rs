//! Integration tests for configuration loading/saving/validation.

use std::path::{Path, PathBuf};

use oc_config::{Account, Config, ConflictPolicy, Jobs, LogLevel, Settings, SyncJob};

fn account(id: &str) -> Account {
    Account {
        id: id.into(),
        server_url: "https://cloud.example.com".into(),
        username: "max@example.com".into(),
    }
}

fn job(id: &str, account_id: &str) -> SyncJob {
    SyncJob {
        id: id.into(),
        account_id: account_id.into(),
        space_id: "storage-users-1$89ad5ad2-dead-beef".into(),
        space_name: Some("Projekt Mars".into()),
        local_path: PathBuf::from(if cfg!(windows) {
            r"C:\Users\max\OpenCloud\projekt-mars"
        } else {
            "/home/max/OpenCloud/projekt-mars"
        }),
        enabled: true,
        include: vec!["Dokumente".into(), "Bilder/2026".into()],
        exclude: vec!["tmp".into(), "*.tmp".into(), ".git".into()],
    }
}

fn sample_config() -> Config {
    Config {
        version: 1,
        accounts: vec![account("konto-1")],
        settings: Settings {
            sync_interval_secs: 45,
            max_parallel_transfers: 8,
            propfind_pause_ms: 250,
            conflict_policy: ConflictPolicy::KeepLocal,
            log_level: LogLevel::Debug,
            autostart: false,
        },
    }
}

fn sample_jobs() -> Jobs {
    Jobs {
        version: 1,
        jobs: vec![job("job-1", "konto-1"), job("job-2", "konto-1")],
    }
}

#[test]
fn config_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let original = sample_config();

    oc_config::save_config(dir.path(), &original).unwrap();
    let loaded = oc_config::load_config(dir.path()).unwrap();

    assert_eq!(original, loaded);
    assert_eq!(loaded.version, 1);
}

#[test]
fn jobs_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let original = sample_jobs();

    oc_config::save_jobs(dir.path(), &original).unwrap();
    let loaded = oc_config::load_jobs(dir.path()).unwrap();

    assert_eq!(original, loaded);
    assert_eq!(loaded.version, 1);
}

#[test]
fn missing_files_yield_defaults() {
    let dir = tempfile::tempdir().unwrap();

    let config = oc_config::load_config(dir.path()).unwrap();
    assert_eq!(config, Config::default());
    assert_eq!(config.settings.sync_interval_secs, 30);
    assert_eq!(config.settings.max_parallel_transfers, 4);
    assert_eq!(config.settings.conflict_policy, ConflictPolicy::Ask);
    assert_eq!(config.settings.log_level, LogLevel::Info);

    let jobs = oc_config::load_jobs(dir.path()).unwrap();
    assert!(jobs.jobs.is_empty());
}

#[test]
fn saving_creates_config_dir() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("a/b/c");
    oc_config::save_config(&nested, &Config::default()).unwrap();
    assert!(nested.join("config.json").is_file());
}

#[test]
fn rewrite_replaces_existing_file() {
    let dir = tempfile::tempdir().unwrap();

    let mut config = sample_config();
    oc_config::save_config(dir.path(), &config).unwrap();
    config.settings.sync_interval_secs = 60;
    config.accounts.clear();
    oc_config::save_config(dir.path(), &config).unwrap();

    let loaded = oc_config::load_config(dir.path()).unwrap();
    assert_eq!(loaded, config);
    // No temp file left behind.
    assert!(!dir.path().join("config.json.tmp").exists());
}

#[test]
fn invalid_json_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), "{ not json").unwrap();
    let err = oc_config::load_config(dir.path()).unwrap_err();
    assert!(err.to_string().contains("invalid JSON"), "got: {err}");
}

#[test]
fn relative_local_path_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut jobs = sample_jobs();
    jobs.jobs[0].local_path = PathBuf::from("relative/path");

    let err = oc_config::save_jobs(dir.path(), &jobs).unwrap_err();
    assert!(err.to_string().contains("absolute"), "got: {err}");

    std::fs::write(
        dir.path().join("jobs.json"),
        serde_json::to_string_pretty(&jobs).unwrap(),
    )
    .unwrap();
    let err = oc_config::load_jobs(dir.path()).unwrap_err();
    assert!(err.to_string().contains("absolute"), "got: {err}");
}

#[test]
fn duplicate_account_ids_are_rejected() {
    let mut config = sample_config();
    config.accounts.push(account("konto-1"));
    let err = config.validate().unwrap_err();
    assert!(err.to_string().contains("duplicate"), "got: {err}");
}

#[test]
fn duplicate_job_ids_are_rejected() {
    let mut jobs = sample_jobs();
    let first = jobs.jobs[0].clone();
    jobs.jobs.push(first);
    let err = jobs.validate().unwrap_err();
    assert!(err.to_string().contains("duplicate"), "got: {err}");
}

#[test]
fn job_referencing_unknown_account_is_rejected() {
    let config = sample_config();
    let mut jobs = sample_jobs();
    jobs.jobs[1].account_id = "gibts-nicht".into();

    let err = oc_config::validate_references(&config, &jobs).unwrap_err();
    assert!(err.to_string().contains("gibts-nicht"), "got: {err}");
}

#[test]
fn validate_references_accepts_consistent_files() {
    assert!(oc_config::validate_references(&sample_config(), &sample_jobs()).is_ok());
}

#[test]
fn account_with_invalid_server_url_is_rejected() {
    let mut config = sample_config();
    config.accounts[0].server_url = "not a url".into();
    let err = config.validate().unwrap_err();
    assert!(err.to_string().contains("server_url"), "got: {err}");
}

#[test]
fn settings_ranges_are_enforced() {
    let mut config = sample_config();
    config.settings.sync_interval_secs = 0;
    assert!(config.validate().is_err());

    let mut config = sample_config();
    config.settings.max_parallel_transfers = 0;
    assert!(config.validate().is_err());
}

#[test]
fn legacy_version_0_is_migrated_to_1() {
    let dir = tempfile::tempdir().unwrap();
    // v0 = no "version" field; settings filled with defaults.
    std::fs::write(
        dir.path().join("config.json"),
        r#"{
            "accounts": [
                {"id": "konto-1", "server_url": "https://cloud.example.com", "username": "max@example.com"}
            ]
        }"#,
    )
    .unwrap();

    let config = oc_config::load_config(dir.path()).unwrap();
    assert_eq!(config.version, 1);
    assert_eq!(config.accounts.len(), 1);
    assert_eq!(config.settings, Settings::default());

    // jobs.json v0: version-less job list.
    std::fs::write(
        dir.path().join("jobs.json"),
        r#"{
            "jobs": [
                {"id": "job-1", "account_id": "konto-1", "space_id": "s1",
                 "local_path": "/home/max/x"}
            ]
        }"#,
    )
    .unwrap();

    let jobs = oc_config::load_jobs(dir.path()).unwrap();
    assert_eq!(jobs.version, 1);
    assert_eq!(jobs.jobs.len(), 1);
    assert!(jobs.jobs[0].enabled, "enabled defaults to true");
    assert!(jobs.jobs[0].include.is_empty());
    assert!(jobs.jobs[0].exclude.is_empty());
}

#[test]
fn future_version_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"version": 99, "accounts": []}"#,
    )
    .unwrap();
    let err = oc_config::load_config(dir.path()).unwrap_err();
    assert!(err.to_string().contains("99"), "got: {err}");
}

#[test]
fn paths_point_into_config_dir() {
    let dir = Path::new("/base");
    assert_eq!(
        oc_config::config_path(dir),
        PathBuf::from("/base/config.json")
    );
    assert_eq!(oc_config::jobs_path(dir), PathBuf::from("/base/jobs.json"));
}
