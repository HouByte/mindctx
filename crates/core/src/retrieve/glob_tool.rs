// SPDX-License-Identifier: MIT OR Apache-2.0

//! Glob tool: structured path matching with sort, details, and pagination.

use chrono::{DateTime, SecondsFormat};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::SystemTime;

use super::search_render::{Noun, fit_largest, quantify};
use crate::budget::{self, ToolKind, what};
use crate::envelope::{Envelope, Page, SkipDetail, SkipReport, Terminal};
use crate::error::{Error, Result};
use crate::retrieve::glob_args::GlobArgs;
use crate::retrieve::grep_filter::PathGlobFilter;
use crate::retrieve::traversal::{self, Candidate, TraversalPolicy, display_path};
use crate::tokenize::count_tokens;

/// Which ignore sources apply during a glob traversal.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GlobFilterMode {
    /// `.ignore` files plus all git ignore sources (`.gitignore`, global excludes,
    /// `.git/info/exclude`) are honored; `.git` is pruned.
    #[default]
    Ignore,
    /// No ignore filtering at all: not `.ignore`, not git. `.git` is still pruned.
    All,
}

/// Result ordering for glob.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GlobSort {
    /// mtime descending, rel path ascending as tie-breaker.
    #[default]
    Modified,
    /// rel path ascending.
    Path,
}

/// Output mode for glob.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GlobOutput {
    /// One canonical display path per line.
    #[default]
    Paths,
    /// One compact JSON object per line with metadata.
    Details,
}

/// Glob parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GlobParams {
    /// Glob pattern(s) to match (array published, accepts a bare string).
    #[schemars(with = "Vec<String>")]
    pub pattern: GlobArgs,
    /// Root directory to search, project-relative or absolute; default: the project root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `ignore` (default; honors `.ignore` + gitignore rules) | `all`.
    #[serde(default)]
    pub filter_mode: GlobFilterMode,
    /// `modified` (default) | `path`.
    #[serde(default)]
    pub sort: GlobSort,
    /// `paths` (default) | `details`.
    #[serde(default)]
    pub output_mode: GlobOutput,
    /// Skip the first N entries before applying limit.
    #[serde(default)]
    pub offset: u64,
    /// Max output entries; default 100, range 1–1000.
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 1000))]
    pub limit: u32,
}

fn default_limit() -> u32 {
    100
}

impl Default for GlobParams {
    fn default() -> Self {
        Self {
            pattern: GlobArgs::default(),
            path: None,
            filter_mode: GlobFilterMode::Ignore,
            sort: GlobSort::Modified,
            output_mode: GlobOutput::Paths,
            offset: 0,
            limit: default_limit(),
        }
    }
}

/// Hard cap on total matches; production default, injectable in tests
/// via [`glob_impl`].
const HARD_CAP: usize = 100_000;

/// Glob entry: validate params → compile patterns → collect candidates → order →
/// paginate → fit the wire text into `budget` → assemble. The caller resolves the
/// budget at the process boundary (core never reads env).
pub fn glob_with_budget(root: &Path, params: &GlobParams, budget: u64) -> Result<Envelope> {
    glob_impl(root, params, budget, HARD_CAP)
}

/// [`glob_with_budget`] with the match hard cap injected (tests drive a small cap to
/// assert the real error text; production always passes [`HARD_CAP`]).
fn glob_impl(root: &Path, params: &GlobParams, budget: u64, hard_cap: usize) -> Result<Envelope> {
    // --- parameter validation ---
    let patterns = &params.pattern.0;
    if patterns.is_empty() {
        return Err(Error::Config("no glob pattern was given".to_string()));
    }

    // Runtime limit check (1–1000).
    if params.limit == 0 || params.limit > 1000 {
        return Err(Error::Config(format!(
            "Invalid limit: {}. Use an integer between 1 and 1000.",
            params.limit
        )));
    }

    // Compile glob patterns through the shared filter. A `None` here can
    // only follow an empty list (rejected above), but surface it as a config error rather
    // than panicking the tool if that invariant ever changes.
    let glob = match PathGlobFilter::from_patterns(patterns).map_err(|error| {
        Error::Config(format!(
            "Malformed glob pattern: {error}. Try shapes like \"**/*.rs\" or \"src/**/*.ts\"."
        ))
    })? {
        Some(filter) => filter,
        None => return Err(Error::Config("no glob pattern was given".to_string())),
    };

    // --- target resolution ---
    let target = match params.path.as_deref() {
        None => root.to_path_buf(),
        Some(rel) => crate::index::resolve_path(root, rel)?,
    };
    if !target.is_dir() {
        return Err(Error::Config(format!(
            "glob path is not a directory: {}",
            target.display()
        )));
    }

    // --- collect candidates under the glob policy ---
    let policy = TraversalPolicy::Glob {
        filter_mode: params.filter_mode,
    };
    let (mut candidates, traversal_skips) = traversal::collect(&target, root, &policy)?;

    // --- filter through glob patterns ---
    candidates.retain(|candidate| glob.admits(&candidate.rel_display));

    // --- handle zero matches ---
    if candidates.is_empty() {
        return assemble_note_page(
            "(Shown: nothing; no file matched.)".to_string(),
            0,
            budget,
            traversal_skips,
        );
    }

    // --- hard cap check ---
    if candidates.len() > hard_cap {
        return Err(hard_cap_error(hard_cap));
    }

    // --- order candidates ---
    traversal::order_glob(&mut candidates, params.sort);

    // --- pagination ---
    let total = candidates.len() as u64;
    let offset = params.offset;
    let limit = params.limit as u64;

    if offset >= total {
        let note = format!(
            "(Shown: none at offset={}; the scan saw only {} in all.)",
            params.offset,
            quantify(total, Noun::FILES)
        );
        return assemble_note_page(note, total, budget, traversal_skips);
    }

    let end = offset.saturating_add(limit).min(total);
    let page = &candidates[offset as usize..end as usize];

    // --- body-first budget fitting ---
    let fitted = fit_page(page, params, root, offset, total, budget, &traversal_skips)?;

    // --- assemble envelope ---
    assemble_envelope(fitted, budget)
}

/// The hard-cap error; the cap renders without digit separators, so the
/// production message is byte-exact `Match cap exceeded: more than 100000 files matched. ...`.
fn hard_cap_error(cap: usize) -> Error {
    Error::Config(format!(
        "Match cap exceeded: more than {cap} files matched. Narrow the pattern or the path."
    ))
}

/// One budget-fitted page: the largest prefix of the page whose rendered wire text
/// (entries + status note) fits `budget` (budget = wire). The skip
/// details stay structural (`skip_report`, off-wire); the note's skip clause discloses
/// the full listing.
struct FittedPage {
    text: String,
    /// The status note as folded into `text` (skip clause included) — the terminal
    /// carries the same render so the structured and textual forms cannot drift.
    note: String,
    returned: u64,
    shown_from: u64,
    shown_to: u64,
    total: u64,
    has_more: bool,
    next_call: Option<serde_json::Value>,
    skip_report: Option<SkipReport>,
}

/// Body-first budget fitting: the wire is the whole page — the rendered
/// entry prefix plus the status note (skip clause folded) after a blank line — and the
/// largest fitting prefix of the page is found by probe. If even a single entry plus
/// the note cannot fit, the frozen `budget_too_small` error is returned — never a
/// bodyless success.
fn fit_page(
    page: &[Candidate],
    params: &GlobParams,
    server_root: &Path,
    offset: u64,
    total: u64,
    budget: u64,
    skips: &[SkipDetail],
) -> Result<FittedPage> {
    let listed = skips.len().min(crate::envelope::SKIP_DETAIL_CAP);

    // Render lazily on demand inside each probe iteration: binary search calls probe
    // with different `shown` values, and each probe renders only the prefix it measures.
    // This avoids O(N) pre-rendering when only k entries (k << N) fit the budget.
    let probe = |shown: usize| -> Option<FittedPage> {
        let shown_to = offset + shown as u64;
        let has_more = shown_to < total;
        let continuation = has_more.then(|| continuation_json(params, shown_to).to_string());
        let terminal = glob_terminal(offset + 1, shown_to, total, has_more);
        let note = fold_skip_clause_into_note(&terminal, skips, listed);
        // Build the body by rendering only the prefix this probe iteration measures.
        let body: String = page[..shown]
            .iter()
            .map(|c| match params.output_mode {
                GlobOutput::Paths => display_path(server_root, c),
                GlobOutput::Details => render_details(server_root, c),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut text = body;
        text.push_str("\n\n");
        text.push_str(&note);
        let returned = count_tokens(&text);
        (returned <= budget).then_some(FittedPage {
            returned,
            text,
            note,
            shown_from: offset + 1,
            shown_to,
            total,
            has_more,
            next_call: continuation.and_then(|json| serde_json::from_str(&json).ok()),
            skip_report: skip_report_for(skips, listed),
        })
    };

    fit_largest(page.len(), probe).ok_or_else(|| {
        Error::Config(budget::budget_too_small(
            ToolKind::Glob.env_var(),
            budget,
            what::GLOB_TRUNCATION_NOTE,
        ))
    })
}

/// Folds the skip clause (search note grammar) into a glob status note, so the text
/// wire discloses unreachable paths the way search's notes do. `listed` is the full
/// listing (skip details are structural and never budget-fitted).
fn fold_skip_clause_into_note(note: &str, skips: &[SkipDetail], listed: usize) -> String {
    let units = super::search_render::NoteUnits {
        fixed: Vec::new(),
        fallback: None,
        tally: super::search_render::SkipTally {
            files: 0,
            unreachable: skips.len() as u64,
            listed,
        },
    };
    units.terminal_note(listed, note)
}

/// The serialized continuation arguments with only `offset` replaced — struct-serialized
/// so `skip_serializing_if` (e.g. `path` omitted when unset) is honored and any future
/// `GlobParams` field stays on the wire without a second, unchecked field list here.
fn continuation_json(params: &GlobParams, new_offset: u64) -> serde_json::Value {
    super::search_render::next_call_with_offset("glob", params, new_offset)
}

/// Builds the structural skip report (glob reports unreachable paths only):
/// the first `shown_details` details are the listed prefix, the overflow is counted in
/// `unlisted`.
fn skip_report_for(skips: &[SkipDetail], shown_details: usize) -> Option<SkipReport> {
    if skips.is_empty() {
        return None;
    }
    let listed = shown_details.min(crate::envelope::SKIP_DETAIL_CAP);
    Some(SkipReport {
        files: 0,
        unreachable: skips.len() as u64,
        details: skips.iter().take(listed).cloned().collect(),
        unlisted: (skips.len() as u64).saturating_sub(listed as u64),
    })
}

/// Renders one candidate in details mode (compact JSON). Serialized
/// positionally instead of via `serde_json::json!` so the frozen key order
/// `{"path","bytes","modified"}` holds even when serde_json's `preserve_order` feature
/// is not enabled by the feature graph (standalone core builds sort keys alphabetically).
fn render_details(server_root: &Path, candidate: &Candidate) -> String {
    let modified = format_systemtime(&candidate.mtime);
    format!(
        "{{\"path\":{},\"bytes\":{},\"modified\":{}}}",
        serde_json::to_string(&display_path(server_root, candidate))
            .expect("string serialization infallible"),
        candidate.size,
        serde_json::to_string(&modified).expect("string serialization infallible"),
    )
}

/// Terminal notes: exactly the frozen set of five variants. Glob always
/// knows the total (it collects all matches up to the hard cap before paginating), so
/// the Partial variant always carries `of {T}`.
fn glob_terminal(shown_from: u64, shown_to: u64, total: u64, has_more: bool) -> String {
    if has_more {
        format!(
            "(Shown: files {shown_from}-{shown_to} of {total}. More remain — resume from offset={shown_to}.)"
        )
    } else if shown_from == 1 && shown_to == total {
        // A single file is a single file, not "files 1-1".
        let range = if total == 1 {
            "file 1".to_string()
        } else {
            format!("files 1-{total}")
        };
        format!(
            "(Shown: {range}. All {} shown.)",
            quantify(total, Noun::FILES)
        )
    } else {
        format!("(Shown: files {shown_from}-{shown_to} of {total}. Reached the end of results.)")
    }
}

/// Formats a SystemTime as ISO 8601 with 9-digit nanoseconds (UTC). Pre-epoch mtimes
/// clamp to the epoch (they cannot occur for real files).
fn format_systemtime(time: &SystemTime) -> String {
    let duration = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    DateTime::from_timestamp(duration.as_secs() as i64, duration.subsec_nanos())
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Assembles the envelope for a successful glob result from a budget-fitted page.
fn assemble_envelope(fitted: FittedPage, budget: u64) -> Result<Envelope> {
    let unit = "files";
    let state = if fitted.has_more {
        "partial"
    } else {
        "complete"
    };
    // The probe's count was a full recount of the same text; the hard check keeps the
    // budget = wire invariant contract-level.
    let returned = budget::finish_wire(&fitted.text, Some(fitted.returned), budget)?;

    Ok(Envelope::page(Page {
        returned,
        budget,
        truncated: fitted.has_more,
        next_call: fitted.next_call,
        text: Some(fitted.text),
        terminal: Some(Terminal {
            state: state.to_string(),
            unit: unit.to_string(),
            shown_from: fitted.shown_from,
            shown_to: fitted.shown_to,
            total: Some(fitted.total),
            note: Some(fitted.note),
        }),
        skip_report: fitted.skip_report,
    }))
}

/// Assembles a note-only envelope (zero matches, or offset past the end): the wire is
/// the note itself (skip clause folded in, so unreachable paths are disclosed on the
/// text wire too), and the full (non-fitted) skip report rides in the envelope for
/// machine consumers.
fn assemble_note_page(
    note: String,
    total: u64,
    budget: u64,
    traversal_skips: Vec<SkipDetail>,
) -> Result<Envelope> {
    let unit = "files";
    // Skip report (unreachable paths only), listed to the cap.
    let skip_report = skip_report_for(&traversal_skips, traversal_skips.len());
    let listed = traversal_skips.len().min(crate::envelope::SKIP_DETAIL_CAP);

    let text = fold_skip_clause_into_note(&note, &traversal_skips, listed);
    let returned = count_tokens(&text);
    if returned > budget {
        return Err(Error::Config(budget::budget_too_small(
            ToolKind::Glob.env_var(),
            budget,
            what::GLOB_TRUNCATION_NOTE,
        )));
    }

    Ok(Envelope::page(Page {
        returned,
        budget,
        truncated: false,
        next_call: None,
        text: Some(text.clone()),
        terminal: Some(Terminal {
            state: "complete".to_string(),
            unit: unit.to_string(),
            shown_from: 0,
            shown_to: 0,
            total: Some(total),
            note: Some(text),
        }),
        skip_report,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: creates a test directory structure.
    fn test_fixture() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Create test files
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("Cargo.toml"), "[package]").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn lib() {}").unwrap();
        fs::write(root.join("src/mod.rs"), "mod lib;").unwrap();
        fs::write(root.join(".hidden.rs"), "fn hidden() {}").unwrap();
        fs::write(root.join("README.md"), "# README").unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        // A `.git` stand-in and an ignored build dir, so the prune and gitignore tests
        // observe real exclusions rather than vacuous ones.
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/debug.bin"), "binary").unwrap();

        // Create nested structure
        fs::create_dir_all(root.join("tests/fixtures")).unwrap();
        fs::write(root.join("tests/fixtures/test.rs"), "fn test() {}").unwrap();

        tmp
    }

    /// Verify that `*` does not cross `/` but `**/` does.
    #[test]
    fn glob_literal_separator_requires_double_star() {
        let tmp = test_fixture();
        let root = tmp.path();

        let params = GlobParams {
            pattern: GlobArgs(vec!["*.rs".to_string()]),
            path: None,
            filter_mode: GlobFilterMode::All,
            sort: GlobSort::Path,
            output_mode: GlobOutput::Paths,
            offset: 0,
            limit: 100,
        };

        let env = glob_with_budget(root, &params, budget::DEFAULT_TOKEN_BUDGET).unwrap();
        let text = env.text.as_ref().unwrap();

        // *.rs should only match top-level .rs files, not src/*.rs
        assert!(text.contains("main.rs"), "should match top-level .rs files");
        assert!(!text.contains("src/lib.rs"), "* should not cross /");
        assert!(
            !text.contains("Cargo.toml"),
            "should not match non-.rs files"
        );
    }

    /// Verify that `!` exclusions always veto.
    #[test]
    fn exclusion_always_vets() {
        let tmp = test_fixture();
        let root = tmp.path();

        let params = GlobParams {
            pattern: GlobArgs(vec!["**/*.rs".to_string(), "!**/test.rs".to_string()]),
            path: None,
            filter_mode: GlobFilterMode::All,
            sort: GlobSort::Path,
            output_mode: GlobOutput::Paths,
            offset: 0,
            limit: 100,
        };

        let env = glob_with_budget(root, &params, budget::DEFAULT_TOKEN_BUDGET).unwrap();
        let text = env.text.as_ref().unwrap();

        assert!(
            !text.contains("test.rs"),
            "exclusion should veto the inclusion"
        );
        assert!(text.contains("main.rs"), "other .rs files should match");
    }

    /// Regression: the default `Ignore` mode honors the full ignore stack — `.ignore` AND
    /// gitignore sources — and prunes `.git`, while
    /// non-ignored dotfiles stay collectible.
    #[test]
    fn ignore_mode_honors_dot_ignore_and_gitignore() {
        let tmp = test_fixture();
        let root = tmp.path();

        // Create a .ignore file on top of the fixture's .gitignore
        fs::write(root.join(".ignore"), "README.md\n").unwrap();

        let params = GlobParams {
            pattern: GlobArgs(vec!["**/*".to_string()]),
            path: None,
            filter_mode: GlobFilterMode::Ignore,
            sort: GlobSort::Path,
            output_mode: GlobOutput::Paths,
            offset: 0,
            limit: 1000,
        };

        let env = glob_with_budget(root, &params, budget::DEFAULT_TOKEN_BUDGET).unwrap();
        let text = env.text.as_ref().unwrap();

        // .ignore should be honored
        assert!(
            !text.contains("README.md"),
            ".ignore should filter README.md"
        );

        // .gitignore should be honored too
        assert!(
            !text.contains("target/"),
            "gitignore should filter target/: {text}"
        );

        // .git internals are pruned in every glob mode
        assert!(
            !text.contains(".git/HEAD"),
            ".git must be pruned in glob mode: {text}"
        );

        // non-ignored dotfiles are still collected
        assert!(
            text.contains(".hidden.rs"),
            "non-ignored dotfiles stay collected: {text}"
        );
    }

    /// Regression: with no explicit `sort`, the most recently modified file leads the page.
    #[test]
    fn default_sort_is_modified_newest_first() {
        let tmp = test_fixture();
        let root = tmp.path();

        // Ensure the new file's mtime is strictly newer than the fixture files'.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(root.join("newest.rs"), "fn newest() {}").unwrap();

        let params = GlobParams {
            pattern: GlobArgs(vec!["*.rs".to_string()]),
            ..GlobParams::default()
        };

        let env = glob_with_budget(root, &params, budget::DEFAULT_TOKEN_BUDGET).unwrap();
        let text = env.text.as_ref().unwrap();
        let first = text.lines().next().unwrap();
        assert!(
            first.contains("newest.rs"),
            "default sort must lead with the newest file, got: {first}"
        );
    }

    /// Verify that All mode applies no ignore filtering.
    #[test]
    fn all_mode_no_filtering() {
        let tmp = test_fixture();
        let root = tmp.path();

        let params = GlobParams {
            pattern: GlobArgs(vec!["*".to_string()]),
            path: None,
            filter_mode: GlobFilterMode::All,
            sort: GlobSort::Path,
            output_mode: GlobOutput::Paths,
            offset: 0,
            limit: 100,
        };

        let env = glob_with_budget(root, &params, budget::DEFAULT_TOKEN_BUDGET).unwrap();
        let text = env.text.as_ref().unwrap();

        // All (top-level) files should be present, including .hidden files and .gitignore.
        assert!(text.contains(".hidden.rs"));
        assert!(text.contains(".gitignore"));
    }

    /// Verify that details mode outputs compact JSON with correct shape and sorts.
    #[test]
    fn details_json_shape_and_sorts() {
        let tmp = test_fixture();
        let root = tmp.path();

        let params = GlobParams {
            pattern: GlobArgs(vec!["*.rs".to_string()]),
            path: None,
            filter_mode: GlobFilterMode::All,
            sort: GlobSort::Path,
            output_mode: GlobOutput::Details,
            offset: 0,
            limit: 100,
        };

        let env = glob_with_budget(root, &params, budget::DEFAULT_TOKEN_BUDGET).unwrap();
        let text = env.text.as_ref().unwrap();
        // The page is the JSON lines plus the status note after a blank line; only the
        // body lines are JSON objects.
        let body = text.split("\n\n(Shown:").next().unwrap();

        // Verify JSON shape
        for line in body.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(value.is_object(), "each line should be a JSON object");
            assert!(value.get("path").is_some(), "should have 'path' field");
            assert!(value.get("bytes").is_some(), "should have 'bytes' field");
            assert!(
                value.get("modified").is_some(),
                "should have 'modified' field"
            );

            // Frozen key order `{"path","bytes","modified"}` — depends on serde_json's
            // preserve_order feature, so pin it explicitly.
            let (path, bytes, modified) = (
                line.find("\"path\":").expect("path key"),
                line.find("\"bytes\":").expect("bytes key"),
                line.find("\"modified\":").expect("modified key"),
            );
            assert!(
                path < bytes && bytes < modified,
                "frozen key order path<bytes<modified, got: {line}"
            );

            // Verify modified is ISO 8601 with nanoseconds
            let modified = value["modified"].as_str().unwrap();
            assert!(modified.contains('T'), "modified should be ISO 8601");
            assert!(modified.contains('Z'), "modified should end with Z");
        }

        // Test Modified sort - ensure we have multiple files with different times
        // Sleep to ensure different timestamps
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(root.join("newer.rs"), "fn newer() {}").unwrap();

        let params_modified = GlobParams {
            pattern: GlobArgs(vec!["*.rs".to_string()]),
            path: None,
            filter_mode: GlobFilterMode::All,
            sort: GlobSort::Modified,
            output_mode: GlobOutput::Details,
            offset: 0,
            limit: 100,
        };

        let env_modified =
            glob_with_budget(root, &params_modified, budget::DEFAULT_TOKEN_BUDGET).unwrap();
        let text_modified = env_modified.text.as_ref().unwrap();

        // Should have results in different order because newer.rs was added; under
        // Modified sort (mtime desc, path asc tie-break) the newest file leads.
        assert!(
            text_modified.contains("newer.rs"),
            "newer file should be present"
        );
        let first = text_modified.lines().next().unwrap();
        assert!(
            first.contains("newer.rs"),
            "Modified sort must lead with the newest file, got: {first}"
        );
        assert_ne!(
            text, text_modified,
            "different sort orders should produce different results"
        );
    }

    /// Verify that limit is bounded 1-1000 with runtime error message.
    #[test]
    fn limit_bounds_and_runtime_message() {
        let tmp = test_fixture();
        let root = tmp.path();

        // Test limit = 0
        let params_zero = GlobParams {
            pattern: GlobArgs(vec!["*".to_string()]),
            path: None,
            filter_mode: GlobFilterMode::All,
            sort: GlobSort::Path,
            output_mode: GlobOutput::Paths,
            offset: 0,
            limit: 0,
        };

        let err = glob_with_budget(root, &params_zero, budget::DEFAULT_TOKEN_BUDGET).unwrap_err();
        assert!(
            err.to_string()
                .contains("Invalid limit: 0. Use an integer between 1 and 1000.")
        );

        // Test limit > 1000
        let params_large = GlobParams {
            pattern: GlobArgs(vec!["*".to_string()]),
            path: None,
            filter_mode: GlobFilterMode::All,
            sort: GlobSort::Path,
            output_mode: GlobOutput::Paths,
            offset: 0,
            limit: 1001,
        };

        let err = glob_with_budget(root, &params_large, budget::DEFAULT_TOKEN_BUDGET).unwrap_err();
        assert!(
            err.to_string()
                .contains("Invalid limit: 1001. Use an integer between 1 and 1000.")
        );
    }

    /// A small injected cap drives the real error text; the production
    /// default stays byte-exact `over 100000`.
    #[test]
    fn hard_cap_message() {
        let tmp = test_fixture();
        let root = tmp.path();

        let params = GlobParams {
            pattern: GlobArgs(vec!["*".to_string()]),
            path: None,
            filter_mode: GlobFilterMode::All,
            sort: GlobSort::Path,
            output_mode: GlobOutput::Paths,
            offset: 0,
            limit: 100,
        };

        // The fixture matches more than 3 files, so the injected cap trips.
        let err = glob_impl(root, &params, budget::DEFAULT_TOKEN_BUDGET, 3).unwrap_err();
        assert_eq!(
            err.to_string(),
            "configuration error: Match cap exceeded: more than 3 files matched. Narrow the pattern or the path."
        );

        // Production behavior: the frozen message with the unseparated 100000 render.
        assert_eq!(
            hard_cap_error(HARD_CAP).to_string(),
            "configuration error: Match cap exceeded: more than 100000 files matched. Narrow the pattern or the path."
        );
        assert_eq!(HARD_CAP, 100_000);
    }

    /// All five frozen terminal variants asserted byte-exact.
    #[test]
    fn terminal_notes_all_variants() {
        // A dedicated 10-file directory so the totals are exact.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        for i in 0..10 {
            fs::write(root.join(format!("f{i}.rs")), "").unwrap();
        }

        let params = |offset: u64, limit: u32, pattern: &str| GlobParams {
            pattern: GlobArgs(vec![pattern.to_string()]),
            path: None,
            filter_mode: GlobFilterMode::All,
            sort: GlobSort::Path,
            output_mode: GlobOutput::Paths,
            offset,
            limit,
        };
        let note = |env: &Envelope| -> String {
            env.terminal
                .as_ref()
                .unwrap()
                .note
                .as_ref()
                .unwrap()
                .clone()
        };

        // 1. Partial (always `of {T}` — glob always knows the total).
        let env =
            glob_with_budget(root, &params(0, 5, "*.rs"), budget::DEFAULT_TOKEN_BUDGET).unwrap();
        assert_eq!(
            note(&env),
            "(Shown: files 1-5 of 10. More remain — resume from offset=5.)"
        );

        // 2. Complete: all shown.
        let env =
            glob_with_budget(root, &params(0, 100, "*.rs"), budget::DEFAULT_TOKEN_BUDGET).unwrap();
        assert_eq!(note(&env), "(Shown: files 1-10. All 10 files shown.)");

        // 3. Complete: end of results (offset page that reaches the end).
        let env =
            glob_with_budget(root, &params(8, 5, "*.rs"), budget::DEFAULT_TOKEN_BUDGET).unwrap();
        assert_eq!(
            note(&env),
            "(Shown: files 9-10 of 10. Reached the end of results.)"
        );

        // 4. Complete: no files matched.
        let env = glob_with_budget(
            root,
            &params(0, 100, "*.nonexistent"),
            budget::DEFAULT_TOKEN_BUDGET,
        )
        .unwrap();
        assert_eq!(note(&env), "(Shown: nothing; no file matched.)");

        // 5. Complete: offset past the end.
        let env =
            glob_with_budget(root, &params(99, 100, "*.rs"), budget::DEFAULT_TOKEN_BUDGET).unwrap();
        assert_eq!(
            note(&env),
            "(Shown: none at offset=99; the scan saw only 10 files in all.)"
        );
    }

    /// Verify offset past end produces the correct note.
    #[test]
    fn offset_past_end() {
        let tmp = test_fixture();
        let root = tmp.path();

        let params = GlobParams {
            pattern: GlobArgs(vec!["*.rs".to_string()]),
            path: None,
            filter_mode: GlobFilterMode::All,
            sort: GlobSort::Path,
            output_mode: GlobOutput::Paths,
            offset: 9999,
            limit: 100,
        };

        let env = glob_with_budget(root, &params, budget::DEFAULT_TOKEN_BUDGET).unwrap();
        let note = env.terminal.as_ref().unwrap().note.as_ref().unwrap();
        assert!(note.contains("(Shown: none at offset=9999;"));
        assert!(note.contains("in all.)"));
    }

    /// The details `modified` field is the frozen `YYYY-MM-DDTHH:MM:SS.NNNNNNNNNZ` UTC
    /// shape — 9-digit nanoseconds, no calendar drift. The sub-second
    /// component must be a multiple of 100ns so the value survives Windows FILETIME
    /// truncation; the formatter preserves whatever the OS reports.
    #[test]
    fn systemtime_format_is_nine_digit_utc() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_700);
        assert_eq!(format_systemtime(&t), "2023-11-14T22:13:20.123456700Z");
        assert_eq!(
            format_systemtime(&SystemTime::UNIX_EPOCH),
            "1970-01-01T00:00:00.000000000Z"
        );
    }

    /// A tight budget degrades the page to the largest fitting prefix: the returned
    /// envelope actually fits `MINDCTX_GLOB_TOKEN_BUDGET`-style budgeting instead of
    /// blowing through it (body-first budgeting). Budget is injected via
    /// [`glob_with_budget`], the same pattern the search tests use.
    #[test]
    fn tiny_budget_degrades_page_to_fit() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // 300 long-named files: an unbudgeted full page (~2000+ tokens) is far beyond
        // the budget below, but a single entry plus the floor still fits.
        for i in 0..300 {
            fs::write(root.join(format!("module_with_long_name_{i:03}.rs")), "").unwrap();
        }

        let params = GlobParams {
            pattern: GlobArgs(vec!["*.rs".to_string()]),
            path: None,
            filter_mode: GlobFilterMode::All,
            sort: GlobSort::Path,
            output_mode: GlobOutput::Paths,
            offset: 0,
            limit: 1000,
        };

        let budget = 400;
        let env = glob_with_budget(root, &params, budget).unwrap();
        assert!(
            env.token_usage.returned <= budget,
            "returned {} must fit budget {budget}",
            env.token_usage.returned
        );
        assert_eq!(env.token_usage.budget, Some(budget));

        // The page degraded: fewer entries than matched, Partial with a continuation.
        assert!(env.truncated, "a 300-line page cannot fit 400 tokens");
        let terminal = env.terminal.as_ref().unwrap();
        assert_eq!(terminal.state, "partial");
        assert_eq!(terminal.shown_from, 1);
        assert!(
            terminal.shown_to < terminal.total.unwrap(),
            "degraded page shows fewer than all 300 entries"
        );
        let text = env.text.as_ref().unwrap();
        let body = text.split("\n\n(Shown:").next().unwrap();
        assert_eq!(
            body.lines().count() as u64,
            terminal.shown_to - terminal.shown_from + 1,
            "body lines match the shown range"
        );
        let next = env.next_call.expect("partial carries next_call");
        assert_eq!(next["tool"], "glob");
        assert_eq!(
            next["arguments"]["offset"],
            serde_json::json!(terminal.shown_to)
        );
        assert_eq!(
            terminal.note.as_deref(),
            Some(&*format!(
                "(Shown: files 1-{} of 300. More remain — resume from offset={}.)",
                terminal.shown_to, terminal.shown_to
            ))
        );
    }

    /// A budget so small that even one entry plus the mandatory truncation note cannot
    /// fit fails with the exact frozen error — never a bodyless success.
    #[test]
    fn tiny_budget_too_small_is_fatal() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("only.rs"), "").unwrap();

        let params = GlobParams {
            pattern: GlobArgs(vec!["*.rs".to_string()]),
            path: None,
            filter_mode: GlobFilterMode::All,
            sort: GlobSort::Path,
            output_mode: GlobOutput::Paths,
            offset: 0,
            limit: 100,
        };

        let err = glob_with_budget(root, &params, 1).unwrap_err();
        assert_eq!(
            err.to_string(),
            "configuration error: MINDCTX_GLOB_TOKEN_BUDGET=1 is too small to return the required glob truncation note. Increase it and retry."
        );
    }

    /// Skip details are envelope bookkeeping and never ride the text wire: the budget
    /// constrains the rendered page only, so the report lists every unreachable path
    /// (to the cap) regardless of how tight the budget is, and the note's skip clause
    /// discloses the tally.
    #[test]
    #[cfg(unix)]
    fn skip_details_are_structural_never_budget_fitted() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Plenty of matching entries with long names: the body competes with the skip
        // details for the budget, so the detail fit cannot take all of them.
        for i in 0..60 {
            fs::write(root.join(format!("module_with_long_name_{i:03}.rs")), "").unwrap();
        }
        // Several unreachable subtrees: each becomes one skip detail.
        let mut locked = Vec::new();
        for i in 0..4 {
            let dir = root.join(format!("locked{i}"));
            fs::create_dir_all(dir.join("inner")).unwrap();
            fs::write(dir.join("inner/deep.rs"), "fn deep() {}").unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
            locked.push(dir);
        }

        let params = GlobParams {
            pattern: GlobArgs(vec!["**/*.rs".to_string()]),
            path: None,
            filter_mode: GlobFilterMode::All,
            sort: GlobSort::Path,
            output_mode: GlobOutput::Paths,
            offset: 0,
            limit: 1000,
        };
        let budget = 350u64;
        let outcome = glob_with_budget(root, &params, budget);
        // Restore permissions so the TempDir can be removed even on failure.
        for dir in &locked {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env = outcome.unwrap();

        assert!(
            env.token_usage.returned <= budget,
            "returned {} must fit budget {budget}",
            env.token_usage.returned
        );

        let report = env.skip_report.expect("unreachable paths produce a report");
        assert_eq!(report.unreachable, 4);
        // Structural bookkeeping: the full listing survives any budget.
        assert_eq!(
            report.details.len(),
            4,
            "details are never budget-fitted: {}",
            report.details.len()
        );
        assert_eq!(report.unlisted, 0);
        // The text note discloses the tally so the wire stays informative.
        let note = env.terminal.as_ref().unwrap().note.as_ref().unwrap();
        assert!(note.contains("4 paths unreachable"), "skip clause: {note}");
    }

    /// Wire names are part of the tool contract.
    #[test]
    fn wire_names_are_snake_case() {
        assert_eq!(
            serde_json::to_value(GlobFilterMode::Ignore).unwrap(),
            "ignore"
        );
        assert_eq!(serde_json::to_value(GlobFilterMode::All).unwrap(), "all");
        assert_eq!(serde_json::to_value(GlobSort::Path).unwrap(), "path");
        assert_eq!(
            serde_json::to_value(GlobSort::Modified).unwrap(),
            "modified"
        );
        assert_eq!(serde_json::to_value(GlobOutput::Paths).unwrap(), "paths");
        assert_eq!(
            serde_json::to_value(GlobOutput::Details).unwrap(),
            "details"
        );
    }

    #[test]
    fn wire_names_roundtrip() {
        for mode in [GlobFilterMode::Ignore, GlobFilterMode::All] {
            let back: GlobFilterMode =
                serde_json::from_value(serde_json::to_value(mode).unwrap()).unwrap();
            assert_eq!(back, mode);
        }
        for sort in [GlobSort::Path, GlobSort::Modified] {
            let back: GlobSort =
                serde_json::from_value(serde_json::to_value(sort).unwrap()).unwrap();
            assert_eq!(back, sort);
        }
        for out in [GlobOutput::Paths, GlobOutput::Details] {
            let back: GlobOutput =
                serde_json::from_value(serde_json::to_value(out).unwrap()).unwrap();
            assert_eq!(back, out);
        }
    }
}
