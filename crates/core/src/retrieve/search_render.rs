// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure rendering for search output: content pages grouped by file, long-line windows,
//! `only_matching` escaping, terminal-note grammar, skip-tally clause, degradation ladder.
//! No budget knowledge — fitting is the caller's orchestration.

use std::collections::HashMap;
use std::ops::Range;

use serde::Serialize;

use super::SearchOutput;
use super::search_sink::{LineSpan, LineTable, RawEntry};

/// The serialized continuation payload: `{"tool":"<tool>","arguments":{...params with offset replaced...}}`.
/// Takes the tool name, serializable params, and the new offset value; clones the params,
/// overwrites the `offset` field in the JSON, and wraps in the tool envelope.
pub(crate) fn next_call_with_offset<P: Serialize>(
    tool: &'static str,
    params: &P,
    offset: u64,
) -> serde_json::Value {
    let mut args = serde_json::to_value(params).expect("params serialize infallibly");
    // Both `SearchParams` and `GlobParams` carry `offset: u64` as the first field in the
    // JSON object (confirmed by the wire contract tests). Set it directly.
    if let Some(obj) = args.as_object_mut() {
        obj.insert("offset".to_string(), serde_json::json!(offset));
    }
    serde_json::json!({
        "tool": tool,
        "arguments": args,
    })
}

/// Lines at or below this many BYTES print whole (`LONG_LINE_BYTES`).
pub(crate) const LONG_LINE_BYTES: usize = 500;
/// Window side chars around a match in a long line (`MATCH_WINDOW_SIDE_CHARS`).
pub(crate) const MATCH_WINDOW_SIDE_CHARS: usize = 100;
/// Upper bound of the match window (`MAX_MATCH_CHARS`).
pub(crate) const MAX_MATCH_CHARS: usize = 2_000;

/// What a real newline in the pattern is rewritten to.
const OPTIONAL_CRLF: &str = "\\r?\\n";

/// Multiline pattern preprocessing: a literal `\n` escape guarded by an odd
/// number of backslashes, or an actual newline in the pattern, becomes `\r?\n`; backslash
/// runs are preserved exactly.
pub(crate) fn normalize_multiline_pattern(pattern: &str) -> String {
    let mut normalized = String::with_capacity(pattern.len());
    // Both split bytes (`\` and newline) are single-byte UTF-8, so every slice below is
    // char-aligned and the plain segments round-trip through `from_utf8` infallibly.
    let mut rest = pattern.as_bytes();
    while let Some(split) = rest.iter().position(|byte| matches!(byte, b'\\' | b'\n')) {
        let (plain, tail) = rest.split_at(split);
        normalized.push_str(std::str::from_utf8(plain).expect("ASCII split stays valid UTF-8"));
        if tail[0] == b'\n' {
            normalized.push_str(OPTIONAL_CRLF);
            rest = &tail[1..];
            continue;
        }
        let run = tail.iter().take_while(|&&byte| byte == b'\\').count();
        // An odd run means the first backslash escapes the `n`: drop that escape backslash
        // and rewrite the guarded pair; an even run leaves the pair untouched.
        let guards_newline = run % 2 == 1 && tail.get(run) == Some(&b'n');
        for _ in 0..if guards_newline { run - 1 } else { run } {
            normalized.push('\\');
        }
        if guards_newline {
            normalized.push_str(OPTIONAL_CRLF);
            rest = &tail[run + 1..];
        } else {
            rest = &tail[run..];
        }
    }
    normalized.push_str(std::str::from_utf8(rest).expect("pattern stays valid UTF-8"));
    normalized
}

/// A singular/plural noun pair of the note grammar ("1 file" / "3 files").
#[derive(Debug, Clone, Copy)]
pub(crate) struct Noun {
    singular: &'static str,
    plural: &'static str,
}

impl Noun {
    /// The nouns the search note grammar distinguishes.
    pub(crate) const FILES: Self = Self {
        singular: "file",
        plural: "files",
    };
    pub(crate) const RESULTS: Self = Self {
        singular: "result",
        plural: "results",
    };
    pub(crate) const OCCURRENCES: Self = Self {
        singular: "occurrence",
        plural: "occurrences",
    };
    pub(crate) const PATHS: Self = Self {
        singular: "path",
        plural: "paths",
    };
}

/// `{count} {noun}`, singular exactly at 1 ("1 file", "0 files", "3 files").
pub(crate) fn quantify(count: u64, noun: Noun) -> String {
    let name = match count {
        1 => noun.singular,
        _ => noun.plural,
    };
    format!("{count} {name}")
}

/// "result 3" / "results 3-9" — the 1-based inclusive label of a shown run.
fn span_label(noun: Noun, first: u64, shown: u64) -> String {
    match shown {
        1 => format!("{} {first}", noun.singular),
        _ => format!("{} {first}-{}", noun.plural, first + shown - 1),
    }
}

/// Terminal note for paginated modes (files/content).
pub(crate) fn paged_terminal(
    noun: Noun,
    offset: u64,
    shown: u64,
    has_more: bool,
    total: u64,
) -> String {
    let label = span_label(noun, offset + 1, shown);
    if has_more {
        format!(
            "(Shown: {label}. More remain — resume from offset={}.)",
            offset + shown
        )
    } else if offset == 0 {
        format!("(Shown: {label}. All {} shown.)", quantify(total, noun))
    } else {
        format!("(Shown: {label}. Reached the end of results.)")
    }
}

/// Aggregate totals line shared by count completion and summary ("4 occurrences in
/// total across 1 file").
fn totals_note(occurrences: u64, files: u64) -> String {
    format!(
        "(Shown: {} in total across {}.)",
        quantify(occurrences, Noun::OCCURRENCES),
        quantify(files, Noun::FILES)
    )
}

/// Terminal note for count mode.
pub(crate) fn count_terminal(
    offset: u64,
    shown_files: u64,
    occurrences: u64,
    has_more: bool,
    total_files: u64,
) -> String {
    if has_more {
        format!(
            "(Shown: {}, subtotal {} this page. More remain — resume from offset={}.)",
            quantify(shown_files, Noun::FILES),
            quantify(occurrences, Noun::OCCURRENCES),
            offset + shown_files
        )
    } else if offset == 0 {
        totals_note(occurrences, total_files)
    } else {
        format!(
            "(Shown: {}, subtotal {} this page. Reached the end.)",
            span_label(Noun::FILES, offset + 1, shown_files),
            quantify(occurrences, Noun::OCCURRENCES)
        )
    }
}

/// Zero-result terminal note per mode.
pub(crate) fn zero_result_note(mode: SearchOutput) -> &'static str {
    match mode {
        SearchOutput::FilesWithMatches => "(Shown: nothing; no file matched.)",
        SearchOutput::Content | SearchOutput::Count | SearchOutput::Summary => {
            "(Shown: nothing; no match found.)"
        }
    }
}

/// Offset-past-end terminal note.
pub(crate) fn offset_exhausted_note(mode: SearchOutput, offset: u64, total: u64) -> String {
    let noun = match mode {
        SearchOutput::Content => Noun::RESULTS,
        SearchOutput::FilesWithMatches | SearchOutput::Count | SearchOutput::Summary => Noun::FILES,
    };
    format!(
        "(Shown: none at offset={offset}; the scan saw only {} in all.)",
        quantify(total, noun)
    )
}

/// Summary terminal note (totals only; summary ignores head_limit/offset).
pub(crate) fn summary_note(occurrences: u64, files: u64) -> String {
    totals_note(occurrences, files)
}

/// Coverage gaps a response must disclose in its own note: skipped
/// files were read but left out; unreachable paths were never entered, so any files
/// below them stay unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SkipTally {
    pub files: u64,
    pub unreachable: u64,
    /// Detail lines available across both kinds, before any cap.
    pub listed: usize,
}

impl SkipTally {
    /// The terminal-note clause text (no trailing punctuation). `shown` detail lines
    /// survived the budget; when that is below `listed`, the narrowing hint is appended.
    pub fn clause(&self, shown: usize) -> Option<String> {
        if self.files + self.unreachable == 0 {
            return None;
        }
        let mut parts: Vec<String> = Vec::new();
        if self.files > 0 {
            parts.push(format!("{} skipped", quantify(self.files, Noun::FILES)));
        }
        if self.unreachable > 0 {
            parts.push(format!(
                "{} unreachable",
                quantify(self.unreachable, Noun::PATHS)
            ));
        }
        if shown < self.listed {
            parts.push(format!(
                "listing {shown}; tighten path/glob to see the rest"
            ));
        }
        Some(parts.join(", "))
    }
}

/// Folds the skip clause into a note whose text closes with `.)`.
fn fold_skip_clause(shown: usize, tally: &SkipTally, terminal: &str) -> Option<String> {
    match tally.clause(shown) {
        None => Some(terminal.to_string()),
        Some(clause) => {
            let stem = terminal.strip_suffix(".)")?;
            Some(format!("{stem}; {clause}.)"))
        }
    }
}

/// The note tail of a response: transcoding notes first, then skip details, then the
/// fallback-encoding note, then the terminal (note ordering).
#[derive(Debug, Default)]
pub(crate) struct NoteUnits {
    pub fixed: Vec<String>,
    pub fallback: Option<String>,
    pub tally: SkipTally,
}

impl NoteUnits {
    /// The note lines rendered into the response text: transcoding notes, the
    /// fallback-encoding note, then the (skip-folded) terminal. Skip details are carried
    /// structurally in the envelope's `skip_report`, never in the text.
    pub fn text_notes(&self, shown: usize, terminal: &str) -> Vec<String> {
        let mut notes: Vec<String> = self.fixed.clone();
        notes.extend(self.fallback.clone());
        notes.push(self.terminal_note(shown, terminal));
        notes
    }

    /// The terminal note with the skip tally folded in at `shown` detail lines.
    pub fn terminal_note(&self, shown: usize, terminal: &str) -> String {
        fold_skip_clause(shown, &self.tally, terminal).unwrap_or_else(|| terminal.to_string())
    }
}

/// Joins body lines and notes into the rendered text (`lines.join("\n") + "\n\n" +
/// notes.join("\n")`, no trailing newline); `None` when there is no body (note-only
/// responses carry their note in the structured terminal instead).
pub(crate) fn assemble_text(body: &[String], notes: &[String]) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    // Body lines joined, then a blank line, then the notes: joining the body once and
    // appending the two groups keeps this to a single copy of the body bytes.
    let mut text = body.join("\n");
    if !notes.is_empty() {
        text.push_str("\n\n");
        text.push_str(&notes.join("\n"));
    }
    Some(text)
}

/// One searched file as the renderer sees it.
pub(crate) struct ContentFile<'a> {
    pub rel: &'a str,
    pub table: &'a LineTable<'a>,
}

/// Render options for one content page (kept together for the renderer's signature).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ContentRenderOptions {
    /// Degraded before/after context depth for this candidate page.
    pub before: usize,
    pub after: usize,
    /// Degraded match window (chars) for long lines.
    pub match_window: usize,
    pub line_numbers: bool,
    pub only_matching: bool,
    /// A single-file target renders no per-file headers (there is only one file).
    pub single_file_target: bool,
}

/// One file group's append-only planning state. Entries arrive in ascending start-line
/// order, so consolidating blocks and matched runs only ever extends the trailing
/// element — a later entry's context block cannot reach back past the previous block's
/// start (that reach is exactly what made the blocks disjoint), and a run extension
/// that would reclassify an emitted line is impossible for the same reason.
struct GroupPlan {
    file_index: usize,
    /// Consolidated context blocks; the last may still grow with the next entry.
    blocks: Vec<(u64, u64)>,
    /// Consolidated matched runs; the last may still grow with the next entry.
    matched: Vec<(u64, u64)>,
    /// Match mode: match spans per line, removed when the line is emitted.
    spans_by_line: HashMap<u64, Vec<LineSpan>>,
    /// Only-matching mode: occurrence ranges per start line, in entry order.
    starts_at: HashMap<u64, Vec<Range<usize>>>,
    /// Block currently being emitted (`blocks.len()` when all emitted so far are done).
    block_index: usize,
    /// Next line to emit within `blocks[block_index]` (`0` = not started yet).
    next_line: u64,
    /// Classification cursor into `matched` (emission is strictly in line order).
    cursor: usize,
    /// A separator is due before the next block's first emitted line.
    need_separator: bool,
}

/// Streaming content planner/renderer: entries are pushed in scan order and every
/// planned line is emitted as soon as its content is final — a line is final once an
/// entry arrives whose start line is beyond it, because no later entry can touch it
/// later entries start at or after that start line and only ever extend the trailing
/// block/run). Lines at or after the latest start line stay pending: a later entry may
/// still claim them (extend the block, cover them with a merged matched run, or start
/// on the same line), so they are rendered on demand per prefix via [`pending_lines`].
/// Pure: no budget, no IO.
pub(crate) struct ContentRenderer<'a> {
    files: &'a [ContentFile<'a>],
    options: ContentRenderOptions,
    /// Emitted (final) body lines, in page order.
    out: Vec<String>,
    group: Option<GroupPlan>,
    /// Groups started so far (drives the blank line between file groups).
    group_index: usize,
}

impl<'a> ContentRenderer<'a> {
    pub(crate) fn new(files: &'a [ContentFile<'a>], options: ContentRenderOptions) -> Self {
        Self {
            files,
            options,
            out: Vec::new(),
            group: None,
            group_index: 0,
        }
    }

    /// Pushes one selected entry in scan order and emits every line that becomes final.
    pub(crate) fn push(&mut self, file_index: usize, entry: &RawEntry) {
        if self
            .group
            .as_ref()
            .is_none_or(|group| group.file_index != file_index)
        {
            self.finish_group();
            if !self.options.single_file_target {
                if self.group_index > 0 {
                    self.out.push(String::new());
                }
                self.out.push(self.files[file_index].rel.to_string());
            }
            self.group_index += 1;
            self.group = Some(GroupPlan {
                file_index,
                blocks: Vec::new(),
                matched: Vec::new(),
                spans_by_line: HashMap::new(),
                starts_at: HashMap::new(),
                block_index: 0,
                next_line: 0,
                cursor: 0,
                need_separator: false,
            });
        }
        let before = self.options.before;
        let after = self.options.after;
        let only_matching = self.options.only_matching;
        let total_lines = self.files[file_index].table.total_lines();
        let group = self.group.as_mut().expect("group just started");
        // Consolidate the entry's context block at the tail (append-only: entries
        // arrive in ascending start-line order).
        let block = context_range(entry.start_line, entry.end_line, before, after, total_lines);
        match group.blocks.last_mut() {
            Some(last) if block.0 <= last.1.saturating_add(1) => {
                last.1 = last.1.max(block.1);
            }
            _ => {
                if !group.blocks.is_empty() {
                    group.need_separator = true;
                }
                group.blocks.push(block);
            }
        }
        // Consolidate the entry's matched run at the tail.
        match group.matched.last_mut() {
            Some(last) if entry.start_line <= last.1.saturating_add(1) => {
                last.1 = last.1.max(entry.end_line);
            }
            _ => group.matched.push((entry.start_line, entry.end_line)),
        }
        if only_matching {
            group
                .starts_at
                .entry(entry.start_line)
                .or_default()
                .push(entry.text_range.clone());
        } else {
            for span in &entry.spans {
                group
                    .spans_by_line
                    .entry(span.line)
                    .or_default()
                    .push(*span);
            }
        }
        // Every line before this entry's start line is now final: later entries start
        // at or after it, so none of them can extend a block or run over those lines.
        self.emit_through(entry.start_line.saturating_sub(1));
    }

    /// Flushes the final pending lines (whole page complete).
    pub(crate) fn finish(&mut self) {
        self.finish_group();
    }

    /// The emitted (final) body lines.
    pub(crate) fn lines(&self) -> &[String] {
        &self.out
    }

    /// The planned-but-pending lines rendered against the CURRENT group state: this is
    /// exactly the tail of the page for the entries pushed so far. Read-only — pending
    /// lines may still be claimed by later entries, so nothing is consumed here.
    pub(crate) fn pending_lines(&self) -> Vec<String> {
        let Some(group) = &self.group else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut cursor = group.cursor;
        let mut block_index = group.block_index;
        let mut need_separator = group.need_separator;
        let mut next_line = group.next_line;
        while block_index < group.blocks.len() {
            let (start, end) = group.blocks[block_index];
            if next_line == 0 {
                if need_separator {
                    out.push("--".to_string());
                }
                next_line = start;
            }
            for line in next_line..=end {
                group.emit_line_readonly(&mut out, line, &mut cursor, self.files, &self.options);
            }
            next_line = 0;
            block_index += 1;
            need_separator = true;
        }
        out
    }

    fn finish_group(&mut self) {
        if self.group.is_some() {
            self.emit_through(u64::MAX);
            self.group = None;
        }
    }

    /// Emits every planned line up to `target` (inclusive), walking the consolidated
    /// blocks in order with the persistent classification cursor.
    fn emit_through(&mut self, target: u64) {
        loop {
            let Some(group) = self.group.as_mut() else {
                return;
            };
            if group.block_index >= group.blocks.len() {
                return;
            }
            let (start, end) = group.blocks[group.block_index];
            if group.next_line == 0 {
                if start > target {
                    return;
                }
                if group.need_separator {
                    self.out.push("--".to_string());
                    group.need_separator = false;
                }
                group.next_line = start;
            }
            let upto = end.min(target);
            for line in group.next_line..=upto {
                group.emit_line(&mut self.out, line, self.files, &self.options);
            }
            group.next_line = upto + 1;
            if group.next_line > end {
                group.block_index += 1;
                group.next_line = 0;
                if group.block_index < group.blocks.len() {
                    group.need_separator = true;
                }
            } else {
                return;
            }
        }
    }
}

impl GroupPlan {
    /// Emits one line, consuming it (match-mode spans are removed from the map).
    fn emit_line(
        &mut self,
        out: &mut Vec<String>,
        line: u64,
        files: &[ContentFile<'_>],
        options: &ContentRenderOptions,
    ) {
        if options.only_matching {
            if let Some(ranges) = self.starts_at.get(&line) {
                for range in ranges {
                    let text = files[self.file_index]
                        .table
                        .text()
                        .get(range.clone())
                        .unwrap_or_default();
                    out.push(format_only_match(
                        &match_prefix(line, options.line_numbers),
                        text,
                        options.match_window,
                    ));
                }
                return;
            }
        }
        while self.cursor < self.matched.len() && self.matched[self.cursor].1 < line {
            self.cursor += 1;
        }
        let on_match = self.cursor < self.matched.len() && self.matched[self.cursor].0 <= line;
        if !options.only_matching && on_match {
            let spans = self.spans_by_line.remove(&line).unwrap_or_default();
            let content = files[self.file_index].table.line(line).unwrap_or_default();
            let ranges: Vec<(usize, usize)> = spans
                .iter()
                .map(|span| (span.char_start, span.char_start + span.char_len))
                .collect();
            out.push(format_match_line(
                &match_prefix(line, options.line_numbers),
                content,
                &ranges,
                options.match_window,
            ));
        } else if !options.only_matching {
            let content = files[self.file_index].table.line(line).unwrap_or_default();
            out.push(format_context_line(
                &context_prefix(line, options.line_numbers),
                content,
            ));
        } else {
            // only_matching: a matched line that starts no occurrence prints nothing
            // the occurrence renders once, at its start line).
        }
    }

    /// Read-only twin of [`GroupPlan::emit_line`] for [`ContentRenderer::pending_lines`]:
    /// no spans removed, no cursor mutation on the real state.
    fn emit_line_readonly(
        &self,
        out: &mut Vec<String>,
        line: u64,
        cursor: &mut usize,
        files: &[ContentFile<'_>],
        options: &ContentRenderOptions,
    ) {
        if options.only_matching
            && let Some(ranges) = self.starts_at.get(&line)
        {
            for range in ranges {
                let text = files[self.file_index]
                    .table
                    .text()
                    .get(range.clone())
                    .unwrap_or_default();
                out.push(format_only_match(
                    &match_prefix(line, options.line_numbers),
                    text,
                    options.match_window,
                ));
            }
            return;
        }
        while *cursor < self.matched.len() && self.matched[*cursor].1 < line {
            *cursor += 1;
        }
        let on_match = *cursor < self.matched.len() && self.matched[*cursor].0 <= line;
        if !options.only_matching && on_match {
            let spans = self.spans_by_line.get(&line).cloned().unwrap_or_default();
            let content = files[self.file_index].table.line(line).unwrap_or_default();
            let ranges: Vec<(usize, usize)> = spans
                .iter()
                .map(|span| (span.char_start, span.char_start + span.char_len))
                .collect();
            out.push(format_match_line(
                &match_prefix(line, options.line_numbers),
                content,
                &ranges,
                options.match_window,
            ));
        } else if !options.only_matching {
            let content = files[self.file_index].table.line(line).unwrap_or_default();
            out.push(format_context_line(
                &context_prefix(line, options.line_numbers),
                content,
            ));
        }
    }
}

/// Renders the content page for `shown` selected entries at the given context depth and
/// match window. Pure: no budget, no IO.
pub(crate) fn render_content_lines(
    files: &[ContentFile<'_>],
    selected: &[(usize, RawEntry)],
    options: ContentRenderOptions,
) -> Vec<String> {
    let mut renderer = ContentRenderer::new(files, options);
    for (file_index, entry) in selected {
        renderer.push(*file_index, entry);
    }
    renderer.finish();
    renderer.lines().to_vec()
}

/// Expands one entry's line span into the context block it prints with, clamped to the
/// document: `before`/`after` lines around it, never past line 1 or the last line.
fn context_range(
    start_line: u64,
    end_line: u64,
    before: usize,
    after: usize,
    total_lines: u64,
) -> (u64, u64) {
    let lowered = start_line.saturating_sub(before as u64).max(1);
    let raised = total_lines.min(end_line.saturating_add(after as u64));
    (lowered, raised)
}

fn match_prefix(line: u64, line_numbers: bool) -> String {
    numbered_prefix(line, line_numbers, ":")
}

fn context_prefix(line: u64, line_numbers: bool) -> String {
    numbered_prefix(line, line_numbers, "-")
}

fn numbered_prefix(line: u64, enabled: bool, suffix: &str) -> String {
    if !enabled {
        return String::new();
    }
    format!("{line}{suffix}")
}

/// One matching line: whole when ≤ [`LONG_LINE_BYTES`], else a window cut around the
/// match(es) with `…`, the shortened-match marker, and the line-size suffix
/// ; spans are char (start, end) pairs in the line).
pub(crate) fn format_match_line(
    prefix: &str,
    line: &str,
    spans: &[(usize, usize)],
    match_window: usize,
) -> String {
    if line.len() <= LONG_LINE_BYTES {
        return format!("{prefix}{line}");
    }
    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<(usize, usize)> = spans
        .iter()
        .map(|&(start, end)| {
            let capped_start = start.min(chars.len());
            (capped_start, end.min(chars.len()).max(start))
        })
        .collect();
    spans.sort_unstable();
    if spans.is_empty() {
        spans.push((0, 0));
    }
    let (head_start, head_end) = spans[0];
    let tail_end = spans
        .iter()
        .fold(0usize, |widest, &(_, end)| widest.max(end));
    let window = match_window.max(1);
    let side = MATCH_WINDOW_SIDE_CHARS;
    // Preferred cut: the head match plus `side` chars of margin on either end.
    let preferred_start = head_start.saturating_sub(side);
    let preferred_end = tail_end.saturating_add(side).min(chars.len());
    let (window_start, window_end) = if preferred_end.saturating_sub(preferred_start) <= window {
        (preferred_start, preferred_end)
    } else {
        // The preferred cut is wider than the window budget: lead the head match with a
        // quarter of the budget and trail the window behind it; a cut pinned to the end
        // of the line slides back instead.
        let lead = side.min(window / 4);
        let mut begin = head_start.saturating_sub(lead);
        let mut stop = begin.saturating_add(window).min(chars.len());
        if stop == chars.len() {
            begin = stop.saturating_sub(window).min(head_start);
            stop = begin.saturating_add(window).min(chars.len());
        }
        (begin, stop)
    };
    let head_clipped = head_end > window_end || head_start < window_start;
    // Later spans sticking out of the window (the head span is `head_clipped` itself).
    let any_beyond = spans[1..]
        .iter()
        .any(|&(start, end)| start < window_start || end > window_end);
    let mut rendered = String::from(prefix);
    if window_start > 0 {
        rendered.push('…');
    }
    rendered.extend(chars[window_start..window_end].iter());
    if head_clipped {
        rendered.push_str(&format!(
            "... [match shortened: {} chars in full]",
            head_end.saturating_sub(head_start)
        ));
    }
    if window_end < chars.len() {
        rendered.push('…');
    }
    let beyond_note = if spans.len() > 1 && any_beyond {
        "; more matches lie beyond this window"
    } else {
        ""
    };
    rendered.push_str(&format!(
        " [line spans {} chars; window centered on match(es){beyond_note}]",
        chars.len(),
    ));
    rendered
}

/// One context line: whole when ≤ [`LONG_LINE_BYTES`], else the omission marker.
pub(crate) fn format_context_line(prefix: &str, line: &str) -> String {
    if line.len() <= LONG_LINE_BYTES {
        format!("{prefix}{line}")
    } else {
        format!(
            "{prefix}[long line skipped: {} chars]",
            line.chars().count()
        )
    }
}

/// One occurrence in `only_matching` mode: newlines escaped as literal `\n`/`\r`
/// two-char sequences (a `\r\n` pair collapsing into one escape when both halves fit),
/// truncated at the window with the frozen marker.
pub(crate) fn format_only_match(prefix: &str, matched_text: &str, match_window: usize) -> String {
    let window = match_window.max(1);
    // Only the window prefix is escaped, but the full char count drives the truncation
    // marker. Walk the chars lazily so a large match never materializes a whole Vec<char>.
    let total = matched_text.chars().count();
    let mut escaped = String::with_capacity(matched_text.len().min(window).saturating_add(8));
    let mut chars = matched_text.chars().peekable();
    let mut index = 0;
    while index < window {
        // The peeked char is copied so the match holds no borrow of `chars`, letting the
        // arms advance it as needed.
        match chars.peek().copied() {
            Some('\n') => {
                escaped.push_str("\\n");
                chars.next();
                index += 1;
            }
            Some('\r') => {
                // Consume the `\r`; swallow a following `\n` too when both fit the window.
                chars.next();
                if index + 2 <= window && chars.peek() == Some(&'\n') {
                    escaped.push_str("\\n");
                    chars.next();
                    index += 2;
                } else {
                    escaped.push_str("\\r");
                    index += 1;
                }
            }
            Some(other) => {
                escaped.push(other);
                chars.next();
                index += 1;
            }
            None => break,
        }
    }
    if total <= window {
        format!("{prefix}{escaped}")
    } else {
        format!("{prefix}{escaped}... [match shortened: {total} chars in full]")
    }
}

/// Largest-fitting inclusive binary probe over `low..=high`: a fitting `middle` searches
/// higher, a miss searches lower; the best fit wins (replays v0.1.1's order).
pub(crate) fn largest_fitting<T>(
    low: usize,
    high: usize,
    mut probe: impl FnMut(usize) -> Option<T>,
) -> Option<T> {
    let mut best = None;
    let mut span = (low, high);
    while span.0 <= span.1 {
        let middle = span.0 + (span.1 - span.0) / 2;
        match probe(middle) {
            Some(candidate) => {
                best = Some(candidate);
                span.0 = middle + 1;
            }
            None if middle == 0 => break,
            None => span.1 = middle - 1,
        }
    }
    best
}

/// The largest fitting prefix over `1..=maximum`; the maximum is probed first (fast
/// path) and the rest binary-searched via [`largest_fitting`] (fitting order).
pub(crate) fn fit_largest<T>(
    maximum: usize,
    mut probe: impl FnMut(usize) -> Option<T>,
) -> Option<T> {
    if maximum == 0 {
        return None;
    }
    if let Some(best) = probe(maximum) {
        return Some(best);
    }
    largest_fitting(1, maximum - 1, probe)
}

/// The content degradation ladder: full context at the full window →
/// binary-search the context depth at the full window → binary-search the match window
/// at zero context. `probe(context_depth, match_window)` renders + measures a candidate.
pub(crate) fn content_degradation_ladder<T>(
    max_context: usize,
    mut probe: impl FnMut(usize, usize) -> Option<T>,
) -> Option<T> {
    if let Some(full) = probe(max_context, MAX_MATCH_CHARS) {
        return Some(full);
    }
    if let Some(no_context) = probe(0, MAX_MATCH_CHARS) {
        if let Some(recovered) =
            largest_fitting(0, max_context, |depth| probe(depth, MAX_MATCH_CHARS))
        {
            return Some(recovered);
        }
        return Some(no_context);
    }
    largest_fitting(1, MAX_MATCH_CHARS - 1, |window| probe(0, window))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenize::count_tokens;

    /// The fitters' per-prefix page counts rest on this decomposition: at any point of
    /// the streaming push, the emitted (final) lines plus the pending lines rendered
    /// against the current state must equal a from-scratch render of exactly the entries
    /// pushed so far — at every context depth and in both modes (block merging, run
    /// merging, separators, and same-line occurrences included).
    #[test]
    fn streamed_body_decomposition_matches_the_direct_render() {
        let text = "l1\nl2 M\nl3 M\nl4\nl5 M\nl6 M\nl7 M\nl8\nl9 M\nl10 M\n";
        let table = LineTable::new(text);
        let files = [ContentFile {
            rel: "f.rs",
            table: &table,
        }];
        let mut entries: Vec<(usize, RawEntry)> = Vec::new();
        for line in [2u64, 3, 5, 6, 7, 9, 10] {
            let range = table.line_range(line).unwrap();
            let start = range.start + text[range.clone()].find('M').unwrap();
            entries.push((
                0,
                RawEntry {
                    text_range: start..start + 1,
                    start_line: line,
                    end_line: line,
                    spans: vec![LineSpan {
                        line,
                        char_start: start - range.start,
                        char_len: 1,
                    }],
                },
            ));
        }
        for (before, after) in [(0usize, 0usize), (1, 1), (2, 2)] {
            for only_matching in [false, true] {
                for single_file_target in [false, true] {
                    let options = ContentRenderOptions {
                        before,
                        after,
                        match_window: MAX_MATCH_CHARS,
                        line_numbers: true,
                        only_matching,
                        single_file_target,
                    };
                    let direct = render_content_lines(&files, &entries, options);
                    let mut renderer = ContentRenderer::new(&files, options);
                    for shown in 1..=entries.len() {
                        let (file_index, entry) = &entries[shown - 1];
                        renderer.push(*file_index, entry);
                        let mut decomposed = renderer.lines().to_vec();
                        decomposed.extend(renderer.pending_lines());
                        let prefix = render_content_lines(&files, &entries[..shown], options);
                        assert_eq!(
                            decomposed, prefix,
                            "decomposition diverges at shown={shown} (before={before}, after={after}, only_matching={only_matching}, single={single_file_target})"
                        );
                    }
                    renderer.finish();
                    assert_eq!(
                        renderer.lines(),
                        direct,
                        "full render diverges (before={before}, after={after}, only_matching={only_matching}, single={single_file_target})"
                    );
                }
            }
        }
    }

    #[test]
    fn match_line_at_500_bytes_prints_whole() {
        // Exactly the threshold: ≤ LONG_LINE_BYTES prints whole, no window artifacts.
        let line = format!("{}NEEDLE", "a".repeat(494));
        assert_eq!(line.len(), LONG_LINE_BYTES);
        let rendered = format_match_line("", &line, &[(494, 500)], MAX_MATCH_CHARS);
        assert_eq!(rendered, line);
    }

    #[test]
    fn match_line_past_500_bytes_takes_window() {
        // One byte over the threshold: the window cut with both ellipses and the size
        // suffix; the head match sits fully inside the window.
        let line = format!("{}NEEDLEMARK{}", "a".repeat(200), "b".repeat(291));
        assert_eq!(line.len(), LONG_LINE_BYTES + 1);
        let rendered = format_match_line("", &line, &[(200, 210)], MAX_MATCH_CHARS);
        let expected = format!(
            "…{}… [line spans 501 chars; window centered on match(es)]",
            line.chars().skip(100).take(210).collect::<String>()
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn context_line_boundary_uses_byte_threshold() {
        let short = "x".repeat(500);
        assert_eq!(format_context_line("", &short), short);
        // The marker counts chars, not bytes: 600 bytes of 3-byte chars → 200 chars.
        let long = "漢".repeat(200);
        assert_eq!(long.len(), 600);
        assert_eq!(
            format_context_line("4-", &long),
            "4-[long line skipped: 200 chars]"
        );
    }

    #[test]
    fn cjk_match_window_never_splits_a_char() {
        // 3-byte chars on both sides of the match: the window must be char-aligned.
        let line = format!("{}NEEDLE{}", "漢".repeat(300), "字".repeat(300));
        assert!(line.len() > LONG_LINE_BYTES);
        let rendered = format_match_line("", &line, &[(300, 306)], MAX_MATCH_CHARS);
        let expected = format!(
            "…{}… [line spans 606 chars; window centered on match(es)]",
            line.chars().skip(200).take(206).collect::<String>()
        );
        assert_eq!(rendered, expected);
        assert!(!rendered.contains('\u{FFFD}'), "char was split: {rendered}");
    }

    #[test]
    fn match_line_without_spans_windows_from_line_start() {
        // No match position applies (e.g. the middle line of a multiline match): the
        // window anchors at the line start, not the omission marker — the marker is
        // reserved for context lines.
        let line = "x".repeat(600);
        let rendered = format_match_line("", &line, &[], MAX_MATCH_CHARS);
        let expected = format!(
            "{}… [line spans 600 chars; window centered on match(es)]",
            "x".repeat(100)
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn truncated_rendering_costs_fewer_tokens_than_full_line() {
        // The truncated rendering is what token accounting measures: it must come in
        // under the full line it replaces.
        let line = format!("{}NEEDLE{}", "a".repeat(50_000), "b".repeat(50_000));
        let rendered = format_match_line("", &line, &[(50_000, 50_006)], MAX_MATCH_CHARS);
        let counted = count_tokens(&rendered);
        assert!(counted > 0);
        assert!(
            counted < count_tokens(&line),
            "truncated rendering must count fewer tokens than the full line"
        );
    }
}
