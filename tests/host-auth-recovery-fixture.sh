#!/usr/bin/env bash
# Run as root in the disposable production image with configure-auth mounted at
# /configure-auth and update-image at /update-image. Host commands are
# deterministic fakes; no network is used.
set -euo pipefail
umask 077
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

[[ $EUID -eq 0 && -x /configure-auth && -x /update-image ]] || exit 2
groupadd --gid 971 legal-mcp
useradd --uid 971 --gid 971 --home-dir /nonexistent --no-create-home legal-mcp
groupadd --gid 973 legal-mcp-publisher
install -d -o root -g legal-mcp-publisher -m 0710 /run/legal-mcp
install -o root -g legal-mcp-publisher -m 0640 /dev/null \
  /run/lock/legal-mcp-host-transaction.lock
install -d -o root -g root -m 0700 /etc/legal-mcp
printf '%s\n' 192.0.2.1 > /etc/legal-mcp/admin-source-ip
chmod 600 /etc/legal-mcp/admin-source-ip
install -d -o root -g root -m 0750 /srv/legal-mcp/lifecycle
printf '%064d\n' 1 > /srv/legal-mcp/lifecycle/active-generation
log=/tmp/host-actions.log
: > "$log"
touch /tmp/caddy-active /tmp/caddy-enabled /tmp/ufw-web-open

cat > /usr/bin/systemctl <<'EOF'
#!/usr/bin/bash
printf 'systemctl:%s\n' "$*" >> /tmp/host-actions.log
case "$1" in
  disable)
    if [[ "$*" == *caddy.service* ]]; then rm -f /tmp/caddy-active /tmp/caddy-enabled; fi
    ;;
  enable)
    if [[ "$*" == *caddy.service* ]]; then touch /tmp/caddy-enabled; fi
    ;;
  start)
    if [[ "$*" == *caddy.service* ]]; then touch /tmp/caddy-active; fi
    ;;
  stop)
    if [[ "$*" == *caddy.service* ]]; then rm -f /tmp/caddy-active; fi
    ;;
  is-enabled)
    if [[ "$2" = legal-mcp.service ]]; then printf '%s\n' generated; exit 0; fi
    if [[ -e /tmp/caddy-enabled ]]; then printf '%s\n' enabled; exit 0; fi
    printf '%s\n' disabled
    exit 1
    ;;
  is-active)
    if [[ "$*" == *caddy.service* ]]; then
      if [[ -e /tmp/caddy-active ]]; then printf '%s\n' active; exit 0; fi
      printf '%s\n' inactive
      exit 3
    fi
    printf '%s\n' active
    exit 0
    ;;
  *) exit 0 ;;
esac
EOF
cat > /usr/sbin/ufw <<'EOF'
#!/usr/bin/bash
printf 'ufw:%s\n' "$*" >> /tmp/host-actions.log
if [[ "$1" = status ]]; then
  cat <<'STATUS'
Status: active
Default: deny (incoming), allow (outgoing), disabled (routed)
22/tcp                     ALLOW IN    192.0.2.1                 # restricted SSH administration
STATUS
  if [[ -e /tmp/ufw-web-open ]]; then
    printf '%s\n' \
      '80/tcp                     ALLOW IN    Anywhere                  # Caddy ACME HTTP' \
      '443/tcp                    ALLOW IN    Anywhere                  # Australian Legal MCP HTTPS'
  fi
  exit 0
fi
if [[ "$*" == '--force delete allow 80/tcp' \
  || "$*" == '--force delete allow 443/tcp' ]]; then
  rm -f /tmp/ufw-web-open
  exit 0
fi
if [[ "$1" = allow ]]; then
  touch /tmp/ufw-web-open
  exit 0
fi
exit 1
EOF
cat > /usr/bin/curl <<'EOF'
#!/usr/bin/bash
headers=''
url=''
write_status=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dump-header) headers="$2"; shift 2 ;;
    --write-out) write_status=true; shift 2 ;;
    http://*|https://*) url="$1"; shift ;;
    --header|--data|--request|--max-time|--max-redirs|--output) shift 2 ;;
    *) shift ;;
  esac
done
printf 'curl:%s\n' "$url" >> /tmp/host-actions.log
case "$url" in
  http://127.0.0.1:51235/readyz)
    printf '{"generation":"%064d","status":"ok"}\n' 1
    ;;
  */.well-known/oauth-protected-resource/mcp)
    printf '{"resource":"https://legal.example.com/mcp"}\n'
    ;;
  */mcp)
    if [[ -n "$headers" ]]; then
      mode="$(awk -F= '$1 == "LEGAL_MCP_HTTP_AUTH" {print $2}' /etc/legal-mcp/.auth-transaction/runtime.env)"
      printf 'HTTP/1.1 401 Unauthorized\r\n' > "$headers"
      if [[ "$mode" == *api-key* ]]; then
        printf 'WWW-Authenticate: ApiKey realm="australian-legal-mcp"\r\n' >> "$headers"
      fi
      if [[ "$mode" == *entra* ]]; then
        printf 'WWW-Authenticate: Bearer resource_metadata="https://legal.example.com/.well-known/oauth-protected-resource/mcp"\r\n' >> "$headers"
      fi
      printf '\r\n' >> "$headers"
    fi
    if [[ "$write_status" = true ]]; then printf '401'; fi
    ;;
  *) exit 1 ;;
esac
EOF
chmod 755 /usr/bin/systemctl /usr/sbin/ufw /usr/bin/curl

transaction=/etc/legal-mcp/.auth-transaction
install -d -o root -g root -m 0700 "$transaction"
cat > "$transaction/runtime.env" <<'EOF'
LEGAL_MCP_HTTP_AUTH=entra
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
EOF
printf '{"keys":[],"version":1}\n' > "$transaction/api-keys.json"
touch "$transaction/service-was-enabled" "$transaction/service-was-active" \
  "$transaction/caddy-was-enabled" "$transaction/caddy-was-active" \
  "$transaction/public-was-open"

/configure-auth --recover >/tmp/recovery.stdout
grep -Fxq 'interrupted authentication transaction rolled back' /tmp/recovery.stdout
[[ ! -e "$transaction" ]]
private_line="$(grep -nF 'curl:http://127.0.0.1:51235/mcp' "$log" | cut -d: -f1)"
open_line="$(grep -nF 'ufw:allow 80/tcp comment Caddy ACME HTTP' "$log" | cut -d: -f1)"
public_line="$(grep -nF 'curl:https://legal.example.com/mcp' "$log" | tail -n1 | cut -d: -f1)"
[[ "$private_line" -lt "$open_line" && "$open_line" -lt "$public_line" ]]

# The initial cutover can restore the deliberately disabled private baseline.
install -d -o root -g root -m 0700 "$transaction"
cat > "$transaction/runtime.env" <<'EOF'
LEGAL_MCP_HTTP_AUTH=disabled
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
EOF
printf '{"keys":[],"version":1}\n' > "$transaction/api-keys.json"
/configure-auth --recover >/dev/null
[[ ! -e "$transaction" ]]

# API-key-only recovery must consume a key and prove a positive private call.
python3 - <<'PY' &
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        body=json.dumps({"result":{"serverInfo":{"name":"australian-legal-mcp"}}}).encode()
        self.send_response(200); self.send_header("Content-Type","application/json")
        self.send_header("Content-Length",str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *_): pass
HTTPServer(("127.0.0.1",51235),Handler).serve_forever()
PY
server_pid=$!
trap 'kill "$server_pid" >/dev/null 2>&1 || true' EXIT
server_ready=false
for _ in $(seq 1 100); do
  if python3 -c 'import socket; s=socket.create_connection(("127.0.0.1",51235),.1); s.close()' 2>/dev/null; then
    server_ready=true
    break
  fi
  kill -0 "$server_pid"
  sleep 0.02
done
[[ "$server_ready" = true ]]
install -d -o root -g root -m 0700 "$transaction"
cat > "$transaction/runtime.env" <<'EOF'
LEGAL_MCP_HTTP_AUTH=api-key
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
EOF
printf '{"keys":[],"version":1}\n' > "$transaction/api-keys.json"
touch "$transaction/service-was-enabled" "$transaction/service-was-active"
printf '%s\n' 'automation.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' |
  /configure-auth --recover >/dev/null
[[ ! -e "$transaction" ]]
kill "$server_pid"
wait "$server_pid" 2>/dev/null || true
trap - EXIT

: > "$log"
install -d -o root -g root -m 0700 "$transaction"
cat > "$transaction/runtime.env" <<'EOF'
LEGAL_MCP_HTTP_AUTH=entra+api-key
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
EOF
if /configure-auth --recover </dev/null >/tmp/incomplete.stdout 2>/tmp/incomplete.stderr; then
  echo 'incomplete auth transaction was unexpectedly accepted' >&2
  exit 1
fi
grep -Fq 'systemctl:disable --now caddy.service' "$log"
grep -Fq 'ufw:--force delete allow 80/tcp' "$log"
[[ -d "$transaction" ]]

# API-key-only image recovery must parse its saved mode only after ingress is
# closed, then reject an incomplete transaction without reaching Podman.
: > "$log"
touch /tmp/ufw-web-open
image_transaction=/etc/legal-mcp/.image-transaction
install -d -o root -g root -m 0700 "$image_transaction"
cat > "$image_transaction/runtime.env" <<'EOF'
LEGAL_MCP_HTTP_AUTH=api-key
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
EOF
if printf '%s\n' 'automation.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' |
  /update-image --recover >/tmp/image-incomplete.stdout 2>/tmp/image-incomplete.stderr; then
  echo 'incomplete image transaction was unexpectedly accepted' >&2
  exit 1
fi
grep -Fq 'image transaction is missing image' /tmp/image-incomplete.stderr
grep -Fq 'systemctl:disable --now caddy.service' "$log"
grep -Fq 'ufw:--force delete allow 80/tcp' "$log"

echo host-auth-recovery-fixture-ok
