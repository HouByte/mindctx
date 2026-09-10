// SPDX-License-Identifier: MIT OR Apache-2.0

//! Unified error type. `anyhow` is only allowed in binary crates (cli/xtask);
//! core/mcp always return this type, and the caller decides how to present it.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("index error: {0}")]
    Index(String),

    #[error("encoding error: {0}")]
    Encoding(String),

    #[error("internal error: {0}")]
    Internal(String),

    /// The incremental fitting count disagreed with a full recount of the assembled wire
    /// text (budget = wire self-check). Always an internal bug: token counting is exact,
    /// so the two measures can never legitimately diverge.
    #[error("internal error: count mismatch: incremental={incremental} full={full}")]
    CountMismatch { incremental: u64, full: u64 },

    /// The assembled wire text exceeded the budget. The per-tool fitters guarantee the
    /// fit, so this fires only when the wire-tail reservation under-covered the actual
    /// render — fail loudly instead of injecting an over-budget page.
    #[error(
        "internal error: wire text exceeded the budget (OverBudget): returned={returned}, budget={budget}"
    )]
    OverBudget { returned: u64, budget: u64 },

    /// A file's identity (length + mtime) changed between the two stats that seal a snapshot;
    /// the request must be retried against a stable file.
    #[error("File changed while reading: {path}. Retry the request.")]
    FileChanged {
        path: String,
        tool: crate::budget::ToolKind,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
