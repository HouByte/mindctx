// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sealed single-open snapshot: a file's bytes are read once and identity-checked
//! (length + mtime before AND after the read), so every downstream consumer of one [`Snapshot`]
//! sees byte-identical content — no re-read, no TOCTOU drift inside a single tool call.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use crate::budget;
use crate::encoding::{Decision, EncodingOutcome, decide};
use crate::error::{Error, Result};

/// Bytes + metadata sealed by a single open-read-stat cycle.
#[derive(Debug)]
pub struct Snapshot {
    /// Full file content, sealed at open time.
    pub bytes: Vec<u8>,
    /// Post-read metadata (the identity the seal was checked against).
    pub meta: fs::Metadata,
}

impl Snapshot {
    /// Open once, read to end, stat before AND after the read; if the file's identity
    /// (length + mtime) changed between the two stats → [`Error::FileChanged`].
    pub fn open(path: &Path, tool: budget::ToolKind) -> Result<Snapshot, Error> {
        let before = fs::metadata(path)?;
        let mut bytes = Vec::new();
        File::open(path)?.read_to_end(&mut bytes)?;
        let after = fs::metadata(path)?;
        if identity_changed(&before, &after) {
            return Err(Error::FileChanged {
                path: path.display().to_string(),
                tool,
            });
        }
        Ok(Snapshot { bytes, meta: after })
    }

    /// Validate encoding over the sealed bytes; identical bytes are guaranteed for all downstream
    /// consumers of this Snapshot. The decision ladder owns the semantics; this
    /// mapping only translates its verdicts onto the stable [`EncodingOutcome`] contract.
    pub fn validate_encoding(&self) -> Result<EncodingOutcome<'_>, Error> {
        Ok(match decide(&self.bytes) {
            Decision::Text {
                decoded,
                notes,
                fallback,
            } => EncodingOutcome::Text {
                decoded,
                notes,
                fallback_used: fallback.map(str::to_string),
            },
            Decision::Binary { .. } => EncodingOutcome::Binary,
            Decision::Rejected { report } => EncodingOutcome::Skipped {
                reason: report.skip_reason(),
            },
        })
    }
}

/// Pure identity comparison: length + modification time. A `modified` failure is treated as
/// changed (conservative): an unreadable timestamp is indistinguishable from a fresh write, and
/// the failure mode of a false positive (one retried request) is far cheaper than a false
/// negative (inconsistent bytes inside a sealed snapshot).
pub(crate) fn identity_changed(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    if before.len() != after.len() {
        return true;
    }
    match (before.modified(), after.modified()) {
        (Ok(before_time), Ok(after_time)) => before_time != after_time,
        // Unreadable timestamps are treated as changed (see doc comment).
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn open_seals_bytes_and_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.txt");
        fs::write(&path, b"hello world").unwrap();
        let snap = Snapshot::open(&path, budget::ToolKind::Read).unwrap();
        assert_eq!(snap.bytes, b"hello world");
        assert_eq!(snap.meta.len(), 11);
        assert!(snap.meta.is_file());
    }

    #[test]
    fn open_missing_file_is_io_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = Snapshot::open(&tmp.path().join("nope.txt"), budget::ToolKind::Read).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "got: {err:?}");
    }

    /// The changed-file error is user-facing contract: its Display is frozen verbatim.
    #[test]
    fn file_changed_error_display_is_exact_contract() {
        use crate::budget::ToolKind;
        let err = Error::FileChanged {
            path: "src/lib.rs".to_string(),
            tool: ToolKind::Search,
        };
        assert_eq!(
            err.to_string(),
            "File changed while reading: src/lib.rs. Retry the request."
        );
    }

    #[test]
    fn identity_unchanged_for_same_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.txt");
        fs::write(&path, b"stable").unwrap();
        let before = fs::metadata(&path).unwrap();
        let after = fs::metadata(&path).unwrap();
        assert!(!identity_changed(&before, &after));
    }

    #[test]
    fn identity_changed_on_length_change() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        fs::write(&a, b"short").unwrap();
        fs::write(&b, b"a much longer body").unwrap();
        let before = fs::metadata(&a).unwrap();
        let after = fs::metadata(&b).unwrap();
        assert!(identity_changed(&before, &after));
    }

    /// Lengths match, only mtime differs — the branch the length test cannot reach. `set_modified`
    /// forces deterministic distinct timestamps (no sleeps, no filesystem granularity bets).
    #[test]
    fn identity_changed_on_mtime_only_change() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        fs::write(&a, b"same").unwrap();
        fs::write(&b, b"same").unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        File::options()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(base)
            .unwrap();
        File::options()
            .write(true)
            .open(&b)
            .unwrap()
            .set_modified(base + Duration::from_secs(60))
            .unwrap();
        let before = fs::metadata(&a).unwrap();
        let after = fs::metadata(&b).unwrap();
        assert_eq!(before.len(), after.len(), "fixture lengths must match");
        assert!(
            identity_changed(&before, &after),
            "an mtime-only change is a changed identity"
        );
        assert!(
            !identity_changed(&before, &fs::metadata(&a).unwrap()),
            "the same file stated twice is still unchanged"
        );
    }

    #[test]
    fn encoding_valid_utf8_is_text_with_no_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.txt");
        fs::write(&path, "héllo\n").unwrap();
        let snap = Snapshot::open(&path, budget::ToolKind::Read).unwrap();
        match snap.validate_encoding().unwrap() {
            EncodingOutcome::Text {
                decoded,
                notes,
                fallback_used,
            } => {
                assert_eq!(decoded, "héllo\n");
                assert!(notes.is_empty());
                assert_eq!(fallback_used, None);
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    /// Full ladder: NUL-in-head → Binary; low-evidence legacy bytes stay rejected as ambiguous
    /// windows-1252 decodes 0xE9 cleanly as "é" but the ladder never guesses), not "undecodable".
    #[test]
    fn encoding_ladder_binary_and_ambiguous_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("b.bin");
        fs::write(&bin, [0x68, 0x69, 0x00, 0xFF]).unwrap();
        let snap = Snapshot::open(&bin, budget::ToolKind::Read).unwrap();
        assert_eq!(snap.validate_encoding().unwrap(), EncodingOutcome::Binary);

        let legacy = tmp.path().join("c.txt");
        fs::write(&legacy, [0xE9]).unwrap();
        let snap = Snapshot::open(&legacy, budget::ToolKind::Read).unwrap();
        assert_eq!(
            snap.validate_encoding().unwrap(),
            EncodingOutcome::Skipped {
                reason: "ambiguous: windows-1252".to_string()
            }
        );
    }
}
