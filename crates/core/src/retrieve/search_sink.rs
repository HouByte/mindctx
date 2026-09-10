// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-file search sink: a [`grep_searcher::Sink`] over one sealed,
//! decoded file. Occurrences are counted by re-running `find_iter` over each delivered
//! match chunk (per matched line in line mode; whole-buffer search in multiline mode);
//! content entries (one matching line, or one occurrence in `only_matching`/multiline
//! plans) are collected under the over-fetch probe with a per-file capture abort at 64 MiB.

use std::ops::Range;

use grep_matcher::LineTerminator;
use grep_matcher::Match;
use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::Searcher;
use grep_searcher::SearcherBuilder;
use grep_searcher::Sink;
use grep_searcher::SinkMatch;

/// Searcher heap limit: one line or multiline buffer above this fails the
/// file's search instead of exhausting memory.
pub(crate) const SEARCH_HEAP_LIMIT_BYTES: usize = 64 * 1024 * 1024;

/// Per-file capture abort: collected content beyond this skips the file with
/// the frozen reason [`CAPTURE_OVERFLOW_REASON`].
pub(crate) const CAPTURE_HEAP_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// Frozen skip reason for the capture abort.
pub(crate) const CAPTURE_OVERFLOW_REASON: &str =
    "matching content and context exceed the 64 MiB safety limit";

/// Which work one file's search must do (plan selection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SinkPlan {
    /// files_with_matches: total matching lines + the first matching line.
    Files,
    /// count: total occurrences (matches, not matching lines).
    Count,
    /// content, line plan (not multiline, not only_matching): one entry per matching line.
    ContentLine,
    /// content, occurrence plan (multiline or only_matching): one entry per occurrence.
    ContentOccurrence,
}

/// Pagination window for one file's content plan: skip the first `skip_entries` seen
/// entries (the still-unconsumed offset), collect at most `max_selected` after that
/// (the remaining over-fetch probe need), then stop the file's search.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ContentWindow {
    pub skip_entries: u64,
    pub max_selected: u64,
    /// Requested before/after context (full depth, for capture accounting; the renderer
    /// may degrade the depth later without changing what the abort accounts for).
    pub before_context: u64,
    pub after_context: u64,
}

/// One match span inside a line, in char offsets relative to the line's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LineSpan {
    pub line: u64,
    pub char_start: usize,
    pub char_len: usize,
}

/// One collected content entry: a matching line (Line plan) or one occurrence
/// (Occurrence plan), with the absolute byte range of the match in the decoded text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawEntry {
    /// Absolute byte range of the occurrence in the decoded text (the matched text).
    pub text_range: Range<usize>,
    pub start_line: u64,
    pub end_line: u64,
    /// Match spans per touched line (Line plan: the line's occurrences).
    pub spans: Vec<LineSpan>,
}

/// The output of one file's successful search.
#[derive(Debug, Default)]
pub(crate) struct SinkOutcome {
    /// line plans: total matching lines seen; files mode: 1 (the scan stops at the
    /// first match).
    pub matched_lines: u64,
    /// count/occurrence plans: total occurrences seen.
    pub occurrence_total: u64,
    /// content: every entry seen (selected or offset-skipped) until the sink stopped.
    pub entries_seen: u64,
    /// content: selected entries (post-offset, within the window cap).
    pub entries: Vec<RawEntry>,
    /// files mode: first matching line and its content.
    pub first_match: Option<(u64, String)>,
}

/// Why one file's search failed. The Display is the skip reason.
#[derive(Debug)]
pub(crate) enum SinkError {
    /// Collected content crossed the capture abort; the reason is frozen.
    CaptureOverflow,
    /// Any other failure (searcher heap limit, matcher error).
    Failed(String),
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SinkError::CaptureOverflow => f.write_str(CAPTURE_OVERFLOW_REASON),
            SinkError::Failed(message) => f.write_str(message),
        }
    }
}

/// Runs one file's search to completion over its decoded text and returns the outcome.
///
/// `text` is the decoded haystack. `lines` carries the byte-offset line table and is
/// required for content plans (they map matches onto lines); files/count plans only
/// consume counters and pass `None` so the table is never materialized. `window` is
/// only consulted for content plans; `capture_limit` bounds the collected content bytes
/// test seam for the 64 MiB contract abort).
pub(crate) fn search_file(
    matcher: &RegexMatcher,
    plan: SinkPlan,
    text: &str,
    lines: Option<&LineTable<'_>>,
    window: ContentWindow,
    multiline: bool,
    capture_limit: u64,
) -> Result<SinkOutcome, SinkError> {
    let mut sink = SearchSink {
        matcher,
        plan,
        text,
        lines,
        window,
        capture_limit,
        outcome: SinkOutcome::default(),
        selected: 0,
        capture_cursor: 0,
        captured_bytes: 0,
        overflow: false,
    };
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .line_terminator(LineTerminator::crlf())
        .multi_line(multiline)
        .heap_limit(Some(SEARCH_HEAP_LIMIT_BYTES))
        .build();
    match searcher.search_slice(matcher, text.as_bytes(), &mut sink) {
        Ok(()) => Ok(sink.outcome),
        Err(_) if sink.overflow => Err(SinkError::CaptureOverflow),
        Err(error) => Err(SinkError::Failed(error.to_string())),
    }
}

/// Byte-offset line table over one decoded file: line N (1-based) is `contents[N-1]`,
/// starting at byte `starts[N-1]`. Total lines = newline count + 1, so a
/// file ending with a newline carries a virtual trailing empty line — the same line the
/// renderer would emit as context.
pub(crate) struct LineTable<'a> {
    text: &'a str,
    starts: Vec<usize>,
    contents: Vec<&'a str>,
}

impl<'a> LineTable<'a> {
    pub fn new(text: &'a str) -> Self {
        let mut starts = Vec::new();
        let mut contents = Vec::new();
        let mut line_start = 0usize;
        for (index, _) in text.match_indices('\n') {
            // A `\r` immediately before this newline belongs to the same line's content.
            let content_end = index
                .checked_sub(1)
                .filter(|&previous| previous >= line_start && text.as_bytes()[previous] == b'\r')
                .unwrap_or(index);
            starts.push(line_start);
            contents.push(&text[line_start..content_end]);
            line_start = index + 1;
        }
        // The line after the last newline always exists: a virtual empty line for a
        // text ending in a newline, the final unterminated line otherwise.
        starts.push(line_start);
        contents.push(&text[line_start..]);
        Self {
            text,
            starts,
            contents,
        }
    }

    pub fn text(&self) -> &'a str {
        self.text
    }

    pub fn total_lines(&self) -> u64 {
        self.contents.len() as u64
    }

    /// Content of line `n` (1-based).
    pub fn line(&self, n: u64) -> Option<&'a str> {
        self.contents.get(n.checked_sub(1)? as usize).copied()
    }

    /// Byte range of line `n`'s content (1-based).
    pub fn line_range(&self, n: u64) -> Option<Range<usize>> {
        let index = n.checked_sub(1)? as usize;
        let start = *self.starts.get(index)?;
        Some(start..start + self.contents[index].len())
    }

    /// The 1-based line whose content range contains `offset` (clamped into the table).
    fn line_at(&self, offset: usize) -> u64 {
        match self.starts.partition_point(|&start| start <= offset) {
            0 => 1,
            index => index.min(self.contents.len()) as u64,
        }
    }

    /// start_line, end_line) for a match spanning absolute byte offsets.
    pub fn locate_span(&self, start: usize, end: usize) -> (u64, u64) {
        let first = self.line_at(start);
        let last = if end > start {
            self.line_at(end - 1)
        } else {
            first
        };
        (first, last.max(first))
    }

    /// Char offset of an absolute byte offset within its line.
    fn char_offset(&self, line: u64, offset: usize) -> usize {
        let Some(range) = self.line_range(line) else {
            return 0;
        };
        let clamped = offset.clamp(range.start, range.end);
        self.text[range.start..clamped].chars().count()
    }

    /// Char length of an absolute byte range, counted within `line`'s content.
    fn char_len(&self, line: u64, start: usize, end: usize) -> usize {
        let Some(range) = self.line_range(line) else {
            return 0;
        };
        let from = start.clamp(range.start, range.end);
        let to = end.clamp(range.start, range.end);
        self.text[from..to.max(from)].chars().count()
    }

    /// Spans of one occurrence across the lines it touches (multiline mapping).
    pub fn spans_for_range(&self, start: usize, end: usize) -> Vec<LineSpan> {
        let (start_line, end_line) = self.locate_span(start, end);
        let mut spans = Vec::with_capacity((end_line - start_line + 1) as usize);
        for line in start_line..=end_line {
            let Some(range) = self.line_range(line) else {
                continue;
            };
            let overlap_start = start.max(range.start).min(range.end);
            let overlap_end = end.min(range.end).max(overlap_start);
            let char_start = self.char_offset(line, overlap_start);
            let char_len = if overlap_start < overlap_end {
                self.char_len(line, overlap_start, overlap_end)
            } else {
                0
            };
            spans.push(LineSpan {
                line,
                char_start,
                char_len,
            });
        }
        spans
    }

    /// Spans of the occurrences found in one matched line's bytes (`SinkMatch::bytes`
    /// includes the terminator, so offsets are clamped into the line content). The raw
    /// offsets are line-relative and are lifted onto the file's absolute byte range.
    pub fn spans_for_line_bytes(&self, line: u64, spans: &[(usize, usize)]) -> Vec<LineSpan> {
        let Some(range) = self.line_range(line) else {
            return Vec::new();
        };
        let content_len = range.end - range.start;
        spans
            .iter()
            .map(|&(start, end)| {
                let start = range.start + start.min(content_len);
                let end = range.start + end.min(content_len).max(start.min(content_len));
                LineSpan {
                    line,
                    char_start: self.char_offset(line, start),
                    char_len: self.char_len(line, start, end),
                }
            })
            .collect()
    }
}

/// Counts occurrences (matches) in one delivered chunk, skipping the synthetic trailing
/// zero-width match ripgrep reports at the end of a newline-terminated buffer.
fn count_occurrences(matcher: &RegexMatcher, bytes: &[u8]) -> Result<u64, String> {
    let mut count = 0u64;
    matcher
        .find_iter(bytes, |found| {
            if phantom_tail_match(bytes, found) {
                return true;
            }
            count += 1;
            true
        })
        .map_err(|error| error.to_string())?;
    Ok(count)
}

/// Collects the (start, end) byte offsets of every occurrence in one chunk.
fn collect_occurrences(
    matcher: &RegexMatcher,
    bytes: &[u8],
    out: &mut Vec<(usize, usize)>,
) -> Result<(), String> {
    matcher
        .find_iter(bytes, |found| {
            if phantom_tail_match(bytes, found) {
                return true;
            }
            out.push((found.start(), found.end()));
            true
        })
        .map_err(|error| error.to_string())
}

/// The zero-width match ripgrep reports at the very end of a newline-terminated chunk is
/// an artifact of the terminator, not a real occurrence.
fn phantom_tail_match(chunk: &[u8], found: Match) -> bool {
    found.start() == found.end() && found.end() == chunk.len() && chunk.last() == Some(&b'\n')
}

struct SearchSink<'a> {
    matcher: &'a RegexMatcher,
    plan: SinkPlan,
    text: &'a str,
    lines: Option<&'a LineTable<'a>>,
    window: ContentWindow,
    capture_limit: u64,
    outcome: SinkOutcome,
    selected: u64,
    capture_cursor: u64,
    captured_bytes: u64,
    overflow: bool,
}

impl SearchSink<'_> {
    /// Whether this entry may still be selected under the pagination window.
    fn select(&mut self) -> bool {
        let selected = self.outcome.entries_seen > self.window.skip_entries
            && self.selected < self.window.max_selected;
        if selected {
            self.selected += 1;
        }
        selected
    }

    fn window_reached(&self) -> bool {
        self.selected >= self.window.max_selected
    }

    /// The line table (content plans only — the caller always supplies it there).
    fn table(&self) -> &LineTable<'_> {
        self.lines.expect("content plans always carry a line table")
    }

    /// Accounts the capture bytes of one selected entry's context window; a monotonic
    /// cursor counts each stored line once. Crossing the limit aborts the file's search.
    fn account_capture(&mut self, start_line: u64, end_line: u64) {
        let total = self.table().total_lines();
        let window_start = start_line.saturating_sub(self.window.before_context).max(1);
        let window_end = end_line
            .saturating_add(self.window.after_context)
            .min(total);
        let from = self.capture_cursor.max(window_start.saturating_sub(1));
        for line in (from + 1)..=window_end {
            self.captured_bytes += self.table().line(line).map_or(0, str::len) as u64;
        }
        self.capture_cursor = self.capture_cursor.max(window_end);
        self.overflow |= self.captured_bytes > self.capture_limit;
    }
}

impl Sink for SearchSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.overflow {
            return Err(capture_overflow_io());
        }
        let bytes = m.bytes();
        let absolute = m.absolute_byte_offset() as usize;
        let start_line = m.line_number().unwrap_or(1);
        let matcher = self.matcher;
        match self.plan {
            SinkPlan::Files => {
                self.outcome.matched_lines += 1;
                if self.outcome.first_match.is_none() {
                    let text = line_at(self.text, absolute);
                    self.outcome.first_match = Some((start_line, text.to_string()));
                }
                // Files mode only needs the fact of a match: stop the file's scan at the
                // first one instead of indexing and scanning the rest of the file.
                Ok(false)
            }
            SinkPlan::Count => {
                let count = count_occurrences(matcher, bytes).map_err(std::io::Error::other)?;
                self.outcome.occurrence_total += count;
                Ok(true)
            }
            SinkPlan::ContentLine => {
                self.outcome.matched_lines += 1;
                self.outcome.entries_seen += 1;
                if self.select() {
                    let mut spans_raw = Vec::new();
                    collect_occurrences(matcher, bytes, &mut spans_raw)
                        .map_err(std::io::Error::other)?;
                    let spans = self.table().spans_for_line_bytes(start_line, &spans_raw);
                    let end = (absolute + bytes.len()).min(self.text.len());
                    self.outcome.entries.push(RawEntry {
                        text_range: absolute..end,
                        start_line,
                        end_line: start_line,
                        spans,
                    });
                    self.account_capture(start_line, start_line);
                    if self.overflow {
                        return Err(capture_overflow_io());
                    }
                }
                Ok(!self.window_reached())
            }
            SinkPlan::ContentOccurrence => {
                let text_len = self.text.len();
                let mut occurrences = Vec::new();
                collect_occurrences(matcher, bytes, &mut occurrences)
                    .map_err(std::io::Error::other)?;
                for (local_start, local_end) in occurrences {
                    self.outcome.occurrence_total += 1;
                    self.outcome.entries_seen += 1;
                    if !self.select() {
                        continue;
                    }
                    let start = (absolute + local_start).min(text_len);
                    let end = (absolute + local_end).min(text_len).max(start);
                    let (first_line, last_line) = self.table().locate_span(start, end);
                    let spans = self.table().spans_for_range(start, end);
                    self.outcome.entries.push(RawEntry {
                        text_range: start..end,
                        start_line: first_line,
                        end_line: last_line,
                        spans,
                    });
                    self.account_capture(first_line, last_line);
                    if self.overflow {
                        return Err(capture_overflow_io());
                    }
                    if self.window_reached() {
                        return Ok(false);
                    }
                }
                Ok(!self.window_reached())
            }
        }
    }
}

fn capture_overflow_io() -> std::io::Error {
    std::io::Error::other(CAPTURE_OVERFLOW_REASON)
}

/// The content of the line containing `offset` (terminator dropped, a `\r` immediately
/// before its `\n` stripped) — the same render [`LineTable::line`] produces, without
/// materializing the table. Used by files mode, which records the first matching line
/// and stops.
fn line_at(text: &str, offset: usize) -> &str {
    let bytes = text.as_bytes();
    let start = bytes[..offset.min(bytes.len())]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |position| position + 1);
    match bytes[offset..].iter().position(|&b| b == b'\n') {
        Some(position) => {
            let end = offset + position;
            // A `\r` immediately before the newline belongs to the terminator, not the
            // content — but only when it starts this line's content.
            let content_end = if end > start && bytes[end - 1] == b'\r' {
                end - 1
            } else {
                end
            };
            &text[start..content_end]
        }
        // No trailing newline: the final unterminated line keeps every byte.
        None => &text[start..],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grep_regex::RegexMatcherBuilder;

    /// The per-file capture abort skips the file with the frozen reason;
    /// the limit is a parameter so the contract is testable without a 64 MiB fixture.
    #[test]
    fn capture_overflow_uses_the_frozen_reason() {
        let text = "MATCH LINE\n".repeat(64);
        let table = LineTable::new(&text);
        let matcher = RegexMatcherBuilder::new().build("MATCH").unwrap();
        let window = ContentWindow {
            skip_entries: 0,
            max_selected: 1000,
            before_context: 0,
            after_context: 0,
        };
        let error = search_file(
            &matcher,
            SinkPlan::ContentLine,
            &text,
            Some(&table),
            window,
            false,
            100,
        )
        .expect_err("the capture limit must abort the file");
        assert!(matches!(error, SinkError::CaptureOverflow));
        assert_eq!(
            error.to_string(),
            "matching content and context exceed the 64 MiB safety limit"
        );
    }

    /// `line_at` renders the same line content [`LineTable::line`] would, without the
    /// table — files mode relies on this for its first-match record.
    #[test]
    fn line_at_matches_the_line_table_render() {
        for text in [
            "first\nsecond\r\nthird\n",
            "no terminator at eof",
            "ends with cr but no newline\r",
            "\n\nthird line\n",
            "",
            "single\r\n",
        ] {
            let table = LineTable::new(text);
            for n in 1..=table.total_lines() {
                let expected = table.line(n).unwrap_or_default();
                let offset = table.line_range(n).map_or(text.len(), |r| r.start);
                assert_eq!(line_at(text, offset), expected, "text={text:?} line={n}");
            }
        }
    }
}
