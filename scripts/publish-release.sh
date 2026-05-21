#!/usr/bin/env bash
# One-shot corpus publication using the schema-5 db-artifact flow.
#
# Pipeline:
#   1. Verify a built corpus exists at $RELEASE_DIR/ato.db
#   2. `ato-mcp package-corpus` strips FTS5 from a copy, zstd-compresses to
#      $RELEASE_DIR/ato.db.zst, and rewrites $RELEASE_DIR/manifest.json so
#      `db: {url, sha256, size}` points at the new artifact
#   3. `ato-mcp publish-release` rewrites manifest URLs to GitHub release URLs
#      and uploads manifest.json + ato.db.zst to the named tag
#
# Prereqs:
#   - ato-mcp build has finished; release/ contains ato.db and manifest.json
#   - target/release/ato-mcp built (or ATO_MCP_BIN points at one)
#   - optional ATO_MCP_MODEL_URL for an approved model mirror
#   - ATO_MCP_MODEL_SHA256 and ATO_MCP_MODEL_SIZE when that mirror is not
#     a Hugging Face source
#   - optional ATO_MCP_SIGN_KEY for manifest signing (minisign secret key)
#   - optional ATO_MCP_ZSTD_LEVEL to override default 19
#   - gh authenticated for the maintainer account
#
# Usage:
#   scripts/publish-release.sh v0.8.0
#   scripts/publish-release.sh v0.8.0 gunba/ato-mcp
set -euo pipefail

TAG="${1:?usage: publish-release.sh <tag> [owner/repo]}"
REPO="${2:-${ATO_MCP_GH_REPO:-gunba/ato-mcp}}"
REPO_DIR="${ATO_MCP_REPO_DIR:-$(pwd)}"
RELEASE_DIR="${ATO_MCP_RELEASE_DIR:-$REPO_DIR/release}"
ATO_MCP_BIN="${ATO_MCP_BIN:-$REPO_DIR/target/release/ato-mcp}"
MODEL_URL="${ATO_MCP_MODEL_URL:-}"
MODEL_SHA256="${ATO_MCP_MODEL_SHA256:-}"
MODEL_SIZE="${ATO_MCP_MODEL_SIZE:-}"
SIGN_KEY="${ATO_MCP_SIGN_KEY:-}"
ZSTD_LEVEL="${ATO_MCP_ZSTD_LEVEL:-19}"

if [ ! -x "$ATO_MCP_BIN" ]; then
  echo "ato-mcp binary not found at $ATO_MCP_BIN" >&2
  echo "Build it first: cargo build --release" >&2
  exit 1
fi
if [ ! -f "$RELEASE_DIR/ato.db" ]; then
  echo "no built corpus at $RELEASE_DIR/ato.db" >&2
  echo "Run 'ato-mcp build ...' first." >&2
  exit 1
fi
if [ ! -f "$RELEASE_DIR/manifest.json" ]; then
  echo "no manifest at $RELEASE_DIR/manifest.json" >&2
  exit 1
fi

echo "=> packaging corpus (zstd -$ZSTD_LEVEL)"
"$ATO_MCP_BIN" package-corpus \
  --db-path  "$RELEASE_DIR/ato.db" \
  --out      "$RELEASE_DIR/ato.db.zst" \
  --level    "$ZSTD_LEVEL" \
  --manifest "$RELEASE_DIR/manifest.json" \
  >/dev/null

EXTRA_ARGS=()
if [ -n "$MODEL_URL" ];    then EXTRA_ARGS+=(--model-url    "$MODEL_URL");    fi
if [ -n "$MODEL_SHA256" ]; then EXTRA_ARGS+=(--model-sha256 "$MODEL_SHA256"); fi
if [ -n "$MODEL_SIZE" ];   then EXTRA_ARGS+=(--model-size   "$MODEL_SIZE");   fi
if [ -n "$SIGN_KEY" ];     then EXTRA_ARGS+=(--sign-key     "$SIGN_KEY");     fi

echo "=> uploading manifest, update summary, and ato.db.zst"
"$ATO_MCP_BIN" publish-release \
  --out-dir "$RELEASE_DIR" \
  --tag     "$TAG" \
  --repo    "$REPO" \
  --overwrite \
  "${EXTRA_ARGS[@]}"

echo "=> promoting $TAG to latest"
gh release edit "$TAG" --repo "$REPO" --latest --prerelease=false

echo
echo "Done. End users install the platform binary and run 'ato-mcp update'."
echo "Release: https://github.com/$REPO/releases/tag/$TAG"
