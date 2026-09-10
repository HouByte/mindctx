// SPDX-License-Identifier: MIT OR Apache-2.0

//! Token accounting: `tiktoken-rs`, one unified measure across the whole pipeline.
//!
//! The envelope `tokens` field, budget truncation, and pack pricing all defer to this module;
//! separate implementations are forbidden. Counting is **exact** under `o200k_base` (the
//! GPT-4o vocabulary): the same measure everywhere means prefix counts compose, so budget
//! truncation can be verified against a full recount instead of trusted. Constructing the
//! BPE encoder has a cost, so `OnceLock` caches it once per process — a pure cache with no
//! business state, which does not violate the core pure-lib discipline (business state such
//! as index/config is still injected by the caller).

use tiktoken_rs::o200k_base;
static ENCODER: std::sync::OnceLock<tiktoken_rs::CoreBPE> = std::sync::OnceLock::new();
fn encoder() -> &'static tiktoken_rs::CoreBPE {
    ENCODER.get_or_init(|| o200k_base().expect("o200k_base construction is infallible"))
}
/// Count the tokens of a text (same measure as the envelope `tokens` field).
pub fn count_tokens(text: &str) -> u64 {
    encoder().encode_ordinary(text).len() as u64
}

// o200k pretoken split pattern, taken verbatim from tiktoken-rs (`O200K_BASE_PAT_STR`) so
// the counter's segmentation can never drift from the encoder's. INVARIANT the counter
// rests on: BPE never merges across pretoken boundaries of THIS pattern, so token counts
// are additive over pretokens — count(whole) == sum of count(piece) over the split.
// The real o200k pattern is case-aware (capitalized and lowercase word
// runs are distinct pretokens) and attaches contractions as a SUFFIX of the word pretoken
// ("don't" is ONE pretoken) — a splitter that cuts between word and contraction commits
// pieces the encoder merges, breaking additivity. A naive '\n' cut is NOT safe either:
// "\n\n" is a single pretoken AND a single BPE token, so newline-cut commits double-count
// blank lines.
static PRETOKEN: std::sync::OnceLock<fancy_regex::Regex> = std::sync::OnceLock::new();
fn pretoken_re() -> &'static fancy_regex::Regex {
    PRETOKEN.get_or_init(|| {
        fancy_regex::Regex::new(tiktoken_rs::O200K_BASE_PAT_STR)
            .expect("o200k pretoken pattern from tiktoken-rs compiles")
    })
}

#[derive(Clone)]
pub struct ExactPrefixCounter {
    committed: u64,
    open: String, // trailing pretoken(s); only the last piece may merge with future input
}
impl Default for ExactPrefixCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl ExactPrefixCounter {
    pub fn new() -> Self {
        Self {
            committed: 0,
            open: String::new(),
        }
    }
    pub fn push_str(&mut self, s: &str) {
        self.open.push_str(s);
        // Re-split the open tail. Pretoken matching is deterministic left-to-right
        // lookahead only ever looks forward), so re-splitting from the open
        // piece's start reproduces the global segmentation of the full text.
        let pieces: Vec<_> = pretoken_re()
            .find_iter(&self.open)
            .map(|m| m.expect("pretoken match"))
            .collect();
        if pieces.len() > 1 {
            for m in &pieces[..pieces.len() - 1] {
                self.committed += encoder().encode_ordinary(m.as_str()).len() as u64;
            }
            self.open = pieces.last().expect("nonempty").as_str().to_string();
        }
    }
    pub fn checkpoint(&self) -> u64 {
        self.committed + encoder().encode_ordinary(&self.open).len() as u64
    }
    /// Exact token count of the counted text with `tail` appended, without mutating the
    /// counter: the committed pieces are a settled prefix of the text's pretokenization,
    /// and the open tail re-splits with `tail`, so this equals `push_str(tail)` +
    /// `checkpoint` at a fraction of the cost (fitters probe arbitrary page tails).
    pub fn count_with_tail(&self, tail: &str) -> u64 {
        let mut merged = String::with_capacity(self.open.len() + tail.len());
        merged.push_str(&self.open);
        merged.push_str(tail);
        self.committed + encoder().encode_ordinary(&merged).len() as u64
    }
    pub fn finish(self) -> u64 {
        self.committed + encoder().encode_ordinary(&self.open).len() as u64
    }
}
/// Verifies an incremental count against a full recount; mismatch is an internal error
/// [`crate::error::Error::CountMismatch`]).
pub fn verify(incremental: u64, full_text: &str) -> Result<(), crate::error::Error> {
    let full = count_tokens(full_text);
    if incremental == full {
        Ok(())
    } else {
        Err(crate::error::Error::CountMismatch { incremental, full })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o200k_known_vectors() {
        assert_eq!(count_tokens("hello world"), 2);
        assert_eq!(count_tokens("{\"a\":1}"), 5);
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn exact_prefix_counter_matches_full_recount() {
        let lines = ["fn main() {", "    println!(\"hi\");", "}"];
        let mut c = ExactPrefixCounter::new();
        let mut acc = String::new();
        for l in &lines {
            c.push_str(l);
            c.push_str("\n");
            acc.push_str(l);
            acc.push('\n');
            assert_eq!(c.checkpoint(), count_tokens(&acc), "prefix drift at: {l}");
        }
        assert_eq!(c.finish(), count_tokens(&acc));
    }

    /// `count_with_tail` must equal a full recount of counted text + tail at every cut
    /// point — including cuts where the open tail merges with the tail's newlines (the
    /// fitters' page probes lean on this).
    #[test]
    fn count_with_tail_equals_push_then_checkpoint() {
        let lines = [
            "fn main() {",
            "    println!(\"hi\");",
            "}",
            "",
            "  indented  ",
            "plain",
        ];
        for cut in 0..=lines.len() {
            let mut c = ExactPrefixCounter::new();
            let mut acc = String::new();
            for (i, line) in lines[..cut].iter().enumerate() {
                if i > 0 {
                    c.push_str("\n");
                    acc.push('\n');
                }
                c.push_str(line);
                acc.push_str(line);
            }
            for tail in ["\n\n(Shown: 3 results. All 3 results shown.)", "\n\n", "x"] {
                assert_eq!(
                    c.count_with_tail(tail),
                    count_tokens(&format!("{acc}{tail}")),
                    "cut={cut} tail={tail:?}"
                );
            }
        }
    }

    #[test]
    fn exact_prefix_counter_handles_blank_lines_and_cjk() {
        // Regression: o200k merges consecutive newlines into ONE piece ("\n\n" is a
        // single BPE token), and whitespace runs absorb following spaces. Committing
        // at a naive '\n' cut double-counts — pretoken boundaries are the only safe
        // commit points. Blank lines occur constantly in real code and in the search
        // content renderer's file-group separators.
        let lines = [
            "fn a() {",
            "",
            "}",
            "你好，世界",
            "",
            "  indented after blank",
        ];
        let mut c = ExactPrefixCounter::new();
        let mut acc = String::new();
        for l in &lines {
            c.push_str(l);
            c.push_str("\n");
            acc.push_str(l);
            acc.push('\n');
            assert_eq!(c.checkpoint(), count_tokens(&acc), "prefix drift at: {l:?}");
        }
        assert_eq!(c.finish(), count_tokens(&acc));
    }

    #[test]
    fn exact_prefix_counter_survives_contractions() {
        // Regression: the real o200k pattern attaches contractions as a SUFFIX of the
        // word pretoken ("don't" is ONE encoder pretoken). A splitter that cuts between
        // the word and the contraction commits pieces the encoder merges across, so
        // counts stop being additive. This test fails with the cl100k-shaped pattern
        // which matches 't / 'll /... as standalone pretokens).
        let lines = [
            "let x = 1; // don't do this",
            "can't we'll they've I'm you're",
        ];
        let mut c = ExactPrefixCounter::new();
        let mut acc = String::new();
        for l in &lines {
            c.push_str(l);
            c.push_str("\n");
            acc.push_str(l);
            acc.push('\n');
            assert_eq!(c.checkpoint(), count_tokens(&acc), "prefix drift at: {l:?}");
        }
        assert_eq!(c.finish(), count_tokens(&acc));
    }

    #[test]
    fn verify_accepts_the_full_recount() {
        let text = "fn main() { println!(\"hi\"); }";
        assert!(verify(count_tokens(text), text).is_ok());
    }

    #[test]
    fn verify_mismatch_message_is_stable() {
        // The text must have exactly 4 tokens (asserted), so the mandated message below
        // is exercised with the exact numbers.
        let text = "one two three four";
        assert_eq!(count_tokens(text), 4);
        let err = verify(3, text).unwrap_err();
        assert_eq!(
            err.to_string(),
            "internal error: count mismatch: incremental=3 full=4"
        );
    }
}
