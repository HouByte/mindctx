// SPDX-License-Identifier: MIT OR Apache-2.0

//! Codex (`~/.codex/config.toml`) configuration planning and the `mcp_servers.mindctx`
//! table removal used during selective unapply.

use std::path::Path;

use super::apply::{ApplyOpts, ChangeSet, FileChange};
use super::fsatomic::read_file_or_empty;
use crate::error::{Error, Result};

/// Plan Codex (~/.codex/config.toml) configuration.
pub(crate) fn plan_codex_config(home: &Path, opts: &ApplyOpts, set: &mut ChangeSet) -> Result<()> {
    let config_path = home.join(".codex/config.toml");
    let (original_existed, original_bytes) = read_file_or_empty(&config_path)?;

    // Error instead of silently corrupting non-UTF-8 configs.
    let content = if original_existed {
        String::from_utf8(original_bytes.clone()).map_err(|_| {
            Error::Config("Codex config is not valid UTF-8; refusing to modify it".into())
        })?
    } else {
        String::new()
    };
    // Parse into immutable Document, then convert to mutable DocumentMut
    let doc = toml_edit::Document::parse(&content)
        .map_err(|e| Error::Config(format!("failed to parse Codex config: {e}")))?;
    let mut doc = doc.into_mut();

    // Use the binary path from opts (injected at CLI entry)
    let binary_path_str = opts.binary_path.to_string_lossy().to_string();

    // Explicit table handling: indexing would panic on a malformed config.
    let root = doc.as_table_mut();
    let mcp_servers = root
        .entry("mcp_servers")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let mcp_servers = mcp_servers
        .as_table_mut()
        .ok_or_else(|| Error::Config("`mcp_servers` in Codex config is not a table".into()))?;
    let mindctx = mcp_servers
        .entry("mindctx")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let mindctx = mindctx.as_table_mut().ok_or_else(|| {
        Error::Config("`mcp_servers.mindctx` in Codex config is not a table".into())
    })?;

    mindctx.insert("command", toml_edit::value(binary_path_str));
    mindctx.insert(
        "args",
        toml_edit::value(toml_edit::Array::from_iter(vec!["serve"])),
    );

    // Add env for budget if specified
    if let Some(budget_val) = opts.budget {
        let env = mindctx
            .entry("env")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        let env = env.as_table_mut().ok_or_else(|| {
            Error::Config("`mcp_servers.mindctx.env` in Codex config is not a table".into())
        })?;
        env.insert(
            "MINDCTX_TOKEN_BUDGET",
            toml_edit::value(budget_val.to_string()),
        );
    } else {
        // Remove env key if it exists and budget is None
        mindctx.remove("env");
    }

    let new_bytes = doc.to_string().into_bytes();

    // Only add change if bytes differ (idempotence)
    if original_bytes != new_bytes {
        set.add(FileChange::write(
            config_path,
            original_bytes,
            new_bytes,
            original_existed,
        ));
    }

    Ok(())
}

/// Remove the `mcp_servers.mindctx` table from Codex config bytes, preserving everything else.
pub(crate) fn strip_codex_mindctx(current: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(current).ok()?;
    let mut doc: toml_edit::DocumentMut = text.parse().ok()?;
    let removed = doc
        .as_table_mut()
        .get_mut("mcp_servers")
        .and_then(|item| item.as_table_mut())
        .is_some_and(|t| t.remove("mindctx").is_some());
    if removed {
        Some(doc.to_string().into_bytes())
    } else {
        None
    }
}
