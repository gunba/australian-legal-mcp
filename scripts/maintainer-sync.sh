#!/usr/bin/env bash
# Maintainer steady-state: refresh all legal-source workspaces, rebuild the
# unified corpus, and publish to the current binary release tag.
#
# Invokes the Rust legal-mcp binary directly. The released corpus lives on the
# same GitHub tag as the binary archives. Update discovery paginates releases
# until it finds the newest non-prerelease manifest asset.
#
# Expects these env vars (set in the systemd unit or your shell):
#   LEGAL_MCP_REPO_DIR       absolute path to this repo checkout
#   LEGAL_MCP_ATO_PAGES_DIR  absolute path to ato_pages/
#                            (default: $LEGAL_MCP_REPO_DIR/../ato_pages)
#   LEGAL_MCP_FRL_DIR        absolute path to the mandatory Federal Register workspace
#   LEGAL_MCP_MODEL_DIR      absolute path to the Granite embedding dir holding
#                            tokenizer.json, onnx/model_fp16.onnx, and
#                            onnx/model_fp16.onnx_data
#   LEGAL_MCP_MODEL_URL      optional approved model mirror URL
#   LEGAL_MCP_MODEL_SHA256   required with a non-Hugging Face LEGAL_MCP_MODEL_URL
#   LEGAL_MCP_MODEL_SIZE     required with a non-Hugging Face LEGAL_MCP_MODEL_URL
#   LEGAL_MCP_FORCE_REBUILD  set to 1/true/yes/on to rebuild even when source did not change
#   LEGAL_MCP_GH_REPO        owner/name (default: gunba/australian-legal-mcp)
#   LEGAL_MCP_ATO_MODE       ATO acquisition mode: incremental | catch_up | full
#                            (default: incremental)
#   LEGAL_MCP_BIN            path to the Rust legal-mcp binary
#                            (default: $LEGAL_MCP_REPO_DIR/target/release/legal-mcp)
#   LEGAL_MCP_RELEASE_TAG    override the publish tag (default: latest gh release on the repo)
#   LEGAL_MCP_ZSTD_LEVEL     package-corpus zstd level (default: 19)
#
# Flow:
#   1. Run the requested ATO acquisition mode (catch_up or full) when set.
#   2. Always run incremental ATO and FRL refreshes before the build.
#   3. Rebuild when either authoritative source inventory changes.
#   4. Package legal.db.zst and publish it plus every ann/<source>.ann before manifest.json.

set -euo pipefail

if [[ -z "${LEGAL_MCP_SLEEP_INHIBITED:-}" && -z "${LEGAL_MCP_ALLOW_SLEEP:-}" ]]; then
	if command -v systemd-inhibit >/dev/null 2>&1 &&
		systemd-inhibit --who=legal-mcp --what=sleep --mode=block \
			--why="Australian Legal MCP maintainer sync probe" true >/dev/null 2>&1; then
		export LEGAL_MCP_SLEEP_INHIBITED=1
		exec systemd-inhibit --who=legal-mcp --what=sleep:idle:handle-lid-switch \
			--mode=block --why="Australian Legal MCP maintainer sync" "$0" "$@"
	fi
fi

REPO_DIR="${LEGAL_MCP_REPO_DIR:?set LEGAL_MCP_REPO_DIR}"
PAGES_DIR="${LEGAL_MCP_ATO_PAGES_DIR:-$REPO_DIR/../ato_pages}"
FRL_DIR="${LEGAL_MCP_FRL_DIR:?set LEGAL_MCP_FRL_DIR to the Federal Register workspace}"
MODEL_DIR="${LEGAL_MCP_MODEL_DIR:?set LEGAL_MCP_MODEL_DIR (path to Granite embedding checkout)}"
MODEL_ONNX="$MODEL_DIR/onnx/model_fp16.onnx"
MODEL_ONNX_DATA="$MODEL_DIR/onnx/model_fp16.onnx_data"
TOKENIZER="$MODEL_DIR/tokenizer.json"
MODEL_URL="${LEGAL_MCP_MODEL_URL:-}"
MODEL_SHA256="${LEGAL_MCP_MODEL_SHA256:-}"
MODEL_SIZE="${LEGAL_MCP_MODEL_SIZE:-}"
if [[ "$MODEL_URL" == https://huggingface.co/* || "$MODEL_URL" == http://huggingface.co/* ]]; then
	echo "LEGAL_MCP_MODEL_URL must use hf://repo@revision for Hugging Face sources, not HTTPS" >&2
	exit 2
fi
if [[ "$MODEL_URL" == hf://* ]]; then
	HF_SPEC="${MODEL_URL#hf://}"
	if [[ "$HF_SPEC" != *@* || "$HF_SPEC" == *@ ]]; then
		echo "LEGAL_MCP_MODEL_URL must include an explicit Hugging Face revision: hf://repo@revision" >&2
		exit 2
	fi
fi
if [[ "$MODEL_URL" != "" &&
	"$MODEL_URL" != hf://* ]]; then
	if [[ -z "$MODEL_SHA256" || ! "$MODEL_SIZE" =~ ^[1-9][0-9]*$ ]]; then
		echo "non-Hugging Face LEGAL_MCP_MODEL_URL requires LEGAL_MCP_MODEL_SHA256 and positive numeric LEGAL_MCP_MODEL_SIZE" >&2
		exit 2
	fi
fi

GH_REPO="${LEGAL_MCP_GH_REPO:-gunba/australian-legal-mcp}"
ATO_MODE="${LEGAL_MCP_ATO_MODE:-incremental}"
FORCE_REBUILD="${LEGAL_MCP_FORCE_REBUILD:-}"
ZSTD_LEVEL="${LEGAL_MCP_ZSTD_LEVEL:-19}"

cd "$REPO_DIR"
command -v flock >/dev/null 2>&1 || {
	echo "missing command: flock" >&2
	exit 2
}
mkdir -p "$REPO_DIR/release"
exec 9>"$REPO_DIR/release/maintainer-sync.lock"
flock -n 9 || {
	echo "another maintainer sync is already running" >&2
	exit 2
}
if [[ -n "${LEGAL_MCP_CUDA_LIB_PATH:-}" ]]; then
	export LD_LIBRARY_PATH="$LEGAL_MCP_CUDA_LIB_PATH:${LD_LIBRARY_PATH:-}"
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
	if ((${#CUDA_LIB_DIRS[@]} > 0)); then
		printf -v CUDA_LIB_PATH '%s:' "${CUDA_LIB_DIRS[@]}"
		CUDA_LIB_PATH="${CUDA_LIB_PATH%:}"
		export LD_LIBRARY_PATH="$CUDA_LIB_PATH:${LD_LIBRARY_PATH:-}"
	fi
fi

BIN="${LEGAL_MCP_BIN:-$REPO_DIR/target/release/legal-mcp}"
if [[ ! -x "$BIN" ]]; then
	echo "legal-mcp binary not found at $BIN — run: cargo build --release --features cuda" >&2
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

echo "== $(date -u +%FT%TZ) maintainer sync (ATO mode: $ATO_MODE) =="

mkdir -p "$FRL_DIR"
BEFORE_COUNT=$(wc -l <"$PAGES_DIR/index.jsonl" 2>/dev/null || echo 0)
source_hash() {
    python3 - "$PAGES_DIR/index.jsonl" "$FRL_DIR/frl/state.json" <<'PY'
import hashlib, pathlib, sys
h = hashlib.sha256()
for raw in sys.argv[1:]:
    path = pathlib.Path(raw)
    h.update(str(path.name).encode())
    h.update(b"\0")
    h.update(path.read_bytes() if path.is_file() else b"<missing>")
    h.update(b"\0")
print(h.hexdigest())
PY
}
BEFORE_HASH=$(source_hash)

SNAPSHOT_BASE="$REPO_DIR/release/scrape_snapshots"
mkdir -p "$SNAPSHOT_BASE"

run_full() {
	local ts snap
	ts="$(date -u +%Y%m%dT%H%M%SZ)"
	snap="$SNAPSHOT_BASE/$ts"
	echo "== full ATO crawl into $snap =="
	"$BIN" tree-crawl --out-dir "$snap"
	"$BIN" snapshot-reduce --nodes-path "$snap/nodes.jsonl"
	"$BIN" link-download \
		--deduped-links "$snap/deduped_links.jsonl" \
		--out-dir "$PAGES_DIR"
}

run_catch_up() {
	local ts snap
	ts="$(date -u +%Y%m%dT%H%M%SZ)"
	snap="$SNAPSHOT_BASE/$ts"
	echo "== ATO catch-up crawl into $snap =="
	"$BIN" tree-crawl --out-dir "$snap"
	"$BIN" snapshot-reduce --nodes-path "$snap/nodes.jsonl"
	"$BIN" scrape-diff \
		--index "$PAGES_DIR/index.jsonl" \
		--deduped "$snap/deduped_links.jsonl" \
		--out "$snap/missing_links.jsonl"
	"$BIN" link-download \
		--deduped-links "$snap/missing_links.jsonl" \
		--out-dir "$PAGES_DIR"
}

run_incremental() {
	local ts run_dir
	ts="$(date -u +%Y%m%dT%H%M%SZ)"
	run_dir="$SNAPSHOT_BASE/source-update_${ts}"
	echo "== source incremental update ($run_dir) =="
	"$BIN" source-update \
		--workspace "ato=$PAGES_DIR" \
		--workspace "frl=$FRL_DIR" \
		--run-dir "$run_dir"
}

case "$ATO_MODE" in
incremental) run_incremental ;;
catch_up)
	run_catch_up
	run_incremental
	;;
full)
	run_full
	run_incremental
	;;
*)
	echo "unknown LEGAL_MCP_ATO_MODE=$ATO_MODE (incremental|catch_up|full)" >&2
	exit 2
	;;
esac

AFTER_COUNT=$(wc -l <"$PAGES_DIR/index.jsonl" 2>/dev/null || echo 0)
AFTER_HASH=$(source_hash)
echo "index.jsonl rows: $BEFORE_COUNT -> $AFTER_COUNT"
echo "combined source sha256: $BEFORE_HASH -> $AFTER_HASH"

FORCE=false
case "$FORCE_REBUILD" in
1 | true | TRUE | yes | YES | on | ON) FORCE=true ;;
esac

if [[ "$FORCE" != true && "$ATO_MODE" != "full" && "$AFTER_HASH" == "$BEFORE_HASH" ]]; then
	echo "no source index changes; skipping rebuild+release"
	exit 0
fi

BUILD_ID="$(date -u +%Y%m%dT%H%M%SZ)"
TAG="${LEGAL_MCP_RELEASE_TAG:-corpus-$BUILD_ID}"
RELEASE_DIR="$REPO_DIR/release/build-$BUILD_ID"
mkdir -p "$RELEASE_DIR"
BUILD_CACHE_ARGS=()
if [[ -n "${LEGAL_MCP_EMBEDDING_CACHE_DB:-}" ]]; then
	[[ -f "$LEGAL_MCP_EMBEDDING_CACHE_DB" ]] || die "embedding cache database not found: $LEGAL_MCP_EMBEDDING_CACHE_DB"
	BUILD_CACHE_ARGS=(--embedding-cache-db "$LEGAL_MCP_EMBEDDING_CACHE_DB")
fi
echo "== build corpus =="
"$BIN" build \
	--pages-dir "$PAGES_DIR" \
	--frl-workspace "$FRL_DIR" \
	--db-path "$RELEASE_DIR/legal.db" \
	--model-dir "$MODEL_DIR" \
	--out-dir "$RELEASE_DIR" \
	"${BUILD_CACHE_ARGS[@]}" \
	--profile

echo "== package and publish corpus to $TAG =="
LEGAL_MCP_RELEASE_DIR="$RELEASE_DIR" \
	LEGAL_MCP_BIN="$BIN" \
	LEGAL_MCP_GH_REPO="$GH_REPO" \
	LEGAL_MCP_ZSTD_LEVEL="$ZSTD_LEVEL" \
	LEGAL_MCP_MODEL_URL="$MODEL_URL" \
	LEGAL_MCP_MODEL_SHA256="$MODEL_SHA256" \
	LEGAL_MCP_MODEL_SIZE="$MODEL_SIZE" \
	"$REPO_DIR/scripts/publish-release.sh" "$TAG" "$GH_REPO"

echo "== done: corpus shipped to $TAG ($((AFTER_COUNT - BEFORE_COUNT)) new rows) =="
