#!/usr/bin/env bash
# Install the CPU serving binary, ONNX Runtime, hardened systemd service,
# content-addressed deployment helper, and optional public Caddy ingress.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: configure-azure-host.sh \
  --host USER@HOST \
  --public-host NAME.REGION.cloudapp.azure.com \
  --blob-base-url https://ACCOUNT.blob.core.windows.net/CONTAINER \
  --binary PATH/TO/legal-mcp \
  --onnx-runtime PATH/TO/libonnxruntime.so
EOF
  exit 2
}

HOST=''
PUBLIC_HOST=''
BLOB_BASE_URL=''
BINARY=''
ONNX_RUNTIME=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --host) HOST="${2:-}"; shift 2 ;;
    --public-host) PUBLIC_HOST="${2:-}"; shift 2 ;;
    --blob-base-url) BLOB_BASE_URL="${2:-}"; shift 2 ;;
    --binary) BINARY="${2:-}"; shift 2 ;;
    --onnx-runtime) ONNX_RUNTIME="${2:-}"; shift 2 ;;
    *) usage ;;
  esac
done

[[ "$HOST" =~ ^[A-Za-z_][A-Za-z0-9_-]*@[A-Za-z0-9][A-Za-z0-9.-]+$ ]] || usage
[[ "$PUBLIC_HOST" =~ ^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]\.[a-z0-9]+\.cloudapp\.azure\.com$ ]] || {
  echo "public host must be the canonical Azure public-IP FQDN" >&2
  exit 2
}
[[ "$BLOB_BASE_URL" =~ ^https://[a-z0-9]{3,24}\.blob\.core\.windows\.net/[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$ ]] || {
  echo "Blob base URL is malformed" >&2
  exit 2
}
for path in "$BINARY" "$ONNX_RUNTIME"; do
  [[ -f "$path" && ! -L "$path" ]] || { echo "missing regular file: $path" >&2; exit 2; }
done
for command_name in curl sha512sum ssh scp python3; do
  command -v "$command_name" >/dev/null || { echo "missing $command_name" >&2; exit 2; }
done

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SSH=(ssh -o BatchMode=yes -o ServerAliveInterval=30 -o ServerAliveCountMax=120 "$HOST")
SCP=(scp -o BatchMode=yes -o ServerAliveInterval=30 -o ServerAliveCountMax=120)
LOCAL_STAGE="$(mktemp -d)"
REMOTE_STAGE=''
cleanup() {
  rm -rf "$LOCAL_STAGE"
  if [[ -n "$REMOTE_STAGE" ]]; then
    "${SSH[@]}" "rm -rf '$REMOTE_STAGE'" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

CADDY_VERSION=2.11.4
CADDY_DEB="caddy_${CADDY_VERSION}_linux_amd64.deb"
CADDY_SHA512=1c6f5404f3622e46d401d81f4af59677d46b886229c6694d60fd936b87c72d3bb5d1fcf42b55c8d555769fa75acf434ab618fc7e0df2c79cf8512ee580d38d06
curl --fail --location --retry 5 \
  --output "$LOCAL_STAGE/$CADDY_DEB" \
  "https://github.com/caddyserver/caddy/releases/download/v$CADDY_VERSION/$CADDY_DEB"
printf '%s  %s\n' "$CADDY_SHA512" "$LOCAL_STAGE/$CADDY_DEB" | sha512sum --check -

python3 - "$REPO_DIR/infra/hosting/Caddyfile" "$LOCAL_STAGE/Caddyfile" "$PUBLIC_HOST" <<'PY'
import pathlib, sys
source = pathlib.Path(sys.argv[1]).read_text()
host = sys.argv[3]
if source.count("__PUBLIC_HOST__") != 1:
    raise SystemExit("Caddy template placeholder contract changed")
pathlib.Path(sys.argv[2]).write_text(source.replace("__PUBLIC_HOST__", host))
PY
python3 - "$LOCAL_STAGE/azure-deployment.json" "$BLOB_BASE_URL" <<'PY'
import json, pathlib, sys
pathlib.Path(sys.argv[1]).write_text(json.dumps({
    "format_version": 1,
    "blob_base_url": sys.argv[2],
}, sort_keys=True) + "\n")
PY

cp "$REPO_DIR/systemd/legal-mcp.service" "$LOCAL_STAGE/"
cp "$REPO_DIR/systemd/legal-mcp.env.example" "$LOCAL_STAGE/"
cp "$REPO_DIR/scripts/legal-mcp-azure-deploy" "$LOCAL_STAGE/"
cp "$REPO_DIR/scripts/legal-mcp-publisher-command" "$LOCAL_STAGE/"
cp "$REPO_DIR/scripts/azure_generation_transport.py" "$LOCAL_STAGE/"
cp "$BINARY" "$LOCAL_STAGE/legal-mcp"
cp "$ONNX_RUNTIME" "$LOCAL_STAGE/libonnxruntime.so"

REMOTE_STAGE="$("${SSH[@]}" 'mktemp -d /tmp/legal-mcp-install.XXXXXXXX')"
[[ "$REMOTE_STAGE" =~ ^/tmp/legal-mcp-install\.[A-Za-z0-9]+$ ]] || {
  echo "remote staging path is unsafe" >&2
  exit 2
}
"${SCP[@]}" "$LOCAL_STAGE"/* "$HOST:$REMOTE_STAGE/"

"${SSH[@]}" "REMOTE_STAGE='$REMOTE_STAGE' bash -s" <<'REMOTE'
set -euo pipefail
sudo mountpoint -q /var/lib/australian-legal-mcp
sudo test -f /var/lib/australian-legal-mcp/.legal-mcp-data-volume
sudo test "$(sudo findmnt -n -o FSTYPE --target /var/lib/australian-legal-mcp)" = xfs
sudo xfs_info /var/lib/australian-legal-mcp | grep -Eq 'reflink=1'
sudo systemctl disable --now caddy.service || true
sudo systemctl mask caddy.service

sudo apt-get update
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y "$REMOTE_STAGE/caddy_2.11.4_linux_amd64.deb"
sudo systemctl unmask caddy.service
sudo systemctl disable --now caddy.service || true
sudo install -d -o root -g root -m 0755 \
  /usr/local/lib/australian-legal-mcp \
  /usr/local/libexec/australian-legal-mcp
sudo install -o root -g root -m 0755 "$REMOTE_STAGE/legal-mcp" /usr/local/bin/legal-mcp
sudo install -o root -g root -m 0644 "$REMOTE_STAGE/libonnxruntime.so" \
  /usr/local/lib/australian-legal-mcp/libonnxruntime.so
sudo install -o root -g root -m 0755 "$REMOTE_STAGE/azure_generation_transport.py" \
  /usr/local/libexec/australian-legal-mcp/azure_generation_transport.py
sudo install -o root -g root -m 0750 "$REMOTE_STAGE/legal-mcp-azure-deploy" \
  /usr/local/sbin/legal-mcp-azure-deploy
sudo install -o root -g root -m 0755 "$REMOTE_STAGE/legal-mcp-publisher-command" \
  /usr/local/sbin/legal-mcp-publisher-command
sudo install -o root -g root -m 0644 "$REMOTE_STAGE/legal-mcp.service" \
  /etc/systemd/system/legal-mcp.service
if ! sudo test -e /etc/australian-legal-mcp/legal-mcp.env; then
  sudo install -o root -g legal-mcp -m 0640 "$REMOTE_STAGE/legal-mcp.env.example" \
    /etc/australian-legal-mcp/legal-mcp.env
fi
sudo install -o root -g legal-mcp -m 0640 "$REMOTE_STAGE/azure-deployment.json" \
  /etc/australian-legal-mcp/azure-deployment.json
sudo install -o root -g caddy -m 0640 "$REMOTE_STAGE/Caddyfile" /etc/caddy/Caddyfile

printf '%s\n' \
  'legal-mcp-publisher ALL=(root) NOPASSWD: /usr/local/sbin/legal-mcp-azure-deploy *' \
  | sudo tee /etc/sudoers.d/legal-mcp-azure-deploy >/dev/null
sudo chmod 0440 /etc/sudoers.d/legal-mcp-azure-deploy
sudo visudo --check --file=/etc/sudoers.d/legal-mcp-azure-deploy

sudo env ORT_DYLIB_PATH=/usr/local/lib/australian-legal-mcp/libonnxruntime.so \
  /usr/local/bin/legal-mcp verify-runtime
caddy version | grep -F 'v2.11.4'
sudo caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
sudo rm -f /etc/systemd/system/legal-mcp.service.d/10-require-http-auth.conf
sudo systemctl daemon-reload
sudo systemctl enable legal-mcp.service
sudo systemctl is-enabled legal-mcp.service
REMOTE

echo "configured Azure host $HOST; public Caddy ingress remains disabled until Entra auth is configured"
