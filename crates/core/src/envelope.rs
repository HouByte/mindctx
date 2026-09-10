// SPDX-License-Identifier: MIT OR Apache-2.0

//! Output envelope — mindctx's external contract.
//!
//! Discipline: the JSON shape is locked by the inline assertions in
//! `crates/tests/core/envelope_smoke.rs` and the envelope unit tests below. A breaking
//! field change must move those locks in the same change; unknown JSON fields deserialize
//! as ignored, so added fields never break old consumers.
//! `token_usage` teaches the model to control its spend; `next_call` makes "truncated"
//! a resumable cursor rather than a dead end.

use serde::{Deserialize, Serialize};

/// Token accounting. `budget` is `None` when the caller set no budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub returned: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<u64>,
}

/// One recorded convention violation of a tool call: filled in when the server detects the caller
/// breaking a preset convention; a caller seeing a non-empty `violations` should self-correct its next call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Violation {
    /// Stable ID, e.g. `read_without_line_range_truncated`.
    pub name: String,
    /// Human-readable explanation, with a correction suggestion.
    pub message: String,
}

/// Pagination/continuation state: which slice of the result space the response
/// body covers, in which unit, and whether more exist. The machine contract for continuing is
/// `next_call`; [`Terminal::note`] is the human-readable rendering of the same fact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Terminal {
    /// `"complete"` when everything under the cap was shown, `"partial"` when not.
    pub state: String,
    /// What `shown_from`/`shown_to`/`total` count: `"files"` | `"matches"` | `"lines"` | `"entries"`.
    pub unit: String,
    /// 1-based inclusive lower bound of the shown slice.
    pub shown_from: u64,
    /// 1-based inclusive upper bound of the shown slice.
    pub shown_to: u64,
    /// Total in `unit` across the whole result space; `None` = unknown (e.g. early-stopped scan).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// Human note, e.g. "(Shown: files 1-100. More remain —
    /// resume from offset=100.)" — informative; the machine contract is next_call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Paths the tool could not return, with a bounded list of representative details.
/// Counts always serialize; `details` is capped at [`SKIP_DETAIL_CAP`] and the overflow goes to
/// `unlisted`, so the report cannot blow the budget it is reported inside of.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkipReport {
    /// Reached but not used (encoding skips, per-file failures).
    #[serde(default)]
    pub files: u64,
    /// Never entered (permission, vanished mid-walk).
    #[serde(default)]
    pub unreachable: u64,
    /// Representative details, capped at [`SKIP_DETAIL_CAP`].
    #[serde(default)]
    pub details: Vec<SkipDetail>,
    /// Details dropped by the cap (the caller cannot see them rendered).
    #[serde(default)]
    pub unlisted: u64,
}

/// Hard cap on [`SkipReport::details`]; overflow is counted in `SkipReport::unlisted`.
pub const SKIP_DETAIL_CAP: usize = 256;

/// One skipped path with its reason. Rendered form is the contract: `"{path} — {reason}"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkipDetail {
    /// Workspace-relative path of the skipped entry.
    pub path: String,
    /// Frozen reason string, e.g. "matching content and context exceed the 64 MiB safety limit".
    pub reason: String,
}

impl std::fmt::Display for SkipDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} — {}", self.path, self.reason)
    }
}

/// Unified output of all tools. JSON field names are the external contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub token_usage: TokenUsage,
    #[serde(default)]
    pub truncated: bool,
    /// Ready-to-paste snippet of continuation arguments; the machine contract for
    /// resuming a partial page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_call: Option<serde_json::Value>,
    /// Recorded convention violations: filled in when the server detects the caller breaking a preset convention; an empty array is not serialized.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<Violation>,
    /// Rendered page for search content mode (grouped text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Pagination/continuation state. Serialized flat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<Terminal>,
    /// Skipped/unreachable paths (search/glob).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_report: Option<SkipReport>,
}

/// Inputs for [`Envelope::page`]: a rendered page plus the exact wire accounting.
pub(crate) struct Page {
    pub returned: u64,
    pub budget: u64,
    pub truncated: bool,
    pub next_call: Option<serde_json::Value>,
    pub text: Option<String>,
    pub terminal: Option<Terminal>,
    pub skip_report: Option<SkipReport>,
}

impl Envelope {
    /// Empty envelope: the legitimate shape when the index is absent or retrieval has no hits.
    pub fn empty(budget: Option<u64>) -> Self {
        Self {
            token_usage: TokenUsage {
                returned: 0,
                budget,
            },
            truncated: false,
            next_call: None,
            violations: Vec::new(),
            // Optional appended fields: None here, so the empty shape stays byte-identical
            // for old consumers (skip_serializing_if drops them entirely).
            text: None,
            terminal: None,
            skip_report: None,
        }
    }

    /// Assemble a rendered page: exact wire accounting carried in `token_usage`, the
    /// structured facets after it. `violations` is the MCP layer's concern and stays
    /// empty on the core retrieval path.
    pub(crate) fn page(page: Page) -> Self {
        Self {
            token_usage: TokenUsage {
                returned: page.returned,
                budget: Some(page.budget),
            },
            truncated: page.truncated,
            next_call: page.next_call,
            violations: Vec::new(),
            text: page.text,
            terminal: page.terminal,
            skip_report: page.skip_report,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JSON shape of the contract — field names and nested structure are the external
    /// contract; a breaking change must move the locks in the same change.
    const DESIGN_EXAMPLE: &str = r#"{
        "text": "src/core/retrieve.rs\n42-pub fn handle()\n\n(Complete: 1 symbol shown.)",
        "token_usage": { "returned": 2380, "budget": 4000 },
        "truncated": false,
        "terminal": { "state": "complete", "unit": "lines", "shown_from": 1, "shown_to": 1 }
    }"#;

    #[test]
    fn parses_design_example() {
        let e: Envelope = serde_json::from_str(DESIGN_EXAMPLE).unwrap();
        assert!(e.text.as_deref().unwrap().contains("pub fn handle()"));
        assert_eq!(e.terminal.as_ref().unwrap().state, "complete");
        assert_eq!(e.token_usage.budget, Some(4000));
        assert!(!e.truncated);
    }

    #[test]
    fn roundtrips_and_tolerates_unknown_fields() {
        let e: Envelope = serde_json::from_str(DESIGN_EXAMPLE).unwrap();
        let e2: Envelope = serde_json::from_value(serde_json::to_value(&e).unwrap()).unwrap();
        assert_eq!(e, e2);

        // Unknown fields deserialize as ignored, so both future additions and fields
        // retired from the contract (results/citations/kb_hits/...) keep parsing.
        let with_extra: serde_json::Value = serde_json::from_str(
            r#"{ "token_usage": { "returned": 1 }, "results": [], "citations": [],
                 "kb_hits": [], "truncation_pointer": null, "future_field": true }"#,
        )
        .unwrap();
        serde_json::from_value::<Envelope>(with_extra).unwrap();
    }

    /// `text`/`terminal`/`skip_report` must be absent — not null — when unset,
    /// so the JSON shape consumers see carries no dead keys.
    #[test]
    fn optional_fields_absent_when_none() {
        let value = serde_json::to_value(Envelope::empty(Some(4_000))).unwrap();
        for key in ["text", "terminal", "skip_report", "next_call", "violations"] {
            assert!(
                value.get(key).is_none(),
                "{key} must be skipped when None: {value}"
            );
        }

        // A minimal payload reads the optional fields back as None.
        let e: Envelope =
            serde_json::from_str(r#"{ "token_usage": { "returned": 1 }, "truncated": false }"#)
                .unwrap();
        assert_eq!(e.text, None);
        assert_eq!(e.terminal, None);
        assert_eq!(e.skip_report, None);
    }

    /// Roundtrip: a fully populated envelope survives serialize → deserialize unchanged.
    #[test]
    fn fields_roundtrip() {
        let mut e = Envelope::empty(Some(4_000));
        e.token_usage.returned = 2380;
        e.truncated = true;
        e.next_call = Some(serde_json::json!({ "tool": "search", "arguments": { "offset": 100 } }));
        e.violations.push(Violation {
            name: "read_without_line_range_truncated".into(),
            message: "pass a line_range to keep responses bounded".into(),
        });
        e.text = Some("src/lib.rs\n  10: pub fn handle()".into());
        e.terminal = Some(Terminal {
            state: "partial".into(),
            unit: "matches".into(),
            shown_from: 1,
            shown_to: 100,
            total: Some(812),
            note: Some(
                "(Shown: matches 1-100 of 812. More remain — resume from offset=100.)".into(),
            ),
        });
        e.skip_report = Some(SkipReport {
            files: 3,
            unreachable: 1,
            details: vec![
                SkipDetail {
                    path: "vendor/big.bin".into(),
                    reason: "exceeds the safety limit".into(),
                },
                SkipDetail {
                    path: "etc/hosts".into(),
                    reason: "outside the workspace".into(),
                },
            ],
            unlisted: 1,
        });

        let e2: Envelope = serde_json::from_value(serde_json::to_value(&e).unwrap()).unwrap();
        assert_eq!(e, e2);
    }

    /// Inner optional fields (`Terminal.total`/`Terminal.note`) are likewise absent when None;
    /// `SkipReport` counts and `details` always serialize (shape the budget floor mirrors).
    #[test]
    fn inner_optional_fields_absent_when_none() {
        let terminal = serde_json::to_value(Terminal {
            state: "complete".into(),
            unit: "files".into(),
            shown_from: 1,
            shown_to: 3,
            total: None,
            note: None,
        })
        .unwrap();
        assert_eq!(
            terminal,
            serde_json::json!({
                "state": "complete",
                "unit": "files",
                "shown_from": 1,
                "shown_to": 3,
            })
        );

        let report = serde_json::to_value(SkipReport {
            files: 2,
            unreachable: 0,
            details: Vec::new(),
            unlisted: 0,
        })
        .unwrap();
        assert_eq!(
            report,
            serde_json::json!({
                "files": 2,
                "unreachable": 0,
                "details": [],
                "unlisted": 0,
            })
        );
    }

    /// Rendered skip line is the contract: `{path} — {reason}` with an em dash and spaces.
    #[test]
    fn skip_detail_renders_path_and_reason() {
        let detail = SkipDetail {
            path: "vendor/big.bin".into(),
            reason: "exceeds the 64 MiB safety limit".into(),
        };
        assert_eq!(
            detail.to_string(),
            "vendor/big.bin — exceeds the 64 MiB safety limit"
        );
    }

    /// Cap is part of the contract (keep the report bounded, overflow goes to `unlisted`).
    #[test]
    fn skip_detail_cap_is_256() {
        assert_eq!(SKIP_DETAIL_CAP, 256);
    }
}
