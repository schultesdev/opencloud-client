//! Provider-neutral remote storage abstraction.
//!
//! The sync engine (`oc-sync`) talks only to [`RemoteStorage`]: one
//! remote file tree (an OpenCloud space, a Nextcloud user's files, ...)
//! addressed by `/`-separated paths relative to its root. Account-level
//! operations — login, logout, listing the available roots and opening a
//! storage for one of them — live behind [`StorageProvider`].
//!
//! Implementations (OpenCloud, Nextcloud) live in `oc-api`; this crate has
//! no HTTP dependencies so the engine stays independent of any provider.

mod error;
mod storage;
mod types;

pub use error::{StorageError, StorageResult};
pub use storage::{RemoteStorage, StorageProvider};
pub use types::{
    ByteRange, Download, Listing, ProgressFn, RemoteEntry, StorageQuota, StorageRoot,
    WriteCondition, normalize_nfc,
};

/// Boxed future returned by the trait methods (keeps the traits
/// dyn-compatible).
pub use futures_util::future::BoxFuture;
/// Boxed byte stream of a [`Download`] body.
pub use futures_util::stream::BoxStream;
