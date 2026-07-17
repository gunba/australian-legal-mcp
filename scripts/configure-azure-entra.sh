#!/usr/bin/env bash
# Transactionally enable single-tenant Entra authorization and only then expose
# Caddy. The MCP VM stores no client secret.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: configure-azure-entra.sh \
  --host ADMIN_USER@HOST \
  --public-host NAME.REGION.cloudapp.azure.com \
  --tenant-id UUID \
  --server-app-id UUID \
  --allowed-client-id UUID [--allowed-client-id UUID ...] \
  [--audience UUID|api://UUID ...] [--scope legal.read]
EOF
  exit 2
}

HOST=''
PUBLIC_HOST=''
TENANT_ID=''
SERVER_APP_ID=''
SCOPE=legal.read
ALLOWED_CLIENT_IDS=()
AUDIENCES=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --host) HOST="${2:-}"; shift 2 ;;
    --public-host) PUBLIC_HOST="${2:-}"; shift 2 ;;
    --tenant-id) TENANT_ID="${2:-}"; shift 2 ;;
    --server-app-id) SERVER_APP_ID="${2:-}"; shift 2 ;;
    --allowed-client-id) ALLOWED_CLIENT_IDS+=("${2:-}"); shift 2 ;;
    --audience) AUDIENCES+=("${2:-}"); shift 2 ;;
    --scope) SCOPE="${2:-}"; shift 2 ;;
    *) usage ;;
  esac
done

UUID_RE='^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
[[ "$HOST" =~ ^[A-Za-z_][A-Za-z0-9_-]*@[A-Za-z0-9][A-Za-z0-9.-]+$ ]] || usage
[[ "$PUBLIC_HOST" =~ ^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]\.[a-z0-9]+\.cloudapp\.azure\.com$ ]] || usage
[[ "$TENANT_ID" =~ $UUID_RE && "$SERVER_APP_ID" =~ $UUID_RE ]] || usage
[[ ${#ALLOWED_CLIENT_IDS[@]} -gt 0 && ${#ALLOWED_CLIENT_IDS[@]} -le 32 ]] || usage
for client_id in "${ALLOWED_CLIENT_IDS[@]}"; do
  [[ "$client_id" =~ $UUID_RE ]] || usage
done
[[ "$(printf '%s\n' "${ALLOWED_CLIENT_IDS[@]}" | sort -u | wc -l)" = "${#ALLOWED_CLIENT_IDS[@]}" ]] || usage
[[ "$SCOPE" =~ ^[A-Za-z0-9._-]{1,128}$ ]] || usage
SCOPE_URI="api://$SERVER_APP_ID/$SCOPE"
if [[ ${#AUDIENCES[@]} = 0 ]]; then
  AUDIENCES=("$SERVER_APP_ID" "api://$SERVER_APP_ID")
fi
[[ ${#AUDIENCES[@]} -le 8 ]] || usage
canonical_audience=false
for audience in "${AUDIENCES[@]}"; do
  [[ ${#audience} -le 512 && "$audience" =~ ^[A-Za-z0-9:/._-]+$ ]] || usage
  if [[ "$audience" = "$SERVER_APP_ID" || "$audience" = "api://$SERVER_APP_ID" ]]; then
    canonical_audience=true
  fi
done
[[ "$canonical_audience" = true ]] || usage
[[ "$(printf '%s\n' "${AUDIENCES[@]}" | sort -u | wc -l)" = "${#AUDIENCES[@]}" ]] || usage
for command_name in curl scp ssh python3; do
  command -v "$command_name" >/dev/null || { echo "missing $command_name" >&2; exit 2; }
done

join_by_comma() {
  local IFS=,
  printf '%s' "$*"
}
AUDIENCE_LIST="$(join_by_comma "${AUDIENCES[@]}")"
CLIENT_LIST="$(join_by_comma "${ALLOWED_CLIENT_IDS[@]}")"
TEMP="$(mktemp)"
REMOTE_TEMP=''
CUTOVER_STAGED=false
SSH=(ssh -o BatchMode=yes -o ServerAliveInterval=30 -o ServerAliveCountMax=120 "$HOST")
cleanup() {
  status=$?
  rm -f "$TEMP"
  if [[ -n "$REMOTE_TEMP" ]]; then
    "${SSH[@]}" "rm -f '$REMOTE_TEMP'" >/dev/null 2>&1 || true
  fi
  if [[ "$status" != 0 && "$CUTOVER_STAGED" = true ]]; then
    rollback_remote >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

python3 - "$TEMP" "$PUBLIC_HOST" "$TENANT_ID" "$SERVER_APP_ID" "$AUDIENCE_LIST" "$SCOPE" "$SCOPE_URI" "$CLIENT_LIST" <<'PY'
import pathlib, sys
path, host, tenant, server_app_id, audiences, scope, scope_uri, clients = sys.argv[1:]
values = {
    "LEGAL_MCP_DATA_DIR": "/var/lib/australian-legal-mcp",
    "LEGAL_MCP_HTTP_WORKERS": "2",
    "LEGAL_MCP_SHUTDOWN_GRACE_SECONDS": "30",
    "ORT_DYLIB_PATH": "/usr/local/lib/australian-legal-mcp/libonnxruntime.so",
    "LEGAL_MCP_HTTP_AUTH": "entra",
    "LEGAL_MCP_EXTERNAL_URL": f"https://{host}/mcp",
    "LEGAL_MCP_ENTRA_TENANT_ID": tenant,
    "LEGAL_MCP_ENTRA_SERVER_APP_ID": server_app_id,
    "LEGAL_MCP_ENTRA_AUDIENCES": audiences,
    "LEGAL_MCP_ENTRA_SCOPE": scope,
    "LEGAL_MCP_ENTRA_SCOPE_URI": scope_uri,
    "LEGAL_MCP_ENTRA_ALLOWED_CLIENT_IDS": clients,
}
for name, value in values.items():
    if not value.isascii() or any(ch.isspace() or ord(ch) < 0x20 for ch in value):
        raise SystemExit(f"unsafe environment value for {name}")
pathlib.Path(path).write_text("".join(f"{name}={value}\n" for name, value in values.items()))
PY
chmod 0600 "$TEMP"
REMOTE_TEMP="$("${SSH[@]}" 'mktemp /tmp/legal-mcp-env.XXXXXXXX')"
[[ "$REMOTE_TEMP" =~ ^/tmp/legal-mcp-env\.[A-Za-z0-9]+$ ]] || { echo "unsafe remote temp path" >&2; exit 2; }
scp -o BatchMode=yes "$TEMP" "$HOST:$REMOTE_TEMP"

rollback_remote() {
  "${SSH[@]}" 'bash -s' <<'REMOTE_ROLLBACK'
set -euo pipefail
backup=/var/lib/australian-legal-mcp/.entra-transaction
sudo test -d "$backup" || exit 0
sudo systemctl disable --now caddy.service || true
if sudo test -f "$backup/env"; then
  sudo install -o root -g legal-mcp -m 0640 "$backup/env" /etc/australian-legal-mcp/legal-mcp.env
else
  sudo rm -f /etc/australian-legal-mcp/legal-mcp.env
fi
if sudo test -f "$backup/dropin"; then
  sudo install -d -o root -g root -m 0755 /etc/systemd/system/legal-mcp.service.d
  sudo install -o root -g root -m 0644 "$backup/dropin" \
    /etc/systemd/system/legal-mcp.service.d/10-require-http-auth.conf
else
  sudo rm -f /etc/systemd/system/legal-mcp.service.d/10-require-http-auth.conf
fi
sudo systemctl daemon-reload
sudo systemctl restart legal-mcp.service || true
if sudo test -f "$backup/caddy-was-active"; then
  sudo systemctl enable --now caddy.service || true
fi
sudo rm -rf "$backup"
REMOTE_ROLLBACK
}

# Recover a cutover interrupted after staging but before the external probe.
if "${SSH[@]}" 'sudo test -d /var/lib/australian-legal-mcp/.entra-transaction'; then
  rollback_remote
fi

"${SSH[@]}" "REMOTE_TEMP='$REMOTE_TEMP' EXPECTED_HOST='$PUBLIC_HOST' EXPECTED_SCOPE='$SCOPE_URI' bash -s" <<'REMOTE'
set -euo pipefail
exec 9>/run/lock/australian-legal-mcp-config.lock
flock -n 9
backup=/var/lib/australian-legal-mcp/.entra-transaction
sudo rm -rf "$backup"
sudo install -d -o root -g root -m 0700 "$backup"
if sudo test -f /etc/australian-legal-mcp/legal-mcp.env; then
  sudo cp --preserve=mode,ownership /etc/australian-legal-mcp/legal-mcp.env "$backup/env"
fi
if sudo test -f /etc/systemd/system/legal-mcp.service.d/10-require-http-auth.conf; then
  sudo cp --preserve=mode,ownership \
    /etc/systemd/system/legal-mcp.service.d/10-require-http-auth.conf "$backup/dropin"
fi
if sudo systemctl is-active --quiet caddy.service \
  && sudo test -f "$backup/env" \
  && sudo grep -Fxq 'LEGAL_MCP_HTTP_AUTH=entra' "$backup/env" \
  && sudo test -f "$backup/dropin"; then
  sudo touch "$backup/caddy-was-active"
fi
rollback() {
  sudo systemctl disable --now caddy.service || true
  if sudo test -f "$backup/env"; then
    sudo install -o root -g legal-mcp -m 0640 "$backup/env" /etc/australian-legal-mcp/legal-mcp.env
  else
    sudo rm -f /etc/australian-legal-mcp/legal-mcp.env
  fi
  if sudo test -f "$backup/dropin"; then
    sudo install -d -o root -g root -m 0755 /etc/systemd/system/legal-mcp.service.d
    sudo install -o root -g root -m 0644 "$backup/dropin" \
      /etc/systemd/system/legal-mcp.service.d/10-require-http-auth.conf
  else
    sudo rm -f /etc/systemd/system/legal-mcp.service.d/10-require-http-auth.conf
  fi
  sudo systemctl daemon-reload
  sudo systemctl restart legal-mcp.service || true
  if sudo test -f "$backup/caddy-was-active"; then
    sudo systemctl enable --now caddy.service || true
  fi
  sudo rm -rf "$backup"
}
trap rollback ERR INT TERM
sudo systemctl disable --now caddy.service || true
sudo install -o root -g legal-mcp -m 0640 "$REMOTE_TEMP" \
  /etc/australian-legal-mcp/legal-mcp.env
rm -f "$REMOTE_TEMP"
sudo install -d -o root -g root -m 0755 /etc/systemd/system/legal-mcp.service.d
cat <<'EOF' | sudo tee /etc/systemd/system/legal-mcp.service.d/10-require-http-auth.conf >/dev/null
[Service]
ExecStart=
ExecStart=/usr/local/bin/legal-mcp serve --port 51235 --require-ready-corpus --require-http-auth
EOF
sudo chmod 0644 /etc/systemd/system/legal-mcp.service.d/10-require-http-auth.conf
sudo systemctl daemon-reload
sudo systemctl restart legal-mcp.service
ready=false
for _ in $(seq 1 300); do
  body="$(curl --fail --silent --show-error --connect-timeout 1 --max-time 3 \
    http://127.0.0.1:51235/readyz 2>/dev/null || true)"
  if [[ -n "$body" ]] && jq -e '.status == "ok" and (.generation | type == "string")' \
    <<<"$body" >/dev/null; then
    ready=true
    break
  fi
  sleep 1
done
[[ "$ready" = true ]]
headers="$(mktemp /tmp/legal-mcp-auth-headers.XXXXXXXX)"
status="$(curl --silent --output /dev/null --dump-header "$headers" --write-out '%{http_code}' \
  --connect-timeout 1 --max-time 3 --max-redirs 0 \
  -X POST -H 'content-type: application/json' \
  -H 'accept: application/json, text/event-stream' \
  -H 'mcp-protocol-version: 2025-06-18' \
  --data '{"jsonrpc":"2.0","id":1,"method":"ping"}' \
  http://127.0.0.1:51235/mcp)"
[[ "$status" = 401 ]]
grep -Fqi 'www-authenticate: Bearer ' "$headers"
grep -Fq "resource_metadata=\"https://$EXPECTED_HOST/.well-known/oauth-protected-resource/mcp\"" "$headers"
rm -f "$headers"
metadata="$(curl --fail --silent --show-error --connect-timeout 1 --max-time 3 --max-redirs 0 \
  http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp)"
jq -e --arg resource "https://$EXPECTED_HOST/mcp" --arg scope "$EXPECTED_SCOPE" \
  '.resource == $resource and .scopes_supported == [$scope]' <<<"$metadata" >/dev/null
sudo systemctl enable --now caddy.service
trap - ERR INT TERM
REMOTE
CUTOVER_STAGED=true
REMOTE_TEMP=''

READY=false
for _ in $(seq 1 60); do
  if metadata="$(curl --fail --silent --show-error --connect-timeout 3 --max-time 10 \
    --max-redirs 0 "https://$PUBLIC_HOST/.well-known/oauth-protected-resource/mcp" 2>/dev/null)" && \
    jq -e --arg resource "https://$PUBLIC_HOST/mcp" --arg scope "$SCOPE_URI" \
      '.resource == $resource and .scopes_supported == [$scope]' \
      <<<"$metadata" >/dev/null; then
    READY=true
    break
  fi
  sleep 5
done
if [[ "$READY" != true ]]; then
  rollback_remote || true
  CUTOVER_STAGED=false
  echo "public TLS/metadata probe failed; the previous environment and Caddy state were restored" >&2
  exit 1
fi
PUBLIC_HEADERS="$(mktemp)"
PUBLIC_STATUS="$(curl --silent --output /dev/null --dump-header "$PUBLIC_HEADERS" \
  --write-out '%{http_code}' --connect-timeout 3 --max-time 10 --max-redirs 0 \
  -X POST -H 'content-type: application/json' \
  -H 'accept: application/json, text/event-stream' \
  -H 'mcp-protocol-version: 2025-06-18' \
  --data '{"jsonrpc":"2.0","id":1,"method":"ping"}' \
  "https://$PUBLIC_HOST/mcp" || true)"
if [[ "$PUBLIC_STATUS" != 401 ]] \
  || ! grep -Fqi 'www-authenticate: Bearer ' "$PUBLIC_HEADERS" \
  || ! grep -Fq "resource_metadata=\"https://$PUBLIC_HOST/.well-known/oauth-protected-resource/mcp\"" "$PUBLIC_HEADERS"; then
  rm -f "$PUBLIC_HEADERS"
  rollback_remote || true
  CUTOVER_STAGED=false
  echo "public MCP challenge probe failed; the previous environment and Caddy state were restored" >&2
  exit 1
fi
rm -f "$PUBLIC_HEADERS"
"${SSH[@]}" 'sudo rm -rf /var/lib/australian-legal-mcp/.entra-transaction'
CUTOVER_STAGED=false

echo "Entra authorization and public HTTPS are enabled at https://$PUBLIC_HOST/mcp"
