// SPDX-License-Identifier: MIT OR Apache-2.0

//! Candidate collection over `ignore::WalkBuilder` with two frozen policies (search,
//! glob) plus the deterministic result orderings.
//!
//! Glob defaults deliberately diverge from search: glob honors the full git ignore stack,
//! prunes `.git`, and sorts by mtime descending. Rationale: token efficiency — VCS
//! internals and ignored build artifacts are never useful default output (a naive `**/*.rs` first
//! page on this repo was ~12× the byte cost of the `git ls-files` equivalent). Search keeps its
//! own frozen policy unchanged.
//!
//! This is the front door for search and glob: it yields the candidate universe
//! (files + resolved symlinks) and the unreachable-path report. No matching, no budget, no encoding
//! decisions happen here — those are downstream concerns.

use std::fs::{self, FileType, Metadata};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::envelope::SkipDetail;
use crate::error::{Error, Result};

use super::glob_tool::{GlobFilterMode, GlobSort};

/// One file a downstream tool may consider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// As-walked path (root as given, joined components).
    pub path: PathBuf,
    /// Slash-normalized path relative to the traversal root (forward slashes on every platform).
    pub rel_display: String,
    /// Modification time of the resolved (link-followed) file.
    pub mtime: SystemTime,
    /// Size in bytes of the resolved file.
    pub size: u64,
}

/// Traversal policy, frozen per tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraversalPolicy {
    /// search: dotfiles ARE searched, `.ignore` + all git ignore sources honored, `.git` pruned.
    Search,
    /// glob: standard filters off; `Ignore` mode honors `.ignore` + all git ignore sources
    /// (aligning with `Search`); `All` mode applies no ignore filtering at all. Every glob
    /// mode prunes `.git`.
    Glob { filter_mode: GlobFilterMode },
}

/// Walks `root` under `policy`, returning the candidate files and the unreachable-path report.
///
/// Files and symlinks only. Symlinks are resolved via `std::fs::metadata` (follows the link);
/// broken links and links resolving to directories are dropped silently — they are not skip
/// details. Walker entry errors (unreadable directory, vanished mid-walk) do not abort the walk:
/// each becomes `SkipDetail { path, reason: "unreachable: {err}" }` (frozen format).
pub fn collect(
    root: &Path,
    policy: &TraversalPolicy,
) -> Result<(Vec<Candidate>, Vec<SkipDetail>), Error> {
    if !root.is_dir() {
        return Err(Error::Config(format!(
            "traversal root is not a directory: {}",
            root.display()
        )));
    }
    let mut candidates = Vec::new();
    let mut skips = Vec::new();
    for entry in build_walker(root, policy) {
        let entry = match entry {
            Ok(entry) => entry,
            // One unreachable entry (permission, vanished mid-walk) is reported, not fatal.
            Err(err) => {
                skips.push(skip_detail_for(root, &err));
                continue;
            }
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        // Directories are never candidates; broken links resolve to nothing.
        let Some(meta) = resolve_metadata(file_type, entry.path()) else {
            continue;
        };
        candidates.push(Candidate {
            path: entry.path().to_path_buf(),
            rel_display: rel_display(root, entry.path()),
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: meta.len(),
        });
    }
    Ok((candidates, skips))
}

/// Builds the walker for `policy`. Flag semantics (frozen):
/// - `hidden(false)` = dotfiles ARE searched. WalkBuilder's flag means "skip hidden entries",
///   so searching dotfiles requires the flag to be FALSE — do not "fix" this.
/// - `ignore(true)` honors `.ignore` files; the git flags honor `.gitignore`, global excludes
///   and `.git/info/exclude` (search and glob's default `Ignore` mode).
/// - `standard_filters(false)` (glob) turns every filter off; only what the mode re-enables applies.
/// - `.git` is pruned under every policy except at depth 0 (a root literally named `.git` stays
///   traversable). `.git` internals are never useful glob output, so the prune is shared
///   instead of search-only.
fn build_walker(root: &Path, policy: &TraversalPolicy) -> ignore::Walk {
    let mut builder = ignore::WalkBuilder::new(root);
    match policy {
        TraversalPolicy::Search => {
            builder
                .hidden(false)
                .ignore(true)
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true);
        }
        TraversalPolicy::Glob { filter_mode } => {
            builder.standard_filters(false).parents(true).hidden(false);
            match filter_mode {
                GlobFilterMode::Ignore => {
                    // Re-enable the full ignore stack (`.ignore` + all git ignore sources) so
                    // the default glob page matches the `git ls-files` universe. Token
                    // efficiency: ignored build artifacts are never useful default output.
                    builder
                        .ignore(true)
                        .git_ignore(true)
                        .git_global(true)
                        .git_exclude(true);
                }
                GlobFilterMode::All => {
                    // No ignore filtering at all: not `.ignore`, not git.
                    builder.ignore(false).git_ignore(false);
                }
            }
        }
    }
    // Predictable semantics (same choice as index::walk): gitignore applies even outside
    // a git repository — scratch projects can still exclude generated files.
    builder.require_git(false);
    // Prune `.git` (except the root itself — someone may point at one). Shared by search and
    // glob: VCS internals are never useful default output under either tool.
    builder.filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".git");
    builder.follow_links(false);
    builder.build()
}

/// Files and symlinks only, resolved to real-file metadata. Symlinks are followed via
/// `std::fs::metadata`; a broken link or a link resolving to a directory yields `None`
/// dropped silently — not a skip detail). mtime/size always come from the resolved target.
fn resolve_metadata(file_type: FileType, path: &Path) -> Option<Metadata> {
    if file_type.is_symlink() {
        fs::metadata(path).ok().filter(|meta| meta.is_file())
    } else if file_type.is_file() {
        fs::metadata(path).ok()
    } else {
        None
    }
}

/// Slash-normalized path relative to `root` (forward slashes even on Windows).
fn rel_display(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

/// Best-effort path attribution for a walker error (`ignore::Error` has no path accessor;
/// the walker wraps IO failures in `WithPath`/`WithDepth`).
fn error_path(err: &ignore::Error) -> Option<&Path> {
    match err {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            error_path(err)
        }
        ignore::Error::Loop { ancestor, .. } => Some(ancestor),
        _ => None,
    }
}

/// Builds the skip detail for one walker error (frozen reason format). When no path is
/// attributable (bare `Io`/`Glob` errors without a `WithPath` wrapper), the detail points at the
/// traversal root itself: an empty relative path would render a confusing skip line, and the root
/// is the most useful locator the error still gives us.
fn skip_detail_for(root: &Path, err: &ignore::Error) -> SkipDetail {
    let path = match error_path(err) {
        Some(path) => rel_display(root, path),
        None => root.display().to_string(),
    };
    SkipDetail {
        path,
        reason: format!("unreachable: {err}"),
    }
}

/// Search order: mtime DESC, then rel_display bytes ASC.
pub fn order_search(cands: &mut [Candidate]) {
    cands.sort_by(|a, b| {
        b.mtime
            .cmp(&a.mtime)
            .then_with(|| a.rel_display.as_bytes().cmp(b.rel_display.as_bytes()))
    });
}

/// Glob order: `Path` →
/// rel_display bytes ASC; `Modified` → mtime DESC then rel_display bytes ASC.
pub fn order_glob(cands: &mut [Candidate], sort: GlobSort) {
    match sort {
        GlobSort::Path => {
            cands.sort_by(|a, b| a.rel_display.as_bytes().cmp(b.rel_display.as_bytes()));
        }
        GlobSort::Modified => order_search(cands),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// Shared fixture: a dotfile, an ignored file, a normal file, a nested file, and a
    /// `.gitignore`. `with_git_dir` adds a `.git/HEAD` stand-in to observe the `.git` prune;
    /// without it the fixture is a plain directory with no git repository, which pins the
    /// `require_git(false)` semantics (gitignore applies outside a repo).
    fn fixture(with_git_dir: bool) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write(&root, "normal.rs", "fn normal() {}");
        write(&root, ".hidden.rs", "fn hidden() {}");
        write(&root, "nested/deep.rs", "fn deep() {}");
        write(&root, "ignored.rs", "fn ignored() {}");
        write(&root, ".gitignore", "ignored.rs\n");
        if with_git_dir {
            write(&root, ".git/HEAD", "ref: refs/heads/main");
        }
        (tmp, root)
    }

    fn rels(cands: &[Candidate]) -> Vec<&str> {
        cands.iter().map(|c| c.rel_display.as_str()).collect()
    }

    #[test]
    fn search_includes_dotfiles_and_honors_gitignore() {
        let (_tmp, root) = fixture(true);
        let (cands, skips) = collect(&root, &TraversalPolicy::Search).unwrap();
        let mut got = rels(&cands);
        got.sort_unstable();
        assert_eq!(
            got,
            [".gitignore", ".hidden.rs", "nested/deep.rs", "normal.rs"],
            "dotfiles are searched, .gitignore honored, .git pruned"
        );
        assert!(
            skips.is_empty(),
            "clean fixture must not report skips: {skips:?}"
        );
    }

    #[test]
    fn root_named_git_is_traversable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".git");
        write(&root, "HEAD", "ref: refs/heads/main");
        let (cands, skips) = collect(&root, &TraversalPolicy::Search).unwrap();
        assert_eq!(rels(&cands), ["HEAD"], "depth-0 root is never pruned");
        assert!(skips.is_empty());
        // The depth-0 exception holds for glob too (all glob modes share the prune).
        let (cands, skips) = collect(
            &root,
            &TraversalPolicy::Glob {
                filter_mode: GlobFilterMode::Ignore,
            },
        )
        .unwrap();
        assert_eq!(rels(&cands), ["HEAD"], "glob depth-0 root is never pruned");
        assert!(skips.is_empty());
    }

    /// Pins `require_git(false)`: the `.gitignore` is honored even without a `.git` directory
    /// no repository in this fixture) — the semantics the walker doc comment claims.
    #[test]
    fn search_gitignore_applies_outside_a_git_repo() {
        let (_tmp, root) = fixture(false);
        let (cands, skips) = collect(&root, &TraversalPolicy::Search).unwrap();
        let got = rels(&cands);
        assert!(
            !got.contains(&"ignored.rs"),
            ".gitignore must be honored outside a git repo: {got:?}"
        );
        assert!(
            got.contains(&".hidden.rs"),
            "dotfiles still searched: {got:?}"
        );
        assert!(skips.is_empty());
    }

    /// Regression: glob's default `Ignore` mode honors the full ignore stack — gitignored
    /// files are excluded AND `.git` internals are pruned — while non-ignored dotfiles stay
    /// collectible.
    #[test]
    fn glob_ignore_mode_honors_gitignore_and_prunes_git() {
        let (_tmp, root) = fixture(true);
        let (cands, _) = collect(
            &root,
            &TraversalPolicy::Glob {
                filter_mode: GlobFilterMode::Ignore,
            },
        )
        .unwrap();
        let got = rels(&cands);
        assert!(
            !got.contains(&"ignored.rs"),
            "gitignore must be honored in glob Ignore mode: {got:?}"
        );
        assert!(
            !got.contains(&".git/HEAD"),
            ".git must be pruned in glob Ignore mode: {got:?}"
        );
        assert!(
            got.contains(&".hidden.rs"),
            "non-ignored dotfiles still collected: {got:?}"
        );
        assert!(
            got.contains(&"normal.rs") && got.contains(&"nested/deep.rs"),
            "unignored files still collected: {got:?}"
        );
    }

    /// `All` mode applies no ignore filtering, but the `.git` prune is shared across every
    /// glob mode: VCS internals are never useful glob output, and explicit patterns can
    /// still address any path outside `.git`.
    #[test]
    fn glob_all_mode_prunes_git_but_applies_no_ignore_filtering() {
        let (_tmp, root) = fixture(true);
        write(&root, "never_listed.rs", "fn x() {}");
        write(&root, ".ignore", "never_listed.rs\n");
        let (cands, _) = collect(
            &root,
            &TraversalPolicy::Glob {
                filter_mode: GlobFilterMode::All,
            },
        )
        .unwrap();
        let got = rels(&cands);
        assert!(
            got.contains(&"ignored.rs"),
            "no .gitignore in All mode: {got:?}"
        );
        assert!(
            got.contains(&"never_listed.rs"),
            "no .ignore in All mode: {got:?}"
        );
        assert!(
            !got.contains(&".git/HEAD"),
            ".git is pruned even in All mode: {got:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlinks_are_resolved_and_broken_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "target.rs", "fn target() {}");
        std::os::unix::fs::symlink(root.join("target.rs"), root.join("link.rs")).unwrap();
        std::os::unix::fs::symlink(root.join("missing.rs"), root.join("broken.rs")).unwrap();
        std::os::unix::fs::symlink(root, root.join("dir_link")).unwrap();

        let (cands, skips) = collect(root, &TraversalPolicy::Search).unwrap();
        let got = rels(&cands);
        assert!(
            got.contains(&"link.rs"),
            "symlink to a file is a candidate: {got:?}"
        );
        assert!(
            !got.contains(&"broken.rs"),
            "broken link dropped silently: {got:?}"
        );
        assert!(
            !got.contains(&"dir_link"),
            "link resolving to a directory is not a candidate: {got:?}"
        );
        let link = cands
            .iter()
            .find(|c| c.rel_display == "link.rs")
            .expect("link.rs collected");
        let target_meta = fs::metadata(root.join("target.rs")).unwrap();
        assert_eq!(
            link.size,
            target_meta.len(),
            "size comes from the resolved target"
        );
        assert_eq!(
            link.mtime,
            target_meta.modified().unwrap(),
            "mtime comes from the resolved target"
        );
        assert!(
            skips.is_empty(),
            "broken links are silent, not skips: {skips:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_dir_becomes_skip_detail() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "top.rs", "fn top() {}");
        let locked = root.join("locked");
        write(&locked, "inner/deep.rs", "fn deep() {}");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let outcome = collect(root, &TraversalPolicy::Search);
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));
        let Ok((cands, skips)) = outcome else {
            // Mode bits are advisory here (e.g. running as root): this environment cannot
            // produce an unreachable entry, so there is nothing to assert.
            return;
        };
        let got = rels(&cands);
        assert!(
            !got.iter().any(|r| r.starts_with("locked/")),
            "unreadable subtree yields no candidates: {got:?}"
        );
        assert!(
            skips.iter().any(|s| s.reason.starts_with("unreachable: ")),
            "frozen unreachable format, got: {skips:?}"
        );
    }

    #[test]
    fn collect_rejects_non_directory_root() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not_a_dir");
        fs::write(&file, "x").unwrap();
        assert!(matches!(
            collect(&file, &TraversalPolicy::Search),
            Err(Error::Config(_))
        ));
    }

    /// Unattributable walker errors (bare `Io`/`Glob`, no `WithPath` wrapper) must not render an
    /// empty relative path; they point at the traversal root itself. Exercised through the helper
    /// because no fixture can make the real walker emit a path-less error.
    #[test]
    fn unattributable_walker_error_points_at_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let err = ignore::Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        let detail = skip_detail_for(root, &err);
        assert_eq!(detail.path, root.display().to_string());
        assert!(
            detail.reason.starts_with("unreachable: denied"),
            "frozen unreachable format, got: {}",
            detail.reason
        );
    }

    fn cand(rel: &str, mtime: SystemTime) -> Candidate {
        Candidate {
            path: PathBuf::from(rel),
            rel_display: rel.to_string(),
            mtime,
            size: 1,
        }
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn order_search_is_mtime_desc_then_path_asc() {
        let mut cands = vec![
            cand("z.rs", at(10)),
            cand("m.rs", at(30)),
            cand("a.rs", at(10)),
        ];
        order_search(&mut cands);
        assert_eq!(rels(&cands), ["m.rs", "a.rs", "z.rs"]);
    }

    #[test]
    fn order_tie_break_is_byte_order_not_locale() {
        let mut cands = vec![
            cand("a.rs", at(1)),
            cand("_.rs", at(1)),
            cand("B.rs", at(1)),
        ];
        order_search(&mut cands);
        // Byte order: 'B' (0x42) < '_' (0x5F) < 'a' (0x61).
        assert_eq!(rels(&cands), ["B.rs", "_.rs", "a.rs"]);
    }

    #[test]
    fn order_glob_path_and_modified() {
        let mut cands = vec![
            cand("z.rs", at(10)),
            cand("m.rs", at(30)),
            cand("a.rs", at(10)),
        ];
        order_glob(&mut cands, GlobSort::Path);
        assert_eq!(rels(&cands), ["a.rs", "m.rs", "z.rs"]);
        order_glob(&mut cands, GlobSort::Modified);
        assert_eq!(rels(&cands), ["m.rs", "a.rs", "z.rs"]);
    }
}
