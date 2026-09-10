// SPDX-License-Identifier: MIT OR Apache-2.0

//! Wire presentation layer: what a tool call actually puts in front of its consumer,
//! and the one accounting rule that ties it to the budget.
//!
//! Two wire modes exist (resolved from `serve --wire` / `MINDCTX_WIRE`, see
//! [`resolve_mode`]):
//!
//! - [`WireMode::Text`] (default): the consumer receives exactly one text block — the
//!   rendered page ([`Envelope::text`]: body + advisory notes + the status note) or, for
//!   pages without a body, the compact fallback from [`render`]. The other envelope fields
//!   (`token_usage` / `terminal` / `skip_report` / `violations` / `next_call` / `truncated`)
//!   never ride this wire; every fact the model needs (continuation arguments, skip tally,
//!   status) is already folded into the page text by the tools.
//! - [`WireMode::Envelope`]: the complete envelope JSON for machine consumers (HTTP /
//!   IDE lines). Not an LLM injection target, so its size is not budget-constrained;
//!   the v3 envelope is the page plus its machine skeleton (`token_usage`/`terminal`/
//!   `skip_report`/`next_call`/`violations`) and carries no per-result duplication.
//!
//! Accounting (budget = wire): `token_usage.returned` is the exact o200k count of
//! [`render`], re-verified against a full recount with `CountMismatch` / `OverBudget`
//! hard errors (`budget::finish_wire`). The [`Envelope`] structure itself is untouched:
//! versioned contract (v3), locked by `crates/tests/core/envelope_smoke.rs` and the
//! inline shape literals in the envelope tests.

use crate::envelope::Envelope;
use crate::error::Error;

/// Environment variable selecting the wire mode (`text` | `envelope`).
pub const WIRE_ENV_VAR: &str = "MINDCTX_WIRE";

/// What the MCP layer sends back for a tool result.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum WireMode {
    /// One rendered text page per tool result (the LLM injection surface).
    #[default]
    Text,
    /// The complete envelope JSON, compact (machine consumers only).
    Envelope,
}

impl WireMode {
    /// Parses a wire-mode label (case-insensitive): `text` | `envelope`.
    pub fn parse(label: &str) -> Option<Self> {
        match label.to_ascii_lowercase().as_str() {
            "text" => Some(Self::Text),
            "envelope" => Some(Self::Envelope),
            _ => None,
        }
    }

    /// Canonical label (also the `MINDCTX_WIRE` value).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Envelope => "envelope",
        }
    }
}

/// Resolves the wire mode: explicit `--wire` label → `MINDCTX_WIRE` value →
/// [`WireMode::Text`]. An unknown label (flag or env) is a config error, never a
/// silent fallback. The caller reads the env value at the process boundary — core is a
/// pure lib and never touches `std::env`; a non-UTF-8 env value is reported through
/// [`non_utf8_env_error`].
pub fn resolve_mode(explicit: Option<&str>, env: Option<&str>) -> Result<WireMode, Error> {
    if let Some(label) = explicit {
        return WireMode::parse(label).ok_or_else(|| unknown_mode_error(Some(label)));
    }
    match env {
        Some(value) => WireMode::parse(value).ok_or_else(|| unknown_mode_error(Some(value))),
        None => Ok(WireMode::Text),
    }
}

/// The frozen config error for a non-UTF-8 `MINDCTX_WIRE` value (the caller reads the
/// env at the process boundary and hands the raw `OsString` here).
pub fn non_utf8_env_error(value: &std::ffi::OsString) -> Error {
    Error::Config(format!(
        "{WIRE_ENV_VAR} must be \"text\" or \"envelope\"; got a non-UTF-8 value: {value:?}"
    ))
}

fn unknown_mode_error(label: Option<&str>) -> Error {
    match label {
        Some(label) => Error::Config(format!(
            "{WIRE_ENV_VAR} must be \"text\" or \"envelope\"; got \"{label}\"."
        )),
        None => Error::Config(format!("{WIRE_ENV_VAR} must be \"text\" or \"envelope\".")),
    }
}

/// The text this envelope puts on the text wire: the rendered page when one exists,
/// otherwise the compact fallback (the terminal note — the whole content of note-only
/// and summary pages).
pub fn render(env: &Envelope) -> String {
    if let Some(text) = &env.text {
        return text.clone();
    }
    env.terminal
        .as_ref()
        .and_then(|terminal| terminal.note.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Terminal;

    fn envelope(text: Option<&str>, note: Option<&str>) -> Envelope {
        Envelope {
            text: text.map(str::to_string),
            terminal: note.map(|note| Terminal {
                state: "complete".to_string(),
                unit: "lines".to_string(),
                shown_from: 0,
                shown_to: 0,
                total: None,
                note: Some(note.to_string()),
            }),
            ..Envelope::empty(None)
        }
    }

    #[test]
    fn mode_parse_is_case_insensitive_and_total() {
        assert_eq!(WireMode::parse("text"), Some(WireMode::Text));
        assert_eq!(WireMode::parse("ENVELOPE"), Some(WireMode::Envelope));
        assert_eq!(WireMode::parse("json"), None);
        assert_eq!(WireMode::Text.as_str(), "text");
        assert_eq!(WireMode::Envelope.as_str(), "envelope");
        assert_eq!(WireMode::default(), WireMode::Text);
    }

    #[test]
    fn render_prefers_the_page_and_falls_back_to_the_note() {
        assert_eq!(
            render(&envelope(Some("1\tbody"), Some("(note)"))),
            "1\tbody"
        );
        assert_eq!(render(&envelope(None, Some("(note)"))), "(note)");
        // No page, no note (e.g. the empty envelope): an empty wire.
        assert_eq!(render(&envelope(None, None)), "");
    }

    #[test]
    fn resolve_mode_flag_overrides_env_and_rejects_unknown_labels() {
        // Flag wins without consulting the env value at all.
        assert_eq!(
            resolve_mode(Some("envelope"), Some("text")).unwrap(),
            WireMode::Envelope
        );
        let err = resolve_mode(Some("json"), None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "configuration error: MINDCTX_WIRE must be \"text\" or \"envelope\"; got \"json\"."
        );
    }

    #[test]
    fn resolve_mode_env_falls_back_to_text_and_rejects_unknown_values() {
        assert_eq!(resolve_mode(None, None).unwrap(), WireMode::Text);
        assert_eq!(
            resolve_mode(None, Some("envelope")).unwrap(),
            WireMode::Envelope
        );
        let err = resolve_mode(None, Some("json")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "configuration error: MINDCTX_WIRE must be \"text\" or \"envelope\"; got \"json\"."
        );
    }
}
