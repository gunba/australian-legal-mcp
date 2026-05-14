#!/usr/bin/env bash
# One-shot corpus publication. Binary assets are built by
# .github/workflows/release-binaries.yml; this script uploads the corpus
# manifest, update summary, and packs. Offline bundles are built explicitly
# with scripts/make-offline-bundle.sh for air-gapped installs; publishing
# them by default duplicates the pack assets.
#
# Prereqs:
#   - build-index has finished; release/ contains packs/ and manifest.json
#   - model at models/embeddinggemma/
#   - optional ATO_MCP_MODEL_URL for an approved model mirror
#   - optional ATO_MCP_RERANKER_BUNDLE / ATO_MCP_RERANKER_URL for the reranker
#     (defaults to models/reranker when present)
#   - gh authenticated for the maintainer account
#
# Usage:
#   scripts/publish-release.sh v0.3.0
#   scripts/publish-release.sh v0.3.0 gunba/ato-mcp
set -euo pipefail

TAG="${1:?usage: publish-release.sh <tag> [owner/repo]}"
REPO="${2:-${ATO_MCP_GH_REPO:-gunba/ato-mcp}}"
REPO_DIR="${ATO_MCP_REPO_DIR:-$(pwd)}"
VENV="${ATO_MCP_VENV:-$REPO_DIR/.venv}"
RELEASE_DIR="${ATO_MCP_RELEASE_DIR:-$REPO_DIR/release}"
MODEL_DIR="${ATO_MCP_MODEL_DIR:-$REPO_DIR/models/embeddinggemma}"
MODEL_URL="${ATO_MCP_MODEL_URL:-}"
MODEL_URL_ARG=()
if [ -n "$MODEL_URL" ]; then
  MODEL_URL_ARG=(--model-url "$MODEL_URL")
fi
DEFAULT_RERANKER_BUNDLE="$REPO_DIR/models/reranker"
RERANKER_BUNDLE="${ATO_MCP_RERANKER_BUNDLE:-}"
if [[ -z "$RERANKER_BUNDLE" && -d "$DEFAULT_RERANKER_BUNDLE" ]]; then
  RERANKER_BUNDLE="$DEFAULT_RERANKER_BUNDLE"
fi
RERANKER_ID="${ATO_MCP_RERANKER_ID:-}"
RERANKER_URL="${ATO_MCP_RERANKER_URL:-}"
RERANKER_SHA256="${ATO_MCP_RERANKER_SHA256:-}"
RERANKER_SIZE="${ATO_MCP_RERANKER_SIZE:-}"
RERANKER_TOKENIZER_SHA256="${ATO_MCP_RERANKER_TOKENIZER_SHA256:-}"
RERANKER_ARGS=()
if [ -n "$RERANKER_BUNDLE" ]; then
  RERANKER_ARGS+=(--reranker-bundle "$RERANKER_BUNDLE")
fi
if [ -n "$RERANKER_ID" ]; then
  RERANKER_ARGS+=(--reranker-id "$RERANKER_ID")
fi
if [ -n "$RERANKER_URL" ]; then
  RERANKER_ARGS+=(--reranker-url "$RERANKER_URL")
fi
if [ -n "$RERANKER_SHA256" ]; then
  RERANKER_ARGS+=(--reranker-sha256 "$RERANKER_SHA256")
fi
if [ -n "$RERANKER_SIZE" ]; then
  RERANKER_ARGS+=(--reranker-size "$RERANKER_SIZE")
fi
if [ -n "$RERANKER_TOKENIZER_SHA256" ]; then
  RERANKER_ARGS+=(--reranker-tokenizer-sha256 "$RERANKER_TOKENIZER_SHA256")
fi

echo "=> uploading manifest, update summary, and packs"
"$VENV/bin/ato-mcp" release \
  --out-dir   "$RELEASE_DIR" \
  --tag       "$TAG" \
  --repo      "$REPO" \
  --model-dir "$MODEL_DIR" \
  "${MODEL_URL_ARG[@]}" \
  "${RERANKER_ARGS[@]}" \
  --overwrite

echo "=> promoting $TAG to latest"
gh release edit "$TAG" --repo "$REPO" --latest --prerelease=false

echo
echo "Done. End-user install requires a Rust binary asset plus these corpus assets."
echo "Release: https://github.com/$REPO/releases/tag/$TAG"
