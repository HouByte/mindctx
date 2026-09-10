// SPDX-License-Identifier: MIT OR Apache-2.0

//! Retrieve layer: deterministic file search + budget discipline.
//! Output modes (files_with_matches / content / count / summary), mtime-descending order, frozen
//! pagination. No relevance ranking — the agent judges which file is the target.

pub mod glob_args;
pub mod glob_tool;
pub mod grep_filter;
pub mod read;
mod search_render;
mod search_sink;
pub mod snapshot;
pub mod traversal;

#[cfg(test)]
mod search_tests;

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use grep_regex::RegexMatcherBuilder;
use serde::{Deserialize, Serialize};

use crate::budget::{self, ToolKind, what};
use crate::encoding::{Decision, EncodingOutcome, Rejection, ambiguous_message, decode_explicit};
use crate::envelope::{Envelope, Page, SkipDetail, SkipReport, Terminal};
use crate::error::{Error, Result};
use crate::index;
use crate::tokenize::{ExactPrefixCounter, count_tokens};

use glob_args::GlobArgs;
use grep_filter::PathGlobFilter;
use search_render::{
    ContentFile, ContentRenderOptions, ContentRenderer, NoteUnits, Noun, SkipTally, assemble_text,
    content_degradation_ladder, count_terminal, fit_largest, next_call_with_offset,
    normalize_multiline_pattern, offset_exhausted_note, paged_terminal, quantify,
    render_content_lines, summary_note, zero_result_note,
};
use search_sink::{
    CAPTURE_HEAP_LIMIT_BYTES, ContentWindow, LineTable, RawEntry, SinkPlan, search_file,
};

/// `head_limit` default; `0` means no entry cap — the token budget still applies.
pub const DEFAULT_HEAD_LIMIT: u32 = 250;
/// outline.depth default: only the top 2 levels by default, avoiding full-depth expansion of large files.
pub const DEFAULT_OUTLINE_DEPTH: u32 = 2;

/// The four search output modes (wire names are snake_case).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SearchOutput {
    /// Matching lines with optional context, grouped by file.
    Content,
    /// Paths of files with at least one match; nothing else per file (default).
    #[default]
    FilesWithMatches,
    /// Per-file occurrence counts plus their aggregate.
    Count,
    /// Scan the whole scope; only global occurrence/file totals (ignores head_limit/offset).
    Summary,
}

impl SearchOutput {
    fn is_default(&self) -> bool {
        *self == Self::FilesWithMatches
    }
}

/// Search parameters — the wire contract. Every defaulted
/// field is skipped during serialization so a round-trip reproduces the caller's args.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// Regex in Rust regex syntax: lookaround and backreferences are unavailable, and
    /// literal braces must be escaped.
    pub pattern: String,
    /// Target file or directory, project-relative; defaults to the project root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// File globs to filter by; a leading `!` marks an exclusion and exclusions always
    /// win. Published as an array, a bare string is accepted.
    #[serde(default, skip_serializing_if = "GlobArgs::is_empty")]
    #[schemars(with = "Vec<String>")]
    pub glob: GlobArgs,
    /// Standard file-type filter, e.g. "js", "py", "rust" (rg --type-like semantics).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
    /// files_with_matches (default) | content | count | summary.
    #[serde(default, skip_serializing_if = "SearchOutput::is_default")]
    pub output_mode: SearchOutput,
    /// Case-insensitive search (rg -i).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub case_insensitive: bool,
    /// Prefix content lines with line numbers (rg -n); defaults to true.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub line_numbers: bool,
    /// Emit each matched part on its own line (rg -o); content mode only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub only_matching: bool,
    /// Context lines printed before each match (rg -B); content mode only.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub before_context: u32,
    /// Context lines printed after each match (rg -A); content mode only.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub after_context: u32,
    /// Context on both sides of each match (rg -C); takes precedence over before/after.
    /// Content mode only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
    /// Patterns may span lines; `.` also matches newlines, and `\n` matches `\r\n`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multiline: bool,
    /// Entry cap for the page; default 250; `0` lifts the entry cap (the token budget
    /// still applies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_limit: Option<u32>,
    /// Entries to skip before head_limit applies.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub offset: u64,
    /// Single-file target only: WHATWG label to decode that one file with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// Directory target only: encoding assumed solely for files auto-detection cannot
    /// decide; a BOM, valid UTF-8, or an already-resolved file is never overridden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_encoding: Option<String>,
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            path: None,
            glob: GlobArgs::default(),
            file_type: None,
            output_mode: SearchOutput::default(),
            case_insensitive: false,
            line_numbers: true,
            only_matching: false,
            before_context: 0,
            after_context: 0,
            context: None,
            multiline: false,
            head_limit: None,
            offset: 0,
            encoding: None,
            fallback_encoding: None,
        }
    }
}

/// A file that contributed content entries (text retained for rendering).
struct ContentSource {
    rel: String,
    text: String,
}

/// Everything the per-mode fitting needs after the walk.
struct Walk {
    mode: SearchOutput,
    offset: u64,
    effective_head_limit: u64,
    scan_complete: bool,
    total_seen: u64,
    /// files mode: (rel, first matching line, its text, matched-line count).
    files: Vec<(String, u64, String, u64)>,
    /// count mode: (rel, occurrence count).
    counts: Vec<(String, u64)>,
    /// summary mode totals.
    summary_occurrences: u64,
    summary_files: u64,
    /// content mode: contributing files + entries.
    sources: Vec<ContentSource>,
    entries: Vec<(usize, RawEntry)>,
    /// skip report inputs: traversal skips lead the detail list.
    traversal_skips: u64,
    skips: Vec<SkipDetail>,
    transcoding_notes: BTreeSet<String>,
    fallback_count: u64,
    fallback_encoding: Option<String>,
}

impl Walk {
    /// The skip tally against the FULL detail list; how many details a
    /// given render shows is passed to the note builders, never baked in here.
    fn tally(&self) -> SkipTally {
        SkipTally {
            files: (self.skips.len() as u64).saturating_sub(self.traversal_skips),
            unreachable: self.traversal_skips,
            listed: self.skips.len().min(crate::envelope::SKIP_DETAIL_CAP),
        }
    }

    fn note_units(&self) -> NoteUnits {
        NoteUnits {
            fixed: self.transcoding_notes.iter().cloned().collect(),
            fallback: self.fallback_note(),
            tally: self.tally(),
        }
    }

    fn fallback_note(&self) -> Option<String> {
        let encoding = self.fallback_encoding.as_ref()?;
        (self.fallback_count > 0).then(|| {
            format!(
                "(Note: {} decoded with fallback encoding {encoding}.)",
                quantify(self.fallback_count, Noun::FILES)
            )
        })
    }

    fn unit(&self) -> &'static str {
        match self.mode {
            SearchOutput::Content | SearchOutput::Summary => "matches",
            SearchOutput::FilesWithMatches | SearchOutput::Count => "files",
        }
    }

    fn has_results(&self) -> bool {
        match self.mode {
            SearchOutput::FilesWithMatches => !self.files.is_empty(),
            SearchOutput::Count => !self.counts.is_empty(),
            SearchOutput::Content => !self.entries.is_empty(),
            SearchOutput::Summary => false,
        }
    }
}

/// The budget-fitted page ready for envelope assembly. The wire is the whole rendered
/// page (`text`: body + notes), so `returned` is its exact o200k count.
struct Fitted {
    text: Option<String>,
    returned: u64,
    state: &'static str,
    unit: &'static str,
    shown_from: u64,
    shown_to: u64,
    total: Option<u64>,
    note: String,
    next_call: Option<serde_json::Value>,
}

/// Search entry: resolve budget → validate params → build matcher → collect candidates →
/// walk in order with sealed snapshots → feed sinks → order → paginate → render →
/// fit the wire text into `budget` → assemble with the exact wire accounting → verify.
/// The caller resolves the budget at the process boundary (core never reads env).
pub fn search_with_budget(root: &Path, params: &SearchParams, budget: u64) -> Result<Envelope> {
    // --- target resolution and parameter/target mismatch errors (frozen strings) ---
    let target = match params.path.as_deref() {
        None => root.to_path_buf(),
        Some(rel) => index::resolve_in_root(root, rel)?,
    };
    if !target.exists() {
        return Err(Error::Config(format!(
            "search target does not exist: {}",
            target.display()
        )));
    }
    let single_file_target = target.is_file();
    if single_file_target && params.fallback_encoding.is_some() {
        return Err(Error::Config(
            "The fallback_encoding parameter is a directory-target setting; for a single file use encoding."
                .to_string(),
        ));
    }
    if !single_file_target && params.encoding.is_some() {
        return Err(Error::Config(
            "The encoding parameter is a single-file setting; for a directory target use fallback_encoding."
                .to_string(),
        ));
    }

    // --- matcher (multiline pattern preprocessing first) ---
    let multiline = params.multiline;
    let pattern = if multiline {
        normalize_multiline_pattern(&params.pattern)
    } else {
        params.pattern.clone()
    };
    let mut builder = RegexMatcherBuilder::new();
    builder
        .crlf(true)
        .multi_line(true)
        .case_insensitive(params.case_insensitive)
        .dot_matches_new_line(multiline);
    if multiline {
        builder.line_terminator(None);
    }
    let matcher = builder.build(&pattern).map_err(|error| {
        Error::Config(format!(
            "Invalid regex pattern: {error}\nNote: patterns follow Rust regex syntax — lookaround and backreferences are unsupported; write literal braces escaped."
        ))
    })?;

    // --- glob and type filters ---
    let glob = PathGlobFilter::from_patterns(&params.glob.0).map_err(|error| {
        Error::Config(format!(
            "Malformed glob pattern: {error}. Try shapes like \"*.rs\" or \"**/*.{{ts,tsx}}\"."
        ))
    })?;
    let types = build_type_filter(params.file_type.as_deref())?;

    // --- candidates (mtime-descending) ---
    // The search root itself must be reachable (checked above); an unreachable subtree
    // only narrows the candidate set, so a zero-candidate walk still returns a
    // zero-result page that carries the skip report instead of failing whole.
    let (candidates, traversal_skips) =
        collect_candidates(&target, single_file_target, glob.as_ref(), types.as_ref())?;

    // --- pagination state ---
    let mode = params.output_mode;
    let budget_entry_limit = 1.max(budget.saturating_mul(4).saturating_add(1));
    let effective_head_limit = match params.head_limit {
        Some(0) => budget_entry_limit,
        Some(limit) => u64::from(limit).min(budget_entry_limit),
        None => u64::from(DEFAULT_HEAD_LIMIT).min(budget_entry_limit),
    };
    let probe_limit = effective_head_limit.saturating_add(1);
    let (before_context, after_context) = if mode == SearchOutput::Content {
        match params.context {
            Some(context) => (u64::from(context), u64::from(context)),
            None => (
                u64::from(params.before_context),
                u64::from(params.after_context),
            ),
        }
    } else {
        (0, 0)
    };
    let only_matching = params.only_matching;
    let plan = match mode {
        SearchOutput::FilesWithMatches => SinkPlan::Files,
        SearchOutput::Count | SearchOutput::Summary => SinkPlan::Count,
        SearchOutput::Content if only_matching || multiline => SinkPlan::ContentOccurrence,
        SearchOutput::Content => SinkPlan::ContentLine,
    };

    // --- walk candidates in order ---
    let mut offset_remaining = params.offset;
    let mut collected = 0u64;
    let mut total_seen = 0u64;
    let mut scan_complete = true;
    let mut walk = Walk {
        mode,
        offset: params.offset,
        effective_head_limit,
        scan_complete: true,
        total_seen: 0,
        files: Vec::new(),
        counts: Vec::new(),
        summary_occurrences: 0,
        summary_files: 0,
        sources: Vec::new(),
        entries: Vec::new(),
        traversal_skips: traversal_skips.len() as u64,
        skips: traversal_skips,
        transcoding_notes: BTreeSet::new(),
        fallback_count: 0,
        fallback_encoding: params.fallback_encoding.clone(),
    };

    for candidate in &candidates {
        let rel = candidate.rel_display.clone();
        let snapshot = match snapshot::Snapshot::open(&candidate.path, ToolKind::Search) {
            Ok(snapshot) => snapshot,
            Err(error @ Error::FileChanged { .. }) => {
                if single_file_target {
                    return Err(error);
                }
                walk.skips.push(SkipDetail {
                    path: rel,
                    reason: error.to_string(),
                });
                continue;
            }
            Err(error) => {
                if single_file_target {
                    return Err(error);
                }
                walk.skips.push(SkipDetail {
                    path: rel,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let decoded = decode_candidate(
            &snapshot,
            params.encoding.as_deref(),
            params.fallback_encoding.as_deref(),
        );
        let text = match decoded {
            Decoded::Binary => continue,
            Decoded::Skip { reason, ambiguity } => {
                if single_file_target {
                    return Err(Error::Encoding(match ambiguity {
                        Some(candidates) => ambiguous_message(&rel, &candidates),
                        None => reason,
                    }));
                }
                walk.skips.push(SkipDetail { path: rel, reason });
                continue;
            }
            Decoded::Text {
                text,
                notes,
                used_fallback,
            } => {
                for note in notes {
                    walk.transcoding_notes.insert(note);
                }
                if used_fallback {
                    walk.fallback_count += 1;
                }
                text
            }
        };
        // files/count plans only consume the outcome's counters (no per-line window), so
        // the full line table is never materialized there; content plans map matches onto
        // lines and need it. Files mode additionally stops the scan at the first match.
        let lines = matches!(plan, SinkPlan::ContentLine | SinkPlan::ContentOccurrence)
            .then(|| LineTable::new(&text));
        let window = ContentWindow {
            skip_entries: offset_remaining,
            max_selected: probe_limit - collected,
            before_context,
            after_context,
        };
        match search_file(
            &matcher,
            plan,
            &text,
            lines.as_ref(),
            window,
            multiline,
            CAPTURE_HEAP_LIMIT_BYTES,
        ) {
            Err(error) => {
                if single_file_target {
                    return Err(Error::Index(error.to_string()));
                }
                walk.skips.push(SkipDetail {
                    path: rel,
                    reason: error.to_string(),
                });
            }
            Ok(outcome) => {
                match mode {
                    SearchOutput::FilesWithMatches | SearchOutput::Count => {
                        let matched = match mode {
                            SearchOutput::FilesWithMatches => outcome.matched_lines > 0,
                            _ => outcome.occurrence_total > 0,
                        };
                        if matched {
                            total_seen += 1;
                            if offset_remaining > 0 {
                                offset_remaining -= 1;
                            } else {
                                collected += 1;
                                accumulate(&mut walk, &rel, &text, &outcome);
                            }
                        }
                    }
                    SearchOutput::Content => {
                        let seen = outcome.entries_seen;
                        total_seen += seen;
                        offset_remaining -= offset_remaining.min(seen);
                        collected += outcome.entries.len() as u64;
                        accumulate(&mut walk, &rel, &text, &outcome);
                    }
                    SearchOutput::Summary => {
                        accumulate(&mut walk, &rel, &text, &outcome);
                    }
                }
                if mode != SearchOutput::Summary && collected >= probe_limit {
                    scan_complete = false;
                    break;
                }
            }
        }
    }
    walk.scan_complete = scan_complete;
    walk.total_seen = total_seen;

    // --- render, fit, assemble ---
    if mode == SearchOutput::Summary {
        return assemble_summary(&walk, budget);
    }
    if !walk.has_results() {
        return assemble_note_only(&walk, budget);
    }
    let fitted = match mode {
        SearchOutput::FilesWithMatches => fit_files(&walk, params, budget)?,
        SearchOutput::Count => fit_count(&walk, params, budget)?,
        SearchOutput::Content => fit_content(&walk, params, budget, single_file_target)?,
        SearchOutput::Summary => unreachable!("summary handled above"),
    };
    assemble_envelope(&walk, fitted, budget)
}

/// Accumulates one file's search outcome into the walk; returns whether the file matched
/// (one entry) for the file/count entry granularity.
fn accumulate(walk: &mut Walk, rel: &str, text: &str, outcome: &search_sink::SinkOutcome) -> bool {
    match walk.mode {
        SearchOutput::FilesWithMatches => {
            if outcome.matched_lines == 0 {
                return false;
            }
            let (line, first) = outcome.first_match.clone().unwrap_or((1, String::new()));
            walk.files
                .push((rel.to_string(), line, first, outcome.matched_lines));
            true
        }
        SearchOutput::Count | SearchOutput::Summary => {
            if outcome.occurrence_total == 0 {
                return false;
            }
            if walk.mode == SearchOutput::Count {
                walk.counts
                    .push((rel.to_string(), outcome.occurrence_total));
            } else {
                walk.summary_occurrences += outcome.occurrence_total;
                walk.summary_files += 1;
            }
            true
        }
        SearchOutput::Content => {
            if outcome.entries.is_empty() {
                return false;
            }
            let index = walk.sources.len();
            walk.sources.push(ContentSource {
                rel: rel.to_string(),
                text: text.to_string(),
            });
            walk.entries
                .extend(outcome.entries.iter().cloned().map(|entry| (index, entry)));
            true
        }
    }
}

/// Why a candidate file cannot be searched. The decoded text borrows the sealed bytes
/// on the UTF-8 fast path (`'a` = the snapshot bytes' lifetime); only legacy/BOM
/// rescues own their text.
enum Decoded<'a> {
    /// Decoded text with advisory notes; `used_fallback` marks a caller-fallback rescue.
    Text {
        text: std::borrow::Cow<'a, str>,
        notes: Vec<String>,
        used_fallback: bool,
    },
    /// Binary content: silently excluded.
    Binary,
    /// Unusable as text, with the frozen skip reason and the ambiguity candidates when
    /// the rejection is an ambiguity (drives the single-file hard error message).
    Skip {
        reason: String,
        ambiguity: Option<Vec<&'static str>>,
    },
}

fn decode_candidate<'a>(
    snapshot: &'a snapshot::Snapshot,
    encoding: Option<&str>,
    fallback: Option<&str>,
) -> Decoded<'a> {
    if let Some(label) = encoding {
        return match decode_explicit(&snapshot.bytes, label) {
            Ok(Decision::Text { decoded, notes, .. }) => Decoded::Text {
                text: decoded,
                notes,
                used_fallback: false,
            },
            Ok(_) => Decoded::Skip {
                reason: Rejection::Undecodable.skip_reason(),
                ambiguity: None,
            },
            Err(rejection) => Decoded::Skip {
                reason: rejection.skip_reason(),
                ambiguity: None,
            },
        };
    }
    let outcome = match snapshot.validate_encoding() {
        Ok(outcome) => outcome,
        Err(error) => {
            return Decoded::Skip {
                reason: error.to_string(),
                ambiguity: None,
            };
        }
    };
    match outcome {
        EncodingOutcome::Text { decoded, notes, .. } => Decoded::Text {
            text: decoded,
            notes,
            used_fallback: false,
        },
        EncodingOutcome::Binary => Decoded::Binary,
        EncodingOutcome::Skipped { reason } => {
            // fallback_encoding rescues only files auto-detection rejected, never a BOM
            // mismatch.
            if let Some(fallback) = fallback
                && reason != "BOM mismatch"
                && let Ok(Decision::Text { decoded, .. }) =
                    decode_explicit(&snapshot.bytes, fallback)
            {
                return Decoded::Text {
                    text: decoded,
                    notes: Vec::new(),
                    used_fallback: true,
                };
            }
            let ambiguity = match crate::encoding::decide(&snapshot.bytes) {
                Decision::Rejected {
                    report: Rejection::Ambiguous { clean_decodes },
                } => Some(clean_decodes),
                _ => None,
            };
            Decoded::Skip { reason, ambiguity }
        }
    }
}

/// Collects the candidate universe for one search: the single file itself, or the
/// traversal under `TraversalPolicy::Search` ordered mtime-descending, both filtered
/// through the glob and type filters.
fn collect_candidates(
    target: &Path,
    single_file_target: bool,
    glob: Option<&PathGlobFilter>,
    types: Option<&ignore::types::Types>,
) -> Result<(Vec<traversal::Candidate>, Vec<SkipDetail>), Error> {
    if single_file_target {
        let metadata = std::fs::metadata(target)?;
        let rel = target
            .file_name()
            .map(|name| name.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| target.display().to_string());
        let candidate = traversal::Candidate {
            path: target.to_path_buf(),
            rel_display: rel,
            mtime: metadata
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            size: metadata.len(),
        };
        if passes_filters(&candidate, glob, types) {
            return Ok((vec![candidate], Vec::new()));
        }
        return Ok((Vec::new(), Vec::new()));
    }
    let (mut candidates, skips) = traversal::collect(target, &traversal::TraversalPolicy::Search)?;
    traversal::order_search(&mut candidates);
    candidates.retain(|candidate| passes_filters(candidate, glob, types));
    Ok((candidates, skips))
}

fn passes_filters(
    candidate: &traversal::Candidate,
    glob: Option<&PathGlobFilter>,
    types: Option<&ignore::types::Types>,
) -> bool {
    let glob_ok = glob.is_none_or(|filter| filter.admits(&candidate.rel_display));
    let types_ok = types.is_none_or(|set| set.matched(&candidate.path, false).is_whitelist());
    glob_ok && types_ok
}

/// Standard file type filter: `ignore::TypesBuilder` with `add_defaults()`
/// plus the selection; unknown types fail with the frozen message.
fn build_type_filter(file_type: Option<&str>) -> Result<Option<ignore::types::Types>, Error> {
    let Some(name) = file_type else {
        return Ok(None);
    };
    let mut builder = ignore::types::TypesBuilder::new();
    builder.add_defaults();
    builder.select(name);
    builder.build().map(Some).map_err(|_| {
        Error::Config(format!(
            "Unknown file type: \"{name}\". Filter by glob instead, or pick a standard type such as js, py, rust, go, java."
        ))
    })
}

/// The serialized continuation arguments with only `offset` replaced (wire contract:
/// `{"tool":"search","arguments":{...original args with offset replaced...}}`).
fn continuation_args(params: &SearchParams, new_offset: u64) -> serde_json::Value {
    next_call_with_offset("search", params, new_offset)
}

/// The page tail the fitters measure: blank-line separator plus the note lines —
/// exactly what `assemble_text` appends to the body.
fn page_tail(units: &NoteUnits, listed: usize, terminal: &str) -> String {
    format!("\n\n{}", units.text_notes(listed, terminal).join("\n"))
}

/// Prefix counters over a one-line-per-entry body: counter k holds the exact
/// incremental state after the first k body lines (joined with `\n`), so
/// `count_with_tail` yields the exact page count for any prefix without re-rendering
/// or re-encoding the whole page per probe.
fn line_prefix_counters(lines: &[String]) -> Vec<ExactPrefixCounter> {
    let mut counters = Vec::with_capacity(lines.len() + 1);
    counters.push(ExactPrefixCounter::new());
    let mut counter = ExactPrefixCounter::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            counter.push_str("\n");
        }
        counter.push_str(line);
        counters.push(counter.clone());
    }
    counters
}

/// Common per-page computation shared by the files/count/content fitting probes.
struct PageInputs<'a> {
    walk: &'a Walk,
    params: &'a SearchParams,
    /// The full skip-detail listing (skip details are structural and off-wire, so the
    /// note's clause always discloses the whole list — no budget fitting of details).
    listed: usize,
}

impl PageInputs<'_> {
    /// State and continuation for a page showing `shown` entries; the caller renders
    /// the terminal note itself (the wire text must match what is measured).
    fn page_state(&self, unit: &'static str, shown: u64, has_more: bool) -> Option<FittedState> {
        let state = if has_more { "partial" } else { "complete" };
        let total = (self.walk.scan_complete).then_some(self.walk.total_seen);
        let continuation = has_more.then(|| {
            serde_json::to_string(&continuation_args(self.params, self.walk.offset + shown))
                .unwrap_or_default()
        });
        let next_call = continuation
            .as_ref()
            .and_then(|json| serde_json::from_str(json).ok());
        Some(FittedState {
            state,
            unit,
            shown_from: if shown > 0 { self.walk.offset + 1 } else { 0 },
            shown_to: if shown > 0 {
                self.walk.offset + shown
            } else {
                0
            },
            total,
            next_call,
        })
    }
}

/// The parts of a fitted page that only depend on the pagination state.
struct FittedState {
    state: &'static str,
    unit: &'static str,
    shown_from: u64,
    shown_to: u64,
    total: Option<u64>,
    next_call: Option<serde_json::Value>,
}

/// files_with_matches fitting: one path per line. The body lines are independent, so
/// prefix page counts come from one incremental counter walk; a probe then measures
/// only its own terminal-note tail.
fn fit_files(walk: &Walk, params: &SearchParams, budget: u64) -> Result<Fitted, Error> {
    let inputs = PageInputs {
        walk,
        params,
        listed: walk.skips.len().min(crate::envelope::SKIP_DETAIL_CAP),
    };
    let units = walk.note_units();
    let body: Vec<String> = walk.files.iter().map(|(rel, ..)| rel.clone()).collect();
    let counters = line_prefix_counters(&body);
    let tail = |shown: usize| -> String {
        let has_more = (shown as u64) < walk.files.len() as u64 || !walk.scan_complete;
        let terminal = paged_terminal(
            Noun::FILES,
            walk.offset,
            shown as u64,
            has_more,
            walk.total_seen,
        );
        page_tail(&units, inputs.listed, &terminal)
    };
    let fits = |shown: usize| -> bool {
        shown > 0 && counters[shown].count_with_tail(&tail(shown)) <= budget
    };
    let initial = (walk.effective_head_limit as usize).min(walk.files.len());
    let Some(shown) = fit_largest(initial, |shown| fits(shown).then_some(shown)) else {
        return Err(budget::budget_too_small_error(
            ToolKind::Search,
            budget,
            what::GREP_CONTINUATION_NOTE,
        ));
    };
    let state = inputs
        .page_state(
            "files",
            shown as u64,
            tail_has_more(shown, walk.files.len(), walk.scan_complete),
        )
        .expect("page state is structural");
    let terminal = paged_terminal(
        Noun::FILES,
        walk.offset,
        shown as u64,
        tail_has_more(shown, walk.files.len(), walk.scan_complete),
        walk.total_seen,
    );
    let text = assemble_text(&body[..shown], &units.text_notes(inputs.listed, &terminal))
        .expect("a fitted page has a nonempty body");
    Ok(Fitted {
        text: Some(text),
        returned: counters[shown].count_with_tail(&tail(shown)),
        state: state.state,
        unit: state.unit,
        shown_from: state.shown_from,
        shown_to: state.shown_to,
        total: state.total,
        note: units.terminal_note(inputs.listed, &terminal),
        next_call: state.next_call,
    })
}

/// `has_more` for a page showing `shown` of `total` entries under the pagination state.
fn tail_has_more(shown: usize, total: usize, scan_complete: bool) -> bool {
    (shown as u64) < total as u64 || !scan_complete
}

/// count fitting: one `"{path}:{count}"` line per file (same prefix-count fitting as
/// files mode).
fn fit_count(walk: &Walk, params: &SearchParams, budget: u64) -> Result<Fitted, Error> {
    let inputs = PageInputs {
        walk,
        params,
        listed: walk.skips.len().min(crate::envelope::SKIP_DETAIL_CAP),
    };
    let units = walk.note_units();
    let mut occurrence_prefix = Vec::with_capacity(walk.counts.len() + 1);
    occurrence_prefix.push(0u64);
    for (_, count) in &walk.counts {
        let next = occurrence_prefix
            .last()
            .copied()
            .unwrap_or(0)
            .saturating_add(*count);
        occurrence_prefix.push(next);
    }
    let body: Vec<String> = walk
        .counts
        .iter()
        .map(|(rel, count)| format!("{rel}:{count}"))
        .collect();
    let counters = line_prefix_counters(&body);
    let tail = |shown: usize| -> String {
        let has_more = tail_has_more(shown, walk.counts.len(), walk.scan_complete);
        let terminal = count_terminal(
            walk.offset,
            shown as u64,
            occurrence_prefix[shown],
            has_more,
            walk.total_seen,
        );
        page_tail(&units, inputs.listed, &terminal)
    };
    let fits = |shown: usize| -> bool {
        shown > 0 && counters[shown].count_with_tail(&tail(shown)) <= budget
    };
    let initial = (walk.effective_head_limit as usize).min(walk.counts.len());
    let Some(shown) = fit_largest(initial, |shown| fits(shown).then_some(shown)) else {
        return Err(budget::budget_too_small_error(
            ToolKind::Search,
            budget,
            what::GREP_CONTINUATION_NOTE,
        ));
    };
    let state = inputs
        .page_state(
            "files",
            shown as u64,
            tail_has_more(shown, walk.counts.len(), walk.scan_complete),
        )
        .expect("page state is structural");
    let terminal = count_terminal(
        walk.offset,
        shown as u64,
        occurrence_prefix[shown],
        tail_has_more(shown, walk.counts.len(), walk.scan_complete),
        walk.total_seen,
    );
    let text = assemble_text(&body[..shown], &units.text_notes(inputs.listed, &terminal))
        .expect("a fitted page has a nonempty body");
    Ok(Fitted {
        text: Some(text),
        returned: counters[shown].count_with_tail(&tail(shown)),
        state: state.state,
        unit: state.unit,
        shown_from: state.shown_from,
        shown_to: state.shown_to,
        total: state.total,
        note: units.terminal_note(inputs.listed, &terminal),
        next_call: state.next_call,
    })
}

/// content fitting: the rendered page (degradation ladder); the outer search fits the
/// shown count, the ladder fits context depth and match window. Each (depth, window)
/// setting streams its body once and records an exact prefix token count per shown
/// count ([`ExactPrefixCounter`] + the still-pending tail rendered per prefix), so a
/// probe measures only its own note tail instead of re-rendering and re-encoding the
/// whole page. The winning page is then rendered once, exactly as before.
fn fit_content(
    walk: &Walk,
    params: &SearchParams,
    budget: u64,
    single_file_target: bool,
) -> Result<Fitted, Error> {
    let listed = walk.skips.len().min(crate::envelope::SKIP_DETAIL_CAP);
    let line_numbers = params.line_numbers;
    let only_matching = params.only_matching;
    let (req_before, req_after) = match params.context {
        Some(context) => (u64::from(context), u64::from(context)),
        None => (
            u64::from(params.before_context),
            u64::from(params.after_context),
        ),
    };
    let max_context = req_before.max(req_after) as usize;
    let units = walk.note_units();
    // Line tables are built once over the retained texts; every probe borrows them.
    let tables: Vec<LineTable<'_>> = walk
        .sources
        .iter()
        .map(|source| LineTable::new(&source.text))
        .collect();
    let files: Vec<ContentFile<'_>> = walk
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| ContentFile {
            rel: &source.rel,
            table: &tables[index],
        })
        .collect();

    // Per-setting probe tables, built lazily: (depth, window) → the exact page token
    // count for every shown prefix (index = shown; slot 0 is never probed — an empty
    // body renders no page).
    let build_counts = |depth: usize, window: usize| -> Vec<u64> {
        let options = ContentRenderOptions {
            before: (req_before as usize).min(depth),
            after: (req_after as usize).min(depth),
            match_window: window,
            line_numbers,
            only_matching,
            single_file_target,
        };
        let mut renderer = ContentRenderer::new(&files, options);
        let mut counter = ExactPrefixCounter::new();
        let mut counts = Vec::with_capacity(walk.entries.len() + 1);
        counts.push(u64::MAX);
        let mut emitted = 0usize;
        for (index, (file_index, entry)) in walk.entries.iter().enumerate() {
            renderer.push(*file_index, entry);
            for line in &renderer.lines()[emitted..] {
                if emitted > 0 {
                    counter.push_str("\n");
                }
                counter.push_str(line);
                emitted += 1;
            }
            // The pending tail (lines still open to later entries) rendered against the
            // prefix state, joined onto the emitted body exactly like `assemble_text`.
            let pending = renderer.pending_lines();
            let mut tail = String::new();
            if !pending.is_empty() {
                if emitted > 0 {
                    tail.push('\n');
                }
                tail.push_str(&pending.join("\n"));
            }
            let shown = index + 1;
            let has_more = tail_has_more(shown, walk.entries.len(), walk.scan_complete);
            let terminal = paged_terminal(
                Noun::RESULTS,
                walk.offset,
                shown as u64,
                has_more,
                walk.total_seen,
            );
            tail.push_str(&page_tail(&units, listed, &terminal));
            counts.push(counter.count_with_tail(&tail));
        }
        counts
    };
    let mut settings: HashMap<(usize, usize), Vec<u64>> = HashMap::new();
    let initial = (walk.effective_head_limit as usize).min(walk.entries.len());
    let winning = fit_largest(initial, |shown| {
        content_degradation_ladder(max_context, |depth, window| {
            let counts = settings
                .entry((depth, window))
                .or_insert_with(|| build_counts(depth, window));
            let fits = counts[shown] <= budget;
            fits.then_some((shown, depth, window))
        })
    });
    let Some((shown, depth, window)) = winning else {
        return Err(budget::budget_too_small_error(
            ToolKind::Search,
            budget,
            what::GREP_CONTINUATION_NOTE,
        ));
    };

    let inputs = PageInputs {
        walk,
        params,
        listed,
    };
    let has_more = tail_has_more(shown, walk.entries.len(), walk.scan_complete);
    let terminal = paged_terminal(
        Noun::RESULTS,
        walk.offset,
        shown as u64,
        has_more,
        walk.total_seen,
    );
    let state = inputs
        .page_state("matches", shown as u64, has_more)
        .expect("page state is structural");
    let selected = &walk.entries[..shown];
    let body = render_content_lines(
        &files,
        selected,
        ContentRenderOptions {
            before: (req_before as usize).min(depth),
            after: (req_after as usize).min(depth),
            match_window: window,
            line_numbers,
            only_matching,
            single_file_target,
        },
    );
    let text = assemble_text(&body, &units.text_notes(listed, &terminal))
        .expect("a fitted page has a nonempty body");
    Ok(Fitted {
        text: Some(text),
        returned: settings[&(depth, window)][shown],
        state: state.state,
        unit: state.unit,
        shown_from: state.shown_from,
        shown_to: state.shown_to,
        total: state.total,
        note: units.terminal_note(listed, &terminal),
        next_call: state.next_call,
    })
}

/// Summary assembly: no results, no text, only the frozen totals note (skip clause
/// folded in). The wire is the note — its exact count is the whole accounting.
fn assemble_summary(walk: &Walk, budget: u64) -> Result<Envelope> {
    let listed = walk.skips.len().min(crate::envelope::SKIP_DETAIL_CAP);
    let note = walk.note_units().terminal_note(
        listed,
        &summary_note(walk.summary_occurrences, walk.summary_files),
    );
    let returned = count_tokens(&note);
    if returned > budget {
        return Err(budget::budget_too_small_error(
            ToolKind::Search,
            budget,
            what::GREP_CONTINUATION_NOTE,
        ));
    }
    Ok(Envelope::page(Page {
        returned,
        budget,
        truncated: false,
        next_call: None,
        text: None,
        terminal: Some(Terminal {
            state: "complete".to_string(),
            unit: "matches".to_string(),
            shown_from: 0,
            shown_to: 0,
            total: Some(walk.summary_occurrences),
            note: Some(note),
        }),
        skip_report: skip_report(walk, listed),
    }))
}

/// Note-only assembly (zero results, or offset past the end): the frozen note and the
/// skip report, never a fabricated body. The wire is the note (advisory notes prepended
/// when present) — its exact count is the whole accounting.
fn assemble_note_only(walk: &Walk, budget: u64) -> Result<Envelope> {
    let terminal = if walk.total_seen == 0 {
        zero_result_note(walk.mode).to_string()
    } else {
        offset_exhausted_note(walk.mode, walk.offset, walk.total_seen)
    };
    let listed = walk.skips.len().min(crate::envelope::SKIP_DETAIL_CAP);
    let units = walk.note_units();
    let note = units.terminal_note(listed, &terminal);
    // Advisory notes ride ahead of the note when they exist; without them the note is
    // the whole page and stays in the structured terminal only (render picks it up).
    let advisory: Vec<String> = units
        .text_notes(listed, &terminal)
        .into_iter()
        .filter(|line| line.starts_with("(Note:"))
        .collect();
    let text = if advisory.is_empty() {
        None
    } else {
        let mut page = advisory.join("\n");
        page.push('\n');
        page.push_str(&note);
        Some(page)
    };
    let wire = text.as_deref().unwrap_or(&note);
    let returned = count_tokens(wire);
    if returned > budget {
        return Err(budget::budget_too_small_error(
            ToolKind::Search,
            budget,
            what::GREP_CONTINUATION_NOTE,
        ));
    }
    Ok(Envelope::page(Page {
        returned,
        budget,
        truncated: false,
        next_call: None,
        text,
        terminal: Some(Terminal {
            state: "complete".to_string(),
            unit: walk.unit().to_string(),
            shown_from: 0,
            shown_to: 0,
            total: Some(walk.total_seen),
            note: Some(note),
        }),
        skip_report: skip_report(walk, listed),
    }))
}

/// The capped skip report: traversal skips lead, per-file search skips follow; the cap
/// overflow is counted in `unlisted`. `Some` only when there is something to report.
fn skip_report(walk: &Walk, listed: usize) -> Option<SkipReport> {
    if walk.skips.is_empty() {
        return None;
    }
    Some(SkipReport {
        files: walk.tally().files,
        unreachable: walk.traversal_skips,
        details: walk.skips.iter().take(listed).cloned().collect(),
        unlisted: (walk.skips.len() as u64).saturating_sub(listed as u64),
    })
}

/// Assembles the final envelope from the fitted page: `truncated` = partial state, the
/// structured terminal, the skip report, and the exact wire token accounting (the
/// probe's count was a full recount of the same text; the hard check keeps the budget
/// = wire invariant contract-level).
fn assemble_envelope(walk: &Walk, fitted: Fitted, budget: u64) -> Result<Envelope> {
    let returned = budget::finish_wire(
        fitted.text.as_deref().unwrap_or_default(),
        Some(fitted.returned),
        budget,
    )?;
    Ok(Envelope::page(Page {
        returned,
        budget,
        truncated: fitted.state == "partial",
        next_call: fitted.next_call,
        text: fitted.text,
        terminal: Some(Terminal {
            state: fitted.state.to_string(),
            unit: fitted.unit.to_string(),
            shown_from: fitted.shown_from,
            shown_to: fitted.shown_to,
            total: fitted.total,
            note: Some(fitted.note),
        }),
        skip_report: skip_report(walk, walk.skips.len().min(crate::envelope::SKIP_DETAIL_CAP)),
    }))
}
