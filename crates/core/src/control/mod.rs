// SPDX-License-Identifier: MIT OR Apache-2.0

//! Transactional apply/unapply: atomic host-config (Claude Code / Codex / AGENTS.md marker) edits with
//! receipt-driven restore. Planning is pure; `commit` writes backups + receipt first, then changes.

pub mod apply;
pub(crate) mod claude_config;
pub(crate) mod codex_config;
pub(crate) mod fsatomic;
pub mod receipt;
pub(crate) mod receipt_plan;

pub use apply::{Action, ApplyOpts, ChangeSet, FileChange, commit, plan_apply, plan_unapply};
pub use receipt::Receipt;
