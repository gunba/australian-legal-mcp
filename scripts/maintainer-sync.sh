#!/usr/bin/env bash
# Maintainer steady-state: refresh ato_pages, rebuild index, publish release.
#
# Now invokes the Rust ato-mcp binary directly — no Python venv, no
# scraper Python modules. Replaces the previous version that shelled
# into the Python pipeline.
#
# Expects these env vars (set in the systemd unit or your shell):
#   ATO_MCP_REPO_DIR   absolute path to this repo checkout
#   ATO_MCP_PAGES_DIR  absolute path to ato_pages/ (default: $ATO_MCP_REPO_DIR/../ato_pages)
#   ATO_MCP_MODEL_DIR  absolute path to the EmbeddingGemma dir holding
#                      tokenizer.json and onnx/model_quantized.onnx
#   ATO_MCP_MODEL_URL  optional approved model mirror URL
#   ATO_MCP_FORCE_REBUILD set to 1/true/yes/on to rebuild even when source did not change
#   ATO_MCP_RELEASE_TAG  tag prefix (default: index)
#   ATO_MCP_GH_REPO    owner/name (default: gunba/ato-mcp)
#   ATO_MCP_MODE       incremental | catch_up | full (default: incremental)
#   ATO_MCP_BIN        path to the Rust ato-mcp binary
#                      (default: $ATO_MCP_REPO_DIR/target/release/ato-mcp)
#
# Flow:
#   1. Run the requested source refresh mode (catch_up or full) when set.
#   2. Always run an incremental What's New refresh as the final pre-build
#      source step, so the release includes the live ATO What's New feed.
#   3. If ato_pages/index.jsonl actually changed, rebuild against the previous
#      release manifest when one is available.
#   4. Publish a new release under tag $ATO_MCP_RELEASE_TAG-YYYY.MM.DD and
#      mark it latest. GitHub's "download latest" URL then points at it,
#      so end-users' `ato-mcp update` picks it up on their next run.

set -euo pipefail

if [[ -z "${ATO_MCP_SLEEP_INHIBITED:-}" && -z "${ATO_MCP_ALLOW_SLEEP:-}" ]]; then
    if command -v systemd-inhibit >/dev/null 2>&1 \
        && systemd-inhibit --who=ato-mcp --what=sleep --mode=block \
            --why="ato-mcp maintainer sync probe" true >/dev/null 2>&1; then
        export ATO_MCP_SLEEP_INHIBITED=1
        exec systemd-inhibit --who=ato-mcp --what=sleep:idle:handle-lid-switch \
            --mode=block --why="ato-mcp maintainer sync" "$0" "$@"
    fi
fi

REPO_DIR="${ATO_MCP_REPO_DIR:?set ATO_MCP_REPO_DIR}"
PAGES_DIR="${ATO_MCP_PAGES_DIR:-$REPO_DIR/../ato_pages}"
MODEL_DIR="${ATO_MCP_MODEL_DIR:?set ATO_MCP_MODEL_DIR (path to EmbeddingGemma checkout)}"
MODEL_ONNX="$MODEL_DIR/onnx/model_quantized.onnx"
TOKENIZER="$MODEL_DIR/tokenizer.json"
MODEL_URL="${ATO_MCP_MODEL_URL:-}"
MODEL_URL_ARG=()
if [ -n "$MODEL_URL" ]; then
    MODEL_URL_ARG=(--model-url "$MODEL_URL")
fi

GH_REPO="${ATO_MCP_GH_REPO:-gunba/ato-mcp}"
MODE="${ATO_MCP_MODE:-incremental}"
FORCE_REBUILD="${ATO_MCP_FORCE_REBUILD:-}"
TAG_PREFIX="${ATO_MCP_RELEASE_TAG:-index}"

cd "$REPO_DIR"
if [[ -n "${ATO_MCP_CUDA_LIB_PATH:-}" ]]; then
    export LD_LIBRARY_PATH="$ATO_MCP_CUDA_LIB_PATH:${LD_LIBRARY_PATH:-}"
else
    CUDA_LIB_DIRS=()
    shopt -s nullglob
    for nvidia_root in \
        "$REPO_DIR"/.venv/lib/python*/site-packages/nvidia \
        "$HOME"/.local/lib/python*/site-packages/nvidia; do
        for component in cuda_runtime cublas cudnn cufft curand; do
            if [[ -d "$nvidia_root/$component/lib" ]]; then
                CUDA_LIB_DIRS+=("$nvidia_root/$component/lib")
            fi
        done
    done
    shopt -u nullglob
    if (( ${#CUDA_LIB_DIRS[@]} > 0 )); then
        CUDA_LIB_PATH=$(IFS=:; echo "${CUDA_LIB_DIRS[*]}")
        export LD_LIBRARY_PATH="$CUDA_LIB_PATH:${LD_LIBRARY_PATH:-}"
    fi
fi

ATO_MCP="${ATO_MCP_BIN:-$REPO_DIR/target/release/ato-mcp}"
if [[ ! -x "$ATO_MCP" ]]; then
    echo "ato-mcp binary not found at $ATO_MCP — run: cargo build --release --features cuda" >&2
    exit 2
fi

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

SNAPSHOT_BASE="$REPO_DIR/release/scrape_snapshots"
mkdir -p "$SNAPSHOT_BASE"

run_full() {
    local ts="$(date -u +%Y%m%dT%H%M%SZ)"
    local snap="$SNAPSHOT_BASE/$ts"
    echo "== full crawl into $snap =="
    "$ATO_MCP" tree-crawl --out-dir "$snap"
    "$ATO_MCP" snapshot-reduce --nodes-path "$snap/nodes.jsonl"
    "$ATO_MCP" link-download \
        --deduped-links "$snap/deduped_links.jsonl" \
        --out-dir       "$PAGES_DIR"
}

run_catch_up() {
    local ts="$(date -u +%Y%m%dT%H%M%SZ)"
    local snap="$SNAPSHOT_BASE/$ts"
    echo "== catch-up crawl into $snap =="
    "$ATO_MCP" tree-crawl --out-dir "$snap"
    "$ATO_MCP" snapshot-reduce --nodes-path "$snap/nodes.jsonl"
    "$ATO_MCP" scrape-diff \
        --index    "$PAGES_DIR/index.jsonl" \
        --deduped  "$snap/deduped_links.jsonl" \
        --out      "$snap/missing_links.jsonl"
    "$ATO_MCP" link-download \
        --deduped-links "$snap/missing_links.jsonl" \
        --out-dir       "$PAGES_DIR"
}

run_incremental() {
    local ts="$(date -u +%Y%m%dT%H%M%SZ)"
    local pending="$SNAPSHOT_BASE/whatsnew_${ts}.jsonl"
    echo "== What's New incremental ($pending) =="
    "$ATO_MCP" scrape-diff \
        --index         "$PAGES_DIR/index.jsonl" \
        --whats-new-url "https://www.ato.gov.au/law/view/whatsnew.htm?fid=whatsnew" \
        --out           "$pending"
    "$ATO_MCP" link-download \
        --deduped-links "$pending" \
        --out-dir       "$PAGES_DIR"
}

case "$MODE" in
    incremental) run_incremental ;;
    catch_up)    run_catch_up; run_incremental ;;
    full)        run_full;     run_incremental ;;
    *)
        echo "unknown MODE=$MODE (incremental|catch_up|full)" >&2
        exit 2
        ;;
esac

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
mkdir -p "$RELEASE_DIR"

echo "== build corpus =="
"$ATO_MCP" build \
    --pages-dir "$PAGES_DIR" \
    --db-path   "$RELEASE_DIR/ato.db" \
    --out-dir   "$RELEASE_DIR" \
    --gpu

echo "== publish release $TAG =="
"$ATO_MCP" publish-release \
    --out-dir "$RELEASE_DIR" \
    --tag     "$TAG" \
    --repo    "$GH_REPO" \
    --overwrite \
    "${MODEL_URL_ARG[@]}"

# Promote to "latest" so /releases/latest/download resolves to this tag.
gh release edit "$TAG" --repo "$GH_REPO" --latest --prerelease=false

# Remember this whole release so the next incremental build can reuse prior
# pack bytes, not just the manifest's offsets.
rm -rf "$REPO_DIR/release/.latest"
ln -s "$RELEASE_DIR" "$REPO_DIR/release/.latest"

echo "== done: released $TAG ($(( AFTER_COUNT - BEFORE_COUNT )) new rows) =="
