//! File transfers: downloads via `.part` temp files with an atomic rename
//! (large files in `Range` chunks), uploads with optimistic-write
//! conditions.

use std::path::Path;
use std::path::PathBuf;

use oc_api::{ApiError, Depth, TokenProvider, WebDavClient, WriteCondition};

use crate::error::SyncError;
use crate::scan::LocalNode;
use crate::scan::RemoteNode;
use crate::scan::abs_local_path;
use crate::scan::unix_mtime;

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
}

/// Downloads `path` into the local tree. Files with a known size above
/// `chunk_size` are fetched in `Range` chunks and appended to a
/// `<name>.part` file; smaller ones (or without a reported size) arrive
/// as a single GET. In both cases the data lands under the `.part` name
/// first and is atomically renamed once complete, so a crash never leaves
/// a half-written file under the real name.
pub async fn download<P: TokenProvider>(
    dav: &WebDavClient<P>,
    root: &Path,
    path: &str,
    remote: &RemoteNode,
    chunk_size: u64,
) -> Result<TransferResult, SyncError> {
    let target = abs_local_path(root, path)?;
    let size = remote
        .size
        .filter(|size| *size >= 0)
        .map(|size| size as u64);
    let (etag, local, remote_size) = match size {
        Some(size) if chunk_size > 0 && size > chunk_size => {
            let (etag, local) = chunked_download(dav, path, &target, size, chunk_size).await?;
            (etag, local, Some(size as i64))
        }
        _ => {
            let (etag, local) = whole_download(dav, root, path, remote, &target).await?;
            let remote_size = local.size;
            (etag, local, remote_size)
        }
    };
    Ok(TransferResult::Done(TransferMeta {
        local,
        remote_etag: etag.or_else(|| remote.etag.clone()),
        remote_size,
    }))
}

/// Single-request download of a file of unknown or small size.
async fn whole_download<P: TokenProvider>(
    dav: &WebDavClient<P>,
    root: &Path,
    path: &str,
    remote: &RemoteNode,
    target: &Path,
) -> Result<(Option<String>, LocalNode), SyncError> {
    let response = dav.get(path).await?;
    let etag = etag_of(&response).or_else(|| remote.etag.clone());
    let bytes = response.bytes().await.map_err(ApiError::from)?;
    let target = target.to_path_buf();
    let local = tokio::task::spawn_blocking(move || write_atomic(&target, &bytes))
        .await
        .map_err(|source| SyncError::io(root, join_error(source)))??;
    Ok((etag, local))
}

/// Chunked download: repeated `Range` GETs, each appended to the `.part`
/// file, until the reported size is reached. A server that ignores the
/// `Range` header (HTTP 200 with the whole body) is handled gracefully.
async fn chunked_download<P: TokenProvider>(
    dav: &WebDavClient<P>,
    path: &str,
    target: &Path,
    size: u64,
    chunk_size: u64,
) -> Result<(Option<String>, LocalNode), SyncError> {
    let part = part_file(target);
    // A fresh part file per attempt; stale ones are cleaned at run start.
    let stale = part.clone();
    if let Err(source) = tokio::task::spawn_blocking(move || std::fs::remove_file(&stale)).await {
        return Err(SyncError::io(target, join_error(source)));
    }

    let mut etag = None;
    let mut offset = 0u64;
    while offset < size {
        let end = (offset + chunk_size - 1).min(size - 1);
        let response = dav.get_range(path, offset, end).await?;
        if etag.is_none() {
            etag = etag_of(&response);
        }
        let status = response.status();
        let bytes = response.bytes().await.map_err(ApiError::from)?;
        if status == reqwest::StatusCode::OK {
            // The server ignored the range and sent the whole body.
            let whole_target = target.to_path_buf();
            let local = tokio::task::spawn_blocking(move || write_atomic(&whole_target, &bytes))
                .await
                .map_err(|source| SyncError::io(target, join_error(source)))??;
            return Ok((etag, local));
        }
        if bytes.is_empty() {
            return Err(SyncError::Api(ApiError::protocol(format!(
                "empty range response for '{path}' at offset {offset}"
            ))));
        }
        let chunk_part = part.clone();
        let written = tokio::task::spawn_blocking(move || -> Result<u64, SyncError> {
            append_chunk(&chunk_part, &bytes)?;
            let length = std::fs::metadata(&chunk_part)
                .map_err(|source| SyncError::io(&chunk_part, source))?;
            Ok(length.len())
        })
        .await
        .map_err(|source| SyncError::io(target, join_error(source)))??;
        if written > size {
            return Err(SyncError::Api(ApiError::protocol(format!(
                "download of '{path}' delivered {written} bytes, expected {size}"
            ))));
        }
        offset = written;
    }

    let actual = std::fs::metadata(&part)
        .map(|meta| meta.len())
        .map_err(|source| SyncError::io(&part, source))?;
    if actual != size {
        return Err(SyncError::Api(ApiError::protocol(format!(
            "download of '{path}' yielded {actual} bytes, expected {size}"
        ))));
    }
    let (part, final_target) = (part.clone(), target.to_path_buf());
    let local = tokio::task::spawn_blocking(move || finish_atomic(&part, &final_target))
        .await
        .map_err(|source| SyncError::io(target, join_error(source)))??;
    Ok((etag, local))
}

/// Uploads `path` to the server. `base_etag` guards against concurrent
/// remote changes (`If-Match`); `create` marks a new file
/// (`If-None-Match: *`). Without an ETag in the PUT response the file is
/// PROPFINDed once to learn its new ETag.
pub async fn upload<P: TokenProvider>(
    dav: &WebDavClient<P>,
    root: &Path,
    path: &str,
    base_etag: Option<String>,
    create: bool,
) -> Result<TransferResult, SyncError> {
    let source = abs_local_path(root, path)?;
    let read = tokio::task::spawn_blocking(move || read_with_meta(&source))
        .await
        .map_err(|source| SyncError::io(root, join_error(source)))??;
    let (content, local) = read;

    let condition = if create {
        Some(WriteCondition::CreateNew)
    } else {
        base_etag.map(|etag| WriteCondition::IfMatch { etag })
    };
    match dav.put_with(path, content, condition).await {
        Err(ApiError::PreconditionFailed) => Ok(TransferResult::PreconditionFailed),
        Err(source) => Err(SyncError::Api(source)),
        Ok(etag) => {
            let etag = match etag {
                Some(etag) => Some(etag),
                None => dav
                    .propfind(path, Depth::Zero)
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
fn write_atomic(target: &Path, bytes: &[u8]) -> Result<LocalNode, SyncError> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SyncError::io(parent, source))?;
    }
    let part = part_file(target);
    std::fs::write(&part, bytes).map_err(|source| SyncError::io(&part, source))?;
    finish_atomic(&part, target)
}

/// Renames the completed `.part` file onto its target; read-only targets
/// are made writable first so replacing and later deleting work on all
/// platforms.
fn finish_atomic(part: &Path, target: &Path) -> Result<LocalNode, SyncError> {
    if target.exists() {
        make_writable(target)?;
    }
    std::fs::rename(part, target).map_err(|source| SyncError::io(part, source))?;
    file_node(target)
}

/// Appends one chunk to the `.part` file.
fn append_chunk(part: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(part)
        .map_err(|source| SyncError::io(part, source))?;
    file.write_all(bytes)
        .map_err(|source| SyncError::io(part, source))
}

/// Reads a file together with its metadata.
fn read_with_meta(path: &Path) -> Result<(Vec<u8>, LocalNode), SyncError> {
    let content = std::fs::read(path).map_err(|source| SyncError::io(path, source))?;
    let node = file_node(path)?;
    Ok((content, node))
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

        let node = write_atomic(&target, b"new content").expect("write");
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
    fn append_chunk_extends_the_part_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let part = dir.path().join("a.txt.part");
        append_chunk(&part, b"abc").expect("append");
        append_chunk(&part, b"de").expect("append");
        assert_eq!(std::fs::read(&part).expect("read"), b"abcde");
    }
}
