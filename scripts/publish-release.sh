#!/usr/bin/env bash
# One-shot corpus publication using the schema-5 db-artifact flow.
#
# Pipeline:
#   1. Verify a built corpus exists at $RELEASE_DIR/legal.db
#   2. `legal-mcp package-corpus` strips FTS5 from a copy, zstd-compresses to
#      $RELEASE_DIR/legal.db.zst, and rewrites $RELEASE_DIR/manifest.json so
#      `db: {url, sha256, size}` points at the new artifact
#   3. `legal-mcp publish-release` verifies and uploads legal.db.zst, every
#      ann/<source>.ann sidecar, then manifest.json strictly last
#
# Prereqs:
#   - legal-mcp build has finished; release/ contains legal.db and manifest.json
#   - target/release/legal-mcp built (or LEGAL_MCP_BIN points at one)
#   - optional LEGAL_MCP_MODEL_URL for an approved model mirror
#   - LEGAL_MCP_MODEL_SHA256 and LEGAL_MCP_MODEL_SIZE when that mirror is not
#     a Hugging Face source
#   - optional LEGAL_MCP_ZSTD_LEVEL to override default 19
#   - gh authenticated for the maintainer account
#
# Usage:
#   scripts/publish-release.sh corpus-20260712T010203Z
#   scripts/publish-release.sh corpus-20260712T010203Z gunba/australian-legal-mcp
set -euo pipefail

TAG="${1:?usage: publish-release.sh <tag> [owner/repo]}"
REPO="${2:-${LEGAL_MCP_GH_REPO:-gunba/australian-legal-mcp}}"
REPO_DIR="${LEGAL_MCP_REPO_DIR:-$(pwd)}"
RELEASE_DIR="${LEGAL_MCP_RELEASE_DIR:-$REPO_DIR/release}"
BIN="${LEGAL_MCP_BIN:-$REPO_DIR/target/release/legal-mcp}"
MODEL_URL="${LEGAL_MCP_MODEL_URL:-}"
MODEL_SHA256="${LEGAL_MCP_MODEL_SHA256:-}"
MODEL_SIZE="${LEGAL_MCP_MODEL_SIZE:-}"
ZSTD_LEVEL="${LEGAL_MCP_ZSTD_LEVEL:-19}"

if [ ! -x "$BIN" ]; then
	echo "legal-mcp binary not found at $BIN" >&2
	echo "Build it first: cargo build --release" >&2
	exit 1
fi
if [ ! -f "$RELEASE_DIR/legal.db" ]; then
	echo "no built corpus at $RELEASE_DIR/legal.db" >&2
	echo "Run 'legal-mcp build ...' first." >&2
	exit 1
fi
if [ ! -f "$RELEASE_DIR/manifest.json" ]; then
	echo "no manifest at $RELEASE_DIR/manifest.json" >&2
	exit 1
fi

echo "=> packaging corpus (zstd -$ZSTD_LEVEL)"
"$BIN" package-corpus \
	--db-path "$RELEASE_DIR/legal.db" \
	--out "$RELEASE_DIR/legal.db.zst" \
	--level "$ZSTD_LEVEL" \
	--manifest "$RELEASE_DIR/manifest.json" \
	>/dev/null

EXTRA_ARGS=()
if [ -n "$MODEL_URL" ]; then EXTRA_ARGS+=(--model-url "$MODEL_URL"); fi
if [ -n "$MODEL_SHA256" ]; then EXTRA_ARGS+=(--model-sha256 "$MODEL_SHA256"); fi
if [ -n "$MODEL_SIZE" ]; then EXTRA_ARGS+=(--model-size "$MODEL_SIZE"); fi

echo "=> publishing database, ANN sidecars, signature, then manifest"
"$BIN" publish-release \
	--out-dir "$RELEASE_DIR" \
	--tag "$TAG" \
	--repo "$REPO" \
	"${EXTRA_ARGS[@]}"

echo
echo "Done. End users install the platform binary and run 'legal-mcp update'."
echo "Release: https://github.com/$REPO/releases/tag/$TAG"
