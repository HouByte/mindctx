// SPDX-License-Identifier: MIT OR Apache-2.0

//! mindctx CLI: manual-entry subcommands (apply/unapply/status/index) plus
//! `mindctx serve` (MCP stdio, 4 tools: search/glob/read/outline).

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
    /// Write host config and bootstrap blocks: Codex/Claude Code MCP + AGENTS.md marker
    Apply {
        /// Host home directory (default: auto-detected from environment)
        #[arg(long)]
        home: Option<PathBuf>,
        /// Set Claude Code MCP registration
        #[arg(long, default_value_t = true)]
        claude: bool,
        /// Set Codex MCP registration
        #[arg(long, default_value_t = true)]
        codex: bool,
        /// Set token budget (omitted if not specified)
        #[arg(long)]
        budget: Option<u64>,
        /// Skip confirmation prompts
        #[arg(short, long, default_value_t = false)]
        yes: bool,
    },
    /// Cleanly remove what apply wrote
    Unapply {
        /// Host home directory (default: auto-detected from environment)
        #[arg(long)]
        home: Option<PathBuf>,
        /// Skip confirmation prompt
        #[arg(short, long, default_value_t = false)]
        yes: bool,
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
        Command::Apply {
            home,
            claude,
            codex,
            budget,
            yes,
        } => apply_cmd(home, claude, codex, budget, yes),
        Command::Unapply { home, yes } => unapply_cmd(home, yes),
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

/// Surface non-fatal planning observations (skipped files, left-alone modified files).
fn print_warnings(changeset: &mindctx_core::control::ChangeSet) {
    for warning in &changeset.warnings {
        eprintln!("[mindctx] warning: {warning}");
    }
}

/// Resolve home directory. PRODUCTION ONLY — in tests, an explicit path is always passed.
fn resolve_home(home_arg: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match home_arg {
        Some(h) => Ok(h),
        None => {
            // PRODUCTION ONLY: auto-detect from environment
            std::env::var("HOME")
                .map(PathBuf::from)
                .map_err(|_| anyhow::anyhow!("HOME not set and --home not provided"))
        }
    }
}

/// Apply command: install host configuration and AGENTS.md markers.
fn apply_cmd(
    home_arg: Option<PathBuf>,
    claude: bool,
    codex: bool,
    budget: Option<u64>,
    yes: bool,
) -> anyhow::Result<()> {
    let home = resolve_home(home_arg)?;
    let project_root =
        std::env::current_dir().map_err(|e| anyhow::anyhow!("project root unreachable: {e}"))?;
    let binary_path =
        std::env::current_exe().map_err(|e| anyhow::anyhow!("binary path unreachable: {e}"))?;

    eprintln!("[mindctx] apply home: {}", home.display());
    eprintln!("[mindctx] project root: {}", project_root.display());

    let opts = mindctx_core::control::ApplyOpts {
        claude,
        codex,
        budget,
        yes,
        binary_path,
    };

    let changeset = mindctx_core::control::plan_apply(&home, &project_root, &opts)
        .map_err(|e| anyhow::anyhow!(e))?;

    print_warnings(&changeset);

    if changeset.files.is_empty() {
        eprintln!("[mindctx] nothing to apply (idempotent - already applied)");
        return Ok(());
    }

    // Show summary of changes (excluding receipt details)
    let file_changes: Vec<_> = changeset
        .files
        .iter()
        .filter(|c| {
            !c.target
                .to_string_lossy()
                .contains(".mindctx/record/receipt")
        })
        .collect();
    if !yes {
        eprintln!("\nChanges to be applied:");
        for fc in &file_changes {
            eprintln!("  {} {}", fc.action_str(), fc.target.display());
        }
        eprint!("\nApply these changes? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            eprintln!("[mindctx] cancelled");
            return Ok(());
        }
    } else {
        eprintln!("\nApplying {} changes...", file_changes.len());
    }

    mindctx_core::control::commit(&changeset).map_err(|e| anyhow::anyhow!(e))?;

    eprintln!("[mindctx] apply complete");
    Ok(())
}

/// Unapply command: restore files from backups and remove receipt. Unapply is destructive
/// (whole-file restores, created-file deletes), so it asks for confirmation unless --yes.
fn unapply_cmd(home_arg: Option<PathBuf>, yes: bool) -> anyhow::Result<()> {
    let home = resolve_home(home_arg)?;

    eprintln!("[mindctx] unapply home: {}", home.display());

    let changeset = mindctx_core::control::plan_unapply(&home).map_err(|e| anyhow::anyhow!(e))?;

    print_warnings(&changeset);

    if changeset.files.is_empty() {
        eprintln!("[mindctx] nothing to unapply (no receipt found)");
        return Ok(());
    }

    if !yes {
        eprintln!("\nUnapply will change:");
        for fc in &changeset.files {
            eprintln!("  {} {}", fc.action_str(), fc.target.display());
        }
        eprint!("\nUnapply these changes? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            eprintln!("[mindctx] cancelled");
            return Ok(());
        }
    }

    eprintln!("Restoring {} files from backups...", changeset.files.len());

    mindctx_core::control::commit(&changeset).map_err(|e| anyhow::anyhow!(e))?;

    eprintln!("[mindctx] unapply complete");
    Ok(())
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
