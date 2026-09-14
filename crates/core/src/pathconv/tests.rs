// SPDX-License-Identifier: MIT OR Apache-2.0

//! Unit tests for the private pathconv internals. Public-API resolution
//! behavior lives in `crates/tests/core/path_resolution.rs`.

use super::*;

// -- `~` expansion ------------------------------------------------------

#[test]
fn tilde_expands_only_prefix_hits() {
    assert_eq!(expand_tilde_with(Some("/home/u"), "~"), "/home/u");
    assert_eq!(expand_tilde_with(Some("/home/u"), "~/x"), "/home/u/x");
    // Not a prefix hit: a relative file name that starts with `~`.
    assert_eq!(expand_tilde_with(Some("/home/u"), "~foo"), "~foo");
    assert_eq!(expand_tilde_with(Some("/home/u"), "a/~/b"), "a/~/b");
    // No home available: identity.
    assert_eq!(expand_tilde_with(None, "~/x"), "~/x");
}

#[test]
fn tilde_public_form_never_touches_non_tilde_input() {
    assert_eq!(expand_tilde("src/main.rs"), "src/main.rs");
    assert_eq!(expand_tilde("~nope.rs"), "~nope.rs");
}

// -- WSL drive + UNC conversion (pure string core) ----------------------

#[test]
fn drive_letter_maps_to_default_mnt_layout() {
    assert_eq!(
        convert_windows_form("C:/Users/foo", "/mnt"),
        "/mnt/c/Users/foo"
    );
    assert_eq!(
        convert_windows_form("C:\\Users\\foo", "/mnt"),
        "/mnt/c/Users/foo"
    );
    assert_eq!(convert_windows_form("C:\\", "/mnt"), "/mnt/c/");
    // Drive letter is lowercased.
    assert_eq!(convert_windows_form("D:/Data", "/mnt"), "/mnt/d/Data");
}

#[test]
fn drive_letter_maps_to_legacy_layout_without_mount_base() {
    assert_eq!(convert_windows_form("C:/Users/foo", ""), "/c/Users/foo");
    assert_eq!(convert_windows_form("C:\\Users\\foo", ""), "/c/Users/foo");
}

#[test]
fn drive_relative_form_is_left_alone() {
    // `C:foo` (no separator after the colon) is a drive-relative path, not a
    // mountable absolute form.
    assert_eq!(convert_windows_form("C:foo", "/mnt"), "C:foo");
}

#[test]
fn wsl_unc_prefixes_are_stripped() {
    assert_eq!(
        convert_windows_form("\\\\wsl$\\Ubuntu-22.04\\home\\me\\a.rs", "/mnt"),
        "/home/me/a.rs"
    );
    assert_eq!(
        convert_windows_form("\\\\wsl.localhost\\Ubuntu\\var\\log", "/mnt"),
        "/var/log"
    );
    // Case-insensitive prefix match; distro missing a tail collapses to the root.
    assert_eq!(convert_windows_form("\\\\WSL$\\Ubuntu", "/mnt"), "/");
}

#[test]
fn other_unc_shares_are_untouched() {
    assert_eq!(
        convert_windows_form("\\\\server\\share\\file.txt", "/mnt"),
        "\\\\server\\share\\file.txt"
    );
}

#[test]
fn posix_input_passes_through_unchanged() {
    assert_eq!(
        convert_windows_form("/home/me/a.rs", "/mnt"),
        "/home/me/a.rs"
    );
    assert_eq!(convert_windows_form("src/main.rs", "/mnt"), "src/main.rs");
    assert_eq!(convert_windows_form(".", "/mnt"), ".");
}

#[test]
fn plain_windows_separators_become_slashes() {
    assert_eq!(convert_windows_form("src\\foo.rs", "/mnt"), "src/foo.rs");
}

// -- non-WSL backslash fallback -----------------------------------------

#[test]
fn fallback_fires_only_on_miss_with_backslash() {
    let miss = || false;
    let hit = || true;
    // Non-WSL: the retry fires on a primary miss carrying Windows separators.
    assert_eq!(
        backslash_fallback_with(false, miss, "src\\foo.rs"),
        Some("src/foo.rs".to_string())
    );
    // WSL never retries: its conversion runs proactively instead.
    assert_eq!(backslash_fallback_with(true, miss, "src\\foo.rs"), None);
    // Primary exists on disk: no retry.
    assert_eq!(backslash_fallback_with(false, hit, "src\\foo.rs"), None);
    // Backslash-free input: no retry.
    assert_eq!(backslash_fallback_with(false, miss, "src/foo.rs"), None);
}

#[test]
fn fallback_never_probes_disk_without_need() {
    let mut stats = 0;
    let counted = || {
        stats += 1;
        false
    };
    // No backslash: the existence probe must be skipped entirely.
    assert_eq!(backslash_fallback_with(false, counted, "src/foo.rs"), None);
    assert_eq!(stats, 0);
}
