//! The three-way diff: local scan vs. remote scan vs. last-synced state
//! from the status database, producing the action plan for one run.
//!
//! Classification per path (see `PLAN.md`, Phase 5):
//!
//! ```text
//! lokal geändert  = mtime/size != DB-Werte
//! remote geändert = ETag != DB-Wert (Fallback: size, wenn ohne ETag)
//! → beide ruhig: nichts
//! → nur lokal:   Upload (PUT mit If-Match = altem ETag)
//! → nur remote:  Download
//! → beide:       KONFLIKT (Eintrag in conflicts, Datei wird nicht angefasst)
//! → gelöscht:    Löschung wird auf die andere Seite übertragen;
//!                Änderung vs. Löschung gilt ebenfalls als Konflikt
//! ```
//!
//! Folders are only compared by existence — their ETags change whenever
//! anything below them changes, which the per-file diff already covers.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;

use oc_db::NewEntry;

use crate::scan::LocalNode;
use crate::scan::RemoteNode;

/// One planned step of a sync run.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncAction {
    /// Create the folder on the server (local folder is new).
    MkcolRemote {
        /// Space-relative folder path.
        path: String,
    },
    /// Create the folder locally (remote folder is new).
    MkcolLocal {
        /// Space-relative folder path.
        path: String,
    },
    /// Upload a file. `base_etag` is the recorded remote ETag for
    /// `If-Match`; `create` marks a new file (`If-None-Match: *`).
    Upload {
        /// Space-relative file path.
        path: String,
        /// Recorded remote ETag to guard against concurrent changes.
        base_etag: Option<String>,
        /// Whether the file is new on the server side.
        create: bool,
    },
    /// Download a file.
    Download {
        /// Space-relative file path.
        path: String,
        /// The remote node (ETag/size) as seen by the scan.
        remote: RemoteNode,
    },
    /// Delete a file or folder on the server (local deletion propagates).
    DeleteRemote {
        /// Space-relative path.
        path: String,
        /// Whether a folder is deleted (recursive on the server).
        is_dir: bool,
    },
    /// Delete a file or folder locally (remote deletion propagates).
    DeleteLocal {
        /// Space-relative path.
        path: String,
        /// Whether a folder is deleted (recursive locally).
        is_dir: bool,
    },
    /// Both sides changed (or the types differ): record a conflict and
    /// freeze the path. Phase 6 resolves it.
    Conflict {
        /// Space-relative path.
        path: String,
        /// Local state at detection (absent = deleted locally).
        local: Option<LocalNode>,
        /// Remote state at detection (absent = deleted remotely).
        remote: Option<RemoteNode>,
    },
    /// Both sides exist and match, but there is no DB entry yet (initial
    /// sync or first time seen): record a synced entry.
    RecordSynced {
        /// Space-relative path.
        path: String,
        /// Whether this is a folder.
        is_dir: bool,
        /// Local state, if the path exists locally.
        local: Option<LocalNode>,
        /// Remote state, if the path exists remotely.
        remote: Option<RemoteNode>,
    },
    /// The path vanished on both sides; drop the DB entry.
    DropEntry {
        /// Space-relative path.
        path: String,
    },
}

/// Builds the plan. `frozen` contains the paths of open conflicts (entry
/// state `conflict` or a pending conflict row); they are never touched.
/// Paths in sync produce no action.
pub fn build(
    local: &HashMap<String, LocalNode>,
    remote: &HashMap<String, RemoteNode>,
    db: &HashMap<String, NewEntry>,
    frozen: &HashSet<String>,
) -> Vec<SyncAction> {
    let mut paths: BTreeSet<&str> = BTreeSet::new();
    paths.extend(local.keys().map(String::as_str));
    paths.extend(remote.keys().map(String::as_str));
    paths.extend(db.keys().map(String::as_str));

    let mut actions = Vec::new();
    for path in paths {
        if is_frozen(path, frozen) {
            continue;
        }
        let l = local.get(path).copied();
        let r = remote.get(path);
        match db.get(path) {
            None => actions.extend(new_path_action(path, l, r)),
            Some(entry) if entry.is_dir => {
                actions.extend(dir_action(path, l.is_some(), r.is_some()))
            }
            Some(entry) => actions.extend(file_action(path, l, r, entry)),
        }
    }

    keep_folders_with_content(actions, frozen)
}

/// Whether `path` or one of its ancestor folders is frozen by an open
/// conflict — nothing below a conflicted folder is touched either.
fn is_frozen(path: &str, frozen: &HashSet<String>) -> bool {
    if frozen.is_empty() {
        return false;
    }
    let mut current = path;
    loop {
        if frozen.contains(current) {
            return true;
        }
        match current.rsplit_once('/') {
            Some((parent, _)) => current = parent,
            None => return false,
        }
    }
}

/// Classification for a path without a DB entry (new on one or both sides).
fn new_path_action(
    path: &str,
    local: Option<LocalNode>,
    remote: Option<&RemoteNode>,
) -> Option<SyncAction> {
    match (local, remote) {
        (Some(l), Some(r)) => {
            if l.is_dir != r.is_dir {
                // A folder here, a file there — cannot be synced.
                Some(SyncAction::Conflict {
                    path: path.to_owned(),
                    local: Some(l),
                    remote: Some(r.clone()),
                })
            } else if l.is_dir || l.size == r.size {
                // Initial sync: both sides agree (folders always; files by
                // size — a heuristic, since there is no baseline yet).
                Some(SyncAction::RecordSynced {
                    path: path.to_owned(),
                    is_dir: l.is_dir,
                    local: Some(l),
                    remote: Some(r.clone()),
                })
            } else {
                Some(SyncAction::Conflict {
                    path: path.to_owned(),
                    local: Some(l),
                    remote: Some(r.clone()),
                })
            }
        }
        (Some(l), None) => Some(if l.is_dir {
            SyncAction::MkcolRemote {
                path: path.to_owned(),
            }
        } else {
            SyncAction::Upload {
                path: path.to_owned(),
                base_etag: None,
                create: true,
            }
        }),
        (None, Some(r)) => Some(if r.is_dir {
            SyncAction::MkcolLocal {
                path: path.to_owned(),
            }
        } else {
            SyncAction::Download {
                path: path.to_owned(),
                remote: r.clone(),
            }
        }),
        (None, None) => None,
    }
}

/// Classification for a folder with a DB entry: existence only.
fn dir_action(path: &str, local: bool, remote: bool) -> Option<SyncAction> {
    match (local, remote) {
        (true, true) => None, // folder present on both sides
        (true, false) => Some(SyncAction::DeleteLocal {
            path: path.to_owned(),
            is_dir: true,
        }),
        (false, true) => Some(SyncAction::DeleteRemote {
            path: path.to_owned(),
            is_dir: true,
        }),
        (false, false) => Some(SyncAction::DropEntry {
            path: path.to_owned(),
        }),
    }
}

/// Classification for a file with a DB entry: the actual three-way logic.
fn file_action(
    path: &str,
    local: Option<LocalNode>,
    remote: Option<&RemoteNode>,
    entry: &NewEntry,
) -> Option<SyncAction> {
    let local_changed =
        local.is_some_and(|node| node.mtime != entry.local_mtime || node.size != entry.local_size);
    let remote_changed = remote.is_some_and(|node| remote_changed(node, entry));

    match (local, remote) {
        (Some(_), Some(r)) => {
            if local_changed && remote_changed {
                Some(SyncAction::Conflict {
                    path: path.to_owned(),
                    local,
                    remote: Some(r.clone()),
                })
            } else if local_changed {
                Some(SyncAction::Upload {
                    path: path.to_owned(),
                    base_etag: entry.remote_etag.clone(),
                    create: false,
                })
            } else if remote_changed {
                Some(SyncAction::Download {
                    path: path.to_owned(),
                    remote: r.clone(),
                })
            } else {
                None // unchanged on both sides
            }
        }
        (Some(l), None) => {
            if local_changed {
                // Local edits vs. remote deletion: a conflict.
                Some(SyncAction::Conflict {
                    path: path.to_owned(),
                    local: Some(l),
                    remote: None,
                })
            } else {
                Some(SyncAction::DeleteLocal {
                    path: path.to_owned(),
                    is_dir: false,
                })
            }
        }
        (None, Some(r)) => {
            if remote_changed {
                // Remote edits vs. local deletion: a conflict.
                Some(SyncAction::Conflict {
                    path: path.to_owned(),
                    local: None,
                    remote: Some(r.clone()),
                })
            } else {
                Some(SyncAction::DeleteRemote {
                    path: path.to_owned(),
                    is_dir: false,
                })
            }
        }
        (None, None) => Some(SyncAction::DropEntry {
            path: path.to_owned(),
        }),
    }
}

/// Whether the remote side of a tracked file changed since the last sync:
/// by ETag when the server provides one, else by size.
fn remote_changed(node: &RemoteNode, entry: &NewEntry) -> bool {
    match &node.etag {
        Some(etag) => entry.remote_etag.as_deref() != Some(etag.as_str()),
        None => node.size != entry.remote_size,
    }
}

/// Prevents a folder deletion from destroying content that must survive
/// it: conflicted paths (open or new), and new or changed files and
/// folders planned for transfer below the folder. Deleting the folder
/// first would remove them — the plan runs in path order, so `d` comes
/// before `d/new.txt`.
///
/// Instead of the deletion, the folder is re-created on the side where it
/// was deleted (`DeleteLocal` → `MkcolRemote`, `DeleteRemote` →
/// `MkcolLocal`), so the surviving children have a parent to land in.
/// Unchanged children still follow the deletion.
fn keep_folders_with_content(
    actions: Vec<SyncAction>,
    frozen: &HashSet<String>,
) -> Vec<SyncAction> {
    let mut protected: BTreeSet<String> = frozen.iter().cloned().collect();
    protected.extend(actions.iter().filter_map(|action| match action {
        SyncAction::Conflict { path, .. }
        | SyncAction::Upload { path, .. }
        | SyncAction::Download { path, .. }
        | SyncAction::MkcolRemote { path }
        | SyncAction::MkcolLocal { path } => Some(path.clone()),
        _ => None,
    }));
    let has_protected_child = |path: &str| {
        let prefix = format!("{path}/");
        protected
            .range(prefix.clone()..)
            .next()
            .is_some_and(|candidate| candidate.starts_with(&prefix))
    };
    actions
        .into_iter()
        .map(|action| match action {
            SyncAction::DeleteLocal { path, is_dir: true } if has_protected_child(&path) => {
                SyncAction::MkcolRemote { path }
            }
            SyncAction::DeleteRemote { path, is_dir: true } if has_protected_child(&path) => {
                SyncAction::MkcolLocal { path }
            }
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_db::EntryState;

    fn local(is_dir: bool, mtime: i64, size: i64) -> HashMap<String, LocalNode> {
        HashMap::from([(
            "a.txt".to_owned(),
            LocalNode {
                is_dir,
                mtime: Some(mtime),
                size: Some(size),
            },
        )])
    }

    fn remote(etag: &str, size: i64) -> HashMap<String, RemoteNode> {
        HashMap::from([(
            "a.txt".to_owned(),
            RemoteNode {
                is_dir: false,
                etag: Some(etag.to_owned()),
                size: Some(size),
            },
        )])
    }

    fn entry(etag: &str, mtime: i64, size: i64) -> HashMap<String, NewEntry> {
        HashMap::from([(
            "a.txt".to_owned(),
            NewEntry {
                path: "a.txt".to_owned(),
                is_dir: false,
                local_mtime: Some(mtime),
                local_size: Some(size),
                remote_etag: Some(etag.to_owned()),
                remote_size: Some(size),
                checksum: None,
                last_synced_at: None,
                state: EntryState::Synced,
            },
        )])
    }

    #[test]
    fn unchanged_file_needs_no_action() {
        let plan = build(
            &local(false, 100, 10),
            &remote("\"e1\"", 10),
            &entry("\"e1\"", 100, 10),
            &HashSet::new(),
        );
        assert!(plan.is_empty());
    }

    #[test]
    fn local_change_plans_upload_with_if_match() {
        let plan = build(
            &local(false, 200, 42),
            &remote("\"e1\"", 10),
            &entry("\"e1\"", 100, 10),
            &HashSet::new(),
        );
        assert_eq!(
            plan,
            vec![SyncAction::Upload {
                path: "a.txt".into(),
                base_etag: Some("\"e1\"".into()),
                create: false,
            }]
        );
    }

    #[test]
    fn remote_change_plans_download() {
        let plan = build(
            &local(false, 100, 10),
            &remote("\"e2\"", 10),
            &entry("\"e1\"", 100, 10),
            &HashSet::new(),
        );
        assert!(matches!(plan.as_slice(), [SyncAction::Download { .. }]));
    }

    #[test]
    fn both_changed_plans_conflict() {
        let plan = build(
            &local(false, 200, 42),
            &remote("\"e2\"", 10),
            &entry("\"e1\"", 100, 10),
            &HashSet::new(),
        );
        assert!(matches!(plan.as_slice(), [SyncAction::Conflict { .. }]));
    }

    #[test]
    fn deletions_propagate_and_drop() {
        // Local deletion: file gone locally, unchanged remotely.
        let plan = build(
            &HashMap::new(),
            &remote("\"e1\"", 10),
            &entry("\"e1\"", 100, 10),
            &HashSet::new(),
        );
        assert_eq!(
            plan,
            vec![SyncAction::DeleteRemote {
                path: "a.txt".into(),
                is_dir: false,
            }]
        );

        // Remote deletion: file gone remotely, unchanged locally.
        let plan = build(
            &local(false, 100, 10),
            &HashMap::new(),
            &entry("\"e1\"", 100, 10),
            &HashSet::new(),
        );
        assert_eq!(
            plan,
            vec![SyncAction::DeleteLocal {
                path: "a.txt".into(),
                is_dir: false,
            }]
        );

        // Both sides gone: entry is dropped.
        let plan = build(
            &HashMap::new(),
            &HashMap::new(),
            &entry("\"e1\"", 100, 10),
            &HashSet::new(),
        );
        assert_eq!(
            plan,
            vec![SyncAction::DropEntry {
                path: "a.txt".into()
            }]
        );
    }

    #[test]
    fn change_against_deletion_is_a_conflict() {
        let plan = build(
            &local(false, 200, 42),
            &HashMap::new(),
            &entry("\"e1\"", 100, 10),
            &HashSet::new(),
        );
        assert!(matches!(plan.as_slice(), [SyncAction::Conflict { .. }]));

        let plan = build(
            &HashMap::new(),
            &remote("\"e2\"", 12),
            &entry("\"e1\"", 100, 10),
            &HashSet::new(),
        );
        assert!(matches!(plan.as_slice(), [SyncAction::Conflict { .. }]));
    }

    #[test]
    fn frozen_paths_are_skipped() {
        let frozen = HashSet::from(["a.txt".to_owned()]);
        let plan = build(
            &local(false, 200, 42),
            &remote("\"e2\"", 10),
            &entry("\"e1\"", 100, 10),
            &frozen,
        );
        assert!(plan.is_empty());
    }

    #[test]
    fn folders_are_only_compared_by_existence() {
        let ldir = HashMap::from([(
            "d".to_owned(),
            LocalNode {
                is_dir: true,
                mtime: Some(1),
                size: None,
            },
        )]);
        let rdir = HashMap::from([(
            "d".to_owned(),
            RemoteNode {
                is_dir: true,
                etag: Some("\"dir-etag\"".to_owned()),
                size: None,
            },
        )]);
        let edir = HashMap::from([(
            "d".to_owned(),
            NewEntry {
                path: "d".to_owned(),
                is_dir: true,
                local_mtime: Some(1),
                local_size: None,
                remote_etag: Some("\"old\"".to_owned()),
                remote_size: None,
                checksum: None,
                last_synced_at: None,
                state: EntryState::Synced,
            },
        )]);

        // Both present: no action, even with differing dir ETags.
        assert!(build(&ldir, &rdir, &edir, &HashSet::new()).is_empty());
        // Remote gone: delete locally.
        let plan = build(&ldir, &HashMap::new(), &edir, &HashSet::new());
        assert_eq!(
            plan,
            vec![SyncAction::DeleteLocal {
                path: "d".into(),
                is_dir: true,
            }]
        );
        // Local gone: delete remotely.
        let plan = build(&HashMap::new(), &rdir, &edir, &HashSet::new());
        assert_eq!(
            plan,
            vec![SyncAction::DeleteRemote {
                path: "d".into(),
                is_dir: true,
            }]
        );
    }

    #[test]
    fn folders_with_conflicted_children_are_not_deleted() {
        let mut l = HashMap::new();
        l.insert(
            "d".to_owned(),
            LocalNode {
                is_dir: true,
                mtime: Some(1),
                size: None,
            },
        );
        l.insert(
            "d/x.txt".to_owned(),
            LocalNode {
                is_dir: false,
                mtime: Some(2),
                size: Some(3),
            },
        );
        let mut r = HashMap::new();
        let mut e = HashMap::new();
        // Remote folder "d" is gone; the file changed on both sides.
        r.insert(
            "d/x.txt".to_owned(),
            RemoteNode {
                is_dir: false,
                etag: Some("\"e2\"".to_owned()),
                size: Some(9),
            },
        );
        e.insert(
            "d".to_owned(),
            NewEntry {
                path: "d".to_owned(),
                is_dir: true,
                local_mtime: Some(1),
                local_size: None,
                remote_etag: None,
                remote_size: None,
                checksum: None,
                last_synced_at: None,
                state: EntryState::Synced,
            },
        );
        e.insert(
            "d/x.txt".to_owned(),
            NewEntry {
                path: "d/x.txt".to_owned(),
                is_dir: false,
                local_mtime: Some(1),
                local_size: Some(3),
                remote_etag: Some("\"e1\"".to_owned()),
                remote_size: Some(3),
                checksum: None,
                last_synced_at: None,
                state: EntryState::Synced,
            },
        );

        let plan = build(&l, &r, &e, &HashSet::new());
        // The folder delete is held back (its child is conflicted), the
        // file conflict is planned.
        assert!(
            !plan
                .iter()
                .any(|a| matches!(a, SyncAction::DeleteLocal { is_dir: true, .. }))
        );
        assert!(
            plan.iter()
                .any(|a| matches!(a, SyncAction::Conflict { path, .. } if path == "d/x.txt"))
        );
    }

    fn dir_entry(path: &str) -> NewEntry {
        NewEntry {
            path: path.to_owned(),
            is_dir: true,
            local_mtime: Some(1),
            local_size: None,
            remote_etag: None,
            remote_size: None,
            checksum: None,
            last_synced_at: None,
            state: EntryState::Synced,
        }
    }

    fn dir_node() -> LocalNode {
        LocalNode {
            is_dir: true,
            mtime: Some(1),
            size: None,
        }
    }

    #[test]
    fn remote_folder_deletion_keeps_new_local_children() {
        // Remote deleted "d"; locally a new file appeared inside it.
        let local = HashMap::from([
            ("d".to_owned(), dir_node()),
            (
                "d/new.txt".to_owned(),
                LocalNode {
                    is_dir: false,
                    mtime: Some(5),
                    size: Some(3),
                },
            ),
        ]);
        let db = HashMap::from([("d".to_owned(), dir_entry("d"))]);
        let plan = build(&local, &HashMap::new(), &db, &HashSet::new());
        assert_eq!(
            plan,
            vec![
                SyncAction::MkcolRemote { path: "d".into() },
                SyncAction::Upload {
                    path: "d/new.txt".into(),
                    base_etag: None,
                    create: true,
                },
            ]
        );
    }

    #[test]
    fn local_folder_deletion_keeps_new_remote_children() {
        // Locally deleted "d"; on the server a new file appeared inside it.
        let remote = HashMap::from([
            (
                "d".to_owned(),
                RemoteNode {
                    is_dir: true,
                    etag: Some("\"d\"".into()),
                    size: None,
                },
            ),
            (
                "d/new.txt".to_owned(),
                RemoteNode {
                    is_dir: false,
                    etag: Some("\"n\"".into()),
                    size: Some(3),
                },
            ),
        ]);
        let db = HashMap::from([("d".to_owned(), dir_entry("d"))]);
        let plan = build(&HashMap::new(), &remote, &db, &HashSet::new());
        assert!(matches!(
            plan.as_slice(),
            [SyncAction::MkcolLocal { path }, SyncAction::Download { .. }] if path == "d"
        ));
    }

    #[test]
    fn unchanged_children_follow_a_folder_deletion() {
        // Remote deleted "d" with an unchanged child: the deletion stands.
        let local = HashMap::from([
            ("d".to_owned(), dir_node()),
            (
                "d/old.txt".to_owned(),
                LocalNode {
                    is_dir: false,
                    mtime: Some(100),
                    size: Some(10),
                },
            ),
        ]);
        let mut old = entry("\"e1\"", 100, 10)
            .remove("a.txt")
            .expect("seeded entry");
        old.path = "d/old.txt".into();
        let db = HashMap::from([
            ("d".to_owned(), dir_entry("d")),
            ("d/old.txt".to_owned(), old),
        ]);
        let plan = build(&local, &HashMap::new(), &db, &HashSet::new());
        assert_eq!(
            plan,
            vec![
                SyncAction::DeleteLocal {
                    path: "d".into(),
                    is_dir: true,
                },
                SyncAction::DeleteLocal {
                    path: "d/old.txt".into(),
                    is_dir: false,
                },
            ]
        );
    }

    #[test]
    fn frozen_folders_freeze_their_subtree() {
        let local = HashMap::from([(
            "d/x.txt".to_owned(),
            LocalNode {
                is_dir: false,
                mtime: Some(5),
                size: Some(3),
            },
        )]);
        let frozen = HashSet::from(["d".to_owned()]);
        assert!(build(&local, &HashMap::new(), &HashMap::new(), &frozen).is_empty());
        // A sibling with the same prefix is not frozen.
        let sibling = HashMap::from([(
            "dd/x.txt".to_owned(),
            LocalNode {
                is_dir: false,
                mtime: Some(5),
                size: Some(3),
            },
        )]);
        assert_eq!(
            build(&sibling, &HashMap::new(), &HashMap::new(), &frozen).len(),
            1
        );
    }
}
