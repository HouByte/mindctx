// SPDX-License-Identifier: MIT OR Apache-2.0

//! Glob include/exclude filter for search: `!`-prefixed globs are exclusions
//! and always veto; other globs are inclusions (no inclusion = everything matches). Globs are
//! compiled with `literal_separator(true)` — `*` does not cross `/`, `**/*` does — and are
//! matched against the slash-normalized path relative to the search root.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// A compiled path filter. Every pattern — inclusion or exclusion — lives in one
/// [`GlobSet`], with a parallel flag marking the exclusion entries. A path passes when
/// no exclusion matches it and, when any inclusion exists, at least one inclusion does.
pub struct PathGlobFilter {
    set: GlobSet,
    is_exclusion: Vec<bool>,
    has_inclusion: bool,
}

impl PathGlobFilter {
    /// Compiles the wire patterns (search semantics: `literal_separator(true)`). An empty
    /// list compiles to `None` — the caller's "no filter" default — because the wire
    /// default cannot distinguish "absent" from "explicitly empty".
    pub fn from_patterns(patterns: &[String]) -> Result<Option<Self>, String> {
        if patterns.is_empty() {
            return Ok(None);
        }
        let mut builder = GlobSetBuilder::new();
        let mut is_exclusion = Vec::with_capacity(patterns.len());
        let mut has_inclusion = false;
        for raw in patterns {
            let (veto, pattern) = match raw.strip_prefix('!') {
                Some("") => return Err("a bare `!` carries no glob pattern".to_string()),
                Some(body) => (true, body),
                None => (false, raw.as_str()),
            };
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|error| error.to_string())?;
            builder.add(glob);
            is_exclusion.push(veto);
            has_inclusion |= !veto;
        }
        let set = builder.build().map_err(|error| error.to_string())?;
        Ok(Some(Self {
            set,
            is_exclusion,
            has_inclusion,
        }))
    }

    /// Whether `path` (slash-normalized, relative to the search root) passes the filter:
    /// no exclusion glob matches it, and an inclusion glob does when inclusions exist —
    /// exclusions always win.
    pub fn admits(&self, path: &str) -> bool {
        // Cheap boolean gate first: a path that matches nothing needs no per-index work,
        // so the common filtered-out case never allocates the match-index vec.
        if !self.set.is_match(path) {
            return !self.has_inclusion;
        }
        let mut included = !self.has_inclusion;
        for index in self.set.matches(path) {
            if self.is_exclusion[index] {
                return false;
            }
            included = true;
        }
        included
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(patterns: &[&str]) -> PathGlobFilter {
        let owned: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        PathGlobFilter::from_patterns(&owned).unwrap().unwrap()
    }

    #[test]
    fn exclusions_always_win_over_inclusions() {
        let f = filter(&["*.rs", "!secret.rs"]);
        assert!(f.admits("main.rs"));
        assert!(!f.admits("secret.rs"), "exclusion vetoes the inclusion");
        assert!(!f.admits("main.py"));
        assert!(!f.admits("src/main.rs"), "`*` does not cross `/`");
        let recursive = filter(&["**/*.rs", "!secret.rs"]);
        assert!(recursive.admits("main.rs"), "`**/` also matches the root");
        assert!(recursive.admits("src/main.rs"), "`**` crosses `/`");
        assert!(!recursive.admits("src/main.py"));
    }

    #[test]
    fn negative_only_list_includes_everything_else() {
        let f = filter(&["!*.md"]);
        assert!(
            f.admits("src/main.rs"),
            "no inclusion = every other file matches"
        );
        assert!(!f.admits("README.md"));
        assert!(f.admits("docs/x.rs"));
    }

    #[test]
    fn empty_list_is_no_filter() {
        assert!(PathGlobFilter::from_patterns(&[]).unwrap().is_none());
        assert!(PathGlobFilter::from_patterns(&["!".to_string()]).is_err());
    }
}
