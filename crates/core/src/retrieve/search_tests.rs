// SPDX-License-Identifier: MIT OR Apache-2.0

//! Contract tests for search: one focused test per frozen behavior.
//! Fixture corpora live in `crates/tests/fixtures/search/`;
//! tests that need controlled mtimes write into a tempdir and use `File::set_modified`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::glob_args::GlobArgs;
use super::{SearchOutput, SearchParams, search_with_budget};
use crate::envelope::Envelope;

const DEFAULT_BUDGET: u64 = 8_500;

fn fixture_root(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/fixtures/search")
        .join(rel)
        .canonicalize()
        .expect("search fixture must exist")
}

fn polyglot_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/fixtures/polyglot")
        .canonicalize()
        .expect("polyglot fixture must exist")
}

fn params(pattern: &str) -> SearchParams {
    SearchParams {
        pattern: pattern.to_string(),
        ..SearchParams::default()
    }
}

fn run(root: &Path, p: &SearchParams) -> Envelope {
    search_with_budget(root, p, DEFAULT_BUDGET).expect("search should not fail")
}

fn run_b(root: &Path, p: &SearchParams, budget: u64) -> Result<Envelope, crate::error::Error> {
    search_with_budget(root, p, budget)
}

/// Writes a file under `root` (creating parent dirs) and stamps an explicit mtime so
/// candidate order is deterministic regardless of checkout time.
fn write_at(root: &Path, rel: &str, content: &str, mtime: SystemTime) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, content).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
}

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// The shown file paths, parsed from the rendered page: one bare path per line in
/// files_with_matches mode, one `path:count` per line in count mode (the body is the
/// page's first blank-line-separated chunk; the status note rides after it).
fn paths(env: &Envelope) -> Vec<String> {
    env.text
        .as_deref()
        .unwrap_or_default()
        .split("\n\n")
        .next()
        .unwrap_or_default()
        .lines()
        .map(|line| line.split(':').next().unwrap_or(line).to_string())
        .filter(|path| !path.is_empty())
        .collect()
}

/// Matcher contract: CRLF-aware `^`/`$` anchors, and multiline pattern
/// preprocessing rewriting a literal `\n` (or an actual newline) to `\r?\n`.
#[test]
fn grep_syntax_and_crlf_anchors() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // Mixed line endings: lines 1-2 CRLF, lines 3-4 LF.
    write_at(root, "mixed.txt", "foo\r\nbar foo\r\nfoo\nbaz\n", at(100));

    // `$` anchors are CRLF-aware even in non-multiline mode: 3 matching lines.
    let env = run(root, &params("foo$"));
    assert_eq!(paths(&env), ["mixed.txt"], "CRLF-aware `$` anchor: {env:?}");

    // `^` anchors likewise.
    let env = run(root, &params("^bar"));
    assert_eq!(paths(&env), ["mixed.txt"]);

    // Multiline: an actual newline in the pattern matches across the CRLF pair
    // `foo\r\nbar` — the normalized `foo\r?\nbar`).
    let env = run(
        root,
        &SearchParams {
            pattern: "foo\nbar".to_string(),
            multiline: true,
            ..SearchParams::default()
        },
    );
    assert_eq!(
        paths(&env),
        ["mixed.txt"],
        "newline pattern spans CRLF lines"
    );

    // A literal `\n` escape (odd backslash run) is normalized the same way.
    let env = run(
        root,
        &SearchParams {
            pattern: "foo\\nbar".to_string(),
            multiline: true,
            ..SearchParams::default()
        },
    );
    assert_eq!(paths(&env), ["mixed.txt"], "literal \\n escape normalized");
}

/// count = occurrences (total matches), never matching lines.
#[test]
fn count_is_occurrences_not_lines() {
    let root = fixture_root("count");
    let env = run(
        &root,
        &SearchParams {
            pattern: "needle".to_string(),
            output_mode: SearchOutput::Count,
            ..SearchParams::default()
        },
    );
    assert_eq!(paths(&env), ["occurrences.txt"]);
    // Line 1 carries three occurrences, line 3 one: 4 occurrences on 2 matching lines.
    assert_eq!(
        env.text.as_deref(),
        Some("occurrences.txt:4\n\n(Shown: 4 occurrences in total across 1 file.)")
    );
    assert_eq!(
        env.terminal.as_ref().unwrap().note.as_deref(),
        Some("(Shown: 4 occurrences in total across 1 file.)")
    );
}

/// Candidate order is mtime DESC (then path asc) — not path order.
#[test]
fn mtime_ordering_not_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // Path order would be a, m, z; mtime order is m (newest), a, z (oldest).
    write_at(root, "a.txt", "hit\n", at(200));
    write_at(root, "m.txt", "hit\n", at(300));
    write_at(root, "z.txt", "hit\n", at(100));
    let env = run(root, &params("hit"));
    assert_eq!(paths(&env), ["m.txt", "a.txt", "z.txt"]);
}

/// Binary files are silently excluded: no result, no skip detail.
#[test]
fn binary_silently_excluded() {
    let root = fixture_root("encoding");

    // A directory whose only candidate is binary: silent exclusion, clean zero result.
    let env = run(
        &root,
        &SearchParams {
            pattern: "needle".to_string(),
            glob: GlobArgs(vec!["*.bin".to_string()]),
            ..SearchParams::default()
        },
    );
    assert!(
        env.skip_report.is_none(),
        "binary exclusion is silent: {:?}",
        env.skip_report
    );
    assert!(paths(&env).is_empty());
    assert_eq!(
        env.terminal.as_ref().unwrap().note.as_deref(),
        Some("(Shown: nothing; no file matched.)")
    );

    // Searched broadly, the binary still never surfaces as a skip detail.
    let env = run(&root, &params("needle"));
    assert!(
        !paths(&env).contains(&"binary.bin".to_string()),
        "binary files never match: {:?}",
        paths(&env)
    );
    let report = env.skip_report.as_ref().unwrap();
    assert!(
        report
            .details
            .iter()
            .all(|detail| detail.path != "binary.bin"),
        "binary exclusion is not a skip detail: {:?}",
        report.details
    );
}

/// files_with_matches is the default mode; pagination notes follow the frozen grammar
/// shown range + resume pointer / all shown / end of results / offset exhausted).
#[test]
fn files_mode_default_and_pagination() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for (index, name) in ["f1", "f2", "f3", "f4", "f5"].iter().enumerate() {
        write_at(
            root,
            &format!("{name}.txt"),
            "hit\n",
            at(100 + index as u64),
        );
    }

    // Default output mode is files_with_matches and the default head_limit shows all.
    let env = run(root, &params("hit"));
    assert_eq!(
        paths(&env),
        ["f5.txt", "f4.txt", "f3.txt", "f2.txt", "f1.txt"]
    );
    assert!(!env.truncated);
    assert_eq!(
        env.terminal.as_ref().unwrap().note.as_deref(),
        Some("(Shown: files 1-5. All 5 files shown.)")
    );

    // Partial page: the continuation offset is the last shown entry.
    let paged = run(
        root,
        &SearchParams {
            pattern: "hit".to_string(),
            head_limit: Some(2),
            ..SearchParams::default()
        },
    );
    assert!(paged.truncated);
    assert_eq!(paged.terminal.as_ref().unwrap().total, None);
    assert_eq!(
        paged.terminal.as_ref().unwrap().note.as_deref(),
        Some("(Shown: files 1-2. More remain — resume from offset=2.)")
    );

    // Offset into the last page: singular range grammar.
    let last = run(
        root,
        &SearchParams {
            pattern: "hit".to_string(),
            offset: 4,
            ..SearchParams::default()
        },
    );
    assert!(!last.truncated);
    assert_eq!(
        last.terminal.as_ref().unwrap().note.as_deref(),
        Some("(Shown: file 5. Reached the end of results.)")
    );

    // Offset past the end: the exhausted grammar with the singular verb.
    let beyond = run(
        root,
        &SearchParams {
            pattern: "hit".to_string(),
            offset: 10,
            ..SearchParams::default()
        },
    );
    assert!(beyond.text.is_none(), "offset past the end is note-only");
    assert_eq!(
        beyond.terminal.as_ref().unwrap().note.as_deref(),
        Some("(Shown: none at offset=10; the scan saw only 5 files in all.)")
    );
    let single_tmp = tempfile::tempdir().unwrap();
    write_at(single_tmp.path(), "only.txt", "hit\n", at(1));
    let single = run(
        single_tmp.path(),
        &SearchParams {
            pattern: "hit".to_string(),
            offset: 10,
            ..SearchParams::default()
        },
    );
    assert_eq!(
        single.terminal.as_ref().unwrap().note.as_deref(),
        Some("(Shown: none at offset=10; the scan saw only 1 file in all.)")
    );
}

/// `head_limit=0` removes the 250-entry default and takes the budget relation
/// `budget*4+1`; the probe early-stop leaves the total unknown.
#[test]
fn head_limit_zero_uses_budget_relation() {
    // 300 files with budget 200: the relation allows 801 entries, so head_limit=0
    // completes the scan where the 250 default early-stops at its probe.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for index in 0..300 {
        write_at(
            root,
            &format!("f{index:03}.txt"),
            "hit\n",
            at(100 + index as u64),
        );
    }
    let unlimited = run_b(
        root,
        &SearchParams {
            pattern: "hit".to_string(),
            head_limit: Some(0),
            ..SearchParams::default()
        },
        200,
    )
    .unwrap();
    assert_eq!(
        unlimited.terminal.as_ref().unwrap().total,
        Some(300),
        "head_limit=0 lifts the 250 default (scan completes)"
    );
    assert!(unlimited.truncated, "the token budget still caps the page");
    let limited = run_b(root, &params("hit"), 200).unwrap();
    assert_eq!(
        limited.terminal.as_ref().unwrap().total,
        None,
        "the 250 default early-stops over 300 files"
    );

    // 810 files exceed the relation 200*4+1=801(+1 probe): even head_limit=0 stops early.
    let big = tempfile::tempdir().unwrap();
    for index in 0..810 {
        write_at(
            big.path(),
            &format!("f{index:03}.txt"),
            "hit\n",
            at(100 + index as u64),
        );
    }
    let capped = run_b(
        big.path(),
        &SearchParams {
            pattern: "hit".to_string(),
            head_limit: Some(0),
            ..SearchParams::default()
        },
        200,
    )
    .unwrap();
    assert_eq!(
        capped.terminal.as_ref().unwrap().total,
        None,
        "head_limit=0 is capped at budget*4+1, not infinite"
    );
}

/// content mode groups by file: bare path header, one blank line between groups,
/// `"{n}:"` matches and `"{n}-"` context.
#[test]
fn content_grouping_and_separators() {
    let root = fixture_root("content");
    let env = run(
        &root,
        &SearchParams {
            pattern: "MATCHLINE".to_string(),
            output_mode: SearchOutput::Content,
            context: Some(1),
            ..SearchParams::default()
        },
    );
    let text = env.text.as_deref().expect("content mode renders text");
    // 3 matching lines across 2 files → 3 match lines on the page.
    assert_eq!(text.lines().filter(|l| l.contains("MATCHLINE")).count(), 3);

    // File groups are blank-line separated chunks, each headed by the bare path
    // the terminal note rides after the body, separated the same way).
    let body = text.split("\n\n(Shown:").next().unwrap();
    let mut groups: Vec<String> = body.split("\n\n").map(str::to_string).collect();
    groups.sort();
    assert_eq!(groups.len(), 2, "one group per matching file: {text}");
    assert_eq!(
        groups[0],
        "groups_a.md\n1-aaa top\n2:MATCHLINE one\n3-ccc mid\n--\n5-eee mid\n6:MATCHLINE two\n7-ggg bottom"
    );
    assert_eq!(
        groups[1],
        "groups_b.md\n1-bbb top\n2:MATCHLINE here\n3-zzz bottom"
    );
}

/// Long lines: ≤500 bytes print whole; longer ones take a window cut with `…` and the
/// frozen markers; >500-byte context lines collapse to the omission marker.
#[test]
fn long_line_window_markers() {
    let root = fixture_root("long");
    let env = run(
        &root,
        &SearchParams {
            pattern: "NEEDLEMARK|M{3,}".to_string(),
            output_mode: SearchOutput::Content,
            after_context: 1,
            path: Some("long_line.md".to_string()),
            ..SearchParams::default()
        },
    );
    let text = env.text.as_deref().expect("content text");
    // 612-byte line: window cut with the size suffix (single match → no beyond note).
    assert!(
        text.contains("[line spans 612 chars; window centered on match(es)]"),
        "window suffix missing: {text}"
    );
    assert!(text.contains('…'), "window ellipses missing: {text}");
    // 2500-char match: the shortened-match marker.
    assert!(
        text.contains("... [match shortened: 2500 chars in full]"),
        "shortened-match marker missing: {text}"
    );
    assert!(
        text.contains("[line spans 2500 chars; window centered on match(es)]"),
        "second window suffix missing: {text}"
    );
    // 600-byte context line collapses to the omission marker.
    assert!(
        text.contains("4-[long line skipped: 600 chars]"),
        "long context omission missing: {text}"
    );
    assert!(!text.contains("more matches lie beyond"));
}

/// Multiple >window matches on one long line add the frozen outside-note to the suffix.
#[test]
fn long_line_outside_note() {
    let root = fixture_root("long");
    let env = run(
        &root,
        &SearchParams {
            pattern: "M{3,}".to_string(),
            output_mode: SearchOutput::Content,
            path: Some("long_line_multi.md".to_string()),
            ..SearchParams::default()
        },
    );
    let text = env.text.as_deref().expect("content text");
    assert!(
        text.contains(
            "[line spans 5001 chars; window centered on match(es); more matches lie beyond this window]"
        ),
        "beyond-note missing: {text}"
    );
}

/// CJK long line end-to-end: match spans are char positions, so the window never splits
/// a 3-byte char, and the truncated page costs fewer tokens than the full line would.
#[test]
fn long_line_cjk_window() {
    let root = fixture_root("long");
    let env = run(
        &root,
        &SearchParams {
            pattern: "NEEDLEMARK".to_string(),
            output_mode: SearchOutput::Content,
            path: Some("long_line_cjk.md".to_string()),
            ..SearchParams::default()
        },
    );
    let text = env.text.as_deref().expect("content text");
    let full_line = format!("{}NEEDLEMARK{}", "漢字".repeat(200), "語句".repeat(150));
    assert_eq!(full_line.len(), 2110, "fixture line drifted");
    let expected = format!(
        "1:…{}… [line spans 710 chars; window centered on match(es)]",
        full_line.chars().skip(300).take(210).collect::<String>()
    );
    assert!(text.contains(&expected), "char-split window: {text}");
    assert!(!text.contains('\u{FFFD}'), "a char was split: {text}");
    assert!(
        env.token_usage.returned <= crate::tokenize::count_tokens(&full_line),
        "truncated page must not count more than the full line"
    );
}

/// only_matching: one line per occurrence, newlines escaped as literal `\n`/`\r`
/// sequences, and long occurrences truncated with the frozen marker.
#[test]
fn only_matching_escaping() {
    let root = fixture_root("only");

    // Multiline occurrence: the spanning match prints once, newline escaped.
    let env = run(
        &root,
        &SearchParams {
            pattern: "NEEDLE\nNEEDLE".to_string(),
            output_mode: SearchOutput::Content,
            only_matching: true,
            multiline: true,
            glob: GlobArgs(vec!["only_match.txt".to_string()]),
            ..SearchParams::default()
        },
    );
    let text = env.text.as_deref().expect("content text");
    assert!(
        text.contains("1:NEEDLE\\nNEEDLE"),
        "escaped newline missing: {text}"
    );

    // Two occurrences on one line → one output line per occurrence.
    let env = run(
        &root,
        &SearchParams {
            pattern: "NEEDLE".to_string(),
            output_mode: SearchOutput::Content,
            only_matching: true,
            glob: GlobArgs(vec!["only_multi.txt".to_string()]),
            ..SearchParams::default()
        },
    );
    assert_eq!(
        env.text.as_deref(),
        Some("only_multi.txt\n1:NEEDLE\n1:NEEDLE\n\n(Shown: results 1-2. All 2 results shown.)")
    );

    // A 2500-char occurrence truncates at the window with the frozen marker.
    let env = run(
        &root,
        &SearchParams {
            pattern: "X{3,}".to_string(),
            output_mode: SearchOutput::Content,
            only_matching: true,
            glob: GlobArgs(vec!["only_long.txt".to_string()]),
            ..SearchParams::default()
        },
    );
    let text = env.text.as_deref().expect("content text");
    assert!(
        text.contains("... [match shortened: 2500 chars in full]"),
        "only-matching truncation marker missing: {text}"
    );
}

/// Distinct context blocks inside one file are separated by a literal `--`; `context`
/// overrides before/after; `line_numbers=false` drops the numeric prefixes.
#[test]
fn context_blocks_with_dashes() {
    let root = fixture_root("content");
    let env = run(
        &root,
        &SearchParams {
            pattern: "MATCHLINE".to_string(),
            output_mode: SearchOutput::Content,
            before_context: 1,
            after_context: 1,
            glob: GlobArgs(vec!["groups_a.md".to_string()]),
            ..SearchParams::default()
        },
    );
    assert_eq!(
        env.text.as_deref(),
        Some(
            "groups_a.md\n1-aaa top\n2:MATCHLINE one\n3-ccc mid\n--\n5-eee mid\n6:MATCHLINE two\n7-ggg bottom\n\n(Shown: results 1-2. All 2 results shown.)"
        )
    );

    // `context` overrides before/after (the wider request collapses to context=1).
    let wider = run(
        &root,
        &SearchParams {
            pattern: "MATCHLINE".to_string(),
            output_mode: SearchOutput::Content,
            before_context: 99,
            after_context: 0,
            context: Some(1),
            glob: GlobArgs(vec!["groups_a.md".to_string()]),
            ..SearchParams::default()
        },
    );
    assert_eq!(
        wider.text.as_deref(),
        Some(
            "groups_a.md\n1-aaa top\n2:MATCHLINE one\n3-ccc mid\n--\n5-eee mid\n6:MATCHLINE two\n7-ggg bottom\n\n(Shown: results 1-2. All 2 results shown.)"
        )
    );

    // line_numbers=false drops the prefixes.
    let bare = run(
        &root,
        &SearchParams {
            pattern: "MATCHLINE".to_string(),
            output_mode: SearchOutput::Content,
            context: Some(1),
            line_numbers: false,
            glob: GlobArgs(vec!["groups_b.md".to_string()]),
            ..SearchParams::default()
        },
    );
    assert_eq!(
        bare.text.as_deref(),
        Some(
            "groups_b.md\nbbb top\nMATCHLINE here\nzzz bottom\n\n(Shown: result 1. All 1 result shown.)"
        )
    );
}

/// The degradation ladder engages under a small budget: the response still fits the
/// body budget with shown count, context depth, and match window degraded — and a
/// large budget shows everything (the ladder is budget-driven, not a constant cap).
#[test]
fn degradation_ladder_fits_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // 20 matching lines of 800 bytes (windowed: > 500) interleaved with 400-byte
    // non-matching pad lines (printed whole as context). The full page (context 2,
    // 20 entries) is far beyond a small budget but fits a large one.
    let mut body = String::new();
    for index in 0..20 {
        body.push_str(&format!("{:03} MATCHLINE {}\n", index, "z".repeat(787)));
        body.push_str(&format!("pad {} {}\n", index, "q".repeat(392)));
    }
    write_at(root, "big.txt", &body, at(100));

    let small_params = SearchParams {
        pattern: "MATCHLINE".to_string(),
        output_mode: SearchOutput::Content,
        before_context: 2,
        after_context: 2,
        ..SearchParams::default()
    };
    let small = run_b(root, &small_params, 600).unwrap();
    let text = small.text.as_deref().expect("degraded page still has text");
    // Match lines, not context lines: the entry count the ladder degrades.
    let match_lines = |env: &Envelope| {
        env.text
            .as_deref()
            .unwrap_or_default()
            .split("\n\n")
            .next()
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("MATCHLINE"))
            .count()
    };
    assert!(small.text.is_some(), "never a bodyless success");
    assert!(
        small.token_usage.returned <= 600,
        "returned {} must fit the budget",
        small.token_usage.returned
    );
    // Budget = wire: the page the model receives is exactly what is budgeted (the
    // structured envelope JSON is machine-facing and no longer budget-constrained).
    let wire = crate::wire::render(&small);
    assert_eq!(wire, text, "the text wire is the rendered page");
    assert_eq!(
        crate::tokenize::count_tokens(&wire),
        small.token_usage.returned,
        "wire tokens == returned (1:1 accounting)"
    );
    assert!(
        match_lines(&small) < 20,
        "the shown count degraded: {}",
        match_lines(&small)
    );
    assert!(
        text.contains("[line spans 801 chars; window centered on match(es)]"),
        "the window degraded onto the long line: {}",
        &text[..text.len().min(2000)]
    );
    let large = run_b(root, &small_params, 400_000).unwrap();
    assert_eq!(match_lines(&large), 20, "large budget shows everything");
    assert!(
        large.text.as_deref().unwrap().len() > text.len() * 10,
        "the small-budget page is a strict degradation of the large one"
    );
}

/// summary ignores head_limit/offset, scans the whole scope, and answers with the
/// frozen totals note only (no results, no text).
#[test]
fn summary_ignores_pagination() {
    let root = fixture_root("count");
    let env = run(
        &root,
        &SearchParams {
            pattern: "needle".to_string(),
            output_mode: SearchOutput::Summary,
            head_limit: Some(0),
            offset: 5,
            ..SearchParams::default()
        },
    );
    assert_eq!(env.text, None);
    assert!(!env.truncated);
    // Budget = wire: the summary's wire is its note, and `returned` counts it exactly.
    let note = env.terminal.as_ref().unwrap().note.as_deref().unwrap();
    assert_eq!(
        env.token_usage.returned,
        crate::tokenize::count_tokens(note)
    );
    assert_eq!(
        env.terminal.as_ref().unwrap().note.as_deref(),
        Some("(Shown: 4 occurrences in total across 1 file.)")
    );

    let tmp = tempfile::tempdir().unwrap();
    write_at(tmp.path(), "a.txt", "hit hit\n", at(1));
    write_at(tmp.path(), "b.txt", "hit\n", at(2));
    let multi = run(
        tmp.path(),
        &SearchParams {
            pattern: "hit".to_string(),
            output_mode: SearchOutput::Summary,
            ..SearchParams::default()
        },
    );
    assert_eq!(
        multi.terminal.as_ref().unwrap().note.as_deref(),
        Some("(Shown: 3 occurrences in total across 2 files.)")
    );
}

/// Unknown standard type → the frozen recovery message; a known type still works.
#[test]
fn unknown_type_message() {
    let root = fixture_root("count");
    let err = run_b(
        &root,
        &SearchParams {
            pattern: "needle".to_string(),
            file_type: Some("nosuchtype".to_string()),
            ..SearchParams::default()
        },
        DEFAULT_BUDGET,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains(
            "Unknown file type: \"nosuchtype\". Filter by glob instead, or pick a standard type such as js, py, rust, go, java."
        ),
        "frozen unknown-type message, got: {err}"
    );

    let ok = run(
        &polyglot_root(),
        &SearchParams {
            pattern: "class ".to_string(),
            file_type: Some("py".to_string()),
            ..SearchParams::default()
        },
    );
    assert_eq!(
        paths(&ok),
        ["app.py"],
        "a standard type filters by glob set"
    );
}

/// `encoding` is single-file only; `fallback_encoding` is directory-only — frozen
/// guidance messages both ways.
#[test]
fn encoding_param_target_mismatch() {
    let root = fixture_root("count");
    let err = run_b(
        &root,
        &SearchParams {
            pattern: "needle".to_string(),
            encoding: Some("utf-8".to_string()),
            ..SearchParams::default()
        },
        DEFAULT_BUDGET,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains(
            "The encoding parameter is a single-file setting; for a directory target use fallback_encoding."
        ),
        "got: {err}"
    );

    let err = run_b(
        &fixture_root("encoding"),
        &SearchParams {
            pattern: "x".to_string(),
            path: Some("ambiguous.dat".to_string()),
            fallback_encoding: Some("gbk".to_string()),
            ..SearchParams::default()
        },
        DEFAULT_BUDGET,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains(
            "The fallback_encoding parameter is a directory-target setting; for a single file use encoding."
        ),
        "got: {err}"
    );
}

/// A single-file target escalates ANY skip into a hard error: ambiguous encodings get
/// the full ambiguity message, everything else the skip reason; an explicit `encoding`
/// rescues the file instead.
#[test]
fn single_file_skip_is_error() {
    let root = fixture_root("encoding");

    // Ambiguous single byte: the frozen ambiguity message names the candidate.
    let err = run_b(
        &root,
        &SearchParams {
            pattern: "x".to_string(),
            path: Some("ambiguous.dat".to_string()),
            ..SearchParams::default()
        },
        DEFAULT_BUDGET,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains(
            "Cannot confidently determine the text encoding of ambiguous.dat: the bytes decode cleanly as windows-1252."
        ),
        "ambiguity message, got: {err}"
    );

    // Undecodable bytes: the skip reason becomes the hard error.
    let err = run_b(
        &root,
        &SearchParams {
            pattern: "x".to_string(),
            path: Some("undecodable.dat".to_string()),
            ..SearchParams::default()
        },
        DEFAULT_BUDGET,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("undecodable"),
        "skip reason escalates, got: {err}"
    );

    // Explicit encoding rescues the single file: the needle becomes searchable.
    let env = run(
        &root,
        &SearchParams {
            pattern: "needle".to_string(),
            path: Some("gbk.txt".to_string()),
            encoding: Some("gbk".to_string()),
            ..SearchParams::default()
        },
    );
    assert_eq!(paths(&env), ["gbk.txt"]);
}

/// The skip report caps its detail list at 256 and counts the overflow in `unlisted`;
/// a budget that cannot fit the details folds the narrowing hint into the terminal.
#[test]
fn skip_report_cap_256_and_unlisted() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // 300 ambiguous files (single 0xE9 byte each) plus one searchable file.
    for index in 0..300 {
        fs::write(root.join(format!("amb{index:03}.dat")), [0xE9]).unwrap();
    }
    write_at(root, "good.txt", "needle\n", at(1));

    let env = run(root, &params("needle"));
    let report = env.skip_report.as_ref().expect("skips must be reported");
    assert_eq!(report.files, 300);
    assert_eq!(report.unreachable, 0);
    assert_eq!(report.details.len(), 256, "details cap at 256");
    assert_eq!(report.unlisted, 44);
    let note = env.terminal.as_ref().unwrap().note.as_deref().unwrap();
    assert!(
        note.contains("300 files skipped"),
        "tally folded into the terminal: {note}"
    );
    assert!(
        !note.contains("tighten path/glob"),
        "the cap equals the listed count: {note}"
    );
    assert!(
        report.details[0]
            .reason
            .starts_with("ambiguous: windows-1252"),
        "frozen skip reason: {:?}",
        report.details[0]
    );

    // The detail list is structural (off-wire): the budget constrains the rendered page
    // only, so even a tight budget lists to the cap and the note's tally discloses the
    // full listing without the narrowing hint.
    let tight = run_b(root, &params("needle"), 1_500).unwrap();
    let report = tight.skip_report.as_ref().unwrap();
    assert_eq!(
        report.details.len(),
        256,
        "details are never budget-fitted: {}",
        report.details.len()
    );
    assert_eq!(report.unlisted, 44);
    assert!(
        tight.token_usage.returned <= 1_500,
        "the wire page still fits the budget: {}",
        tight.token_usage.returned
    );
    let note = tight.terminal.as_ref().unwrap().note.as_deref().unwrap();
    assert!(
        note.contains("300 files skipped"),
        "tally folded into the terminal: {note}"
    );
    assert!(
        !note.contains("tighten path/glob"),
        "a full listing needs no narrowing hint: {note}"
    );
}

/// Glob filters: `!` exclusions always veto; negative-only lists include every other
/// file; inclusions match the root-relative path with literal separators.
#[test]
fn glob_exclusions_always_win() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_at(root, "a.txt", "needle\n", at(3));
    write_at(root, "skip_me.txt", "needle\n", at(2));
    write_at(root, "b.rs", "needle\n", at(1));

    let both = run(
        root,
        &SearchParams {
            pattern: "needle".to_string(),
            glob: GlobArgs(vec!["*.txt".to_string(), "!skip_me.txt".to_string()]),
            ..SearchParams::default()
        },
    );
    assert_eq!(paths(&both), ["a.txt"], "exclusion vetoes the inclusion");

    let negative_only = run(
        root,
        &SearchParams {
            pattern: "needle".to_string(),
            glob: GlobArgs(vec!["!*.txt".to_string()]),
            ..SearchParams::default()
        },
    );
    assert_eq!(paths(&negative_only), ["b.rs"]);

    // literal_separator: `*` does not cross `/`, `**` does.
    write_at(root, "nested/deep.txt", "needle\n", at(0));
    let shallow = run(
        root,
        &SearchParams {
            pattern: "needle".to_string(),
            glob: GlobArgs(vec!["*.txt".to_string(), "!skip_me.txt".to_string()]),
            ..SearchParams::default()
        },
    );
    assert_eq!(paths(&shallow), ["a.txt"], "`*` does not cross `/`");
    let recursive = run(
        root,
        &SearchParams {
            pattern: "needle".to_string(),
            glob: GlobArgs(vec!["**/*.txt".to_string(), "!skip_me.txt".to_string()]),
            ..SearchParams::default()
        },
    );
    assert_eq!(paths(&recursive), ["a.txt", "nested/deep.txt"]);
}

/// `next_call` reproduces the original call arguments with only `offset` replaced
/// the machine continuation contract).
#[test]
fn next_call_matches_original_args() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_at(root, "a.txt", "NEEDLE\nNEEDLE\nNEEDLE\n", at(2));
    write_at(root, "b.txt", "NEEDLE\nNEEDLE\nNEEDLE\n", at(1));

    let original = serde_json::json!({
        "pattern": "NEEDLE",
        "glob": ["*.txt"],
        "output_mode": "content",
        "before_context": 1,
        "head_limit": 2,
    });
    let parsed: SearchParams = serde_json::from_value(original.clone()).unwrap();
    let env = run(root, &parsed);
    assert!(env.truncated, "head_limit 2 of 6 entries must be partial");
    let next = env.next_call.as_ref().expect("partial carries next_call");
    assert_eq!(next["tool"], "search");
    let mut expected = original.clone();
    expected["offset"] = serde_json::json!(2);
    assert_eq!(
        next["arguments"], expected,
        "continuation args = original args with offset replaced"
    );

    // The continuation arguments deserialize back into the same request shape.
    let round: SearchParams =
        serde_json::from_value(next["arguments"].clone()).expect("wire round-trip");
    assert_eq!(round.pattern, "NEEDLE");
    assert_eq!(round.output_mode, SearchOutput::Content);
    assert_eq!(round.before_context, 1);
    assert_eq!(round.head_limit, Some(2));
    assert_eq!(round.offset, 2);
    assert!(round.line_numbers, "wire default applies");
}

/// When even the smallest body plus the mandatory envelope cannot fit, the tool fails
/// with the frozen budget message — never a bodyless success.
#[test]
fn budget_too_small_error_text() {
    let root = fixture_root("count");
    let err = run_b(&root, &params("needle"), 1).unwrap_err();
    assert_eq!(
        err.to_string(),
        "configuration error: MINDCTX_SEARCH_TOKEN_BUDGET=1 is too small to return the required grep continuation note. Increase it and retry."
    );

    // A note-only (zero-result) response obeys the same floor rule.
    let err = run_b(&root, &params("zzz_no_match"), 1).unwrap_err();
    assert!(
        err.to_string().ends_with(
            "is too small to return the required grep continuation note. Increase it and retry."
        ),
        "got: {err}"
    );
}

/// Invalid regex → the frozen message with the syntax hint.
#[test]
fn invalid_pattern_error_text() {
    let root = fixture_root("count");
    let err = run_b(&root, &params("(["), DEFAULT_BUDGET).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("Invalid regex pattern: "), "got: {text}");
    assert!(
        text.contains(
            "Note: patterns follow Rust regex syntax — lookaround and backreferences are unsupported; write literal braces escaped."
        ),
        "syntax hint missing: {text}"
    );
}

/// fallback_encoding rescues files auto-detection rejected (never a BOM mismatch) and
/// reports the rescue in the frozen note.
#[test]
fn fallback_encoding_rescues_ambiguous() {
    let root = fixture_root("encoding");
    let env = run(
        &root,
        &SearchParams {
            pattern: "needle".to_string(),
            glob: GlobArgs(vec!["gbk.txt".to_string()]),
            fallback_encoding: Some("gbk".to_string()),
            ..SearchParams::default()
        },
    );
    assert_eq!(
        paths(&env),
        ["gbk.txt"],
        "fallback rescue made it searchable"
    );
    let text = env.text.as_deref().unwrap_or_default();
    assert!(
        text.contains("(Note: 1 file decoded with fallback encoding gbk.)"),
        "fallback note: {text}"
    );
}

/// A walk that yields zero entries because an unreachable subtree was the only source
/// still returns a zero-result page carrying the skip report — "nothing searched" is a
/// coverage fact, not a hard failure. Only an unreachable SEARCH ROOT itself fails.
#[test]
#[cfg(unix)]
fn zero_candidate_walk_with_skips_reports_skips() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let locked = root.join("locked");
    fs::create_dir_all(locked.join("inner")).unwrap();
    fs::write(locked.join("inner/deep.txt"), "needle\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let outcome = run_b(root, &params("needle"), DEFAULT_BUDGET);
    let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));
    let Ok(env) = outcome else {
        // Mode bits are advisory in some environments; nothing to assert there.
        return;
    };
    assert!(env.text.is_none(), "no candidate survived: {env:?}");
    let report = env.skip_report.as_ref().expect("skips must be reported");
    assert_eq!(report.unreachable, 1, "the locked subtree: {report:?}");
    let note = env.terminal.as_ref().unwrap().note.as_deref().unwrap();
    assert!(
        note.contains("unreachable"),
        "the skip tally folds into the terminal note: {note}"
    );
    assert!(!env.truncated);
}

/// The search root itself is the one unreachable case that hard-fails (frozen config
/// error), regardless of what a walk could have reported.
#[test]
fn unreachable_search_root_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let err = run_b(
        tmp.path(),
        &SearchParams {
            pattern: "needle".to_string(),
            path: Some("missing_dir".to_string()),
            ..SearchParams::default()
        },
        DEFAULT_BUDGET,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("search target does not exist"),
        "root unreachable is a hard failure: {err}"
    );
}
