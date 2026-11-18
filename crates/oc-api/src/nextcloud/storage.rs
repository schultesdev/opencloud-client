//! The file tree of one Nextcloud user (`/remote.php/dav/files/{user}/`)
//! as [`RemoteStorage`]. Everything but large uploads is plain WebDAV
//! and shared with OpenCloud; files from [`ChunkingConfig::threshold`] on
//! use Nextcloud's chunked upload v2:
//!
//! 1. `MKCOL /remote.php/dav/uploads/{user}/{id}` with `Destination`,
//! 2. `PUT …/{id}/{n}` per chunk (`n` from 1) with `Destination` and
//!    `OC-Total-Length`,
//! 3. `MOVE …/{id}/.file` to the destination, which assembles the file;
//!    write conditions travel here (`If: <dest> (["etag"])` instead of
//!    `If-Match`, `Overwrite: F` instead of `If-None-Match: *`).
//!
//! Failed uploads remove their upload folder (best effort; the server
//! also expires stale ones).

use std::path::Path;
use std::time::{Duration, SystemTime};

use futures_util::FutureExt;
use oc_storage::{
    BoxFuture, ByteRange, Download, Listing, ProgressFn, RemoteEntry, RemoteStorage, StorageError,
    StorageResult, WriteCondition,
};
use url::Url;

use super::ocs::below;
use crate::dav::{WebDavClient, etag_header, file_body, watched_progress, webdav_method};
use crate::error::ApiError;
use crate::http::{Activity, Replay, parse_server_url};
use crate::path::{build_url, encode_segment};
use crate::retry::RetryConfig;
use crate::token::TokenProvider;

/// When and how large files are uploaded in chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkingConfig {
    /// Files of at least this size are uploaded in chunks.
    pub threshold: u64,
    /// Size of one chunk (the last one may be smaller). Grows when the
    /// file would otherwise need more than [`MAX_CHUNKS`] chunks.
    pub chunk_size: u64,
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        Self {
            threshold: 50 * 1024 * 1024,
            chunk_size: 25 * 1024 * 1024,
        }
    }
}

/// Largest number of chunks Nextcloud accepts for one upload.
pub const MAX_CHUNKS: u64 = 10_000;

/// How long the final `MOVE` may take: the server assembles the chunks
/// before it answers, which takes a while for large files.
const ASSEMBLY_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// One Nextcloud user's file tree.
#[derive(Debug)]
pub struct NextcloudStorage<P: TokenProvider> {
    dav: WebDavClient<P>,
    uploads: Url,
    chunking: ChunkingConfig,
}

impl<P: TokenProvider> NextcloudStorage<P> {
    /// Creates the storage of user `user_id` (the id from the OCS user
    /// endpoint, not necessarily the login name) at `server_url`.
    pub fn new(server_url: &str, user_id: &str, token: P) -> Result<Self, ApiError> {
        let server = parse_server_url(server_url)?;
        let user = encode_segment(user_id);
        let files = below(&server, &format!("remote.php/dav/files/{user}/"))?;
        let uploads = below(&server, &format!("remote.php/dav/uploads/{user}/"))?;
        Ok(Self {
            dav: WebDavClient::from_webdav_url(files.as_str(), token)?,
            uploads,
            chunking: ChunkingConfig::default(),
        })
    }

    /// Overrides the retry policy.
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.dav = self.dav.with_retry(retry);
        self
    }

    /// Overrides the metadata-request and transfer timeouts.
    pub fn with_timeouts(mut self, request: Duration, transfer: Duration) -> Self {
        self.dav = self.dav.with_timeouts(request, transfer);
        self
    }

    /// Overrides when and how files are uploaded in chunks.
    pub fn with_chunking(mut self, chunking: ChunkingConfig) -> Self {
        self.chunking = chunking;
        self
    }

    /// The plain WebDAV client of the file tree.
    pub fn webdav(&self) -> &WebDavClient<P> {
        &self.dav
    }

    /// Chunked upload; see the module documentation.
    async fn upload_chunked(
        &self,
        path: &str,
        source: &Path,
        length: u64,
        condition: Option<WriteCondition>,
        progress: ProgressFn,
    ) -> StorageResult<Option<String>> {
        // Check the condition first: a doomed upload of gigabytes would
        // only fail at the final MOVE.
        let before = self.dav.stat_entry(path).await?;
        match (&condition, &before) {
            (Some(WriteCondition::IfMatch { .. }), None)
            | (Some(WriteCondition::CreateNew), Some(_)) => {
                return Err(StorageError::PreconditionFailed);
            }
            (Some(WriteCondition::IfMatch { etag }), Some(entry))
                if entry.etag.as_ref().is_some_and(|current| current != etag) =>
            {
                return Err(StorageError::PreconditionFailed);
            }
            _ => {}
        }
        let local_before = local_state(source)?;
        let destination = build_url(self.dav.base_url(), path)?;
        let upload_id = format!("oc-sync-{:016x}", rand::random::<u64>());
        let folder = self
            .uploads
            .join(&format!("{upload_id}/"))
            .map_err(|err| ApiError::invalid_url(self.uploads.as_str(), err.to_string()))?;

        let result = async {
            self.create_upload_folder(&folder, &destination).await?;
            self.send_chunks(&folder, &destination, source, length, &progress)
                .await?;
            if local_state(source)? != local_before {
                return Err(StorageError::protocol(format!(
                    "'{path}' changed while it was uploaded; retrying next pass"
                )));
            }
            self.assemble(&folder, &destination, path, length, &condition, &before)
                .await
        }
        .await;
        if result.is_err() {
            let _ = self.delete_upload_folder(&folder).await;
        }
        result
    }

    /// `MKCOL` of the upload folder; a repeated request finding it
    /// already created is fine.
    async fn create_upload_folder(&self, folder: &Url, destination: &Url) -> StorageResult<()> {
        let http = self.dav.http();
        let mkcol = webdav_method(b"MKCOL")?;
        match http
            .send(Some(http.request_timeout()), Replay::Safe, || {
                http.http()
                    .request(mkcol.clone(), folder.clone())
                    .header("Destination", destination.as_str())
            })
            .await
        {
            Ok(_) | Err(ApiError::Status { code: 405 }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Uploads the chunks, numbered from 1, streaming each from disk.
    async fn send_chunks(
        &self,
        folder: &Url,
        destination: &Url,
        source: &Path,
        length: u64,
        progress: &ProgressFn,
    ) -> StorageResult<()> {
        let http = self.dav.http();
        let chunk_size = self
            .chunking
            .chunk_size
            .max(length.div_ceil(MAX_CHUNKS))
            .max(1);
        let mut offset = 0u64;
        let mut number = 1u64;
        while offset < length {
            let size = chunk_size.min(length - offset);
            let url = folder
                .join(&number.to_string())
                .map_err(|err| ApiError::invalid_url(folder.as_str(), err.to_string()))?;
            let activity = Activity::new();
            let reporting = progress.clone();
            let chunk_progress =
                watched_progress(&activity, size, move |sent| reporting(offset + sent));
            http.send_upload(&activity, Replay::Safe, || {
                http.upload_http()
                    .put(url.clone())
                    .header("Destination", destination.as_str())
                    .header("OC-Total-Length", length)
                    .header(reqwest::header::CONTENT_LENGTH, size)
                    .body(file_body(source, offset, size, chunk_progress.clone()))
            })
            .await?;
            offset += size;
            number += 1;
        }
        Ok(())
    }

    /// The final `MOVE …/.file`: the server assembles the file at the
    /// destination, guarded by the write condition.
    async fn assemble(
        &self,
        folder: &Url,
        destination: &Url,
        path: &str,
        length: u64,
        condition: &Option<WriteCondition>,
        before: &Option<RemoteEntry>,
    ) -> StorageResult<Option<String>> {
        let http = self.dav.http();
        let source = folder
            .join(".file")
            .map_err(|err| ApiError::invalid_url(folder.as_str(), err.to_string()))?;
        let method = webdav_method(b"MOVE")?;
        let timeout = ASSEMBLY_TIMEOUT.max(http.request_timeout());
        let result = http
            .send(Some(timeout), Replay::OnlyUnprocessed, || {
                let request = http
                    .http()
                    .request(method.clone(), source.clone())
                    .header("Destination", destination.as_str())
                    .header("OC-Total-Length", length);
                match condition {
                    // `If-Match` would apply to the source (`.file`); the
                    // tagged `If` header checks the destination.
                    Some(WriteCondition::IfMatch { etag }) => request
                        .header("If", format!("<{}> ([{etag}])", destination.as_str()))
                        .header("Overwrite", "T"),
                    Some(WriteCondition::CreateNew) => request.header("Overwrite", "F"),
                    None => request.header("Overwrite", "T"),
                }
            })
            .await;
        match result {
            Ok(response) => Ok(response
                .headers()
                .get("OC-ETag")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
                .or_else(|| etag_header(&response))),
            Err(ApiError::PreconditionFailed) => Err(StorageError::PreconditionFailed),
            // The destination vanished since the check: the same as a
            // failed `If-Match` on a plain PUT.
            Err(ApiError::NotFound)
                if matches!(condition, Some(WriteCondition::IfMatch { .. })) =>
            {
                match self.dav.stat_entry(path).await {
                    Ok(None) => Err(StorageError::PreconditionFailed),
                    _ => Err(StorageError::NotFound),
                }
            }
            // No answer: the server may still have assembled the file.
            Err(error @ (ApiError::Network(_) | ApiError::Server { .. })) => {
                match self.dav.stat_entry(path).await {
                    Ok(Some(entry))
                        if entry.size == Some(length)
                            && entry.etag.is_some()
                            && entry.etag != before.as_ref().and_then(|b| b.etag.clone()) =>
                    {
                        Ok(entry.etag)
                    }
                    _ => Err(error.into()),
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Removes an upload folder (best effort).
    async fn delete_upload_folder(&self, folder: &Url) -> Result<(), ApiError> {
        let http = self.dav.http();
        http.send(Some(http.request_timeout()), Replay::Safe, || {
            http.http().delete(folder.clone())
        })
        .await
        .map(|_| ())
    }
}

/// Size and modification time of the local source, to detect changes
/// while its chunks are uploaded.
fn local_state(source: &Path) -> StorageResult<(u64, Option<SystemTime>)> {
    let metadata = std::fs::metadata(source).map_err(|err| {
        StorageError::protocol(format!("cannot read '{}': {err}", source.display()))
    })?;
    Ok((metadata.len(), metadata.modified().ok()))
}

impl<P: TokenProvider> RemoteStorage for NextcloudStorage<P> {
    fn stat<'a>(&'a self, path: &'a str) -> BoxFuture<'a, StorageResult<Option<RemoteEntry>>> {
        RemoteStorage::stat(&self.dav, path)
    }

    fn list<'a>(&'a self, path: &'a str) -> BoxFuture<'a, StorageResult<Listing>> {
        RemoteStorage::list(&self.dav, path)
    }

    fn download<'a>(
        &'a self,
        path: &'a str,
        range: Option<ByteRange>,
        if_match: Option<&'a str>,
    ) -> BoxFuture<'a, StorageResult<Download>> {
        RemoteStorage::download(&self.dav, path, range, if_match)
    }

    fn upload<'a>(
        &'a self,
        path: &'a str,
        source: &'a Path,
        length: u64,
        condition: Option<WriteCondition>,
        progress: ProgressFn,
    ) -> BoxFuture<'a, StorageResult<Option<String>>> {
        if length >= self.chunking.threshold {
            self.upload_chunked(path, source, length, condition, progress)
                .boxed()
        } else {
            self.dav
                .upload_single(path, source, length, condition, progress)
                .boxed()
        }
    }

    fn create_dir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, StorageResult<()>> {
        RemoteStorage::create_dir(&self.dav, path)
    }

    fn delete<'a>(&'a self, path: &'a str) -> BoxFuture<'a, StorageResult<()>> {
        RemoteStorage::delete(&self.dav, path)
    }
}
