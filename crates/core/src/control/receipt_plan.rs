// SPDX-License-Identifier: MIT OR Apache-2.0

//! Receipt planning and the receipt/backup path helpers shared with unapply.

use std::path::{Component, Path, PathBuf};

use super::apply::{Action, ApplyOpts, ChangeSet, FileChange};
use super::fsatomic::read_file_or_empty;
use crate::control::receipt::{FileRecord, Receipt, sha256};
use crate::error::{Error, Result};

/// Receipt file mode: user-private.
const RECEIPT_MODE: u32 = 0o600;

/// Plan receipt creation and backup storage.
///
/// Pure: backups are only recorded on the changes (`backup_target`); `commit` materializes them.
/// Records for managed files this run does not touch are carried over from the
/// existing receipt, and the still-intact original backup of a re-planned file is reused, so a
/// second apply never destroys restore information.
pub(crate) fn plan_receipt(home: &Path, opts: &ApplyOpts, set: &mut ChangeSet) -> Result<()> {
    let record_dir = home.join(".mindctx/record");
    let backup_dir = record_dir.join("backup");
    let receipt_path = record_dir.join("receipt.json");

    set.create_dirs.push(record_dir.clone());
    set.create_dirs.push(backup_dir.clone());

    let (receipt_existed, receipt_bytes) = read_file_or_empty(&receipt_path)?;
    let existing: Option<Receipt> = if receipt_existed {
        Some(
            serde_json::from_slice(&receipt_bytes)
                .map_err(|e| Error::Config(format!("failed to parse existing receipt: {e}")))?,
        )
    } else {
        None
    };

    // Use the binary path from opts (injected at CLI entry, never env::current_exe() in library code)
    let binary_bytes = std::fs::read(&opts.binary_path).map_err(|e| {
        Error::Internal(format!("failed to read binary {:?}: {e}", opts.binary_path))
    })?;

    // Backup numbering continues after the highest existing backup.
    let mut next_backup = next_backup_index(&backup_dir);

    let mut records: Vec<FileRecord> = Vec::new();
    // Collected outside the iter_mut() loop (it borrows set.files) and merged afterwards.
    let mut reapply_warnings: Vec<String> = Vec::new();
    for change in set.files.iter_mut() {
        if change.action != Action::Write {
            continue;
        }
        let path_str = record_path_string(home, &change.target)?;
        let applied_sha256 = sha256(change.new_bytes.as_deref().unwrap_or(&[]));
        let prev = existing.as_ref().and_then(|r| {
            r.files
                .iter()
                .find(|f| resolve_record_path(home, &f.path) == change.target)
        });

        // The file may have been hand-edited since the previous apply: this
        // apply is about to overwrite those edits. Warn unless the current bytes are exactly
        // what the last apply wrote or the user manually restored the pre-apply original.
        if let Some(p) = prev {
            let current_sha = sha256(&change.original_bytes);
            if current_sha != p.applied_sha256 && current_sha != p.original_sha256 {
                reapply_warnings.push(format!(
                    "{path_str} changed since the last apply; re-applying overwrites the current contents"
                ));
            }
        }

        // Reuse the previous original backup when it is still intact: the true pre-apply bytes
        // survive any number of re-applies.
        let reusable = prev.filter(|p| {
            p.original_existed
                && !p.backup_path.is_empty()
                && std::fs::read(resolve_record_path(home, &p.backup_path))
                    .map(|bytes| sha256(&bytes) == p.original_sha256)
                    .unwrap_or(false)
        });

        let record = if let Some(p) = reusable {
            change.backup_target = None;
            FileRecord {
                path: path_str,
                original_existed: true,
                original_sha256: p.original_sha256.clone(),
                applied_sha256,
                backup_path: p.backup_path.clone(),
            }
        } else if prev.map(|p| !p.original_existed).unwrap_or(false) {
            // Apply created this file earlier; it remains a created file (deleted on unapply).
            change.backup_target = None;
            FileRecord {
                path: path_str,
                original_existed: false,
                original_sha256: sha256(b""),
                applied_sha256,
                backup_path: String::new(),
            }
        } else if change.original_existed {
            let backup_path = backup_dir.join(next_backup.to_string());
            next_backup += 1;
            change.backup_target = Some(backup_path.clone());
            FileRecord {
                path: path_str,
                original_existed: true,
                original_sha256: sha256(&change.original_bytes),
                applied_sha256,
                backup_path: record_path_string(home, &backup_path)?,
            }
        } else {
            // Created by this apply: no backup; unapply deletes the file.
            change.backup_target = None;
            FileRecord {
                path: path_str,
                original_existed: false,
                original_sha256: sha256(b""),
                applied_sha256,
                backup_path: String::new(),
            }
        };
        records.push(record);
    }
    for warning in reapply_warnings {
        set.warn_once(warning);
    }

    // Carry over records for managed files this run does not touch, so a second apply keeps the
    // receipt complete.
    if let Some(prev) = &existing {
        for p in &prev.files {
            let target = resolve_record_path(home, &p.path);
            if !records
                .iter()
                .any(|r| resolve_record_path(home, &r.path) == target)
            {
                records.push(p.clone());
            }
        }
    }

    let receipt = Receipt {
        version: 2,
        binary_path: opts.binary_path.to_string_lossy().to_string(),
        binary_sha256: sha256(&binary_bytes),
        budget: opts.budget,
        files: records,
    };
    let mut new_bytes = serde_json::to_vec_pretty(&receipt)
        .map_err(|e| Error::Config(format!("failed to serialize receipt: {e}")))?;
    new_bytes.push(b'\n');

    // Fully idempotent run with no previous receipt: create nothing.
    let has_file_changes = !set.files.is_empty();
    if !has_file_changes && !receipt_existed {
        return Ok(());
    }
    // Idempotent run with an unchanged merged receipt: don't rewrite it.
    if receipt_existed && new_bytes == receipt_bytes {
        return Ok(());
    }

    // Execute the receipt write FIRST among file changes (commit writes dirs, then all
    // backups, then the changes): a crash after the backups are materialized leaves a valid
    // receipt + intact backups on disk no matter how many target writes landed, so unapply
    // always stays possible (the receipt was appended last, leaving a window
    // where targets were modified with no receipt on disk).
    set.files.insert(
        0,
        FileChange {
            target: receipt_path,
            action: Action::Write,
            original_bytes: receipt_bytes,
            new_bytes: Some(new_bytes),
            original_existed: receipt_existed,
            backup_target: None,
            mode: Some(RECEIPT_MODE),
        },
    );

    Ok(())
}

/// Receipt record path string: home-relative when possible, absolute for files outside home
/// (project-root markers). Paths are emitted with forward slashes so the receipt is portable
/// across platforms (Windows would otherwise serialize `\` separators, breaking unapply on a
/// different host).
pub(crate) fn record_path_string(home: &Path, target: &Path) -> Result<String> {
    let raw = if let Ok(rel) = target.strip_prefix(home) {
        rel.to_string_lossy()
    } else {
        target.to_string_lossy()
    };
    Ok(raw.replace('\\', "/"))
}

/// Resolve a receipt record path against home (absolute entries are used as-is; apply records
/// project files outside home as absolute).
pub(crate) fn resolve_record_path(home: &Path, path_str: &str) -> PathBuf {
    let p = Path::new(path_str);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        home.join(p)
    }
}

/// Reject `..` traversal in receipt-driven paths.
pub(crate) fn validate_record_path(path_str: &str) -> Result<()> {
    if Path::new(path_str)
        .components()
        .any(|c| c == Component::ParentDir)
    {
        return Err(Error::Config(format!(
            "receipt path rejected (parent traversal): {path_str}"
        )));
    }
    Ok(())
}

/// Next free backup index: max existing numeric backup filename + 1.
pub(crate) fn next_backup_index(backup_dir: &Path) -> u64 {
    std::fs::read_dir(backup_dir)
        .map(|entries| {
            entries
                .filter_map(std::result::Result::ok)
                .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u64>().ok()))
                .max()
                .map_or(0, |max| max + 1)
        })
        .unwrap_or(0)
}
