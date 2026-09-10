// SPDX-License-Identifier: MIT OR Apache-2.0

//! read — single + batch. 1-based `N\tcontent` lines, `limit` omitted means the budget is the
//! only ceiling, long-line truncation marker, empty-file / offset-past-EOF warnings, exact
//! terminal-note grammar, batch with one shared budget fitted as the largest prefix of whole
//! segments, per-entry problems inlined without aborting neighbors, exact continuation JSON.
//!
//! Budget = wire: `token_usage.returned` is the exact o200k count of the rendered page
//! (body + status note), hard-verified by `budget::finish_wire`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::search_render::fit_largest;
use super::snapshot;
use crate::budget::{self, ToolKind, WireTail, what};
use crate::encoding::{Decision, Rejection, ambiguous_message, decode_explicit, has_binary_magic};
use crate::envelope::{Envelope, Page, Terminal};
use crate::error::{Error, Result};
use crate::index;
use crate::tokenize::{ExactPrefixCounter, count_tokens};

/// Lines longer than this many chars are truncated with a marker.
const MAX_LINE_CHARS: usize = 2000;
/// Batch entries accepted per call.
const MAX_BATCH_ENTRIES: usize = 32;
/// `total_lines` is exact only up to this file size; above it the terminal reports an
/// unknown total.
const TOTAL_COUNT_SIZE_LIMIT: u64 = 64 * 1024 * 1024;

/// Read parameters — the wire contract. Every defaulted
/// field is skipped during serialization so a round-trip reproduces the caller's args.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReadParams {
    /// Single file path (mutually exclusive with `files`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Batch entries (mutually exclusive with `file_path`); 1..=32.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 32))]
    pub files: Option<Vec<BatchEntry>>,
    /// 1-based line offset (minimum 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub offset: Option<u64>,
    /// Line limit (omitted = unbounded; the budget is the only ceiling).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub limit: Option<u64>,
    /// WHATWG encoding label to decode the file with (single-file reads).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
}

/// One batch read entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BatchEntry {
    /// Project-relative file path.
    pub path: String,
    /// 1-based line offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub offset: Option<u64>,
    /// Line limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub limit: Option<u64>,
    /// WHATWG encoding label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
}

/// Read entry: validate exactly-one-of → single or batch → assemble with the wire-tail
/// reservation (single file) or whole-page measurement (batch). `budget` is the shared
/// budget for the whole response, resolved by the caller at the process boundary.
pub fn read_with_budget(root: &Path, params: &ReadParams, budget: u64) -> Result<Envelope> {
    match (&params.file_path, &params.files) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(Error::Config(
                "Provide exactly one of file_path or files.".to_string(),
            ));
        }
        _ => {}
    }
    if let Some(file_path) = &params.file_path {
        return read_single(
            root,
            file_path,
            params.offset,
            params.limit,
            params.encoding.as_deref(),
            budget,
        );
    }
    let files = params
        .files
        .as_deref()
        .expect("exactly-one-of validated above");
    if files.is_empty() || files.len() > MAX_BATCH_ENTRIES {
        return Err(Error::Config(format!(
            "Invalid files value: expected 1 to {MAX_BATCH_ENTRIES} entries, got {}.",
            files.len()
        )));
    }
    // Top-level limit is ALLOWED with files (backfilled per entry); offset/encoding are not.
    for (parameter, present) in [
        ("offset", params.offset.is_some()),
        ("encoding", params.encoding.is_some()),
    ] {
        if present {
            return Err(Error::Config(format!(
                "{parameter} is a top-level setting and cannot be combined with files; \
                 put it on each files entry instead."
            )));
        }
    }
    read_batch(root, files, params.limit, budget)
}

// ---------------------------------------------------------------------------
// single-file read
// ---------------------------------------------------------------------------

/// Single-file read: seal a snapshot → binary/encoding ladder → line window →
/// budget-first greedy fit (largest prefix of lines whose body + floor fits) → envelope.
fn read_single(
    root: &Path,
    file_path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
    encoding: Option<&str>,
    budget: u64,
) -> Result<Envelope> {
    let resolved = index::resolve_in_root(root, file_path)?;
    if resolved.is_dir() {
        return Err(Error::Config(format!(
            "Cannot read directory as text: {file_path}."
        )));
    }
    let snap = snapshot::Snapshot::open(&resolved, ToolKind::Read)?;
    // Magic signatures win before the ladder: some binaries (ZIP headers) are valid UTF-8.
    if has_binary_magic(&snap.bytes) {
        return Err(binary_error(file_path, "binary"));
    }
    let (text, _notes) = decode_text(&snap.bytes, encoding, file_path).map_err(Error::Encoding)?;

    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let total = count_total(&lines);
    let total_known = snap.bytes.len() <= TOTAL_COUNT_SIZE_LIMIT as usize;
    let reported_total = total_known.then_some(total);

    if total == 0 {
        return note_envelope(budget, "Warning: the file exists but is empty.", Some(0));
    }
    let requested = offset.unwrap_or(1).max(1);
    if requested > total {
        return note_envelope(
            budget,
            &format!(
                "Warning: the file has only {total} lines, but offset={requested} was requested."
            ),
            reported_total,
        );
    }
    let limit = limit.map(|l| l.max(1));
    let end = limit
        .map_or(total, |l| requested.saturating_add(l).saturating_sub(1))
        .min(total);

    // Budget-first greedy fit: body + wire-tail reservation for the state that would
    // result. Both the body and the trailer are nondecreasing in the shown count, so
    // the fitting set is a prefix; a line that does not fit ends the page (popped from
    // the end by simply not being taken). Candidates are measured on a cloned trial
    // counter so the committed counter ends up holding exactly the accepted body —
    // the final verify then recounts the whole wire (body + note) against it.
    let mut shown: Vec<(u64, String)> = Vec::new();
    let mut counter = ExactPrefixCounter::new();
    for (i, raw) in lines[(requested - 1) as usize..end as usize]
        .iter()
        .enumerate()
    {
        let line_num = requested + i as u64;
        let content = truncate_line(raw);
        let mut trial = counter.clone();
        if !shown.is_empty() {
            trial.push_str("\n");
        }
        trial.push_str(&format!("{line_num}\t{content}"));
        let body = trial.checkpoint();
        let floor = line_floor(requested, line_num, reported_total);
        if body + floor <= budget {
            counter = trial;
            shown.push((line_num, content));
        } else {
            break;
        }
    }
    // Even one line plus its continuation note must fit, else the budget is too small.
    if shown.is_empty() {
        return Err(budget::budget_too_small_error(
            ToolKind::Read,
            budget,
            what::CONTINUATION_NOTE,
        ));
    }
    // Partial only when the BUDGET cut the window short: a page that exhausted its
    // explicit `limit` — even mid-file — is complete, matching the batch semantics
    // (`entry_incomplete` judges by the requested window end).
    let window_len = end.saturating_sub(requested) + 1;
    let has_more = (shown.len() as u64) < window_len;

    let text_body = shown
        .iter()
        .map(|(n, c)| format!("{n}\t{c}"))
        .collect::<Vec<_>>()
        .join("\n");
    let last = shown.last().map(|(n, _)| *n).unwrap_or(requested);

    let note = page_note(requested, last, reported_total, has_more, last == total);
    let next_call =
        has_more.then(|| serde_json::json!({ "file_path": file_path, "offset": last + 1 }));

    // The wire is the whole page: body lines plus the status note after a blank line.
    // Pushing the trailer through the committed counter makes the incremental count
    // boundary-exact; the full recount must agree (CountMismatch) and fit (OverBudget).
    let text = format!("{text_body}\n\n{note}");
    counter.push_str(&format!("\n\n{note}"));
    let returned = budget::finish_wire(&text, Some(counter.finish()), budget)?;

    Ok(Envelope::page(Page {
        returned,
        budget,
        truncated: has_more,
        next_call,
        text: Some(text),
        terminal: Some(Terminal {
            state: if has_more { "partial" } else { "complete" }.to_string(),
            unit: "lines".to_string(),
            shown_from: requested,
            shown_to: last,
            total: reported_total,
            note: Some(note),
        }),
        skip_report: None,
    }))
}

/// The wire-tail reservation for a page showing through line `to`: the worst-case
/// status-note render for that state (`budget::tail_skeleton`, blank-line separator
/// included) — the exact trailer is recounted at assembly via `budget::finish_wire`.
fn line_floor(from: u64, to: u64, total: Option<u64>) -> u64 {
    budget::reserve_wire_tail(WireTail {
        unit: "lines",
        from,
        to,
        total,
        skip_tally_line: None,
    })
}

/// The exact status note for a page showing `from..=last` (frozen terminal grammar).
/// `has_more` means the budget cut the page short (a continuation exists); a page that
/// merely exhausted an explicit `limit` is complete, and `at_eof` distinguishes the two
/// complete wordings.
fn page_note(from: u64, last: u64, total: Option<u64>, has_more: bool, at_eof: bool) -> String {
    match (has_more, total, at_eof) {
        (true, Some(t), _) => format!(
            "(Partial: lines {from}-{last} of {t} shown. Continue with offset={}.)",
            last + 1
        ),
        (true, None, _) => format!(
            "(Partial: lines {from}-{last} shown. Continue with offset={}.)",
            last + 1
        ),
        (false, Some(t), true) => {
            format!("(Complete: reached end of file; lines {from}-{last} of {t} shown.)")
        }
        (false, Some(t), false) => {
            format!("(Complete: requested window shown; lines {from}-{last} of {t} shown.)")
        }
        (false, None, true) => {
            format!("(Complete: reached end of file; lines {from}-{last} shown.)")
        }
        (false, None, false) => {
            format!("(Complete: requested window shown; lines {from}-{last} shown.)")
        }
    }
}

/// Warning-only page (empty file / offset past EOF): no results, no continuation. The
/// wire is the message itself (it is both the page text and the status note), so the
/// exact message cost is the whole accounting — no tail to reserve.
fn note_envelope(budget: u64, message: &str, total: Option<u64>) -> Result<Envelope> {
    let returned = count_tokens(message);
    if returned > budget {
        return Err(budget::budget_too_small_error(
            ToolKind::Read,
            budget,
            what::CONTINUATION_NOTE,
        ));
    }
    Ok(Envelope::page(Page {
        returned,
        budget,
        truncated: false,
        next_call: None,
        text: Some(message.to_string()),
        terminal: Some(Terminal {
            state: "complete".to_string(),
            unit: "lines".to_string(),
            shown_from: 0,
            shown_to: 0,
            total,
            note: Some(message.to_string()),
        }),
        skip_report: None,
    }))
}

// ---------------------------------------------------------------------------
// batch read
// ---------------------------------------------------------------------------

/// One collected batch entry, ready for rendering and fitting. Per-entry problems are
/// carried inline (`error`/`warning`) and never abort the neighbors.
#[derive(Clone)]
struct Segment {
    /// The path exactly as requested (used in the header and results).
    path: String,
    /// Hard per-entry problem (missing, directory, binary, undecodable).
    error: Option<String>,
    /// Advisory per-entry outcome (empty file, offset past EOF).
    warning: Option<String>,
    /// Shown lines `(line number, content)`; line numbers are 1-based absolute.
    lines: Vec<(u64, String)>,
    /// Requested window start (the entry's offset, minimum 1).
    first_line: u64,
    /// Last SHOWN line (`first_line - 1` when nothing fit).
    last_line: u64,
    /// File line count (known internally even when `total_known` is false).
    total_lines: u64,
    /// Whether `total_lines` may be reported (files > 64 MiB report an unknown total).
    total_known: bool,
    /// Requested window end (`offset + limit - 1`), `None` when the limit was omitted.
    window_end: Option<u64>,
    /// Advisory transcoding notes rendered under the header.
    notes: Vec<String>,
    /// The entry's encoding label, echoed in the continuation.
    encoding: Option<String>,
    /// The original (limit-backfilled) request, used verbatim when the entry never fit.
    entry: BatchEntry,
}

/// One serialized continuation entry; inapplicable keys are omitted.
#[derive(Serialize)]
struct ContinuationEntry {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding: Option<String>,
}

/// Batch read: backfill the top-level limit → reject duplicates → collect every entry
/// independently (each capped by the FULL budget) → fit the largest prefix of whole
/// segments under the one shared budget → exact continuation JSON.
fn read_batch(
    root: &Path,
    files: &[BatchEntry],
    top_level_limit: Option<u64>,
    budget: u64,
) -> Result<Envelope> {
    // Top-level limit is backfilled as the default into every entry that omits one; an
    // entry's own limit wins. The duplicate key is computed AFTER the backfill.
    let entries: Vec<BatchEntry> = files
        .iter()
        .map(|entry| BatchEntry {
            path: entry.path.clone(),
            offset: entry.offset,
            limit: entry.limit.or(top_level_limit),
            encoding: entry.encoding.clone(),
        })
        .collect();

    let mut seen: BTreeMap<(String, u64, Option<u64>, Option<String>), usize> = BTreeMap::new();
    for (i, entry) in entries.iter().enumerate() {
        // Canonical path when resolvable; an unresolvable path is still deterministic,
        // so identical raw paths stay detectable as duplicates. Limitation: this is
        // lexical normalization only — no symlink resolution, no case folding — so on a
        // case-insensitive filesystem two entries differing only in case are not
        // detected as duplicates even though they request the same interval.
        let canonical = match index::resolve_in_root(root, &entry.path) {
            Ok(resolved) => resolved.to_string_lossy().to_string(),
            Err(_) => entry.path.clone(),
        };
        let key = (
            canonical,
            entry.offset.unwrap_or(1),
            entry.limit,
            entry.encoding.clone(),
        );
        if let Some(&prev) = seen.get(&key) {
            return Err(Error::Config(format!(
                "files contains a duplicate entry: two entries ask for the same text \
                 interval of {} (offset, limit, and encoding all included).",
                entries[prev].path
            )));
        }
        seen.insert(key, i);
    }

    let segments: Vec<Segment> = entries
        .iter()
        .map(|entry| collect_segment(root, entry, budget))
        .collect();
    let total_entries = segments.len();

    // Pre-render each segment once (segments are immutable across probes); the probe
    // only joins a prefix of these pre-rendered lines plus the status note.
    let rendered_segments: Vec<String> = segments.iter().map(format_segment).collect();

    // Whole-slice probe first, then binary search (fitting order). The
    // wire is the whole page — rendered segments plus the status note after a blank
    // line — so the probe measures exactly that; there is no separate tail to reserve.
    let probe = |k: usize| -> Option<FittedBatch> {
        let complete = k == total_entries
            && segments
                .iter()
                .all(|seg| seg.error.is_some() || seg.warning.is_some() || !entry_incomplete(seg));
        let continuation = (!complete).then(|| continuation_entries(&segments, k));
        let note = match (&complete, &continuation) {
            (true, _) => format!("(Complete: {total_entries} entries processed.)"),
            (false, Some(remaining)) => format!(
                "(Partial: {} entries in progress, {total_entries} requested. Continue with files={}.)",
                k,
                serde_json::to_string(remaining)
                    .expect("continuation entries serialize infallibly")
            ),
            (false, None) => unreachable!("partial batch always has remaining entries"),
        };
        let mut text = rendered_segments[..k].join("\n\n");
        // Only add the blank-line separator when there are body lines; k==0 produces no
        // leading blank line (the note is the only content).
        if k > 0 {
            text.push_str("\n\n");
        }
        text.push_str(&note);
        let returned = count_tokens(&text);
        (returned <= budget).then_some(FittedBatch {
            k,
            returned,
            text,
            note,
            complete,
            continuation,
        })
    };
    let Some(fitted) = fit_largest(total_entries, probe) else {
        return Err(budget::budget_too_small_error(
            ToolKind::Read,
            budget,
            what::BATCH_CONTINUATION_NOTE,
        ));
    };

    let next_call = fitted
        .continuation
        .as_ref()
        .map(|entries| serde_json::json!({ "files": entries }));

    // The probe's count was a full recount of the same text; the hard check keeps the
    // budget = wire invariant contract-level.
    let returned = budget::finish_wire(&fitted.text, Some(fitted.returned), budget)?;

    Ok(Envelope::page(Page {
        returned,
        budget,
        truncated: !fitted.complete,
        next_call,
        text: Some(fitted.text),
        terminal: Some(Terminal {
            state: if fitted.complete {
                "complete"
            } else {
                "partial"
            }
            .to_string(),
            unit: "entries".to_string(),
            shown_from: if fitted.k > 0 { 1 } else { 0 },
            shown_to: fitted.k as u64,
            total: Some(total_entries as u64),
            note: Some(fitted.note),
        }),
        skip_report: None,
    }))
}

/// A probe result: the first `k` segments fit the shared budget together with the
/// status note (the wire = the whole rendered page).
struct FittedBatch {
    k: usize,
    returned: u64,
    text: String,
    note: String,
    complete: bool,
    continuation: Option<Vec<ContinuationEntry>>,
}

/// Collects one batch entry into a segment: sealed snapshot, binary/encoding ladder,
/// window collection capped by the full budget. All failures become inline problems.
fn collect_segment(root: &Path, entry: &BatchEntry, budget: u64) -> Segment {
    let blank = Segment {
        path: entry.path.clone(),
        error: None,
        warning: None,
        lines: Vec::new(),
        first_line: 0,
        last_line: 0,
        total_lines: 0,
        total_known: false,
        window_end: None,
        notes: Vec::new(),
        encoding: entry.encoding.clone(),
        entry: entry.clone(),
    };
    let fail = |message: String| Segment {
        error: Some(message),
        ..blank.clone()
    };
    let resolved = match index::resolve_in_root(root, &entry.path) {
        Ok(resolved) => resolved,
        Err(error) => return fail(error.to_string()),
    };
    if resolved.is_dir() {
        return fail(format!("Cannot read directory as text: {}.", entry.path));
    }
    let snap = match snapshot::Snapshot::open(&resolved, ToolKind::Read) {
        Ok(snap) => snap,
        Err(error) => return fail(format!("Cannot read {}: {error}", entry.path)),
    };
    if has_binary_magic(&snap.bytes) {
        return fail(binary_error(&entry.path, "binary").to_string());
    }
    let (text, notes) = match decode_text(&snap.bytes, entry.encoding.as_deref(), &entry.path) {
        Ok(decoded) => decoded,
        Err(message) => return fail(message),
    };
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let total = count_total(&lines);
    let total_known = snap.bytes.len() <= TOTAL_COUNT_SIZE_LIMIT as usize;
    if total == 0 {
        return Segment {
            warning: Some("Warning: the file exists but is empty.".to_string()),
            ..blank
        };
    }
    let offset = entry.offset.unwrap_or(1).max(1);
    if offset > total {
        return Segment {
            warning: Some(format!(
                "Warning: the file has only {total} lines, but offset={offset} was requested."
            )),
            total_lines: total,
            total_known,
            ..blank
        };
    }
    let limit = entry.limit.map(|l| l.max(1));
    let window_end = limit.map(|l| offset.saturating_add(l).saturating_sub(1));
    let end = window_end.map_or(total, |w| w.min(total));

    // Each entry is collected with the full budget as its ceiling: a single entry can
    // never exceed the whole response budget on its own.
    let mut shown: Vec<(u64, String)> = Vec::new();
    let mut counter = ExactPrefixCounter::new();
    for (i, raw) in lines[(offset - 1) as usize..end as usize]
        .iter()
        .enumerate()
    {
        let line_num = offset + i as u64;
        let content = truncate_line(raw);
        if !shown.is_empty() {
            counter.push_str("\n");
        }
        counter.push_str(&format!("{line_num}\t{content}"));
        if counter.checkpoint() > budget {
            break;
        }
        shown.push((line_num, content));
    }
    let last_line = shown.last().map(|(n, _)| *n).unwrap_or(offset - 1);
    Segment {
        lines: shown,
        first_line: offset,
        last_line,
        total_lines: total,
        total_known,
        window_end,
        notes,
        ..blank
    }
}

/// Renders one segment: inline problem, warning, or header + notes + `N\tcontent` lines.
fn format_segment(seg: &Segment) -> String {
    if let Some(error) = &seg.error {
        return format!("=== {} ===\n{}", seg.path, error);
    }
    if let Some(warning) = &seg.warning {
        return format!("=== {} ===\n{}", seg.path, warning);
    }
    if seg.lines.is_empty() {
        return format!("=== {} ===", seg.path);
    }
    let header = if seg.total_known {
        format!(
            "=== {} (lines {}-{} of {}) ===",
            seg.path, seg.first_line, seg.last_line, seg.total_lines
        )
    } else {
        format!(
            "=== {} (lines {}-{}) ===",
            seg.path, seg.first_line, seg.last_line
        )
    };
    let mut out = header;
    for note in &seg.notes {
        out.push('\n');
        out.push_str(note);
    }
    for (n, c) in &seg.lines {
        out.push('\n');
        out.push_str(&format!("{n}\t{c}"));
    }
    out
}

/// Whether a successfully-read segment still has requested window left to show.
fn entry_incomplete(seg: &Segment) -> bool {
    let end = seg
        .window_end
        .map_or(seg.total_lines, |w| w.min(seg.total_lines));
    seg.last_line < end
}

/// The exact continuation entries: fitted entries resume at
/// `last shown + 1` with the remainder of their requested window (the `limit` is
/// dropped when the window ran past EOF); unfitted entries are echoed verbatim with
/// their requested offset/limit/encoding — the echo carries the entry's own `limit`
/// (the top-level backfill was already applied at entry construction). Fitted
/// error/warning entries are already reported and drop out.
fn continuation_entries(segments: &[Segment], fitted: usize) -> Vec<ContinuationEntry> {
    let mut out = Vec::new();
    for seg in &segments[..fitted] {
        if seg.error.is_some() || seg.warning.is_some() || !entry_incomplete(seg) {
            continue;
        }
        let limit = seg
            .window_end
            .filter(|w| *w < seg.total_lines)
            .map(|w| w - seg.last_line);
        out.push(ContinuationEntry {
            path: seg.path.clone(),
            offset: Some(seg.last_line + 1),
            limit,
            encoding: seg.encoding.clone(),
        });
    }
    for seg in &segments[fitted..] {
        out.push(ContinuationEntry {
            offset: seg.entry.offset,
            limit: seg.entry.limit,
            path: seg.entry.path.clone(),
            encoding: seg.entry.encoding.clone(),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Real line count of a `\n`-split buffer: `split` yields one extra empty trailing
/// element for a newline-terminated file, which is not a line.
fn count_total(lines: &[&str]) -> u64 {
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.len() as u64 - 1
    } else {
        lines.len() as u64
    }
}

/// Lines longer than [`MAX_LINE_CHARS`] chars keep the first 2000 chars plus the frozen
/// marker; counting is per char (multibyte content must not panic the slice).
fn truncate_line(line: &str) -> String {
    let chars = line.chars().count();
    if chars > MAX_LINE_CHARS {
        let head: String = line.chars().take(MAX_LINE_CHARS).collect();
        format!("{head}... [line truncated: {chars} chars total]")
    } else {
        line.to_string()
    }
}

/// Frozen binary-as-text error.
fn binary_error(path: &str, kind: &str) -> Error {
    Error::Encoding(format!(
        "Cannot read binary file as text: {path} (looks like {kind})."
    ))
}

/// Decode sealed bytes: an explicit label goes through `decode_explicit`, otherwise the
/// automatic ladder (`decide`) decides. `Err` carries the frozen user-facing message so
/// both the hard single-file error and the inline batch segment can use it.
fn decode_text<'a>(
    bytes: &'a [u8],
    encoding: Option<&str>,
    path: &str,
) -> std::result::Result<(std::borrow::Cow<'a, str>, Vec<String>), String> {
    let decision = match encoding {
        Some(label) => decode_explicit(bytes, label).map_err(|rejection| rejection.skip_reason()),
        None => Ok(crate::encoding::decide(bytes)),
    };
    match decision {
        Ok(Decision::Text { decoded, notes, .. }) => Ok((decoded, notes)),
        Ok(Decision::Binary { kind }) => Err(binary_error(path, kind).to_string()),
        Ok(Decision::Rejected {
            report: Rejection::Ambiguous { clean_decodes },
        }) => Err(ambiguous_message(path, &clean_decodes)),
        Ok(Decision::Rejected { report }) => Err(report.skip_reason()),
        Err(message) => Err(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_test_file(content: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), content).unwrap();
        tmp
    }

    fn setup_test_file_owned(content: String) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), content).unwrap();
        tmp
    }

    #[test]
    fn read_line_format_and_offsets() {
        let tmp = setup_test_file("line1\nline2\nline3\n");
        let root = tmp.path();

        let env = read_single(root, "test.txt", Some(2), Some(2), None, 10000).unwrap();
        assert!(env.text.as_ref().unwrap().contains("2\tline2"));
        assert!(env.text.as_ref().unwrap().contains("3\tline3"));
        assert!(
            !env.text.as_ref().unwrap().contains("1\tline1"),
            "offset 2 page must not show line 1"
        );
    }

    #[test]
    fn limit_omitted_budget_decides() {
        let tmp = setup_test_file("line1\nline2\nline3\n");
        let root = tmp.path();

        // limit omitted: unbounded request — a generous budget shows the whole file
        // and the terminal is complete with no continuation.
        let env = read_single(root, "test.txt", Some(1), None, None, 500).unwrap();
        assert!(env.text.as_ref().unwrap().contains("3\tline3"));
        assert_eq!(env.terminal.as_ref().unwrap().state, "complete");
        assert!(env.next_call.is_none());

        // A tight (but feasible) budget cuts the page: the budget, not a limit, decided.
        let env = read_single(root, "test.txt", Some(1), None, None, 120).unwrap();
        assert!(env.text.as_ref().unwrap().contains("1\tline1"));
        assert!(env.token_usage.returned <= 120);
    }

    #[test]
    fn long_line_truncation_marker() {
        let long_line = "a".repeat(3000);
        let tmp = setup_test_file(&format!("{}\nshort\n", long_line));
        let root = tmp.path();

        let env = read_single(root, "test.txt", Some(1), Some(1), None, 10000).unwrap();
        let text = env.text.as_ref().unwrap();
        assert!(text.contains("[line truncated: 3000 chars total]"));
        assert!(text.len() < 3000, "the 3000-char line must be cut: {text}");
    }

    #[test]
    fn empty_file_warning() {
        let tmp = setup_test_file("");
        let root = tmp.path();

        let env = read_single(root, "test.txt", None, None, None, 10000).unwrap();
        assert!(
            env.text
                .as_ref()
                .unwrap()
                .contains("Warning: the file exists but is empty.")
        );
    }

    #[test]
    fn offset_past_eof_warning() {
        let tmp = setup_test_file("line1\nline2\n");
        let root = tmp.path();

        let env = read_single(root, "test.txt", Some(10), None, None, 10000).unwrap();
        assert!(
            env.text
                .as_ref()
                .unwrap()
                .contains("Warning: the file has only 2 lines, but offset=10 was requested.")
        );
    }

    #[test]
    fn terminal_complete_vs_partial() {
        let tmp = setup_test_file("line1\nline2\nline3\nline4\nline5\n");
        let root = tmp.path();

        // Complete: window exactly reaching EOF.
        let env = read_single(root, "test.txt", Some(1), Some(5), None, 10000).unwrap();
        assert!(
            env.terminal
                .as_ref()
                .unwrap()
                .note
                .as_ref()
                .unwrap()
                .contains("Complete: reached end of file")
        );

        // Complete: an explicit limit satisfied mid-file — the window, not the budget,
        // decided the page (batch-aligned semantics), so no continuation is offered.
        let env = read_single(root, "test.txt", Some(1), Some(2), None, 10000).unwrap();
        let note = env.terminal.as_ref().unwrap().note.as_ref().unwrap();
        assert!(
            note.contains("(Complete: requested window shown; lines 1-2 of 5 shown.)"),
            "{note}"
        );
        assert_eq!(env.terminal.as_ref().unwrap().state, "complete");
        assert!(env.next_call.is_none());
        assert!(!env.truncated);

        // Partial: the BUDGET (not a limit) cut the page short. Line 2 is far beyond
        // any feasible budget even truncated, so only line 1 can fit.
        let long_line = "qZ".repeat(1000);
        let tmp = setup_test_file(&format!("line1\n{long_line}\nline3\nline4\nline5\n"));
        let env = read_single(tmp.path(), "test.txt", Some(1), Some(5), None, 200).unwrap();
        let note = env.terminal.as_ref().unwrap().note.as_ref().unwrap();
        assert!(
            note.contains("(Partial: lines 1-1 of 5 shown. Continue with offset=2.)"),
            "{note}"
        );
        assert_eq!(env.terminal.as_ref().unwrap().state, "partial");
        assert_eq!(
            env.next_call.unwrap(),
            serde_json::json!({ "file_path": "test.txt", "offset": 2 })
        );
    }

    #[test]
    fn over_budget_pops_from_end() {
        let content = (1..=100)
            .map(|i| format!("line {}\n", i))
            .collect::<String>();
        let tmp = setup_test_file(&content);
        let root = tmp.path();

        let env = read_single(root, "test.txt", Some(1), Some(100), None, 200).unwrap();
        // Should show fewer lines than requested due to budget, and popping from the
        // end means the SHOWN lines are the requested prefix (lines 1..=n).
        let body = env
            .text
            .as_ref()
            .unwrap()
            .split("\n\n")
            .next()
            .unwrap()
            .lines()
            .collect::<Vec<_>>();
        assert!(!body.is_empty());
        assert!(body.len() < 100);
        assert!(body[0].starts_with("1\t"), "prefix property: line 1 leads");
        let last_n: u32 = body
            .last()
            .unwrap()
            .split('\t')
            .next()
            .unwrap()
            .parse()
            .expect("numbered body line");
        assert_eq!(last_n as usize, body.len());
        assert!(env.token_usage.returned <= 200);
    }

    /// CRLF input renders without the `\r` (frozen behavior); the wire is
    /// the whole page — body plus the status note after a blank line.
    #[test]
    fn trailing_carriage_returns_are_dropped() {
        let tmp = setup_test_file("alpha\r\nbeta\r\n");
        let root = tmp.path();

        let env = read_single(root, "test.txt", None, None, None, 10000).unwrap();
        let text = env.text.as_ref().unwrap();
        assert_eq!(
            text,
            "1\talpha\n2\tbeta\n\n(Complete: reached end of file; lines 1-2 of 2 shown.)"
        );
    }

    #[test]
    fn batch_exactly_one_of() {
        let tmp = setup_test_file("line1\n");
        let root = tmp.path();

        // Both present
        let params = ReadParams {
            file_path: Some("test.txt".to_string()),
            files: Some(vec![]),
            offset: None,
            limit: None,
            encoding: None,
        };
        let err = read_with_budget(root, &params, 10000).unwrap_err();
        assert!(
            err.to_string()
                .contains("Provide exactly one of file_path or files.")
        );

        // Neither present
        let params = ReadParams {
            file_path: None,
            files: None,
            offset: None,
            limit: None,
            encoding: None,
        };
        let err = read_with_budget(root, &params, 10000).unwrap_err();
        assert!(
            err.to_string()
                .contains("Provide exactly one of file_path or files.")
        );
    }

    #[test]
    fn batch_bounds() {
        let tmp = setup_test_file("line1\n");
        let root = tmp.path();

        // Empty array
        let params = ReadParams {
            file_path: None,
            files: Some(vec![]),
            offset: None,
            limit: None,
            encoding: None,
        };
        let err = read_with_budget(root, &params, 10000).unwrap_err();
        assert!(err.to_string().contains("expected 1 to 32 entries, got 0"));

        // Too many entries
        let files = (0..33)
            .map(|i| BatchEntry {
                path: format!("test{}.txt", i),
                offset: None,
                limit: None,
                encoding: None,
            })
            .collect();
        let params = ReadParams {
            file_path: None,
            files: Some(files),
            offset: None,
            limit: None,
            encoding: None,
        };
        let err = read_with_budget(root, &params, 10000).unwrap_err();
        assert!(err.to_string().contains("expected 1 to 32 entries, got 33"));
    }

    #[test]
    fn batch_rejects_top_level_offset_encoding() {
        let tmp = setup_test_file("line1\n");
        let root = tmp.path();

        let files = vec![BatchEntry {
            path: "test.txt".to_string(),
            offset: None,
            limit: None,
            encoding: None,
        }];

        // Top-level offset
        let params = ReadParams {
            file_path: None,
            files: Some(files.clone()),
            offset: Some(1),
            limit: None,
            encoding: None,
        };
        let err = read_with_budget(root, &params, 10000).unwrap_err();
        assert_eq!(
            err.to_string(),
            "configuration error: offset is a top-level setting and cannot be combined with files; put it on each files entry instead."
        );

        // Top-level encoding
        let params = ReadParams {
            file_path: None,
            files: Some(files),
            offset: None,
            limit: None,
            encoding: Some("utf-8".to_string()),
        };
        let err = read_with_budget(root, &params, 10000).unwrap_err();
        assert!(
            err.to_string()
                .contains("encoding is a top-level setting and cannot be combined with files; put it on each files entry instead.")
        );
    }

    #[test]
    fn batch_top_level_limit_backfills_and_entry_limit_wins() {
        // Create two test files
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file1.txt"), "line1\nline2\nline3\n").unwrap();
        std::fs::write(tmp.path().join("file2.txt"), "a\nb\nc\n").unwrap();
        let root = tmp.path();

        let files = vec![
            BatchEntry {
                path: "file1.txt".to_string(),
                offset: None,
                limit: None, // Should be backfilled
                encoding: None,
            },
            BatchEntry {
                path: "file2.txt".to_string(),
                offset: None,
                limit: Some(1), // Entry limit wins
                encoding: None,
            },
        ];

        let params = ReadParams {
            file_path: None,
            files: Some(files),
            offset: None,
            limit: Some(2), // Backfill value
            encoding: None,
        };

        let env = read_with_budget(root, &params, 10000).unwrap();

        // file1 shows exactly 2 lines (backfilled limit), file2 exactly 1 (entry limit wins)
        let text = env.text.as_ref().unwrap();
        assert!(
            text.contains("=== file1.txt (lines 1-2 of 3) ==="),
            "{text}"
        );
        assert!(
            text.contains("=== file2.txt (lines 1-1 of 3) ==="),
            "{text}"
        );
    }

    #[test]
    fn duplicate_entry_detection() {
        let tmp = setup_test_file("line1\nline2\n");
        let root = tmp.path();

        let files = vec![
            BatchEntry {
                path: "test.txt".to_string(),
                offset: Some(1),
                limit: Some(2),
                encoding: None,
            },
            BatchEntry {
                path: "test.txt".to_string(),
                offset: Some(1),
                limit: Some(2),
                encoding: None,
            },
        ];

        let params = ReadParams {
            file_path: None,
            files: Some(files),
            offset: None,
            limit: None,
            encoding: None,
        };

        let err = read_with_budget(root, &params, 10000).unwrap_err();
        assert!(
            err.to_string().contains(
                "files contains a duplicate entry: two entries ask for the same text interval of test.txt (offset, limit, and encoding all included)."
            )
        );
    }

    #[test]
    fn duplicate_key_uses_backfilled_limit() {
        let tmp = setup_test_file("line1\nline2\nline3\n");
        let root = tmp.path();

        let files = vec![
            BatchEntry {
                path: "test.txt".to_string(),
                offset: Some(1),
                limit: None, // Will be backfilled to 2
                encoding: None,
            },
            BatchEntry {
                path: "test.txt".to_string(),
                offset: Some(1),
                limit: None, // Will be backfilled to 2
                encoding: None,
            },
        ];

        let params = ReadParams {
            file_path: None,
            files: Some(files),
            offset: None,
            limit: Some(2), // Backfill value
            encoding: None,
        };

        let err = read_with_budget(root, &params, 10000).unwrap_err();
        assert!(err.to_string().contains("duplicate entry"));
    }

    #[test]
    fn shared_budget_largest_fitting_prefix() {
        // Create multiple files
        let tmp = tempfile::tempdir().unwrap();
        for i in 1..=5 {
            let content = (1..=10)
                .map(|j| format!("file{} line{}\n", i, j))
                .collect::<String>();
            std::fs::write(tmp.path().join(format!("file{i}.txt")), content).unwrap();
        }
        let root = tmp.path();

        let files = (1..=5)
            .map(|i| BatchEntry {
                path: format!("file{i}.txt"),
                offset: None,
                limit: None,
                encoding: None,
            })
            .collect();

        let params = ReadParams {
            file_path: None,
            files: Some(files),
            offset: None,
            limit: None,
            encoding: None,
        };

        // A budget that fits two whole entries but not three forces a partial page:
        // the shared budget is fitted as the largest prefix of whole segments.
        let env = read_with_budget(root, &params, 220).unwrap();

        // Should show some entries but not all due to budget
        assert!(env.text.is_some());
        assert_eq!(env.terminal.as_ref().unwrap().state, "partial");
        assert!(
            env.terminal
                .as_ref()
                .unwrap()
                .note
                .as_ref()
                .unwrap()
                .starts_with("(Partial: ")
        );
        assert!(env.next_call.is_some());
    }

    #[test]
    fn inline_segment_does_not_abort_neighbors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("good.txt"), "line1\nline2\n").unwrap();
        // Create a binary file
        std::fs::write(tmp.path().join("bad.bin"), b"\x00\x01\x02\x03").unwrap();
        std::fs::write(tmp.path().join("good2.txt"), "line3\nline4\n").unwrap();
        let root = tmp.path();

        let files = vec![
            BatchEntry {
                path: "good.txt".to_string(),
                offset: None,
                limit: None,
                encoding: None,
            },
            BatchEntry {
                path: "bad.bin".to_string(),
                offset: None,
                limit: None,
                encoding: None,
            },
            BatchEntry {
                path: "good2.txt".to_string(),
                offset: None,
                limit: None,
                encoding: None,
            },
        ];

        let params = ReadParams {
            file_path: None,
            files: Some(files),
            offset: None,
            limit: None,
            encoding: None,
        };

        let env = read_with_budget(root, &params, 10000).unwrap();

        // Should show error for bad.bin but still process other files
        assert!(
            env.text
                .as_ref()
                .unwrap()
                .contains("Cannot read binary file as text: bad.bin (looks like binary).")
        );
        assert!(env.text.as_ref().unwrap().contains("good.txt"));
        assert!(env.text.as_ref().unwrap().contains("good2.txt"));
        assert_eq!(env.terminal.as_ref().unwrap().state, "complete");
        assert!(
            env.terminal
                .as_ref()
                .unwrap()
                .note
                .as_ref()
                .unwrap()
                .contains("(Complete: 3 entries processed.)")
        );
    }

    #[test]
    fn batch_continuation_json_exactness() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 1..=5 {
            let content = (1..=100)
                .map(|j| format!("file{} line{}\n", i, j))
                .collect::<String>();
            std::fs::write(tmp.path().join(format!("file{i}.txt")), content).unwrap();
        }
        let root = tmp.path();

        let files = (1..=5)
            .map(|i| BatchEntry {
                path: format!("file{i}.txt"),
                offset: None,
                limit: Some(10),
                encoding: None,
            })
            .collect();

        let params = ReadParams {
            file_path: None,
            files: Some(files),
            offset: None,
            limit: None,
            encoding: None,
        };

        // Use a small budget to force continuation: two whole entries fit, the third
        // does not (each entry ≈ 74 tokens, the partial note ≈ 45).
        let env = read_with_budget(root, &params, 220).unwrap();
        let next_call = env.next_call.expect("partial batch must carry next_call");
        let remaining = next_call
            .get("files")
            .and_then(|f| f.as_array())
            .expect("files array");

        // Every window (limit 10) either completed or never started, so continuation
        // entries echo the request: no resume offset, no encoding, limit 10.
        assert!(!remaining.is_empty());
        for entry in remaining {
            assert!(entry.get("path").is_some());
            assert!(entry.get("offset").is_none(), "no resume offset: {entry}");
            assert!(entry.get("encoding").is_none(), "encoding omitted: {entry}");
            assert_eq!(entry.get("limit").and_then(|l| l.as_u64()), Some(10));
        }
        assert_eq!(
            remaining.len(),
            3,
            "two entries shown, three resume: {remaining:?}"
        );
        // The terminal note embeds the same compact JSON array — byte-exact against the
        // frozen grammar, including the closing paren. Written out literally so the
        // assertion cannot drift with the JSON map's feature-dependent key order.
        let note = env.terminal.as_ref().unwrap().note.as_ref().unwrap();
        let expected = "(Partial: 2 entries in progress, 5 requested. Continue with files=\
[{\"path\":\"file3.txt\",\"limit\":10},\
{\"path\":\"file4.txt\",\"limit\":10},\
{\"path\":\"file5.txt\",\"limit\":10}].)";
        assert_eq!(note, expected, "frozen batch Partial note, byte-exact");
    }

    /// A fitted entry that is budget-cut mid-window resumes at `last shown + 1` with
    /// `limit` = the remainder of its requested window. The fixture's
    /// first three lines are short and line 4 is enormous, so the collect ceiling cuts
    /// the window after exactly those three lines while the entry still fits the page.
    #[test]
    fn batch_continuation_mid_file_resume_offset_and_remainder_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut content = String::new();
        content.push_str("alpha one\nbeta two\ngamma three\n");
        // Far beyond any plausible token budget even after the 2000-char render cap.
        content.push_str(&"qZ".repeat(1000));
        content.push('\n');
        for i in 4..100 {
            content.push_str(&format!("filler line {i}\n"));
        }
        std::fs::write(tmp.path().join("mixed.txt"), content).unwrap();
        let root = tmp.path();

        let files = vec![BatchEntry {
            path: "mixed.txt".to_string(),
            offset: Some(1),
            limit: Some(50), // window 1-50 of 100 lines: the remainder rule applies
            encoding: None,
        }];
        let params = ReadParams {
            file_path: None,
            files: Some(files),
            offset: None,
            limit: None,
            encoding: None,
        };

        let env = read_with_budget(root, &params, 260).unwrap();
        assert_eq!(env.terminal.as_ref().unwrap().state, "partial");
        // Exactly the three short lines were shown before the giant line broke the page.
        let text = env.text.as_ref().unwrap();
        assert!(text.contains("1\talpha one"), "{text}");
        assert!(text.contains("3\tgamma three"), "{text}");
        assert!(
            !text.contains("4\t"),
            "the giant line must not be shown: {text}"
        );

        let next = env.next_call.expect("partial batch carries next_call");
        let remaining = next
            .get("files")
            .and_then(|f| f.as_array())
            .expect("files array");
        assert_eq!(
            remaining.len(),
            1,
            "one in-progress entry resumes: {remaining:?}"
        );
        // offset = last shown + 1; limit = remainder of the requested window (50 - 3).
        assert_eq!(
            remaining[0],
            serde_json::json!({"path": "mixed.txt", "offset": 4, "limit": 47}),
            "exact resume entry (encoding key omitted)"
        );
    }

    /// The remainder rule drops `limit` entirely when the requested window ran past EOF: the
    /// resumed call reads to the end of file.
    #[test]
    fn batch_continuation_limit_dropped_when_window_ran_past_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let mut content = String::new();
        content.push_str("alpha one\nbeta two\ngamma three\n");
        content.push_str(&"qZ".repeat(1000));
        content.push('\n');
        for i in 4..100 {
            content.push_str(&format!("filler line {i}\n"));
        }
        std::fs::write(tmp.path().join("mixed.txt"), content).unwrap();
        let root = tmp.path();

        let files = vec![BatchEntry {
            path: "mixed.txt".to_string(),
            offset: Some(1),
            limit: Some(1000), // window 1-1000 runs past the 100-line file
            encoding: None,
        }];
        let params = ReadParams {
            file_path: None,
            files: Some(files),
            offset: None,
            limit: None,
            encoding: None,
        };

        let env = read_with_budget(root, &params, 260).unwrap();
        assert_eq!(env.terminal.as_ref().unwrap().state, "partial");
        let next = env.next_call.expect("partial batch carries next_call");
        let remaining = next
            .get("files")
            .and_then(|f| f.as_array())
            .expect("files array");
        assert_eq!(remaining.len(), 1, "{remaining:?}");
        // offset = last shown + 1; the limit key is OMITTED (window ran past EOF), as
        // is the encoding key.
        assert_eq!(
            remaining[0],
            serde_json::json!({"path": "mixed.txt", "offset": 4}),
            "limit dropped when the window ran past EOF"
        );
    }

    /// A budget-cut page carries the exact next call (same path, offset = last shown
    /// + 1); a page that satisfied its explicit `limit` is complete and carries none.
    #[test]
    fn continuation_limit_remainder_rule() {
        let content = (1..=100)
            .map(|i| format!("line{}\n", i))
            .collect::<String>();
        let tmp = setup_test_file_owned(content);
        let root = tmp.path();

        // An explicit window fully satisfied by the limit (not the budget) is complete.
        let env = read_single(root, "test.txt", Some(1), Some(10), None, 10000).unwrap();
        assert_eq!(env.terminal.as_ref().unwrap().state, "complete");
        assert!(
            env.next_call.is_none(),
            "no continuation for a satisfied window"
        );

        // A tight budget cuts the same window: the page is partial with a continuation.
        let env = read_single(root, "test.txt", Some(1), Some(10), None, 60).unwrap();
        assert_eq!(env.terminal.as_ref().unwrap().state, "partial");
        let next_call = env.next_call.expect("partial read must carry next_call");
        // Continuation is exactly the next call: same path, offset = last shown + 1.
        assert_eq!(next_call["file_path"], "test.txt");
        let shown = env
            .text
            .as_ref()
            .unwrap()
            .split("\n\n")
            .next()
            .unwrap()
            .lines()
            .count() as u64;
        assert_eq!(next_call["offset"], shown + 1);
        assert!(shown < 10, "the budget cut the window");
    }

    #[test]
    fn binary_file_typed_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("binary.bin"), b"PK\x03\x04").unwrap();
        let root = tmp.path();

        let err = read_single(root, "binary.bin", None, None, None, 10000).unwrap_err();
        assert!(err.to_string().contains("Cannot read binary file as text"));
        assert!(err.to_string().contains("binary"));
    }
}
