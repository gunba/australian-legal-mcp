#!/usr/bin/env bash
# Validate the locally active generation, upload only content-addressed chunks
# absent from Azure Blob Storage, then ask the Azure VM's narrow root helper to
# reconstruct, validate, atomically activate, restart, and roll back on failure.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: deploy-generation.sh \
  --host USER@HOST \
  --blob-base-url https://ACCOUNT.blob.core.windows.net/CONTAINER \
  [--tier Hot|Cool] [--upload-workers 1..8]
EOF
  exit 2
}

HOST=''
BLOB_BASE_URL=''
TIER=Cool
UPLOAD_WORKERS=4
while [[ $# -gt 0 ]]; do
  case "$1" in
    --host) HOST="${2:-}"; shift 2 ;;
    --blob-base-url) BLOB_BASE_URL="${2:-}"; shift 2 ;;
    --tier) TIER="${2:-}"; shift 2 ;;
    --upload-workers) UPLOAD_WORKERS="${2:-}"; shift 2 ;;
    *) usage ;;
  esac
done

[[ "$HOST" =~ ^[A-Za-z_][A-Za-z0-9_-]*@[A-Za-z0-9][A-Za-z0-9.-]+$ ]] || usage
[[ "$BLOB_BASE_URL" =~ ^https://[a-z0-9]{3,24}\.blob\.core\.windows\.net/[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$ ]] || usage
[[ "$TIER" = Hot || "$TIER" = Cool ]] || usage
[[ "$UPLOAD_WORKERS" =~ ^[1-8]$ ]] || usage
for command_name in az python3 ssh zstd; do
  command -v "$command_name" >/dev/null || { echo "missing $command_name" >&2; exit 2; }
done

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOCAL_DATA_DIR="${LEGAL_MCP_DATA_DIR:-$REPO_DIR/data/runtime}"
BINARY="${LEGAL_MCP_BINARY:-$REPO_DIR/target/release/legal-mcp}"
TRANSPORT="$REPO_DIR/scripts/azure_generation_transport.py"
[[ -x "$BINARY" && -x "$TRANSPORT" ]] || {
  echo "build the release binary and keep the transport script executable" >&2
  exit 2
}
GENERATION="$(<"$LOCAL_DATA_DIR/lifecycle/active-generation")"
[[ "$GENERATION" =~ ^[0-9a-f]{64}$ ]] || { echo "local active-generation is malformed" >&2; exit 2; }
SOURCE="$LOCAL_DATA_DIR/generations/$GENERATION"
[[ -d "$SOURCE" && ! -L "$SOURCE" ]] || { echo "local generation is missing: $SOURCE" >&2; exit 2; }

# This expensive check is deliberate: transport-cache reuse is allowed only
# after the canonical lifecycle has revalidated every local file and executed
# the semantic model successfully.
env LEGAL_MCP_DATA_DIR="$LOCAL_DATA_DIR" "$BINARY" verify --quiet >/dev/null
az account show --output none

python3 "$TRANSPORT" upload \
  --generation-dir "$SOURCE" \
  --destination "$BLOB_BASE_URL" \
  --token-mode azure-cli \
  --tier "$TIER" \
  --workers "$UPLOAD_WORKERS" \
  --cache-dir "$REPO_DIR/data/cache/azure-transport"

ssh \
  -o BatchMode=yes \
  -o ServerAliveInterval=30 \
  -o ServerAliveCountMax=120 \
  "$HOST" \
  sudo -n /usr/local/sbin/legal-mcp-azure-deploy "$GENERATION"

echo "deployed and verified generation $GENERATION on Azure host $HOST"
