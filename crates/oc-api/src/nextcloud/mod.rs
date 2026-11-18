//! Nextcloud as a storage provider: one file tree per user at
//! `/remote.php/dav/files/{user}/`, login via app password
//! ([`oc_auth::AppPasswordManager`], HTTP Basic), user and quota via the
//! OCS API, and chunked uploads (v2) for large files.

mod ocs;
mod provider;
mod storage;

pub use ocs::NextcloudUser;
pub use provider::NextcloudProvider;
pub use storage::{ChunkingConfig, NextcloudStorage};
