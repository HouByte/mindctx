// SPDX-License-Identifier: MIT OR Apache-2.0

//! Index layer: walker (gitignore-aware). Current surface — corpus enumeration for rg-layer queries +
//! path resolution for tool inputs (project-relative or absolute; see [`resolve_path`]).

use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};
use crate::pathconv;

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

/// Resolves a caller-supplied path to an absolute [`PathBuf`]. Relative inputs join
/// the (canonicalized, absolute) project root; absolute inputs, `~` forms, and —
/// inside WSL — Windows-form inputs resolve to anywhere on the filesystem. `.` and
/// `..` are normalized lexically: `..` pops at most to the filesystem root and never
/// escapes above it. No canonicalization and no symlink following: the result is the
/// pure lexical join. read/outline/search/glob "fetch by path" entry points all go
/// through here, as the single resolution choke point.
pub fn resolve_path(root: &Path, input: &str) -> Result<PathBuf> {
    // WSL conversion runs first: it turns Windows-form separators into slashes,
    // so a `~\x` input becomes the expandable `~/x` form.
    let normalized = pathconv::normalize_wsl_input(input);
    let expanded = pathconv::expand_tilde(&normalized);
    let primary = lexical_resolve(root, input, &expanded)?;
    // Backslash fallback (non-WSL): Unix file names may legally contain `\`, so the
    // slash-normalized retry runs only when the primary parse missed on disk. The
    // existence probe is lazy — backslash-free inputs never stat.
    let Some(retry) = pathconv::backslash_fallback(|| primary.exists(), &expanded) else {
        return Ok(primary);
    };
    lexical_resolve(root, input, &retry)
}

/// Lexical component walk: `path` (already tilde/WSL-normalized) resolves against
/// `root` when relative and replaces it when absolute. `input` is the original user
/// string, kept for error messages. Purely lexical — no filesystem access.
/// Precondition: `root` is canonicalized and absolute (callers canonicalize at
/// startup); the relative branch joins it verbatim, without re-normalizing.
fn lexical_resolve(root: &Path, input: &str, path: &str) -> Result<PathBuf> {
    // No `./` trimming here: a leading `./` (or `.//`) makes `CurDir` the first
    // component, which the walks below skip. Trimming instead would turn `.//a`
    // into `/a` and silently flip it onto the absolute branch.
    let rel = Path::new(path);

    if !rel.is_absolute() {
        // Relative inputs join the root buffer directly: push/pop over its
        // components, no per-component re-walk of the root.
        let mut resolved = root.to_path_buf();
        for component in rel.components() {
            match component {
                Component::Normal(part) => resolved.push(part),
                Component::CurDir => {}
                // Clamped at the root's own anchor: `..` pops at most to the
                // filesystem root (`pop` is a no-op there) and never above it.
                Component::ParentDir => {
                    resolved.pop();
                }
                // A drive prefix or a root separator in a relative path (e.g. the
                // Windows drive-relative `C:foo`) has no relative meaning here.
                _ => {
                    return Err(Error::Config(format!(
                        "path contains an illegal component: {input}"
                    )));
                }
            }
        }
        return Ok(resolved);
    }

    // (drive prefix, anchored-at-root, component stack)
    let mut prefix: Option<std::ffi::OsString> = None;
    let mut parts: Vec<std::ffi::OsString> = Vec::new();

    for component in rel.components() {
        match component {
            Component::Prefix(p) => prefix = Some(p.as_os_str().to_os_string()),
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                // No-op once the filesystem root is reached: `..` never escapes above it.
                parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_os_string()),
        }
    }

    let mut resolved = PathBuf::new();
    if let Some(p) = prefix {
        resolved.push(p);
    }
    resolved.push(std::path::MAIN_SEPARATOR.to_string());
    for part in parts {
        resolved.push(part);
    }
    Ok(resolved)
}
