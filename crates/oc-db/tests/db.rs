//! Integration tests for the per-job status database: migrations, WAL
//! mode, the DAO layer (entries, conflicts, meta), and statistics.

// Test helpers may use unwrap/expect just like the test functions.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use oc_db::{
    ConflictResolution, ConflictStatus, DbError, EntryState, NewConflict, NewEntry, SCHEMA_VERSION,
    StatusDb, db_path,
};

/// Unix seconds helper: keeps the tests independent of `SystemTime`.
fn unix(seconds: i64) -> i64 {
    seconds
}

#[test]
fn fresh_file_database_migrates_from_v0_to_v1() {
    let dir = tempfile::tempdir().unwrap();
    let db = StatusDb::open(dir.path().join("job-1.sqlite")).unwrap();

    // Migration stamped the version in meta.
    assert_eq!(
        db.meta("schema_version").unwrap().as_deref(),
        Some(SCHEMA_VERSION.to_string().as_str())
    );
    // WAL journal is active for file databases.
    assert_eq!(db.journal_mode().unwrap().to_lowercase(), "wal");
}

#[test]
fn reopening_an_existing_database_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("job-1.sqlite");
    StatusDb::open(&path).unwrap();

    let db = StatusDb::open(&path).unwrap();
    assert_eq!(
        db.meta("schema_version").unwrap().as_deref(),
        Some("1".to_string().as_str())
    );
}

#[test]
fn upsert_inserts_then_updates_the_same_path() {
    let db = StatusDb::open_in_memory().unwrap();

    db.upsert_entry(&NewEntry {
        path: "Dokumente/Bericht Q1.pdf".into(),
        is_dir: false,
        local_mtime: Some(unix(100)),
        local_size: Some(2048),
        remote_etag: Some("\"etag-1\"".into()),
        remote_size: Some(2048),
        checksum: Some("xxh:abcd".into()),
        last_synced_at: Some(unix(120)),
        state: EntryState::Synced,
    })
    .unwrap();
    let entry = db.get_entry("Dokumente/Bericht Q1.pdf").unwrap().unwrap();
    assert_eq!(entry.id, 1);
    assert!(!entry.entry.is_dir);
    assert_eq!(entry.entry.remote_etag.as_deref(), Some("\"etag-1\""));
    assert_eq!(entry.entry.state, EntryState::Synced);

    // Second upsert with the same path updates in place (no duplicate row).
    db.upsert_entry(&NewEntry {
        path: "Dokumente/Bericht Q1.pdf".into(),
        is_dir: false,
        local_mtime: Some(unix(200)),
        local_size: Some(4096),
        remote_etag: Some("\"etag-2\"".into()),
        remote_size: Some(4096),
        checksum: None,
        last_synced_at: Some(unix(210)),
        state: EntryState::Error,
    })
    .unwrap();
    let updated = db.get_entry("Dokumente/Bericht Q1.pdf").unwrap().unwrap();
    assert_eq!(updated.id, 1, "same row updated");
    assert_eq!(updated.entry.state, EntryState::Error);
    assert_eq!(updated.entry.local_size, Some(4096));
    assert_eq!(updated.entry.remote_etag.as_deref(), Some("\"etag-2\""));

    // Unknown path reads as None.
    assert!(db.get_entry("gibt-es-nicht.txt").unwrap().is_none());
}

#[test]
fn all_entries_returns_every_path_ordered() {
    let db = StatusDb::open_in_memory().unwrap();
    db.upsert_entry(&NewEntry::new("b.txt", false, EntryState::Synced))
        .unwrap();
    db.upsert_entry(&NewEntry::new("a/Dokumente", true, EntryState::Synced))
        .unwrap();
    db.upsert_entry(&NewEntry::new("a/a.txt", false, EntryState::Error))
        .unwrap();

    let all = db.all_entries().unwrap();
    assert_eq!(all.len(), 3);
    let paths: Vec<&str> = all.iter().map(|e| e.entry.path.as_str()).collect();
    assert_eq!(paths, vec!["a/Dokumente", "a/a.txt", "b.txt"]);
    assert!(all[0].entry.is_dir);
    assert_eq!(all[2].entry.state, EntryState::Synced);
}

#[test]
fn delete_subtree_removes_only_the_prefix_tree() {
    let db = StatusDb::open_in_memory().unwrap();
    for (path, is_dir) in [
        ("Dokumente", true),
        ("Dokumente/2026", true),
        ("Dokumente/2026/a.txt", false),
        ("Dokumente/a.txt", false),
        ("Dokumente2/b.txt", false),
        ("root.txt", false),
    ] {
        db.upsert_entry(&NewEntry::new(path, is_dir, EntryState::Synced))
            .unwrap();
    }

    let removed = db.delete_subtree("Dokumente").unwrap();
    assert_eq!(removed, 4);
    assert!(db.get_entry("Dokumente").unwrap().is_none());
    assert!(db.get_entry("Dokumente/2026/a.txt").unwrap().is_none());
    // Sibling with a shared name prefix and unrelated files survive.
    assert!(db.get_entry("Dokumente2/b.txt").unwrap().is_some());
    assert!(db.get_entry("root.txt").unwrap().is_some());

    // Deleting a single entry reports whether it existed.
    assert!(db.delete_entry("root.txt").unwrap());
    assert!(!db.delete_entry("root.txt").unwrap());
}

#[test]
fn delete_subtree_treats_wildcards_and_case_literally() {
    let db = StatusDb::open_in_memory().unwrap();
    for path in [
        "my_dir/a.txt",
        "myXdir/b.txt",
        "100%/c.txt",
        "100x/d.txt",
        "Foo/e.txt",
        "foo/f.txt",
    ] {
        db.upsert_entry(&NewEntry::new(path, false, EntryState::Synced))
            .unwrap();
    }
    assert_eq!(db.delete_subtree("my_dir").unwrap(), 1);
    assert!(db.get_entry("myXdir/b.txt").unwrap().is_some());
    assert_eq!(db.delete_subtree("100%").unwrap(), 1);
    assert!(db.get_entry("100x/d.txt").unwrap().is_some());
    assert_eq!(db.delete_subtree("Foo").unwrap(), 1);
    assert!(db.get_entry("foo/f.txt").unwrap().is_some());
}

#[test]
fn identity_change_resets_the_database() {
    let db = StatusDb::open_in_memory().unwrap();
    // A fresh database adopts the identity.
    assert!(!db.ensure_identity("acc\nspace-a\n/tmp/a").unwrap());
    db.upsert_entry(&NewEntry::new("a.txt", false, EntryState::Synced))
        .unwrap();
    db.set_meta("space_etag", "\"e\"").unwrap();
    db.record_conflict(&NewConflict {
        path: "a.txt".into(),
        detected_at: 1,
        local_mtime: None,
        local_size: None,
        remote_etag: None,
        remote_size: None,
        remote_mtime: None,
    })
    .unwrap();

    // Same identity: nothing changes.
    assert!(!db.ensure_identity("acc\nspace-a\n/tmp/a").unwrap());
    assert!(db.get_entry("a.txt").unwrap().is_some());

    // Another space or path: the old state is dropped.
    assert!(db.ensure_identity("acc\nspace-b\n/tmp/a").unwrap());
    assert!(db.all_entries().unwrap().is_empty());
    assert!(db.pending_conflicts().unwrap().is_empty());
    assert_eq!(db.meta("space_etag").unwrap(), None);
    assert!(!db.ensure_identity("acc\nspace-b\n/tmp/a").unwrap());
}

#[test]
fn entries_with_state_filters_and_is_bounded() {
    let db = StatusDb::open_in_memory().unwrap();
    db.upsert_entry(&NewEntry::new("a.txt", false, EntryState::Error))
        .unwrap();
    db.upsert_entry(&NewEntry::new("b.txt", false, EntryState::Synced))
        .unwrap();
    db.upsert_entry(&NewEntry::new("c.txt", false, EntryState::Error))
        .unwrap();

    let errors = db.entries_with_state(EntryState::Error, 10).unwrap();
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0].entry.path, "a.txt");
    let bounded = db.entries_with_state(EntryState::Error, 1).unwrap();
    assert_eq!(bounded.len(), 1);
}

#[test]
fn conflict_lifecycle_from_detection_to_resolution() {
    let db = StatusDb::open_in_memory().unwrap();
    let id = db
        .record_conflict(&NewConflict {
            path: "Bild/2026/foto.jpg".into(),
            detected_at: unix(500),
            local_mtime: Some(unix(400)),
            local_size: Some(1024),
            remote_etag: Some("\"etag-x\"".into()),
            remote_size: Some(2048),
            remote_mtime: Some(unix(450)),
        })
        .unwrap();
    assert_eq!(id, 1);

    let pending = db.pending_conflicts().unwrap();
    assert_eq!(pending.len(), 1);
    let conflict = &pending[0];
    assert_eq!(conflict.path, "Bild/2026/foto.jpg");
    assert_eq!(conflict.status, ConflictStatus::Pending);
    assert_eq!(conflict.remote_size, Some(2048));
    assert!(conflict.resolved_at.is_none());

    // Resolve in favor of the server version.
    assert!(
        db.resolve_conflict(id, ConflictResolution::Server, unix(600))
            .unwrap()
    );
    let resolved = &db
        .conflicts_with_status(ConflictStatus::ResolvedServer)
        .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].resolved_at, Some(unix(600)));
    assert!(db.pending_conflicts().unwrap().is_empty());

    // Already resolved: a second resolution is rejected.
    assert!(
        !db.resolve_conflict(id, ConflictResolution::Local, unix(700))
            .unwrap()
    );
    // Unknown ids are rejected too.
    assert!(
        !db.resolve_conflict(999, ConflictResolution::Local, unix(700))
            .unwrap()
    );
}

#[test]
fn get_conflict_and_remote_state_update() {
    let db = StatusDb::open_in_memory().unwrap();
    let id = db
        .record_conflict(&NewConflict {
            path: "a.txt".into(),
            detected_at: unix(500),
            local_mtime: Some(unix(400)),
            local_size: Some(10),
            remote_etag: Some("\"e1\"".into()),
            remote_size: Some(10),
            remote_mtime: None,
        })
        .unwrap();
    assert!(db.get_conflict(999).unwrap().is_none());

    // The server changed again during an attempted resolution: the
    // pending row learns the new remote state.
    assert!(
        db.update_conflict_remote(id, Some("\"e2\""), Some(12), Some(unix(550)))
            .unwrap()
    );
    let conflict = db.get_conflict(id).unwrap().unwrap();
    assert_eq!(conflict.remote_etag.as_deref(), Some("\"e2\""));
    assert_eq!(conflict.remote_size, Some(12));
    assert_eq!(conflict.remote_mtime, Some(unix(550)));

    // Once resolved, the remote state is frozen.
    db.resolve_conflict(id, ConflictResolution::Server, unix(600))
        .unwrap();
    assert!(
        !db.update_conflict_remote(id, Some("\"e3\""), None, None)
            .unwrap()
    );
    assert_eq!(
        db.get_conflict(id).unwrap().unwrap().remote_etag.as_deref(),
        Some("\"e2\"")
    );
}

#[test]
fn stats_aggregates_entries_conflicts_and_last_sync() {
    let db = StatusDb::open_in_memory().unwrap();
    db.upsert_entry(&NewEntry::new("ordner", true, EntryState::Synced))
        .unwrap();
    db.upsert_entry(&NewEntry::new("ordner/a.txt", false, EntryState::Synced))
        .unwrap();
    db.upsert_entry(&NewEntry::new("b.txt", false, EntryState::Uploading))
        .unwrap();
    db.upsert_entry(&NewEntry::new("c.txt", false, EntryState::Downloading))
        .unwrap();
    db.upsert_entry(&NewEntry::new("d.txt", false, EntryState::Error))
        .unwrap();
    db.record_conflict(&NewConflict {
        path: "e.txt".into(),
        detected_at: unix(1),
        local_mtime: None,
        local_size: None,
        remote_etag: None,
        remote_size: None,
        remote_mtime: None,
    })
    .unwrap();
    db.set_last_sync(unix(1234)).unwrap();

    let stats = db.stats().unwrap();
    assert_eq!(stats.files, 4);
    assert_eq!(stats.dirs, 1);
    assert_eq!(stats.transfers, 2);
    assert_eq!(stats.errors, 1);
    assert_eq!(stats.conflicts, 1);
    assert_eq!(stats.last_sync, Some(unix(1234)));
}

#[test]
fn meta_roundtrip_and_arbitrary_keys() {
    let db = StatusDb::open_in_memory().unwrap();
    assert_eq!(db.meta("space_etag").unwrap(), None);
    db.set_meta("space_etag", "\"root-42\"").unwrap();
    db.set_meta("space_etag", "\"root-43\"").unwrap();
    assert_eq!(
        db.meta("space_etag").unwrap().as_deref(),
        Some("\"root-43\"")
    );
}

#[test]
fn db_path_lays_files_under_the_db_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(dir.path(), "job-1").unwrap();
    assert!(path.ends_with("db/job-1.sqlite"));
    assert!(StatusDb::open(&path).is_ok());
    // Parent directories are created on demand.
    assert!(path.is_file());

    assert!(matches!(
        db_path(dir.path(), "../escape").unwrap_err(),
        DbError::InvalidJobId { .. }
    ));
}
