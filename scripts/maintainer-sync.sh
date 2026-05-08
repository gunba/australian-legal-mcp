#!/usr/bin/env bash
# Maintainer steady-state: refresh ato_pages, rebuild index, publish release.
#
# Expects these env vars (set in the systemd unit or your shell):
#   ATO_MCP_REPO_DIR   absolute path to this repo checkout
#   ATO_MCP_PAGES_DIR  absolute path to ato_pages/ (default: $ATO_MCP_REPO_DIR/../ato_pages)
#   ATO_MCP_MODEL_DIR  absolute path to the EmbeddingGemma dir holding
#                      tokenizer.json and onnx/model_quantized.onnx
#   ATO_MCP_MODEL_URL  optional approved model mirror URL
#   ATO_MCP_RERANKER_BUNDLE optional dir holding reranker ONNX + tokenizer.json
#   ATO_MCP_RERANKER_URL optional pinned hf:// source for the reranker
#   ATO_MCP_RERANKER_ID / SHA256 / SIZE / TOKENIZER_SHA256 optional overrides
#   ATO_MCP_FORCE_REBUILD set to 1/true/yes/on to rebuild even when source did not change
#   ATO_MCP_RELEASE_TAG  tag prefix (default: index)
#   ATO_MCP_GH_REPO    owner/name (default: gunba/ato-mcp)
#   ATO_MCP_MODE       incremental | catch_up | full (default: incremental)
#
# Flow:
#   1. Run the requested source refresh mode when it is catch_up or full.
#   2. Always run refresh-source --mode incremental as the final pre-build
#      source step, so the release includes the live ATO What's New feed.
#   3. If ato_pages/index.jsonl actually changed, rebuild against the previous
#      release manifest when one is available.
#   4. Publish a new release under tag $ATO_MCP_RELEASE_TAG-YYYY.MM.DD and
#      mark it latest. GitHub's "download latest" URL then points at it,
#      so end-users' `ato-mcp update` picks it up on their next run.

set -euo pipefail

REPO_DIR="${ATO_MCP_REPO_DIR:?set ATO_MCP_REPO_DIR}"
PAGES_DIR="${ATO_MCP_PAGES_DIR:-$REPO_DIR/../ato_pages}"
MODEL_DIR="${ATO_MCP_MODEL_DIR:?set ATO_MCP_MODEL_DIR}"
if [[ -f "$MODEL_DIR/onnx/model_quantized.onnx" ]]; then
    MODEL_ONNX="$MODEL_DIR/onnx/model_quantized.onnx"
else
    echo "onnx/model_quantized.onnx not found under $MODEL_DIR" >&2
    exit 2
fi
TOKENIZER="$MODEL_DIR/tokenizer.json"
if [[ ! -f "$TOKENIZER" ]]; then
    echo "tokenizer.json not found under $MODEL_DIR" >&2
    exit 2
fi
MODEL_URL="${ATO_MCP_MODEL_URL:-}"
MODEL_URL_ARG=()
if [ -n "$MODEL_URL" ]; then
    MODEL_URL_ARG=(--model-url "$MODEL_URL")
fi
RERANKER_BUNDLE="${ATO_MCP_RERANKER_BUNDLE:-}"
RERANKER_ID="${ATO_MCP_RERANKER_ID:-}"
RERANKER_URL="${ATO_MCP_RERANKER_URL:-}"
RERANKER_SHA256="${ATO_MCP_RERANKER_SHA256:-}"
RERANKER_SIZE="${ATO_MCP_RERANKER_SIZE:-}"
RERANKER_TOKENIZER_SHA256="${ATO_MCP_RERANKER_TOKENIZER_SHA256:-}"
RERANKER_BUILD_ARGS=()
RERANKER_RELEASE_ARGS=()
if [ -n "$RERANKER_BUNDLE" ]; then
    RERANKER_RELEASE_ARGS+=(--reranker-bundle "$RERANKER_BUNDLE")
fi
if [ -n "$RERANKER_ID" ]; then
    RERANKER_RELEASE_ARGS+=(--reranker-id "$RERANKER_ID")
fi
if [ -n "$RERANKER_URL" ]; then
    RERANKER_RELEASE_ARGS+=(--reranker-url "$RERANKER_URL")
fi
if [ -n "$RERANKER_SHA256" ]; then
    RERANKER_RELEASE_ARGS+=(--reranker-sha256 "$RERANKER_SHA256")
fi
if [ -n "$RERANKER_SIZE" ]; then
    RERANKER_RELEASE_ARGS+=(--reranker-size "$RERANKER_SIZE")
fi
if [ -n "$RERANKER_TOKENIZER_SHA256" ]; then
    RERANKER_RELEASE_ARGS+=(--reranker-tokenizer-sha256 "$RERANKER_TOKENIZER_SHA256")
fi
if [ -n "$RERANKER_ID" ] && [ -n "$RERANKER_URL" ] && [ -n "$RERANKER_SHA256" ] && [ -n "$RERANKER_SIZE" ]; then
    RERANKER_BUILD_ARGS=(
        --reranker-id "$RERANKER_ID"
        --reranker-url "$RERANKER_URL"
        --reranker-sha256 "$RERANKER_SHA256"
        --reranker-size "$RERANKER_SIZE"
    )
    if [ -n "$RERANKER_TOKENIZER_SHA256" ]; then
        RERANKER_BUILD_ARGS+=(--reranker-tokenizer-sha256 "$RERANKER_TOKENIZER_SHA256")
    fi
fi
GH_REPO="${ATO_MCP_GH_REPO:-gunba/ato-mcp}"
MODE="${ATO_MCP_MODE:-incremental}"
FORCE_REBUILD="${ATO_MCP_FORCE_REBUILD:-}"
TAG_PREFIX="${ATO_MCP_RELEASE_TAG:-index}"

cd "$REPO_DIR"
VENV="$REPO_DIR/.venv"
ATO_MCP="$VENV/bin/ato-mcp"
if [[ ! -x "$ATO_MCP" ]]; then
    echo "no venv at $VENV — run: python -m venv .venv && .venv/bin/pip install -e ." >&2
    exit 2
fi

# nvidia libs for GPU build (harmless if absent)
LIBS=$(find "$VENV"/lib*/python*/site-packages/nvidia/ -maxdepth 2 -name lib -type d 2>/dev/null | tr '\n' ':' | sed 's/:$//')
export LD_LIBRARY_PATH="${LIBS:-}${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

LOG="$REPO_DIR/logs/maintainer-sync-$(date -u +%Y%m%dT%H%M%SZ).log"
mkdir -p "$(dirname "$LOG")"
exec > >(tee -a "$LOG") 2>&1

echo "== $(date -u +%FT%TZ) maintainer sync ($MODE) =="

BEFORE_COUNT=$(wc -l < "$PAGES_DIR/index.jsonl" 2>/dev/null || echo 0)
index_hash() {
    if [[ ! -f "$PAGES_DIR/index.jsonl" ]]; then
        echo "missing"
        return
    fi
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$PAGES_DIR/index.jsonl" | awk '{print $1}'
    else
        shasum -a 256 "$PAGES_DIR/index.jsonl" | awk '{print $1}'
    fi
}
BEFORE_HASH=$(index_hash)

case "$MODE" in
    incremental)
        ;;
    catch_up)
        "$ATO_MCP" catch-up --output-dir "$PAGES_DIR"
        ;;
    full)
        "$ATO_MCP" refresh-source --mode full --output-dir "$PAGES_DIR"
        ;;
    *)
        echo "unknown MODE=$MODE (incremental|catch_up|full)" >&2
        exit 2
        ;;
esac

echo "== final What's New refresh =="
"$ATO_MCP" refresh-source --mode incremental --output-dir "$PAGES_DIR"

AFTER_COUNT=$(wc -l < "$PAGES_DIR/index.jsonl" 2>/dev/null || echo 0)
AFTER_HASH=$(index_hash)
echo "index.jsonl rows: $BEFORE_COUNT -> $AFTER_COUNT"
echo "index.jsonl sha256: $BEFORE_HASH -> $AFTER_HASH"

FORCE=false
case "$FORCE_REBUILD" in
    1|true|TRUE|yes|YES|on|ON) FORCE=true ;;
esac

if [[ "$FORCE" != true && "$MODE" != "full" && "$AFTER_HASH" == "$BEFORE_HASH" ]]; then
    echo "no source index changes; skipping rebuild+release"
    exit 0
fi

TAG="$TAG_PREFIX-$(date -u +%Y.%m.%d)"
RELEASE_DIR="$REPO_DIR/release/$TAG"
PREV_MANIFEST="$REPO_DIR/release/.latest/manifest.json"
mkdir -p "$RELEASE_DIR"

PREV_ARG=()
if [[ -f "$PREV_MANIFEST" ]]; then
    PREV_ARG=(--previous-manifest "$PREV_MANIFEST")
fi

"$ATO_MCP" build-index \
    --pages-dir "$PAGES_DIR" \
    --out-dir "$RELEASE_DIR" \
    --db-path "$RELEASE_DIR/ato.db" \
    --model-path "$MODEL_ONNX" \
    --tokenizer-path "$TOKENIZER" \
    --gpu \
    "${PREV_ARG[@]}" \
    "${RERANKER_BUILD_ARGS[@]}"

"$ATO_MCP" release \
    --out-dir "$RELEASE_DIR" \
    --tag "$TAG" \
    --repo "$GH_REPO" \
    --model-dir "$MODEL_DIR" \
    "${MODEL_URL_ARG[@]}" \
    "${RERANKER_RELEASE_ARGS[@]}" \
    --overwrite

# Promote to "latest" so /releases/latest/download resolves to this tag.
gh release edit "$TAG" --repo "$GH_REPO" --latest --prerelease=false

# Remember this whole release so the next incremental build can reuse prior
# pack bytes, not just the manifest's offsets.
rm -rf "$REPO_DIR/release/.latest"
ln -s "$RELEASE_DIR" "$REPO_DIR/release/.latest"

echo "== done: released $TAG ($(( AFTER_COUNT - BEFORE_COUNT )) new rows) =="
