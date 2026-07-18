#!/usr/bin/env bash
# Exercise the exact V2 auth journal, immutable host contract, listener/firewall
# ordering, rollback, retirement, and the one-shot v0.19.2 recovery path.
set -euo pipefail
umask 077
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

[[ $EUID -eq 0 && -x /configure-auth && -x /install-host ]] || exit 2

version=0.19.4
revision=1111111111111111111111111111111111111111
generation=a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3
old_image="ghcr.io/gunba/australian-legal-mcp@sha256:$(printf 'a%.0s' {1..64})"
auth_impl=/usr/local/sbin/legal-mcp-configure-auth
implementation_dir=/usr/local/libexec/legal-mcp/host-tools
launcher=/usr/local/libexec/legal-mcp/host-tool-launcher
launcher_marker=/etc/legal-mcp/host-tool-launcher
configure_pointer=/etc/legal-mcp/configure-auth-implementation
update_pointer=/etc/legal-mcp/update-image-implementation
auth_ready=/etc/legal-mcp/auth-ready
auth_permit=/run/legal-mcp/auth-configuring
dispatch=/run/legal-mcp/host-tool-launcher-dispatch
transaction=/etc/legal-mcp/.auth-transaction
preparing=${transaction}.preparing
preparing_retired=${transaction}.preparing-retired
retiring=${transaction}.retiring
retired=${transaction}.retired
host_tools=/etc/legal-mcp/host-tools
runtime=/etc/legal-mcp/runtime.env
api_keys=/etc/legal-mcp/api-keys.json
template=/usr/local/libexec/legal-mcp/legal-mcp.container.template
quadlet=/etc/containers/systemd/legal-mcp.container
log=/tmp/host-actions.log
token='automation.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'
wrong_token='automation.BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB'

getent group legal-mcp >/dev/null || groupadd --gid 971 legal-mcp
getent passwd legal-mcp >/dev/null ||
  useradd --uid 971 --gid 971 --home-dir /nonexistent --no-create-home legal-mcp
getent group legal-mcp-publisher >/dev/null || groupadd --gid 973 legal-mcp-publisher
getent group caddy >/dev/null || groupadd --gid 975 caddy
install -d -o root -g root -m 0755 \
  /etc/legal-mcp /etc/containers/systemd /etc/caddy /etc/sudoers.d \
  /usr/local/sbin "$implementation_dir"
install -d -o root -g legal-mcp-publisher -m 0710 /run/legal-mcp
install -o root -g legal-mcp-publisher -m 0640 /dev/null \
  /run/lock/legal-mcp-host-transaction.lock
install -d -o root -g legal-mcp -m 0750 \
  /srv/legal-mcp/lifecycle /srv/legal-mcp/generations \
  "/srv/legal-mcp/generations/$generation"
printf '%s' "$generation" > /srv/legal-mcp/lifecycle/active-generation
chown root:root /srv/legal-mcp/lifecycle/active-generation
chmod 644 /srv/legal-mcp/lifecycle/active-generation
printf '%s\n' 192.0.2.1 > /etc/legal-mcp/admin-source-ip
chmod 600 /etc/legal-mcp/admin-source-ip
printf '%s\n' "$old_image" > /etc/legal-mcp/image
chmod 600 /etc/legal-mcp/image
configure_sha="$(sha256sum /configure-auth | awk '{print $1}')"
install -o root -g root -m 0755 /configure-auth "$auth_impl"
install -o root -g root -m 0755 /configure-auth \
  "$implementation_dir/configure-auth.$configure_sha"

cat > /usr/local/sbin/legal-mcp-host-deploy <<'EOF'
#!/usr/bin/bash
exit 0
EOF
cat > /usr/local/sbin/legal-mcp-publisher-command <<'EOF'
#!/usr/bin/bash
exit 0
EOF
cat > /tmp/update-image-implementation <<'EOF'
#!/usr/bin/bash
exit 0
EOF
update_sha="$(sha256sum /tmp/update-image-implementation | awk '{print $1}')"
install -o root -g root -m 0755 /tmp/update-image-implementation \
  /usr/local/sbin/legal-mcp-update-image
install -o root -g root -m 0755 /tmp/update-image-implementation \
  "$implementation_dir/update-image.$update_sha"
chmod 755 /usr/local/sbin/legal-mcp-host-deploy \
  /usr/local/sbin/legal-mcp-publisher-command
printf '%s\n' 'Defaults:legal-mcp-publisher !requiretty' \
  > /etc/sudoers.d/legal-mcp-publisher
chmod 440 /etc/sudoers.d/legal-mcp-publisher

cat > "$template" <<'EOF'
[Unit]
Description=fixture
ExecCondition=/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready

[Container]
Image=__IMAGE_DIGEST__
PublishPort=127.0.0.1:51235:51235

[Install]
WantedBy=multi-user.target
EOF
chmod 644 "$template"
sed "s|__IMAGE_DIGEST__|$old_image|g" "$template" > "$quadlet"
chmod 644 "$quadlet"

cat > /etc/caddy/Caddyfile <<'EOF'
{
	servers {
		timeouts {
			read_body 30s
			read_header 10s
			write 5m
			idle 5m
		}
	}
}

http://legal.example.com {
	respond "not found" 404
}

https://legal.example.com {
	encode zstd gzip

	@mcp path /mcp /.well-known/oauth-protected-resource/mcp
	handle @mcp {
		request_body {
			max_size 1MB
		}
		header {
			-Server
			Cache-Control "no-store"
			Strict-Transport-Security "max-age=31536000"
			X-Content-Type-Options "nosniff"
		}
		reverse_proxy 127.0.0.1:51235 {
			flush_interval -1
			transport http {
				dial_timeout 5s
				response_header_timeout 310s
				read_timeout 310s
				write_timeout 310s
				max_conns_per_host 8
			}
		}
	}

	handle {
		respond "not found" 404
	}
}
EOF
chown root:caddy /etc/caddy/Caddyfile
chmod 640 /etc/caddy/Caddyfile
install -o root -g caddy -m 0640 /etc/caddy/Caddyfile /tmp/expected-Caddyfile

write_caddy_json() {
  python3 - /tmp/caddy-adapted.json <<'PY'
import json, sys
host = "legal.example.com"
timeouts = {
    "read_timeout": 30_000_000_000,
    "read_header_timeout": 10_000_000_000,
    "write_timeout": 300_000_000_000,
    "idle_timeout": 300_000_000_000,
}
https_routes = [
    {"handle": [{"encodings": {"gzip": {}, "zstd": {}}, "handler": "encode", "prefer": ["zstd", "gzip"]}]},
    {"group": "group2", "handle": [{"handler": "subroute", "routes": [{"handle": [
        {"handler": "headers", "response": {"deferred": True, "delete": ["Server"], "set": {
            "Cache-Control": ["no-store"], "Strict-Transport-Security": ["max-age=31536000"],
            "X-Content-Type-Options": ["nosniff"]}}},
        {"handler": "request_body", "max_size": 1_000_000},
        {"flush_interval": -1, "handler": "reverse_proxy", "transport": {
            "dial_timeout": 5_000_000_000, "max_conns_per_host": 8, "protocol": "http",
            "read_timeout": 310_000_000_000, "response_header_timeout": 310_000_000_000,
            "write_timeout": 310_000_000_000}, "upstreams": [{"dial": "127.0.0.1:51235"}]},
    ]}]}], "match": [{"path": ["/mcp", "/.well-known/oauth-protected-resource/mcp"]}]},
    {"group": "group2", "handle": [{"handler": "subroute", "routes": [{"handle": [
        {"body": "not found", "handler": "static_response", "status_code": 404}
    ]}]}]},
]
value = {"apps": {"http": {"servers": {
    "srv0": {"listen": [":443"], **timeouts, "routes": [{"match": [{"host": [host]}],
        "handle": [{"handler": "subroute", "routes": https_routes}], "terminal": True}]},
    "srv1": {"listen": [":80"], **timeouts, "routes": [{"match": [{"host": [host]}],
        "handle": [{"handler": "subroute", "routes": [{"handle": [
            {"body": "not found", "handler": "static_response", "status_code": 404}
        ]}]}], "terminal": True}]},
}}}}
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    json.dump(value, handle, separators=(",", ":"))
PY
}
write_caddy_json

cat > /usr/bin/caddy <<'EOF'
#!/usr/bin/bash
printf 'caddy:%s\n' "$*" >> /tmp/host-actions.log
[[ "$1" = adapt && "$*" == *'--validate'* ]] || exit 91
cmp --silent /tmp/expected-Caddyfile /etc/caddy/Caddyfile || exit 92
[[ ! -e /tmp/fail-caddy-adapt ]] || exit 93
cat /tmp/caddy-adapted.json
EOF

cat > /usr/bin/systemctl <<'EOF'
#!/usr/bin/bash
printf 'systemctl:%s\n' "$*" >> /tmp/host-actions.log
unit=''
for argument in "$@"; do
  case "$argument" in legal-mcp.service|caddy.service) unit="$argument" ;; esac
done
case "$1" in
  is-enabled)
    if [[ "$unit" = legal-mcp.service ]]; then printf '%s\n' generated; exit 0; fi
    if [[ -e /tmp/caddy-enabled ]]; then printf '%s\n' enabled; exit 0; fi
    printf '%s\n' disabled; exit 1
    ;;
  is-active)
    if [[ "$unit" = legal-mcp.service ]]; then flag=service; else flag=caddy; fi
    if [[ -e "/tmp/${flag}-active" ]]; then printf '%s\n' active; exit 0; fi
    printf '%s\n' inactive; exit 3
    ;;
  disable)
    [[ "$unit" != legal-mcp.service ]] || exit 64
    if [[ "$unit" = caddy.service ]]; then
      if [[ -e /tmp/kill-auth-on-caddy-disable ]]; then
        rm -f /tmp/kill-auth-on-caddy-disable
        kill -KILL "$PPID"
        sleep 1
      fi
      rm -f /tmp/caddy-enabled
      [[ "$*" != *--now* ]] || rm -f /tmp/caddy-active
    fi
    ;;
  enable)
    [[ "$unit" != legal-mcp.service ]] || exit 64
    [[ "$unit" = caddy.service ]] || exit 65
    if [[ -e /tmp/fail-caddy-enable-once ]]; then
      rm -f /tmp/fail-caddy-enable-once
      exit 69
    fi
    touch /tmp/caddy-enabled
    [[ "$*" != *--now* ]] || touch /tmp/caddy-active
    ;;
  start)
    if [[ "$unit" = caddy.service ]]; then
      touch /tmp/caddy-active
    else
      { [[ -f /etc/legal-mcp/auth-ready && ! -L /etc/legal-mcp/auth-ready ]] \
        || [[ -f /run/legal-mcp/auth-configuring && ! -L /run/legal-mcp/auth-configuring ]]; } || exit 66
      touch /tmp/service-active
    fi
    ;;
  restart)
    [[ "$unit" = legal-mcp.service ]] || exit 67
    { [[ -f /etc/legal-mcp/auth-ready && ! -L /etc/legal-mcp/auth-ready ]] \
      || [[ -f /run/legal-mcp/auth-configuring && ! -L /run/legal-mcp/auth-configuring ]]; } || {
      rm -f /tmp/service-active
      exit 68
    }
    touch /tmp/service-active
    ;;
  stop)
    if [[ "$unit" = caddy.service ]]; then rm -f /tmp/caddy-active; else rm -f /tmp/service-active; fi
    ;;
  daemon-reload) ;;
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
  [[ ! -e /tmp/ufw-80 ]] || printf '%s\n' '80/tcp                     ALLOW IN    Anywhere                  # Caddy ACME HTTP'
  [[ ! -e /tmp/ufw-443 ]] || printf '%s\n' '443/tcp                    ALLOW IN    Anywhere                  # Australian Legal MCP HTTPS'
  [[ ! -e /tmp/ufw-extra ]] || printf '%s\n' '9999/tcp                   ALLOW IN    Anywhere'
  exit 0
fi
if [[ "$*" = '--force delete allow 80/tcp' ]]; then rm -f /tmp/ufw-80; exit 0; fi
if [[ "$*" = '--force delete allow 443/tcp' ]]; then rm -f /tmp/ufw-443; exit 0; fi
if [[ "$1" = allow && "$2" = 80/tcp ]]; then touch /tmp/ufw-80; exit 0; fi
if [[ "$1" = allow && "$2" = 443/tcp ]]; then touch /tmp/ufw-443; exit 0; fi
exit 1
EOF

cat > /usr/bin/ss <<'EOF'
#!/usr/bin/bash
printf 'ss:%s\n' "$*" >> /tmp/host-actions.log
[[ ! -e /tmp/fail-ss ]] || exit 86
if [[ -e /tmp/service-active ]]; then
  if [[ -e /tmp/wildcard-service-listener ]]; then
    printf '%s\n' 'LISTEN 0 4096 0.0.0.0:51235 0.0.0.0:*'
  else
    printf '%s\n' 'LISTEN 0 4096 127.0.0.1:51235 0.0.0.0:*'
  fi
fi
if [[ -e /tmp/caddy-active ]]; then
  printf '%s\n' \
    'LISTEN 0 4096 *:80 *:*' \
    'LISTEN 0 4096 *:443 *:*'
fi
EOF

cat > /usr/bin/curl <<'EOF'
#!/usr/bin/bash
headers=''
output=''
url=''
write_status=false
fail=false
read_config=false
method=GET
while [[ $# -gt 0 ]]; do
  case "$1" in
    --config) [[ "$2" = - ]] || exit 90; read_config=true; shift 2 ;;
    --dump-header) headers="$2"; shift 2 ;;
    --output) output="$2"; shift 2 ;;
    --write-out) write_status=true; shift 2 ;;
    --request) method="$2"; shift 2 ;;
    --fail) fail=true; shift ;;
    http://*|https://*) url="$1"; shift ;;
    --header|--data|--max-time|--max-redirs) shift 2 ;;
    *) shift ;;
  esac
done
api_key=''
if [[ "$read_config" = true ]]; then
  IFS= read -r config_line
  [[ "$config_line" =~ ^header\ =\ \"X-API-Key:\ ([A-Za-z0-9._-]+)\"$ ]] || exit 91
  api_key="${BASH_REMATCH[1]}"
fi
printf 'curl:%s:%s\n' "$method" "$url" >> /tmp/host-actions.log
status=404
body=''
case "$url" in
  http://127.0.0.1:51235/readyz)
    [[ -e /tmp/service-active ]] || exit 7
    status=200
    body='{"generation":"a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3","status":"ok"}'
    ;;
  http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp|https://legal.example.com/.well-known/oauth-protected-resource/mcp)
    status=200
    body='{"resource":"https://legal.example.com/mcp"}'
    ;;
  http://127.0.0.1:51235/mcp|https://legal.example.com/mcp)
    if [[ -n "$api_key" ]]; then
      expected="$(</tmp/expected-api-key)"
      if [[ "$api_key" = "$expected" ]]; then
        status=200
        body='{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"australian-legal-mcp"}}}'
        printf '%s\n' valid >> /tmp/api-server.log
      else
        status=401
        printf '%s\n' invalid >> /tmp/api-server.log
      fi
    else
      status=401
    fi
    ;;
  *) status=404 ;;
esac
if [[ -n "$headers" ]]; then
  printf 'HTTP/1.1 %s Fixture\r\n' "$status" > "$headers"
  if [[ "$status" = 401 ]]; then
    mode="$(awk -F= '$1 == "LEGAL_MCP_HTTP_AUTH" {print $2}' /etc/legal-mcp/runtime.env)"
    [[ "$mode" != *api-key* ]] || printf 'WWW-Authenticate: ApiKey realm="australian-legal-mcp"\r\n' >> "$headers"
    [[ "$mode" != *entra* ]] || printf 'WWW-Authenticate: Bearer resource_metadata="https://legal.example.com/.well-known/oauth-protected-resource/mcp"\r\n' >> "$headers"
  fi
  printf '\r\n' >> "$headers"
fi
if [[ -n "$output" ]]; then
  [[ -z "$body" ]] || printf '%s\n' "$body" > "$output"
elif [[ -n "$body" ]]; then
  printf '%s\n' "$body"
fi
[[ "$write_status" = false ]] || printf '%s' "$status"
if [[ "$fail" = true && "$status" -ge 400 ]]; then exit 22; fi
EOF
chmod 755 /usr/bin/caddy /usr/bin/systemctl /usr/sbin/ufw /usr/bin/ss /usr/bin/curl

write_host_tools_v2() {
  local deploy_sha publisher_sha template_sha sudoers_sha
  deploy_sha="$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}')"
  publisher_sha="$(sha256sum /usr/local/sbin/legal-mcp-publisher-command | awk '{print $1}')"
  template_sha="$(sha256sum "$template" | awk '{print $1}')"
  sudoers_sha="$(sha256sum /etc/sudoers.d/legal-mcp-publisher | awk '{print $1}')"
  cat > "$host_tools" <<EOF
LEGAL_MCP_HOST_TOOLS_V2
VERSION=$version
SOURCE_COMMIT=$revision
HOST_DEPLOY_SHA256=$deploy_sha
PUBLISHER_COMMAND_SHA256=$publisher_sha
CONFIGURE_AUTH_SHA256=$configure_sha
UPDATE_IMAGE_SHA256=$update_sha
CONTAINER_TEMPLATE_SHA256=$template_sha
SUDOERS_SHA256=$sudoers_sha
EOF
  chown root:root "$host_tools"
  chmod 444 "$host_tools"
}
write_host_tools_v2

real_launcher=/tmp/real-host-tool-launcher
awk '
  /^  cat <<'\''LAUNCHER'\''$/ { in_launcher=1; next }
  in_launcher && /^LAUNCHER$/ { exit }
  in_launcher { print }
' /install-host > "$real_launcher"
chmod 755 "$real_launcher"
install -o root -g root -m 0755 "$real_launcher" "$launcher"
launcher_sha="$(sha256sum "$real_launcher" | awk '{print $1}')"
cat > "$launcher_marker" <<EOF
LEGAL_MCP_HOST_TOOL_LAUNCHER_V1
LAUNCHER_SHA256=$launcher_sha
EOF
chmod 444 "$launcher_marker"
printf '%s' "$configure_sha" > "$configure_pointer"
printf '%s' "$update_sha" > "$update_pointer"
chmod 644 "$configure_pointer" "$update_pointer"

cat > /usr/bin/unshare <<'EOF'
#!/usr/bin/bash
[[ "$1" = --mount && "$2" = --propagation && "$3" = private && "$4" = -- ]] || exit 97
shift 4
exec "$@"
EOF
cat > /usr/bin/mount <<'EOF'
#!/usr/bin/bash
if [[ "$1" = --bind && $# -eq 3 ]]; then
  install -o root -g root -m 0755 "$2" "$3"
  exit 0
fi
if [[ "$1" = -o && "$2" = remount,bind,ro,nodev,nosuid && $# -eq 3 ]]; then
  exit 0
fi
exit 98
EOF
chmod 755 /usr/bin/unshare /usr/bin/mount

restore_public_launchers() {
  install -o root -g root -m 0755 "$real_launcher" "$auth_impl"
  install -o root -g root -m 0755 "$real_launcher" /usr/local/sbin/legal-mcp-update-image
}

run_auth() {
  local status
  restore_public_launchers
  if "$auth_impl" "$@"; then status=0; else status=$?; fi
  restore_public_launchers
  return "$status"
}

restore_public_launchers

write_dark_runtime() {
  cat > "$runtime" <<'EOF'
LEGAL_MCP_HTTP_AUTH=disabled
LEGAL_MCP_API_KEYS_FILE=/run/secrets/legal-mcp-api-keys.json
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
LEGAL_MCP_ALLOWED_ORIGINS=https://legal.example.com
LEGAL_MCP_HTTP_WORKERS=4
LEGAL_MCP_SHUTDOWN_GRACE_SECONDS=30
EOF
  chown root:root "$runtime"
  chmod 600 "$runtime"
  printf '%s\n' '{"keys":[],"version":1}' > "$api_keys"
  chown legal-mcp:legal-mcp "$api_keys"
  chmod 400 "$api_keys"
}

write_api_input() {
  local path="$1" selected_token="$2" digest
  digest="$(printf '%s' "$selected_token" | sha256sum | awk '{print $1}')"
  printf '{"keys":[{"id":"automation","sha256":"%s"}],"version":1}\n' "$digest" > "$path"
  chown root:root "$path"
  chmod 600 "$path"
}

set_dark_state() {
  rm -rf -- "$transaction" "$preparing" "$preparing_retired" "$retiring" "$retired"
  rm -f "$auth_ready" /tmp/service-active /tmp/caddy-active /tmp/caddy-enabled \
    /tmp/ufw-80 /tmp/ufw-443 /tmp/ufw-extra /tmp/wildcard-service-listener
  write_dark_runtime
}

set_public_api_state() {
  local input=/tmp/api-input.json
  write_api_input "$input" "$token"
  install -o root -g root -m 0600 /dev/null "$runtime"
  cat > "$runtime" <<'EOF'
LEGAL_MCP_HTTP_AUTH=api-key
LEGAL_MCP_API_KEYS_FILE=/run/secrets/legal-mcp-api-keys.json
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
LEGAL_MCP_ALLOWED_ORIGINS=https://legal.example.com
LEGAL_MCP_HTTP_WORKERS=4
LEGAL_MCP_SHUTDOWN_GRACE_SECONDS=30
EOF
  install -o legal-mcp -g legal-mcp -m 0400 "$input" "$api_keys"
  install -o root -g root -m 0444 /dev/null "$auth_ready"
  touch /tmp/service-active /tmp/caddy-active /tmp/caddy-enabled /tmp/ufw-80 /tmp/ufw-443
}

run_entra_cutover() {
  run_auth \
    --mode entra \
    --public-host legal.example.com \
    --tenant-id 11111111-1111-1111-1111-111111111111 \
    --server-app-id 22222222-2222-2222-2222-222222222222 \
    --audiences 22222222-2222-2222-2222-222222222222 \
    --scope legal.read \
    --scope-uri api://22222222-2222-2222-2222-222222222222/legal.read \
    --allowed-client-ids 33333333-3333-3333-3333-333333333333
}

recover_public_entra() {
  local output
  [[ -d "$transaction" && ! -e "$auth_ready" \
    && ! -e /tmp/service-active && ! -e /tmp/caddy-active \
    && ! -e /tmp/ufw-80 && ! -e /tmp/ufw-443 ]]
  output="$(run_auth --recover)"
  grep -Fq 'interrupted V2 authentication transaction rolled back' <<< "$output"
  [[ ! -e "$transaction" \
    && "$(stat -c '%U:%G:%a:%h:%s' "$auth_ready")" = root:root:444:1:0 \
    && -e /tmp/service-active && -e /tmp/caddy-active \
    && -e /tmp/ufw-80 && -e /tmp/ufw-443 ]]
}

# A real loopback server proves that urllib sent exactly the intended API key.
printf '%s' "$token" > /tmp/expected-api-key
chmod 600 /tmp/expected-api-key
python3 - <<'PY' &
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        expected=open('/tmp/expected-api-key', encoding='ascii').read()
        supplied=self.headers.get('X-API-Key')
        with open('/tmp/api-server.log','a',encoding='utf-8') as log:
            log.write('valid\n' if supplied == expected else 'invalid\n')
        if self.path != '/mcp' or supplied != expected:
            self.send_response(401); self.end_headers(); return
        body=json.dumps({"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"australian-legal-mcp"}}}).encode()
        self.send_response(200); self.send_header('Content-Type','application/json')
        self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *_): pass
HTTPServer(('127.0.0.1',51235),Handler).serve_forever()
PY
server_pid=$!
trap 'kill "$server_pid" >/dev/null 2>&1 || true' EXIT
for _ in $(seq 1 100); do
  python3 -c 'import socket; s=socket.create_connection(("127.0.0.1",51235),.1); s.close()' 2>/dev/null && break
  kill -0 "$server_pid"
  sleep 0.02
done

: > "$log"
set_dark_state

for hidden_argument in --prepare-auth-dispatch --finalize-auth-ready; do
  if run_auth "$hidden_argument" >/tmp/hidden.stdout 2>/tmp/hidden.stderr; then
    echo 'public launcher accepted a hidden authentication handoff command' >&2
    exit 1
  fi
  [[ ! -e "$transaction" && ! -e "$auth_ready" ]]
done

# Exact pointer semantics: a newline is rejected before any journal is made.
printf '%s\n' "$generation" > /srv/legal-mcp/lifecycle/active-generation
chmod 644 /srv/legal-mcp/lifecycle/active-generation
if run_entra_cutover >/tmp/pointer.stdout 2>/tmp/pointer.stderr; then
  echo 'newline-terminated active pointer was accepted' >&2
  exit 1
fi
grep -Fq 'active-generation must be exactly 64 bytes' /tmp/pointer.stderr
[[ ! -e "$transaction" ]]
printf '%s' "$generation" > /srv/legal-mcp/lifecycle/active-generation
chmod 644 /srv/legal-mcp/lifecycle/active-generation

# Marker/helper/template/Caddy drift fails before journal publication.
cp "$host_tools" /tmp/host-tools.good
sed -i 's/LEGAL_MCP_HOST_TOOLS_V2/LEGAL_MCP_HOST_TOOLS_V1/' "$host_tools"
if run_entra_cutover >/tmp/marker.stdout 2>/tmp/marker.stderr; then
  echo 'non-V2 marker was accepted for normal auth' >&2
  exit 1
fi
install -o root -g root -m 0444 /tmp/host-tools.good "$host_tools"
printf '\n' >> "$configure_pointer"
if run_entra_cutover >/tmp/configure-pointer.stdout 2>/tmp/configure-pointer.stderr; then
  echo 'newline-terminated immutable implementation pointer was accepted' >&2
  exit 1
fi
printf '%s' "$configure_sha" > "$configure_pointer"
chmod 644 "$configure_pointer"
printf '%s\n' '# immutable drift' >> "$implementation_dir/configure-auth.$configure_sha"
chmod 755 "$implementation_dir/configure-auth.$configure_sha"
if run_entra_cutover >/tmp/implementation.stdout 2>/tmp/implementation.stderr; then
  echo 'changed immutable configure-auth implementation was accepted' >&2
  exit 1
fi
install -o root -g root -m 0755 /configure-auth \
  "$implementation_dir/configure-auth.$configure_sha"
: > "$auth_ready"
chmod 444 "$auth_ready"
if run_entra_cutover >/tmp/auth-ready.stdout 2>/tmp/auth-ready.stderr; then
  echo 'implementation accepted auth-ready not removed by its launcher' >&2
  exit 1
fi
rm -f "$auth_ready"
printf '%s\n' '# drift' >> "$quadlet"
if run_entra_cutover >/tmp/quadlet.stdout 2>/tmp/quadlet.stderr; then
  echo 'rendered Quadlet drift was accepted' >&2
  exit 1
fi
sed "s|__IMAGE_DIGEST__|$old_image|g" "$template" > "$quadlet"
chmod 644 "$quadlet"
chmod 644 /etc/caddy/Caddyfile
if run_entra_cutover >/tmp/caddy-mode.stdout 2>/tmp/caddy-mode.stderr; then
  echo 'unsafe Caddyfile mode was accepted' >&2
  exit 1
fi
chown root:caddy /etc/caddy/Caddyfile
chmod 640 /etc/caddy/Caddyfile

# First dark-to-public Entra cutover validates private/listener/Caddy state,
# starts Caddy while UFW is closed, and opens UFW only afterward.
: > "$log"
output="$(run_entra_cutover)"
[[ "$output" = 'authentication configured; exact private/public auth and route probes passed' ]]
[[ "$(stat -c '%U:%G:%a:%h:%s' "$auth_ready")" = root:root:444:1:0 \
  && ! -e "$transaction" \
  && -e /tmp/service-active && -e /tmp/caddy-active \
  && -e /tmp/caddy-enabled && -e /tmp/ufw-80 && -e /tmp/ufw-443 ]]
if grep -Eq '^systemctl:(enable|disable).*legal-mcp\.service' "$log"; then
  echo 'generated Quadlet was passed to enable/disable' >&2
  exit 1
fi
private_line="$(grep -nF 'curl:POST:http://127.0.0.1:51235/mcp' "$log" | head -n1 | cut -d: -f1)"
caddy_line="$(grep -nF 'systemctl:enable --now caddy.service' "$log" | tail -n1 | cut -d: -f1)"
ufw_line="$(grep -nF 'ufw:allow 80/tcp comment Caddy ACME HTTP' "$log" | tail -n1 | cut -d: -f1)"
public_line="$(grep -nF 'curl:POST:https://legal.example.com/mcp' "$log" | tail -n1 | cut -d: -f1)"
[[ "$private_line" -lt "$caddy_line" && "$caddy_line" -lt "$ufw_line" && "$ufw_line" -lt "$public_line" ]]
for route in /mcp/ /.well-known/oauth-protected-resource /readyz /livez; do
  grep -Fq "https://legal.example.com$route" "$log"
done

# SIGKILL after durable baseline preparation but before marker removal keeps
# the exact prior public marker and matrix. Recovery recognizes that this is
# pre-cutover state, not a committed target.
mv /usr/bin/rm /usr/bin/rm.auth-real
cat > /usr/bin/rm <<'EOF'
#!/usr/bin/bash
if [[ -e /tmp/kill-auth-before-marker-removal \
  && "$*" == *'/etc/legal-mcp/auth-ready'* ]]; then
  /usr/bin/rm.auth-real -f /tmp/kill-auth-before-marker-removal
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
exec /usr/bin/rm.auth-real "$@"
EOF
chmod 755 /usr/bin/rm
touch /tmp/kill-auth-before-marker-removal
set +e
run_entra_cutover >/tmp/pre-marker-kill.stdout 2>/tmp/pre-marker-kill.stderr
pre_marker_status=$?
set -e
[[ $pre_marker_status -ne 0 && -d "$transaction" && -e "$auth_ready" \
  && ! -e "$transaction/target" && -e /tmp/service-active \
  && -e /tmp/caddy-active && -e /tmp/ufw-80 && -e /tmp/ufw-443 ]]
pre_marker_recovery="$(run_auth --recover)"
[[ "$pre_marker_recovery" = 'pre-cutover V2 authentication preparation discarded' \
  && ! -e "$transaction" && -e "$auth_ready" \
  && -e /tmp/service-active && -e /tmp/caddy-active ]]
mv /usr/bin/rm.auth-real /usr/bin/rm

# The verifier file and probe must be exact and mutually bound before a
# journal exists. The fixture server independently rejects any other header.
api_input=/tmp/new-api-input.json
write_api_input "$api_input" "$token"
chmod 640 "$api_input"
if printf '%s\n' "$token" | run_auth --mode api-key \
  --public-host legal.example.com --api-key-file "$api_input" \
  >/tmp/api-mode.stdout 2>/tmp/api-mode.stderr; then
  echo 'group-readable API verifier input was accepted' >&2
  exit 1
fi
recover_public_entra
chmod 600 "$api_input"
ln "$api_input" /tmp/api-input-hardlink.json
if printf '%s\n' "$token" | run_auth --mode api-key \
  --public-host legal.example.com --api-key-file "$api_input" \
  >/tmp/api-link.stdout 2>/tmp/api-link.stderr; then
  echo 'multi-link API verifier input was accepted' >&2
  exit 1
fi
recover_public_entra
rm -f /tmp/api-input-hardlink.json
if printf '%s\n' "$wrong_token" | run_auth --mode api-key \
  --public-host legal.example.com --api-key-file "$api_input" \
  >/tmp/wrong-key.stdout 2>/tmp/wrong-key.stderr; then
  echo 'probe key not represented by the verifier file was accepted' >&2
  exit 1
fi
grep -Fq 'probe key does not match the supplied verifier document' /tmp/wrong-key.stderr
recover_public_entra
printf '{"keys":[{"id":"Bad","sha256":"%064d"}],"version":1}\n' 1 > /tmp/bad-api.json
chmod 600 /tmp/bad-api.json
if printf '%s\n' "$token" | run_auth --mode api-key \
  --public-host legal.example.com --api-key-file /tmp/bad-api.json \
  >/tmp/bad-api.stdout 2>/tmp/bad-api.stderr; then
  echo 'noncanonical API verifier document was accepted' >&2
  exit 1
fi
recover_public_entra
python3 - <<'PY'
import json, urllib.error, urllib.request
request=urllib.request.Request('http://127.0.0.1:51235/mcp', method='POST', data=b'{}', headers={'X-API-Key':'automation.BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB'})
try:
    urllib.request.urlopen(request, timeout=2)
except urllib.error.HTTPError as error:
    assert error.code == 401
else:
    raise SystemExit(1)
request=urllib.request.Request('http://127.0.0.1:51235/mcp', method='POST', data=b'{}', headers={'X-API-Key':'automation.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'})
with urllib.request.urlopen(request, timeout=2) as response:
    assert json.load(response)["result"]["serverInfo"]["name"] == "australian-legal-mcp"
PY
: > /tmp/api-server.log
printf '%s\n' "$token" | run_auth --mode api-key \
  --public-host legal.example.com --api-key-file "$api_input" \
  >/tmp/api-cutover.stdout
grep -Fxq 'authentication configured; exact private/public auth and route probes passed' /tmp/api-cutover.stdout
grep -Fxq valid /tmp/api-server.log
if grep -Fq "$token" "$log"; then
  echo 'plaintext API key leaked into the host action log' >&2
  exit 1
fi

# A hidden pre-prepare failure cannot darken or mutate an exactly validated
# prior public state. No journal was durably published, so the prior marker,
# services, verifier, Caddy, UFW, and listeners remain exact.
cp "$runtime" /tmp/preflight-runtime
cp "$api_keys" /tmp/preflight-api-keys
touch /tmp/fail-caddy-adapt
if run_entra_cutover >/tmp/preflight.stdout 2>/tmp/preflight.stderr; then
  echo 'injected authentication preparation preflight failure succeeded' >&2
  exit 1
fi
rm -f /tmp/fail-caddy-adapt
cmp --silent /tmp/preflight-runtime "$runtime"
cmp --silent /tmp/preflight-api-keys "$api_keys"
[[ ! -e "$transaction" && -e "$auth_ready" \
  && -e /tmp/service-active && -e /tmp/caddy-active \
  && -e /tmp/caddy-enabled && -e /tmp/ufw-80 && -e /tmp/ufw-443 ]]

# SIGKILL after the marker rename but before handoff finalization leaves the
# exact target receipt and marker as a committed-public recovery decision.
mv /usr/bin/mv /usr/bin/mv.auth-real
cat > /usr/bin/mv <<'EOF'
#!/usr/bin/bash
/usr/bin/mv.auth-real "$@"
status=$?
[[ $status -eq 0 ]] || exit "$status"
if [[ -e /tmp/kill-auth-on-ready-publication \
  && "${!#}" = /etc/legal-mcp/auth-ready ]]; then
  rm -f /tmp/kill-auth-on-ready-publication
  kill -KILL "$PPID"
  sleep 1
fi
if [[ -e /tmp/kill-auth-after-target-publication \
  && "${!#}" = /etc/legal-mcp/.auth-transaction/target ]]; then
  rm -f /tmp/kill-auth-after-target-publication
  kill -KILL "$PPID"
  sleep 1
fi
EOF
chmod 755 /usr/bin/mv

# A complete target receipt is still uncommitted until auth-ready is durable.
# Killing the implementation immediately after target publication therefore
# rolls back the exact prior public API-key baseline.
touch /tmp/kill-auth-after-target-publication
set +e
run_entra_cutover >/tmp/target-kill.stdout 2>/tmp/target-kill.stderr
target_status=$?
set -e
[[ $target_status -ne 0 && -d "$transaction/target" && ! -e "$auth_ready" ]]
target_recovery="$(printf '%s\n' "$token" | run_auth --recover)"
[[ "$target_recovery" = 'interrupted V2 authentication transaction rolled back' \
  && ! -e "$transaction" && -e "$auth_ready" ]]
grep -Fxq 'LEGAL_MCP_HTTP_AUTH=api-key' "$runtime"

touch /tmp/kill-auth-on-ready-publication
set +e
printf '%s\n' "$token" | run_auth --mode api-key \
  --public-host legal.example.com --api-key-file "$api_input" \
  >/tmp/publication-kill.stdout 2>/tmp/publication-kill.stderr
publication_status=$?
set -e
[[ $publication_status -ne 0 && -d "$transaction" && -d "$transaction/target" \
  && "$(stat -c '%U:%G:%a:%h:%s' "$auth_ready")" = root:root:444:1:0 ]]
"$launcher" --check-auth-ready
mv "$auth_ready" /tmp/committed-auth-ready
if "$launcher" --check-auth-ready; then
  echo 'reboot auth gate accepted an absent committed marker' >&2
  exit 1
fi
install -o root -g root -m 0444 /tmp/committed-auth-ready "$auth_ready"
publication_recovery="$(printf '%s\n' "$token" | run_auth --recover)"
[[ "$publication_recovery" = 'committed-public V2 authentication handoff finalized' \
  && ! -e "$transaction" && -e "$auth_ready" ]]
mv /usr/bin/mv.auth-real /usr/bin/mv

# Invalid mixed service/Caddy/UFW/listener states are closed, never journalled.
touch /tmp/wildcard-service-listener
if printf '%s\n' "$token" | run_auth --mode api-key \
  --public-host legal.example.com --api-key-file "$api_input" \
  >/tmp/listener.stdout 2>/tmp/listener.stderr; then
  echo 'wildcard 51235 listener was accepted' >&2
  exit 1
fi
[[ ! -e "$transaction" && ! -e /tmp/caddy-active \
  && ! -e /tmp/ufw-80 && ! -e /tmp/ufw-443 ]]
rm -f /tmp/wildcard-service-listener
set_public_api_state

# Preparing and deletion-only preparation-retired states never accept their
# partial contents as a V2 journal.
install -d -o root -g root -m 0700 "$preparing"
printf '%s\n' partial > "$preparing/kind"
recovery_output="$(printf '%s\n' "$token" | run_auth --recover)"
[[ "$recovery_output" = 'interrupted V2 authentication preparation discarded' \
  && ! -e "$preparing" && ! -e "$preparing_retired" \
  && -e /tmp/service-active && -e /tmp/caddy-active && -e /tmp/ufw-80 && -e /tmp/ufw-443 ]]
install -d -o root -g root -m 0700 "$preparing_retired"
printf '%s\n' partial > "$preparing_retired/deleted-member"
recovery_output="$(printf '%s\n' "$token" | run_auth --recover)"
[[ "$recovery_output" = 'interrupted V2 authentication preparation retirement completed' \
  && ! -e "$preparing_retired" ]]
install -d -o root -g root -m 0700 /etc/legal-mcp/.auth-transaction.preparing.123
if run_auth --recover >/tmp/unknown-state.stdout 2>/tmp/unknown-state.stderr; then
  echo 'unknown authentication transaction state was accepted' >&2
  exit 1
fi
[[ -d /etc/legal-mcp/.auth-transaction.preparing.123 ]]
rm -rf /etc/legal-mcp/.auth-transaction.preparing.123
set_public_api_state

# An ordinary post-publication failure is not allowed to retire the rollback
# data into the launcher's outer fail-closed action. Explicit recovery restores
# the exact prior public API-key matrix and only then retires the journal.
touch /tmp/fail-caddy-enable-once
if run_entra_cutover >/tmp/ordinary-failure.stdout 2>/tmp/ordinary-failure.stderr; then
  echo 'injected post-publication failure unexpectedly succeeded' >&2
  exit 1
fi
grep -Fq 'V2 journal requires --recover' /tmp/ordinary-failure.stderr
[[ -d "$transaction" && ! -e /tmp/service-active && ! -e /tmp/caddy-active \
  && ! -e /tmp/ufw-80 && ! -e /tmp/ufw-443 ]]
printf '%s\n' "$token" | run_auth --recover >/tmp/ordinary-recovery.stdout
grep -Fxq 'interrupted V2 authentication transaction rolled back' /tmp/ordinary-recovery.stdout
[[ ! -e "$transaction" && -e /tmp/service-active && -e /tmp/caddy-active \
  && -e /tmp/ufw-80 && -e /tmp/ufw-443 ]]

# SIGKILL immediately after canonical publication leaves an exact V2 journal.
# Recovery needs the prior positive API key, restores the public matrix, and
# retires the journal without ever enabling/disabling the generated unit.
: > "$log"
touch /tmp/kill-auth-on-caddy-disable
set +e
run_entra_cutover >/tmp/killed.stdout 2>/tmp/killed.stderr
kill_status=$?
set -e
[[ $kill_status -ne 0 && -d "$transaction" ]]
[[ "$(<"$transaction/kind")" = LEGAL_MCP_AUTH_TRANSACTION_V2 ]]
expected_names=$'Caddyfile\nactive-generation\napi-keys.json\nbaseline\nhost-tools\nkind\nruntime.env\nsha256'
actual_names="$(find "$transaction" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort)"
[[ "$actual_names" = "$expected_names" ]]
for file in Caddyfile active-generation api-keys.json baseline host-tools kind runtime.env sha256; do
  [[ "$(stat -c '%U:%G:%a:%h' "$transaction/$file")" = root:root:600:1 ]]
done
if grep -R -Fq "$token" "$transaction"; then
  echo 'plaintext API key leaked into the V2 journal' >&2
  exit 1
fi
printf '%s\n' "$token" | run_auth --recover >/tmp/recovered.stdout
grep -Fxq 'interrupted V2 authentication transaction rolled back' /tmp/recovered.stdout
[[ ! -e "$transaction" && -e /tmp/service-active && -e /tmp/caddy-active \
  && -e /tmp/ufw-80 && -e /tmp/ufw-443 ]]
if grep -Eq '^systemctl:(enable|disable).*legal-mcp\.service' "$log"; then
  echo 'V2 recovery mutated generated-unit enablement' >&2
  exit 1
fi

# Hash or whitelist drift is rejected after first forcing service and ingress
# off; the canonical journal is retained for exact repair and retry.
touch /tmp/kill-auth-on-caddy-disable
set +e
run_entra_cutover >/tmp/killed.stdout 2>/tmp/killed.stderr
set -e
cp "$transaction/runtime.env" /tmp/saved-journal-runtime
printf '%s\n' changed >> "$transaction/runtime.env"
chmod 600 "$transaction/runtime.env"
if printf '%s\n' "$token" | run_auth --recover \
  >/tmp/hash.stdout 2>/tmp/hash.stderr; then
  echo 'changed journal rollback bytes were accepted' >&2
  exit 1
fi
[[ -d "$transaction" && ! -e /tmp/service-active && ! -e /tmp/caddy-active \
  && ! -e /tmp/ufw-80 && ! -e /tmp/ufw-443 ]]
install -o root -g root -m 0600 /tmp/saved-journal-runtime "$transaction/runtime.env"
printf '%s\n' unexpected > "$transaction/unexpected"
chmod 600 "$transaction/unexpected"
if printf '%s\n' "$token" | run_auth --recover \
  >/tmp/whitelist.stdout 2>/tmp/whitelist.stderr; then
  echo 'unexpected V2 journal member was accepted' >&2
  exit 1
fi
rm -f "$transaction/unexpected"
printf '%s\n' "$token" | run_auth --recover >/dev/null

# SIGKILL after the commit rename leaves only retirement state. Recovery
# validates the exact live public matrix, never rolls it back, and deletes it.
set_public_api_state
/usr/bin/mv /usr/bin/mv /usr/bin/mv.fixture-real
cat > /usr/bin/mv <<'EOF'
#!/usr/bin/bash
/usr/bin/mv.fixture-real "$@"
status=$?
[[ $status -eq 0 ]] || exit "$status"
if [[ -e /tmp/kill-auth-on-retiring && "${!#}" = /etc/legal-mcp/.auth-transaction.retiring ]]; then
  rm -f /tmp/kill-auth-on-retiring
  kill -KILL "$PPID"
  sleep 1
fi
EOF
chmod 755 /usr/bin/mv
touch /tmp/kill-auth-on-retiring
set +e
run_entra_cutover >/tmp/retiring-kill.stdout 2>/tmp/retiring-kill.stderr
retiring_status=$?
set -e
[[ $retiring_status -ne 0 && -d "$retiring" && ! -e "$transaction" ]]
rm -f /tmp/service-active /tmp/caddy-active /tmp/caddy-enabled /tmp/ufw-80 /tmp/ufw-443
recovery_output="$(printf '%s\n' "$token" | run_auth --recover)"
[[ "$recovery_output" = 'interrupted committed-public authentication retirement restored; auth-ready publication is pending' \
  && ! -e "$retiring" && ! -e "$retired" \
  && -e "$auth_ready" \
  && -e /tmp/service-active && -e /tmp/caddy-active && -e /tmp/ufw-80 && -e /tmp/ufw-443 ]]
grep -Fxq 'LEGAL_MCP_HTTP_AUTH=entra' "$runtime"
# A genuinely partial retired directory is deletion-only and equally resumable.
install -d -o root -g root -m 0700 "$retired"
printf '%s\n' partial > "$retired/only-remaining-member"
rm -f "$auth_ready" /tmp/service-active /tmp/caddy-active /tmp/caddy-enabled /tmp/ufw-80 /tmp/ufw-443
recovery_output="$(printf '%s\n' "$token" | run_auth --recover)"
[[ "$recovery_output" = 'interrupted committed-public authentication retirement restored; auth-ready publication is pending' \
  && ! -e "$retired" && -e "$auth_ready" ]]
/usr/bin/mv.fixture-real /usr/bin/mv.fixture-real /usr/bin/mv

# One explicit legacy path accepts only the exact v0.19.2 V1 marker, v20
# 64-byte pointer, dark saved files, generated-enable marker, and safe headers.
set_dark_state
rm -rf "$dispatch"
rm -f "$auth_permit" "$auth_ready" "$launcher" "$launcher_marker" \
  "$configure_pointer" "$update_pointer"
install -o root -g root -m 0755 /configure-auth "$auth_impl"
install -o root -g root -m 0755 /tmp/update-image-implementation \
  /usr/local/sbin/legal-mcp-update-image
deploy_sha="$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}')"
publisher_sha="$(sha256sum /usr/local/sbin/legal-mcp-publisher-command | awk '{print $1}')"
sudoers_sha="$(sha256sum /etc/sudoers.d/legal-mcp-publisher | awk '{print $1}')"
cat > "$host_tools" <<EOF
LEGAL_MCP_HOST_TOOLS_V1
VERSION=0.19.2
SOURCE_COMMIT=2222222222222222222222222222222222222222
HOST_DEPLOY_SHA256=$deploy_sha
PUBLISHER_COMMAND_SHA256=$publisher_sha
SUDOERS_SHA256=$sudoers_sha
EOF
chmod 444 "$host_tools"

# The deployed v0.19.2 helper built its journal in a PID-suffixed directory.
# Every reachable dark-host interruption during that construction is a
# deletion-only one-shot recovery after the PID is dead; partial contents are
# never interpreted as rollback data.
legacy_preparing=/etc/legal-mcp/.auth-transaction.preparing.999999
for legacy_step in directory runtime verifier service-marker synced; do
  rm -rf "$legacy_preparing"
  install -d -o root -g root -m 0700 "$legacy_preparing"
  if [[ "$legacy_step" != directory ]]; then
    install -o root -g root -m 0600 "$runtime" "$legacy_preparing/runtime.env"
  fi
  if [[ "$legacy_step" = verifier || "$legacy_step" = service-marker \
    || "$legacy_step" = synced ]]; then
    install -o legal-mcp -g legal-mcp -m 0400 "$api_keys" \
      "$legacy_preparing/api-keys.json"
  fi
  if [[ "$legacy_step" = service-marker || "$legacy_step" = synced ]]; then
    install -o root -g root -m 0600 /dev/null \
      "$legacy_preparing/service-was-enabled"
  fi
  [[ "$legacy_step" != synced ]] || sync -f "$legacy_preparing"
  legacy_output="$("$auth_impl" --recover)"
  [[ "$legacy_output" = 'one-shot dead-PID v0.19.2 authentication preparation discarded; upgrade V2 host tools before configuring authentication' \
    && ! -e "$legacy_preparing" ]]
done

# Recovery retirement is itself restartable without trusting any remaining
# member of the deletion-only directory.
for legacy_retirement in \
  /etc/legal-mcp/.auth-transaction.legacy-v0192-preparing-retiring \
  /etc/legal-mcp/.auth-transaction.legacy-v0192-preparing-retired; do
  install -d -o root -g root -m 0700 "$legacy_retirement"
  printf '%s\n' partial > "$legacy_retirement/deleted-member"
  legacy_output="$("$auth_impl" --recover)"
  [[ "$legacy_output" = 'one-shot dead-PID v0.19.2 authentication preparation discarded; upgrade V2 host tools before configuring authentication' \
    && ! -e "$legacy_retirement" ]]
done

# A PID-suffixed preparation is never retired while that PID is live.
sleep 300 &
live_legacy_pid=$!
live_legacy_preparing="/etc/legal-mcp/.auth-transaction.preparing.$live_legacy_pid"
install -d -o root -g root -m 0700 "$live_legacy_preparing"
if "$auth_impl" --recover >/tmp/live-legacy.stdout 2>/tmp/live-legacy.stderr; then
  echo 'live v0.19.2 preparation owner was accepted as dead' >&2
  exit 1
fi
[[ -d "$live_legacy_preparing" ]]
kill "$live_legacy_pid"
wait "$live_legacy_pid" 2>/dev/null || true
rm -rf "$live_legacy_preparing"

install -d -o root -g root -m 0700 "$transaction"
install -o root -g root -m 0600 "$runtime" "$transaction/runtime.env"
install -o legal-mcp -g legal-mcp -m 0400 "$api_keys" "$transaction/api-keys.json"
install -o root -g root -m 0600 /dev/null "$transaction/service-was-enabled"
printf 'HTTP/1.1 401 Unauthorized\r\n\r\n' > "$transaction/private.headers"
chmod 600 "$transaction/private.headers"
# Model an interrupted attempt's unsafe live state; legacy recovery closes it.
touch /tmp/service-active /tmp/caddy-active /tmp/caddy-enabled /tmp/ufw-80 /tmp/ufw-443
legacy_output="$("$auth_impl" --recover)"
[[ "$legacy_output" = 'one-shot legacy v0.19.2 authentication transaction rolled back; upgrade V2 host tools before configuring authentication' ]]
[[ ! -e "$transaction" && ! -e /tmp/service-active && ! -e /tmp/caddy-active \
  && ! -e /tmp/ufw-80 && ! -e /tmp/ufw-443 ]]

# Legacy shape is never a normal V2 journal and any extra legacy member is
# rejected rather than guessed.
install -d -o root -g root -m 0700 "$transaction"
install -o root -g root -m 0600 "$runtime" "$transaction/runtime.env"
install -o legal-mcp -g legal-mcp -m 0400 "$api_keys" "$transaction/api-keys.json"
install -o root -g root -m 0600 /dev/null "$transaction/service-was-enabled"
printf '%s\n' unexpected > "$transaction/unknown"
chmod 600 "$transaction/unknown"
if "$auth_impl" --recover >/tmp/legacy-extra.stdout 2>/tmp/legacy-extra.stderr; then
  echo 'expanded legacy auth journal was accepted' >&2
  exit 1
fi
[[ -d "$transaction" ]]
rm -rf "$transaction"
if run_entra_cutover >/tmp/legacy-normal.stdout 2>/tmp/legacy-normal.stderr; then
  echo 'normal auth accepted the legacy V1 host contract' >&2
  exit 1
fi
[[ ! -e "$transaction" ]]

kill "$server_pid"
wait "$server_pid" 2>/dev/null || true
trap - EXIT

echo host-auth-recovery-fixture-ok
