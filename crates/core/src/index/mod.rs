// SPDX-License-Identifier: MIT OR Apache-2.0

//! Index layer: walker (gitignore-aware). Current surface — corpus enumeration for rg-layer queries +
//! corpus access conventions (path resolution).

use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// Per-file skip threshold: files above it are treated as generated/binary and excluded from the retrieval corpus.
pub const MAX_FILE_SIZE: u64 = 1024 * 1024;

/// A file in the corpus: project-relative path (`/`-separated, stable across platforms) + size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub rel: String,
    pub size: u64,
}

/// Walks the project corpus: gitignore / .ignore / hidden files are all respected (standard `ignore` filtering),
/// additionally skipping mindctx's own runtime directory `.mindctx/` and oversized files.
/// Output is sorted by relative path, keeping retrieval and reports deterministic.
pub fn walk(root: &Path) -> Result<Vec<FileEntry>> {
    let root = root
        .canonicalize()
        .map_err(|e| Error::Index(format!("project root unreachable {root:?}: {e}")))?;
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(&root)
        // Respect .gitignore even in non-git directories (scratch projects can still exclude
        // generated files); unlike rg's default (require_git), we take the more predictable semantics.
        .require_git(false)
        .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".mindctx")
        .build();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            // A single unreadable entry (permissions etc.) is not fatal: skip it; the corpus is whatever is reachable.
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(&root) else {
            continue;
        };
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if size > MAX_FILE_SIZE {
            continue;
        }
        out.push(FileEntry {
            rel: rel.to_string_lossy().replace('\\', "/"),
            size,
        });
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// Resolves a project-relative path to inside the project root: rejects absolute paths and `..` escapes.
/// read/outline and other "fetch corpus by path" entry points all go through here, as the minimal hygiene line.
pub fn resolve_in_root(root: &Path, rel: &str) -> Result<PathBuf> {
    let trimmed = rel.trim_start_matches("./");
    let rel_path = Path::new(trimmed);
    if rel_path.is_absolute() {
        return Err(Error::Config(format!(
            "path must be project-relative, got an absolute path: {rel}"
        )));
    }
    let mut resolved = root.to_path_buf();
    for component in rel_path.components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(Error::Config(format!(
                    "path must not escape the project root (..): {rel}"
                )));
            }
            _ => {
                return Err(Error::Config(format!(
                    "path contains an illegal component: {rel}"
                )));
            }
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn walk_respects_gitignore_and_hidden_and_mindctx() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "src/main.rs", "fn main() {}");
        write(root, "notes.md", "doc");
        write(root, "generated.bin", &"x".repeat(1024));
        write(root, ".gitignore", "generated.bin\n");
        write(root, ".mindctx/index.db", "junk");
        fs::write(root.join(".hidden"), "nope").unwrap();

        let files = walk(root).unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, ["notes.md", "src/main.rs"]);
    }

    #[test]
    fn resolve_rejects_escape_and_absolute() {
        let root = Path::new("/tmp/proj");
        assert!(resolve_in_root(root, "src/main.rs").is_ok());
        assert!(resolve_in_root(root, "./src/main.rs").is_ok());
        assert!(resolve_in_root(root, "../outside").is_err());
        assert!(resolve_in_root(root, "/etc/passwd").is_err());
        assert!(resolve_in_root(root, "a/../../b").is_err());
    }
}
