// SPDX-License-Identifier: MIT OR Apache-2.0

//! mindctx core library: pure lib (state injected by callers), reusable across protocol lines.

pub mod budget;
pub mod config;
pub mod envelope;
pub mod error;
pub mod guard;

pub mod index;
pub mod retrieve;
pub mod symbol;

pub mod encoding;
pub mod tokenize;
pub mod wire;

/// Single source of the crate version (baseline for aligning `--version` with the npm package version).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
