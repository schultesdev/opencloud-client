//! The new-job wizard (Phase 7): pick an account, pick a space from the
//! Graph API, pick a local directory, and select the folders to sync
//! (selective sync) from the space's remote tree (PROPFIND). Saving
//! writes the job to `jobs.json` and restarts the engine.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

use oc_api::{DavResource, Space};
use oc_config::SyncJob;

use super::backend::UiCommand;

/// What the wizard edits: a new job or the folder selection of an
/// existing one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Mode {
    /// Create a job (all steps).
    #[default]
    New,
    /// Re-select the folders of an existing job.
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
}

/// The lazily loaded remote folder tree (wizard step 4).
#[derive(Debug, Default)]
pub struct FolderTree {
    /// Loaded subfolders per path (`""` = space root).
    pub children: HashMap<String, Vec<DavResource>>,
    /// Pending child requests: path -> correlation token.
    pub loading: HashMap<String, u32>,
    /// Expanded folders.
    pub expanded: HashSet<String>,
    /// Selected folders (the job's `include` list).
    pub selected: HashSet<String>,
    /// Next correlation token.
    pub next_token: u32,
}

impl FolderTree {
    /// Sends a child request for `path` unless it is loaded or pending.
    pub fn fetch(
        &mut self,
        account_id: &str,
        space_id: &str,
        path: &str,
        send: impl FnOnce(UiCommand),
    ) {
        if self.children.contains_key(path) || self.loading.contains_key(path) {
            return;
        }
        self.next_token = self.next_token.wrapping_add(1);
        let token = self.next_token;
        self.loading.insert(path.to_owned(), token);
        send(UiCommand::FetchChildren {
            account_id: account_id.to_owned(),
            space_id: space_id.to_owned(),
            path: path.to_owned(),
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
                self.children.insert(path.clone(), children);
                self.expanded.insert(path);
                true
            }
            Err(_) => true,
        }
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
    let mut include: Vec<String> = selected
        .iter()
        .filter(|path| {
            // Drop a folder when one of its ancestors is selected too —
            // the whitelist covers whole subtrees.
            !selected
                .iter()
                .any(|other| other != *path && path.starts_with(&format!("{other}/")))
        })
        .cloned()
        .collect();
    include.sort();
    include
}

/// The wizard's state, independent of egui (the rendering lives in
/// `mod.rs`).
#[derive(Debug, Default)]
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
    /// Status/error line below the wizard.
    pub status: Option<String>,
}

impl Wizard {
    /// A wizard for a new job.
    pub fn new() -> Self {
        Self::default()
    }

    /// A wizard that re-selects the folders of an existing job; account,
    /// space, and local path are prefilled.
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
            Ok(list) => self.spaces.list = Some(list),
            Err(error) => self.status = Some(error),
        }
    }

    /// Applies a `Children` answer of the backend (stale tokens are
    /// ignored); reports whether the answer was consumed. Errors land
    /// in the wizard's status line.
    pub fn apply_children(&mut self, token: u32, result: Result<Vec<DavResource>, String>) -> bool {
        match &result {
            Ok(_) => self.status = None,
            Err(error) => self.status = Some(error.clone()),
        }
        self.tree.apply_children(token, result)
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
    fn derive_include_collapses_descendants() {
        let include = derive_include(&set(&["a", "a/b", "a/b/c", "d", "e/f"]));
        assert_eq!(include, ["a", "d", "e/f"]);
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
    fn wizard_edit_folders_prefills_selection() {
        let job = SyncJob {
            id: "job-1".into(),
            account_id: "konto-1".into(),
            space_id: "space-1".into(),
            space_name: Some("Mein Space".into()),
            local_path: PathBuf::from("/tmp/sync"),
            enabled: true,
            include: vec!["Dokumente".into()],
            exclude: vec![],
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
        assert!(wizard.can_finish());
    }
}
