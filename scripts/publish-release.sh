#!/usr/bin/env bash
# One-shot corpus publication. Binary assets are built by
# .github/workflows/release-binaries.yml; this script uploads the corpus
# manifest, update summary, and packs by shelling into the Rust binary's
# `publish-release` subcommand.
#
# Prereqs:
#   - ato-mcp build has finished; release/ contains packs/ and manifest.json
#   - target/release/ato-mcp built (or ATO_MCP_BIN points at one)
#   - optional ATO_MCP_MODEL_URL for an approved model mirror
#   - optional ATO_MCP_SIGN_KEY for manifest signing (minisign secret key)
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

if [ ! -x "$ATO_MCP_BIN" ]; then
  echo "ato-mcp binary not found at $ATO_MCP_BIN" >&2
  echo "Build it first: cargo build --release" >&2
  exit 1
fi

EXTRA_ARGS=()
if [ -n "$MODEL_URL" ]; then
  EXTRA_ARGS+=(--model-url "$MODEL_URL")
fi
if [ -n "$MODEL_SHA256" ]; then
  EXTRA_ARGS+=(--model-sha256 "$MODEL_SHA256")
fi
if [ -n "$MODEL_SIZE" ]; then
  EXTRA_ARGS+=(--model-size "$MODEL_SIZE")
fi
if [ -n "$SIGN_KEY" ]; then
  EXTRA_ARGS+=(--sign-key "$SIGN_KEY")
fi

echo "=> uploading manifest, update summary, and packs"
"$ATO_MCP_BIN" publish-release \
  --out-dir "$RELEASE_DIR" \
  --tag     "$TAG" \
  --repo    "$REPO" \
  --overwrite \
  "${EXTRA_ARGS[@]}"

echo "=> promoting $TAG to latest"
gh release edit "$TAG" --repo "$REPO" --latest --prerelease=false

echo
echo "Done. End-user install requires a Rust binary asset plus these corpus assets."
echo "Release: https://github.com/$REPO/releases/tag/$TAG"
