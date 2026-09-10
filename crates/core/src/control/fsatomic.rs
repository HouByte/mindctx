// SPDX-License-Identifier: MIT OR Apache-2.0

//! Filesystem read/atomic-write primitives shared by the apply/unapply planning steps.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};

/// Record/backup directory mode: user-private.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Read a file, returning (existed, bytes). Only NotFound means absent; every other I/O error is
/// propagated so unreadable configs are never overwritten.
pub(crate) fn read_file_or_empty(path: &Path) -> Result<(bool, Vec<u8>)> {
    match std::fs::read(path) {
        Ok(bytes) => Ok((true, bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((false, Vec::new())),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Global counter for unique tempfile naming (avoids uuid dependency for temp file generation).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write a file atomically using tempfile + fsync + rename, with an optional unix mode.
/// A replacement never widens an existing target: when no explicit mode is given and the
/// target exists, its permission mode is applied to the tempfile before the rename (a
/// 0600 credential file must not land as 0644). New files keep the process default.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    // Create parent directory if needed
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Generate unique tempfile name using counter
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let tempfile_path = path.with_extension(format!("{}.{}.tmp", pid, counter));

    // Explicit mode wins; otherwise an existing target's mode is inherited onto the
    // tempfile. Applied after open (not via OpenOptions::mode): creation masks the mode
    // with the process umask, set_permissions does not.
    #[cfg(unix)]
    let mode = mode.or_else(|| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .map(|meta| meta.permissions().mode() & 0o777)
    });

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut file = options.open(&tempfile_path)?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    file.write_all(bytes)?;
    // fsync before rename: a crash must not leave a short/zero-length target.
    file.sync_all()?;
    drop(file);

    // Atomic rename
    std::fs::rename(&tempfile_path, path).map_err(|e| {
        // Clean up tempfile on failure
        let _ = std::fs::remove_file(&tempfile_path);
        Error::Io(e)
    })?;

    Ok(())
}

/// Create a directory and restrict it to the owner (0700 on unix).
#[cfg(unix)]
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(PRIVATE_DIR_MODE))?;
    Ok(())
}

/// Create a directory (no permission restriction on non-unix platforms).
#[cfg(not(unix))]
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    /// write_atomically directly: an existing target's mode is inherited; a new file
    /// keeps the default; an explicit mode wins over both (unix only).
    #[cfg(unix)]
    #[test]
    fn write_atomically_mode_inheritance() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;

        // Existing target 0640: replacement keeps 0640 even with a restrictive umask.
        let target = tmp.path().join("existing.json");
        fs::write(&target, b"old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        write_atomically(&target, b"new", None).unwrap();
        assert_eq!(mode(&target), 0o640);

        // New file: no mode given, nothing to inherit — process default applies.
        let fresh = tmp.path().join("fresh.json");
        write_atomically(&fresh, b"new", None).unwrap();
        assert_eq!(mode(&fresh) & 0o600, 0o600);

        // Explicit mode wins over the inherited one.
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        write_atomically(&target, b"newer", Some(0o600)).unwrap();
        assert_eq!(mode(&target), 0o600);
    }
}
