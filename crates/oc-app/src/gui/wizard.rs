//! The new-job wizard (Phase 7): pick an account, pick a space from the
//! Graph API, pick a local directory, select the folders to sync
//! (selective sync) from the space's remote tree (PROPFIND), and set
//! exclude patterns. Saving writes the job to `jobs.json` and restarts
//! the engine.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use oc_api::{DavResource, Space};
use oc_config::SyncJob;

use super::backend::UiCommand;

/// What the wizard edits: a new job or the filters of an existing one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Mode {
    /// Create a job (all steps).
    #[default]
    New,
    /// Re-select the folders and exclude patterns of an existing job.
    EditFolders {
        /// The job's id.
        job_id: String,
    },
}

/// Loading state of the spaces list (wizard step 2).
#[derive(Debug, Default)]
pub struct SpacesState {
    /// A `ListSpaces` request is in flight.
    pub loading: bool,
    /// The loaded spaces, once available.
    pub list: Option<Vec<Space>>,
    /// Why the last request failed; no new request is sent until the
    /// user retries (otherwise every frame would send one).
    pub error: Option<String>,
}

/// The lazily loaded remote folder tree (wizard step 4).
#[derive(Debug, Default)]
pub struct FolderTree {
    /// Loaded subfolders per path (`""` = space root); shared slices, so
    /// rendering a node does not copy its children every frame.
    pub children: HashMap<String, Rc<[DavResource]>>,
    /// Pending child requests: path -> correlation token.
    pub loading: HashMap<String, u32>,
    /// Failed child requests: path -> error. Not re-sent automatically;
    /// [`FolderTree::retry`] clears the failure.
    pub failed: HashMap<String, String>,
    /// Expanded folders.
    pub expanded: HashSet<String>,
    /// Selected folders (the job's `include` list).
    pub selected: HashSet<String>,
    /// Next correlation token.
    pub next_token: u32,
}

impl FolderTree {
    /// Sends a child request for `path` unless it is loaded or pending.
    /// `server_path` addresses the folder on the server (its spelling may
    /// differ from the normalized `path`, see `DavResource::server_path`).
    pub fn fetch(
        &mut self,
        account_id: &str,
        space_id: &str,
        path: &str,
        server_path: &str,
        send: impl FnOnce(UiCommand),
    ) {
        if self.children.contains_key(path)
            || self.loading.contains_key(path)
            || self.failed.contains_key(path)
        {
            return;
        }
        self.next_token = self.next_token.wrapping_add(1);
        let token = self.next_token;
        self.loading.insert(path.to_owned(), token);
        send(UiCommand::FetchChildren {
            account_id: account_id.to_owned(),
            space_id: space_id.to_owned(),
            path: server_path.to_owned(),
            token,
        });
    }

    /// Applies a `Children` answer: resolves the token to its path and
    /// stores (and expands) the loaded subfolders. Returns false when
    /// the token is unknown (stale answer).
    pub fn apply_children(&mut self, token: u32, result: Result<Vec<DavResource>, String>) -> bool {
        let Some(path) = self
            .loading
            .iter()
            .find(|(_, pending)| **pending == token)
            .map(|(path, _)| path.clone())
        else {
            return false;
        };
        self.loading.remove(&path);
        match result {
            Ok(children) => {
                self.failed.remove(&path);
                self.children.insert(path.clone(), children.into());
                self.expanded.insert(path);
                true
            }
            Err(error) => {
                self.failed.insert(path, error);
                true
            }
        }
    }

    /// Forgets a failed request for `path`, so the next
    /// [`FolderTree::fetch`] sends it again.
    pub fn retry(&mut self, path: &str) {
        self.failed.remove(path);
    }

    /// Whether a folder or one of its descendants is selected (drives the
    /// half-checked marker).
    pub fn descendant_selected(&self, path: &str) -> bool {
        let prefix = format!("{path}/");
        self.selected
            .iter()
            .any(|selected| selected.starts_with(&prefix))
    }
}

/// Derives the `include` list from the selected folders: when both a
/// folder and one of its descendants are selected, the folder alone
/// covers the descendant (the whitelist matches whole subtrees).
pub fn derive_include(selected: &HashSet<String>) -> Vec<String> {
    // A folder is dropped when one of its ancestors is selected too —
    // the whitelist covers whole subtrees. Checking the ancestors (set
    // lookups) is linear in the depth, not in the selection size.
    let mut include: Vec<String> = selected
        .iter()
        .filter(|path| {
            let mut current = path.as_str();
            while let Some((parent, _)) = current.rsplit_once('/') {
                if selected.contains(parent) {
                    return false;
                }
                current = parent;
            }
            true
        })
        .cloned()
        .collect();
    include.sort();
    include
}

/// The wizard's state, independent of egui (the rendering lives in
/// `mod.rs`).
#[derive(Debug)]
pub struct Wizard {
    /// New job or folder re-selection of an existing job.
    pub mode: Mode,
    /// The chosen account id.
    pub account_id: Option<String>,
    /// Spaces list state.
    pub spaces: SpacesState,
    /// The chosen space id and display name.
    pub space_id: Option<String>,
    /// Display name of the chosen space (stored in `jobs.json`).
    pub space_name: Option<String>,
    /// Local target directory (editable text).
    pub local_path: String,
    /// The remote folder tree.
    pub tree: FolderTree,
    /// Whether files directly in the space root are synced; only
    /// meaningful while the folder selection is non-empty (an empty
    /// selection syncs everything anyway). Defaults to on.
    pub root_files: bool,
    /// Exclude patterns of the job (step 5).
    pub exclude: Vec<String>,
    /// Text field of the exclude pattern editor.
    pub exclude_input: String,
    /// Status/error line below the wizard.
    pub status: Option<String>,
}

impl Default for Wizard {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            account_id: None,
            spaces: SpacesState::default(),
            space_id: None,
            space_name: None,
            local_path: String::new(),
            tree: FolderTree::default(),
            root_files: true,
            exclude: Vec::new(),
            exclude_input: String::new(),
            status: None,
        }
    }
}

impl Wizard {
    /// A wizard for a new job.
    pub fn new() -> Self {
        Self::default()
    }

    /// A wizard that re-selects the folders of an existing job; account,
    /// space, local path, and excludes are prefilled.
    pub fn edit_folders(job: &SyncJob) -> Self {
        let mut tree = FolderTree::default();
        for folder in &job.include {
            tree.selected.insert(folder.clone());
        }
        Self {
            mode: Mode::EditFolders {
                job_id: job.id.clone(),
            },
            account_id: Some(job.account_id.clone()),
            spaces: SpacesState::default(),
            space_id: Some(job.space_id.clone()),
            space_name: job.space_name.clone(),
            local_path: job.local_path.display().to_string(),
            tree,
            root_files: job.sync_root_files,
            exclude: job.exclude.clone(),
            exclude_input: String::new(),
            status: None,
        }
    }

    /// Whether all required fields are set to finish the wizard.
    pub fn can_finish(&self) -> bool {
        match &self.mode {
            Mode::New => {
                self.account_id.is_some()
                    && self.space_id.is_some()
                    && !self.local_path.trim().is_empty()
            }
            Mode::EditFolders { .. } => self.space_id.is_some(),
        }
    }

    /// The chosen local path, or none when unset/invalid.
    pub fn local_path_buf(&self) -> Option<PathBuf> {
        let path = PathBuf::from(self.local_path.trim());
        path.is_absolute().then_some(path)
    }

    /// Applies a `Spaces` answer of the backend.
    pub fn apply_spaces(&mut self, result: Result<Vec<Space>, String>) {
        self.spaces.loading = false;
        match result {
            Ok(list) => {
                self.spaces.list = Some(list);
                self.spaces.error = None;
            }
            Err(error) => self.spaces.error = Some(error),
        }
    }

    /// Applies a `Children` answer of the backend (stale tokens are
    /// ignored); reports whether the answer was consumed. Errors land
    /// in the wizard's status line.
    pub fn apply_children(&mut self, token: u32, result: Result<Vec<DavResource>, String>) -> bool {
        let status = result.as_ref().err().cloned();
        let consumed = self.tree.apply_children(token, result);
        if consumed {
            self.status = status;
        }
        consumed
    }

    /// Preselects `account_id` when none is chosen yet. The account
    /// combobox displays the first account either way, and clicking the
    /// already-displayed entry is a no-op — without the preselection the
    /// wizard would never leave step 1.
    pub fn preselect_account(&mut self, account_id: &str) {
        if self.account_id.is_none() {
            self.account_id = Some(account_id.to_owned());
        }
    }

    /// Resets the space-dependent state (after choosing another space).
    pub fn reset_space(&mut self, space: Option<&Space>) {
        self.space_id = space.map(|space| space.id.clone());
        self.space_name = space.map(|space| space.name.clone());
        self.tree = FolderTree::default();
        self.status = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn failed_requests_are_not_resent_until_retried() {
        let mut tree = FolderTree::default();
        let mut sent = 0;
        tree.fetch("a", "s", "", "", |_| sent += 1);
        let token = tree.next_token;
        assert!(tree.apply_children(token, Err("boom".into())));
        // Every frame calls fetch again; nothing is sent.
        tree.fetch("a", "s", "", "", |_| sent += 1);
        tree.fetch("a", "s", "", "", |_| sent += 1);
        assert_eq!(sent, 1);
        tree.retry("");
        tree.fetch("a", "s", "", "", |_| sent += 1);
        assert_eq!(sent, 2);
    }

    #[test]
    fn stale_answers_leave_the_status_alone() {
        let mut wizard = Wizard::default();
        assert!(!wizard.apply_children(42, Err("old".into())));
        assert_eq!(wizard.status, None);
    }

    #[test]
    fn derive_include_collapses_descendants() {
        let include = derive_include(&set(&["a", "a/b", "a/b/c", "d", "e/f"]));
        assert_eq!(include, ["a", "d", "e/f"]);
        // A sibling sorting between a folder and its child stays.
        let include = derive_include(&set(&["a", "a b", "a/b"]));
        assert_eq!(include, ["a", "a b"]);
    }

    #[test]
    fn derive_include_of_empty_selection_is_everything() {
        assert!(derive_include(&set(&[])).is_empty());
    }

    #[test]
    fn folder_tree_ignores_stale_children() {
        let mut tree = FolderTree::default();
        tree.loading.insert("".into(), 7);
        assert!(tree.apply_children(7, Ok(Vec::new())));
        assert!(tree.children.contains_key(""));
        // Stale token: ignored.
        assert!(!tree.apply_children(7, Ok(Vec::new())));
    }

    #[test]
    fn preselect_account_sets_only_when_unset() {
        let mut wizard = Wizard::default();
        wizard.preselect_account("konto-1");
        assert_eq!(wizard.account_id.as_deref(), Some("konto-1"));
        // An existing choice is not overwritten.
        wizard.preselect_account("konto-2");
        assert_eq!(wizard.account_id.as_deref(), Some("konto-1"));
    }

    #[test]
    fn wizard_edit_folders_prefills_selection() {
        let job = SyncJob {
            id: "job-1".into(),
            account_id: "konto-1".into(),
            space_id: "space-1".into(),
            space_name: Some("Mein Space".into()),
            local_path: PathBuf::from("/tmp/sync"),
            enabled: true,
            include: vec!["Dokumente".into()],
            exclude: vec!["*.tmp".into()],
            sync_root_files: true,
        };
        let wizard = Wizard::edit_folders(&job);
        assert_eq!(
            wizard.mode,
            Mode::EditFolders {
                job_id: "job-1".into()
            }
        );
        assert_eq!(wizard.account_id.as_deref(), Some("konto-1"));
        assert_eq!(wizard.tree.selected, set(&["Dokumente"]));
        assert_eq!(wizard.exclude, ["*.tmp"]);
        assert!(wizard.root_files, "root-files toggle must be prefilled");
        assert!(wizard.can_finish());
    }
}
