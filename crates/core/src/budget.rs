// SPDX-License-Identifier: MIT OR Apache-2.0

//! Token budget resolution and wire-tail reservation (budget = wire).
//!
//! The budget constrains the WIRE TEXT the model receives (`wire::render`), not the
//! structured envelope. A page's cost is body + wire tail (status note, advisory notes).
//! Tools that render the complete candidate text per probe measure it directly; the
//! incremental read fitter reserves the tail up front via [`reserve_wire_tail`]
//! (the worst-case trailer, [`tail_skeleton`]). Every assembled page passes through
//! [`finish_wire`]: a full recount must match the incremental count (`CountMismatch`)
//! and fit the budget (`OverBudget`) — both hard errors, never a silent overshoot.

use crate::error::Error;
use crate::tokenize::count_tokens;

/// Default response budget when neither the global nor the per-tool env var is set.
pub const DEFAULT_TOKEN_BUDGET: u64 = 8_500;

/// Global budget env var; overridden by the per-tool var (`ToolKind::env_var`).
pub const GLOBAL_BUDGET_VAR: &str = "MINDCTX_TOKEN_BUDGET";

/// Which tool a budget is resolved for; selects the per-tool env var.
#[derive(Copy, Clone, Debug)]
pub enum ToolKind {
    Search,
    Glob,
    Read,
    Outline,
}

impl ToolKind {
    /// Per-tool budget env var; read before `MINDCTX_TOKEN_BUDGET`.
    pub fn env_var(self) -> &'static str {
        match self {
            ToolKind::Search => "MINDCTX_SEARCH_TOKEN_BUDGET",
            ToolKind::Glob => "MINDCTX_GLOB_TOKEN_BUDGET",
            ToolKind::Read => "MINDCTX_READ_TOKEN_BUDGET",
            ToolKind::Outline => "MINDCTX_OUTLINE_TOKEN_BUDGET",
        }
    }
}

/// Resolution order: per-tool raw value → global raw value → DEFAULT_TOKEN_BUDGET.
/// Per-tool value > global → hard error. 0 or non-integer → hard error. The caller
/// (MCP handler / CLI) reads the raw env values at the process boundary — core is a
/// pure lib and never touches `std::env`. `kind` is kept in the signature (it names
/// the budget being resolved and mirrors the per-tool var the caller read) but the
/// resolution itself is value-driven.
pub fn resolve_budget(
    _kind: ToolKind,
    global: Option<&str>,
    per_tool: Option<&str>,
) -> Result<u64, Error> {
    let global = match global {
        Some(raw) => parse_budget(raw).map_err(Error::Config)?,
        None => DEFAULT_TOKEN_BUDGET,
    };
    let per_tool = match per_tool {
        Some(raw) => Some(parse_budget(raw).map_err(Error::Config)?),
        None => None,
    };
    clamp_per_tool(global, per_tool).map_err(Error::Config)
}

/// Inputs that determine the worst-case wire tail of a response.
#[derive(Clone, Debug)]
pub struct WireTail {
    /// "files" | "matches" | "lines" | "entries"
    pub unit: &'static str,
    pub from: u64,
    pub to: u64,
    pub total: Option<u64>,
    /// Worst-case skip clause folded into the trailer note.
    pub skip_tally_line: Option<String>,
}

/// Exact reserved cost (tokens) of the wire tail for a page in the `params` state.
///
/// Counts the worst-case trailer from [`tail_skeleton`] — the status note a page can
/// carry in that state — with the same exact o200k measure as everything else
/// ([`crate::tokenize::count_tokens`]). The reservation over-covers on purpose: the
/// final exact accounting happens in [`finish_wire`].
pub fn reserve_wire_tail(params: WireTail) -> u64 {
    count_tokens(&tail_skeleton(&params))
}

/// The worst-case wire trailer that [`reserve_wire_tail`] counts: a blank-line
/// separator plus the longest status-note render for the state — the partial grammar
/// with the known total (its `resume from offset={to+1}` render upper-bounds the
/// complete grammar and the one-past-the-end resume pointer), with the skip clause
/// folded in as the trailing clause.
fn tail_skeleton(params: &WireTail) -> String {
    let mut note = match params.total {
        Some(total) => format!(
            "(Shown: {} {}-{} of {} shown. More remain — resume from offset={}.",
            params.unit,
            params.from,
            params.to,
            total,
            params.to.saturating_add(1)
        ),
        None => format!(
            "(Shown: {} {}-{} shown. More remain — resume from offset={}.",
            params.unit,
            params.from,
            params.to,
            params.to.saturating_add(1)
        ),
    };
    if let Some(tally) = &params.skip_tally_line {
        note.push(' ');
        note.push_str(tally);
    }
    note.push(')');
    format!("\n\n{note}")
}

/// Budget = wire: the page's exact wire token count, hard-verified.
///
/// `incremental` (when carried over from fitting) must equal a full recount of the
/// wire text — else [`Error::CountMismatch`]. The wire must fit `budget` — else
/// [`Error::OverBudget`]. Both are internal invariants (the fitters reserve the wire
/// tail), so tripping them means an accounting bug, never a degraded page.
pub fn finish_wire(wire: &str, incremental: Option<u64>, budget: u64) -> Result<u64, Error> {
    let full = count_tokens(wire);
    if let Some(expected) = incremental
        && expected != full
    {
        return Err(Error::CountMismatch {
            incremental: expected,
            full,
        });
    }
    if full > budget {
        return Err(Error::OverBudget {
            returned: full,
            budget,
        });
    }
    Ok(full)
}

/// Frozen `what` values for [`budget_too_small`] — contract strings, not free prose.
pub mod what {
    /// search content degradation that cannot even fit its continuation note
    pub const GREP_CONTINUATION_NOTE: &str = "grep continuation note";
    /// glob results that cannot fit their truncation note
    pub const GLOB_TRUNCATION_NOTE: &str = "glob truncation note";
    /// read (single file) continuation note
    pub const CONTINUATION_NOTE: &str = "continuation note";
    /// read (batch) continuation note
    pub const BATCH_CONTINUATION_NOTE: &str = "batch continuation note";
    /// outline tree page that cannot fit the budget at all (outline never paginates)
    pub const OUTLINE_TREE: &str = "outline tree page";
    /// terminal + token accounting fields alone exceed the budget
    pub const ENVELOPE_ACCOUNTING_FIELDS: &str = "envelope accounting fields";
}

/// Budget-exceeded message ladder (contract string): names the env var to raise and the
/// mandatory piece that cannot fit. Never a bodyless success.
pub fn budget_too_small(var: &str, budget: u64, what: &str) -> String {
    format!("{var}={budget} is too small to return the required {what}. Increase it and retry.")
}

/// The same ladder wrapped as a config error for a tool, naming its per-tool env var.
pub fn budget_too_small_error(tool: ToolKind, budget: u64, what: &'static str) -> Error {
    Error::Config(budget_too_small(tool.env_var(), budget, what))
}

/// A budget must be a positive integer; "0", negatives, and non-numbers are all rejected
/// with the same frozen message.
fn parse_budget(raw: &str) -> Result<u64, String> {
    raw.parse::<u64>()
        .ok()
        .filter(|budget| *budget > 0)
        .ok_or_else(|| "budget must be a positive integer".to_string())
}

/// A per-tool budget narrows the global budget but never widens it.
fn clamp_per_tool(global: u64, per_tool: Option<u64>) -> Result<u64, String> {
    match per_tool {
        None => Ok(global),
        Some(per) if per > global => Err(format!("per-tool budget {per} exceeds global {global}")),
        Some(per) => Ok(per),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_resolution_precedence() {
        // env manipulation via a scoped helper is unnecessary: unit-test the pure core
        assert_eq!(
            super::clamp_per_tool(8_500, Some(9_000))
                .unwrap_err()
                .to_string(),
            "per-tool budget 9000 exceeds global 8500"
        );
        assert_eq!(
            super::parse_budget("0").unwrap_err().to_string(),
            "budget must be a positive integer"
        );
        assert_eq!(
            super::parse_budget("abc").unwrap_err().to_string(),
            "budget must be a positive integer"
        );
        assert_eq!(super::parse_budget("1200").unwrap(), 1200);
    }

    #[test]
    fn wire_tail_is_the_wire_trailer_skeleton() {
        let params = super::WireTail {
            unit: "lines",
            from: 1,
            to: 100,
            total: Some(812),
            skip_tally_line: Some("2 files skipped, 1 path unreachable".into()),
        };
        let floor = super::reserve_wire_tail(params.clone());
        assert!(floor > 10 && floor < 120, "floor={floor}");

        // The reservation only covers the wire if the skeleton mirrors the worst-case
        // page tail: blank-line separator + the longest status-note render, with the
        // skip clause folded in and the one-past-the-end resume pointer.
        let rendered = super::tail_skeleton(&params);
        assert!(
            rendered.starts_with("\n\n(Shown: lines 1-100 of 812 shown."),
            "{rendered}"
        );
        assert!(rendered.contains("resume from offset=101"), "{rendered}");
        assert!(rendered.ends_with(")"), "{rendered}");
        assert!(rendered.contains("2 files skipped"), "{rendered}");
        // Body content and the structured envelope never ride the wire trailer.
        assert!(!rendered.contains("results"), "{rendered}");
        assert!(!rendered.contains("token_usage"), "{rendered}");
    }

    #[test]
    fn finish_wire_recounts_and_hard_checks() {
        let wire = "1\tfn main() {\n2\t}\n\n(Complete: reached end of file; lines 1-2 shown.)";
        let exact = super::count_tokens(wire);
        assert_eq!(
            super::finish_wire(wire, Some(exact), 8_500).unwrap(),
            exact,
            "matching incremental count returns the exact wire cost"
        );
        // Incremental ≠ full recount → CountMismatch.
        let err = super::finish_wire(wire, Some(exact + 1), 8_500).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "internal error: count mismatch: incremental={} full={exact}",
                exact + 1
            )
        );
        // Full recount over budget → OverBudget.
        let err = super::finish_wire(wire, Some(exact), exact - 1).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "internal error: wire text exceeded the budget (OverBudget): returned={exact}, budget={}",
                exact - 1
            )
        );
    }

    #[test]
    fn budget_too_small_message_is_stable() {
        // The ladder string is contract: var, budget, and the frozen `what`
        // must appear in exactly this sentence shape.
        assert_eq!(
            budget_too_small("MINDCTX_GLOB_TOKEN_BUDGET", 100, what::GLOB_TRUNCATION_NOTE),
            "MINDCTX_GLOB_TOKEN_BUDGET=100 is too small to return the required glob truncation note. Increase it and retry."
        );
    }
}
