// SPDX-License-Identifier: MIT OR Apache-2.0

//! Receipt structure for tracking applied changes.
//!
//! Receipt format version 2: adds `applied_sha256` per file record so unapply can
//! detect post-apply user edits and apply the ownership rule (selective removal instead of
//! whole-file restore). Pre-release; no v1 compatibility is kept.

use serde::{Deserialize, Serialize};

/// Record of a single managed file in the receipt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRecord {
    /// Path of the managed file: relative to home, or absolute for files outside home
    /// (e.g. a project-root AGENTS.md).
    pub path: String,
    /// Whether the file existed before the first apply that managed it. `false` means apply
    /// created the file and unapply must delete it.
    pub original_existed: bool,
    /// SHA256 of the original (pre-apply) bytes; SHA256 of empty bytes when apply created it.
    pub original_sha256: String,
    /// SHA256 of the bytes apply wrote. Unapply compares it against the file's current bytes to
    /// decide between byte-exact restore and selective removal (ownership rule).
    pub applied_sha256: String,
    /// Home-relative path to the backup file; empty when apply created the file.
    pub backup_path: String,
}

/// Receipt file that tracks all applied changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    /// Receipt format version (2).
    pub version: u32,
    /// Absolute path to the mindctx binary.
    pub binary_path: String,
    /// SHA256 hash of the binary.
    pub binary_sha256: String,
    /// Token budget that was set (None = runtime default).
    pub budget: Option<u64>,
    /// Records of all managed files.
    pub files: Vec<FileRecord>,
}

impl Receipt {
    /// Create a new empty receipt.
    pub fn new() -> Self {
        Self {
            version: 2,
            binary_path: String::new(),
            binary_sha256: String::new(),
            budget: None,
            files: Vec::new(),
        }
    }
}

impl Default for Receipt {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the SHA256 hex digest of bytes. Single shared helper (the duplicate
/// private copy in apply.rs was removed in favor of this one).
pub fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}
