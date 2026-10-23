//! File transfers: downloads via `.part` temp files with an atomic rename
//! (large files in `Range` chunks), uploads with optimistic-write
//! conditions.

use std::path::Path;
use std::path::PathBuf;

use oc_api::{ApiError, Depth, TokenProvider, WebDavClient, WriteCondition};

use crate::error::SyncError;
use crate::progress::TransferHandle;
use crate::scan::LocalNode;
use crate::scan::RemoteNode;
use crate::scan::abs_local_path;
use crate::scan::unix_mtime;

/// Where a transfer reads or writes: the job's local root and the
/// normalized key of the file (its local location), plus the path that
/// addresses it on the server (see [`crate::scan::ServerNames`]).
#[derive(Debug, Clone, Copy)]
pub struct TransferPaths<'a> {
    /// The job's local root directory.
    pub root: &'a Path,
    /// The space-relative, normalized path (diff/database key).
    pub path: &'a str,
    /// The server spelling of `path`.
    pub remote_path: &'a str,
}

/// Metadata of a completed transfer, as recorded in the status database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferMeta {
    /// Local state after the transfer.
    pub local: LocalNode,
    /// Remote ETag after the transfer, if known.
    pub remote_etag: Option<String>,
    /// Remote size after the transfer.
    pub remote_size: Option<i64>,
}

/// Outcome of a transfer attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferResult {
    /// The transfer completed; carry the new metadata.
    Done(TransferMeta),
    /// The server rejected an optimistic write (`If-Match`/`If-None-Match`
    /// failed, HTTP 412): the remote side changed concurrently and the
    /// path must be treated as a conflict.
    PreconditionFailed,
    /// A download was not applied because the local file changed after
    /// the scan (see [`LocalGuard`]); the downloaded data is discarded and
    /// the next pass sees both changes and records a conflict.
    LocalChanged,
}

/// What the local target of a download must still look like when the
/// downloaded file is moved into place. Protects edits made between the
/// scan and the end of the download from being overwritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalGuard {
    /// Overwrite whatever is there (explicit "server wins" resolution).
    Any,
    /// The file did not exist at scan time; a file that appeared since
    /// is kept.
    Absent,
    /// The file must still have the scanned mtime and size.
    Unchanged(LocalNode),
}

impl LocalGuard {
    /// The guard for a download planned from a scan: absent or unchanged.
    pub fn from_scan(local: Option<LocalNode>) -> Self {
        match local {
            Some(node) => Self::Unchanged(node),
            None => Self::Absent,
        }
    }

    /// Whether the target's current state still allows the overwrite.
    fn permits(&self, target: &Path) -> Result<bool, SyncError> {
        let current = match std::fs::metadata(target) {
            Ok(metadata) => Some(metadata),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(SyncError::io(target, source)),
        };
        Ok(match (self, current) {
            (Self::Any, _) | (_, None) => true,
            (Self::Absent, Some(_)) => false,
            (Self::Unchanged(node), Some(metadata)) => {
                !metadata.is_dir()
                    && unix_mtime(&metadata) == node.mtime
                    && Some(metadata.len() as i64) == node.size
            }
        })
    }
}

/// Downloads `path` into the local tree. Files with a known size above
/// `chunk_size` are fetched in `Range` chunks and appended to a
/// `<name>.part` file; smaller ones (or without a reported size) arrive
/// as a single GET. In both cases the data lands under the `.part` name
/// first and is atomically renamed once complete, so a crash never leaves
/// a half-written file under the real name. Before the rename, `guard`
/// checks that the local file was not changed in the meantime; if it was,
/// the download is discarded ([`TransferResult::LocalChanged`]).
/// `progress` receives chunk-granular byte counts.
pub async fn download<P: TokenProvider>(
    dav: &WebDavClient<P>,
    paths: TransferPaths<'_>,
    remote: &RemoteNode,
    chunk_size: u64,
    guard: LocalGuard,
    progress: Option<&TransferHandle>,
) -> Result<TransferResult, SyncError> {
    let TransferPaths {
        root,
        path,
        remote_path,
    } = paths;
    let target = abs_local_path(root, path)?;
    let size = remote
        .size
        .filter(|size| *size >= 0)
        .map(|size| size as u64);
    let landed = match size {
        Some(size) if chunk_size > 0 && size > chunk_size => {
            chunked_download(dav, remote_path, &target, size, chunk_size, guard, progress)
                .await?
                .map(|(etag, local)| (etag, local, Some(size as i64)))
        }
        _ => whole_download(dav, root, remote_path, remote, &target, guard, progress)
            .await?
            .map(|(etag, local)| {
                let remote_size = local.size;
                (etag, local, remote_size)
            }),
    };
    let Some((etag, local, remote_size)) = landed else {
        return Ok(TransferResult::LocalChanged);
    };
    Ok(TransferResult::Done(TransferMeta {
        local,
        remote_etag: etag.or_else(|| remote.etag.clone()),
        remote_size,
    }))
}

/// What a completed download reports: its ETag and the new local state;
/// `None` when the guard kept the local file.
type Landed = Option<(Option<String>, LocalNode)>;

/// Single-request download of a file of unknown or small size.
async fn whole_download<P: TokenProvider>(
    dav: &WebDavClient<P>,
    root: &Path,
    path: &str,
    remote: &RemoteNode,
    target: &Path,
    guard: LocalGuard,
    progress: Option<&TransferHandle>,
) -> Result<Landed, SyncError> {
    let response = dav.get(path).await?;
    let etag = etag_of(&response).or_else(|| remote.etag.clone());
    let part = part_file(target);
    stream_to_part(response, &part, progress).await?;
    let (part, final_target) = (part, target.to_path_buf());
    let local = tokio::task::spawn_blocking(move || finish_atomic(&part, &final_target, guard))
        .await
        .map_err(|source| SyncError::io(root, join_error(source)))??;
    Ok(local.map(|local| (etag, local)))
}

/// Streams a response body into a fresh `.part` file (parent folders are
/// created) without holding it in memory; reports the received bytes to
/// `progress`. A failed transfer removes the incomplete part file.
async fn stream_to_part(
    mut response: reqwest::Response,
    part: &Path,
    progress: Option<&TransferHandle>,
) -> Result<u64, SyncError> {
    if let Some(parent) = part.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| SyncError::io(parent, source))?;
    }
    let result = async {
        let mut file = tokio::fs::File::create(part)
            .await
            .map_err(|source| SyncError::io(part, source))?;
        stream_body(&mut response, &mut file, part, 0, progress).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(part).await;
    }
    result
}

/// Streams a response body into `file` (open at `part`, already holding
/// `base` bytes), reporting `base + received` to `progress` after every
/// network chunk; flushes and returns the new length.
async fn stream_body(
    response: &mut reqwest::Response,
    file: &mut tokio::fs::File,
    part: &Path,
    base: u64,
    progress: Option<&TransferHandle>,
) -> Result<u64, SyncError> {
    use tokio::io::AsyncWriteExt;
    let mut length = base;
    while let Some(chunk) = response.chunk().await.map_err(ApiError::from)? {
        file.write_all(&chunk)
            .await
            .map_err(|source| SyncError::io(part, source))?;
        length += chunk.len() as u64;
        if let Some(progress) = progress {
            progress.report(length);
        }
    }
    file.flush()
        .await
        .map_err(|source| SyncError::io(part, source))?;
    Ok(length)
}

/// Chunked download: repeated `Range` GETs, each streamed onto the end of
/// the `.part` file, until the reported size is reached. A server that
/// ignores the `Range` header (HTTP 200 with the whole body) is handled
/// gracefully. Progress is reported per network chunk, not per range.
async fn chunked_download<P: TokenProvider>(
    dav: &WebDavClient<P>,
    path: &str,
    target: &Path,
    size: u64,
    chunk_size: u64,
    guard: LocalGuard,
    progress: Option<&TransferHandle>,
) -> Result<Landed, SyncError> {
    let part = part_file(target);
    // A fresh part file per attempt; stale ones are cleaned at run start.
    let stale = part.clone();
    if let Err(source) = tokio::task::spawn_blocking(move || std::fs::remove_file(&stale)).await {
        return Err(SyncError::io(target, join_error(source)));
    }

    // Every chunk must come from the same version of the file: the
    // ranges are requested with `If-Match` on the first chunk's ETag, and
    // each response's ETag is compared as well (for servers that ignore
    // `If-Match` on GET).
    let mut etag: Option<String> = None;
    let mut offset = 0u64;
    while offset < size {
        let end = (offset + chunk_size - 1).min(size - 1);
        let response = dav.get_range(path, offset, end, etag.as_deref()).await?;
        match (&etag, etag_of(&response)) {
            (None, first) => etag = first,
            (Some(expected), Some(current)) if *expected != current => {
                return Err(SyncError::Api(ApiError::protocol(format!(
                    "'{path}' changed on the server during the download; retrying next pass"
                ))));
            }
            _ => {}
        }
        if response.status() == reqwest::StatusCode::OK {
            // The server ignored the range and sends the whole body:
            // stream it into a fresh part file.
            stream_to_part(response, &part, progress).await?;
            let (part, whole_target) = (part.clone(), target.to_path_buf());
            let local =
                tokio::task::spawn_blocking(move || finish_atomic(&part, &whole_target, guard))
                    .await
                    .map_err(|source| SyncError::io(target, join_error(source)))??;
            return Ok(local.map(|local| (etag, local)));
        }
        // Opened per range (not held across the loop) so the HTTP-200
        // fallback above can start a fresh part file.
        let mut response = response;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&part)
            .await
            .map_err(|source| SyncError::io(&part, source))?;
        let written = stream_body(&mut response, &mut file, &part, offset, progress).await?;
        drop(file);
        if written == offset {
            return Err(SyncError::Api(ApiError::protocol(format!(
                "empty range response for '{path}' at offset {offset}"
            ))));
        }
        if written > size {
            return Err(SyncError::Api(ApiError::protocol(format!(
                "download of '{path}' delivered {written} bytes, expected {size}"
            ))));
        }
        offset = written;
    }

    let actual = tokio::fs::metadata(&part)
        .await
        .map(|meta| meta.len())
        .map_err(|source| SyncError::io(&part, source))?;
    if actual != size {
        return Err(SyncError::Api(ApiError::protocol(format!(
            "download of '{path}' yielded {actual} bytes, expected {size}"
        ))));
    }
    let (part, final_target) = (part.clone(), target.to_path_buf());
    let local = tokio::task::spawn_blocking(move || finish_atomic(&part, &final_target, guard))
        .await
        .map_err(|source| SyncError::io(target, join_error(source)))??;
    Ok(local.map(|local| (etag, local)))
}

/// Uploads `path` to the server. `base_etag` guards against concurrent
/// remote changes (`If-Match`); `create` marks a new file
/// (`If-None-Match: *`). Without an ETag in the PUT response the file is
/// PROPFINDed once to learn its new ETag. While the body streams,
/// `progress` receives byte-granular counts of the current attempt.
pub async fn upload<P: TokenProvider>(
    dav: &WebDavClient<P>,
    paths: TransferPaths<'_>,
    base_etag: Option<String>,
    create: bool,
    progress: Option<&TransferHandle>,
) -> Result<TransferResult, SyncError> {
    let TransferPaths {
        root,
        path,
        remote_path,
    } = paths;
    let source = abs_local_path(root, path)?;
    // The state recorded for the upload is the one before sending. If the
    // file changes while it streams, the next pass sees a local change
    // against this record and uploads again (with the new ETag).
    let local = file_node(&source)?;
    let length = local.size.unwrap_or(0).max(0) as u64;

    let condition = if create {
        Some(WriteCondition::CreateNew)
    } else {
        base_etag.map(|etag| WriteCondition::IfMatch { etag })
    };
    let reporting = progress.cloned();
    let response = dav
        .put_file(remote_path, &source, length, condition, move |bytes| {
            if let Some(handle) = &reporting {
                handle.report(bytes);
            }
        })
        .await;
    match response {
        Err(ApiError::PreconditionFailed) => Ok(TransferResult::PreconditionFailed),
        Err(source) => Err(SyncError::Api(source)),
        Ok(etag) => {
            let etag = match etag {
                Some(etag) => Some(etag),
                None => dav
                    .propfind(remote_path, Depth::Zero)
                    .await
                    .ok()
                    .and_then(|resources| {
                        resources.first().and_then(|resource| resource.etag.clone())
                    }),
            };
            Ok(TransferResult::Done(TransferMeta {
                local,
                remote_etag: etag,
                remote_size: local.size,
            }))
        }
    }
}

/// Writes `bytes` to `<target>.part`, then renames it atomically onto
/// `target` ([`finish_atomic`]).
#[cfg(test)]
fn write_atomic(
    target: &Path,
    bytes: &[u8],
    guard: LocalGuard,
) -> Result<Option<LocalNode>, SyncError> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SyncError::io(parent, source))?;
    }
    let part = part_file(target);
    std::fs::write(&part, bytes).map_err(|source| SyncError::io(&part, source))?;
    finish_atomic(&part, target, guard)
}

/// Renames the completed `.part` file onto its target; read-only targets
/// are made writable first so replacing and later deleting work on all
/// platforms. When `guard` reports a local change since the scan, the
/// part file is discarded instead and `None` is returned.
fn finish_atomic(
    part: &Path,
    target: &Path,
    guard: LocalGuard,
) -> Result<Option<LocalNode>, SyncError> {
    if !guard.permits(target)? {
        let _ = std::fs::remove_file(part);
        return Ok(None);
    }
    if target.exists() {
        make_writable(target)?;
    }
    std::fs::rename(part, target).map_err(|source| SyncError::io(part, source))?;
    file_node(target).map(Some)
}

/// Local metadata of a file as [`LocalNode`].
fn file_node(path: &Path) -> Result<LocalNode, SyncError> {
    let metadata = std::fs::metadata(path).map_err(|source| SyncError::io(path, source))?;
    Ok(LocalNode {
        is_dir: metadata.is_dir(),
        mtime: unix_mtime(&metadata),
        size: if metadata.is_dir() {
            None
        } else {
            Some(metadata.len() as i64)
        },
    })
}

/// The `.part` temp path of a target file.
fn part_file(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.push_str(".part");
    target.with_file_name(name)
}

/// The `ETag` header of a response, if the server reports one.
fn etag_of(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Clears the read-only flag if set: on Unix by setting the owner write
/// bit (only that bit, so the file does not become world-writable), on
/// Windows/macOS via the read-only attribute.
pub(crate) fn make_writable(path: &Path) -> Result<(), SyncError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(path).map_err(|source| SyncError::io(path, source))?;
        let mut permissions = metadata.permissions();
        if permissions.mode() & 0o200 == 0 {
            permissions.set_mode(permissions.mode() | 0o200);
            std::fs::set_permissions(path, permissions)
                .map_err(|source| SyncError::io(path, source))?;
        }
    }
    #[cfg(not(unix))]
    {
        let metadata = std::fs::metadata(path).map_err(|source| SyncError::io(path, source))?;
        let mut permissions = metadata.permissions();
        if permissions.readonly() {
            permissions.set_readonly(false);
            std::fs::set_permissions(path, permissions)
                .map_err(|source| SyncError::io(path, source))?;
        }
    }
    Ok(())
}

/// Joins a panicked or cancelled blocking task into an io error.
fn join_error(source: tokio::task::JoinError) -> std::io::Error {
    if source.is_panic() {
        let payload = source.into_panic();
        let message = payload
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "blocking task panicked".to_owned());
        std::io::Error::other(message)
    } else {
        std::io::Error::other(source.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_file_appends_suffix() {
        assert_eq!(
            part_file(Path::new("/tmp/root/a/b.txt")),
            PathBuf::from("/tmp/root/a/b.txt.part")
        );
    }

    #[test]
    fn write_atomic_replaces_readonly_targets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, b"old").expect("write");
        let mut permissions = std::fs::metadata(&target).expect("meta").permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&target, permissions).expect("readonly");

        let node = write_atomic(&target, b"new content", LocalGuard::Any)
            .expect("write")
            .expect("applied");
        assert_eq!(std::fs::read(&target).expect("read"), b"new content");
        assert_eq!(node.size, Some(11));
        assert!(!dir.path().join("a.txt.part").exists());

        make_writable(&target).expect("writable");
        assert!(
            !std::fs::metadata(&target)
                .expect("meta")
                .permissions()
                .readonly()
        );
    }

    #[test]
    fn guards_keep_local_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, b"scanned").expect("write");
        let scanned = file_node(&target).expect("node");

        // Unchanged since the scan: the download lands.
        let landed =
            write_atomic(&target, b"server", LocalGuard::Unchanged(scanned)).expect("write");
        assert!(landed.is_some());
        assert_eq!(std::fs::read(&target).expect("read"), b"server");

        // Edited since the scan (size differs): the edit is kept.
        std::fs::write(&target, b"edited locally").expect("write");
        let landed =
            write_atomic(&target, b"server", LocalGuard::Unchanged(scanned)).expect("write");
        assert!(landed.is_none());
        assert_eq!(std::fs::read(&target).expect("read"), b"edited locally");
        assert!(!dir.path().join("a.txt.part").exists());

        // Absent at scan time, created since: kept as well.
        let fresh = dir.path().join("b.txt");
        std::fs::write(&fresh, b"new local file").expect("write");
        assert!(
            write_atomic(&fresh, b"server", LocalGuard::Absent)
                .expect("write")
                .is_none()
        );
    }
}
