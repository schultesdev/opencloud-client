//! Selective-sync filters: the `include` whitelist and `exclude` glob
//! patterns of a sync job.
//!
//! Semantics (see `PLAN.md`, Phase 5):
//!
//! - `include` is a folder whitelist of exact, `/`-separated folder paths.
//!   An empty list syncs everything. Traversal stops below folders that are
//!   neither on the list nor on the path to a listed folder. While the list
//!   is set, files directly in the root are only synced when the job's
//!   root-files toggle is on (see [`Filter`]); root folders always need a
//!   whitelist entry.
//! - `exclude` holds glob patterns (`*` = any run of characters except `/`,
//!   `?` = one character). A pattern without `/` matches a single path
//!   segment (so `tmp` excludes any folder named `tmp` and `*.tmp` any
//!   file with that suffix, at any depth); a pattern with `/` is matched
//!   against the full relative path and every folder prefix of it.
//! - Global exclude patterns (the settings' `global_exclude`) are merged
//!   into every job's `exclude` list on engine start (see
//!   [`Filter::add_global_excludes`]).

use crate::error::SyncError;

/// The `include`/`exclude` filter set of one sync job.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Filter {
    include: Vec<String>,
    exclude: Vec<String>,
    /// Whether files directly in the space root belong to the synced
    /// set while the whitelist is non-empty (root folders always need
    /// an `include` entry).
    root_files: bool,
}

impl Filter {
    /// Creates a filter from the job's `include`/`exclude` lists and the
    /// root-files toggle. Patterns must be non-empty and relative
    /// (validated like in `oc-config`).
    pub fn new(
        include: &[String],
        exclude: &[String],
        root_files: bool,
    ) -> Result<Self, SyncError> {
        for pattern in include.iter().chain(exclude.iter()) {
            validate_pattern(pattern)?;
        }
        Ok(Self {
            include: include
                .iter()
                .map(|pattern| pattern.trim_end_matches('/').to_string())
                .filter(|pattern| !pattern.is_empty())
                .collect(),
            exclude: exclude.to_vec(),
            root_files,
        })
    }

    /// A filter that covers the entire tree (no include/exclude, root
    /// files on).
    pub fn everything() -> Self {
        Self {
            root_files: true,
            ..Self::default()
        }
    }

    /// Adds patterns that apply on top of the job's own `exclude` list:
    /// the global excludes of the settings, merged into every job by
    /// [`crate::Engine::start`]. Each pattern is validated; the first
    /// invalid one aborts with an error (the filter stays unchanged).
    pub fn add_global_excludes(&mut self, patterns: &[String]) -> Result<(), SyncError> {
        for pattern in patterns {
            validate_pattern(pattern)?;
        }
        self.exclude.extend_from_slice(patterns);
        Ok(())
    }

    /// Whether the whitelist is empty (everything is synced).
    pub fn syncs_everything(&self) -> bool {
        self.include.is_empty()
    }

    /// A canonical fingerprint of everything `covers` depends on: the
    /// sorted `include` and `exclude` patterns plus the root-files
    /// toggle (global excludes arrive via `add_global_excludes`, so
    /// they are part of it as well). It changes exactly when the
    /// synced set changes, which lets the engine detect
    /// selective-sync edits between two passes and invalidate the
    /// root-ETag shortcut (see `run_once`).
    pub fn signature(&self) -> String {
        let mut include = self.include.clone();
        let mut exclude = self.exclude.clone();
        include.sort_unstable();
        exclude.sort_unstable();
        format!(
            "i\u{1f}{}\u{1f}e\u{1f}{}\u{1f}r\u{1f}{}",
            include.join("\u{1f}"),
            exclude.join("\u{1f}"),
            self.root_files
        )
    }

    /// Whether `path` (relative to the space root; `""` is the root
    /// itself) belongs to the synced set. The root is always covered —
    /// included folders may live anywhere in the tree. With a
    /// non-empty whitelist, files directly in the root are only
    /// covered when the root-files toggle is on; root folders always
    /// need an entry on the path to a listed folder.
    pub fn covers(&self, path: &str, is_dir: bool) -> bool {
        if self.is_excluded(path) {
            return false;
        }
        if self.include.is_empty() || path.is_empty() {
            return true;
        }
        if !is_dir && !path.contains('/') && self.root_files {
            return true;
        }
        self.include.iter().any(|included| {
            included == path
                || path.starts_with(&format!("{included}/"))
                || included.starts_with(&format!("{path}/"))
        })
    }

    /// Whether `path` is ruled out by an `exclude` pattern.
    fn is_excluded(&self, path: &str) -> bool {
        for pattern in &self.exclude {
            if !pattern.contains('/') {
                if path.split('/').any(|segment| glob_match(pattern, segment)) {
                    return true;
                }
            } else {
                // Match the path itself and every folder prefix of it.
                let mut prefix = String::new();
                for segment in path.split('/') {
                    if !prefix.is_empty() {
                        prefix.push('/');
                    }
                    prefix.push_str(segment);
                    if glob_match(pattern, &prefix) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// Validates one pattern the same way `oc-config` does.
fn validate_pattern(pattern: &str) -> Result<(), SyncError> {
    if pattern.trim().is_empty() {
        return Err(SyncError::InvalidPattern {
            pattern: pattern.to_owned(),
            reason: "must not be empty",
        });
    }
    if pattern.starts_with('/') || pattern.starts_with('\\') {
        return Err(SyncError::InvalidPattern {
            pattern: pattern.to_owned(),
            reason: "must be relative",
        });
    }
    if pattern.contains('\0') {
        return Err(SyncError::InvalidPattern {
            pattern: pattern.to_owned(),
            reason: "must not contain NUL bytes",
        });
    }
    for segment in pattern.split(['/', '\\']) {
        if matches!(segment, "." | "..") {
            return Err(SyncError::InvalidPattern {
                pattern: pattern.to_owned(),
                reason: "must not contain '.' or '..' segments",
            });
        }
    }
    Ok(())
}

/// Glob match with `*` (any run of non-`/` characters) and `?` (one
/// character). Recursive but bounded by pattern + text length.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    glob_match_chars(&pattern, &text)
}

fn glob_match_chars(pattern: &[char], text: &[char]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some('*') => {
            glob_match_chars(&pattern[1..], text)
                || (!text.is_empty() && text[0] != '/' && glob_match_chars(pattern, &text[1..]))
        }
        Some('?') => !text.is_empty() && glob_match_chars(&pattern[1..], &text[1..]),
        Some(expected) => {
            !text.is_empty() && text[0] == *expected && glob_match_chars(&pattern[1..], &text[1..])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(include: &[&str], exclude: &[&str]) -> Filter {
        let include: Vec<String> = include.iter().map(|s| s.to_string()).collect();
        let exclude: Vec<String> = exclude.iter().map(|s| s.to_string()).collect();
        Filter::new(&include, &exclude, true).expect("filter")
    }

    #[test]
    fn empty_filter_covers_everything() {
        let filter = Filter::everything();
        assert!(filter.covers("", true));
        assert!(filter.covers("a.txt", false));
        assert!(filter.covers("a/b/c.txt", false));
        assert!(filter.syncs_everything());
    }

    #[test]
    fn include_whitelist_covers_only_listed_branches() {
        let filter = filter(&["Dokumente", "Bilder/2026"], &[]);
        assert!(!filter.syncs_everything());
        // The root and intermediate folders are covered so traversal reaches
        // the included branches.
        assert!(filter.covers("", true));
        assert!(filter.covers("Bilder", true));
        assert!(filter.covers("Bilder/2026", true));
        assert!(filter.covers("Bilder/2026/foto.jpg", false));
        assert!(filter.covers("Dokumente", true));
        assert!(filter.covers("Dokumente/a.pdf", false));
        // Outside the whitelist.
        assert!(!filter.covers("Bilder/2025", false));
        assert!(!filter.covers("Bilder/alt.jpg", false));
        assert!(!filter.covers("Musik/song.mp3", false));
    }

    #[test]
    fn root_files_toggle_decides_root_level_files_only() {
        let include: Vec<String> = vec!["Dokumente".into()];
        let empty: Vec<String> = vec![];
        let on = Filter::new(&include, &empty, true).expect("filter");
        let off = Filter::new(&include, &empty, false).expect("filter");
        // Root-level files follow the toggle…
        assert!(on.covers("a.txt", false));
        assert!(!off.covers("a.txt", false));
        // …root folders do not: they need a whitelist entry either way.
        assert!(!on.covers("Musik", true));
        assert!(!off.covers("Musik", true));
        // Inside whitelisted branches the toggle is irrelevant.
        assert!(on.covers("Dokumente/a.pdf", false));
        assert!(off.covers("Dokumente/a.pdf", false));
    }

    #[test]
    fn signature_is_order_insensitive_and_tracks_every_input() {
        // Pattern order is irrelevant for `covers`, so it must not
        // change the signature.
        assert_eq!(
            filter(&["A", "B"], &["x", "y"]).signature(),
            filter(&["B", "A"], &["y", "x"]).signature()
        );
        // Include, exclude and the root-files toggle each do.
        assert_ne!(
            filter(&["A"], &[]).signature(),
            filter(&["A", "B"], &[]).signature()
        );
        assert_ne!(
            filter(&["A"], &[]).signature(),
            filter(&["A"], &["x"]).signature()
        );
        let include: Vec<String> = vec!["A".into()];
        let empty: Vec<String> = vec![];
        assert_ne!(
            Filter::new(&include, &empty, true)
                .expect("filter")
                .signature(),
            Filter::new(&include, &empty, false)
                .expect("filter")
                .signature()
        );
    }

    #[test]
    fn exclude_segment_patterns_match_at_any_depth() {
        let filter = filter(&[], &["tmp", "*.tmp", ".git"]);
        assert!(!filter.covers("tmp", true));
        assert!(!filter.covers("a/b/tmp/c.txt", false));
        assert!(!filter.covers("work/data.tmp", false));
        assert!(!filter.covers(".git/config", false));
        assert!(filter.covers("tmp2", false));
        assert!(filter.covers("work/data.txt", false));
    }

    #[test]
    fn exclude_path_patterns_match_folders_and_their_contents() {
        let filter = filter(&[], &["build/out"]);
        assert!(!filter.covers("build/out", true));
        assert!(!filter.covers("build/out/x.obj", false));
        assert!(filter.covers("build", true));
        assert!(filter.covers("build/src/main.rs", false));
    }

    #[test]
    fn glob_star_does_not_cross_segments() {
        assert!(glob_match("*.tmp", "a.tmp"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("*.tmp", "a/b.tmp"));
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        let empty: Vec<String> = vec![];
        let bad: Vec<String> = vec!["/abs".into()];
        assert!(Filter::new(&bad, &empty, true).is_err());
        let dotdot: Vec<String> = vec!["../x".into()];
        assert!(Filter::new(&empty, &dotdot, true).is_err());
        let blank: Vec<String> = vec!["  ".into()];
        assert!(Filter::new(&blank, &empty, true).is_err());
    }

    #[test]
    fn global_excludes_merge_into_the_job_filter() {
        let mut filter = filter(&[], &["*.tmp"]);
        let global: Vec<String> = vec![".git".into(), "node_modules".into()];
        filter.add_global_excludes(&global).expect("merge");
        assert!(!filter.covers("work/.git/config", false));
        assert!(!filter.covers("node_modules/pkg", false));
        assert!(!filter.covers("work/data.tmp", false));
        assert!(filter.covers("work/data.txt", false));

        // Invalid patterns abort and leave the filter untouched.
        let before = filter.clone();
        let bad: Vec<String> = vec!["../escape".into()];
        assert!(filter.add_global_excludes(&bad).is_err());
        assert_eq!(filter, before);
    }
}
