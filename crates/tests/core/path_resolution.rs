// SPDX-License-Identifier: MIT OR Apache-2.0

//! Public-API behavior of `mindctx_core::index::resolve_path` — the single
//! resolution choke point shared by read/search/glob/outline. Cases here lock
//! consumer-visible resolution semantics; private-logic unit tests live in
//! sibling `tests.rs` files next to their module under `crates/core`.

use std::fs;
use std::path::Path;

use mindctx_core::budget::DEFAULT_TOKEN_BUDGET;
use mindctx_core::index::resolve_path;
use mindctx_core::retrieve::glob_args::GlobArgs;
use mindctx_core::retrieve::glob_tool::{GlobParams, glob_with_budget};
use mindctx_core::retrieve::read::{ReadParams, read_with_budget};
use mindctx_core::retrieve::{SearchOutput, SearchParams, search_with_budget};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Reads `path` (as returned by a search/glob page) back through the same
/// server root and returns the rendered page text.
fn read_back(root: &Path, path: &str) -> String {
    read_with_budget(
        root,
        &ReadParams {
            file_path: Some(path.to_string()),
            files: None,
            offset: None,
            limit: None,
            encoding: None,
        },
        DEFAULT_TOKEN_BUDGET,
    )
    .unwrap_or_else(|e| panic!("read-back of {path} failed: {e}"))
    .text
    .expect("read must render a page")
}

#[test]
fn dot_slash_prefixed_input_with_repeated_separators_stays_relative() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // A naive leading-`./` trim turns `.//a` into `/a` and silently flips the
    // input onto the absolute branch, resolving at the filesystem root.
    let resolved = resolve_path(root, ".//src/main.rs").unwrap();
    assert_eq!(resolved, root.join("src/main.rs"));
}

#[test]
fn resolve_accepts_absolute_and_parent_components() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let resolve = |input: &str| resolve_path(root, input).unwrap();

    // Relative and `./`-prefixed inputs still join the root.
    assert_eq!(resolve("src/main.rs"), root.join("src/main.rs"));
    assert_eq!(resolve("./src/main.rs"), root.join("src/main.rs"));

    // Absolute inputs replace the root instead of being rejected. The joined
    // string may mix separators on Windows; Path equality is component-based.
    let abs = root.join("src/main.rs").to_string_lossy().into_owned();
    assert_eq!(resolve(&abs), root.join("src/main.rs"));

    // `..`/`.` normalize lexically against the root — no escape rejection.
    assert_eq!(
        resolve("../outside"),
        root.parent().unwrap().join("outside")
    );
    assert_eq!(resolve("a/../../b"), root.parent().unwrap().join("b"));
}

#[test]
fn resolve_clamps_parent_walk_at_the_filesystem_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let fs_root = root.ancestors().last().unwrap();
    // Enough `..` to overshoot every ancestor: the walk clamps at the
    // filesystem root instead of escaping above it.
    let overshoot = "../".repeat(root.components().count() + 5);
    assert_eq!(
        resolve_path(root, &format!("{overshoot}b")).unwrap(),
        fs_root.join("b")
    );
}

#[test]
fn resolve_backslash_fallback_retries_with_slashes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "src/foo.rs", "fn main() {}");
    // Primary parse (a literal `\` file name) misses on disk; the slash-normalized
    // retry resolves the real file.
    assert_eq!(
        resolve_path(root, "src\\foo.rs").unwrap(),
        root.join("src/foo.rs")
    );
    // No backslash: the input is never rewritten, even when it misses.
    assert_eq!(
        resolve_path(root, "src/nope.rs").unwrap(),
        root.join("src/nope.rs")
    );
}

#[cfg(windows)]
#[test]
fn resolve_rejects_drive_relative_form() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let error = resolve_path(root, "C:foo").unwrap_err().to_string();
    assert!(
        error.contains("illegal component"),
        "drive-relative form must stay an error: {error}"
    );
}

#[test]
fn inside_wsl_windows_drive_form_resolves_to_the_mount() {
    // Activates only inside a real WSL runtime, where the WSL detection, the
    // drive-base probe (`/mnt` vs `/<letter>`), and the conversion all run
    // against the actual environment end-to-end. Elsewhere this is a no-op.
    if std::env::var_os("WSL_DISTRO_NAME").is_none() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let resolved = resolve_path(tmp.path(), "C:\\Windows\\System32\\drivers\\etc\\hosts")
        .unwrap_or_else(|e| panic!("windows drive form must resolve: {e}"));
    let rendered = resolved.to_string_lossy();
    assert!(
        rendered.starts_with("/mnt/c/") || rendered.starts_with("/c/"),
        "windows drive form must map onto the WSL drive mount, got {rendered}"
    );
}

#[test]
fn inside_wsl_backslash_tilde_expands_after_conversion() {
    // Same WSL gating as the drive-form test. The WSL conversion turns the
    // Windows-form `~\x` into `~/x`, which the tilde expansion then picks up;
    // resolution is lexical, so no file needs to exist on disk.
    if std::env::var_os("WSL_DISTRO_NAME").is_none() {
        return;
    }
    let home = std::env::var("HOME").expect("HOME is set inside WSL");
    let tmp = tempfile::tempdir().unwrap();
    let resolved = resolve_path(tmp.path(), "~\\mindctx-tilde-order-probe")
        .unwrap_or_else(|e| panic!("backslash tilde form must resolve: {e}"));
    assert_eq!(resolved, Path::new(&home).join("mindctx-tilde-order-probe"));
}

// -- outside-root display: returned paths re-resolve to the same file ------
//
// Every fixture plants a decoy inside the server root with the same file name
// as the outside target, so a regression to target-relative display fails
// loudly (the read-back would return the decoy's content).

const OUTSIDE_CONTENT: &str = "SECRET_MARKER: the outside file";
const DECOY_CONTENT: &str = "DECOY: the same-named file inside the server root";

#[test]
fn glob_outside_root_renders_absolute_paths_that_read_back() {
    let proj = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(proj.path(), "leaked.txt", DECOY_CONTENT);
    write(outside.path(), "leaked.txt", OUTSIDE_CONTENT);

    let env = glob_with_budget(
        proj.path(),
        &GlobParams {
            pattern: GlobArgs(vec!["*.txt".to_string()]),
            path: Some(outside.path().to_string_lossy().into_owned()),
            ..GlobParams::default()
        },
        DEFAULT_TOKEN_BUDGET,
    )
    .expect("glob succeeds");
    let text = env.text.as_deref().expect("glob renders a page");
    let abs = outside
        .path()
        .join("leaked.txt")
        .to_string_lossy()
        .replace('\\', "/");
    assert!(
        text.contains(&abs),
        "outside-root glob must render the absolute path {abs}: {text}"
    );

    let page = read_back(proj.path(), &abs);
    assert!(
        page.contains(OUTSIDE_CONTENT),
        "read-back must return the outside file, not the in-root decoy: {page}"
    );
}

#[test]
fn search_outside_root_lists_absolute_paths_that_read_back() {
    let proj = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(proj.path(), "leaked.txt", DECOY_CONTENT);
    write(outside.path(), "leaked.txt", OUTSIDE_CONTENT);

    let env = search_with_budget(
        proj.path(),
        &SearchParams {
            pattern: "SECRET_MARKER".to_string(),
            path: Some(outside.path().to_string_lossy().into_owned()),
            output_mode: SearchOutput::FilesWithMatches,
            ..SearchParams::default()
        },
        DEFAULT_TOKEN_BUDGET,
    )
    .expect("search succeeds");
    let text = env.text.as_deref().expect("search renders a page");
    let abs = outside
        .path()
        .join("leaked.txt")
        .to_string_lossy()
        .replace('\\', "/");
    assert!(
        text.contains(&abs),
        "outside-root search must list the absolute path {abs}: {text}"
    );

    let page = read_back(proj.path(), &abs);
    assert!(
        page.contains(OUTSIDE_CONTENT),
        "read-back must return the outside file, not the in-root decoy: {page}"
    );
}

#[test]
fn search_single_file_outside_root_displays_the_absolute_path() {
    let proj = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(proj.path(), "leaked.txt", DECOY_CONTENT);
    write(outside.path(), "leaked.txt", OUTSIDE_CONTENT);

    let abs = outside
        .path()
        .join("leaked.txt")
        .to_string_lossy()
        .into_owned();
    let env = search_with_budget(
        proj.path(),
        &SearchParams {
            pattern: "SECRET_MARKER".to_string(),
            path: Some(abs.clone()),
            output_mode: SearchOutput::FilesWithMatches,
            ..SearchParams::default()
        },
        DEFAULT_TOKEN_BUDGET,
    )
    .expect("search succeeds");
    let text = env.text.as_deref().expect("search renders a page");
    assert!(
        text.contains(&abs.replace('\\', "/")),
        "single-file outside target must display the absolute path {abs}: {text}"
    );
}

#[test]
fn in_root_subdirectory_target_keeps_target_relative_display() {
    let proj = tempfile::tempdir().unwrap();
    write(proj.path(), "src/inner.txt", "inside the root");

    let env = glob_with_budget(
        proj.path(),
        &GlobParams {
            pattern: GlobArgs(vec!["*.txt".to_string()]),
            path: Some("src".to_string()),
            ..GlobParams::default()
        },
        DEFAULT_TOKEN_BUDGET,
    )
    .expect("glob succeeds");
    let text = env.text.as_deref().expect("glob renders a page");
    let body = text.split("\n\n").next().unwrap();
    assert_eq!(
        body, "inner.txt",
        "in-root subdirectory display stays target-relative: {text}"
    );
}
