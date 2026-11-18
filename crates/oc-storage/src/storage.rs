//! The storage traits.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;

use crate::error::StorageResult;
use crate::types::{
    ByteRange, Download, Listing, ProgressFn, RemoteEntry, StorageRoot, WriteCondition,
};

/// One remote file tree, addressed by `/`-separated paths relative to its
/// root (`""` is the root). Paths passed in should use the server's
/// spelling ([`RemoteEntry::server_path`]).
///
/// Implementations handle retries, authentication and provider specifics
/// (e.g. chunked uploads) internally.
pub trait RemoteStorage: Send + Sync {
    /// Metadata of one entry; `None` when it does not exist.
    fn stat<'a>(&'a self, path: &'a str) -> BoxFuture<'a, StorageResult<Option<RemoteEntry>>>;

    /// A folder and its direct children.
    fn list<'a>(&'a self, path: &'a str) -> BoxFuture<'a, StorageResult<Listing>>;

    /// Downloads a file, or the given range of it. With `if_match`, the
    /// file is only served while it still has that ETag; otherwise the
    /// call fails with [`crate::StorageError::PreconditionFailed`].
    fn download<'a>(
        &'a self,
        path: &'a str,
        range: Option<ByteRange>,
        if_match: Option<&'a str>,
    ) -> BoxFuture<'a, StorageResult<Download>>;

    /// Uploads exactly `length` bytes of the local file `source`, streamed
    /// from disk. `progress` receives the cumulative byte count. Returns
    /// the new remote ETag, if the server reports one.
    fn upload<'a>(
        &'a self,
        path: &'a str,
        source: &'a Path,
        length: u64,
        condition: Option<WriteCondition>,
        progress: ProgressFn,
    ) -> BoxFuture<'a, StorageResult<Option<String>>>;

    /// Creates a folder (the parent must exist); an existing folder counts
    /// as success.
    fn create_dir<'a>(&'a self, path: &'a str) -> BoxFuture<'a, StorageResult<()>>;

    /// Deletes a file or (recursively) a folder; a missing entry counts as
    /// success.
    fn delete<'a>(&'a self, path: &'a str) -> BoxFuture<'a, StorageResult<()>>;
}

/// Account-level operations of one storage provider account.
pub trait StorageProvider: Send + Sync {
    /// Whether a login is stored (keyring only, no network access).
    fn has_stored_login(&self) -> bool;

    /// Interactive login; with `open_browser` the login page is opened,
    /// otherwise its URL is printed.
    fn login<'a>(
        &'a self,
        open_browser: bool,
        timeout: Duration,
    ) -> BoxFuture<'a, StorageResult<()>>;

    /// Revokes the stored login (best effort) and removes it.
    fn logout<'a>(&'a self) -> BoxFuture<'a, StorageResult<()>>;

    /// The syncable roots of the account.
    fn roots<'a>(&'a self) -> BoxFuture<'a, StorageResult<Vec<StorageRoot>>>;

    /// Opens the storage of one root (no network access).
    fn storage(&self, root_id: &str) -> StorageResult<Arc<dyn RemoteStorage>>;
}
