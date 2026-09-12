#!/bin/bash
# One-line uninstaller for mindctx (macOS, Linux, WSL).
#
# Reverses what the installer did: strips the marker blocks it wrote, removes the MCP
# registration through the host CLIs, and deletes only its own binary. Content outside those
# blocks is never touched. Pass --purge to also drop ~/.mindctx/record/ (receipts left behind
# by the removed `mindctx apply`), which is kept by default.
set -euo pipefail

# Markers delimit the chunks the installer owns. Prompt blocks live in markdown files (HTML
# comments); the [core] block lives in TOML and uses comments — HTML comments are not TOML.
PROMPT_BEGIN='<!-- mindctx:begin -->'
PROMPT_END='<!-- mindctx:end -->'
CORE_BEGIN='# mindctx:begin:core'
CORE_END='# mindctx:end:core'

usage() {
  cat <<'EOF'
Uninstall mindctx.

Usage: uninstall.sh [--purge]

  --purge   also delete ~/.mindctx/record/ (receipts and backups from `mindctx apply`)
EOF
}

# Strip every inclusive begin..end block, in place. Markers are matched as whole lines, so
# only blocks the installer wrote are removed, and an unterminated block is left alone rather
# than truncating the file.
strip_marked_block() {
  local target="$1" begin="$2" end="$3"
  local tmp="$target.mindctx-tmp"

  [ -f "$target" ] || return 0

  while grep -qxF "$begin" "$target"; do
    if ! grep -qxF "$end" "$target"; then
      echo "warning: $target has an unterminated $begin block — left as-is" >&2
      return 0
    fi
    awk -v begin="$begin" -v end="$end" '
      $0 == begin { skip = 1; next }
      skip { if ($0 == end) skip = 0; next }
      { print }
    ' "$target" > "$tmp"
    mv "$tmp" "$target"
  done
}

# Drop trailing blank lines left where the block used to be, then remove the file when nothing
# else was in it (the installer creates these files when the host has none).
strip_block_and_prune() {
  local target="$1" begin="$2" end="$3" tmp="$1.mindctx-tmp"

  [ -f "$target" ] || return 0

  strip_marked_block "$target" "$begin" "$end"
  if [ ! -s "$target" ]; then
    rm -f "$target"
    echo "Removed $target (no content left after stripping the mindctx block)"
    return 0
  fi

  awk '
    { line[NR] = $0 }
    END {
      last = NR
      while (last > 0 && line[last] == "") last--
      for (i = 1; i <= last; i++) print line[i]
    }
  ' "$target" > "$tmp"
  mv "$tmp" "$target"

  if [ -s "$target" ]; then
    echo "Stripped the mindctx block from $target"
  else
    rm -f "$target"
    echo "Removed $target (no content left after stripping the mindctx block)"
  fi
}

main() {
  local purge=0 arg

  for arg in "$@"; do
    case "$arg" in
      --purge) purge=1 ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        echo "Unknown option: $arg" >&2
        usage >&2
        exit 2
        ;;
    esac
  done

  # Configuration cleanup first: the host CLIs below may rewrite their own config files, and
  # the marker blocks live in plain files next to them.
  strip_block_and_prune "$HOME/.claude/CLAUDE.md" "$PROMPT_BEGIN" "$PROMPT_END"
  strip_block_and_prune "$HOME/.codex/AGENTS.md" "$PROMPT_BEGIN" "$PROMPT_END"
  strip_block_and_prune "$HOME/.mindctx/config.toml" "$CORE_BEGIN" "$CORE_END"

  # Remove MCP registrations (idempotent — don't fail if not registered)
  if command -v claude >/dev/null 2>&1; then
    if claude mcp remove mindctx --scope user 2>/dev/null; then
      echo "Removed mindctx MCP server from Claude Code"
    fi
  fi
  if command -v codex >/dev/null 2>&1; then
    if codex mcp remove mindctx 2>/dev/null; then
      echo "Removed mindctx MCP server from Codex"
    fi
  fi

  local target="$HOME/.local/bin/mindctx"
  if [ -f "$target" ]; then
    rm -f "$target"
    echo "Removed $target"
  else
    echo "mindctx is not installed (no file at $target)" >&2
  fi

  if [ "$purge" -eq 1 ]; then
    if [ -e "$HOME/.mindctx/record" ]; then
      rm -rf "$HOME/.mindctx/record"
      echo "Purged $HOME/.mindctx/record"
    else
      echo "Nothing to purge at $HOME/.mindctx/record" >&2
    fi
  fi

  exit 0
}

main "$@"
