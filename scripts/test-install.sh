#!/bin/bash
# Behavioral smoke for install.sh / uninstall.sh marker + config logic.
#
# Dot-sources the real functions and drives them against a fake HOME, so no network, host CLI,
# or real ~/.mindctx is touched. Asserts the properties that matter: idempotency, ownership-rule
# preservation, and prune-empty. Run locally or from CI (`bash scripts/test-install.sh`).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home"
mkdir -p "$HOME"

# Dot-source the functions without running main().
sed '$d' "$HERE/install.sh" > "$WORK/install-funcs.sh"
sed '$d' "$HERE/uninstall.sh" > "$WORK/uninstall-funcs.sh"
# shellcheck disable=SC1091
source "$WORK/install-funcs.sh"
# shellcheck disable=SC1091
source "$WORK/uninstall-funcs.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }
count() { grep -c "$1" "$2" 2>/dev/null || echo 0; }

TPL="$HERE/agent-prompt.md"
[ -f "$TPL" ] || fail "missing $TPL"

# --- write_core_config: writes a marked [core], idempotent, preserves user sections ---
write_core_config >/dev/null
grep -qxF "$CORE_BEGIN" "$HOME/.mindctx/config.toml" || fail "core begin marker missing"
grep -qxF "$CORE_END"   "$HOME/.mindctx/config.toml" || fail "core end marker missing"
grep -qxF '[core]'      "$HOME/.mindctx/config.toml" || fail "core table missing"
write_core_config >/dev/null
[ "$(count '^\[core\]$' "$HOME/.mindctx/config.toml")" = "1" ] || fail "core duplicated on re-write"
printf '\n[user_section]\nkeep = "me"\n' >> "$HOME/.mindctx/config.toml"
write_core_config >/dev/null
grep -qxF '[user_section]' "$HOME/.mindctx/config.toml" || fail "user section dropped on re-write"

# --- upsert_prompt_block: writes block, idempotent, preserves user content ---
upsert_prompt_block "$HOME/.claude/CLAUDE.md" "$TPL" >/dev/null
grep -qxF "$PROMPT_BEGIN" "$HOME/.claude/CLAUDE.md" || fail "prompt begin marker missing"
upsert_prompt_block "$HOME/.claude/CLAUDE.md" "$TPL" >/dev/null
[ "$(count "$PROMPT_BEGIN" "$HOME/.claude/CLAUDE.md")" = "1" ] || fail "prompt block duplicated on re-upsert"
printf '# my note\n' >> "$HOME/.claude/CLAUDE.md"
upsert_prompt_block "$HOME/.claude/CLAUDE.md" "$TPL" >/dev/null
grep -qxF '# my note' "$HOME/.claude/CLAUDE.md" || fail "user content dropped on re-upsert"

# --- strip_block_and_prune: strips only the block, preserves user content, prunes empty ---
strip_block_and_prune "$HOME/.claude/CLAUDE.md" "$PROMPT_BEGIN" "$PROMPT_END" >/dev/null
grep -qxF '# my note' "$HOME/.claude/CLAUDE.md" || fail "strip dropped user content"
if grep -qxF "$PROMPT_BEGIN" "$HOME/.claude/CLAUDE.md"; then fail "strip left the begin marker"; fi
printf '%s\n\n%s\n' "$PROMPT_BEGIN" "$PROMPT_END" > "$WORK/only.md"
strip_block_and_prune "$WORK/only.md" "$PROMPT_BEGIN" "$PROMPT_END" >/dev/null
[ ! -f "$WORK/only.md" ] || fail "block-only file not pruned"

echo "PASS: install/uninstall behavioral smoke"
