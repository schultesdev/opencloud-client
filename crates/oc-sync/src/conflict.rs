//! Conflict resolution (Phase 6, see `PLAN.md`): a detected conflict is
//! recorded and its path frozen; the actual resolution is an explicit
//! action — from the GUI (Phase 7), a `conflict_policy` setting
//! (`ask`/`keep_server`/`keep_local`/`conflict_copy`), or the CLI.
//!
//! Resolution semantics:
//!
//! - `resolve_server`: the server version wins. Its current content is
//!   downloaded over the local file (or, if the server side was deleted,
//!   the local file is removed) and both the entry and the conflict row
//!   are updated.
//! - `resolve_local`: the local version wins. The file is uploaded with
//!   `If-Match` set to the conflict's recorded ETag — when the server
//!   changed again in the meantime the upload fails with 412, the
//!   conflict stays pending with the updated remote state recorded,
//!   and nothing is lost. A locally deleted file propagates the
//!   deletion to the server instead.
//!
//! Folder/file mismatches have no automatic resolution; they surface as
//! [`SyncError::UnresolvableConflict`] and must be sorted out by hand.

use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use oc_api::{ApiError, Depth, TokenProvider, WebDavClient};
use oc_db::Conflict;
use oc_db::ConflictResolution;
use oc_db::ConflictStatus;
use oc_db::EntryState;
use oc_db::NewEntry;
use oc_db::StatusDb;

use crate::engine::db_lock;
use crate::engine::local_node_at;
use crate::engine::unix_now;
use crate::error::SyncError;
use crate::scan::LocalNode;
use crate::scan::RemoteNode;
use crate::scan::abs_local_path;
use crate::transfer::{self, TransferResult};

/// Outcome of applying a conflict resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// The resolution was applied; entry and conflict row are updated.
    Resolved,
    /// The remote side changed again during a keep-local upload
    /// (`If-Match` failed with 412): the conflict stays pending, now
    /// with the current remote state recorded, and must be resolved
    /// anew.
    RemoteChanged,
}

/// Resolves a pending conflict by row id. `Server` downloads and
/// overwrites the local file; `Local` uploads and overwrites the server
/// file, guarded by the conflict's recorded ETag.
pub async fn resolve<P: TokenProvider>(
    dav: &WebDavClient<P>,
    db: &Mutex<StatusDb>,
    local_root: &Path,
    conflict_id: i64,
    resolution: ConflictResolution,
    chunk_size: u64,
) -> Result<ResolveOutcome, SyncError> {
    let conflict = match db_lock(db).get_conflict(conflict_id)? {
        Some(conflict) if conflict.status == ConflictStatus::Pending => conflict,
        _ => return Err(SyncError::UnknownConflict { id: conflict_id }),
    };
    // Folder/file mismatches (a collection here, a file there) have no
    // meaningful automatic resolution.
    if let Some(entry) = db_lock(db).get_entry(&conflict.path)? {
        if entry.entry.is_dir {
            return Err(SyncError::UnresolvableConflict {
                path: conflict.path,
                reason: "one side is a folder; remove one side manually",
            });
        }
    }

    let outcome = match resolution {
        ConflictResolution::Server => {
            resolve_server(dav, db, local_root, &conflict, chunk_size).await
        }
        ConflictResolution::Local => resolve_local(dav, db, local_root, &conflict).await,
    }?;
    if outcome == ResolveOutcome::Resolved {
        let resolved = db_lock(db).resolve_conflict(conflict_id, resolution, unix_now())?;
        if !resolved {
            // Resolved concurrently (second client); the local outcome
            // still stands, but the row no longer accepts updates.
            return Err(SyncError::UnknownConflict { id: conflict_id });
        }
    }
    Ok(outcome)
}

/// Server wins: its current state overwrites the local side.
async fn resolve_server<P: TokenProvider>(
    dav: &WebDavClient<P>,
    db: &Mutex<StatusDb>,
    local_root: &Path,
    conflict: &Conflict,
    chunk_size: u64,
) -> Result<ResolveOutcome, SyncError> {
    let Some(etag) = conflict.remote_etag.clone() else {
        // The server side was deleted: accept the deletion locally.
        remove_local(local_root, &conflict.path)?;
        db_lock(db).delete_entry(&conflict.path)?;
        return Ok(ResolveOutcome::Resolved);
    };
    let remote = RemoteNode {
        is_dir: false,
        etag: Some(etag),
        size: conflict.remote_size,
    };
    match transfer::download(dav, local_root, &conflict.path, &remote, chunk_size, None).await? {
        TransferResult::Done(meta) => {
            record_resolved(
                db,
                &conflict.path,
                meta.local,
                meta.remote_etag,
                meta.remote_size,
            )?;
            Ok(ResolveOutcome::Resolved)
        }
        // Downloads carry no write condition; this cannot happen.
        TransferResult::PreconditionFailed => Err(SyncError::Api(ApiError::protocol(
            "download was rejected by a write condition",
        ))),
    }
}

/// Local wins: the local state overwrites the server side, guarded by
/// the conflict's recorded ETag.
async fn resolve_local<P: TokenProvider>(
    dav: &WebDavClient<P>,
    db: &Mutex<StatusDb>,
    local_root: &Path,
    conflict: &Conflict,
) -> Result<ResolveOutcome, SyncError> {
    if local_node_at(local_root, &conflict.path)?.is_none() {
        // The local side was deleted: keep the deletion, propagate it.
        match dav.delete(&conflict.path).await {
            Ok(()) | Err(ApiError::NotFound) => {}
            Err(error) => return Err(SyncError::Api(error)),
        }
        db_lock(db).delete_entry(&conflict.path)?;
        return Ok(ResolveOutcome::Resolved);
    }
    // `create` covers the "remote was deleted, local changed" case: the
    // upload re-creates the file; a server that got a new file in the
    // meantime rejects `If-None-Match` with 412 — handled below.
    let create = conflict.remote_etag.is_none();
    match transfer::upload(
        dav,
        local_root,
        &conflict.path,
        conflict.remote_etag.clone(),
        create,
        None,
    )
    .await?
    {
        TransferResult::Done(meta) => {
            record_resolved(
                db,
                &conflict.path,
                meta.local,
                meta.remote_etag,
                meta.remote_size,
            )?;
            Ok(ResolveOutcome::Resolved)
        }
        // The server changed again: record its current state on the
        // pending row; the user resolves anew with fresh information.
        TransferResult::PreconditionFailed => {
            let remote = current_remote(dav, &conflict.path).await;
            let updated = db_lock(db).update_conflict_remote(
                conflict.id,
                remote.as_ref().and_then(|node| node.etag.as_deref()),
                remote.as_ref().and_then(|node| node.size),
                None,
            )?;
            if !updated {
                return Err(SyncError::UnknownConflict { id: conflict.id });
            }
            Ok(ResolveOutcome::RemoteChanged)
        }
    }
}

/// Records the state after a successful resolution: the entry is back
/// in sync, the conflict row is resolved by the caller.
fn record_resolved(
    db: &Mutex<StatusDb>,
    path: &str,
    local: LocalNode,
    remote_etag: Option<String>,
    remote_size: Option<i64>,
) -> Result<(), SyncError> {
    db_lock(db).upsert_entry(&NewEntry {
        path: path.to_owned(),
        is_dir: false,
        local_mtime: local.mtime,
        local_size: local.size,
        remote_etag,
        remote_size,
        checksum: None,
        last_synced_at: Some(unix_now()),
        state: EntryState::Synced,
    })?;
    Ok(())
}

/// The current remote state of a path (`None` when it no longer exists),
/// e.g. after a failed optimistic write.
async fn current_remote<P: TokenProvider>(dav: &WebDavClient<P>, path: &str) -> Option<RemoteNode> {
    let resources = dav.propfind(path, Depth::Zero).await.ok()?;
    resources.first().map(RemoteNode::from_resource)
}

/// Removes the local file of a resolved conflict (read-only attributes
/// are cleared first); a missing file is fine.
fn remove_local(root: &Path, rel: &str) -> Result<(), SyncError> {
    let target = abs_local_path(root, rel)?;
    match std::fs::metadata(&target) {
        Ok(_) => {
            transfer::make_writable(&target)?;
            std::fs::remove_file(&target).map_err(|source| SyncError::io(&target, source))
        }
        Err(source) if source.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SyncError::io(&target, source)),
    }
}

/// Saves the current local file next to the original as a dated
/// conflicted copy — the `conflict_copy` policy's way of keeping the
/// local version before letting the server version win. The copy is
/// picked up by the next sync pass as a new file and uploaded.
pub fn keep_conflict_copy(root: &Path, rel: &str) -> Result<PathBuf, SyncError> {
    keep_conflict_copy_at(root, rel, SystemTime::now())
}

/// Like [`keep_conflict_copy`], with the timestamp of the copy name
/// supplied by the caller (tests pin it).
fn keep_conflict_copy_at(root: &Path, rel: &str, at: SystemTime) -> Result<PathBuf, SyncError> {
    let source = abs_local_path(root, rel)?;
    let content = std::fs::read(&source).map_err(|error| SyncError::io(&source, error))?;
    let base = conflict_copy_name(rel, at);
    // 0 keeps the plain dated name, then a counter from 2 on.
    for number in std::iter::once(0).chain(2..) {
        let candidate_rel = if number == 0 {
            base.clone()
        } else {
            match base.rsplit_once('.') {
                Some((stem, ext)) => format!("{stem} ({number}).{ext}"),
                None => format!("{base} ({number})"),
            }
        };
        let candidate = abs_local_path(root, &candidate_rel)?;
        if candidate == source {
            continue;
        }
        if !candidate.exists() {
            std::fs::write(&candidate, &content)
                .map_err(|error| SyncError::io(&candidate, error))?;
            return Ok(candidate);
        }
    }
    Err(SyncError::io(
        &source,
        std::io::Error::new(ErrorKind::AlreadyExists, "no free conflicted-copy name"),
    ))
}

/// The space-relative name of a conflicted copy:
/// `name (conflicted copy YYYY-MM-DD HH-MM-SS).ext`.
pub fn conflict_copy_name(rel: &str, at: SystemTime) -> String {
    let (dir, file) = match rel.rsplit_once('/') {
        Some((dir, file)) => (format!("{dir}/"), file),
        None => (String::new(), rel),
    };
    let stamp = local_timestamp(at);
    match file.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.contains('/') => {
            format!("{dir}{stem} (conflicted copy {stamp}).{ext}")
        }
        _ => format!("{dir}{file} (conflicted copy {stamp})"),
    }
}

/// `YYYY-MM-DD HH-MM-SS` of a timestamp (UTC).
fn local_timestamp(at: SystemTime) -> String {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}-{minute:02}-{second:02}")
}

/// Days since the Unix epoch to a civil date (proleptic Gregorian,
/// Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflicted_copy_names_carry_a_timestamp() {
        let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_704_067_200);
        assert_eq!(
            conflict_copy_name("bericht.txt", at),
            "bericht (conflicted copy 2024-01-01 00-00-00).txt"
        );
        assert_eq!(
            conflict_copy_name("README", at),
            "README (conflicted copy 2024-01-01 00-00-00)"
        );
        assert_eq!(
            conflict_copy_name("Ordner/datei.v2.bin", at),
            "Ordner/datei.v2 (conflicted copy 2024-01-01 00-00-00).bin"
        );
    }

    #[test]
    fn conflicted_copies_avoid_existing_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(root.join("a.txt"), "local").expect("write");
        let at = SystemTime::UNIX_EPOCH;

        let first = keep_conflict_copy_at(&root, "a.txt", at).expect("copy");
        assert_eq!(
            first
                .file_name()
                .expect("name")
                .to_string_lossy()
                .to_string(),
            "a (conflicted copy 1970-01-01 00-00-00).txt"
        );

        // Same second, same name: the next copy gets a counter.
        let second = keep_conflict_copy_at(&root, "a.txt", at).expect("copy");
        assert_eq!(
            second
                .file_name()
                .expect("name")
                .to_string_lossy()
                .to_string(),
            "a (conflicted copy 1970-01-01 00-00-00) (2).txt"
        );
        assert_eq!(std::fs::read_to_string(&second).expect("read"), "local");
        // The original stays untouched.
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("read"),
            "local"
        );
    }
}
