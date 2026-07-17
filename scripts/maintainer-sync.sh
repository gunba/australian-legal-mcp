#!/usr/bin/env bash
# Refresh official source workspaces, build one complete local generation on
# this maintainer PC, validate/activate it atomically, and retain rollback data.
# Corpus/model bytes are never published or downloaded by the runtime.
set -euo pipefail

REQUESTED_MODE=incremental
if (($# > 1)); then
  echo "usage: $0 [--full]" >&2
  exit 2
fi
if (($# == 1)); then
  [[ "$1" == "--full" ]] || { echo "usage: $0 [--full]" >&2; exit 2; }
  REQUESTED_MODE=full
fi

if [[ -z "${LEGAL_MCP_SLEEP_INHIBITED:-}" && -z "${LEGAL_MCP_ALLOW_SLEEP:-}" ]]; then
  if command -v systemd-inhibit >/dev/null 2>&1 &&
    systemd-inhibit --who=legal-mcp --what=sleep --mode=block \
      --why="Australian Legal MCP maintainer sync probe" true >/dev/null 2>&1; then
    export LEGAL_MCP_SLEEP_INHIBITED=1
    exec systemd-inhibit --who=legal-mcp --what=sleep:idle:handle-lid-switch \
      --mode=block --why="Australian Legal MCP maintainer sync" "$0" "$@"
  fi
fi

REPO_DIR="${LEGAL_MCP_REPO_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
DATA_ROOT="${LEGAL_MCP_PROJECT_DATA_DIR:-$REPO_DIR/data}"
SOURCE_DATA_DIR="${LEGAL_MCP_SOURCE_DATA_DIR:-$DATA_ROOT/sources}"
MODEL_DIR="${LEGAL_MCP_MODEL_DIR:-$DATA_ROOT/models/mdbr-leaf-ir-standard}"
RUNTIME_DIR="${LEGAL_MCP_DATA_DIR:-$DATA_ROOT/runtime}"
BUILD_ROOT="$DATA_ROOT/builds"
RUN_ROOT="$DATA_ROOT/runs"
CACHE_ROOT="$DATA_ROOT/cache"
LOG_ROOT="$DATA_ROOT/logs"
SNAPSHOT_ROOT="$DATA_ROOT/source-snapshots"
PENDING_FILE="$RUN_ROOT/pending-generation.json"
BIN="${LEGAL_MCP_BIN:-$REPO_DIR/target/release/legal-mcp}"
FORCE_REBUILD="${LEGAL_MCP_FORCE_REBUILD:-}"

cd "$REPO_DIR"
for command_name in flock python3 unrtf antiword soffice pdftotext pdftoppm tesseract; do
  command -v "$command_name" >/dev/null 2>&1 || { echo "missing command: $command_name" >&2; exit 2; }
done
[[ -x "$BIN" ]] || { echo "legal-mcp binary not found at $BIN" >&2; exit 2; }
for model_file in "$MODEL_DIR/onnx/model.onnx" "$MODEL_DIR/tokenizer.json"; do
  [[ -f "$model_file" && ! -L "$model_file" ]] || { echo "required model file missing: $model_file" >&2; exit 2; }
done
mkdir -p "$SOURCE_DATA_DIR" "$RUNTIME_DIR" "$BUILD_ROOT" "$RUN_ROOT" "$CACHE_ROOT" "$LOG_ROOT" \
  "$SNAPSHOT_ROOT/rollback" "$SNAPSHOT_ROOT/full-refresh" "$SNAPSHOT_ROOT/failed"
exec 9>"$RUN_ROOT/maintainer-sync.lock"
flock -n 9 || { echo "another maintainer sync is already running" >&2; exit 2; }

write_pending() {
  local phase="$1" previous="${2:-}" active="${3:-}" fingerprint="${4:-}"
  python3 - "$PENDING_FILE" "$RUN_ID" "$UPDATE_MODE" "$phase" "$RUN_DIR" "$BUILD_DIR" \
    "$FRESH_ROOT" "$previous" "$active" "$fingerprint" <<'PY'
import json, os, sys
path, run_id, mode, phase, run_dir, build_dir, fresh_root, previous, active, fingerprint = sys.argv[1:]
payload = {"schema_version": 1, "run_id": run_id, "mode": mode, "phase": phase,
           "run_dir": run_dir, "build_dir": build_dir, "fresh_root": fresh_root,
           "previous_generation": previous or None, "active_generation": active or None,
           "fresh_source_fingerprint": fingerprint or None}
tmp = path + ".tmp"
with open(tmp, "w", encoding="utf-8") as f:
    json.dump(payload, f, sort_keys=True, indent=2); f.write("\n"); f.flush(); os.fsync(f.fileno())
os.replace(tmp, path)
fd = os.open(os.path.dirname(path), os.O_RDONLY | os.O_DIRECTORY)
try: os.fsync(fd)
finally: os.close(fd)
PY
}

remove_pending() {
  python3 - "$PENDING_FILE" <<'PY'
import os, sys
path=sys.argv[1]
try: os.unlink(path)
except FileNotFoundError: pass
fd=os.open(os.path.dirname(path), os.O_RDONLY | os.O_DIRECTORY)
try: os.fsync(fd)
finally: os.close(fd)
PY
}

source_set_fingerprint() {
  local root="$1"; shift
  python3 - "$root" "$@" <<'PY'
import hashlib, pathlib, sys
root=pathlib.Path(sys.argv[1]); h=hashlib.sha256()
for source in sorted(sys.argv[2:]):
    state=root/source/"state.json"
    if not state.is_file() or state.is_symlink(): raise SystemExit(f"missing real source state: {state}")
    h.update(source.encode()); h.update(b"\0"); h.update(state.read_bytes()); h.update(b"\0")
print(h.hexdigest())
PY
}

rename_path() {
  python3 - "$1" "$2" <<'PY'
import os, sys
source, destination = sys.argv[1:]
if os.path.lexists(destination): raise SystemExit(f"rename destination exists: {destination}")
os.rename(source, destination)
for directory in {os.path.dirname(source), os.path.dirname(destination)}:
    fd=os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try: os.fsync(fd)
    finally: os.close(fd)
PY
}

active_key() {
  local path="$RUNTIME_DIR/lifecycle/active-generation"
  [[ -f "$path" ]] || return 0
  local key; key="$(<"$path")"
  [[ "$key" =~ ^[0-9a-f]{64}$ ]] || { echo "malformed active-generation" >&2; return 2; }
  printf '%s' "$key"
}

if [[ -n "${LEGAL_MCP_CUDA_LIB_PATH:-}" ]]; then
  export LD_LIBRARY_PATH="$LEGAL_MCP_CUDA_LIB_PATH:${LD_LIBRARY_PATH:-}"
else
  CUDA_LIB_DIRS=()
  shopt -s nullglob
  for nvidia_root in "$REPO_DIR"/.venv/lib/python*/site-packages/nvidia "$HOME"/.local/lib/python*/site-packages/nvidia; do
    for component_lib in "$nvidia_root"/*/lib; do [[ -d "$component_lib" ]] && CUDA_LIB_DIRS+=("$component_lib"); done
  done
  for tensorrt_lib in "$REPO_DIR"/.venv/lib/python*/site-packages/tensorrt_libs "$HOME"/.local/lib/python*/site-packages/tensorrt_libs; do
    [[ -d "$tensorrt_lib" ]] && CUDA_LIB_DIRS+=("$tensorrt_lib")
  done
  shopt -u nullglob
  if ((${#CUDA_LIB_DIRS[@]})); then
    printf -v CUDA_LIB_PATH '%s:' "${CUDA_LIB_DIRS[@]}"
    export LD_LIBRARY_PATH="${CUDA_LIB_PATH%:}:${LD_LIBRARY_PATH:-}"
  fi
fi
export MALLOC_ARENA_MAX="${MALLOC_ARENA_MAX:-24}"
case "$FORCE_REBUILD" in 1|true|TRUE|yes|YES|on|ON) FORCE_REBUILD=true ;; *) FORCE_REBUILD=false ;; esac

mapfile -t SOURCE_IDS < <("$BIN" source-list)
((${#SOURCE_IDS[@]})) || { echo "production source catalogue is empty" >&2; exit 2; }
[[ ! -L "$SOURCE_DATA_DIR" && -d "$SOURCE_DATA_DIR" ]] || { echo "source root must be a real directory" >&2; exit 2; }

RESUMING=false
PHASE=""
PREVIOUS_GENERATION=""
NEW_GENERATION=""
FRESH_FINGERPRINT=""
if [[ -f "$PENDING_FILE" ]]; then
  mapfile -t pending < <(python3 - "$PENDING_FILE" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
if x.get("schema_version") != 1: raise SystemExit("unsupported pending-generation schema")
for key in ("run_id","mode","phase","run_dir","build_dir","fresh_root"):
    print(x.get(key) or "")
print(x.get("previous_generation") or "")
print(x.get("active_generation") or "")
print(x.get("fresh_source_fingerprint") or "")
PY
  )
  RUN_ID="${pending[0]}"; UPDATE_MODE="${pending[1]}"; PHASE="${pending[2]}"
  RUN_DIR="${pending[3]}"; BUILD_DIR="${pending[4]}"; FRESH_ROOT="${pending[5]}"
  PREVIOUS_GENERATION="${pending[6]}"; NEW_GENERATION="${pending[7]}"; FRESH_FINGERPRINT="${pending[8]}"
  RESUMING=true
  if [[ "$REQUESTED_MODE" == full && "$UPDATE_MODE" != full ]]; then
    echo "finish the pending incremental generation before requesting --full" >&2; exit 2
  fi
fi

if [[ "$RESUMING" != true ]]; then
  UPDATE_MODE="$REQUESTED_MODE"
  RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)"
  RUN_DIR="$RUN_ROOT/$RUN_ID"
  BUILD_DIR="$BUILD_ROOT/generation-$RUN_ID"
  if [[ "$UPDATE_MODE" == full ]]; then FRESH_ROOT="$SNAPSHOT_ROOT/full-refresh/$RUN_ID/sources"; else FRESH_ROOT=""; fi
  PHASE=acquiring
  write_pending "$PHASE"
fi

LOG="$LOG_ROOT/maintainer-sync-$RUN_ID.log"
exec > >(tee -a "$LOG") 2>&1
echo "== $(date -u +%FT%TZ) local maintainer sync (mode: $UPDATE_MODE, phase: $PHASE) =="

declare -A UPDATE_WORKSPACES
if [[ "$UPDATE_MODE" == full ]]; then
  mkdir -p "$FRESH_ROOT"
  for source in "${SOURCE_IDS[@]}"; do UPDATE_WORKSPACES["$source"]="$FRESH_ROOT/$source"; done
else
  for source in "${SOURCE_IDS[@]}"; do UPDATE_WORKSPACES["$source"]="$SOURCE_DATA_DIR/$source"; done
fi

if [[ "$PHASE" == acquiring ]]; then
  UPDATE_ARGS=()
  for source in "${SOURCE_IDS[@]}"; do
    mkdir -p "${UPDATE_WORKSPACES[$source]}"
    if [[ "$UPDATE_MODE" == full && -f "${UPDATE_WORKSPACES[$source]}/state.json" ]]; then
      echo "full refresh source already committed; retaining $source"
      continue
    fi
    UPDATE_ARGS+=(--workspace "$source=${UPDATE_WORKSPACES[$source]}")
  done
  if ((${#UPDATE_ARGS[@]})); then
    [[ "$UPDATE_MODE" == full ]] && UPDATE_ARGS+=(--full)
    echo "== source $UPDATE_MODE update ($RUN_DIR) =="
    UPDATE_JSON="$("$BIN" source-update "${UPDATE_ARGS[@]}" --run-dir "$RUN_DIR")"
    echo "$UPDATE_JSON"
    SOURCE_CHANGED="$(python3 -c 'import json,sys; print("true" if any(x.get("changed") for x in json.load(sys.stdin)["sources"]) else "false")' <<<"$UPDATE_JSON")"
  else
    UPDATE_JSON='{"sources":[]}'
    SOURCE_CHANGED=true
    echo "all full-refresh sources were already committed"
  fi
  if [[ "$FORCE_REBUILD" != true && "$UPDATE_MODE" != full && "$SOURCE_CHANGED" != true && "$RESUMING" != true ]]; then
    remove_pending
    echo "no source inventory changes; local active generation is unchanged"
    exit 0
  fi
  if [[ "$UPDATE_MODE" == full ]]; then FRESH_FINGERPRINT="$(source_set_fingerprint "$FRESH_ROOT" "${SOURCE_IDS[@]}")"; fi
  PHASE=build
  write_pending "$PHASE" "" "" "$FRESH_FINGERPRINT"
fi

if [[ "$PHASE" == build ]]; then
  mkdir -p "$BUILD_DIR"
  if command -v chattr >/dev/null 2>&1; then
    chattr +C "$BUILD_DIR" 2>/dev/null || true
  fi
  export LEGAL_MCP_TENSORRT_CACHE_DIR="$CACHE_ROOT/tensorrt/$RUN_ID"
  mkdir -p "$LEGAL_MCP_TENSORRT_CACHE_DIR"
  BUILD_CACHE_ARGS=()
  if [[ -n "${LEGAL_MCP_EMBEDDING_CACHE_DB:-}" ]]; then
    [[ -f "$LEGAL_MCP_EMBEDDING_CACHE_DB" ]] || { echo "embedding cache DB missing" >&2; exit 2; }
    BUILD_CACHE_ARGS=(--embedding-cache-db "$LEGAL_MCP_EMBEDDING_CACHE_DB")
  fi
  BUILD_SOURCE_ARGS=()
  for source in "${SOURCE_IDS[@]}"; do BUILD_SOURCE_ARGS+=(--source-workspace "$source=${UPDATE_WORKSPACES[$source]}"); done
  if [[ ! -f "$BUILD_DIR/generation.json" ]]; then
    echo "== build local generation =="
    "$BIN" build "${BUILD_SOURCE_ARGS[@]}" --model-dir "$MODEL_DIR" --out-dir "$BUILD_DIR" \
      "${BUILD_CACHE_ARGS[@]}" --profile
  fi
  PREVIOUS_GENERATION="$(active_key)"
  PHASE=activating
  write_pending "$PHASE" "$PREVIOUS_GENERATION" "" "$FRESH_FINGERPRINT"
fi

if [[ "$PHASE" == activating ]]; then
  if [[ -d "$BUILD_DIR" ]]; then
    ACTIVATION_JSON="$(LEGAL_MCP_DATA_DIR="$RUNTIME_DIR" "$BIN" activate --generation-dir "$BUILD_DIR")"
    echo "$ACTIVATION_JSON"
    NEW_GENERATION="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["active_generation"])' <<<"$ACTIVATION_JSON")"
    PREVIOUS_GENERATION="$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("previous_generation") or "")' <<<"$ACTIVATION_JSON")"
  else
    NEW_GENERATION="$(active_key)"
    [[ -n "$NEW_GENERATION" && "$NEW_GENERATION" != "$PREVIOUS_GENERATION" ]] || {
      echo "activation was interrupted before its outcome could be recovered" >&2; exit 1;
    }
  fi
  LEGAL_MCP_DATA_DIR="$RUNTIME_DIR" "$BIN" verify >/dev/null
  PHASE=activated
  write_pending "$PHASE" "$PREVIOUS_GENERATION" "$NEW_GENERATION" "$FRESH_FINGERPRINT"
fi

if [[ "$UPDATE_MODE" == full && "$PHASE" == activated ]]; then
  echo "== atomically promote complete full source set =="
  current_fingerprint="$(source_set_fingerprint "$SOURCE_DATA_DIR" "${SOURCE_IDS[@]}")"
  if [[ "$current_fingerprint" != "$FRESH_FINGERPRINT" ]]; then
    [[ -d "$FRESH_ROOT" && ! -L "$FRESH_ROOT" ]] || { echo "fresh source set missing: $FRESH_ROOT" >&2; exit 1; }
    mv -T --exchange --no-copy "$FRESH_ROOT" "$SOURCE_DATA_DIR"
    current_fingerprint="$(source_set_fingerprint "$SOURCE_DATA_DIR" "${SOURCE_IDS[@]}")"
    [[ "$current_fingerprint" == "$FRESH_FINGERPRINT" ]] || { echo "source-set exchange verification failed" >&2; exit 1; }
  fi
  if [[ -d "$FRESH_ROOT" ]]; then
    backup="$SNAPSHOT_ROOT/rollback/pre-full-$RUN_ID"
    [[ ! -e "$backup" ]] || { echo "rollback snapshot collision: $backup" >&2; exit 2; }
    mkdir -p "$(dirname "$backup")"
    rename_path "$FRESH_ROOT" "$backup"
  fi
fi

remove_pending
if [[ -n "${LEGAL_MCP_RESTART_USER_SERVICE:-}" ]]; then
  systemctl --user try-restart "$LEGAL_MCP_RESTART_USER_SERVICE"
fi
echo "== active local generation: $NEW_GENERATION =="
echo "Builds and acquisition ran on this PC; no corpus bytes were published."
