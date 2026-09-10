#!/usr/bin/env bash
# verify-npm-trust.sh -- confirm npm Trusted Publishing is configured for every package that
# still needs publishing, without publishing anything.
#
# `npm publish` exchanges a GitHub OIDC token per package at
# POST /-/npm/v1/oidc/token/exchange/package/<name>. The exchange succeeds only when the
# package carries a trusted publisher entry whose claims match this run -- repository,
# workflow file, and environment. Doing the exchange up front turns a mid-release ENEEDAUTH
# into a named list before anything is written to a registry.
#
# Must run from the same workflow file and environment the trusted publisher was configured
# for: the workflow file and environment are part of the claims being verified, so running
# this from a different workflow proves nothing. GitHub Actions only, which is why
# release.yml exposes it under workflow_dispatch as an on-demand check.
#
# Reads the state file written by release-probe.sh.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

STATE_FILE="$REPO_ROOT/release-state.json"

while [ $# -gt 0 ]; do
  case "$1" in
    --state-file) STATE_FILE="$2"; shift 2 ;;
    -h | --help)
      echo "usage: verify-npm-trust.sh [--state-file PATH]" >&2
      exit 0
      ;;
    *)
      echo "verify-npm-trust.sh: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [ ! -f "$STATE_FILE" ]; then
  echo "verify-npm-trust.sh: $STATE_FILE not found; run release-probe.sh first" >&2
  exit 1
fi

PENDING="$(jq -r '.npms | to_entries[] | select(.value == false) | .key' "$STATE_FILE")"
if [ -z "$PENDING" ]; then
  echo "every npm package already carries the probed version -- no exchange needed"
  exit 0
fi

if [ -z "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" ] || [ -z "${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-}" ]; then
  echo "verify-npm-trust.sh: the GitHub OIDC request environment is absent." >&2
  echo "This check needs a GitHub Actions run with 'id-token: write'." >&2
  exit 1
fi

SEP='&'
case "$ACTIONS_ID_TOKEN_REQUEST_URL" in
  *\?*) ;;
  *) SEP='?' ;;
esac

ID_TOKEN="$(curl -sf --retry 3 --retry-delay 1 --retry-connrefused --max-time 20 \
  -H "User-Agent: actions/oidc-client" \
  -H "Authorization: Bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
  "${ACTIONS_ID_TOKEN_REQUEST_URL}${SEP}audience=npm:registry.npmjs.org" \
  | jq -r '.value // empty' || true)"
if [ -z "$ID_TOKEN" ]; then
  echo "verify-npm-trust.sh: could not obtain a GitHub OIDC token for audience npm:registry.npmjs.org" >&2
  exit 1
fi

# Claim values reported on failure, so the missing entry can be created without guessing.
REPOSITORY="${GITHUB_REPOSITORY:-unknown}"
WORKFLOW_FILE="${GITHUB_WORKFLOW_REF:-unknown}"
WORKFLOW_FILE="${WORKFLOW_FILE%%@*}"
WORKFLOW_FILE="${WORKFLOW_FILE##*/}"
ENVIRONMENT="${TRUSTED_PUBLISHER_ENVIRONMENT:-release}"

failed=()
unreachable=()
while IFS= read -r p; do
  [ -n "$p" ] || continue
  escaped="$(jq -rn --arg s "$p" '$s|@uri')"
  body="$(curl -s -X POST -H "Authorization: Bearer $ID_TOKEN" \
    "https://registry.npmjs.org/-/npm/v1/oidc/token/exchange/package/$escaped" || true)"
  if [ -n "$(jq -r '.token // empty' <<< "$body" 2>/dev/null)" ]; then
    echo "  $p: trusted publisher OK"
  elif [ -n "$(jq -r '.message // empty' <<< "$body" 2>/dev/null)" ]; then
    # A JSON error body is the registry refusing the exchange: the trusted publisher entry
    # is missing or its claims do not match this run.
    echo "  $p: exchange refused -- $(jq -r '.message' <<< "$body")"
    failed+=("$p")
  else
    # Empty or non-JSON response: the registry was not reached. Same verdict, different fix.
    echo "  $p: no response from the npm registry"
    unreachable+=("$p")
  fi
done <<< "$PENDING"

if [ "${#unreachable[@]}" -gt 0 ]; then
  echo "verify-npm-trust.sh: the npm registry did not answer for: ${unreachable[*]}" >&2
  echo "Nothing was published. Check connectivity and re-run." >&2
  exit 1
fi

if [ "${#failed[@]}" -gt 0 ]; then
  echo "verify-npm-trust.sh: npm Trusted Publishing is not configured for: ${failed[*]}" >&2
  echo "Nothing was published. On registry.npmjs.org, add a trusted publisher for each" >&2
  echo "package naming repository $REPOSITORY, workflow $WORKFLOW_FILE, environment $ENVIRONMENT." >&2
  exit 1
fi

echo "all pending npm packages can mint a publish token"
