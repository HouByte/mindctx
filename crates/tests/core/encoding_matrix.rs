// SPDX-License-Identifier: MIT OR Apache-2.0

//! Encoding matrix: every fixture pins the exact `Decision` the
//! ladder must produce, plus the explicit-label path and the frozen ambiguity message.
//!
//! Fixtures live at `crates/tests/fixtures/encoding/` (same convention as the polyglot corpus).

use std::borrow::Cow;
use std::fs;
use std::path::PathBuf;

use mindctx_core::encoding::{Decision, Rejection, ambiguous_message, decide, decode_explicit};

fn fixture(name: &str) -> Vec<u8> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/encoding");
    fs::read(root.join(name)).unwrap_or_else(|e| panic!("fixture {name} must exist: {e}"))
}

/// Assert Text and hand back its payload for content/fallback checks. The bytes binding
/// stays in the caller (the ladder may borrow the input on the UTF-8 fast path).
fn expect_text(decision: Decision<'_>) -> (Cow<'_, str>, Vec<String>, Option<&'static str>) {
    match decision {
        Decision::Text {
            decoded,
            notes,
            fallback,
        } => (decoded, notes, fallback),
        other => panic!("expected Text, got {other:?}"),
    }
}

fn expect_rejected(decision: Decision<'_>) -> Rejection {
    match decision {
        Decision::Rejected { report } => report,
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[test]
fn plain_utf8_is_text_without_notes_or_fallback() {
    let bytes = fixture("utf8.txt");
    let (decoded, notes, fallback) = expect_text(decide(&bytes));
    assert_eq!(decoded, "你好，世界\n");
    assert!(notes.is_empty());
    assert_eq!(fallback, None);
}

#[test]
fn utf8_bom_is_stripped_and_wins_over_everything() {
    let bytes = fixture("utf8-bom.txt");
    let (decoded, notes, fallback) = expect_text(decide(&bytes));
    assert_eq!(decoded, "你好，世界\n");
    assert!(notes.is_empty());
    assert_eq!(fallback, None);
}

#[test]
fn utf16le_bom_decodes_as_utf16le_text() {
    let bytes = fixture("utf16le-bom.txt");
    let (decoded, notes, fallback) = expect_text(decide(&bytes));
    assert_eq!(decoded, "你好，世界\n");
    assert!(notes.is_empty());
    assert_eq!(fallback, None);
}

#[test]
fn utf16be_bom_decodes_as_utf16be_text() {
    let bytes = fixture("utf16be-bom.txt");
    let (decoded, notes, fallback) = expect_text(decide(&bytes));
    assert_eq!(decoded, "你好，世界\n");
    assert!(notes.is_empty());
    assert_eq!(fallback, None);
}

#[test]
fn utf32le_bom_decodes_as_utf32le_text() {
    let bytes = fixture("utf32le-bom.txt");
    let (decoded, notes, fallback) = expect_text(decide(&bytes));
    assert_eq!(decoded, "你好，世界\n");
    assert!(notes.is_empty());
    assert_eq!(fallback, None);
}

#[test]
fn gbk_fixture_is_accepted_with_gbk_fallback() {
    let bytes = fixture("gbk.txt");
    let (decoded, notes, fallback) = expect_text(decide(&bytes));
    assert_eq!(decoded, "你好，世界。".repeat(8) + "\n");
    assert!(notes.is_empty());
    assert_eq!(fallback, Some("gbk"));
}

#[test]
fn shift_jis_fixture_is_accepted_with_shift_jis_fallback() {
    let bytes = fixture("shift_jis.txt");
    let (decoded, notes, fallback) = expect_text(decide(&bytes));
    assert_eq!(decoded, "こんにちは世界。".repeat(8) + "\n");
    assert!(notes.is_empty());
    assert_eq!(fallback, Some("shift_jis"));
}

#[test]
fn big5_fixture_is_accepted_with_big5_fallback() {
    let bytes = fixture("big5.txt");
    let (decoded, notes, fallback) = expect_text(decide(&bytes));
    assert_eq!(decoded, "你好世界。".repeat(12) + "\n");
    assert!(notes.is_empty());
    assert_eq!(fallback, Some("big5"));
}

#[test]
fn euc_kr_fixture_is_accepted_with_euc_kr_fallback() {
    let bytes = fixture("euc-kr.txt");
    let (decoded, notes, fallback) = expect_text(decide(&bytes));
    assert_eq!(decoded, "안녕하세요세계.".repeat(8) + "\n");
    assert!(notes.is_empty());
    assert_eq!(fallback, Some("euc-kr"));
}

#[test]
fn windows1252_fixture_is_accepted_with_windows_1252_fallback() {
    let bytes = fixture("windows1252.txt");
    let (decoded, notes, fallback) = expect_text(decide(&bytes));
    assert_eq!(decoded, "Café naïve résumé über où à propos \n".repeat(6));
    assert!(notes.is_empty());
    assert_eq!(fallback, Some("windows-1252"));
}

#[test]
fn low_evidence_legacy_bytes_are_ambiguous_never_guessed() {
    let bytes = fixture("ambiguous-legacy.bin");
    let report = expect_rejected(decide(&bytes));
    match report {
        Rejection::Ambiguous { ref clean_decodes } => {
            assert_eq!(clean_decodes, &["windows-1252", "gbk", "shift_jis", "big5"]);
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
    assert_eq!(
        report.skip_reason(),
        "ambiguous: windows-1252, gbk, shift_jis, big5"
    );
}

#[test]
fn zip_magic_is_binary_even_without_nul() {
    let bytes = fixture("binary.zip");
    assert_eq!(decide(&bytes), Decision::Binary { kind: "binary" });
}

#[test]
fn iso2022_signature_is_rejected_even_though_bytes_are_valid_utf8() {
    let bytes = fixture("iso2022.txt");
    assert_eq!(
        expect_rejected(decide(&bytes)),
        Rejection::Iso2022JpSignature
    );
    assert_eq!(
        expect_rejected(decide(&bytes)).skip_reason(),
        "ambiguous: iso-2022-jp"
    );
}

#[test]
fn utf8_conflict_is_mixed_or_inconsistent_at_the_conflict_offset() {
    let bytes = fixture("utf8-conflict.bin");
    let report = expect_rejected(decide(&bytes));
    match report {
        Rejection::MixedOrInconsistent { hex_offset } => assert_eq!(hex_offset, 45),
        other => panic!("expected MixedOrInconsistent, got {other:?}"),
    }
    assert_eq!(report.skip_reason(), "mixed or inconsistent encodings");
}

#[test]
fn explicit_label_decodes_strictly_without_note_for_real_legacy_bytes() {
    // Real GBK bytes are not valid UTF-8, so the mojibake warning must stay silent.
    let bytes = fixture("gbk.txt");
    let (decoded, notes, fallback) = expect_text(decode_explicit(&bytes, "gbk").unwrap());
    assert_eq!(decoded, "你好，世界。".repeat(8) + "\n");
    assert!(notes.is_empty());
    assert_eq!(fallback, None);
}

#[test]
fn explicit_label_over_utf8_bytes_warns_about_mojibake() {
    // utf8.txt's raw bytes are valid UTF-8; windows-1252 decodes them "cleanly" — and garbles
    // them. This is the case the frozen warning note exists for.
    let bytes = fixture("utf8.txt");
    let (decoded, notes, fallback) = expect_text(decode_explicit(&bytes, "windows-1252").unwrap());
    assert_eq!(decoded, "ä½\u{a0}å¥½ï¼Œä¸–ç•Œ\n");
    assert_eq!(fallback, None);
    assert_eq!(
        notes,
        [
            "(Note: decoded from windows-1252 as requested; output is UTF-8. Warning: those raw bytes are valid UTF-8 as well — when the text looks garbled, retry with encoding=\"utf-8\" or leave encoding unset.)"
        ]
    );
}

#[test]
fn explicit_utf8_label_skips_matching_bom() {
    let bytes = fixture("utf8-bom.txt");
    let (decoded, notes, fallback) = expect_text(decode_explicit(&bytes, "utf-8").unwrap());
    assert_eq!(decoded, "你好，世界\n");
    assert!(notes.is_empty());
    assert_eq!(fallback, None);
}

#[test]
fn explicit_utf32le_label_decodes_with_matching_bom_skipped() {
    let bytes = fixture("utf32le-bom.txt");
    let (decoded, _, _) = expect_text(decode_explicit(&bytes, "utf-32le").unwrap());
    assert_eq!(decoded, "你好，世界\n");
}

#[test]
fn explicit_label_conflicting_with_bom_is_bom_mismatch() {
    let bytes = fixture("utf8-bom.txt");
    assert_eq!(
        decode_explicit(&bytes, "windows-1252").unwrap_err(),
        Rejection::BomMismatch
    );
}

#[test]
fn unknown_explicit_label_is_invalid_label() {
    let bytes = fixture("utf8.txt");
    assert_eq!(
        decode_explicit(&bytes, "klingon").unwrap_err(),
        Rejection::InvalidLabel {
            label: "klingon".to_string()
        }
    );
}

#[test]
fn ambiguous_message_is_frozen_verbatim() {
    assert_eq!(
        ambiguous_message("src/lib.rs", &["gbk", "windows-1252"]),
        "Cannot confidently determine the text encoding of src/lib.rs: the bytes decode \
         cleanly as gbk, windows-1252. Retry with encoding=\"...\" when the context tells \
         you which codec it is."
    );
}
