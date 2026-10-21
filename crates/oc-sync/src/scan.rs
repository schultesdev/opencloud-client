//! Scanners: the local directory tree and the remote WebDAV tree, plus the
//! mapping between space-relative paths and local paths.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use oc_api::{DavResource, Depth, TokenProvider, WebDavClient, normalize_nfc};

use crate::error::SyncError;
use crate::filter::Filter;

/// Suffix of partial-transfer temp files. They are ignored by the local
/// scanner and the file watcher, and cleaned up at the start of every run.
pub const PART_SUFFIX: &str = ".part";

/// Local file/folder metadata as seen by the scanner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalNode {
    /// Whether this is a folder.
    pub is_dir: bool,
    /// Modification time (unix seconds), if determinable.
    pub mtime: Option<i64>,
    /// Size in bytes (files only).
    pub size: Option<i64>,
}

/// Remote file/folder metadata as seen by PROPFIND.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteNode {
    /// Whether this is a collection (folder).
    pub is_dir: bool,
    /// ETag in quoted form, if the server reports one.
    pub etag: Option<String>,
    /// Size in bytes (files only), if reported.
    pub size: Option<i64>,
}

impl RemoteNode {
    /// Converts a PROPFIND resource into a diff-relevant node.
    pub fn from_resource(resource: &DavResource) -> Self {
        Self {
            is_dir: resource.is_dir,
            etag: resource.etag.clone(),
            size: resource.size.map(|size| size as i64),
        }
    }
}

/// The remote tree snapshot plus the root ETag (change-detection shortcut).
#[derive(Debug, Clone, Default)]
pub struct RemoteSnapshot {
    /// All covered resources, keyed by space-relative path.
    pub entries: HashMap<String, RemoteNode>,
    /// ETag of the space root, if reported.
    pub root_etag: Option<String>,
}

/// Maps a space-relative path to the absolute local path.
pub fn abs_local_path(root: &Path, rel: &str) -> Result<PathBuf, SyncError> {
    let mut path = root.to_path_buf();
    for segment in rel.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains('\0') {
            return Err(SyncError::invalid_path(rel));
        }
        path.push(segment);
    }
    Ok(path)
}

/// Recursively scans the local directory (`root` is created when missing)
/// and returns all covered paths with their metadata. Symlinks are skipped
/// (they have no meaningful server-side representation in v1), as are
/// `.part` transfer temp files.
pub fn scan_local(root: &Path, filter: &Filter) -> Result<HashMap<String, LocalNode>, SyncError> {
    let mut entries = HashMap::new();
    if !root.exists() {
        std::fs::create_dir_all(root).map_err(|source| SyncError::io(root, source))?;
    }
    let mut pending: Vec<String> = vec![String::new()];
    while let Some(rel_dir) = pending.pop() {
        let dir = if rel_dir.is_empty() {
            root.to_path_buf()
        } else {
            abs_local_path(root, &rel_dir)?
        };
        let read = std::fs::read_dir(&dir).map_err(|source| SyncError::io(&dir, source))?;
        for entry in read {
            let entry = entry.map_err(|source| SyncError::io(&dir, source))?;
            let name = normalize_nfc(&entry.file_name().to_string_lossy());
            if name.ends_with(PART_SUFFIX) {
                continue;
            }
            let rel_path = if rel_dir.is_empty() {
                name.clone()
            } else {
                format!("{rel_dir}/{name}")
            };
            if !filter.covers(&rel_path) {
                continue;
            }
            let metadata = entry
                .metadata()
                .map_err(|source| SyncError::io(&dir, source))?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            let node = LocalNode {
                is_dir: metadata.is_dir(),
                mtime: unix_mtime(&metadata),
                size: if metadata.is_dir() {
                    None
                } else {
                    Some(metadata.len() as i64)
                },
            };
            if node.is_dir {
                pending.push(rel_path.clone());
            }
            entries.insert(rel_path, node);
        }
    }
    Ok(entries)
}

/// Removes stale `.part` files below `root` (run start recovery).
pub fn remove_part_files(root: &Path) -> Result<(), SyncError> {
    let mut pending: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(path);
            } else if entry.file_name().to_string_lossy().ends_with(PART_SUFFIX) {
                std::fs::remove_file(&path).map_err(|source| SyncError::io(&path, source))?;
            }
        }
    }
    Ok(())
}

/// Traverses the remote tree level by level (PROPFIND `Depth: 1` per
/// folder), starting from an already-fetched root listing. Traversal stops
/// at folders outside the filter.
pub async fn scan_remote_from_root<P: TokenProvider>(
    dav: &WebDavClient<P>,
    filter: &Filter,
    root_listing: &[DavResource],
) -> Result<RemoteSnapshot, SyncError> {
    let root_etag = root_listing
        .first()
        .and_then(|resource| resource.etag.clone());
    let mut entries = HashMap::new();
    let mut pending: VecDeque<String> = VecDeque::new();
    for resource in root_listing.iter().skip(1) {
        add_remote_child(resource, filter, &mut entries, &mut pending);
    }
    while let Some(dir) = pending.pop_front() {
        let children = match dav.propfind(&dir, Depth::One).await {
            Ok(children) => children,
            // The folder vanished between the parent listing and this
            // request: its contents are gone; keep syncing the rest.
            Err(oc_api::ApiError::NotFound) => continue,
            Err(source) => return Err(source.into()),
        };
        for resource in children.iter().skip(1) {
            add_remote_child(resource, filter, &mut entries, &mut pending);
        }
    }
    Ok(RemoteSnapshot { entries, root_etag })
}

/// Records one PROPFIND child if it passes the filter; folders are queued
/// for traversal.
fn add_remote_child(
    resource: &DavResource,
    filter: &Filter,
    entries: &mut HashMap<String, RemoteNode>,
    pending: &mut VecDeque<String>,
) {
    if resource.path.is_empty() || !filter.covers(&resource.path) {
        return;
    }
    let node = RemoteNode::from_resource(resource);
    if node.is_dir {
        pending.push_back(resource.path.clone());
    }
    entries.insert(resource.path.clone(), node);
}

/// Modification time of a file as unix seconds.
pub(crate) fn unix_mtime(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_local_path_rejects_traversal() {
        let root = Path::new("/tmp/sync");
        assert_eq!(
            abs_local_path(root, "a/b.txt").expect("path"),
            PathBuf::from("/tmp/sync/a/b.txt")
        );
        assert!(abs_local_path(root, "../escape").is_err());
        assert!(abs_local_path(root, "a//b").is_err());
    }

    #[test]
    fn scan_local_skips_part_files_and_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("a.txt"), "hello").expect("write");
        std::fs::write(root.join("a.txt.part"), b"partial").expect("write");
        std::fs::write(root.join("sub/b.txt"), "world").expect("write");

        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("a.txt"), root.join("link.txt")).expect("symlink");

        let filter = Filter::everything();
        let entries = scan_local(&root, &filter).expect("scan");
        assert!(entries.contains_key("a.txt"));
        assert!(entries.contains_key("sub"));
        assert!(entries.contains_key("sub/b.txt"));
        assert!(!entries.contains_key("a.txt.part"));
        assert!(!entries.contains_key("link.txt"));
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn scan_local_creates_missing_root_and_applies_filters() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("keep")).expect("mkdir");
        std::fs::create_dir_all(root.join("skip")).expect("mkdir");
        std::fs::write(root.join("keep/a.txt"), "a").expect("write");
        std::fs::write(root.join("skip/b.txt"), "b").expect("write");

        let include: Vec<String> = vec!["keep".into()];
        let filter = Filter::new(&include, &[]).expect("filter");
        let entries = scan_local(&root, &filter).expect("scan");
        assert!(entries.contains_key("keep/a.txt"));
        assert!(!entries.contains_key("skip"));
        assert!(!entries.contains_key("skip/b.txt"));
    }

    #[test]
    fn remove_part_files_deletes_only_part_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "keep").expect("write");
        std::fs::write(dir.path().join("a.txt.part"), "drop").expect("write");
        std::fs::write(dir.path().join("b.part.part"), "drop").expect("write");

        remove_part_files(dir.path()).expect("cleanup");
        assert!(dir.path().join("a.txt").exists());
        assert!(!dir.path().join("a.txt.part").exists());
        assert!(!dir.path().join("b.part.part").exists());
    }
}
