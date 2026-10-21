//! Schema migrations for both configuration files.
//!
//! Files are loaded as [`serde_json::Value`] first; the `version` field is
//! read, migrations are applied step by step (v0 -> v1 -> ...), and the
//! result is then deserialized into the current types.

use serde_json::Value;

use crate::CONFIG_VERSION;
use crate::error::ConfigError;

/// Applies all pending migrations to a `config.json` document.
pub fn migrate_config(value: &mut Value) -> Result<(), ConfigError> {
    let version = document_version(value);
    check_supported(version)?;
    if version < 1 {
        v0_to_v1(value);
    }
    Ok(())
}

/// Applies all pending migrations to a `jobs.json` document.
pub fn migrate_jobs(value: &mut Value) -> Result<(), ConfigError> {
    let version = document_version(value);
    check_supported(version)?;
    if version < 1 {
        v0_to_v1(value);
    }
    Ok(())
}

/// Reads the `version` field; a missing or non-numeric version counts as 0
/// (the legacy pre-versioning format).
fn document_version(value: &Value) -> u64 {
    value.get("version").and_then(Value::as_u64).unwrap_or(0)
}

fn check_supported(found: u64) -> Result<(), ConfigError> {
    if found > CONFIG_VERSION {
        return Err(ConfigError::UnsupportedVersion {
            found,
            supported: CONFIG_VERSION,
        });
    }
    Ok(())
}

/// v0 -> v1: stamp the version and make sure all containers exist so that
/// field defaults can fill in during deserialization.
fn v0_to_v1(value: &mut Value) {
    if !value.is_object() {
        // Not an object: let deserialization produce a proper error later.
        return;
    }
    value["version"] = Value::from(1u64);
    if let Some(object) = value.as_object_mut() {
        object
            .entry("settings")
            .or_insert_with(|| Value::Object(Default::default()));
        object
            .entry("accounts")
            .or_insert_with(|| Value::Array(Vec::new()));
        object
            .entry("jobs")
            .or_insert_with(|| Value::Array(Vec::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_version_migrates_to_v1() {
        let mut value = serde_json::json!({
            "accounts": [{"id": "a", "server_url": "https://x", "username": "u"}]
        });
        migrate_config(&mut value).unwrap();
        assert_eq!(value["version"].as_u64(), Some(1));
        assert!(value["settings"].is_object());
    }

    #[test]
    fn current_version_passes_through() {
        let mut value = serde_json::json!({"version": 1, "accounts": []});
        migrate_config(&mut value).unwrap();
        assert_eq!(value["version"].as_u64(), Some(1));
    }

    #[test]
    fn future_version_is_rejected() {
        let mut value = serde_json::json!({"version": 99});
        let err = migrate_config(&mut value).unwrap_err();
        assert!(err.to_string().contains("99"), "got: {err}");
    }

    #[test]
    fn non_object_document_is_left_to_deserialization() {
        let mut value = serde_json::json!(42);
        migrate_config(&mut value).unwrap();
        assert_eq!(value, serde_json::json!(42));
    }
}
