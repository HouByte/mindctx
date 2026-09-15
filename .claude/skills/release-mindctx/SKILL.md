---
name: release-mindctx
description: Use when cutting a new mindctx release — bumps workspace version via xtask release, promotes the CHANGELOG [Unreleased] section, opens the release PR, and tags v* on main. Covers the release/* branch flow per AGENTS.md "PR with rebase-merge".
---

# release-mindctx

Mechanical pipeline for a mindctx release. Follows AGENTS.md "PR with rebase-merge" + CI-on-PR-only rules. Every step assumes `source scripts/activate.sh` has been run.

## Inputs to confirm with the user up front

Do not proceed until each is settled (these are user-decision, not for the agent to guess):

- **Version** (`<ver>`): semver, must differ from current `workspace.package.version`.
- **Tag message** (`<tag-msg>`): one short line, ends up as the annotated tag body and the GitHub Release title.
- **PR title / body**: 1-line conventional subject + bullets; body should mention the user-visible change buckets.

## Branch precondition

Working tree must be clean and `origin/main` must be the parent. If the prior release PR is still open or main is behind, stop and resolve first — `xtask release` only accepts `main` or `release/*` as the source branch (`crates/xtask/src/main.rs:212`).

## Steps

### 1. Create `release/<ver>` from `origin/main`

```bash
git checkout main
git fetch origin
git checkout -b release/<ver> origin/main
```

Do **not** touch local `main` (auto-mode denies direct pushes to the default branch; also AGENTS.md wants PR as the merge gate).

### 2. Promote CHANGELOG `[Unreleased]` → `[<ver>]`

Edit `CHANGELOG.md`: rename the topmost `## [Unreleased]` heading to `## [<ver>]`, and insert a fresh empty `## [Unreleased]` line above it. If the section body is empty, skip the promote (a no-content [Unreleased] section is fine to leave alone; do not invent entries).

```bash
git add CHANGELOG.md
git commit -m "docs: promote [Unreleased] to [<ver>]" \
  -m "The release workflow reads the GitHub Release body from the CHANGELOG section matching the tag, so the heading has to exist before the tag is pushed."
```

### 3. Bump versions via `xtask release`

```bash
cargo run -p xtask -- release <ver> --tag-msg "<tag-msg>"
```

`xtask` rewrites `Cargo.toml` (`workspace.package.version` + the two internal path-dep mirrors), every `packages/*/package.json`, and the launcher's `optionalDependencies` pins. It runs `xtask npm --version-sync --dry-run` as a self-check before committing. Output ends with:

- on `release/*`: a single `chore(release): bump to <ver>` commit and a 4-step "next steps" list — **do not** `git tag` yet.
- on `main`: same commit + a local `v<ver>` tag (not the path we want; see step 7).

### 4. Pre-PR gate on the release branch

```bash
cargo build --workspace   # rebuild target/<profile>/mindctx BEFORE test
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

**Why the explicit `cargo build --workspace` first**: the CLI crate is bin-only and not in the test dep graph of `mindctx-tests`. After the bump, `cargo test --workspace` alone leaves the previous `target/debug/mindctx` binary in place, so `cli_mcp_roundtrip` spawns a subprocess whose `serverInfo.version` still reports the previous version — 11 contract tests fail with `single version source` mismatches. CI is immune because `.github/workflows/ci.yml` already runs `cargo build --workspace` before `cargo test --workspace` (lines 136-139). Local dev is not.

If any step is red, stop. `git reset --hard HEAD~1` drops the bump commit if the failure is in step 3's output; debug from there.

### 5. Commit `Cargo.lock`

`xtask release` only edits `Cargo.toml` and `packages/*/package.json`. The bump triggers a stale `Cargo.lock` whose version fields still point at the old release. CI's `cargo build` rewrites it on a fresh checkout, but committing the lock update here keeps the release PR self-contained.

```bash
git add Cargo.lock
git commit -m "chore: update Cargo.lock for <ver>" \
  -m "Lock file version fields trail Cargo.toml after cargo build --workspace; CI fresh checkouts need this to resolve against <ver>."
```

Verify only `Cargo.lock` is staged (5-line version-only diff is the expected shape).

### 6. Push branch and open the PR

```bash
git push -u origin release/<ver>
gh pr create --base main --head release/<ver> \
  --title "chore(release): cut <ver>" \
  --body "<pr-body>"
```

PR body should call out user-visible change buckets (not commit lists). This is the release gate — every CI scope runs because the `release/*` branch detection at `ci.yml:49` lists every tracked file.

### 7. Wait for CI, then rebase-merge

`ci.yml` runs fmt / clippy / test matrix (3 OS) / wsl-path / deny / comment-language / install-scripts / npm-package-smoke on `release/*` PRs. The npm-package-smoke job (lines 231+) only triggers for `release/*` PRs and runs the slowest gate — budget accordingly.

After CI is green, merge via the GitHub "Rebase and merge" button so `main` fast-forwards to the PR head. Do **not** squash or merge-commit — the `chore(release): bump` commit must remain identifiable as the version-source point.

### 8. Tag `v<ver>` on `main` and push

```bash
git checkout main
git pull --ff-only
git tag -a v<ver> -m "<tag-msg>"
git push origin v<ver>
```

`v*` tag push is the publish trigger (`ci.yml` "v* tag that follows only publishes"). The `cd` + `git pull` is required because main moved forward during the merge; tagging without fast-forwarding would put the tag on the wrong commit and publish a release main never had.

## Rollback map

| Step | Where things go wrong | How to undo |
|---|---|---|
| 2 | Promote commit has wrong content | `git commit --amend` before pushing |
| 3 | `xtask release` errors (version drift, dirty tree, wrong branch) | The commit is local-only; `git reset --hard HEAD~1`, fix, retry |
| 4 | Gate fails | Stay on branch, fix, re-run the gate |
| 5 | Cargo.lock picked up unintended edits | `git restore --staged Cargo.lock && git checkout -- Cargo.lock`, then rebuild |
| 6 | PR opened with wrong base or body | `gh pr close <n>` and reopen |
| 7 | CI red | Push follow-up commits; do **not** force-push the branch (the tag is the trigger, not the merge commit) |
| 8 | Tag pushed but wrong commit | Delete local + remote: `git tag -d v<ver> && git push origin :refs/tags/v<ver>` — but **only if the publish pipeline has not already picked it up**. Once GitHub Release is published, treat it as a real release (see memory `prerelease-channel.md` for the deprecation ladder) |

## Things this skill will not do

- Pick the version number (semver is a user call).
- Decide patch vs minor vs major (read CHANGELOG sections for the change shape; ask the user).
- Generate the GitHub Release notes body (CI reads them from the CHANGELOG section that step 2 created — keep that section in good shape).
- Run `xtask dist` / `xtask npm` / `xtask verify-publish` — those are publish-pipeline ops, not release-prep ops.
- Bump dependency versions (`cargo update`, Dependabot territory).

## Quick checklist

- [ ] Version decided, tag-msg written, PR body drafted
- [ ] `git status` clean on main, `origin/main` is the parent
- [ ] `release/<ver>` branched from `origin/main`
- [ ] CHANGELOG promoted + committed
- [ ] `xtask release` clean, single bump commit
- [ ] `cargo build --workspace && cargo test --workspace` green
- [ ] `cargo clippy -D warnings` + `cargo fmt --check` green
- [ ] Cargo.lock committed (5-line version-only diff)
- [ ] Branch pushed, PR opened, all CI scopes green
- [ ] Rebase-merged to main
- [ ] `v<ver>` tagged on updated main and pushed
