// SPDX-License-Identifier: MIT OR Apache-2.0

//! Apply and unapply logic for transactional configuration changes.
//!
//! This module implements the plan/commit split:
//! - `plan_apply` / `plan_unapply`: pure functions that compute what needs to change (reads only;
//!   no filesystem mutation)
//! - `commit`: execution of the planned changes: private dirs, then backups (0600), then the file
//!   changes in plan order (receipt first, so unapply stays possible after partial failure), then
//!   empty-dir cleanup
//!
//! All file-path arguments (home, project_root) are injected explicitly.
//! std::env::current_exe() is only called at the CLI entry point and passed via ApplyOpts.binary_path.

use std::path::{Path, PathBuf};

use crate::control::receipt::{FileRecord, Receipt, sha256};
use crate::error::{Error, Result};

use super::claude_config::{plan_claude_config, strip_claude_mindctx};
use super::codex_config::{plan_codex_config, strip_codex_mindctx};
use super::fsatomic::{ensure_private_dir, read_file_or_empty, write_atomically};
use super::receipt_plan::{plan_receipt, resolve_record_path, validate_record_path};

/// Backup file mode: user-private.
const BACKUP_MODE: u32 = 0o600;

pub const MARKER_BEGIN: &str = "<!-- mindctx:begin (guidance-v1) -->";
pub const MARKER_END: &str = "<!-- mindctx:end -->";

/// Guidance block body: 3 lines.
const GUIDANCE: &str = "\
- Prefer `mindctx search` over ad-hoc grep for cross-file questions; built-in grep is fine for single-file literal matches.
- Run `mindctx outline` before reading files, then read with line ranges to stay inside the token budget.
- When a tool response is truncated, resume from the provided continuation pointer instead of re-issuing the query.
";

/// Action to take on a file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Action {
    Write,
    Delete,
}

/// A single file change in a ChangeSet.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FileChange {
    /// Target file path (absolute).
    pub target: PathBuf,
    /// Action to perform.
    pub action: Action,
    /// Original bytes of the file (before any change). Also the source committed to
    /// `backup_target` when one is set.
    pub original_bytes: Vec<u8>,
    /// New bytes to write (None for Delete action).
    pub new_bytes: Option<Vec<u8>>,
    /// Whether the file existed before the change.
    pub original_existed: bool,
    /// When set, `commit` writes `original_bytes` here (0600) before executing any target write,
    /// so unapply is possible from the first target write on.
    #[serde(default)]
    pub backup_target: Option<PathBuf>,
    /// Unix permission mode for the written file (e.g. 0600 for the receipt); None = default.
    #[serde(default)]
    pub mode: Option<u32>,
}

impl FileChange {
    /// A Write change with defaults for the planning-side fields.
    pub fn write(
        target: PathBuf,
        original_bytes: Vec<u8>,
        new_bytes: Vec<u8>,
        original_existed: bool,
    ) -> Self {
        Self {
            target,
            action: Action::Write,
            original_bytes,
            new_bytes: Some(new_bytes),
            original_existed,
            backup_target: None,
            mode: None,
        }
    }

    /// A Delete change with defaults for the planning-side fields.
    pub fn delete(target: PathBuf, original_bytes: Vec<u8>, original_existed: bool) -> Self {
        Self {
            target,
            action: Action::Delete,
            original_bytes,
            new_bytes: None,
            original_existed,
            backup_target: None,
            mode: None,
        }
    }

    /// Human-readable action name.
    pub fn action_str(&self) -> &'static str {
        match self.action {
            Action::Write => "write",
            Action::Delete => "delete",
        }
    }
}

/// A complete set of planned changes. Produced by `plan_apply`/`plan_unapply`; `commit` executes
/// it in order without recomputation. Fields are public for test construction; treat the set as
/// frozen between planning and commit (the old "immutable" claim was unenforced,
/// so it is now stated as a contract, not a structural guarantee).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ChangeSet {
    /// All file changes in this set, in execution order.
    pub files: Vec<FileChange>,
    /// Directories created before any write (0700 on unix); used for ~/.mindctx/record.
    #[serde(default)]
    pub create_dirs: Vec<PathBuf>,
    /// Directories removed after execution when empty (e.g. record/ left empty by unapply).
    #[serde(default)]
    pub cleanup_dirs: Vec<PathBuf>,
    /// Non-fatal observations for the caller (the CLI prints them).
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl ChangeSet {
    /// Create a new empty ChangeSet.
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            create_dirs: Vec::new(),
            cleanup_dirs: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Add a file change to the set.
    pub fn add(&mut self, change: FileChange) {
        self.files.push(change);
    }

    /// Push a warning unless it is already present.
    pub(crate) fn warn_once(&mut self, message: String) {
        if !self.warnings.contains(&message) {
            self.warnings.push(message);
        }
    }
}

impl Default for ChangeSet {
    fn default() -> Self {
        Self::new()
    }
}

/// Options for apply operation.
#[derive(Debug, Clone, Default)]
pub struct ApplyOpts {
    /// Install Claude Code MCP configuration.
    pub claude: bool,
    /// Install Codex MCP configuration.
    pub codex: bool,
    /// Token budget to set (None = use runtime default of 8500).
    pub budget: Option<u64>,
    /// Skip confirmation (for CLI).
    pub yes: bool,
    /// Absolute path to the mindctx binary (PRODUCTION ONLY: resolved from std::env::current_exe() at CLI entry).
    pub binary_path: PathBuf,
}

/// Plan what needs to be changed for apply - pure function (reads only, no writes).
///
/// - `home`: the host home directory (~) for config files
/// - `project_root`: the project root where AGENTS.md/CLAUDE.md markers are placed
/// - `opts`: apply options including binary_path
pub fn plan_apply(home: &Path, project_root: &Path, opts: &ApplyOpts) -> Result<ChangeSet> {
    let mut set = ChangeSet::new();

    // 1. Plan Claude Code config
    if opts.claude {
        plan_claude_config(home, opts, &mut set)?;
    }

    // 2. Plan Codex config
    if opts.codex {
        plan_codex_config(home, opts, &mut set)?;
    }

    // 3. Plan AGENTS.md markers (uses project_root, not home)
    plan_agents_markers(project_root, &mut set)?;

    // 4. Plan ~/.mindctx/config.toml if absent (before the receipt so it gets recorded)
    plan_mindctx_config(home, &mut set)?;

    // 5. Plan receipt (merges with the existing receipt so re-apply never destroys records)
    plan_receipt(home, opts, &mut set)?;

    Ok(set)
}

/// Plan what needs to be changed for unapply - pure function (reads only, no writes).
///
/// Restore sources and target paths come exclusively from the receipt; the current working
/// directory is never consulted, so unapply only touches recorded paths.
pub fn plan_unapply(home: &Path) -> Result<ChangeSet> {
    let mut set = ChangeSet::new();
    let record_dir = home.join(".mindctx/record");
    let backup_dir = record_dir.join("backup");
    let receipt_path = record_dir.join("receipt.json");

    let receipt_bytes = match std::fs::read(&receipt_path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(set),
        Err(e) => return Err(Error::Io(e)),
    };
    let receipt: Receipt = serde_json::from_slice(&receipt_bytes)
        .map_err(|e| Error::Config(format!("failed to parse receipt: {e}")))?;
    if receipt.version != 2 {
        return Err(Error::Config(format!(
            "unsupported receipt version {} (expected 2)",
            receipt.version
        )));
    }

    // plan_unapply_record reports whether the record's backup was consumed by a planned
    // restoration/removal; unconsumed backups are retained (the backup is the
    // only copy of the user's pre-apply bytes).
    let mut consumed = Vec::with_capacity(receipt.files.len());
    for record in &receipt.files {
        consumed.push(plan_unapply_record(home, &mut set, record)?);
    }

    // Remove consumed backups, then the receipt itself (last, so restore stays possible), then
    // the directories apply created if they end up empty.
    for (record, consumed) in receipt.files.iter().zip(&consumed) {
        if *consumed && !record.backup_path.is_empty() {
            validate_record_path(&record.backup_path)?;
            let backup_path = resolve_record_path(home, &record.backup_path);
            set.add(FileChange::delete(backup_path, Vec::new(), true));
        }
    }

    // Orphaned numbered backups no record references (a re-plan that allocated a fresh index
    // leaves the old file behind) are removed too; retained backups are still referenced by
    // their record and survive.
    let referenced: Vec<PathBuf> = receipt
        .files
        .iter()
        .filter(|r| !r.backup_path.is_empty())
        .map(|r| resolve_record_path(home, &r.backup_path))
        .collect();
    if let Ok(entries) = std::fs::read_dir(&backup_dir) {
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            let numbered = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|s| s.parse::<u64>().is_ok());
            if numbered && path.is_file() && !referenced.contains(&path) {
                set.add(FileChange::delete(path, Vec::new(), true));
            }
        }
    }

    set.add(FileChange::delete(receipt_path, receipt_bytes, true));
    for dir in [backup_dir, record_dir.clone(), home.join(".mindctx")] {
        if !set.cleanup_dirs.contains(&dir) {
            set.cleanup_dirs.push(dir);
        }
    }
    Ok(set)
}

/// Reads and verifies a backup file: validates the path, reads the bytes, and checks
/// the SHA256 against `record.original_sha256`. The divergent empty-path error messages
/// stay at the call sites since they encode different intent.
fn load_verified_backup(home: &Path, record: &FileRecord) -> Result<Vec<u8>> {
    validate_record_path(&record.backup_path)?;
    let backup_path = resolve_record_path(home, &record.backup_path);
    let backup_bytes = std::fs::read(&backup_path)
        .map_err(|e| Error::Config(format!("failed to read backup {backup_path:?}: {e}")))?;
    let computed = sha256(&backup_bytes);
    if computed != record.original_sha256 {
        return Err(Error::Config(format!(
            "backup SHA256 mismatch for {}: expected {}, got {}",
            record.path, record.original_sha256, computed
        )));
    }
    Ok(backup_bytes)
}

/// Plan the unapply action for one receipt record. Returns `true` when the
/// record's backup was consumed by the plan and may be deleted; `false` when it must be
/// retained, because the backup is the only copy of the user's pre-apply bytes and is never
/// destroyed unless the restoration it backs was actually planned.
fn plan_unapply_record(home: &Path, set: &mut ChangeSet, record: &FileRecord) -> Result<bool> {
    validate_record_path(&record.path)?;
    let target = resolve_record_path(home, &record.path);

    let current = match std::fs::read(&target) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return plan_unapply_absent_target(home, set, record);
        }
        Err(e) => return Err(Error::Io(e)),
    };

    if sha256(&current) == record.applied_sha256 {
        // File is in the exact state apply left it: mindctx owns it.
        if !record.original_existed {
            // Apply created this file: unapply deletes it.
            set.add(FileChange::delete(target.clone(), current, false));
            if let Some(parent) = target.parent() {
                // Only single-level directories directly under home (e.g. ~/.codex) — never the
                // home directory itself and never project trees.
                if parent.parent() == Some(home)
                    && !set.cleanup_dirs.contains(&parent.to_path_buf())
                {
                    set.cleanup_dirs.push(parent.to_path_buf());
                }
            }
            return Ok(true);
        }
        if record.backup_path.is_empty() {
            return Err(Error::Config(format!(
                "receipt record for {} has no backup path",
                record.path
            )));
        }
        let backup_bytes = load_verified_backup(home, record)?;
        // Byte-exact restore from the backup.
        set.add(FileChange::write(target, current, backup_bytes, true));
        return Ok(true);
    }

    // The file was modified after apply: never clobber user edits. Remove only the mindctx-owned
    // block (ownership rule); if that is not possible, leave the file alone.
    match selective_removal(&record.path, &current) {
        Some(new_bytes) if new_bytes != current => {
            set.add(FileChange::write(target, current, new_bytes, true));
            Ok(true)
        }
        _ => {
            // The user's current bytes stay untouched on disk; retain the pre-apply backup so a
            // manual restore remains possible after the receipt is gone.
            let retained = if record.backup_path.is_empty() {
                String::new()
            } else {
                validate_record_path(&record.backup_path)?;
                format!(" (original backup retained at {})", record.backup_path)
            };
            set.warn_once(format!(
                "{} was modified after apply; mindctx-owned content could not be removed selectively, file left untouched{retained}",
                record.path
            ));
            Ok(false)
        }
    }
}

/// The recorded target is absent at unapply time:
/// - Original existed: the user deleted the managed file after apply. The backup IS the
///   byte-exact pre-apply content, so plan a restore-from-backup Write — the previous
///   behavior deleted the backup instead, destroying the only copy of the original.
/// - Original did not exist (apply created the file): absence is the desired end state;
///   skip quietly.
fn plan_unapply_absent_target(
    home: &Path,
    set: &mut ChangeSet,
    record: &FileRecord,
) -> Result<bool> {
    if !record.original_existed {
        return Ok(false); // created by apply, already gone: nothing to do, no backup exists
    }
    if record.backup_path.is_empty() {
        return Err(Error::Config(format!(
            "receipt record for {} has no backup path, but the file is absent and cannot be restored",
            record.path
        )));
    }
    let backup_bytes = load_verified_backup(home, record)?;
    set.add(FileChange::write(
        resolve_record_path(home, &record.path),
        Vec::new(),
        backup_bytes,
        false,
    ));
    Ok(true)
}

/// Commit a change set. Never recomputes; executes in plan order:
/// 1. `create_dirs` (0700 on unix)
/// 2. backups (0600) written from in-ChangeSet original bytes
/// 3. file changes in order (the receipt is FIRST: plan_receipt inserts it at the front, so a
///    crash after step 2 always leaves a usable receipt on disk)
/// 4. `cleanup_dirs` removed when empty
///
/// Residual non-atomicity (documented): each individual write is atomic
/// (tempfile + rename), but the set is not transactional across files. The receipt is written
/// before the targets and references backups materialized in step 2, so a crash at any point
/// after step 2 leaves the system unapply-able: unapply restores every modified target from its
/// backup, and targets the crash never reached restore to their identical current bytes. A
/// crash before step 2 leaves the targets untouched; at worst stray tempfiles remain.
pub fn commit(set: &ChangeSet) -> Result<()> {
    for dir in &set.create_dirs {
        ensure_private_dir(dir)?;
    }
    for change in &set.files {
        if let Some(backup_path) = &change.backup_target {
            write_atomically(backup_path, &change.original_bytes, Some(BACKUP_MODE))?;
        }
    }
    for change in &set.files {
        match (&change.action, &change.new_bytes) {
            (Action::Write, Some(new_bytes)) => {
                write_atomically(&change.target, new_bytes, change.mode)?;
            }
            (Action::Write, None) => {}
            (Action::Delete, _) => {
                if change.target.exists() {
                    std::fs::remove_file(&change.target)?;
                }
            }
        }
    }
    // Deepest first: children must be empty (and gone) before their parents are attempted.
    let mut cleanup: Vec<&PathBuf> = set.cleanup_dirs.iter().collect();
    cleanup.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    for dir in cleanup {
        // remove_dir only succeeds when the directory is empty.
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

/// Plan AGENTS.md marker blocks.
/// Uses project_root, not home directory.
fn plan_agents_markers(project_root: &Path, set: &mut ChangeSet) -> Result<()> {
    // AGENTS.md is created when missing; CLAUDE.md is the conditional one.
    for filename in ["AGENTS.md", "CLAUDE.md"] {
        let file_path = project_root.join(filename);
        if filename == "CLAUDE.md" && !file_path.exists() {
            continue;
        }

        let (original_existed, original_bytes) = read_file_or_empty(&file_path)?;
        let content = match std::str::from_utf8(&original_bytes) {
            Ok(content) => content,
            Err(_) => {
                set.warn_once(format!("skipping {filename}: not valid UTF-8"));
                continue;
            }
        };

        // Skip when any marker is present — even a partial (interrupted) block — so a second
        // block is never appended.
        if content.contains(MARKER_BEGIN) || content.contains(MARKER_END) {
            continue;
        }

        let new_content = if content.is_empty() {
            format!("{MARKER_BEGIN}\n{GUIDANCE}{MARKER_END}\n")
        } else {
            format!("{content}\n\n{MARKER_BEGIN}\n{GUIDANCE}{MARKER_END}\n")
        };

        set.add(FileChange::write(
            file_path,
            original_bytes,
            new_content.into_bytes(),
            original_existed,
        ));
    }

    Ok(())
}

/// Plan ~/.mindctx/config.toml creation if absent.
fn plan_mindctx_config(home: &Path, set: &mut ChangeSet) -> Result<()> {
    let config_path = home.join(".mindctx/config.toml");
    let (original_existed, original_bytes) = read_file_or_empty(&config_path)?;

    if original_existed {
        return Ok(()); // Already exists, nothing to do
    }

    set.add(FileChange::write(
        config_path,
        original_bytes,
        b"[core]\n".to_vec(),
        original_existed,
    ));

    Ok(())
}

/// Remove only the mindctx-owned content from a user-modified file, keyed by the receipt record
/// path. Returns None when the file kind is unknown or the content cannot be handled safely.
fn selective_removal(record_path: &str, current: &[u8]) -> Option<Vec<u8>> {
    let p = Path::new(record_path);
    let file_name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let parent_name = p
        .parent()
        .and_then(|d| d.file_name())
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_default();
    if file_name == "config.toml" && parent_name == ".codex" {
        strip_codex_mindctx(current)
    } else if file_name == ".claude.json" {
        strip_claude_mindctx(current)
    } else if file_name == "AGENTS.md" || file_name == "CLAUDE.md" {
        strip_marker_block(current)
    } else {
        None
    }
}

/// Remove the mindctx marker block from markdown bytes. Touches only the splice boundary —
/// no global whitespace mutation.
fn strip_marker_block(current: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(current).ok()?;
    let begin = text.find(MARKER_BEGIN)?;
    let end_rel = text[begin..].find(MARKER_END)?;
    let after_end = begin + end_rel + MARKER_END.len();
    let mut before = &text[..begin];
    let after = &text[after_end..];
    if after.trim().is_empty() {
        // The block sits at EOF: drop the blank-line separator apply inserted before it.
        before = before.strip_suffix("\n\n").unwrap_or(before);
    }
    Some(format!("{before}{after}").into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const CODEX_CONFIG_TOML_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/fixtures/host-config/config.toml"
    ));
    const AGENTS_MD_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/fixtures/host-config/AGENTS.md"
    ));

    /// Helper: create a fake binary file in the temp directory for testing.
    fn create_fake_binary(temp_dir: &std::path::Path) -> PathBuf {
        let bin_dir = temp_dir.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let binary_path = bin_dir.join("mindctx");
        // Write some bytes to represent the "binary"
        fs::write(&binary_path, b"fake mindctx binary for testing").unwrap();
        binary_path
    }

    fn opts(claude: bool, codex: bool, budget: Option<u64>, binary_path: PathBuf) -> ApplyOpts {
        ApplyOpts {
            claude,
            codex,
            budget,
            yes: true,
            binary_path,
        }
    }

    /// Helper: run one full apply (plan + commit).
    fn apply(home: &Path, project_root: &Path, o: &ApplyOpts) -> ChangeSet {
        let set = plan_apply(home, project_root, o).unwrap();
        commit(&set).unwrap();
        set
    }

    fn codex_config(home: &Path) -> PathBuf {
        let path = home.join(".codex/config.toml");
        fs::create_dir_all(home.join(".codex")).unwrap();
        path
    }

    /// Test that Claude Code config has the correct JSON shape.
    #[test]
    fn apply_claude_config_shape() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let project_root = temp_dir.path();
        let binary_path = create_fake_binary(home);

        let o = opts(true, false, Some(4000), binary_path);
        let changeset = plan_apply(home, project_root, &o).unwrap();
        assert!(!changeset.files.is_empty(), "Changeset should have files");

        let claude_change = changeset
            .files
            .iter()
            .find(|c| c.target.ends_with(".claude.json"))
            .expect("Should have Claude config change");

        assert_eq!(claude_change.action, Action::Write);
        assert!(claude_change.new_bytes.is_some());

        let new_bytes = claude_change.new_bytes.as_ref().unwrap();
        let config: serde_json::Value = serde_json::from_slice(new_bytes).unwrap();

        // Verify structure
        assert!(config["mcpServers"].is_object());
        assert!(config["mcpServers"]["mindctx"].is_object());

        let mindctx = &config["mcpServers"]["mindctx"];
        assert!(mindctx["command"].is_string());
        assert!(mindctx["args"].is_array());
        assert_eq!(mindctx["args"], serde_json::json!(["serve"]));

        // Budget env is a STRING in both hosts
        assert!(mindctx["env"].is_object());
        assert_eq!(
            mindctx["env"]["MINDCTX_TOKEN_BUDGET"],
            serde_json::json!("4000")
        );
    }

    /// Claude config upsert is surgical: unrelated members keep their exact bytes and order,
    /// only `mcpServers.mindctx` is touched.
    #[test]
    fn apply_claude_config_preserves_key_order() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let binary_path = create_fake_binary(home);

        // Keys in non-alphabetical order, plus a trailing newline
        let original = r#"{
  "z-last": {
    "b": 1
  },
  "mcpServers": {},
  "a-first": 2
}
"#;
        fs::write(home.join(".claude.json"), original).unwrap();

        let o = opts(true, false, None, binary_path);
        let changeset = plan_apply(home, temp_dir.path(), &o).unwrap();
        let change = changeset
            .files
            .iter()
            .find(|c| c.target.ends_with(".claude.json"))
            .unwrap();
        let new_content = String::from_utf8(change.new_bytes.clone().unwrap()).unwrap();

        // The mindctx entry is inserted in place; the rest of the document is byte-identical.
        assert!(
            new_content.contains("\"mcpServers\": {\n    \"mindctx\": {\n      \"args\": ["),
            "got: {new_content}"
        );
        assert!(
            new_content.contains("\"command\": \""),
            "got: {new_content}"
        );
        assert!(new_content.contains("\n  \"z-last\": {\n    \"b\": 1\n  },"));
        assert!(new_content.contains("\n  \"a-first\": 2\n}"));
        let z = new_content.find("\"z-last\"").unwrap();
        let a = new_content.find("\"a-first\"").unwrap();
        assert!(z < a, "user key order preserved, got: {new_content}");
        assert!(new_content.ends_with('\n'), "trailing newline preserved");
        assert!(!new_content.contains("env"), "no env without --budget");

        // Idempotence: after commit, a second plan produces no change at all.
        commit(&changeset).unwrap();
        let set2 = plan_apply(home, temp_dir.path(), &o).unwrap();
        assert!(set2.files.is_empty(), "{:?}", set2.files);
    }

    /// A budget change replaces the mindctx entry surgically; env is a string.
    #[test]
    fn apply_claude_budget_change_replaces_entry() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let binary_path = create_fake_binary(home);

        let original = r#"{
  "userID": "u-1",
  "mcpServers": {
    "mindctx": {
      "command": "/old/path",
      "args": [
        "serve"
      ],
      "env": {
        "MINDCTX_TOKEN_BUDGET": "1000"
      }
    }
  }
}
"#;
        fs::write(home.join(".claude.json"), original).unwrap();

        let o = opts(true, false, Some(4000), binary_path);
        let changeset = plan_apply(home, temp_dir.path(), &o).unwrap();
        let change = changeset
            .files
            .iter()
            .find(|c| c.target.ends_with(".claude.json"))
            .unwrap();
        let new_content = String::from_utf8(change.new_bytes.clone().unwrap()).unwrap();

        assert!(new_content.contains("command\": \""), "got: {new_content}");
        assert!(!new_content.contains("/old/path"));
        assert!(new_content.contains("MINDCTX_TOKEN_BUDGET\": \"4000\""));
        assert!(new_content.contains("\"userID\": \"u-1\""));
        assert!(new_content.ends_with('\n'));
    }

    /// A malformed existing config must produce an error, not a panic.
    #[test]
    fn apply_claude_config_rejects_non_object_mcp_servers() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let binary_path = create_fake_binary(home);

        fs::write(home.join(".claude.json"), br#"{"mcpServers": "oops"}"#).unwrap();

        let o = opts(true, false, None, binary_path);
        let err = plan_apply(home, temp_dir.path(), &o).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got: {err:?}");
    }

    /// Test that Codex config preserves TOML comments.
    #[test]
    fn apply_codex_toml_preserves_comments() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let codex_config = codex_config(home);
        let original_content = "# This is a comment\n[mcp_servers]\n# Another comment\n";
        fs::write(&codex_config, original_content).unwrap();

        let o = opts(false, true, None, create_fake_binary(home));
        let changeset = plan_apply(home, temp_dir.path(), &o).unwrap();
        let codex_change = changeset
            .files
            .iter()
            .find(|c| c.target.ends_with("config.toml"))
            .expect("Should have Codex config change");

        let new_content = String::from_utf8(codex_change.new_bytes.clone().unwrap()).unwrap();

        // Comments should be preserved
        assert!(new_content.contains("# This is a comment"));
        assert!(new_content.contains("# Another comment"));

        // Should NOT have env key when budget is None
        assert!(!new_content.contains("env"));
    }

    /// The host-config fixture applies cleanly: unrelated tables and comments survive.
    #[test]
    fn apply_codex_config_fixture_preserves_unrelated_tables() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let codex_config = codex_config(home);
        fs::write(&codex_config, CODEX_CONFIG_TOML_FIXTURE).unwrap();

        let o = opts(false, true, None, create_fake_binary(home));
        let changeset = plan_apply(home, temp_dir.path(), &o).unwrap();
        let change = changeset
            .files
            .iter()
            .find(|c| c.target == codex_config)
            .expect("Should have Codex config change");
        let new_content = String::from_utf8(change.new_bytes.clone().unwrap()).unwrap();

        assert!(new_content.contains("# Sample Codex configuration"));
        assert!(new_content.contains("[model]"));
        assert!(new_content.contains("model = \"gpt-5\""));
        assert!(new_content.contains("[mcp_servers.mindctx]"));
        // toml_edit switches to literal strings (single quotes) when the value contains
        // backslashes — both are valid TOML, so accept either quote style.
        assert!(
            new_content.contains("command = \"") || new_content.contains("command = '"),
            "expected command line in [mcp_servers.mindctx]; got: {new_content}"
        );
        assert!(!new_content.contains("env"));
    }

    /// Non-UTF-8 Codex config is an error, not silent corruption.
    #[test]
    fn apply_errors_on_non_utf8_codex_config() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let codex_config = codex_config(home);
        fs::write(&codex_config, b"\xff\xfe broken \xff").unwrap();

        let o = opts(false, true, None, create_fake_binary(home));
        let err = plan_apply(home, temp_dir.path(), &o).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got: {err:?}");
    }

    /// Unreadable configs propagate the I/O error instead of being treated as absent: a
    /// directory in place of ~/.claude.json must fail the plan.
    #[test]
    fn apply_rejects_unreadable_claude_config() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        fs::create_dir_all(home.join(".claude.json")).unwrap();

        let o = opts(true, false, None, create_fake_binary(home));
        let err = plan_apply(home, temp_dir.path(), &o).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "got: {err:?}");
    }

    /// Test that AGENTS.md marker block is positioned correctly with proper separators.
    #[test]
    fn agents_marker_block_position_and_separators() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let project_root = temp_dir.path();

        let agents_path = project_root.join("AGENTS.md");
        fs::write(&agents_path, AGENTS_MD_FIXTURE).unwrap();

        let o = opts(false, false, None, create_fake_binary(home));
        let changeset = plan_apply(home, project_root, &o).unwrap();
        let agents_change = changeset
            .files
            .iter()
            .find(|c| c.target.ends_with("AGENTS.md"))
            .expect("Should have AGENTS.md change");

        let new_content = String::from_utf8(agents_change.new_bytes.clone().unwrap()).unwrap();

        // Original content (including the fixture's own text) comes first
        assert!(new_content.starts_with("# AGENTS.md"));
        assert!(new_content.contains("## Project-specific guidance"));

        // Markers come after the original content, separated by exactly one blank line
        let begin_pos = new_content.find(MARKER_BEGIN).unwrap();
        let original_pos = new_content.find("# AGENTS.md").unwrap();
        assert!(
            begin_pos > original_pos,
            "Marker should be after original content"
        );
        assert!(new_content.contains("\n\n<!-- mindctx:begin (guidance-v1) -->"));
        assert!(new_content.contains(MARKER_END));

        // Guidance block is 3 content lines
        let block = &new_content[begin_pos..];
        let guidance_lines = block
            .lines()
            .skip(1)
            .take_while(|l| *l != MARKER_END)
            .count();
        assert_eq!(guidance_lines, 3, "block was: {block}");
    }

    /// A missing AGENTS.md is created with the marker block; CLAUDE.md stays conditional.
    #[test]
    fn apply_creates_missing_agents_md_not_claude_md() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let project_root = temp_dir.path();

        let o = opts(false, false, None, create_fake_binary(home));
        let changeset = plan_apply(home, project_root, &o).unwrap();

        let agents_change = changeset
            .files
            .iter()
            .find(|c| c.target.ends_with("AGENTS.md"))
            .expect("AGENTS.md should be planned for creation");
        assert!(!agents_change.original_existed);
        let content = String::from_utf8(agents_change.new_bytes.clone().unwrap()).unwrap();
        assert!(content.starts_with(MARKER_BEGIN));
        assert!(content.trim_end().ends_with(MARKER_END));

        assert!(
            !changeset
                .files
                .iter()
                .any(|c| c.target.ends_with("CLAUDE.md")),
            "CLAUDE.md must not be created"
        );

        commit(&changeset).unwrap();
        assert!(project_root.join("AGENTS.md").exists());
        assert!(!project_root.join("CLAUDE.md").exists());
    }

    /// Second apply writes nothing at all — not even the receipt.
    #[test]
    fn apply_idempotent_second_run_writes_nothing() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let project_root = temp_dir.path();
        let binary_path = create_fake_binary(home);

        fs::write(project_root.join("AGENTS.md"), "# Test\n").unwrap();

        let o = opts(true, true, Some(4000), binary_path);
        apply(home, project_root, &o);

        let set2 = plan_apply(home, project_root, &o).unwrap();
        assert!(
            set2.files.is_empty(),
            "second apply should plan nothing, got: {:?}",
            set2.files
        );
    }

    /// Second apply must not wipe the receipt: records for untouched files survive and unapply
    /// stays possible.
    #[test]
    fn second_apply_preserves_receipt_records() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let project_root = temp_dir.path();
        let binary_path = create_fake_binary(home);

        fs::write(project_root.join("AGENTS.md"), "# Test\n").unwrap();
        let o = opts(true, true, Some(4000), binary_path.clone());
        apply(home, project_root, &o);

        // Second apply with a different budget re-plans claude/codex but not AGENTS.md.
        let o2 = opts(true, true, Some(5000), binary_path);
        let set2 = plan_apply(home, project_root, &o2).unwrap();
        commit(&set2).unwrap();

        let receipt: Receipt =
            serde_json::from_slice(&fs::read(home.join(".mindctx/record/receipt.json")).unwrap())
                .unwrap();
        for expected in [
            ".claude.json",
            ".codex/config.toml",
            "AGENTS.md",
            ".mindctx/config.toml",
        ] {
            let record = receipt
                .files
                .iter()
                .find(|f| f.path == expected)
                .unwrap_or_else(|| panic!("receipt lost record for {expected} after re-apply"));
            assert!(
                !record.applied_sha256.is_empty(),
                "{expected}: applied_sha256 set"
            );
        }
        // The AGENTS.md record was not re-planned in run 2, yet it is still restorable.
        let agents = receipt
            .files
            .iter()
            .find(|f| f.path == "AGENTS.md")
            .unwrap();
        let backup_bytes = fs::read(home.join(&agents.backup_path)).unwrap();
        assert_eq!(sha256(&backup_bytes), agents.original_sha256);

        // Unapply after the re-apply works and restores the true original.
        let unapply_set = plan_unapply(home).unwrap();
        commit(&unapply_set).unwrap();
        let restored = fs::read(project_root.join("AGENTS.md")).unwrap();
        assert_eq!(restored, b"# Test\n");
    }

    /// Re-applying a file reuses the intact original backup instead of overwriting it.
    #[test]
    fn reapply_reuses_original_backup() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let codex_config = codex_config(home);
        let original = b"# my config\n[other]\nkey = 1\n";
        fs::write(&codex_config, original).unwrap();

        apply(
            home,
            temp_dir.path(),
            &opts(false, true, Some(1000), create_fake_binary(home)),
        );
        apply(
            home,
            temp_dir.path(),
            &opts(false, true, Some(2000), create_fake_binary(home)),
        );

        let backup_dir = home.join(".mindctx/record/backup");
        let backup0 = fs::read(backup_dir.join("0")).unwrap();
        assert_eq!(backup0, original, "backup/0 still holds the true original");

        let receipt: Receipt =
            serde_json::from_slice(&fs::read(home.join(".mindctx/record/receipt.json")).unwrap())
                .unwrap();
        let record = receipt
            .files
            .iter()
            .find(|f| f.path == ".codex/config.toml")
            .unwrap();
        assert_eq!(record.backup_path, ".mindctx/record/backup/0");
        assert_eq!(record.original_sha256, sha256(original));

        let unapply_set = plan_unapply(home).unwrap();
        commit(&unapply_set).unwrap();
        assert_eq!(fs::read(&codex_config).unwrap(), original);
    }

    /// Backup numbering continues from the highest existing backup.
    #[test]
    fn backup_numbering_continues_across_applies() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let binary_path = create_fake_binary(home);

        fs::write(home.join(".claude.json"), b"{\n  \"a\": 1\n}\n").unwrap();
        fs::write(codex_config(home), b"# codex\n").unwrap();
        apply(
            home,
            temp_dir.path(),
            &opts(true, false, None, binary_path.clone()),
        );
        apply(home, temp_dir.path(), &opts(false, true, None, binary_path));

        let backup_dir = home.join(".mindctx/record/backup");
        assert!(backup_dir.join("0").exists(), "first backup at 0");
        assert!(
            backup_dir.join("1").exists(),
            "second apply must not restart numbering at 0"
        );
    }

    /// plan_apply is pure: no directories, backups, or files are created during planning.
    /// Previewing or cancelling leaves the system untouched.
    #[test]
    fn plan_apply_does_not_touch_filesystem() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let project_root = temp_dir.path();
        fs::write(project_root.join("AGENTS.md"), "# Original\n").unwrap();

        let o = opts(true, true, Some(4000), create_fake_binary(home));
        let _set = plan_apply(home, project_root, &o).unwrap();

        assert!(!home.join(".mindctx").exists(), "no record dir created");
        assert!(!home.join(".claude.json").exists(), "no config written");
        assert!(!home.join(".codex").exists(), "no codex dir created");
        assert_eq!(
            fs::read(project_root.join("AGENTS.md")).unwrap(),
            b"# Original\n",
            "markers not written during planning"
        );
    }

    /// Unapply restores every managed file byte-for-byte in the normal case (markers still
    /// present).
    #[test]
    fn unapply_restores_bytes_exactly() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let project_root = temp_dir.path();
        let binary_path = create_fake_binary(home);

        let agents_original = b"# Original Content\nLine 2\nLine 3\n";
        fs::write(project_root.join("AGENTS.md"), agents_original).unwrap();
        let codex_original = b"# codex original\n[other]\nkey = 1\n";
        fs::write(codex_config(home), codex_original).unwrap();
        let claude_original = b"{\n  \"z-key\": 1,\n  \"a-key\": 2\n}\n";
        fs::write(home.join(".claude.json"), claude_original).unwrap();

        apply(
            home,
            project_root,
            &opts(true, true, Some(4000), binary_path),
        );

        // No manual edits here: AGENTS.md still contains the markers, exercising the
        // markers-present restore path (the old test masked this by overwriting the file first).
        let unapply_set = plan_unapply(home).unwrap();
        assert!(
            unapply_set.warnings.is_empty(),
            "{:?}",
            unapply_set.warnings
        );
        commit(&unapply_set).unwrap();

        assert_eq!(
            fs::read(project_root.join("AGENTS.md")).unwrap(),
            agents_original
        );
        assert_eq!(fs::read(codex_config(home)).unwrap(), codex_original);
        assert_eq!(
            fs::read(home.join(".claude.json")).unwrap(),
            claude_original
        );
    }

    /// When the user edited a file after apply, unapply removes only the mindctx-owned block
    /// and keeps the user's edits.
    #[test]
    fn unapply_strips_markers_preserving_user_edits() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let project_root = temp_dir.path();
        let binary_path = create_fake_binary(home);

        fs::write(project_root.join("AGENTS.md"), "# My Notes\n").unwrap();
        apply(home, project_root, &opts(false, false, None, binary_path));

        // User appends their own content after the marker block.
        let mut edited = fs::read(project_root.join("AGENTS.md")).unwrap();
        edited.extend_from_slice(b"\nAppended by user\n");
        fs::write(project_root.join("AGENTS.md"), &edited).unwrap();

        let unapply_set = plan_unapply(home).unwrap();
        commit(&unapply_set).unwrap();

        let result = String::from_utf8(fs::read(project_root.join("AGENTS.md")).unwrap()).unwrap();
        assert!(!result.contains(MARKER_BEGIN), "marker removed: {result}");
        assert!(!result.contains("mindctx"));
        assert!(result.contains("# My Notes"));
        assert!(result.contains("Appended by user"));
    }

    /// Codex config: unapply removes the mindctx table selectively when the user edited the
    /// file after apply (ownership rule).
    #[test]
    fn unapply_codex_selective_removal_on_user_edit() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let codex_config = codex_config(home);
        let binary_path = create_fake_binary(home);

        let original = "# top comment\n\n[mcp_servers]\n\n[other]\nkey = 1\n";
        fs::write(&codex_config, original).unwrap();
        apply(home, temp_dir.path(), &opts(false, true, None, binary_path));

        // User adds their own table after apply.
        let mut edited = fs::read(&codex_config).unwrap();
        edited.extend_from_slice(b"[user]\nextra = true\n");
        fs::write(&codex_config, &edited).unwrap();

        let unapply_set = plan_unapply(home).unwrap();
        commit(&unapply_set).unwrap();

        let result = String::from_utf8(fs::read(&codex_config).unwrap()).unwrap();
        assert!(
            !result.contains("mindctx"),
            "mindctx table removed: {result}"
        );
        assert!(result.contains("key = 1"), "user's [other] table kept");
        assert!(
            result.contains("extra = true"),
            "user's post-apply edit kept"
        );
        assert!(result.contains("# top comment"));
    }

    /// Files apply created are deleted by unapply, and the directories apply created are
    /// removed when empty.
    #[test]
    fn unapply_deletes_created_files_and_dirs() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let project_root = temp_dir.path();
        let binary_path = create_fake_binary(home);

        let receipt_path = home.join(".mindctx/record/receipt.json");
        let claude_path = home.join(".claude.json");
        let codex_path = home.join(".codex/config.toml");
        let mindctx_config = home.join(".mindctx/config.toml");

        apply(
            home,
            project_root,
            &opts(true, true, Some(4000), binary_path),
        );
        assert!(claude_path.exists());
        assert!(codex_path.exists());
        assert!(mindctx_config.exists());
        assert!(receipt_path.exists());
        assert!(
            project_root.join("AGENTS.md").exists(),
            "AGENTS.md was created"
        );

        let unapply_set = plan_unapply(home).unwrap();
        commit(&unapply_set).unwrap();

        assert!(!claude_path.exists(), "created .claude.json deleted");
        assert!(!codex_path.exists(), "created codex config deleted");
        assert!(!mindctx_config.exists(), "created mindctx config deleted");
        assert!(
            !project_root.join("AGENTS.md").exists(),
            "created AGENTS.md deleted"
        );
        assert!(!receipt_path.exists());
        assert!(!home.join(".mindctx").exists(), "empty .mindctx removed");
        assert!(!home.join(".codex").exists(), "empty .codex removed");
    }

    /// A created file that the user modified after apply is NOT deleted; only the mindctx-owned
    /// parts are removed (interplay between the apply and unapply flows).
    #[test]
    fn unapply_keeps_modified_created_file() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let binary_path = create_fake_binary(home);

        apply(home, temp_dir.path(), &opts(true, false, None, binary_path));
        let claude_path = home.join(".claude.json");

        // User adds a personal key after apply.
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&claude_path).unwrap()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("my-key".into(), serde_json::json!(true));
        let mut edited = serde_json::to_vec_pretty(&value).unwrap();
        edited.push(b'\n');
        fs::write(&claude_path, &edited).unwrap();

        let unapply_set = plan_unapply(home).unwrap();
        commit(&unapply_set).unwrap();

        assert!(claude_path.exists(), "user-modified created file kept");
        let result = String::from_utf8(fs::read(&claude_path).unwrap()).unwrap();
        assert!(!result.contains("mindctx"));
        assert!(result.contains("my-key"));
    }

    /// The receipt change is planned FIRST in the changeset, so commit writes it before any
    /// target: a crash after the backups are materialized can never leave modified configs
    /// without a receipt.
    #[test]
    fn plan_apply_puts_receipt_change_first() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let project_root = temp_dir.path();
        fs::write(project_root.join("AGENTS.md"), "# Test\n").unwrap();

        let changeset = plan_apply(
            home,
            project_root,
            &opts(true, true, Some(4000), create_fake_binary(home)),
        )
        .unwrap();
        assert!(!changeset.files.is_empty());
        let receipt_change = changeset
            .files
            .first()
            .expect("changeset must not be empty");
        assert!(
            receipt_change
                .target
                .ends_with(".mindctx/record/receipt.json"),
            "receipt must be the first file change, got: {:?}",
            receipt_change.target
        );
    }

    /// A managed file the user deleted after apply is restored byte-exactly from the backup on
    /// unapply — the backup is not destroyed.
    #[test]
    fn unapply_restores_user_deleted_file_from_backup() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let codex_config = codex_config(home);
        let original = b"# my original codex config\n";
        fs::write(&codex_config, original).unwrap();
        apply(
            home,
            temp_dir.path(),
            &opts(false, true, None, create_fake_binary(home)),
        );

        // The user (or the host) deletes the managed file after apply.
        fs::remove_file(&codex_config).unwrap();

        let unapply_set = plan_unapply(home).unwrap();
        assert!(
            unapply_set.warnings.is_empty(),
            "{:?}",
            unapply_set.warnings
        );
        commit(&unapply_set).unwrap();

        assert_eq!(
            fs::read(&codex_config).unwrap(),
            original,
            "pre-apply bytes restored"
        );
        assert!(!home.join(".mindctx/record/receipt.json").exists());
        assert!(
            fs::read_dir(home.join(".mindctx/record/backup"))
                .map(|mut e| e.next().is_none())
                .unwrap_or(true),
            "consumed backup removed"
        );
    }

    /// A file apply created that is already absent at unapply time stays absent — quietly, with
    /// the flow completing and no data touched.
    #[test]
    fn unapply_created_file_already_absent_stays_absent() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let claude_path = home.join(".claude.json");
        apply(
            home,
            temp_dir.path(),
            &opts(true, false, None, create_fake_binary(home)),
        );
        assert!(claude_path.exists());

        fs::remove_file(&claude_path).unwrap();

        let unapply_set = plan_unapply(home).unwrap();
        assert!(
            !unapply_set
                .warnings
                .iter()
                .any(|w| w.contains(".claude.json")),
            "absent created file must be skipped quietly: {:?}",
            unapply_set.warnings
        );
        assert!(
            unapply_set.files.iter().all(|c| c.target != claude_path),
            "no change planned for an already-absent created file"
        );
        commit(&unapply_set).unwrap();

        assert!(!claude_path.exists(), "stays absent");
        assert!(!home.join(".mindctx/record/receipt.json").exists());
    }

    /// When the user's edit makes selective removal impossible, the file is left untouched and
    /// the pre-apply backup is RETAINED, not destroyed.
    #[test]
    fn unapply_retains_backup_when_selective_removal_impossible() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let codex_config = codex_config(home);
        fs::write(&codex_config, b"# ok\n").unwrap();
        apply(
            home,
            temp_dir.path(),
            &opts(false, true, None, create_fake_binary(home)),
        );

        // Unparseable TOML: strip_codex_mindctx cannot handle it, so nothing is removed.
        fs::write(&codex_config, b"#[[ broken\nmindctx =").unwrap();

        let unapply_set = plan_unapply(home).unwrap();
        assert!(
            unapply_set
                .warnings
                .iter()
                .any(|w| w.contains(".codex/config.toml") && w.contains("backup retained")),
            "{:?}",
            unapply_set.warnings
        );
        commit(&unapply_set).unwrap();

        assert!(
            home.join(".mindctx/record/backup/0").exists(),
            "retained backup survives the receipt removal"
        );
    }

    /// Numbered backup files no receipt record references (stale entries from an earlier
    /// re-plan) are removed by unapply.
    #[test]
    fn unapply_removes_orphaned_backups() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let codex_config = codex_config(home);
        fs::write(&codex_config, b"# ok\n").unwrap();
        apply(
            home,
            temp_dir.path(),
            &opts(false, true, None, create_fake_binary(home)),
        );

        // Simulate an orphaned backup left behind by a prior re-plan.
        fs::write(home.join(".mindctx/record/backup/9"), b"stale").unwrap();

        let unapply_set = plan_unapply(home).unwrap();
        commit(&unapply_set).unwrap();

        assert!(
            fs::read_dir(home.join(".mindctx/record/backup"))
                .map(|mut e| e.next().is_none())
                .unwrap_or(true),
            "backup dir empty after unapply (orphan 9 removed too)"
        );
    }

    /// Re-applying over a file the user edited since the last apply warns before overwriting;
    /// an unedited re-apply stays silent.
    #[test]
    fn reapply_warns_when_user_edited_since_last_apply() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let binary_path = create_fake_binary(home);
        let o = opts(true, false, Some(4000), binary_path.clone());

        apply(home, temp_dir.path(), &o);

        // Unedited re-plan: no warning.
        let set2 = plan_apply(home, temp_dir.path(), &o).unwrap();
        assert!(set2.warnings.is_empty(), "{:?}", set2.warnings);

        // User hand-edit after apply.
        let claude_path = home.join(".claude.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&claude_path).unwrap()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("my-key".into(), serde_json::json!(true));
        let mut edited = serde_json::to_vec_pretty(&value).unwrap();
        edited.push(b'\n');
        fs::write(&claude_path, &edited).unwrap();

        let set3 = plan_apply(
            home,
            temp_dir.path(),
            &opts(true, false, Some(5000), binary_path.clone()),
        )
        .unwrap();
        assert!(
            set3.warnings
                .iter()
                .any(|w| w.contains(".claude.json") && w.contains("overwrites")),
            "re-apply over a user edit must warn: {:?}",
            set3.warnings
        );
    }

    /// Unapply of a user-edited .claude.json splices out only the mindctx entry; every other
    /// byte of the document (key order, inline shape, user edits) survives — no full-document
    /// rewrite.
    #[test]
    fn unapply_claude_selective_removal_preserves_bytes() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let binary_path = create_fake_binary(home);

        let original = r#"{
  "z-last": {"b": 1},
  "mcpServers": {"other": {"command": "x"}},
  "a-first": 2
}
"#;
        fs::write(home.join(".claude.json"), original).unwrap();
        apply(home, temp_dir.path(), &opts(true, false, None, binary_path));

        // User edit after apply so the ownership rule (selective removal) applies.
        let edited = String::from_utf8(fs::read(home.join(".claude.json")).unwrap())
            .unwrap()
            .replace("\"a-first\": 2", "\"a-first\": 3");
        fs::write(home.join(".claude.json"), &edited).unwrap();

        let unapply_set = plan_unapply(home).unwrap();
        assert!(
            unapply_set.warnings.is_empty(),
            "{:?}",
            unapply_set.warnings
        );
        commit(&unapply_set).unwrap();

        let result = String::from_utf8(fs::read(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(
            result,
            r#"{
  "z-last": {"b": 1},
  "mcpServers": {
    "other": {"command": "x"}},
  "a-first": 3
}
"#,
            "only the mindctx entry was spliced out; user bytes untouched"
        );
    }

    /// Duplicate "mcpServers"/"mindctx" members are refused by the surgical scanner and fall
    /// back to the full rewrite, so the member the host's parser actually keeps (the last one)
    /// is the one updated.
    #[test]
    fn apply_claude_duplicate_members_fall_back_to_full_rewrite() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let binary_path = create_fake_binary(home);

        // Valid JSON: parsers resolve duplicate keys last-wins.
        fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers": {"x": 1}, "mcpServers": {"mindctx": {"command": "old"}}}"#,
        )
        .unwrap();

        let changeset =
            plan_apply(home, temp_dir.path(), &opts(true, false, None, binary_path)).unwrap();
        let change = changeset
            .files
            .iter()
            .find(|c| c.target.ends_with(".claude.json"))
            .expect("duplicate-key config must still be updatable via the fallback");
        let parsed: serde_json::Value =
            serde_json::from_slice(change.new_bytes.as_deref().unwrap()).unwrap();
        assert_eq!(
            parsed["mcpServers"]["mindctx"]["args"],
            serde_json::json!(["serve"]),
            "the effective (last) mcpServers object carries the new registration"
        );
    }

    /// Test that unapply removes receipt and backup files.
    #[test]
    fn unapply_removes_receipt() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let project_root = temp_dir.path();
        let binary_path = create_fake_binary(home);

        fs::write(project_root.join("AGENTS.md"), "# Test\n").unwrap();
        apply(home, project_root, &opts(false, false, None, binary_path));

        let receipt_path = home.join(".mindctx/record/receipt.json");
        assert!(receipt_path.exists(), "Receipt should exist after apply");

        let unapply_set = plan_unapply(home).unwrap();
        commit(&unapply_set).unwrap();

        assert!(
            !receipt_path.exists(),
            "Receipt should be removed after unapply"
        );
        let backup_dir = home.join(".mindctx/record/backup");
        assert!(
            fs::read_dir(&backup_dir).map_or(true, |mut entries| entries.next().is_none()),
            "backups should be removed"
        );
    }

    /// Missing receipt means nothing to unapply (empty ChangeSet, no error).
    #[test]
    fn unapply_with_no_receipt_returns_empty_set() {
        let temp_dir = TempDir::new().unwrap();
        let set = plan_unapply(temp_dir.path()).unwrap();
        assert!(set.files.is_empty());
    }

    /// Receipt-driven path traversal is rejected.
    #[test]
    fn unapply_rejects_traversal_paths_in_receipt() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let record_dir = home.join(".mindctx/record");
        fs::create_dir_all(&record_dir).unwrap();
        let receipt = r#"{
  "version": 2,
  "binary_path": "/bin/mindctx",
  "binary_sha256": "x",
  "budget": null,
  "files": [
    {
      "path": "../evil.txt",
      "original_existed": true,
      "original_sha256": "x",
      "applied_sha256": "x",
      "backup_path": "0"
    }
  ]
}"#;
        fs::write(record_dir.join("receipt.json"), receipt).unwrap();
        let err = plan_unapply(home).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got: {err:?}");
    }

    /// Test that receipt records correct SHA256 values, including the applied hash used by the
    /// ownership rule.
    #[test]
    fn receipt_records_sha256() {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let project_root = temp_dir.path();
        let binary_path = create_fake_binary(home);

        let known_content = b"# Known Content\nWith specific bytes\n";
        fs::write(project_root.join("AGENTS.md"), known_content).unwrap();

        apply(home, project_root, &opts(false, false, None, binary_path));

        let receipt: Receipt =
            serde_json::from_slice(&fs::read(home.join(".mindctx/record/receipt.json")).unwrap())
                .unwrap();
        let agents_record = receipt
            .files
            .iter()
            .find(|f| f.path == "AGENTS.md")
            .expect("Should have AGENTS.md in receipt");

        assert_eq!(agents_record.original_sha256, sha256(known_content));

        // applied_sha256 matches the bytes actually on disk after commit
        let current = fs::read(project_root.join("AGENTS.md")).unwrap();
        assert_eq!(agents_record.applied_sha256, sha256(&current));
        assert_ne!(agents_record.applied_sha256, agents_record.original_sha256);
    }

    /// Receipt and backups are 0600, record dirs are 0700 (unix only).
    #[cfg(unix)]
    #[test]
    fn receipt_and_backups_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let binary_path = create_fake_binary(home);

        fs::write(home.join(".claude.json"), b"{\n  \"a\": 1\n}\n").unwrap();
        apply(home, temp_dir.path(), &opts(true, false, None, binary_path));

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&home.join(".mindctx/record/receipt.json")), 0o600);
        assert_eq!(mode(&home.join(".mindctx/record/backup/0")), 0o600);
        assert_eq!(mode(&home.join(".mindctx/record")), 0o700);
        assert_eq!(mode(&home.join(".mindctx/record/backup")), 0o700);
    }

    /// Atomic replacement preserves an existing target's permission mode: a 0600
    /// credential file rewritten by apply must not land as 0644 (unix only).
    #[cfg(unix)]
    #[test]
    fn apply_preserves_existing_target_mode() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path();
        let binary_path = create_fake_binary(home);

        let config = home.join(".claude.json");
        fs::write(&config, b"{\n  \"a\": 1\n}\n").unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        apply(home, temp_dir.path(), &opts(true, false, None, binary_path));

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&config), 0o600, "rewritten config keeps 0600");
    }
}
