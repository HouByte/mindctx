// SPDX-License-Identifier: MIT OR Apache-2.0

//! Release automation. Run: `cargo run -p xtask -- <task>`.
//!
//! Pipeline: platform matrix builds -> SHA256SUMS -> GitHub Release -> npm platform packages +
//! launcher (classic NPM_TOKEN auth) -> third-party disclosure (cargo-about).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// Staging map: dist target triple -> npm platform package directory.
const PLATFORM_TARGETS: &[(&str, &str)] = &[
    ("aarch64-apple-darwin", "mindctx-darwin-arm64"),
    ("x86_64-apple-darwin", "mindctx-darwin-x64"),
    ("x86_64-unknown-linux-musl", "mindctx-linux-x64"),
    ("x86_64-pc-windows-msvc", "mindctx-win32-x64"),
];

// crates.io publish order (dependency order): core first, then mcp, then the cli binary.
const PUBLISH_ORDER: &[(&str, &str)] = &[
    ("mindctx-core", "crates/core"),
    ("mindctx-mcp", "crates/mcp"),
    ("mindctx", "crates/cli"),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("dist") => dist(&args[2..]),
        Some("npm") => npm(&args[2..]),
        Some("release") => release(&args[2..]),
        Some("verify-publish") => verify_publish(&args[2..]),
        Some("preflight") => preflight(&args[2..]),
        Some("about") => about(&args[2..]),
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!("usage: cargo run -p xtask -- <dist|npm|release|verify-publish|preflight|about>");
    std::process::exit(2)
}

// ---------------------------------------------------------------------------
// dist --target <triple>
// ---------------------------------------------------------------------------

fn dist(args: &[String]) {
    let mut target = None;
    let dry_run = consume_flag(args, "--dry-run");

    for i in 0..args.len() {
        if args[i] == "--target" && i + 1 < args.len() {
            target = Some(&args[i + 1]);
            break;
        }
    }

    let Some(target) = target else {
        eprintln!("usage: xtask dist --target <triple> [--dry-run]");
        std::process::exit(2);
    };

    if dry_run {
        println!("plan: cargo build --release --target {} -p mindctx", target);
        println!("plan: mkdir -p dist/{}", target);
        println!(
            "plan: cp target/{}/release/mindctx[.exe] dist/{}/mindctx-{}[.exe]",
            target, target, target
        );
        println!(
            "plan: write dist/{}/SHA256SUMS with \"<hash>  {}/mindctx-{}[.exe]\"",
            target, target, target
        );
        return;
    }

    // Build
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", target, "-p", "mindctx"])
        .status()
        .expect("cargo build failed");
    if !status.success() {
        std::process::exit(1);
    }

    // Stage artifact directory
    let dist_dir = Path::new("dist").join(target);
    fs::create_dir_all(&dist_dir).expect("create dist dir failed");

    // The release binary is staged under a target-qualified name: GitHub Release assets are
    // flat, and four binaries named `mindctx` would collapse into one upload.
    let src = Path::new("target")
        .join(target)
        .join("release")
        .join(packaged_binary_name(target));
    let dst = dist_dir.join(staged_binary_name(target));
    fs::copy(&src, &dst).expect("copy binary failed");

    // SHA256, target-qualified so a combined multi-platform SHA256SUMS stays matchable
    let sha256 = sha256_file(&dst);
    let checksum_path = dist_dir.join("SHA256SUMS");
    fs::write(
        &checksum_path,
        format!("{}  {}/{}\n", sha256, target, staged_binary_name(target)),
    )
    .expect("write SHA256SUMS failed");

    println!("dist: {} -> {:?}", dst.display(), checksum_path);
}

/// Release-asset file name for a target: target-qualified so the flat GitHub Release asset
/// namespace never collides across the build matrix. Keeps the `.exe` suffix
/// on Windows targets.
fn staged_binary_name(target: &str) -> String {
    if target.contains("windows") {
        format!("mindctx-{target}.exe")
    } else {
        format!("mindctx-{target}")
    }
}

/// File name of the binary inside an npm platform package's bin/ dir: the launcher's
/// resolveBinPath expects bin/mindctx[.exe], so the per-target staged name is renamed back to
/// the packaged name when `xtask npm` stages the bins.
fn packaged_binary_name(target: &str) -> &'static str {
    if target.contains("windows") {
        "mindctx.exe"
    } else {
        "mindctx"
    }
}

// ---------------------------------------------------------------------------
// release <version> --tag-msg <msg> [--no-verify] [--allow-dirty]
// ---------------------------------------------------------------------------

// Bump the workspace and every package version, then commit. On main the commit
// is also tagged locally; on a `release/*` branch the tag is left to be created
// on main after the merge, since the commit is not on main yet.
fn release(args: &[String]) {
    let mut version: Option<String> = None;
    let mut tag_msg: Option<String> = None;
    let mut no_verify = false;
    let mut allow_dirty = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tag-msg" => {
                tag_msg = args.get(i + 1).cloned();
                i += 2;
            }
            "--no-verify" => {
                no_verify = true;
                i += 1;
            }
            "--allow-dirty" => {
                allow_dirty = true;
                i += 1;
            }
            "--help" | "-h" => {
                eprintln!(
                    "usage: xtask release <version> --tag-msg <msg> [--no-verify] [--allow-dirty]"
                );
                return;
            }
            other if other.starts_with("--") => {
                eprintln!("xtask release: unknown flag {}", other);
                std::process::exit(2);
            }
            _ => {
                if version.is_some() {
                    eprintln!("xtask release: unexpected positional argument: {}", args[i]);
                    std::process::exit(2);
                }
                version = Some(args[i].clone());
                i += 1;
            }
        }
    }

    let version = version.unwrap_or_else(|| {
        eprintln!("xtask release: missing <version>");
        eprintln!("usage: xtask release <version> --tag-msg <msg> [--no-verify] [--allow-dirty]");
        std::process::exit(2);
    });
    let tag_msg = tag_msg.unwrap_or_else(|| {
        eprintln!("xtask release: missing --tag-msg <msg>");
        std::process::exit(2);
    });

    if semver::Version::parse(&version).is_err() {
        eprintln!(
            "xtask release: '{}' is not a valid semver version (e.g. 1.2.3)",
            version
        );
        std::process::exit(2);
    }

    if !allow_dirty && !is_working_tree_clean() {
        eprintln!("xtask release: working tree is not clean (use --allow-dirty to override)");
        std::process::exit(1);
    }

    // A release lands on main through a pull request, so the bump is prepared on
    // a `release/*` branch and merged. Every other branch is still refused: a tag
    // pointing outside main's history would publish a release main never had.
    let branch = current_git_branch();
    let on_main = branch == "main";
    if !on_main && !branch.starts_with("release/") {
        eprintln!(
            "xtask release: refusing to release from branch '{}' (must be 'main' or 'release/*')",
            branch
        );
        std::process::exit(1);
    }

    let tag_name = format!("v{}", version);
    if git_tag_exists(&tag_name) {
        eprintln!("xtask release: tag {} already exists locally", tag_name);
        std::process::exit(1);
    }

    let root = workspace_root();
    let current_version = workspace_version(&root.join("Cargo.toml")).unwrap_or_else(|e| {
        eprintln!("xtask release: failed to read current version: {}", e);
        std::process::exit(1);
    });
    if version == current_version {
        eprintln!(
            "xtask release: new version {} equals current version {}",
            version, current_version
        );
        std::process::exit(1);
    }

    // Write all files first; git add + commit happens only after every write
    // succeeded (and the self-check passed).
    let mut changed: Vec<PathBuf> = Vec::new();

    let cargo_path = root.join("Cargo.toml");
    update_cargo_toml_version(&cargo_path, &version).unwrap_or_else(|e| {
        eprintln!(
            "xtask release: failed to update {}: {}",
            cargo_path.display(),
            e
        );
        std::process::exit(1);
    });
    changed.push(cargo_path);

    let packages_dir = root.join("packages");
    let launcher_dir = packages_dir.join("mindctx");
    update_launcher_package_json(&launcher_dir, &current_version, &version).unwrap_or_else(|e| {
        eprintln!(
            "xtask release: failed to update {}: {}",
            launcher_dir.join("package.json").display(),
            e
        );
        std::process::exit(1);
    });
    changed.push(launcher_dir.join("package.json"));

    let mut platform_entries: Vec<PathBuf> = fs::read_dir(&packages_dir)
        .map_err(|e| format!("read {}: {}", packages_dir.display(), e))
        .unwrap_or_else(|e| {
            eprintln!("xtask release: {}", e);
            std::process::exit(1);
        })
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| p.file_name().and_then(|n| n.to_str()) != Some("mindctx"))
        .collect();
    platform_entries.sort();

    for dir in &platform_entries {
        let pkg_path = dir.join("package.json");
        update_platform_package_json(&pkg_path, &current_version, &version).unwrap_or_else(|e| {
            eprintln!(
                "xtask release: failed to update {}: {}",
                pkg_path.display(),
                e
            );
            std::process::exit(1);
        });
        changed.push(pkg_path);
    }

    // dry_run=true skips the dist/bin staging.
    if !no_verify {
        if let Err(err) = npm_version_sync(
            &packages_dir,
            &root.join("dist"),
            &root.join("Cargo.toml"),
            true,
        ) {
            eprintln!(
                "xtask release: version-sync self-check FAILED after file writes; \
                 files have been modified but not committed.\n  {}",
                err
            );
            std::process::exit(1);
        }
    }

    for path in &changed {
        run_git_in(&root, "add", &[&path.to_string_lossy()]);
    }

    let subject = format!("chore(release): bump to {}", version);
    let body = format!(
        "- xtask release {}: writes [workspace.package].version + the two internal \
         path-dep mirrors, all {} packages/*/package.json versions, the launcher's \
         optionalDependencies pins.\n- self-check \
         (xtask npm --version-sync --dry-run) {} before this commit landed.",
        version,
        platform_entries.len() + 1,
        if no_verify {
            "skipped via --no-verify"
        } else {
            "passed"
        },
    );
    run_git_in(&root, "commit", &["-m", &subject, "-m", &body]);

    println!();
    if on_main {
        run_git_in(&root, "tag", &["-a", &tag_name, "-m", &tag_msg]);
        println!("xtask release: tag {} created locally.", tag_name);
        println!(
            "xtask release: next step: git push origin main {}",
            tag_name
        );
    } else {
        // No tag on a release branch: this commit is not on main yet, and a tag
        // pushed before the merge would trigger a release built from a commit
        // main never had. Tagging main after the merge also survives a rebase,
        // which would have moved the commit and left a branch-created tag behind.
        println!("xtask release: bump committed on {}.", branch);
        println!("xtask release: next steps:");
        println!("  1. open a pull request for {} and merge it", branch);
        println!("  2. git switch main && git pull --ff-only");
        println!("  3. git tag -a {} -m {:?}", tag_name, tag_msg);
        println!("  4. git push origin {}", tag_name);
    }
}

fn is_working_tree_clean() -> bool {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .expect("git status failed");
    output.status.success() && output.stdout.is_empty()
}

fn current_git_branch() -> String {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .expect("git branch failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn git_tag_exists(name: &str) -> bool {
    let output = Command::new("git")
        .args(["tag", "--list", name])
        .output()
        .expect("git tag failed");
    String::from_utf8_lossy(&output.stdout).trim() == name
}

fn run_git_in(cwd: &Path, cmd: &str, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(cwd)
        .arg(cmd)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("git {} failed: {}", cmd, e));
    if !status.success() {
        eprintln!("xtask release: git {} failed", cmd);
        std::process::exit(1);
    }
}

// Update workspace.package.version and the two internal path-dep mirrors.
fn update_cargo_toml_version(path: &Path, version: &str) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("parse {}: {}", path.display(), e))?;

    {
        let ws_pkg = doc
            .get_mut("workspace")
            .and_then(|w| w.as_table_mut())
            .and_then(|t| t.get_mut("package"))
            .and_then(|p| p.as_table_mut())
            .ok_or_else(|| format!("missing [workspace.package] in {}", path.display()))?;
        ws_pkg.insert(
            "version",
            toml_edit::Item::Value(toml_edit::Value::from(version)),
        );
    }
    {
        let deps = doc
            .get_mut("workspace")
            .and_then(|w| w.as_table_mut())
            .and_then(|t| t.get_mut("dependencies"))
            .and_then(|d| d.as_table_mut())
            .ok_or_else(|| format!("missing [workspace.dependencies] in {}", path.display()))?;
        for key in ["mindctx-core", "mindctx-mcp"] {
            let dep = deps
                .get_mut(key)
                .and_then(|d| d.as_inline_table_mut())
                .ok_or_else(|| {
                    format!(
                        "[workspace.dependencies].{} not an inline table in {}",
                        key,
                        path.display()
                    )
                })?;
            dep.insert("version", toml_edit::Value::from(version));
        }
    }

    fs::write(path, doc.to_string()).map_err(|e| format!("write {}: {}", path.display(), e))?;
    Ok(())
}

// Update the launcher's package.json: top-level version and every optionalDependencies pin.
// Iterates the keys actually present (no fixed list) so adding or removing a platform doesn't
// require touching this helper.
fn update_launcher_package_json(
    pkg_dir: &Path,
    old_version: &str,
    new_version: &str,
) -> Result<(), String> {
    let path = pkg_dir.join("package.json");
    let text = fs::read_to_string(&path).map_err(|e| format!("read {}: {}", path.display(), e))?;

    let mut updated = replace_json_string_field(&text, "version", old_version, new_version)
        .ok_or_else(|| format!("\"version\" not found in {}", path.display()))?;

    let json: serde_json::Value =
        serde_json::from_str(&updated).map_err(|e| format!("parse {}: {}", path.display(), e))?;
    let opt_deps = json
        .get("optionalDependencies")
        .and_then(|v| v.as_object())
        .ok_or_else(|| format!("missing optionalDependencies in {}", path.display()))?;

    for (platform, current) in opt_deps {
        let current = current
            .as_str()
            .ok_or_else(|| format!("{} pin in {} is not a string", platform, path.display()))?;
        if current == new_version {
            continue;
        }
        if current != old_version {
            return Err(format!(
                "{} pin in {} is {} (expected old {} or new {}); refusing to overwrite a value we did not set",
                platform,
                path.display(),
                current,
                old_version,
                new_version
            ));
        }
        let replaced = replace_json_string_field(&updated, platform, old_version, new_version);
        match replaced {
            Some(s) => updated = s,
            None => {
                return Err(format!(
                    "{} pin to {} not found in {} (string match failed despite parse locating it)",
                    platform,
                    old_version,
                    path.display()
                ));
            }
        }
    }

    fs::write(&path, updated).map_err(|e| format!("write {}: {}", path.display(), e))?;
    Ok(())
}

// Update packages/<platform>/package.json: only the top-level "version" field.
fn update_platform_package_json(
    path: &Path,
    old_version: &str,
    new_version: &str,
) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let updated = replace_json_string_field(&text, "version", old_version, new_version)
        .ok_or_else(|| format!("\"version\" not found in {}", path.display()))?;
    fs::write(path, updated).map_err(|e| format!("write {}: {}", path.display(), e))?;
    Ok(())
}

// Replace the value of a top-level string-valued JSON field. Returns None if
// the field is missing or its value doesn't equal old_value.
fn replace_json_string_field(
    text: &str,
    field: &str,
    old_value: &str,
    new_value: &str,
) -> Option<String> {
    let needle = format!("\"{}\": \"{}\"", field, old_value);
    let replacement = format!("\"{}\": \"{}\"", field, new_value);
    let idx = text.find(&needle)?;
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..idx]);
    out.push_str(&replacement);
    out.push_str(&text[idx + needle.len()..]);
    Some(out)
}

// ---------------------------------------------------------------------------
// npm --version-sync [--dry-run]
// ---------------------------------------------------------------------------

fn npm(args: &[String]) {
    let dry_run = consume_flag(args, "--dry-run");
    if !consume_flag(args, "--version-sync") {
        eprintln!("usage: xtask npm --version-sync [--dry-run]");
        std::process::exit(2);
    }

    let root = workspace_root();
    match npm_version_sync(
        &root.join("packages"),
        &root.join("dist"),
        &root.join("Cargo.toml"),
        dry_run,
    ) {
        Ok(()) => println!("xtask npm: version sync + bin staging OK"),
        Err(err) => {
            eprintln!("xtask npm: {}", err);
            std::process::exit(1);
        }
    }
}

fn npm_version_sync(
    packages_dir: &Path,
    dist_dir: &Path,
    workspace_manifest: &Path,
    dry_run: bool,
) -> Result<(), String> {
    let version = workspace_version(workspace_manifest)?;
    println!("workspace version: {}", version);

    check_package_versions(
        packages_dir,
        &version,
        &workspace_repository(workspace_manifest)?,
    )?;

    if dry_run {
        println!("plan: all package.json versions match workspace version");
        for (target, pkg) in PLATFORM_TARGETS {
            println!(
                "plan: stage {}/mindctx-{}[.exe] -> packages/{}/bin/mindctx[.exe]",
                target, target, pkg
            );
        }
        return Ok(());
    }

    stage_platform_bins(packages_dir, dist_dir)
}

fn check_package_versions(
    packages_dir: &Path,
    version: &str,
    repository: &str,
) -> Result<(), String> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(packages_dir)
        .map_err(|e| format!("read {}: {}", e, packages_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    let mut mismatches = Vec::new();
    for dir in &dirs {
        let manifest = dir.join("package.json");
        let text =
            fs::read_to_string(&manifest).map_err(|e| format!("read package.json: {}", e))?;
        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("parse package.json: {}", e))?;
        let name = json["name"].as_str().unwrap_or("?").to_string();
        let found = json["version"].as_str().unwrap_or("?").to_string();
        // npm enables provenance automatically when publishing from GitHub Actions for a
        // public package, and the registry then rejects the publish unless repository.url
        // names the repository the provenance was minted from. A package that omits the
        // field publishes fine everywhere else and fails only at the registry, so it is
        // checked here, before anything is built or uploaded.
        let declared = json["repository"]["url"]
            .as_str()
            .map(normalize_repository)
            .unwrap_or_default();
        if found != version {
            mismatches.push(format!(
                "{} has version {} but workspace is {}",
                name, found, version
            ));
        } else if declared != repository {
            mismatches.push(format!(
                "{} declares repository {:?} but the workspace repository is {}",
                name,
                json["repository"]["url"].as_str().unwrap_or(""),
                repository
            ));
        } else {
            println!("  {} {}: OK", name, found);
        }
    }

    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "version sync check FAILED:\n  {}",
            mismatches.join("\n  ")
        ))
    }
}

// Stage release binaries from dist/<target>/ into the npm platform packages' bin/ dirs.
// The target-qualified staged name is renamed back to the packaged mindctx[.exe] the launcher
// expects. All four artifacts are required.
fn stage_platform_bins(packages_dir: &Path, dist_dir: &Path) -> Result<(), String> {
    for (target, pkg) in PLATFORM_TARGETS {
        let src = dist_dir.join(target).join(staged_binary_name(target));
        if !src.exists() {
            return Err(format!(
                "missing dist artifact {}: run `cargo run -p xtask -- dist --target {}` first",
                src.display(),
                target
            ));
        }
        let bin_dir = packages_dir.join(pkg).join("bin");
        fs::create_dir_all(&bin_dir).map_err(|e| format!("create {}: {}", e, bin_dir.display()))?;
        let dst = bin_dir.join(packaged_binary_name(target));
        fs::copy(&src, &dst)
            .map_err(|e| format!("copy {} -> {}: {}", e, src.display(), dst.display()))?;
        make_executable(&dst)?;
        println!("  staged {} -> {}", src.display(), dst.display());
    }
    Ok(())
}

fn make_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)
            .map_err(|e| format!("stat {}: {}", e, path.display()))?
            .permissions();
        perms.set_mode(perms.mode() | 0o755);
        fs::set_permissions(path, perms).map_err(|e| format!("chmod {}: {}", e, path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn workspace_version(manifest: &Path) -> Result<String, String> {
    let text =
        fs::read_to_string(manifest).map_err(|e| format!("read {}: {}", e, manifest.display()))?;
    let value = parse_manifest(&text, manifest)?;
    value
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            format!(
                "no [workspace.package] version found in {}",
                manifest.display()
            )
        })
}

fn workspace_repository(manifest: &Path) -> Result<String, String> {
    let text =
        fs::read_to_string(manifest).map_err(|e| format!("read {}: {}", manifest.display(), e))?;
    let value = parse_manifest(&text, manifest)?;
    value
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("repository"))
        .and_then(|v| v.as_str())
        .map(normalize_repository)
        .ok_or_else(|| {
            format!(
                "no [workspace.package] repository found in {}",
                manifest.display()
            )
        })
}

// Compare repository URLs across ecosystems: Cargo uses "https://host/owner/repo", npm
// package.json uses "git+https://host/owner/repo.git". Only the path identifies the
// repository, so the transport prefix and the .git suffix are dropped.
fn normalize_repository(url: &str) -> String {
    url.trim()
        .trim_start_matches("git+")
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

// Parse a Cargo.toml into the document root table.
fn parse_manifest(text: &str, path: &Path) -> Result<toml::Table, String> {
    toml::from_str(text).map_err(|e| format!("parse {}: {}", path.display(), e))
}

fn workspace_root() -> PathBuf {
    // cargo run/test sets CARGO_MANIFEST_DIR to crates/xtask; the workspace root is two levels up.
    std::env::var("CARGO_MANIFEST_DIR")
        .ok()
        .and_then(|d| {
            PathBuf::from(d)
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// verify-publish [--dry-run]
// ---------------------------------------------------------------------------

fn verify_publish(args: &[String]) {
    let dry_run = consume_flag(args, "--dry-run");

    if dry_run {
        println!("plan: verify-publish dry-run — publish order:");
        for (i, (crate_name, _)) in PUBLISH_ORDER.iter().enumerate() {
            println!(
                "plan:   {}. cargo publish --dry-run -p {}",
                i + 1,
                crate_name
            );
        }
        println!("plan:   4. npm publish packages/mindctx --access public (dry-run)");
        return;
    }

    let root = workspace_root();
    if let Err(err) = verify_publish_checks(&root) {
        eprintln!("verify-publish: {}", err);
        std::process::exit(1);
    }

    // Real dry-run publishes, in dependency order (crates.io). A dependent crate's
    // dry-run cannot resolve internal deps that are not yet on crates.io, so before
    // the first real publish those failures are expected and only the manifest
    // checks cover them; every other failure is fatal.
    for (i, (crate_name, _)) in PUBLISH_ORDER.iter().enumerate() {
        println!("verify-publish: cargo publish --dry-run -p {}", crate_name);
        let output = match Command::new("cargo")
            .args(["publish", "--dry-run", "-p", crate_name])
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                eprintln!("verify-publish: failed to run cargo: {}", e);
                std::process::exit(1);
            }
        };
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            continue;
        }
        let internal_deps: Vec<&str> = PUBLISH_ORDER[..i].iter().map(|(n, _)| *n).collect();
        if is_unpublished_internal_dep(&stderr, &internal_deps) {
            println!(
                "  SKIPPED (expected pre-first-publish: {} not yet on crates.io; dependency order verified from manifests)",
                internal_deps.join(", ")
            );
            continue;
        }
        eprintln!(
            "verify-publish: cargo publish --dry-run -p {} failed ({})",
            crate_name, output.status
        );
        eprintln!("{}", stderr);
        std::process::exit(1);
    }

    // npm launcher dry-run: builds the tarball without publishing (no auth needed).
    // The ./ prefix matters: a bare `packages/mindctx` spec is parsed by npm as a
    // GitHub user/repo shorthand, not a directory.
    println!("verify-publish: npm publish --dry-run ./packages/mindctx --access public");
    let status = Command::new("npm")
        .args([
            "publish",
            "--dry-run",
            "./packages/mindctx",
            "--access",
            "public",
        ])
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("verify-publish: npm publish --dry-run failed ({})", s);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!(
                "verify-publish: failed to run npm (is Node.js installed?): {}",
                e
            );
            std::process::exit(1);
        }
    }

    println!("verify-publish: OK (manifest checks + dry-run publishes in dependency order)");
}

// True when cargo's dry-run failure is the expected pre-first-publish condition:
// an internal dependency is not on crates.io yet, so packaging cannot resolve it.
//
// Two shapes, because cargo words this differently depending on whether the dependency has
// any published version at all: `no matching package named` when the name is absent from the
// index, and `failed to select a version for the requirement` when versions exist but none
// match the requirement. Both must be recognised, or the expected case is reported as a
// fatal dry-run failure.
fn is_unpublished_internal_dep(stderr: &str, internal_deps: &[&str]) -> bool {
    internal_deps.iter().any(|dep| {
        stderr.contains(&format!("no matching package named `{dep}` found"))
            || (stderr.contains(&format!(
                "failed to select a version for the requirement `{dep} = "
            )) && stderr.contains("location searched: crates.io index"))
    })
}

// Manifest-level checks: every publishable crate carries the workspace version, each
// crate depends on the previous one in publish order, and npm package versions match.
fn verify_publish_checks(root: &Path) -> Result<(), String> {
    let version = workspace_version(&root.join("Cargo.toml"))?;

    for (i, (crate_name, dir)) in PUBLISH_ORDER.iter().enumerate() {
        let manifest = root.join(dir).join("Cargo.toml");
        let text = fs::read_to_string(&manifest)
            .map_err(|e| format!("read {}: {}", e, manifest.display()))?;
        let value = parse_manifest(&text, &manifest)?;

        // A literal string version must equal the workspace version; a `version.workspace
        // = true` inheritance (dotted-key table) is fine by construction.
        match value.get("package").and_then(|p| p.get("version")) {
            Some(toml::Value::String(v)) => {
                if v.as_str() != version {
                    return Err(format!(
                        "{} version {} != workspace version {}",
                        crate_name, v, version
                    ));
                }
            }
            Some(toml::Value::Table(t))
                if t.get("workspace").and_then(|w| w.as_bool()) == Some(true) => {}
            Some(other) => {
                return Err(format!(
                    "{}: unsupported [package] version entry: {}",
                    manifest.display(),
                    other
                ));
            }
            None => {
                return Err(format!("{}: no [package] version", manifest.display()));
            }
        }

        // Dependency-order evidence: crate i must depend on crate i-1.
        if i > 0 {
            let (prev, _) = PUBLISH_ORDER[i - 1];
            let depends = value
                .get("dependencies")
                .and_then(|d| d.get(prev))
                .is_some();
            if !depends {
                return Err(format!(
                    "{} does not depend on {} — publish order is not dependency-justified",
                    crate_name, prev
                ));
            }
        }
    }

    // npm packages must carry the workspace version and repository too (same assertions
    // npm --version-sync makes).
    let packages_dir = root.join("packages");
    check_package_versions(
        &packages_dir,
        &version,
        &workspace_repository(&root.join("Cargo.toml"))?,
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// preflight [--dry-run]
// ---------------------------------------------------------------------------

// Fails closed: any check that cannot be completed (tool missing, network error,
// unexpected output) aborts preflight with a non-zero exit. There is no
// "prior verification" fallback — a release-day name collision is a ladder
// incident and must surface here, not be waved through.
fn preflight(args: &[String]) {
    let _dry_run = consume_flag(args, "--dry-run");

    println!("preflight: checking name availability (fails closed on errors)");

    let names_to_check = [
        ("cargo search", "mindctx"),
        ("cargo search", "mindctx-core"),
        ("cargo search", "mindctx-mcp"),
        ("npm view", "mindctx"),
        ("npm view", "@mindctx/darwin-arm64"),
        ("npm view", "@mindctx/darwin-x64"),
        ("npm view", "@mindctx/linux-x64"),
        ("npm view", "@mindctx/win32-x64"),
    ];

    let mut taken = Vec::new();
    let mut failures = Vec::new();

    for (tool, name) in &names_to_check {
        match run_availability_check(tool, name) {
            Ok(true) => println!("  {} {}: FREE", name, tool),
            Ok(false) => {
                println!("  {} {}: TAKEN", name, tool);
                taken.push((*tool, name.to_string()));
            }
            Err(err) => {
                println!("  {} {}: CHECK FAILED ({})", name, tool, err);
                failures.push(format!("{} {}: {}", name, tool, err));
            }
        }
    }

    if !taken.is_empty() {
        eprintln!("preflight: names taken: {:?}", taken);
        eprintln!("preflight: STOP and escalate to the owner.");
        std::process::exit(1);
    }

    if !failures.is_empty() {
        eprintln!(
            "preflight: {} availability check(s) failed — network or tool error. Preflight does not pass on unverified names; fix connectivity and rerun:",
            failures.len()
        );
        for f in &failures {
            eprintln!("  {}", f);
        }
        std::process::exit(1);
    }

    println!("preflight: all names free");
}

fn run_availability_check(tool: &str, name: &str) -> Result<bool, String> {
    let output = match tool {
        "cargo search" => Command::new("cargo")
            .args(["search", name, "--limit", "5"])
            .output()
            .map_err(|e| format!("failed to run cargo: {}", e))?,
        "npm view" => Command::new("npm")
            .args(["view", name])
            .output()
            .map_err(|e| format!("failed to run npm: {}", e))?,
        other => return Err(format!("unknown availability tool: {}", other)),
    };

    match tool {
        "npm view" => classify_npm_view(
            output.status.success(),
            &String::from_utf8_lossy(&output.stderr),
        ),
        "cargo search" => classify_cargo_search(
            output.status.success(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
            name,
        ),
        _ => unreachable!("tool was validated above"),
    }
}

// npm view: exit 0 = package exists (TAKEN); exit non-zero with npm error code E404
// = registry says not found (FREE); anything else = network/auth/tool failure.
fn classify_npm_view(success: bool, stderr: &str) -> Result<bool, String> {
    if success {
        return Ok(false);
    }
    if stderr.contains("E404") {
        return Ok(true);
    }
    let first_line = stderr.lines().next().unwrap_or("(no stderr)");
    Err(format!(
        "npm view failed without E404 (network, auth, or npm error): {}",
        first_line
    ))
}

// cargo search: exit 0; the first result line starts with the crate name, so a
// first word equal to the queried name means the crate is already published.
// Exit 0 with zero results prints nothing — empty stdout AND empty stderr means
// the name is free. Empty stdout with stderr content is treated as a check that
// did not really run (fail closed). Network failures exit non-zero.
fn classify_cargo_search(
    success: bool,
    stdout: &str,
    stderr: &str,
    name: &str,
) -> Result<bool, String> {
    if !success {
        return Err("cargo search exited non-zero (network or registry error)".to_string());
    }
    let first_word = stdout.split_whitespace().next().unwrap_or("");
    if first_word.is_empty() {
        if stderr.trim().is_empty() {
            return Ok(true);
        }
        let first_line = stderr.lines().next().unwrap_or("(no stderr)");
        return Err(format!(
            "cargo search produced no results but reported: {}",
            first_line
        ));
    }
    Ok(first_word != name)
}

// ---------------------------------------------------------------------------
// about [--dry-run]
// ---------------------------------------------------------------------------

// Third-party license disclosure via cargo-about. Requires the
// cargo-about plugin; --dry-run prints the plan without invoking anything.
fn about(args: &[String]) {
    let dry_run = consume_flag(args, "--dry-run");
    if dry_run {
        println!(
            "plan: cargo about generate --workspace about.hbs -> third-party-notices.html (config: about.toml)"
        );
        return;
    }

    match Command::new("cargo").args(["about", "--version"]).output() {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!(
                "xtask about: cargo-about is not usable (exit {}) — install with `cargo install cargo-about`",
                o.status
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("xtask about: failed to run cargo: {e}");
            std::process::exit(1);
        }
    }

    let output = Command::new("cargo")
        .args(["about", "generate", "--workspace", "about.hbs"])
        .output()
        .expect("cargo about generate failed");
    if !output.status.success() {
        eprintln!(
            "xtask about: cargo about generate failed ({})",
            output.status
        );
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        std::process::exit(1);
    }
    fs::write("third-party-notices.html", &output.stdout)
        .expect("write third-party notices failed");
    println!("xtask about: wrote third-party-notices.html");
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn consume_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn sha256_file(path: &Path) -> String {
    use sha2::Digest; // bring trait into scope for digest()
    use std::io::Read;
    let mut file = fs::File::open(path).expect("open file for sha256 failed");
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .expect("read file for sha256 failed");
    let hash = sha2::Sha256::digest(&buf);
    hex::encode(hash)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn test_workspace_version_parsing_toml() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        write(&manifest, "[workspace.package]\nversion = \"9.9.9\"\n");
        assert_eq!(workspace_version(&manifest).unwrap(), "9.9.9");
    }

    #[test]
    fn test_workspace_version_missing_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        write(&manifest, "[package]\nname = \"not-a-workspace\"\n");
        assert!(workspace_version(&manifest).is_err());
    }

    #[test]
    fn test_workspace_version_matches_real_manifest() {
        // No hardcoded version here: the real manifest must parse to a plausible semver,
        // including pre-release suffixes.
        let version = workspace_version(&workspace_root().join("Cargo.toml")).unwrap();
        semver::Version::parse(&version)
            .unwrap_or_else(|e| panic!("not semver: {} ({})", version, e));
    }

    #[test]
    fn test_version_sync_mismatch_detection() {
        let dir = tempfile::tempdir().unwrap();
        let packages = dir.path().join("packages");
        write(
            &packages.join("mindctx/package.json"),
            r#"{"name":"mindctx","version":"1.2.3","repository":{"url":"git+https://example.test/o/r.git"}}"#,
        );
        write(
            &packages.join("mindctx-darwin-arm64/package.json"),
            r#"{"name":"@mindctx/darwin-arm64","version":"9.9.9","repository":{"url":"git+https://example.test/o/r.git"}}"#,
        );
        let err =
            check_package_versions(&packages, "1.2.3", "https://example.test/o/r").unwrap_err();
        assert!(err.contains("@mindctx/darwin-arm64"), "{}", err);
        assert!(err.contains("9.9.9"), "{}", err);
    }

    #[test]
    fn test_version_sync_all_match() {
        let dir = tempfile::tempdir().unwrap();
        let packages = dir.path().join("packages");
        write(
            &packages.join("mindctx/package.json"),
            r#"{"name":"mindctx","version":"1.2.3","repository":{"url":"git+https://example.test/o/r.git"}}"#,
        );
        write(
            &packages.join("mindctx-darwin-arm64/package.json"),
            r#"{"name":"@mindctx/darwin-arm64","version":"1.2.3","repository":{"url":"git+https://example.test/o/r.git"}}"#,
        );
        check_package_versions(&packages, "1.2.3", "https://example.test/o/r").unwrap();
    }

    // A package without repository.url publishes everywhere except the registry, which
    // rejects it once provenance is attached. The manifest check has to catch it instead.
    #[test]
    fn test_version_sync_requires_matching_repository() {
        let dir = tempfile::tempdir().unwrap();
        let packages = dir.path().join("packages");
        write(
            &packages.join("mindctx/package.json"),
            r#"{"name":"mindctx","version":"1.2.3"}"#,
        );
        let err =
            check_package_versions(&packages, "1.2.3", "https://example.test/o/r").unwrap_err();
        assert!(err.contains("repository"), "{}", err);

        // A different repository is just as fatal, and the two spellings of the same
        // repository are not.
        write(
            &packages.join("mindctx/package.json"),
            r#"{"name":"mindctx","version":"1.2.3","repository":{"url":"git+https://example.test/o/other.git"}}"#,
        );
        assert!(check_package_versions(&packages, "1.2.3", "https://example.test/o/r").is_err());

        write(
            &packages.join("mindctx/package.json"),
            r#"{"name":"mindctx","version":"1.2.3","repository":{"url":"git+https://example.test/o/r.git/"}}"#,
        );
        check_package_versions(&packages, "1.2.3", "https://example.test/o/r").unwrap();
    }

    #[test]
    fn test_staging_copies_dist_bins_executable() {
        let dir = tempfile::tempdir().unwrap();
        let packages = dir.path().join("packages");
        let dist = dir.path().join("dist");

        write(
            &packages.join("mindctx/package.json"),
            r#"{"name":"mindctx","version":"0.0.1"}"#,
        );
        for (target, pkg) in PLATFORM_TARGETS {
            write(
                &packages.join(pkg).join("package.json"),
                r#"{"name":"x","version":"0.0.1"}"#,
            );
            write(
                &dist.join(target).join(staged_binary_name(target)),
                "#!/bin/sh\nexit 0\n",
            );
        }

        stage_platform_bins(&packages, &dist).unwrap();

        for (target, pkg) in PLATFORM_TARGETS {
            // The packaged bin must carry the launcher-expected name, not the staged one
            //.
            let staged = packages
                .join(pkg)
                .join("bin")
                .join(packaged_binary_name(target));
            assert!(staged.exists(), "{} missing", staged.display());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&staged).unwrap().permissions().mode();
                assert_ne!(mode & 0o111, 0, "{} not executable", staged.display());
            }
        }
    }

    /// Release-asset names stay target-qualified (unique in the flat GitHub Release
    /// namespace) while the packaged bin keeps the launcher-expected plain name.
    #[test]
    fn test_staged_names_are_target_qualified_and_packaged_names_are_plain() {
        let mut staged_names: Vec<String> = Vec::new();
        for (target, _) in PLATFORM_TARGETS {
            let staged = staged_binary_name(target);
            assert!(
                staged.starts_with("mindctx-") && staged.contains(target),
                "{} not target-qualified",
                staged
            );
            let packaged = packaged_binary_name(target);
            assert_eq!(
                staged.ends_with(".exe"),
                packaged.ends_with(".exe"),
                "{} must carry the same extension as the packaged binary {}",
                staged,
                packaged
            );
            staged_names.push(staged);
        }
        staged_names.sort();
        staged_names.dedup();
        assert_eq!(
            staged_names.len(),
            PLATFORM_TARGETS.len(),
            "staged release-asset names must be unique across the matrix"
        );
    }

    #[test]
    fn test_staging_fails_without_dist_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let packages = dir.path().join("packages");
        for (target, pkg) in PLATFORM_TARGETS {
            let _ = target;
            write(
                &packages.join(pkg).join("package.json"),
                r#"{"name":"x","version":"0.0.1"}"#,
            );
        }
        let err = stage_platform_bins(&packages, &dir.path().join("empty-dist")).unwrap_err();
        assert!(err.contains("missing dist artifact"), "{}", err);
    }

    #[test]
    fn test_verify_publish_checks_manifest_order_and_versions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("Cargo.toml"),
            "[workspace.package]\nversion = \"1.2.3\"\nrepository = \"https://example.test/o/r\"\n",
        );
        write(
            &root.join("crates/core/Cargo.toml"),
            "[package]\nname = \"mindctx-core\"\nversion = \"1.2.3\"\n",
        );
        write(
            &root.join("crates/mcp/Cargo.toml"),
            "[package]\nname = \"mindctx-mcp\"\nversion = \"1.2.3\"\n\n[dependencies]\nmindctx-core = { workspace = true }\n",
        );
        write(
            &root.join("crates/cli/Cargo.toml"),
            "[package]\nname = \"mindctx\"\nversion = \"1.2.3\"\n\n[dependencies]\nmindctx-mcp = { workspace = true }\n",
        );
        write(
            &root.join("packages/mindctx/package.json"),
            r#"{"name":"mindctx","version":"1.2.3","repository":{"url":"git+https://example.test/o/r.git"}}"#,
        );
        verify_publish_checks(root).unwrap();
    }

    #[test]
    fn test_verify_publish_checks_reject_version_drift() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("Cargo.toml"),
            "[workspace.package]\nversion = \"1.2.3\"\nrepository = \"https://example.test/o/r\"\n",
        );
        write(
            &root.join("crates/core/Cargo.toml"),
            "[package]\nname = \"mindctx-core\"\nversion = \"0.0.9\"\n",
        );
        let err = verify_publish_checks(root).unwrap_err();
        assert!(err.contains("!= workspace version"), "{}", err);
    }

    #[test]
    fn test_unpublished_internal_dep_detection() {
        let stderr = "error: failed to prepare local package for uploading\n\nCaused by:\n  no matching package named `mindctx-core` found\n  location searched: crates.io index\n";
        assert!(is_unpublished_internal_dep(
            stderr,
            &["mindctx-core", "mindctx-mcp"]
        ));
        assert!(!is_unpublished_internal_dep(
            "error: failed to verify byte-compiled something else",
            &["mindctx-core"]
        ));
        assert!(!is_unpublished_internal_dep(stderr, &[]));
    }

    // The dependency is on crates.io, but only at versions the requirement excludes, so cargo
    // reports a version-selection failure instead of a missing package. This is the wording
    // `cargo publish --dry-run -p mindctx-mcp` actually produces before mindctx-core has ever
    // been published at the workspace version, and the previous revision of this check did not
    // recognise it: a normal pre-first-publish state was reported as a fatal dry-run failure.
    #[test]
    fn test_unpublished_internal_dep_detection_version_selection_failure() {
        let stderr = "error: failed to prepare local package for uploading\n\nCaused by:\n  failed to select a version for the requirement `mindctx-core = \"^0.1.0\"`\n  candidate versions found which didn't match: 0.0.1-alpha.2, 0.0.1-alpha.1\n  location searched: crates.io index\n  required by package `mindctx-mcp v0.1.0`\n";
        assert!(is_unpublished_internal_dep(stderr, &["mindctx-core"]));
        assert!(is_unpublished_internal_dep(
            stderr,
            &["mindctx-core", "mindctx-mcp"]
        ));
        // A different dependency's version conflict is not this crate's expected condition.
        assert!(!is_unpublished_internal_dep(stderr, &["mindctx"]));
    }

    // The crates.io location is load-bearing: only a dependency that cannot be resolved from
    // the registry is the expected pre-first-publish case. Any other source failing to resolve
    // is a real misconfiguration and must still fail the dry-run.
    #[test]
    fn test_unpublished_internal_dep_requires_registry_location() {
        let stderr = "error: failed to prepare local package for uploading\n\nCaused by:\n  failed to select a version for the requirement `mindctx-core = \"^0.1.0\"`\n  location searched: some other source\n";
        assert!(!is_unpublished_internal_dep(stderr, &["mindctx-core"]));
    }

    #[test]
    fn test_classify_npm_view() {
        assert!(!classify_npm_view(true, "").unwrap());
        assert!(classify_npm_view(false, "npm error code E404\nnpm error 404 Not Found").unwrap());
        assert!(classify_npm_view(false, "npm error network timeout").is_err());
    }

    #[test]
    fn test_classify_cargo_search() {
        assert!(!classify_cargo_search(true, "mindctx = \"0.0.1\"\n", "", "mindctx").unwrap());
        assert!(classify_cargo_search(true, "mindctx-cli = \"0.1.0\"\n", "", "mindctx").unwrap());
        // Zero results with clean exit = free (cargo prints nothing on no match).
        assert!(classify_cargo_search(true, "", "", "mindctx").unwrap());
        // Zero results but stderr output = did not really run, fail closed.
        assert!(classify_cargo_search(true, "", "warning: something odd", "mindctx").is_err());
        assert!(classify_cargo_search(false, "", "", "mindctx").is_err());
    }

    #[test]
    fn test_unknown_availability_tool_is_error() {
        // Unknown tools must fail closed, not default to FREE.
        assert!(run_availability_check("dig", "mindctx").is_err());
    }

    #[test]
    fn test_sha256_hex_encoding() {
        use hex::encode;
        use sha2::Digest;
        use sha2::Sha256;
        let hash = Sha256::digest(b"test");
        let hex_str = encode(hash);
        assert_eq!(hex_str.len(), 64);
        assert_eq!(&hex_str[..8], "9f86d081");
    }

    #[test]
    fn test_consume_flag() {
        assert!(consume_flag(&["--dry-run".to_string()], "--dry-run"));
        assert!(!consume_flag(&["--version-sync".to_string()], "--dry-run"));
        assert!(!consume_flag(&[], "--dry-run"));
    }

    #[test]
    fn test_replace_json_string_field_basic() {
        let text = r#"{"name": "x", "version": "1.2.3"}"#;
        let out = replace_json_string_field(text, "version", "1.2.3", "1.2.4").unwrap();
        assert_eq!(out, r#"{"name": "x", "version": "1.2.4"}"#);
    }

    #[test]
    fn test_replace_json_string_field_preserves_other_fields() {
        let text = r#"{"name": "x", "version": "0.0.1", "license": "MIT"}"#;
        let out = replace_json_string_field(text, "version", "0.0.1", "0.0.2").unwrap();
        assert_eq!(
            out,
            r#"{"name": "x", "version": "0.0.2", "license": "MIT"}"#
        );
    }

    #[test]
    fn test_replace_json_string_field_returns_none_on_miss() {
        let text = r#"{"name": "x", "version": "0.0.1"}"#;
        assert!(replace_json_string_field(text, "version", "9.9.9", "0.0.2").is_none());
        assert!(replace_json_string_field(text, "missing", "0.0.1", "0.0.2").is_none());
    }

    #[test]
    fn test_update_platform_package_json() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("package.json");
        write(
            &pkg,
            r#"{"name": "@mindctx/darwin-arm64", "version": "1.2.3", "license": "MIT"}"#,
        );
        update_platform_package_json(&pkg, "1.2.3", "1.2.4").unwrap();
        assert_eq!(
            fs::read_to_string(&pkg).unwrap(),
            r#"{"name": "@mindctx/darwin-arm64", "version": "1.2.4", "license": "MIT"}"#
        );
    }

    #[test]
    fn test_update_launcher_package_json_updates_optional_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir.path();
        write(
            &pkg_dir.join("package.json"),
            r#"{"name": "mindctx", "version": "1.2.3", "optionalDependencies": {"@mindctx/darwin-arm64": "1.2.3", "@mindctx/darwin-x64": "1.2.3", "@mindctx/linux-x64": "1.2.3", "@mindctx/win32-x64": "1.2.3"}}"#,
        );
        update_launcher_package_json(pkg_dir, "1.2.3", "1.2.4").unwrap();
        let updated = fs::read_to_string(pkg_dir.join("package.json")).unwrap();
        assert!(updated.contains("\"version\": \"1.2.4\""), "{}", updated);
        for platform in [
            "@mindctx/darwin-arm64",
            "@mindctx/darwin-x64",
            "@mindctx/linux-x64",
            "@mindctx/win32-x64",
        ] {
            assert!(
                updated.contains(&format!("\"{}\": \"1.2.4\"", platform)),
                "{} missing pin",
                platform
            );
        }
        assert!(
            !updated.contains("1.2.3"),
            "leftover old version in {}",
            updated
        );
    }

    #[test]
    fn test_update_launcher_package_json_rejects_unexpected_pin_value() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir.path();
        write(
            &pkg_dir.join("package.json"),
            r#"{"name": "mindctx", "version": "1.2.3", "optionalDependencies": {"@mindctx/darwin-arm64": "9.9.9", "@mindctx/darwin-x64": "1.2.3", "@mindctx/linux-x64": "1.2.3", "@mindctx/win32-x64": "1.2.3"}}"#,
        );
        let err = update_launcher_package_json(pkg_dir, "1.2.3", "1.2.4").unwrap_err();
        assert!(err.contains("@mindctx/darwin-arm64"), "{}", err);
        assert!(err.contains("9.9.9"), "{}", err);
    }

    #[test]
    fn test_update_launcher_package_json_skips_already_current_pins() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir.path();
        write(
            &pkg_dir.join("package.json"),
            r#"{"name": "mindctx", "version": "1.2.3", "optionalDependencies": {"@mindctx/darwin-arm64": "1.2.4", "@mindctx/darwin-x64": "1.2.3", "@mindctx/linux-x64": "1.2.3", "@mindctx/win32-x64": "1.2.3"}}"#,
        );
        update_launcher_package_json(pkg_dir, "1.2.3", "1.2.4").unwrap();
        let updated = fs::read_to_string(pkg_dir.join("package.json")).unwrap();
        assert!(updated.contains("\"version\": \"1.2.4\""), "{}", updated);
        assert!(
            updated.contains("\"@mindctx/darwin-arm64\": \"1.2.4\""),
            "{}",
            updated
        );
        assert!(
            !updated.contains("@mindctx/darwin-x64\": \"1.2.3\""),
            "{}",
            updated
        );
    }

    #[test]
    fn test_update_cargo_toml_version_writes_three_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cargo = dir.path().join("Cargo.toml");
        write(
            &cargo,
            r#"[workspace.package]
version = "1.2.3"
edition = "2024"

[workspace.dependencies]
mindctx-core = { path = "crates/core", version = "1.2.3" }
mindctx-mcp = { path = "crates/mcp", version = "1.2.3" }
serde = "1"
"#,
        );
        update_cargo_toml_version(&cargo, "1.2.4").unwrap();
        let updated = fs::read_to_string(&cargo).unwrap();
        // [workspace.package].version
        assert!(
            updated.contains("version = \"1.2.4\""),
            "missing workspace version: {}",
            updated
        );
        // Both internal path-dep mirrors bumped, edition + unrelated dep left alone
        assert!(
            updated.contains("mindctx-core = { path = \"crates/core\", version = \"1.2.4\" }"),
            "{}",
            updated
        );
        assert!(
            updated.contains("mindctx-mcp = { path = \"crates/mcp\", version = \"1.2.4\" }"),
            "{}",
            updated
        );
        assert!(updated.contains("edition = \"2024\""), "{}", updated);
        assert!(updated.contains("serde = \"1\""), "{}", updated);
        // No leftover old version
        assert!(!updated.contains("1.2.3"), "{}", updated);
    }

    #[test]
    fn test_update_cargo_toml_version_rejects_missing_section() {
        let dir = tempfile::tempdir().unwrap();
        let cargo = dir.path().join("Cargo.toml");
        write(&cargo, r#"[package]"#);
        let err = update_cargo_toml_version(&cargo, "0.0.2").unwrap_err();
        assert!(err.contains("[workspace.package]"), "{}", err);
    }
}
