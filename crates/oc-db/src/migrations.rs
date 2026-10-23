//! Versioned schema migrations, applied step by step from the version
//! recorded in the `meta` table.

use rusqlite::OptionalExtension;
use rusqlite::TransactionBehavior;

/// Highest schema version this build understands.
pub const SCHEMA_VERSION: u64 = 3;

/// All known migrations, ordered by target version. Each entry upgrades a
/// database from `version - 1` to `version`.
const MIGRATIONS: &[(u64, &str)] = &[
    (
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
    ),
    (
        2,
        r#"
        -- `path UNIQUE` already indexes the paths; the extra index only
        -- doubled the write cost.
        DROP INDEX IF EXISTS idx_entries_parent;
        -- Small partial index for the non-synced states (transfers,
        -- errors, conflicts) the UI and the recovery query look up.
        CREATE INDEX idx_entries_active ON entries (state, path)
          WHERE state <> 'synced';

        -- At most one pending conflict per path; older duplicates (if a
        -- database ever got them) are dropped first.
        DELETE FROM conflicts
          WHERE status = 'pending'
            AND id NOT IN (SELECT MAX(id) FROM conflicts
                           WHERE status = 'pending' GROUP BY path);
        CREATE UNIQUE INDEX idx_conflicts_pending_path ON conflicts (path)
          WHERE status = 'pending';

        -- Server spellings of paths whose name is not in NFC (e.g. NFD
        -- names uploaded from macOS): key = normalized path, value = the
        -- path that addresses the resource on the server.
        CREATE TABLE server_names (
          path TEXT PRIMARY KEY,
          server_path TEXT NOT NULL
        );
        "#,
    ),
    (
        3,
        r#"
        -- Local modification times are stored in milliseconds from now
        -- on (they were whole seconds); the diff treats a whole-second
        -- value as "recorded at second precision".
        UPDATE entries SET local_mtime = local_mtime * 1000
          WHERE local_mtime IS NOT NULL;
        UPDATE conflicts SET local_mtime = local_mtime * 1000
          WHERE local_mtime IS NOT NULL;
        "#,
    ),
];

/// Creates the `meta` table if needed and returns the current schema
/// version (0 for a fresh, pre-schema database).
pub(super) fn current_version(conn: &rusqlite::Connection) -> Result<u64, crate::DbError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);")?;
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match stored {
        None => Ok(0),
        Some(value) => value.parse().map_err(|_| crate::DbError::InvalidValue {
            column: "schema_version",
            value,
        }),
    }
}

/// Applies all pending migrations and stamps the new version into `meta`.
/// Databases written by a newer build are rejected.
///
/// The engine, the GUI, and the CLI may open the same database at the
/// same time, so the version is read inside an `IMMEDIATE` transaction:
/// it takes the write lock up front, and a second process waits (busy
/// timeout) and then sees the already migrated version instead of running
/// the same migration twice.
pub(super) fn migrate(conn: &mut rusqlite::Connection) -> Result<(), crate::DbError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut version = current_version(&tx)?;
    if version > SCHEMA_VERSION {
        return Err(crate::DbError::InvalidValue {
            column: "schema_version",
            value: version.to_string(),
        });
    }
    for &(target, sql) in MIGRATIONS {
        if version < target {
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [target.to_string()],
            )?;
            version = target;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_migrates_to_latest_version() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
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
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn future_schema_version_is_rejected() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO meta (key, value) VALUES ('schema_version', '99');",
        )
        .unwrap();
        let err = migrate(&mut conn).unwrap_err();
        assert!(err.to_string().contains('9'), "got: {err}");
    }

    #[test]
    fn garbage_schema_version_is_an_error_not_a_remigration() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO meta (key, value) VALUES ('schema_version', 'kaputt');",
        )
        .unwrap();
        assert!(migrate(&mut conn).is_err());
    }

    #[test]
    fn v1_database_with_duplicate_pending_conflicts_migrates() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        // Build a v1 database by hand.
        conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        conn.execute_batch(MIGRATIONS[0].1).unwrap();
        conn.execute_batch(
            "INSERT INTO meta VALUES ('schema_version', '1');
             INSERT INTO conflicts (path, detected_at, status) VALUES ('a', 1, 'pending');
             INSERT INTO conflicts (path, detected_at, status) VALUES ('a', 2, 'pending');",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conflicts WHERE status = 'pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1);
    }
}
