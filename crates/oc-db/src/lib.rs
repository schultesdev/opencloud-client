//! SQLite status database (Phase 4): one database file per sync job at
//! `<config-dir>/db/<job-id>.sqlite`, resolved via [`db_path`].
//!
//! Building blocks:
//!
//! - [`StatusDb`]: opens a job database, applies schema migrations (WAL
//!   mode on), and offers the DAO layer — [`StatusDb::upsert_entry`],
//!   [`StatusDb::get_entry`], [`StatusDb::delete_subtree`],
//!   [`StatusDb::record_conflict`], [`StatusDb::resolve_conflict`], and
//!   [`StatusDb::stats`] for the UI.
//! - [`Entry`]/[`NewEntry`]: per-path last-known state (local mtime/size,
//!   remote ETag/size, checksum, [`EntryState`]).
//! - [`Conflict`]/[`NewConflict`]: detected conflicts with
//!   [`ConflictStatus`] and [`ConflictResolution`] (server wins / local
//!   wins).
//! - [`Stats`]: aggregated numbers (file/dir counts, open conflicts,
//!   errors, transfers, last sync).
//!
//! The schema is versioned in the `meta` table and migrated step by step;
//! a fresh database starts at version 0 and is migrated to
//! [`SCHEMA_VERSION`] on open.

mod db;
mod error;
mod migrations;
mod model;
mod paths;

pub use db::StatusDb;
pub use error::DbError;
pub use model::{
    Conflict, ConflictResolution, ConflictStatus, Entry, EntryState, NewConflict, NewEntry, Stats,
};
pub use paths::{DB_DIR, DB_EXTENSION, db_path};

/// Current schema version of the status database.
pub const SCHEMA_VERSION: u64 = migrations::SCHEMA_VERSION;
