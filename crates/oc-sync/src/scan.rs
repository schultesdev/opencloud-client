//! Scanners: the local directory tree and the remote WebDAV tree, plus the
//! mapping between space-relative paths and local paths.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
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
    /// Modification time (unix milliseconds), if determinable.
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
    /// Server spellings of paths whose name is not in NFC (normalized
    /// path → server path); empty in the common case. See
    /// [`ServerNames`].
    pub server_names: HashMap<String, String>,
}

/// Maps normalized (NFC) paths — the keys of scans, diff, and database —
/// to the paths that address them on the server. A server may store a
/// name in another Unicode form (e.g. NFD from macOS clients); requesting
/// its NFC form would hit a different, non-existing URL.
#[derive(Debug, Clone, Default)]
pub struct ServerNames(HashMap<String, String>);

impl ServerNames {
    /// Wraps the recorded spellings (normalized path → server path).
    pub fn new(names: HashMap<String, String>) -> Self {
        Self(names)
    }

    /// The server path of `key`: the server spelling of its nearest
    /// recorded ancestor (or itself), with the remaining segments
    /// appended; `key` itself when nothing on its path is recorded.
    pub fn resolve(&self, key: &str) -> String {
        if self.0.is_empty() {
            return key.to_owned();
        }
        let mut prefix = key;
        loop {
            if let Some(server) = self.0.get(prefix) {
                return format!("{server}{}", &key[prefix.len()..]);
            }
            match prefix.rsplit_once('/') {
                Some((parent, _)) => prefix = parent,
                None => return key.to_owned(),
            }
        }
    }
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

/// The result of a local scan: what could be read, and what could not.
#[derive(Debug, Default)]
pub struct LocalScan {
    /// All covered, readable paths with their metadata.
    pub entries: HashMap<String, LocalNode>,
    /// Paths that could not be read (permissions, vanished mid-scan,
    /// broken directory listing), with the reason. Their state is
    /// unknown: the engine freezes them (and everything below) for the
    /// pass, so an unreadable folder never looks deleted.
    pub unreadable: Vec<(String, SyncError)>,
}

/// Recursively scans the local directory (`root` is created when missing)
/// and returns all covered paths with their metadata. Symlinks are skipped
/// (they have no meaningful server-side representation in v1), as are
/// `.part` transfer temp files. Only an unreadable root fails the scan;
/// other read errors are collected in [`LocalScan::unreadable`].
pub fn scan_local(root: &Path, filter: &Filter) -> Result<LocalScan, SyncError> {
    let mut scan = LocalScan::default();
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
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(source) if rel_dir.is_empty() => return Err(SyncError::io(&dir, source)),
            Err(source) => {
                scan.unreadable
                    .push((rel_dir.clone(), SyncError::io(&dir, source)));
                continue;
            }
        };
        for entry in read {
            let entry = match entry {
                Ok(entry) => entry,
                Err(source) => {
                    // The listing broke off: the folder is incomplete.
                    scan.unreadable
                        .push((rel_dir.clone(), SyncError::io(&dir, source)));
                    break;
                }
            };
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
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(source) => {
                    scan.unreadable
                        .push((rel_path, SyncError::io(entry.path(), source)));
                    continue;
                }
            };
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
            scan.entries.insert(rel_path, node);
        }
    }
    Ok(scan)
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
/// folder, strictly serially to keep the server load bounded), starting
/// from an already-fetched root listing. Traversal stops at folders
/// outside the filter. `pause` is slept between two folder listings —
/// the configurable server-load throttle (`propfind_pause_ms`).
pub async fn scan_remote_from_root<P: TokenProvider>(
    dav: &WebDavClient<P>,
    filter: &Filter,
    root_listing: &[DavResource],
    pause: Duration,
) -> Result<RemoteSnapshot, SyncError> {
    let root_etag = root_listing
        .first()
        .and_then(|resource| resource.etag.clone());
    let mut entries = HashMap::new();
    let mut server_names = HashMap::new();
    // Folders to list, by their server path (see `ServerNames`).
    let mut pending: VecDeque<String> = VecDeque::new();
    for resource in root_listing.iter().skip(1) {
        add_remote_child(
            resource,
            filter,
            &mut entries,
            &mut server_names,
            &mut pending,
        );
    }
    while let Some(dir) = pending.pop_front() {
        if !pause.is_zero() {
            tokio::time::sleep(pause).await;
        }
        let children = match dav.propfind(&dir, Depth::One).await {
            Ok(children) => children,
            // The folder vanished between the parent listing and this
            // request: its contents are gone; keep syncing the rest.
            Err(oc_api::ApiError::NotFound) => continue,
            Err(source) => return Err(source.into()),
        };
        for resource in children.iter().skip(1) {
            add_remote_child(
                resource,
                filter,
                &mut entries,
                &mut server_names,
                &mut pending,
            );
        }
    }
    Ok(RemoteSnapshot {
        entries,
        root_etag,
        server_names,
    })
}

/// Records one PROPFIND child if it passes the filter; folders are queued
/// for traversal.
fn add_remote_child(
    resource: &DavResource,
    filter: &Filter,
    entries: &mut HashMap<String, RemoteNode>,
    server_names: &mut HashMap<String, String>,
    pending: &mut VecDeque<String>,
) {
    if resource.path.is_empty() || !filter.covers(&resource.path) {
        return;
    }
    if resource.server_path != resource.path {
        server_names.insert(resource.path.clone(), resource.server_path.clone());
    }
    let node = RemoteNode::from_resource(resource);
    if node.is_dir {
        pending.push_back(resource.server_path.clone());
    }
    entries.insert(resource.path.clone(), node);
}

/// Modification time of a file as unix milliseconds. Second precision
/// would miss two same-size edits within one second (e.g. an editor
/// saving right after an upload recorded the first one).
pub(crate) fn unix_mtime(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_names_resolve_nested_paths() {
        let names = ServerNames::new(HashMap::from([(
            "Caf\u{e9}".to_owned(),
            "Cafe\u{301}".to_owned(),
        )]));
        assert_eq!(names.resolve("Caf\u{e9}"), "Cafe\u{301}");
        assert_eq!(names.resolve("Caf\u{e9}/neu.txt"), "Cafe\u{301}/neu.txt");
        assert_eq!(names.resolve("Caf\u{e9}2/x"), "Caf\u{e9}2/x");
        assert_eq!(names.resolve("anders.txt"), "anders.txt");
    }

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
        let entries = scan_local(&root, &filter).expect("scan").entries;
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
        let entries = scan_local(&root, &filter).expect("scan").entries;
        assert!(entries.contains_key("keep/a.txt"));
        assert!(!entries.contains_key("skip"));
        assert!(!entries.contains_key("skip/b.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_folders_are_reported_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("locked")).expect("mkdir");
        std::fs::write(root.join("locked/x.txt"), "x").expect("write");
        std::fs::write(root.join("ok.txt"), "ok").expect("write");
        let locked = root.join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let readable_anyway = std::fs::read_dir(&locked).is_ok(); // root ignores modes

        let scan = scan_local(&root, &Filter::everything()).expect("scan");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(scan.entries.contains_key("ok.txt"));
        assert!(scan.entries.contains_key("locked"));
        if !readable_anyway {
            assert_eq!(scan.unreadable.len(), 1);
            assert_eq!(scan.unreadable[0].0, "locked");
        }
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
