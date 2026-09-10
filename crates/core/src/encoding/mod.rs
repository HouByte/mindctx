// SPDX-License-Identifier: MIT OR Apache-2.0

//! Encoding decision ladder: BOM/NUL/UTF-8/legacy detection with strict decoding.
//! Reads come out as canonical UTF-8 or a precise rejection; replacement characters are NEVER emitted.
//! Consumers code against [`EncodingOutcome`]; [`decide`] is the ladder. Decoded text is a
//! `Cow`: the valid-UTF-8 fast path borrows the input bytes, only legacy/BOM decodes own.

use std::borrow::Cow;

use encoding_rs::Encoding;

/// Legacy codecs detection may ever answer with (fixed set; order is contract).
pub const FIXED_LEGACY_ENCODINGS: [&str; 5] =
    ["windows-1252", "gbk", "shift_jis", "big5", "euc-kr"];

/// Minimum raw non-ASCII bytes a chardetng nomination must show before it may be accepted.
const MIN_NOMINATION_EVIDENCE_BYTES: usize = 32;

/// The NUL sentinel is scanned only in this many leading bytes.
const NUL_SCAN_BYTES: usize = 8 * 1024;

/// Segment size for the segment-consistency hard check.
const SEGMENT_BYTES: usize = 4 * 1024;

/// A valid UTF-8 prefix must reach this many bytes before its truncation counts as a
/// mixed/inconsistent conflict rather than ordinary non-UTF-8 bytes.
const UTF8_CONFLICT_MIN_BYTES: usize = MIN_NOMINATION_EVIDENCE_BYTES;
/// ...and must carry at least this many non-ASCII bytes.
const UTF8_CONFLICT_MIN_NON_ASCII: usize = 8;

/// Files at or above this size skip the LEGACY-FALLBACK path (chardetng whole-file
/// nomination + the five legacy codecs under strict decode, byte-exact re-encode, and
/// 4 KiB segment checks): garbage binary at this scale costs CPU for a verdict that is
/// `Undecodable` in practice. The cheap O(n) passes above it (BOM, NUL sniff, strict
/// UTF-8, binary magic, UTF-8-conflict scan) are NOT gated — a large valid-UTF-8 file
/// or a large NUL/magic binary keeps its verdict.
const LEGACY_DETECTION_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Verdict of the automatic decision ladder for one sealed byte stream. Borrowed from
/// the input bytes on the UTF-8 fast path (`'a` = the bytes' lifetime).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision<'a> {
    /// Decoded canonical UTF-8 text. `notes` carries advisory messages; `fallback` names the
    /// legacy codec when detection (not a BOM, not plain UTF-8) produced the text.
    Text {
        decoded: Cow<'a, str>,
        notes: Vec<String>,
        fallback: Option<&'static str>,
    },
    /// Binary content: never decoded; the caller drops or reports it.
    Binary { kind: &'static str },
    /// Not usable as text; `report` carries the precise machine-checkable reason.
    Rejected { report: Rejection },
}

/// Why a byte stream was rejected, with the frozen user-facing vocabulary (`skip_reason`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// Several legacy codecs decode the bytes cleanly and the evidence is too thin to
    /// pick one (never guesses). `clean_decodes` follows
    /// [`FIXED_LEGACY_ENCODINGS`] order.
    Ambiguous { clean_decodes: Vec<&'static str> },
    /// The bytes start as a substantial valid UTF-8 run and then break — mixed/inconsistent
    /// encodings; `hex_offset` is the byte offset of the first conflicting byte.
    MixedOrInconsistent { hex_offset: usize },
    /// The bytes are valid UTF-8 but carry ISO-2022-JP designation escape sequences, so they
    /// are equally readable as ISO-2022-JP — ambiguous, not confidently UTF-8.
    Iso2022JpSignature,
    /// No codec in the automatic set decodes the bytes cleanly.
    Undecodable,
    /// A BOM in the bytes contradicts the requested (explicit) encoding, or a BOM's own
    /// encoding fails to decode the rest.
    BomMismatch,
    /// The explicit `encoding` label is not known to the ladder.
    InvalidLabel { label: String },
}

impl Rejection {
    /// Frozen skip-reason vocabulary; consumed by `EncodingOutcome::Skipped`.
    pub fn skip_reason(&self) -> String {
        match self {
            Rejection::Ambiguous { clean_decodes } => {
                format!("ambiguous: {}", clean_decodes.join(", "))
            }
            Rejection::MixedOrInconsistent { .. } => "mixed or inconsistent encodings".to_string(),
            Rejection::Iso2022JpSignature => "ambiguous: iso-2022-jp".to_string(),
            Rejection::Undecodable => "undecodable".to_string(),
            Rejection::BomMismatch => "BOM mismatch".to_string(),
            Rejection::InvalidLabel { label } => format!("invalid encoding label: {label}"),
        }
    }
}

/// Ladder:
/// 1. BOM (UTF-32BE 00 00 FE FF, UTF-32LE FF FE 00 00, UTF-8 EF BB BF, UTF-16LE FF FE,
///    UTF-16BE FE FF) wins over everything incl. NUL binary detection for UTF-8 BOM.
/// 2. NUL in first 8 KiB → Binary{kind:"binary"}.
/// 3. Strict UTF-8: valid + ISO-2022 ESC sequences present → Iso2022JpSignature;
///    invalid + binary magic → Binary (magic: ZIP PK\x03\x04, gzip \x1f\x8b, 7z, ELF \x7fELF,
///    MZ, Mach-O fe ed fa ce/ce fa ed fe/feedface/cffaedfe, SQLite "SQLite format 3\0", wasm).
/// 4. chardetng nomination (whole file, Iso2022JpDetection::Allow, Utf8Detection::Deny) +
///    parallel UTF-8-conflict scanner (≥32-byte valid run with ≥8 non-ASCII bytes → hex offset).
/// 5. Nomination validated cleanly with ≥32 non-ASCII bytes → accept (multi-byte immediately;
///    single-byte also needs byte-exact round-trip + 4 KiB segment consistency).
/// 6. Else: all 5 fixed legacy encodings under hard checks (no control chars/noncharacters;
///    byte-exact round-trip; 4 KiB segment consistency). Clean decodes → Ambiguous{clean_decodes};
///    none → Undecodable.
///
/// Strict decode: any malformed byte is a hard failure; replacement chars NEVER emitted.
pub fn decide(bytes: &[u8]) -> Decision<'_> {
    // Step 1: a BOM outranks every other signal, including the NUL sentinel (UTF-16/UTF-32
    // text legitimately contains NULs, and a UTF-8-BOM file stays text even if polluted).
    if let Some((codec, bom_len)) = matching_bom(bytes) {
        return match codec.decode_strict(&bytes[bom_len..]) {
            Some(decoded) => Decision::Text {
                decoded,
                notes: Vec::new(),
                fallback: None,
            },
            // The BOM's claim did not hold for the remaining bytes.
            None => Decision::Rejected {
                report: Rejection::BomMismatch,
            },
        };
    }

    // Step 2: NUL inside the sniff window is the binary sentinel.
    let head_len = bytes.len().min(NUL_SCAN_BYTES);
    if bytes[..head_len].contains(&0) {
        return Decision::Binary { kind: "binary" };
    }

    // Step 3: strict UTF-8.
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            if has_iso_2022_signature(bytes) {
                return Decision::Rejected {
                    report: Rejection::Iso2022JpSignature,
                };
            }
            return Decision::Text {
                decoded: Cow::Borrowed(text),
                notes: Vec::new(),
                fallback: None,
            };
        }
        Err(err) => {
            // Step 3 (invalid branch): file-signature magics win before anything else — a ZIP
            // or ELF with a coincidental valid UTF-8 run is still binary.
            if has_binary_magic(bytes) {
                return Decision::Binary { kind: "binary" };
            }
            // Step 4 (parallel scanner): a substantial valid UTF-8 run (>= 32 bytes with
            // >= 8 non-ASCII bytes) followed by invalid bytes is a mixed/inconsistent file;
            // never paper over it with a legacy codec.
            let valid = err.valid_up_to();
            if valid >= UTF8_CONFLICT_MIN_BYTES
                && count_non_ascii(&bytes[..valid]) >= UTF8_CONFLICT_MIN_NON_ASCII
            {
                return Decision::Rejected {
                    report: Rejection::MixedOrInconsistent { hex_offset: valid },
                };
            }
        }
    }

    // Size gate on the LEGACY-FALLBACK path only: at this size the nomination + codec
    // trial (steps 5/6) cost whole-file detection plus 5+ full strict decodes, a
    // byte-exact re-encode each, and 4 KiB segment-consistency scans. The bytes reaching
    // this point are not valid UTF-8 and carry no binary magic, so the frozen verdict is
    // Undecodable — the ladder never guesses, and it must not spend seconds guessing.
    if bytes.len() >= LEGACY_DETECTION_MAX_BYTES {
        return Decision::Rejected {
            report: Rejection::Undecodable,
        };
    }

    // Step 4/5: chardetng nomination over the whole file, restricted to the fixed legacy set
    // (automatic detection is limited to that set) and gated on evidence.
    if count_non_ascii(bytes) >= MIN_NOMINATION_EVIDENCE_BYTES {
        if let Some(enc) = nomination(bytes) {
            if let Some(decoded) = validate_legacy(enc, bytes, true) {
                return Decision::Text {
                    decoded: Cow::Owned(decoded),
                    notes: Vec::new(),
                    fallback: Some(legacy_label(enc)),
                };
            }
        }
    }

    // Step 6: every fixed legacy codec under the full hard checks; every clean decode is a
    // candidate. With none, the bytes are undecodable — no guessing.
    let mut clean_decodes = Vec::new();
    for label in FIXED_LEGACY_ENCODINGS {
        let Some(enc) = Encoding::for_label(label.as_bytes()) else {
            continue;
        };
        if validate_legacy(enc, bytes, false).is_some() {
            clean_decodes.push(label);
        }
    }
    if clean_decodes.is_empty() {
        Decision::Rejected {
            report: Rejection::Undecodable,
        }
    } else {
        Decision::Rejected {
            report: Rejection::Ambiguous { clean_decodes },
        }
    }
}

/// Explicit-label path: decode with the encoding the caller named.
///
/// Canonical label lookup covers everything `encoding_rs` knows plus UTF-32LE/BE (which
/// `encoding_rs` deliberately lacks). A BOM of the requested encoding is still skipped; a BOM
/// of a different encoding contradicts the request → [`Rejection::BomMismatch`]. Decoding raw
/// bytes that are also valid UTF-8 (with a non-UTF-8-family codec) appends the frozen warning
/// note so garbled reads stay recoverable.
pub fn decode_explicit<'a>(bytes: &'a [u8], label: &str) -> Result<Decision<'a>, Rejection> {
    let codec = resolve_label(label).ok_or_else(|| Rejection::InvalidLabel {
        label: label.to_string(),
    })?;
    let mut start = 0;
    if let Some((bom_codec, bom_len)) = matching_bom(bytes) {
        if bom_codec.name() != codec.name() {
            return Err(Rejection::BomMismatch);
        }
        start = bom_len;
    }
    let decoded = codec
        .decode_strict(&bytes[start..])
        .ok_or(Rejection::Undecodable)?;
    let mut notes = Vec::new();
    if !codec.is_utf8_family() && std::str::from_utf8(&bytes[start..]).is_ok() {
        notes.push(format!(
            "(Note: decoded from {} as requested; output is UTF-8. Warning: those raw bytes \
             are valid UTF-8 as well — when the text looks garbled, retry with \
             encoding=\"utf-8\" or leave encoding unset.)",
            codec.name()
        ));
    }
    Ok(Decision::Text {
        decoded,
        notes,
        fallback: None,
    })
}

/// User-facing ambiguity message (frozen). The full ambiguity spec includes
/// `or use view="hex"` — mindctx has no hex view, so the tail is
/// dropped, not replaced.
pub fn ambiguous_message(path: &str, candidates: &[&str]) -> String {
    format!(
        "Cannot confidently determine the text encoding of {path}: the bytes decode cleanly \
         as {}. Retry with encoding=\"...\" when the context tells you which codec it is.",
        candidates.join(", ")
    )
}

/// A concrete text codec the ladder can decode with. `encoding_rs` has no UTF-32, so the two
/// UTF-32 variants are hand-rolled here (explicit labels).
#[derive(Copy, Clone, PartialEq, Eq)]
enum TextCodec {
    Rs(&'static Encoding),
    Utf32Le,
    Utf32Be,
}

impl TextCodec {
    /// Canonical name (`encoding_rs` names; UTF-32 named after its byte order).
    fn name(&self) -> &'static str {
        match self {
            TextCodec::Rs(enc) => enc.name(),
            TextCodec::Utf32Le => "UTF-32LE",
            TextCodec::Utf32Be => "UTF-32BE",
        }
    }

    fn is_utf8_family(&self) -> bool {
        matches!(self, TextCodec::Rs(enc) if std::ptr::eq(*enc, encoding_rs::UTF_8))
    }

    /// Strict decode: `None` when any byte is malformed — replacement characters are never
    /// emitted (BOM handling stays out; callers strip BOMs explicitly). The UTF-8 codec
    /// borrows the input; every other codec owns its output.
    fn decode_strict<'a>(&self, bytes: &'a [u8]) -> Option<Cow<'a, str>> {
        match self {
            TextCodec::Rs(enc) => enc.decode_without_bom_handling_and_without_replacement(bytes),
            TextCodec::Utf32Le => try_decode_utf32(bytes, true).map(Cow::Owned),
            TextCodec::Utf32Be => try_decode_utf32(bytes, false).map(Cow::Owned),
        }
    }
}

/// BOM table in precedence order: the UTF-32 variants must be tested before the UTF-16 variants
/// whose BOMs they extend (FF FE 00 00 before FF FE, 00 00 FE FF before FE FF).
fn matching_bom(bytes: &[u8]) -> Option<(TextCodec, usize)> {
    const UTF32BE: &[u8] = &[0x00, 0x00, 0xFE, 0xFF];
    const UTF32LE: &[u8] = &[0xFF, 0xFE, 0x00, 0x00];
    const UTF8: &[u8] = &[0xEF, 0xBB, 0xBF];
    const UTF16LE: &[u8] = &[0xFF, 0xFE];
    const UTF16BE: &[u8] = &[0xFE, 0xFF];
    for (bom, codec) in [
        (UTF32BE, TextCodec::Utf32Be),
        (UTF32LE, TextCodec::Utf32Le),
        (UTF8, TextCodec::Rs(encoding_rs::UTF_8)),
        (UTF16LE, TextCodec::Rs(encoding_rs::UTF_16LE)),
        (UTF16BE, TextCodec::Rs(encoding_rs::UTF_16BE)),
    ] {
        if bytes.starts_with(bom) {
            return Some((codec, bom.len()));
        }
    }
    None
}

/// Hand-rolled strict UTF-32 (explicit labels): any non-multiple-of-4 length,
/// surrogate, or out-of-range scalar is a hard failure.
fn try_decode_utf32(bytes: &[u8], little_endian: bool) -> Option<String> {
    if bytes.len() % 4 != 0 {
        return None;
    }
    let units = bytes.chunks_exact(4).map(|chunk| {
        let word: [u8; 4] = chunk.try_into().expect("chunks_exact yields 4-byte groups");
        if little_endian {
            u32::from_le_bytes(word)
        } else {
            u32::from_be_bytes(word)
        }
    });
    units.map(char::from_u32).collect()
}

/// Canonical label lookup: `encoding_rs::Encoding::for_label` plus the UTF-32 labels it lacks.
fn resolve_label(label: &str) -> Option<TextCodec> {
    match label.to_ascii_lowercase().as_str() {
        "utf-32le" => return Some(TextCodec::Utf32Le),
        "utf-32be" => return Some(TextCodec::Utf32Be),
        _ => {}
    }
    Encoding::for_label(label.as_bytes()).map(TextCodec::Rs)
}

/// chardetng nomination (step 4): the detector is fed the whole file once with ISO-2022-JP
/// candidacy allowed and UTF-8 denied, then asked for its guess. chardetng 1.x's `guess` always
/// returns an encoding (`Utf8Detection::Deny` merely excludes UTF-8 from the answer), so the
/// ladder's real gate is the evidence + hard-check validation, and only the fixed legacy set
/// may be accepted: anything else the detector names (KOI8-R, UTF-16, ISO-8859-*, ...) is
/// discarded and the ladder falls through to the legacy trial, because automatic detection is
/// limited to the fixed set.
fn nomination(bytes: &[u8]) -> Option<&'static Encoding> {
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    detector.feed(bytes, true);
    let guessed = detector.guess(None, chardetng::Utf8Detection::Deny);
    FIXED_LEGACY_ENCODINGS.iter().find_map(|label| {
        let enc = Encoding::for_label(label.as_bytes())?;
        (enc.name() == guessed.name()).then_some(enc)
    })
}

/// Legacy validation (steps 5/6): strict clean decode, no control characters or
/// noncharacters, and — unless `relax_multi_byte` (the step-5 nomination path, which accepts a
/// cleanly-decoding multi-byte codec immediately) — the byte-exact round-trip and 4 KiB
/// segment-consistency hard checks. `None` disqualifies the codec.
fn validate_legacy(enc: &'static Encoding, bytes: &[u8], relax_multi_byte: bool) -> Option<String> {
    let decoded = enc
        .decode_without_bom_handling_and_without_replacement(bytes)?
        .into_owned();
    if decoded.chars().any(is_disallowed_legacy_character) {
        return None;
    }
    if relax_multi_byte && !enc.is_single_byte() {
        return Some(decoded);
    }
    // Byte-exact round-trip: encode(decoded) must reproduce the raw bytes exactly.
    if enc.encode(decoded.as_str()).0 != bytes {
        return None;
    }
    // 4 KiB segment consistency: every segment must also decode cleanly on its own.
    if !bytes.chunks(SEGMENT_BYTES).all(|segment| {
        enc.decode_without_bom_handling_and_without_replacement(segment)
            .is_some()
    }) {
        return None;
    }
    Some(decoded)
}

/// The `FIXED_LEGACY_ENCODINGS` label for a codec known to be in the fixed set; falls back to
/// the canonical `encoding_rs` name should that ever diverge.
fn legacy_label(enc: &'static Encoding) -> &'static str {
    FIXED_LEGACY_ENCODINGS
        .into_iter()
        .find(|label| {
            Encoding::for_label(label.as_bytes())
                .is_some_and(|candidate| candidate.name() == enc.name())
        })
        .unwrap_or(enc.name())
}

/// Legacy hard check: no control characters or Unicode noncharacters. Binary
/// garbage decodes "cleanly" under single-byte legacy codecs far too often for byte-validity
/// alone to filter it — C1 controls and noncharacters are its fingerprints. Only the benign
/// text whitespace set (tab, LF, CR) is allowed among the controls.
fn is_disallowed_legacy_character(c: char) -> bool {
    match c {
        // The benign text-whitespace set stays allowed among the controls.
        '\t' | '\n' | '\r' => false,
        // C0 controls, DEL, and the C1 block are binary garbage fingerprints.
        c if c.is_control() => true,
        // Unicode noncharacters: the U+FDD0..U+FDEF block and every U+xFFFE/U+xFFFF pair.
        c if ('\u{FDD0}'..='\u{FDEF}').contains(&c) => true,
        c => (c as u32 & 0xFFFE) == 0xFFFE,
    }
}

fn count_non_ascii(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b >= 0x80).count()
}

/// ISO-2022 designation sequences (step 3): ESC followed by intermediate bytes (0x20-0x2F)
/// and a final byte (0x40-0x7E) — e.g. ESC $ B, ESC ( B, ESC & @ ESC $ B. A file that is valid
/// UTF-8 but carries these is equally readable as ISO-2022-JP, hence the rejection.
fn has_iso_2022_signature(bytes: &[u8]) -> bool {
    let mut scan_from = 0;
    while let Some(pos) = bytes[scan_from..].iter().position(|&b| b == 0x1B) {
        let mut cursor = scan_from + pos + 1;
        if cursor < bytes.len() && (0x20..=0x2F).contains(&bytes[cursor]) {
            while cursor < bytes.len() && (0x20..=0x2F).contains(&bytes[cursor]) {
                cursor += 1;
            }
            if cursor < bytes.len() && (0x40..=0x7E).contains(&bytes[cursor]) {
                return true;
            }
        }
        scan_from = cursor;
    }
    false
}

/// File-signature magics (step 3, invalid-UTF-8 branch), matched at offset 0 — every entry is
/// a file-header signature, so only a prefix match is meaningful. Also used by read (magic
/// wins before the ladder there, because some signatures (ZIP headers) are
/// themselves valid UTF-8 and would otherwise decode as text.
pub(crate) fn has_binary_magic(bytes: &[u8]) -> bool {
    const MAGICS: [&[u8]; 11] = [
        b"PK\x03\x04",          // ZIP
        b"\x1f\x8b",            // gzip
        b"7z\xbc\xaf\x27\x1c",  // 7z
        b"\x7fELF",             // ELF
        b"MZ",                  // DOS MZ / PE
        b"\xfe\xed\xfa\xce",    // Mach-O 32-bit big-endian
        b"\xfe\xed\xfa\xcf",    // Mach-O 64-bit big-endian
        b"\xce\xfa\xed\xfe",    // Mach-O 32-bit little-endian
        b"\xcf\xfa\xed\xfe",    // Mach-O 64-bit little-endian
        b"SQLite format 3\x00", // SQLite
        b"\x00asm",             // WebAssembly
    ];
    MAGICS.iter().any(|magic| bytes.starts_with(magic))
}

/// Result of validating a sealed file's bytes: what downstream tools may do with
/// them. Borrowed from the sealed bytes on the UTF-8 fast path (`'a` = the bytes' lifetime).
#[derive(Debug, Clone, PartialEq)]
pub enum EncodingOutcome<'a> {
    /// Decoded canonical UTF-8 text. `notes` records normalization decisions; `fallback_used`
    /// names the legacy codec when detection had to fall back.
    Text {
        decoded: Cow<'a, str>,
        notes: Vec<String>,
        fallback_used: Option<String>,
    },
    /// Binary content: never decoded; the caller drops or reports it.
    Binary,
    /// Unusable for search, with a frozen reason string.
    Skipped { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use Rejection::{Ambiguous, InvalidLabel};

    #[test]
    fn skip_reason_vocabulary_is_frozen() {
        assert_eq!(
            Ambiguous {
                clean_decodes: vec!["gbk", "windows-1252"],
            }
            .skip_reason(),
            "ambiguous: gbk, windows-1252"
        );
        assert_eq!(
            Rejection::MixedOrInconsistent { hex_offset: 0x1f }.skip_reason(),
            "mixed or inconsistent encodings"
        );
        assert_eq!(
            Rejection::Iso2022JpSignature.skip_reason(),
            "ambiguous: iso-2022-jp"
        );
        assert_eq!(Rejection::Undecodable.skip_reason(), "undecodable");
        assert_eq!(Rejection::BomMismatch.skip_reason(), "BOM mismatch");
        assert_eq!(
            InvalidLabel {
                label: "latin9".to_string(),
            }
            .skip_reason(),
            "invalid encoding label: latin9"
        );
    }

    #[test]
    fn empty_input_is_empty_text() {
        assert_eq!(
            decide(b""),
            Decision::Text {
                decoded: String::new().into(),
                notes: Vec::new(),
                fallback: None,
            }
        );
    }

    #[test]
    fn ascii_only_is_plain_text() {
        assert_eq!(
            decide(b"hello world\n"),
            Decision::Text {
                decoded: "hello world\n".to_string().into(),
                notes: Vec::new(),
                fallback: None,
            }
        );
    }

    #[test]
    fn nul_in_first_8k_is_binary() {
        let mut bytes = vec![0x61; NUL_SCAN_BYTES - 1];
        bytes.push(0);
        assert_eq!(decide(&bytes), Decision::Binary { kind: "binary" });
    }

    #[test]
    fn nul_beyond_8k_is_not_the_binary_sentinel() {
        // NUL is valid UTF-8, so past the sniff window the bytes stay text.
        let mut bytes = vec![0x61; NUL_SCAN_BYTES];
        bytes.push(0);
        assert_eq!(
            decide(&bytes),
            Decision::Text {
                decoded: String::from_utf8(bytes.clone()).unwrap().into(),
                notes: Vec::new(),
                fallback: None,
            }
        );
    }

    #[test]
    fn utf8_bom_wins_over_nul_sentinel() {
        let mut bytes = b"\xef\xbb\xbfh\xc3\xa9llo".to_vec();
        bytes.push(0);
        assert_eq!(
            decide(&bytes),
            Decision::Text {
                decoded: "héllo\0".to_string().into(),
                notes: Vec::new(),
                fallback: None,
            }
        );
    }

    #[test]
    fn bom_whose_encoding_fails_is_bom_mismatch() {
        // UTF-16LE BOM over an odd number of remaining bytes: the BOM's claim did not hold.
        assert_eq!(
            decide(b"\xff\xfe\x61"),
            Decision::Rejected {
                report: Rejection::BomMismatch
            }
        );
    }

    #[test]
    fn utf32be_bom_takes_precedence_over_utf16be() {
        // 00 00 FE FF is the UTF-32BE BOM; read as UTF-16BE its leading NULs would be binary.
        let bytes = [0x00, 0x00, 0xFE, 0xFF, 0x00, 0x00, 0x00, 0x61];
        assert_eq!(
            decide(&bytes),
            Decision::Text {
                decoded: "a".to_string().into(),
                notes: Vec::new(),
                fallback: None,
            }
        );
    }

    #[test]
    fn iso2022_signature_scans_designations() {
        assert!(has_iso_2022_signature(b"\x1b$B$3$s$K$A$O\x1b(B"));
        assert!(has_iso_2022_signature(b"plain \x1b&@\x1b$Btext\x1b(B"));
        assert!(!has_iso_2022_signature(b"plain text without escapes"));
        // ANSI CSI (ESC [) and a lone ESC are not ISO-2022 designations.
        assert!(!has_iso_2022_signature(b"colored: \x1b[31mred\x1b[0m"));
        assert!(!has_iso_2022_signature(b"truncated \x1b"));
    }

    #[test]
    fn binary_magics_fire_without_nul_in_window() {
        for magic in [
            &b"PK\x03\x04"[..],
            b"\x1f\x8b",
            b"7z\xbc\xaf\x27\x1c",
            b"\x7fELF",
            b"MZ",
            b"\xfe\xed\xfa\xce",
            b"\xfe\xed\xfa\xcf",
            b"\xce\xfa\xed\xfe",
            b"\xcf\xfa\xed\xfe",
            b"SQLite format 3\x00",
        ] {
            let bytes = [magic, b"garbage \xff\xfe\x80"].concat();
            assert_eq!(
                decide(&bytes),
                Decision::Binary { kind: "binary" },
                "magic {magic:?} must classify as binary"
            );
        }
    }

    #[test]
    fn low_evidence_single_candidate_is_still_ambiguous_not_guessed() {
        // 0xE9 decodes as "é" under windows-1252 (evidence 1 < 32) and errors everywhere else.
        assert_eq!(
            decide(&[0xE9]),
            Decision::Rejected {
                report: Ambiguous {
                    clean_decodes: vec!["windows-1252"],
                }
            }
        );
    }

    #[test]
    fn binary_garbage_no_legacy_codec_may_claim() {
        // 0x81 has no windows-1252 mapping and 0x7F is an invalid trail byte everywhere (and a
        // DEL control where a decoder accepts it): garbage no legacy codec may claim cleanly.
        let bytes = [0x81u8, 0x7F].repeat(24);
        assert_eq!(
            decide(&bytes),
            Decision::Rejected {
                report: Rejection::Undecodable
            }
        );
    }

    /// Size gate on the LEGACY-FALLBACK path: garbage binary at/above the threshold is
    /// rejected as Undecodable without running the nomination + legacy-codec trial, and
    /// a large valid-UTF-8 file still decodes as text (the cheap O(n) passes are ungated).
    #[test]
    fn large_garbage_hits_the_legacy_size_gate_but_large_utf8_still_decodes() {
        // Exactly at the threshold: the legacy trial would classify these bytes (small
        // garbage of the same shape does reach it), but the gate short-circuits.
        let garbage: Vec<u8> = [0x81u8, 0x7F].repeat(LEGACY_DETECTION_MAX_BYTES / 2);
        assert_eq!(garbage.len(), LEGACY_DETECTION_MAX_BYTES);
        assert_eq!(
            decide(&garbage),
            Decision::Rejected {
                report: Rejection::Undecodable
            }
        );

        // The gate must not touch the cheap passes: a large valid-UTF-8 file keeps its
        // Text verdict even though it is above the threshold.
        let text = format!("héllo world\n{}", "a".repeat(LEGACY_DETECTION_MAX_BYTES));
        assert!(text.len() > LEGACY_DETECTION_MAX_BYTES);
        assert_eq!(
            decide(text.as_bytes()),
            Decision::Text {
                decoded: Cow::Borrowed(text.as_str()),
                notes: Vec::new(),
                fallback: None,
            }
        );
    }

    #[test]
    fn explicit_utf16le_label_round_trips_without_bom() {
        let bytes: Vec<u8> = "你好，世界\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let (decoded, notes, fallback) = match decode_explicit(&bytes, "utf-16le").unwrap() {
            Decision::Text {
                decoded,
                notes,
                fallback,
            } => (decoded, notes, fallback),
            other => panic!("expected Text, got {other:?}"),
        };
        assert_eq!(decoded, "你好，世界\n");
        // The raw bytes contain NULs, so they are not valid UTF-8: no warning note.
        assert!(notes.is_empty());
        assert_eq!(fallback, None);
    }

    #[test]
    fn explicit_label_on_malformed_bytes_is_undecodable() {
        // A lone high surrogate with label utf-16be: strict decode cannot succeed — no
        // replacement character is ever emitted.
        assert_eq!(
            decode_explicit(&[0xD8, 0x00], "utf-16be").unwrap_err(),
            Rejection::Undecodable
        );
    }
}
