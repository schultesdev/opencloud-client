//! Versioned schema migrations, applied step by step from the version
//! recorded in the `meta` table.

/// Highest schema version this build understands.
pub const SCHEMA_VERSION: u64 = 1;

/// All known migrations, ordered by target version. Each entry upgrades a
/// database from `version - 1` to `version`.
const MIGRATIONS: &[(u64, &str)] = &[(
    1,
    r#"
        CREATE TABLE entries (
          id INTEGER PRIMARY KEY,
          path TEXT UNIQUE NOT NULL,
          is_dir INTEGER NOT NULL,
          local_mtime INTEGER,
          local_size INTEGER,
          remote_etag TEXT,
          remote_size INTEGER,
          checksum TEXT,
          last_synced_at INTEGER,
          state TEXT NOT NULL CHECK (state IN ('synced','uploading','downloading','conflict','error'))
        );
        CREATE INDEX idx_entries_parent ON entries (path);

        CREATE TABLE conflicts (
          id INTEGER PRIMARY KEY,
          path TEXT NOT NULL,
          detected_at INTEGER NOT NULL,
          local_mtime INTEGER, local_size INTEGER,
          remote_etag TEXT, remote_size INTEGER, remote_mtime INTEGER,
          status TEXT NOT NULL CHECK (status IN ('pending','resolved_server','resolved_local')),
          resolved_at INTEGER
        );
        CREATE INDEX idx_conflicts_status ON conflicts (status);
        "#,
)];

/// Creates the `meta` table if needed and returns the current schema
/// version (0 for a fresh, pre-schema database).
pub(super) fn current_version(conn: &rusqlite::Connection) -> Result<u64, rusqlite::Error> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);")?;
    Ok(conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

/// Applies all pending migrations in one transaction and stamps the new
/// version into `meta`. Databases written by a newer build are rejected.
pub(super) fn migrate(conn: &rusqlite::Connection) -> Result<(), crate::DbError> {
    let mut version = current_version(conn)?;
    if version > SCHEMA_VERSION {
        return Err(crate::DbError::InvalidValue {
            column: "schema_version",
            value: version.to_string(),
        });
    }
    for &(target, sql) in MIGRATIONS {
        if version < target {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [target.to_string()],
            )?;
            tx.commit()?;
            version = target;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_migrates_to_latest_version() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), SCHEMA_VERSION);
        // Tables exist and constraints are active.
        conn.execute(
            "INSERT INTO entries (path, is_dir, state) VALUES ('a.txt', 0, 'synced')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO entries (path, is_dir, state) VALUES ('b.txt', 0, 'bogus')",
                [],
            )
            .is_err()
        );
    }

    #[test]
    fn migrating_twice_is_a_noop() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn future_schema_version_is_rejected() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO meta (key, value) VALUES ('schema_version', '99');",
        )
        .unwrap();
        let err = migrate(&conn).unwrap_err();
        assert!(err.to_string().contains('9'), "got: {err}");
    }
}
