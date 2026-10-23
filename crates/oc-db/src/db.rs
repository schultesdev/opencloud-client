//! The status database handle: opens (and migrates) a per-job SQLite file
//! and offers the DAO operations used by the sync engine and the UI.

use std::path::Path;
use std::path::PathBuf;

use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::error::DbError;
use crate::migrations;
use crate::model::Conflict;
use crate::model::ConflictResolution;
use crate::model::ConflictStatus;
use crate::model::Entry;
use crate::model::EntryState;
use crate::model::NewConflict;
use crate::model::NewEntry;
use crate::model::Stats;

/// A per-job status database. One instance owns one SQLite connection; the
/// sync engine keeps one per job and the UI reads through snapshots.
#[derive(Debug)]
pub struct StatusDb {
    conn: Connection,
    path: PathBuf,
}

impl StatusDb {
    /// Opens (or creates) the database file for a sync job, applies
    /// pending migrations, and enables WAL mode.
    ///
    /// The parent directory is created if it does not exist yet.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| DbError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let conn = Connection::open(path).map_err(|source| DbError::open(path, source))?;
        // WAL first: the migration itself already runs in WAL mode, so
        // concurrent readers (GUI, CLI) are not blocked by it.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::enable_wal(&conn)?;
        Self::from_conn(conn, path.to_path_buf())
    }

    /// Opens a private in-memory database (used by tests and tools that
    /// only need throwaway state).
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn =
            Connection::open_in_memory().map_err(|source| DbError::open("<in-memory>", source))?;
        Self::from_conn(conn, PathBuf::from("<in-memory>"))
    }

    /// Common setup for file-backed and in-memory databases.
    fn from_conn(mut conn: Connection, path: PathBuf) -> Result<Self, DbError> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrations::migrate(&mut conn)?;
        Ok(Self { conn, path })
    }

    /// Switches the journal to WAL. Best effort: in-memory databases (and
    /// exotic filesystems) keep their default journal.
    fn enable_wal(conn: &Connection) -> Result<(), DbError> {
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(())
    }

    /// Path of the underlying database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current SQLite journal mode (`wal` for healthy file databases).
    pub fn journal_mode(&self) -> Result<String, DbError> {
        Ok(self
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?)
    }

    /// Reads a `meta` value (`schema_version`, `space_etag`, `last_sync`, ...).
    pub fn meta(&self, key: &str) -> Result<Option<String>, DbError> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?)
    }

    /// The recorded server spellings of non-NFC paths (normalized path →
    /// server path); see [`StatusDb::replace_server_names`].
    pub fn server_names(&self) -> Result<std::collections::HashMap<String, String>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, server_path FROM server_names")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Replaces the recorded server spellings with the ones of a complete
    /// remote scan, so passes that skip the scan (root-ETag shortcut) and
    /// conflict resolutions still address such files by their real name.
    pub fn replace_server_names(
        &self,
        names: &std::collections::HashMap<String, String>,
    ) -> Result<(), DbError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM server_names", [])?;
        {
            let mut insert =
                tx.prepare("INSERT INTO server_names (path, server_path) VALUES (?1, ?2)")?;
            for (path, server_path) in names {
                insert.execute(params![path, server_path])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Writes a `meta` value, overwriting any existing entry.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Removes a `meta` value; a missing key is fine.
    pub fn delete_meta(&self, key: &str) -> Result<(), DbError> {
        self.conn
            .execute("DELETE FROM meta WHERE key = ?1", [key])?;
        Ok(())
    }

    /// Binds the database to the job it tracks. `identity` describes what
    /// the recorded state refers to (account, space, local root). A fresh
    /// database adopts it; a database recorded for a different identity —
    /// e.g. a reused job id, or a job pointed at another space or folder —
    /// is reset (entries, conflicts, space ETag, last sync), because its
    /// "last synced" baseline would otherwise turn into deletions on the
    /// new target. Returns whether a reset happened.
    pub fn ensure_identity(&self, identity: &str) -> Result<bool, DbError> {
        let tx = self.conn.unchecked_transaction()?;
        let stored: Option<String> = tx
            .query_row(
                "SELECT value FROM meta WHERE key = 'job_identity'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let reset = match stored.as_deref() {
            Some(stored) if stored == identity => return Ok(false),
            Some(_) => {
                tx.execute_batch(
                    "DELETE FROM entries;
                     DELETE FROM conflicts;
                     DELETE FROM server_names;
                     DELETE FROM meta WHERE key IN ('space_etag', 'last_sync');",
                )?;
                true
            }
            None => false,
        };
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('job_identity', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [identity],
        )?;
        tx.commit()?;
        Ok(reset)
    }

    /// Stores when the job last completed a sync run (`meta.last_sync`,
    /// unix seconds).
    pub fn set_last_sync(&self, unix_seconds: i64) -> Result<(), DbError> {
        self.set_meta("last_sync", &unix_seconds.to_string())
    }

    /// Inserts or updates an entry keyed by its path. All columns are
    /// replaced with the values of `entry`.
    pub fn upsert_entry(&self, entry: &NewEntry) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT INTO entries
               (path, is_dir, local_mtime, local_size, remote_etag,
                remote_size, checksum, last_synced_at, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(path) DO UPDATE SET
               is_dir = excluded.is_dir,
               local_mtime = excluded.local_mtime,
               local_size = excluded.local_size,
               remote_etag = excluded.remote_etag,
               remote_size = excluded.remote_size,
               checksum = excluded.checksum,
               last_synced_at = excluded.last_synced_at,
               state = excluded.state",
            params![
                entry.path,
                entry.is_dir,
                entry.local_mtime,
                entry.local_size,
                entry.remote_etag,
                entry.remote_size,
                entry.checksum,
                entry.last_synced_at,
                entry.state.as_str(),
            ],
        )?;
        Ok(())
    }

    /// Runs `operations` in one transaction: all of their writes commit
    /// together (one disk sync instead of one per statement), or none do
    /// when `operations` fails. Not reentrant — `operations` must not call
    /// `transaction` again.
    pub fn transaction<T>(
        &self,
        operations: impl FnOnce(&Self) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let tx = self.conn.unchecked_transaction()?;
        let value = operations(self)?;
        tx.commit()?;
        Ok(value)
    }

    /// Sets the lifecycle state of a tracked path; returns whether the
    /// path was tracked.
    pub fn set_state(&self, path: &str, state: EntryState) -> Result<bool, DbError> {
        Ok(self.conn.execute(
            "UPDATE entries SET state = ?2 WHERE path = ?1",
            params![path, state.as_str()],
        )? > 0)
    }

    /// Resets every entry left in a transfer state (`uploading`,
    /// `downloading`) by an interrupted run to `synced`: its recorded
    /// baseline is intact, so the next diff plans the transfer again.
    /// Returns the number of reset entries.
    pub fn reset_transfer_states(&self) -> Result<usize, DbError> {
        Ok(self.conn.execute(
            "UPDATE entries SET state = 'synced'
             WHERE state IN ('uploading', 'downloading')",
            [],
        )?)
    }

    /// Returns the entry for an exact path, if tracked.
    pub fn get_entry(&self, path: &str) -> Result<Option<Entry>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, is_dir, local_mtime, local_size, remote_etag,
                    remote_size, checksum, last_synced_at, state
             FROM entries WHERE path = ?1",
        )?;
        let mut rows = stmt.query([path])?;
        Ok(match rows.next()? {
            Some(row) => Some(row_to_entry(row)?),
            None => None,
        })
    }

    /// Deletes the entry for an exact path. Returns whether a row was
    /// removed.
    pub fn delete_entry(&self, path: &str) -> Result<bool, DbError> {
        self.conn
            .execute("DELETE FROM server_names WHERE path = ?1", [path])?;
        Ok(self
            .conn
            .execute("DELETE FROM entries WHERE path = ?1", [path])?
            > 0)
    }

    /// Deletes an entry and everything beneath it (`path` acts as a
    /// folder prefix; a sibling named `foo2` is not touched when `foo` is
    /// deleted). Used when files/folders vanish or when selective sync
    /// filters shrink the synced tree. Returns the number of removed rows.
    ///
    /// The descendants are selected as the byte range `[path/, path0)` —
    /// `'0'` is the character right after `'/'` — which is exact and
    /// case-sensitive (unlike `LIKE`, where `_`/`%` in names are
    /// wildcards and ASCII case is ignored) and uses the path index.
    pub fn delete_subtree(&self, path: &str) -> Result<usize, DbError> {
        let bounds = params![path, format!("{path}/"), format!("{path}0")];
        self.conn.execute(
            "DELETE FROM server_names WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
            bounds,
        )?;
        let removed = self.conn.execute(
            "DELETE FROM entries WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
            bounds,
        )?;
        Ok(removed)
    }

    /// Lists all tracked entries, ordered by path. The snapshot the sync
    /// engine diffs its local and remote scans against.
    pub fn all_entries(&self) -> Result<Vec<Entry>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, is_dir, local_mtime, local_size, remote_etag,
                    remote_size, checksum, last_synced_at, state
             FROM entries ORDER BY path",
        )?;
        let rows = stmt
            .query_map([], row_to_entry)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Lists up to `limit` entries in the given state, ordered by path;
    /// used by startup recovery and the UI error list.
    pub fn entries_with_state(&self, state: EntryState, limit: i64) -> Result<Vec<Entry>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, is_dir, local_mtime, local_size, remote_etag,
                    remote_size, checksum, last_synced_at, state
             FROM entries WHERE state = ?1
             ORDER BY path LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![state.as_str(), limit], row_to_entry)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Records a newly detected conflict and returns its row id. The
    /// conflict starts as `pending`.
    ///
    /// A path has at most one pending conflict: recording one for a path
    /// that already has a pending row refreshes that row (detection time
    /// and both sides) and returns its id.
    pub fn record_conflict(&self, conflict: &NewConflict) -> Result<i64, DbError> {
        Ok(self.conn.query_row(
            "INSERT INTO conflicts
               (path, detected_at, local_mtime, local_size,
                remote_etag, remote_size, remote_mtime, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending')
             ON CONFLICT (path) WHERE status = 'pending' DO UPDATE SET
               detected_at = excluded.detected_at,
               local_mtime = excluded.local_mtime,
               local_size = excluded.local_size,
               remote_etag = excluded.remote_etag,
               remote_size = excluded.remote_size,
               remote_mtime = excluded.remote_mtime
             RETURNING id",
            params![
                conflict.path,
                conflict.detected_at,
                conflict.local_mtime,
                conflict.local_size,
                conflict.remote_etag,
                conflict.remote_size,
                conflict.remote_mtime,
            ],
            |row| row.get(0),
        )?)
    }

    /// Applies a resolution to a pending conflict: sets `status` and
    /// `resolved_at`. Returns `false` if the id is unknown or the conflict
    /// was already resolved.
    pub fn resolve_conflict(
        &self,
        conflict_id: i64,
        resolution: ConflictResolution,
        resolved_at: i64,
    ) -> Result<bool, DbError> {
        let changed = self.conn.execute(
            "UPDATE conflicts
             SET status = ?1, resolved_at = ?2
             WHERE id = ?3 AND status = 'pending'",
            params![
                resolution.resulting_status().as_str(),
                resolved_at,
                conflict_id
            ],
        )?;
        Ok(changed > 0)
    }

    /// Lists conflicts by status, oldest first.
    pub fn conflicts_with_status(&self, status: ConflictStatus) -> Result<Vec<Conflict>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, detected_at, local_mtime, local_size,
                    remote_etag, remote_size, remote_mtime, status, resolved_at
             FROM conflicts WHERE status = ?1 ORDER BY detected_at, id",
        )?;
        let rows = stmt
            .query_map([status.as_str()], row_to_conflict)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// All pending conflicts, oldest first — the UI conflict list.
    pub fn pending_conflicts(&self) -> Result<Vec<Conflict>, DbError> {
        self.conflicts_with_status(ConflictStatus::Pending)
    }

    /// Returns one conflict by row id, regardless of status; `None`
    /// when the id is unknown. Resolution flows use it to load the
    /// conflict before applying its side of the bargain.
    pub fn get_conflict(&self, conflict_id: i64) -> Result<Option<Conflict>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, detected_at, local_mtime, local_size,
                    remote_etag, remote_size, remote_mtime, status, resolved_at
             FROM conflicts WHERE id = ?1",
        )?;
        let mut rows = stmt.query([conflict_id])?;
        Ok(match rows.next()? {
            Some(row) => Some(row_to_conflict(row)?),
            None => None,
        })
    }

    /// Re-records the remote state of a still-pending conflict: the
    /// server changed again while a resolution was on its way (e.g. the
    /// `If-Match` upload of `resolve_local` failed with 412). Returns
    /// `false` when the id is unknown or the conflict is already
    /// resolved — then the conflict stays exactly as it was.
    pub fn update_conflict_remote(
        &self,
        conflict_id: i64,
        remote_etag: Option<&str>,
        remote_size: Option<i64>,
        remote_mtime: Option<i64>,
    ) -> Result<bool, DbError> {
        let changed = self.conn.execute(
            "UPDATE conflicts
             SET remote_etag = ?1, remote_size = ?2, remote_mtime = ?3
             WHERE id = ?4 AND status = 'pending'",
            params![remote_etag, remote_size, remote_mtime, conflict_id],
        )?;
        Ok(changed > 0)
    }

    /// Aggregated numbers for the UI (file/dir counts, open conflicts,
    /// errors, active transfers, last sync).
    pub fn stats(&self) -> Result<Stats, DbError> {
        // One statement: the numbers form a consistent snapshot even
        // while the engine writes, and `entries` is scanned once.
        let (files, dirs, transfers, errors, conflicts, last_sync) = self.conn.query_row(
            "SELECT
               COALESCE(SUM(is_dir = 0), 0),
               COALESCE(SUM(is_dir = 1), 0),
               COALESCE(SUM(state IN ('uploading', 'downloading')), 0),
               COALESCE(SUM(state = 'error'), 0),
               (SELECT COUNT(*) FROM conflicts WHERE status = 'pending'),
               (SELECT value FROM meta WHERE key = 'last_sync')
             FROM entries",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )?;
        Ok(Stats {
            files: files as u64,
            dirs: dirs as u64,
            transfers: transfers as u64,
            conflicts: conflicts as u64,
            errors: errors as u64,
            last_sync: last_sync.and_then(|value| value.parse().ok()),
        })
    }
}

/// Maps one `entries` row to an [`Entry`]. Returns rusqlite errors so the
/// function can be used with `query_map`; unknown enum values are
/// surfaced as conversion failures and become `DbError::Query` upstream.
fn row_to_entry(row: &rusqlite::Row<'_>) -> Result<Entry, rusqlite::Error> {
    let state_raw: String = row.get(9)?;
    let state = EntryState::from_db(&state_raw).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(err))
    })?;
    Ok(Entry {
        id: row.get(0)?,
        entry: NewEntry {
            path: row.get(1)?,
            is_dir: row.get::<_, i64>(2)? != 0,
            local_mtime: row.get(3)?,
            local_size: row.get(4)?,
            remote_etag: row.get(5)?,
            remote_size: row.get(6)?,
            checksum: row.get(7)?,
            last_synced_at: row.get(8)?,
            state,
        },
    })
}

/// Maps one `conflicts` row to a [`Conflict`] (see [`row_to_entry`]).
fn row_to_conflict(row: &rusqlite::Row<'_>) -> Result<Conflict, rusqlite::Error> {
    let status_raw: String = row.get(8)?;
    let status = ConflictStatus::from_db(&status_raw).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(err))
    })?;
    Ok(Conflict {
        id: row.get(0)?,
        path: row.get(1)?,
        detected_at: row.get(2)?,
        local_mtime: row.get(3)?,
        local_size: row.get(4)?,
        remote_etag: row.get(5)?,
        remote_size: row.get(6)?,
        remote_mtime: row.get(7)?,
        status,
        resolved_at: row.get(9)?,
    })
}
