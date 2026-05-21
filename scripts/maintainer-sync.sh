#!/usr/bin/env bash
# Maintainer steady-state: refresh ato_pages, rebuild corpus, publish to the
# current binary release tag.
#
# Invokes the Rust ato-mcp binary directly. The released corpus lives on the
# same GitHub tag as the binary archives — the maintainer queries the latest
# binary tag with `gh release view`, runs the build/package/publish chain,
# and uploads manifest.json + ato.db.zst to that tag with
# --clobber. Users always hit releases/latest/download/manifest.json.
#
# Expects these env vars (set in the systemd unit or your shell):
#   ATO_MCP_REPO_DIR   absolute path to this repo checkout
#   ATO_MCP_PAGES_DIR  absolute path to ato_pages/ (default: $ATO_MCP_REPO_DIR/../ato_pages)
#   ATO_MCP_MODEL_DIR  absolute path to the Granite embedding dir holding
#                      tokenizer.json, onnx/model_fp16.onnx, and
#                      onnx/model_fp16.onnx_data
#   ATO_MCP_MODEL_URL  optional approved model mirror URL
#   ATO_MCP_MODEL_SHA256 required with a non-Hugging Face ATO_MCP_MODEL_URL
#   ATO_MCP_MODEL_SIZE   required with a non-Hugging Face ATO_MCP_MODEL_URL
#   ATO_MCP_FORCE_REBUILD set to 1/true/yes/on to rebuild even when source did not change
#   ATO_MCP_GH_REPO    owner/name (default: gunba/ato-mcp)
#   ATO_MCP_MODE       incremental | catch_up | full (default: incremental)
#   ATO_MCP_BIN        path to the Rust ato-mcp binary
#                      (default: $ATO_MCP_REPO_DIR/target/release/ato-mcp)
#   ATO_MCP_RELEASE_TAG override the publish tag (default: latest gh release on the repo)
#   ATO_MCP_ZSTD_LEVEL package-corpus zstd level (default: 19)
#
# Flow:
#   1. Run the requested source refresh mode (catch_up or full) when set.
#   2. Always run an incremental What's New refresh as the final pre-build step.
#   3. If ato_pages/index.jsonl changed, rebuild the corpus from scratch
#      (or resume a matching interrupted checkpoint).
#   4. package-corpus → ato.db.zst.
#   5. publish-release to the current binary release tag with --overwrite.

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
MODEL_DIR="${ATO_MCP_MODEL_DIR:?set ATO_MCP_MODEL_DIR (path to Granite embedding checkout)}"
MODEL_ONNX="$MODEL_DIR/onnx/model_fp16.onnx"
MODEL_ONNX_DATA="$MODEL_DIR/onnx/model_fp16.onnx_data"
TOKENIZER="$MODEL_DIR/tokenizer.json"
MODEL_URL="${ATO_MCP_MODEL_URL:-}"
MODEL_SHA256="${ATO_MCP_MODEL_SHA256:-}"
MODEL_SIZE="${ATO_MCP_MODEL_SIZE:-}"
MODEL_RELEASE_ARGS=()
if [ -n "$MODEL_URL" ]; then
    MODEL_RELEASE_ARGS+=(--model-url "$MODEL_URL")
fi
if [ -n "$MODEL_SHA256" ]; then
    MODEL_RELEASE_ARGS+=(--model-sha256 "$MODEL_SHA256")
fi
if [ -n "$MODEL_SIZE" ]; then
    MODEL_RELEASE_ARGS+=(--model-size "$MODEL_SIZE")
fi
if [[ "$MODEL_URL" == https://huggingface.co/* || "$MODEL_URL" == http://huggingface.co/* ]]; then
    echo "ATO_MCP_MODEL_URL must use hf://repo@revision for Hugging Face sources, not HTTPS" >&2
    exit 2
fi
if [[ "$MODEL_URL" == hf://* ]]; then
    HF_SPEC="${MODEL_URL#hf://}"
    if [[ "$HF_SPEC" != *@* || "$HF_SPEC" == *@ ]]; then
        echo "ATO_MCP_MODEL_URL must include an explicit Hugging Face revision: hf://repo@revision" >&2
        exit 2
    fi
fi
if [[ "$MODEL_URL" != "" \
    && "$MODEL_URL" != hf://* ]]; then
    if [[ -z "$MODEL_SHA256" || ! "$MODEL_SIZE" =~ ^[1-9][0-9]*$ ]]; then
        echo "non-Hugging Face ATO_MCP_MODEL_URL requires ATO_MCP_MODEL_SHA256 and positive numeric ATO_MCP_MODEL_SIZE" >&2
        exit 2
    fi
fi

GH_REPO="${ATO_MCP_GH_REPO:-gunba/ato-mcp}"
MODE="${ATO_MCP_MODE:-incremental}"
FORCE_REBUILD="${ATO_MCP_FORCE_REBUILD:-}"
ZSTD_LEVEL="${ATO_MCP_ZSTD_LEVEL:-19}"

cd "$REPO_DIR"
if [[ -n "${ATO_MCP_CUDA_LIB_PATH:-}" ]]; then
    export LD_LIBRARY_PATH="$ATO_MCP_CUDA_LIB_PATH:${LD_LIBRARY_PATH:-}"
else
    CUDA_LIB_DIRS=()
    shopt -s nullglob
    for nvidia_root in \
        "$REPO_DIR"/.venv/lib/python*/site-packages/nvidia \
        "$HOME"/.local/lib/python*/site-packages/nvidia; do
        for component_lib in "$nvidia_root"/*/lib; do
            [[ -d "$component_lib" ]] && CUDA_LIB_DIRS+=("$component_lib")
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
for model_file in "$MODEL_ONNX" "$MODEL_ONNX_DATA" "$TOKENIZER"; do
    if [[ ! -f "$model_file" ]]; then
        echo "required Granite model file not found: $model_file" >&2
        exit 2
    fi
done

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

# Target tag: explicit override, or the current latest binary release.
TAG="${ATO_MCP_RELEASE_TAG:-}"
if [[ -z "$TAG" ]]; then
    TAG=$(gh release view --repo "$GH_REPO" --json tagName --jq .tagName 2>/dev/null || true)
    if [[ -z "$TAG" ]]; then
        echo "could not determine latest release tag from $GH_REPO; set ATO_MCP_RELEASE_TAG" >&2
        exit 2
    fi
fi
RELEASE_DIR="$REPO_DIR/release/build-$(date -u +%Y%m%d)"

# Resume an interrupted build matching the current source-index hash.
try_resumable_build_checkpoint() {
    local candidate check_output
    while IFS= read -r candidate; do
        [[ -z "$candidate" ]] && continue
        [[ -f "$candidate/build-state.json" ]] || continue
        if check_output=$("$ATO_MCP" check-build-checkpoint \
            --release-dir "$candidate" \
            --source-index-sha256 "$AFTER_HASH" \
            --zstd-level 3 2>&1); then
            echo "$check_output"
            RELEASE_DIR="$candidate"
            echo "resuming interrupted build checkpoint in $RELEASE_DIR"
            return 0
        fi
    done < <(find "$REPO_DIR/release" -maxdepth 1 -mindepth 1 -type d -name "build-*" | sort -r)
    return 1
}

try_resumable_build_checkpoint || true

mkdir -p "$RELEASE_DIR"
echo "== build corpus =="
"$ATO_MCP" build \
    --pages-dir "$PAGES_DIR" \
    --db-path   "$RELEASE_DIR/ato.db" \
    --model-dir "$MODEL_DIR" \
    --out-dir   "$RELEASE_DIR" \
    --gpu \
    --profile

echo "== package corpus (zstd -$ZSTD_LEVEL) =="
"$ATO_MCP" package-corpus \
    --db-path  "$RELEASE_DIR/ato.db" \
    --out      "$RELEASE_DIR/ato.db.zst" \
    --level    "$ZSTD_LEVEL" \
    --manifest "$RELEASE_DIR/manifest.json"

echo "== publish corpus to $TAG =="
"$ATO_MCP" publish-release \
    --out-dir "$RELEASE_DIR" \
    --tag     "$TAG" \
    --repo    "$GH_REPO" \
    --overwrite \
    "${MODEL_RELEASE_ARGS[@]}"

echo "== done: corpus shipped to $TAG ($(( AFTER_COUNT - BEFORE_COUNT )) new rows) =="
