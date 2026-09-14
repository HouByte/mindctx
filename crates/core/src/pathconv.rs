// SPDX-License-Identifier: MIT OR Apache-2.0

//! Path-input conversion helpers behind [`crate::index::resolve_path`]: `~` expansion,
//! WSL Windows-form input normalization, and the non-WSL backslash fallback.
//!
//! All conversion is input-side only: results, continuation cursors, and rendered
//! paths stay in the runtime's own (POSIX) form.

use std::borrow::Cow;
use std::path::Path;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// `~` expansion
// ---------------------------------------------------------------------------

/// Expands a leading `~`/`~/` to the user's home directory. Everything else —
/// including `~user` and `~foo` — passes through unchanged, so legitimate relative
/// file names starting with `~` are never touched. Non-tilde inputs borrow.
pub(crate) fn expand_tilde(input: &str) -> Cow<'_, str> {
    expand_tilde_with(home_dir().as_deref(), input)
}

/// The expansion core, parameterized on the home directory for testability.
fn expand_tilde_with<'a>(home: Option<&str>, input: &'a str) -> Cow<'a, str> {
    let Some(home) = home else {
        return Cow::Borrowed(input);
    };
    if input == "~" {
        Cow::Owned(home.to_string())
    } else if let Some(rest) = input.strip_prefix("~/") {
        Cow::Owned(format!("{home}/{rest}"))
    } else {
        Cow::Borrowed(input)
    }
}

/// The `~` expansion source. `None` when the platform home directory cannot be
/// determined — tilde inputs then pass through untouched.
fn home_dir() -> Option<String> {
    home::home_dir().map(|path| path.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// WSL detection
// ---------------------------------------------------------------------------

/// One-shot WSL probe: the distro env var, or a `microsoft` marker in the kernel
/// identity files. Any single hit wins.
fn detect_wsl() -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_some() {
        return true;
    }
    for probe in ["/proc/version", "/proc/sys/kernel/osrelease"] {
        if let Ok(content) = std::fs::read_to_string(probe)
            && content.to_ascii_lowercase().contains("microsoft")
        {
            return true;
        }
    }
    false
}

/// Whether the process runs inside WSL. Judged once at first use and cached.
pub(crate) fn wsl_active() -> bool {
    static WSL: OnceLock<bool> = OnceLock::new();
    *WSL.get_or_init(detect_wsl)
}

/// Resolves the WSL drive mount base: `/mnt` on the default automount layout, the
/// empty string for legacy distros that mount drives at `/<letter>` directly. Probed
/// once on the default `c` mount; falls back to `/mnt` when neither layout is visible.
fn drive_base() -> &'static str {
    static BASE: OnceLock<&'static str> = OnceLock::new();
    BASE.get_or_init(|| {
        if Path::new("/mnt/c").is_dir() {
            "/mnt"
        } else if Path::new("/c").is_dir() {
            ""
        } else {
            "/mnt"
        }
    })
}

// ---------------------------------------------------------------------------
// WSL input conversion
// ---------------------------------------------------------------------------

/// Normalizes a caller-supplied path for the WSL runtime: Windows forms (drive
/// letters, backslashes, `\\wsl$`/`\\wsl.localhost` UNC introspection prefixes) are
/// rewritten to the WSL POSIX form. Identity outside WSL — Unix file names may
/// legally contain `\`, so conversion there happens only as the resolve-time
/// fallback (see [`backslash_fallback`]). Non-WSL inputs borrow.
pub(crate) fn normalize_wsl_input(input: &str) -> Cow<'_, str> {
    if wsl_active() {
        Cow::Owned(convert_windows_form(input, drive_base()))
    } else {
        Cow::Borrowed(input)
    }
}

/// The WSL conversion core, parameterized on the drive mount base for testability.
///
/// Rules, in priority order:
/// 1. `\\wsl$\<distro>\x` / `\\wsl.localhost\<distro>\x` → `/x` (prefix + distro stripped)
/// 2. any other UNC (`\\server\share`) → untouched; downstream resolution reports the miss
/// 3. `C:/x` or `C:\x` → `<drive_base>/c/x` (drive letter lowercased); `C:foo`
///    (drive-relative, no separator) is left alone
/// 4. remaining Windows separators → `/`
fn convert_windows_form(input: &str, drive_base: &str) -> String {
    let lower = input.to_ascii_lowercase();
    for prefix in ["\\\\wsl.localhost\\", "\\\\wsl$\\"] {
        if lower.starts_with(prefix) {
            let after_distro = input[prefix.len()..]
                .split_once('\\')
                .map(|(_, tail)| tail)
                .unwrap_or("");
            return format!("/{}", after_distro.replace('\\', "/"));
        }
    }
    if input.starts_with("\\\\") {
        return input.to_string();
    }
    let bytes = input.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
    {
        let letter = (bytes[0] as char).to_ascii_lowercase();
        let rest = input[3..].replace('\\', "/");
        return format!("{drive_base}/{letter}/{rest}");
    }
    input.replace('\\', "/")
}

// ---------------------------------------------------------------------------
// Non-WSL backslash fallback
// ---------------------------------------------------------------------------

/// The non-WSL backslash fallback: returns the slash-normalized retry input when the
/// primary resolution missed on disk and the normalized input carries Windows
/// separators; `None` otherwise (no retry). WSL never lands here — its conversion
/// runs proactively. `primary_exists` is a lazy probe and is never consulted on the
/// WSL or backslash-free paths, so inputs without `\` cost no stat call.
pub(crate) fn backslash_fallback(
    primary_exists: impl FnOnce() -> bool,
    normalized: &str,
) -> Option<String> {
    backslash_fallback_with(wsl_active(), primary_exists, normalized)
}

/// The fallback predicate core, parameterized on the WSL verdict for testability.
/// Short-circuit order matters: the `primary_exists` probe runs last, only when the
/// retry can actually fire.
fn backslash_fallback_with(
    wsl: bool,
    primary_exists: impl FnOnce() -> bool,
    normalized: &str,
) -> Option<String> {
    if wsl || !normalized.contains('\\') || primary_exists() {
        return None;
    }
    Some(normalized.replace('\\', "/"))
}

#[cfg(test)]
mod tests;
