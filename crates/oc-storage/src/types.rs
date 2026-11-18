//! Data types exchanged between the sync engine and storage providers.

use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use unicode_normalization::UnicodeNormalization;

use crate::error::StorageResult;

/// NFC-normalizes text (file and folder names from local and remote
/// sources, so both sides compare equal regardless of decomposition).
pub fn normalize_nfc(input: &str) -> String {
    input.nfc().collect()
}

/// Receives the cumulative number of bytes sent during an upload.
pub type ProgressFn = Arc<dyn Fn(u64) + Send + Sync>;

/// One remote file or folder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteEntry {
    /// Path relative to the storage root (`""` is the root), NFC-normalized,
    /// without trailing slash — the comparison key.
    pub path: String,
    /// The same path as the server spells it (not normalized). Use it to
    /// address the resource in requests.
    pub server_path: String,
    /// Last path segment; empty for the root.
    pub name: String,
    /// Whether the entry is a folder.
    pub is_dir: bool,
    /// Size in bytes (files only, if reported).
    pub size: Option<u64>,
    /// ETag in quoted form, as far as the server provides one.
    pub etag: Option<String>,
    /// The server's SHA1 of the content (lowercase hex), if reported.
    pub sha1: Option<String>,
}

/// A folder and its direct children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The listed folder itself.
    pub dir: RemoteEntry,
    /// Its direct children.
    pub children: Vec<RemoteEntry>,
}

/// Optimistic-write condition for uploads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteCondition {
    /// Only write when the remote ETag still matches the recorded one; a
    /// mismatch yields [`crate::StorageError::PreconditionFailed`].
    IfMatch {
        /// The recorded remote ETag (quoted form, e.g. `"abc"`).
        etag: String,
    },
    /// Only create the file when it does not yet exist remotely.
    CreateNew,
}

/// An inclusive byte range of a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// First byte.
    pub start: u64,
    /// Last byte (inclusive).
    pub end: u64,
}

/// A running download: metadata plus a streamed body.
pub struct Download {
    /// ETag of the served version, if reported.
    pub etag: Option<String>,
    /// `true` when only the requested range is served; `false` when the
    /// server sent the whole file (also when it ignored a range request).
    pub partial: bool,
    body: BoxStream<'static, StorageResult<Bytes>>,
}

impl Download {
    /// Creates a download from its metadata and body stream.
    pub fn new(
        etag: Option<String>,
        partial: bool,
        body: BoxStream<'static, StorageResult<Bytes>>,
    ) -> Self {
        Self {
            etag,
            partial,
            body,
        }
    }

    /// The next chunk of the body, or `None` at its end.
    pub async fn chunk(&mut self) -> StorageResult<Option<Bytes>> {
        self.body.next().await.transpose()
    }
}

impl fmt::Debug for Download {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Download")
            .field("etag", &self.etag)
            .field("partial", &self.partial)
            .finish_non_exhaustive()
    }
}

/// Storage usage of a root.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StorageQuota {
    /// Used bytes, if known.
    pub used: Option<u64>,
    /// Total bytes, if limited.
    pub total: Option<u64>,
}

/// A syncable root of an account: an OpenCloud space or a Nextcloud
/// user's file tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRoot {
    /// Identifier stored in the sync job (`space_id`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Kind shown as a chip (`personal`, `project`, `files`, ...).
    pub kind: String,
    /// Usage, if reported.
    pub quota: Option<StorageQuota>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StorageError;

    #[test]
    fn nfc_normalization() {
        assert_eq!(normalize_nfc("Cafe\u{301}"), "Caf\u{e9}");
        assert_eq!(normalize_nfc("already nfc"), "already nfc");
    }

    #[tokio::test]
    async fn download_yields_chunks_then_end() {
        let chunks: Vec<StorageResult<Bytes>> =
            vec![Ok(Bytes::from_static(b"ab")), Ok(Bytes::from_static(b"c"))];
        let mut download = Download::new(
            Some("\"e\"".into()),
            false,
            futures_util::stream::iter(chunks).boxed(),
        );
        assert_eq!(download.chunk().await, Ok(Some(Bytes::from_static(b"ab"))));
        assert_eq!(download.chunk().await, Ok(Some(Bytes::from_static(b"c"))));
        assert_eq!(download.chunk().await, Ok(None));
    }

    #[tokio::test]
    async fn download_surfaces_body_errors() {
        let chunks: Vec<StorageResult<Bytes>> = vec![Err(StorageError::protocol("broken"))];
        let mut download = Download::new(None, true, futures_util::stream::iter(chunks).boxed());
        assert_eq!(
            download.chunk().await,
            Err(StorageError::protocol("broken"))
        );
    }
}
