//! Selective-sync filters: the `include` whitelist and `exclude` glob
//! patterns of a sync job.
//!
//! Semantics (see `PLAN.md`, Phase 5):
//!
//! - `include` is a folder whitelist of exact, `/`-separated folder paths.
//!   An empty list syncs everything. Traversal stops below folders that are
//!   neither on the list nor on the path to a listed folder.
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
}

impl Filter {
    /// Creates a filter from the job's `include`/`exclude` lists. Patterns
    /// must be non-empty and relative (validated like in `oc-config`).
    pub fn new(include: &[String], exclude: &[String]) -> Result<Self, SyncError> {
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
        })
    }

    /// A filter that covers the entire tree (no include/exclude).
    pub fn everything() -> Self {
        Self::default()
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

    /// Whether `path` (relative to the space root; `""` is the root itself)
    /// belongs to the synced set. The root is always covered — included
    /// folders may live anywhere in the tree.
    pub fn covers(&self, path: &str) -> bool {
        if self.is_excluded(path) {
            return false;
        }
        if self.include.is_empty() || path.is_empty() {
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
        Filter::new(&include, &exclude).expect("filter")
    }

    #[test]
    fn empty_filter_covers_everything() {
        let filter = Filter::everything();
        assert!(filter.covers(""));
        assert!(filter.covers("a.txt"));
        assert!(filter.covers("a/b/c.txt"));
        assert!(filter.syncs_everything());
    }

    #[test]
    fn include_whitelist_covers_only_listed_branches() {
        let filter = filter(&["Dokumente", "Bilder/2026"], &[]);
        assert!(!filter.syncs_everything());
        // The root and intermediate folders are covered so traversal reaches
        // the included branches.
        assert!(filter.covers(""));
        assert!(filter.covers("Bilder"));
        assert!(filter.covers("Bilder/2026"));
        assert!(filter.covers("Bilder/2026/foto.jpg"));
        assert!(filter.covers("Dokumente"));
        assert!(filter.covers("Dokumente/a.pdf"));
        // Outside the whitelist.
        assert!(!filter.covers("Bilder/2025"));
        assert!(!filter.covers("Bilder/alt.jpg"));
        assert!(!filter.covers("Musik/song.mp3"));
    }

    #[test]
    fn exclude_segment_patterns_match_at_any_depth() {
        let filter = filter(&[], &["tmp", "*.tmp", ".git"]);
        assert!(!filter.covers("tmp"));
        assert!(!filter.covers("a/b/tmp/c.txt"));
        assert!(!filter.covers("work/data.tmp"));
        assert!(!filter.covers(".git/config"));
        assert!(filter.covers("tmp2"));
        assert!(filter.covers("work/data.txt"));
    }

    #[test]
    fn exclude_path_patterns_match_folders_and_their_contents() {
        let filter = filter(&[], &["build/out"]);
        assert!(!filter.covers("build/out"));
        assert!(!filter.covers("build/out/x.obj"));
        assert!(filter.covers("build"));
        assert!(filter.covers("build/src/main.rs"));
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
        assert!(Filter::new(&bad, &empty).is_err());
        let dotdot: Vec<String> = vec!["../x".into()];
        assert!(Filter::new(&empty, &dotdot).is_err());
        let blank: Vec<String> = vec!["  ".into()];
        assert!(Filter::new(&blank, &empty).is_err());
    }

    #[test]
    fn global_excludes_merge_into_the_job_filter() {
        let mut filter = filter(&[], &["*.tmp"]);
        let global: Vec<String> = vec![".git".into(), "node_modules".into()];
        filter.add_global_excludes(&global).expect("merge");
        assert!(!filter.covers("work/.git/config"));
        assert!(!filter.covers("node_modules/pkg"));
        assert!(!filter.covers("work/data.tmp"));
        assert!(filter.covers("work/data.txt"));

        // Invalid patterns abort and leave the filter untouched.
        let before = filter.clone();
        let bad: Vec<String> = vec!["../escape".into()];
        assert!(filter.add_global_excludes(&bad).is_err());
        assert_eq!(filter, before);
    }
}
