// SPDX-License-Identifier: MIT OR Apache-2.0

//! mindctx CLI: `mindctx serve` (MCP stdio, 4 tools: search/glob/read/outline) plus
//! the status/index inspection subcommands.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "mindctx",
    version,
    about = "Local context engineering layer for coding agents — the right context, within budget, remembered",
    after_help = "MCP debugging: npx @modelcontextprotocol/inspector mindctx serve"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run as MCP stdio (host integration point: search/glob/read/outline)
    Serve {
        /// Project root (corpus scope); defaults to the current working directory. Use this when the host cannot set cwd
        #[arg(long)]
        root: Option<PathBuf>,
        /// Wire presentation: text (default, one rendered page per result) | envelope
        /// (complete envelope JSON for machine consumers). Overrides MINDCTX_WIRE.
        #[arg(long)]
        wire: Option<String>,
    },
    /// Show index and connection status
    Status,
    /// Build/rebuild the index (direct rg-layer query needs no prebuilt index; this command only reports corpus coverage)
    Index,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve { root, wire } => {
            // stdout is the MCP protocol channel; logs go to stderr only.
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .init();
            let root = root
                .map(Ok)
                .unwrap_or_else(|| std::env::current_dir().map_err(anyhow::Error::from))?;
            let root = root
                .canonicalize()
                .map_err(|e| anyhow::anyhow!("project root unreachable {root:?}: {e}"))?;
            let wire = resolve_wire_mode(wire.as_deref())?;
            eprintln!("[mindctx] serve project root: {}", root.display());
            eprintln!("[mindctx] serve wire mode: {}", wire.as_str());
            mindctx_mcp::serve_stdio(root, wire)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            Ok(())
        }
        Command::Status => status(),
        Command::Index => index_report(),
    }
}

/// Resolves the wire mode at the process boundary: explicit `--wire` → `MINDCTX_WIRE`
/// → text. Core is a pure lib and never reads env; the frozen non-UTF-8 env error text
/// stays in core (`wire::non_utf8_env_error`).
fn resolve_wire_mode(explicit: Option<&str>) -> anyhow::Result<mindctx_core::wire::WireMode> {
    let env = match std::env::var(mindctx_core::wire::WIRE_ENV_VAR) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(value)) => {
            return Err(mindctx_core::wire::non_utf8_env_error(&value).into());
        }
    };
    mindctx_core::wire::resolve_mode(explicit, env.as_deref()).map_err(|e| anyhow::anyhow!(e))
}

/// Direct rg-layer query needs no prebuilt index; this command walks the corpus once and
/// reports the scope retrieval will cover.
fn index_report() -> anyhow::Result<()> {
    let root = std::env::current_dir()?;
    let files = mindctx_core::index::walk(&root)?;
    println!("Project root: {}", root.display());
    println!(
        "the current scope queries the rg layer directly (zero index cost), no prebuild needed; retrieval corpus coverage:"
    );
    println!("  Files: {}", files.len());
    let mut by_ext: Vec<(String, usize)> = group_by_extension(&files).into_iter().collect();
    by_ext.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (ext, count) in by_ext.iter().take(15) {
        println!("  {ext:>10}  {count}");
    }
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    println!("  Total size: {total_bytes} bytes");
    if files.len() > u32::MAX as usize {
        // Theoretical guard; the single-threaded walk gets slow on huge repos.
        println!("  Hint: file count exceeds the direct-query's sane range.");
    }
    Ok(())
}

fn status() -> anyhow::Result<()> {
    let root = std::env::current_dir()?;
    let project = mindctx_core::config::project_dir(&root);
    let index_db = project.join("index.db");
    println!("mindctx v{}", mindctx_core::VERSION);
    println!("Project root: {}", root.display());
    println!(
        "Run directory: {} ({})",
        project.display(),
        if project.exists() {
            "exists"
        } else {
            "not created (created on demand on first use)"
        }
    );
    println!(
        "Index: {}",
        if index_db.exists() {
            "present"
        } else {
            "not built (retrieval queries the rg layer directly; no index required)"
        }
    );
    match mindctx_core::index::walk(&root) {
        Ok(files) => println!(
            "Retrieval corpus: {} files (gitignore respected)",
            files.len()
        ),
        Err(e) => println!("Retrieval corpus: unavailable ({e})"),
    }
    println!(
        "MCP integration: mindctx serve (stdio); debug with npx @modelcontextprotocol/inspector mindctx serve"
    );
    Ok(())
}

fn group_by_extension(files: &[mindctx_core::index::FileEntry]) -> Vec<(String, usize)> {
    let mut groups: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for f in files {
        let ext = std::path::Path::new(&f.rel)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_else(|| "(no extension)".to_string());
        *groups.entry(ext).or_insert(0) += 1;
    }
    groups.into_iter().collect()
}
