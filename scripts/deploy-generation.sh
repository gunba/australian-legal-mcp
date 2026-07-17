#!/usr/bin/env bash
# Strictly verify the active local generation, delta-copy it through the
# restricted publisher account, and request transactional host activation.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: deploy-generation.sh --host legal-mcp-publisher@HOST
       deploy-generation.sh --host legal-mcp-publisher@HOST --abort GENERATION

SSH identity selection belongs in ~/.ssh/config (use IdentitiesOnly yes).
LEGAL_MCP_DATA_DIR and LEGAL_MCP_BINARY may override their normal local paths.
Abort is explicit and never runs automatically after an upload or activation failure.
EOF
  exit 2
}

HOST=''
ABORT=false
ABORT_GENERATION=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --host)
      [[ $# -ge 2 && -z "$HOST" ]] || usage
      HOST="$2"
      shift 2
      ;;
    --abort)
      [[ $# -ge 2 && "$ABORT" = false ]] || usage
      ABORT=true
      ABORT_GENERATION="$2"
      shift 2
      ;;
    *) usage ;;
  esac
done

[[ "$HOST" =~ ^legal-mcp-publisher@[A-Za-z0-9][A-Za-z0-9.-]+$ \
  && "$HOST" != *@*..* \
  && "$HOST" != *@*. ]] || usage
[[ "$ABORT" = false || "$ABORT_GENERATION" =~ ^[0-9a-f]{64}$ ]] || usage

SSH_OPTIONS=(
  -o BatchMode=yes
  -o ConnectTimeout=15
  -o ServerAliveInterval=30
  -o ServerAliveCountMax=120
)

if [[ "$ABORT" = true ]]; then
  command -v ssh >/dev/null || { echo 'missing ssh' >&2; exit 2; }
  # shellcheck disable=SC2029 # The exact validated generation is intentionally expanded locally.
  abort_result="$(ssh "${SSH_OPTIONS[@]}" "$HOST" "abort $ABORT_GENERATION")"
  case "$abort_result" in
    aborted)
      echo "aborted generation $ABORT_GENERATION on $HOST"
      ;;
    already-aborted)
      echo "generation $ABORT_GENERATION is already aborted on $HOST"
      ;;
    *)
      echo 'unexpected abort response from deployment host' >&2
      exit 1
      ;;
  esac
  exit 0
fi

for command_name in rsync ssh; do
  command -v "$command_name" >/dev/null || { echo "missing $command_name" >&2; exit 2; }
done

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOCAL_DATA_DIR="${LEGAL_MCP_DATA_DIR:-$REPO_DIR/data/runtime}"
BINARY="${LEGAL_MCP_BINARY:-$REPO_DIR/target/release/legal-mcp}"
[[ -x "$BINARY" ]] || { echo "build the release binary first: $BINARY" >&2; exit 2; }

POINTER="$LOCAL_DATA_DIR/lifecycle/active-generation"
[[ -f "$POINTER" && ! -L "$POINTER" ]] || { echo "local active-generation is missing" >&2; exit 2; }
GENERATION="$(<"$POINTER")"
[[ "$GENERATION" =~ ^[0-9a-f]{64}$ ]] || { echo "local active-generation is malformed" >&2; exit 2; }
SOURCE="$LOCAL_DATA_DIR/generations/$GENERATION"
[[ -d "$SOURCE" && ! -L "$SOURCE" ]] || { echo "local generation is missing: $SOURCE" >&2; exit 2; }

# This expensive check is deliberate. It hashes every artifact and executes the
# semantic model while holding the shared corpus lock before any remote bytes
# are changed.
env LEGAL_MCP_DATA_DIR="$LOCAL_DATA_DIR" "$BINARY" verify --quiet >/dev/null

# shellcheck disable=SC2029 # The validated generation is intentionally expanded locally.
prepare_result="$(ssh "${SSH_OPTIONS[@]}" "$HOST" "prepare $GENERATION")"
SKIP_UPLOAD=false
case "$prepare_result" in
  prepared) ;;
  staged) SKIP_UPLOAD=true ;;
  already-active)
    echo "generation $GENERATION is already active on $HOST"
    exit 0
    ;;
  *)
    echo "unexpected prepare response from deployment host" >&2
    exit 1
    ;;
esac

# The remote helper CoW-clones the active generation first. --checksum and the
# rsync delta algorithm then transmit only changed blocks; --inplace preserves
# unchanged reflink extents and interrupted transfers resume in the same upload.
if [[ "$SKIP_UPLOAD" = false ]]; then
  RSYNC_RSH='ssh -o BatchMode=yes -o ConnectTimeout=15 -o ServerAliveInterval=30 -o ServerAliveCountMax=120'
  export RSYNC_RSH
  rsync \
    --recursive \
    --links \
    --times \
    --checksum \
    --inplace \
    --no-whole-file \
    --partial \
    --delete-delay \
    --safe-links \
    --chmod=Du=rwx,Dgo=,Fu=rw,Fgo= \
    --info=progress2,stats2 \
    "$SOURCE/" "$HOST:$GENERATION/"
fi

# shellcheck disable=SC2029 # The validated generation is intentionally expanded locally.
activate_result="$(ssh "${SSH_OPTIONS[@]}" "$HOST" "activate $GENERATION")"
case "$activate_result" in
  activated)
    echo "deployed and verified generation $GENERATION on $HOST"
    ;;
  activated-pending-auth)
    echo "activated bootstrap generation $GENERATION on $HOST; configure authentication before starting ingress"
    ;;
  *)
    echo "unexpected activation response from deployment host" >&2
    exit 1
    ;;
esac
