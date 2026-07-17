#!/usr/bin/env bash
# Transactionally configure hosted authentication, prove the private service,
# then enable Caddy and public 80/443. Run on the host as root.
set -euo pipefail
umask 077
ulimit -c 0
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

usage() {
  cat >&2 <<'EOF'
usage: sudo infra/hosting/configure-auth.sh \
  --mode api-key|entra|entra+api-key \
  --public-host legal.example.com \
  [--api-key-file /root/api-key-verifiers.json] \
  [--tenant-id UUID --server-app-id UUID --audiences CSV \
   --scope legal.read --scope-uri api://UUID/legal.read \
   --allowed-client-ids CSV]

For modes containing api-key, stream the plaintext probe key only on standard
input: sudo ... </root/one-time-probe-key
To roll back a transaction interrupted by power loss or SIGKILL, run:
  sudo infra/hosting/configure-auth.sh --recover
If the saved prior mode contains api-key, stream a still-valid prior key on
standard input so recovery can prove positive authentication before ingress.
Update the Akamai Cloud Firewall to allow TCP 80/443 immediately before this
cutover. The script rolls back host exposure if public TLS/auth probes fail.
EOF
  exit 2
}

[[ $EUID -eq 0 ]] || { echo 'run configure-auth.sh as root' >&2; exit 2; }
LOCK_FILE=/run/lock/legal-mcp-host-transaction.lock
[[ -f "$LOCK_FILE" && ! -L "$LOCK_FILE" \
  && "$(stat -c '%U:%G:%a:%h' "$LOCK_FILE")" = root:legal-mcp-publisher:640:1 ]] || {
  echo 'host transaction lock is missing or unsafe' >&2
  exit 1
}
exec 9<>"$LOCK_FILE"
flock -x 9
MODE=''
PUBLIC_HOST=''
API_KEY_FILE=''
TENANT_ID=''
SERVER_APP_ID=''
AUDIENCES=''
SCOPE=''
SCOPE_URI=''
ALLOWED_CLIENT_IDS=''
PROBE_API_KEY=''
RECOVER=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --recover) RECOVER=true; shift ;;
    --mode) MODE="${2:-}"; shift 2 ;;
    --public-host) PUBLIC_HOST="${2:-}"; shift 2 ;;
    --api-key-file) API_KEY_FILE="${2:-}"; shift 2 ;;
    --tenant-id) TENANT_ID="${2:-}"; shift 2 ;;
    --server-app-id) SERVER_APP_ID="${2:-}"; shift 2 ;;
    --audiences) AUDIENCES="${2:-}"; shift 2 ;;
    --scope) SCOPE="${2:-}"; shift 2 ;;
    --scope-uri) SCOPE_URI="${2:-}"; shift 2 ;;
    --allowed-client-ids) ALLOWED_CLIENT_IDS="${2:-}"; shift 2 ;;
    *) usage ;;
  esac
done

TRANSACTION=/etc/legal-mcp/.auth-transaction

ufw_rule_exists() {
  local port="$1"
  ufw status | grep -Eq "^${port}/tcp[[:space:]]+ALLOW IN([[:space:]]|$)"
}

ufw_is_fail_closed() {
  local report admin_source_file=/etc/legal-mcp/admin-source-ip admin_source
  [[ -f "$admin_source_file" && ! -L "$admin_source_file" \
    && "$(stat -c '%U:%G:%a:%h' "$admin_source_file")" = root:root:600:1 ]] || return 1
  admin_source="$(<"$admin_source_file")"
  [[ "$admin_source" =~ ^[0-9A-Fa-f:.]{2,45}$ ]] || return 1
  report="$(ufw status verbose)"
  grep -Fxq 'Status: active' <<< "$report" \
    && grep -Eq '^Default: deny \(incoming\), allow \(outgoing\), (disabled|deny) \(routed\)$' <<< "$report" \
    && ! grep -Eq '^51235([/[:space:]]|$)' <<< "$report" \
    && awk -v admin="$admin_source" '
      /(DENY|REJECT|LIMIT) IN/ { bad=1 }
      /ALLOW IN/ {
        target=$1
        source=""
        for (i=1; i<NF; i++) if ($i == "IN") source=$(i+1)
        if (target != "22/tcp" && target != "80/tcp" && target != "443/tcp") bad=1
        if (target == "22/tcp") {
          ssh++
          if (source != admin) bad=1
        } else if (source != "Anywhere") bad=1
      }
      END { exit !bad && ssh == 1 ? 0 : 1 }
    ' <<< "$report"
}

wait_for_generation() {
  local expected="$1" deadline=$((SECONDS + 180))
  while (( SECONDS < deadline )); do
    if curl --fail --silent --max-time 5 http://127.0.0.1:51235/readyz 2>/dev/null |
      python3 -c 'import json,sys; v=json.load(sys.stdin); raise SystemExit(0 if v == {"status":"ok","generation":sys.argv[1]} else 1)' \
        "$expected" 2>/dev/null; then
      return 0
    fi
    systemctl is-active --quiet legal-mcp.service || return 1
    sleep 1
  done
  return 1
}

load_recovery_auth_contract() {
  local path="$1"
  [[ -f "$path" && ! -L "$path" ]] || return 1
  mapfile -t recovery_auth_values < <(awk -F= '$1 == "LEGAL_MCP_HTTP_AUTH" {print $2}' "$path")
  mapfile -t recovery_url_values < <(awk -F= '$1 == "LEGAL_MCP_EXTERNAL_URL" {print $2}' "$path")
  [[ ${#recovery_auth_values[@]} -eq 1 && ${#recovery_url_values[@]} -eq 1 ]] || return 1
  RECOVERY_AUTH_MODE="${recovery_auth_values[0]}"
  RECOVERY_EXTERNAL_URL="${recovery_url_values[0]}"
  [[ "$RECOVERY_AUTH_MODE" = disabled || "$RECOVERY_AUTH_MODE" = api-key \
    || "$RECOVERY_AUTH_MODE" = entra || "$RECOVERY_AUTH_MODE" = entra+api-key ]] || return 1
  [[ "$RECOVERY_EXTERNAL_URL" =~ ^https://[a-z0-9.-]+/mcp$ ]] || return 1
  RECOVERY_HAS_API=false
  RECOVERY_HAS_ENTRA=false
  [[ "$RECOVERY_AUTH_MODE" == *api-key* ]] && RECOVERY_HAS_API=true
  [[ "$RECOVERY_AUTH_MODE" == *entra* ]] && RECOVERY_HAS_ENTRA=true
  return 0
}

close_public_ingress() {
  systemctl disable --now caddy.service >/dev/null 2>&1 || return 1
  ufw --force delete allow 80/tcp >/dev/null 2>&1 || true
  ufw --force delete allow 443/tcp >/dev/null 2>&1 || true
  if systemctl is-active --quiet caddy.service \
    || ufw_rule_exists 80 || ufw_rule_exists 443; then
    echo 'failed to prove Caddy and UFW public ingress closed' >&2
    return 1
  fi
  return 0
}

probe_recovery_api_key() {
  local url="$1"
  [[ "$PROBE_API_KEY" =~ ^[a-z0-9][a-z0-9_-]{0,63}\.[A-Za-z0-9_-]{43}$ ]] || {
    echo 'restoring the prior API-key mode requires a valid probe key on standard input' >&2
    return 1
  }
  printf '%s' "$PROBE_API_KEY" | python3 -c '
import json, sys, urllib.request
key=sys.stdin.read(); url=sys.argv[1]
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None
request=urllib.request.Request(url, method="POST", data=b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}", headers={"Accept":"application/json, text/event-stream","Content-Type":"application/json","X-API-Key":key})
with urllib.request.build_opener(NoRedirect).open(request, timeout=20) as response:
    value=json.load(response)
if value.get("result",{}).get("serverInfo",{}).get("name") != "australian-legal-mcp":
    raise SystemExit(1)
' "$url"
}

probe_recovery_auth_boundary() {
  local mcp_url="$1" metadata_url="$2" headers status boundary_ok
  headers="$(mktemp /run/legal-mcp-auth-recovery.XXXXXX)"
  status="$(curl --silent --show-error --dump-header "$headers" --output /dev/null \
    --write-out '%{http_code}' --max-time 20 --max-redirs 0 --request POST \
    --header 'Accept: application/json, text/event-stream' \
    --header 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    "$mcp_url" 2>/dev/null || true)"
  boundary_ok=true
  [[ "$status" = 401 ]] || boundary_ok=false
  grep -Eiq '^WWW-Authenticate:' "$headers" || boundary_ok=false
  ! grep -Eiq '^Location:' "$headers" || boundary_ok=false
  if [[ "$RECOVERY_HAS_API" = true ]]; then
    grep -Eiq '^WWW-Authenticate:.*ApiKey realm=' "$headers" || boundary_ok=false
  fi
  if [[ "$RECOVERY_HAS_ENTRA" = true ]]; then
    grep -Eiq '^WWW-Authenticate:.*Bearer resource_metadata=' "$headers" || boundary_ok=false
  fi
  if [[ "$boundary_ok" = false ]]; then
    rm -f "$headers"
    return 1
  fi
  rm -f "$headers"
  if [[ "$RECOVERY_HAS_ENTRA" = true ]]; then
    if ! curl --fail --silent --show-error --max-time 20 --max-redirs 0 "$metadata_url" |
      python3 -c 'import json,sys; value=json.load(sys.stdin); raise SystemExit(0 if value.get("resource") == sys.argv[1] else 1)' \
        "$RECOVERY_EXTERNAL_URL"; then
      return 1
    fi
  fi
  if [[ "$RECOVERY_HAS_API" = true ]] && ! probe_recovery_api_key "$mcp_url"; then return 1; fi
  return 0
}

recover_transaction() {
  local expected external_url deadline status
  close_public_ingress || {
    echo 'authentication recovery could not close public ingress' >&2
    return 1
  }
  if ! ufw_is_fail_closed; then
    echo 'UFW is not active with default-deny incoming; recovery refuses to start Caddy' >&2
    return 1
  fi
  [[ -d "$TRANSACTION" && ! -L "$TRANSACTION" \
    && "$(stat -c '%U:%G:%a' "$TRANSACTION")" = root:root:700 \
    && -f "$TRANSACTION/runtime.env" && ! -L "$TRANSACTION/runtime.env" \
    && -f "$TRANSACTION/api-keys.json" && ! -L "$TRANSACTION/api-keys.json" ]] || {
    echo 'auth transaction is incomplete or unsafe; leaving ingress closed' >&2
    return 1
  }
  load_recovery_auth_contract "$TRANSACTION/runtime.env" || {
    echo 'saved authentication contract is malformed; ingress remains closed' >&2
    return 1
  }
  if [[ "$RECOVERY_AUTH_MODE" = disabled \
    && ( -e "$TRANSACTION/service-was-active" || -e "$TRANSACTION/public-was-open" ) ]]; then
    echo 'disabled saved auth cannot restore an active hosted service' >&2
    return 1
  fi
  install -o root -g root -m 0600 "$TRANSACTION/runtime.env" /etc/legal-mcp/runtime.env
  install -o legal-mcp -g legal-mcp -m 0400 "$TRANSACTION/api-keys.json" /etc/legal-mcp/api-keys.json
  systemctl daemon-reload

  if [[ -e "$TRANSACTION/service-was-enabled" ]]; then
    systemctl enable legal-mcp.service >/dev/null
  else
    systemctl disable legal-mcp.service >/dev/null
  fi
  if [[ -e "$TRANSACTION/service-was-active" ]]; then
    expected="$(</srv/legal-mcp/lifecycle/active-generation)"
    [[ "$expected" =~ ^[0-9a-f]{64}$ ]]
    systemctl restart legal-mcp.service
    wait_for_generation "$expected"
    probe_recovery_auth_boundary http://127.0.0.1:51235/mcp \
      http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp
  else
    systemctl stop legal-mcp.service >/dev/null 2>&1 || true
  fi

  if [[ -e "$TRANSACTION/caddy-was-enabled" ]]; then
    systemctl enable caddy.service >/dev/null
  else
    systemctl disable caddy.service >/dev/null
  fi
  if [[ -e "$TRANSACTION/caddy-was-active" ]]; then
    systemctl start caddy.service
  else
    systemctl stop caddy.service >/dev/null 2>&1 || true
  fi
  if [[ -e "$TRANSACTION/public-was-open" ]]; then
    [[ -e "$TRANSACTION/caddy-was-active" && -e "$TRANSACTION/service-was-active" ]]
    external_url="$(awk -F= '$1 == "LEGAL_MCP_EXTERNAL_URL" {print $2}' "$TRANSACTION/runtime.env")"
    [[ "$external_url" =~ ^https://[a-z0-9.-]+/mcp$ ]]
    if ! ufw allow 80/tcp comment 'Caddy ACME HTTP' >/dev/null \
      || ! ufw allow 443/tcp comment 'Australian Legal MCP HTTPS' >/dev/null; then
      systemctl disable --now caddy.service >/dev/null 2>&1 || true
      ufw --force delete allow 80/tcp >/dev/null 2>&1 || true
      ufw --force delete allow 443/tcp >/dev/null 2>&1 || true
      return 1
    fi
    deadline=$((SECONDS + 180))
    while (( SECONDS < deadline )); do
      status="$(curl --silent --show-error --dump-header "$TRANSACTION/recovery.headers" \
        --output /dev/null --write-out '%{http_code}' \
        --max-time 15 --max-redirs 0 --request POST \
        --header 'Accept: application/json, text/event-stream' \
        --header 'Content-Type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
        "$external_url" 2>/dev/null || true)"
      [[ "$status" = 401 ]] && break
      systemctl is-active --quiet caddy.service || return 1
      sleep 2
    done
    if [[ "$status" != 401 ]] \
      || ! probe_recovery_auth_boundary "$external_url" \
        "${external_url%/mcp}/.well-known/oauth-protected-resource/mcp"; then
      systemctl disable --now caddy.service >/dev/null 2>&1 || true
      ufw --force delete allow 80/tcp >/dev/null 2>&1 || true
      ufw --force delete allow 443/tcp >/dev/null 2>&1 || true
      echo 'restored public authentication boundary did not pass its 401 probe' >&2
      return 1
    fi
  fi
  rm -rf "$TRANSACTION"
  sync -f /etc/legal-mcp
}

if [[ "$RECOVER" = true ]]; then
  [[ $# -eq 0 && -z "$MODE$PUBLIC_HOST$API_KEY_FILE$TENANT_ID$SERVER_APP_ID$AUDIENCES$SCOPE$SCOPE_URI$ALLOWED_CLIENT_IDS" ]] || usage
  [[ -e "$TRANSACTION" ]] || { echo 'no auth transaction exists' >&2; exit 1; }
  close_public_ingress || { echo 'could not close ingress before recovery' >&2; exit 1; }
  if load_recovery_auth_contract "$TRANSACTION/runtime.env" && [[ "$RECOVERY_HAS_API" = true ]]; then
    IFS= read -r PROBE_API_KEY || {
      echo 'restoring the prior API-key mode requires a probe key on standard input' >&2
      exit 2
    }
  fi
  recover_transaction
  echo 'interrupted authentication transaction rolled back'
  exit 0
fi

[[ "$MODE" = api-key || "$MODE" = entra || "$MODE" = entra+api-key ]] || usage
[[ "$PUBLIC_HOST" =~ ^[a-z0-9.-]{3,253}$ && "$PUBLIC_HOST" == *.* ]] || usage
python3 - "$PUBLIC_HOST" <<'PY' || usage
import ipaddress, sys
try:
    ipaddress.ip_address(sys.argv[1])
except ValueError:
    pass
else:
    raise SystemExit(1)
labels = sys.argv[1].split('.')
if any(not label or len(label) > 63 or label[0] == '-' or label[-1] == '-'
       or not all(c.isascii() and (c.islower() or c.isdigit() or c == '-') for c in label)
       for label in labels):
    raise SystemExit(1)
PY

has_api=false
has_entra=false
[[ "$MODE" == *api-key* ]] && has_api=true
[[ "$MODE" == *entra* ]] && has_entra=true
if [[ "$has_api" = true ]]; then
  [[ -f "$API_KEY_FILE" && ! -L "$API_KEY_FILE" ]] || usage
  IFS= read -r PROBE_API_KEY || { echo 'API-key mode requires a probe key on standard input' >&2; exit 2; }
  [[ "$PROBE_API_KEY" =~ ^[a-z0-9][a-z0-9_-]{0,63}\.[A-Za-z0-9_-]{43}$ ]] || {
    echo 'probe API key has an invalid shape' >&2
    exit 2
  }
else
  [[ -z "$API_KEY_FILE" ]] || usage
  PROBE_API_KEY=''
fi

uuid_re='^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
if [[ "$has_entra" = true ]]; then
  [[ "$TENANT_ID" =~ $uuid_re && "$SERVER_APP_ID" =~ $uuid_re ]] || usage
  [[ "$SCOPE" =~ ^[A-Za-z0-9._-]{1,128}$ ]] || usage
  [[ "$SCOPE_URI" = "api://$SERVER_APP_ID/$SCOPE" ]] || usage
  for list in "$AUDIENCES" "$ALLOWED_CLIENT_IDS"; do
    [[ "$list" =~ ^[A-Za-z0-9:/._-]+(,[A-Za-z0-9:/._-]+)*$ ]] || usage
  done
  [[ ",$AUDIENCES," == *",$SERVER_APP_ID,"* || ",$AUDIENCES," == *",api://$SERVER_APP_ID,"* ]] || usage
  IFS=',' read -r -a client_ids <<< "$ALLOWED_CLIENT_IDS"
  for client_id in "${client_ids[@]}"; do [[ "$client_id" =~ $uuid_re ]] || usage; done
else
  [[ -z "$TENANT_ID$SERVER_APP_ID$AUDIENCES$SCOPE$SCOPE_URI$ALLOWED_CLIENT_IDS" ]] || usage
fi

[[ -f /etc/legal-mcp/image && -f /etc/containers/systemd/legal-mcp.container ]] || {
  echo 'install the OCI host contract first' >&2
  exit 1
}
[[ ! -e /etc/legal-mcp/.image-transaction \
  && ! -e /srv/legal-mcp/lifecycle/.deployment-transaction ]] || {
  echo 'recover the pending image or corpus transaction before changing authentication' >&2
  exit 1
}
[[ -f /etc/caddy/Caddyfile && ! -L /etc/caddy/Caddyfile ]] || { echo 'Caddy is not installed' >&2; exit 1; }
ufw_is_fail_closed || {
  close_public_ingress || true
  echo 'UFW must match the exact fail-closed allowlist; auth cutover was refused' >&2
  exit 1
}
if ! grep -Fxq "http://$PUBLIC_HOST {" /etc/caddy/Caddyfile \
  || ! grep -Fxq "https://$PUBLIC_HOST {" /etc/caddy/Caddyfile; then
  echo 'Caddyfile host does not match --public-host' >&2
  exit 1
fi
caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile >/dev/null
[[ -f /srv/legal-mcp/lifecycle/active-generation ]] || { echo 'activate a corpus generation before auth cutover' >&2; exit 1; }
EXPECTED_GENERATION="$(</srv/legal-mcp/lifecycle/active-generation)"
[[ "$EXPECTED_GENERATION" =~ ^[0-9a-f]{64}$ ]] || { echo 'active generation is malformed' >&2; exit 1; }

[[ ! -e "$TRANSACTION" ]] || {
  echo 'an auth transaction already exists; run this command with --recover first' >&2
  exit 1
}
transaction_tmp="${TRANSACTION}.preparing.$$"
install -d -o root -g root -m 0700 "$transaction_tmp"
cp --preserve=mode,ownership,timestamps /etc/legal-mcp/runtime.env "$transaction_tmp/runtime.env"
cp --preserve=mode,ownership,timestamps /etc/legal-mcp/api-keys.json "$transaction_tmp/api-keys.json"
systemctl is-enabled --quiet legal-mcp.service && touch "$transaction_tmp/service-was-enabled"
systemctl is-active --quiet legal-mcp.service && touch "$transaction_tmp/service-was-active"
systemctl is-enabled --quiet caddy.service && touch "$transaction_tmp/caddy-was-enabled"
systemctl is-active --quiet caddy.service && touch "$transaction_tmp/caddy-was-active"
port_80_open=false
port_443_open=false
ufw_rule_exists 80 && port_80_open=true
ufw_rule_exists 443 && port_443_open=true
[[ "$port_80_open" = "$port_443_open" ]] || {
  echo 'UFW 80/443 state is inconsistent; refusing auth cutover' >&2
  rm -rf "$transaction_tmp"
  exit 1
}
if [[ "$port_80_open" = true ]]; then
  [[ -e "$transaction_tmp/caddy-was-active" \
    && -e "$transaction_tmp/service-was-active" ]] || {
    echo 'public UFW ingress is open while Caddy or legal-mcp is inactive' >&2
    rm -rf "$transaction_tmp"
    exit 1
  }
  touch "$transaction_tmp/public-was-open"
fi
sync -f "$transaction_tmp"
mv -T "$transaction_tmp" "$TRANSACTION"
sync -f /etc/legal-mcp

rollback() {
  local status=$?
  trap - ERR HUP INT TERM EXIT
  recover_transaction
  echo 'authentication/ingress cutover rolled back' >&2
  exit "$status"
}
trap rollback ERR HUP INT TERM EXIT

wait_for_private_readiness() {
  wait_for_generation "$EXPECTED_GENERATION"
}

# Never mutate credentials while the old service remains publicly reachable.
close_public_ingress

if [[ "$has_api" = true ]]; then
  python3 - "$API_KEY_FILE" <<'PY'
import json, pathlib, stat, sys
path = pathlib.Path(sys.argv[1])
meta = path.lstat()
if path.is_symlink() or not stat.S_ISREG(meta.st_mode) or meta.st_size > 65536 or meta.st_nlink != 1:
    raise SystemExit('unsafe API-key verifier file')
value = json.loads(path.read_bytes())
if not isinstance(value, dict) or set(value) != {'version', 'keys'} or value['version'] != 1:
    raise SystemExit('invalid API-key verifier schema')
keys = value['keys']
if not isinstance(keys, list) or not 1 <= len(keys) <= 32:
    raise SystemExit('API-key verifier count is invalid')
seen_ids, seen_hashes = set(), set()
for item in keys:
    if not isinstance(item, dict) or set(item) != {'id', 'sha256'}:
        raise SystemExit('invalid API-key verifier record')
    key_id, digest = item['id'], item['sha256']
    if (not isinstance(key_id, str) or not isinstance(digest, str)
            or len(digest) != 64 or any(c not in '0123456789abcdef' for c in digest)
            or key_id in seen_ids or digest in seen_hashes):
        raise SystemExit('invalid or duplicate API-key verifier')
    seen_ids.add(key_id); seen_hashes.add(digest)
PY
  install -o legal-mcp -g legal-mcp -m 0400 "$API_KEY_FILE" /etc/legal-mcp/api-keys.json
fi

runtime_tmp="$(mktemp /etc/legal-mcp/.runtime.env.XXXXXX)"
cat > "$runtime_tmp" <<EOF
LEGAL_MCP_HTTP_AUTH=$MODE
LEGAL_MCP_API_KEYS_FILE=/run/secrets/legal-mcp-api-keys.json
LEGAL_MCP_EXTERNAL_URL=https://$PUBLIC_HOST/mcp
LEGAL_MCP_ALLOWED_ORIGINS=https://$PUBLIC_HOST
LEGAL_MCP_HTTP_WORKERS=4
LEGAL_MCP_SHUTDOWN_GRACE_SECONDS=30
EOF
if [[ "$has_entra" = true ]]; then
  cat >> "$runtime_tmp" <<EOF
LEGAL_MCP_ENTRA_TENANT_ID=$TENANT_ID
LEGAL_MCP_ENTRA_SERVER_APP_ID=$SERVER_APP_ID
LEGAL_MCP_ENTRA_AUDIENCES=$AUDIENCES
LEGAL_MCP_ENTRA_SCOPE=$SCOPE
LEGAL_MCP_ENTRA_SCOPE_URI=$SCOPE_URI
LEGAL_MCP_ENTRA_ALLOWED_CLIENT_IDS=$ALLOWED_CLIENT_IDS
EOF
fi
chown root:root "$runtime_tmp"
chmod 600 "$runtime_tmp"
mv -fT "$runtime_tmp" /etc/legal-mcp/runtime.env

systemctl daemon-reload
systemctl enable legal-mcp.service
systemctl restart legal-mcp.service
wait_for_private_readiness

private_headers="$TRANSACTION/private.headers"
probe_status="$(curl --silent --dump-header "$private_headers" --output /dev/null --write-out '%{http_code}' --max-time 10 \
  --request POST --header 'Accept: application/json, text/event-stream' \
  --header 'Content-Type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  http://127.0.0.1:51235/mcp)"
[[ "$probe_status" = 401 ]] || { echo 'private unauthenticated MCP probe did not return 401' >&2; exit 1; }
grep -Eiq '^WWW-Authenticate:' "$private_headers" || { echo 'private 401 has no authentication challenge' >&2; exit 1; }
if [[ "$has_api" = true ]]; then grep -Eiq '^WWW-Authenticate:.*ApiKey realm=' "$private_headers"; fi
if [[ "$has_entra" = true ]]; then grep -Eiq '^WWW-Authenticate:.*Bearer resource_metadata=' "$private_headers"; fi
if [[ "$has_entra" = true ]]; then
  curl --fail --silent --show-error --max-time 10 \
    http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp |
    python3 -c 'import json,sys; v=json.load(sys.stdin); raise SystemExit(0 if v.get("resource")==sys.argv[1] else 1)' \
      "https://$PUBLIC_HOST/mcp"
fi

probe_api() {
  local url="$1"
  printf '%s' "$PROBE_API_KEY" | python3 -c '
import json, sys, urllib.request
key=sys.stdin.read(); url=sys.argv[1]
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None
request=urllib.request.Request(url, method="POST", data=b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}", headers={"Accept":"application/json, text/event-stream","Content-Type":"application/json","X-API-Key":key})
with urllib.request.build_opener(NoRedirect).open(request, timeout=15) as response:
    value=json.load(response)
if value.get("result",{}).get("serverInfo",{}).get("name") != "australian-legal-mcp":
    raise SystemExit(1)
' "$url"
}
if [[ "$has_api" = true ]]; then probe_api http://127.0.0.1:51235/mcp; fi

wait_for_public_401() {
  local deadline=$((SECONDS + 180)) status
  while (( SECONDS < deadline )); do
    if status="$(curl --silent --show-error --dump-header "$public_headers" \
      --output /dev/null --write-out '%{http_code}' --max-time 15 --max-redirs 0 \
      --request POST --header 'Accept: application/json, text/event-stream' \
      --header 'Content-Type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
      "https://$PUBLIC_HOST/mcp" 2>/dev/null)" && [[ "$status" = 401 ]]; then
      return 0
    fi
    systemctl is-active --quiet caddy.service || return 1
    sleep 2
  done
  return 1
}

ufw allow 80/tcp comment 'Caddy ACME HTTP'
ufw allow 443/tcp comment 'Australian Legal MCP HTTPS'
systemctl enable --now caddy.service

public_headers="$TRANSACTION/public.headers"
wait_for_public_401 || { echo 'public unauthenticated MCP probe did not reach 401' >&2; exit 1; }
! grep -Eiq '^Location:' "$public_headers" || { echo 'public endpoint attempted a redirect' >&2; exit 1; }
grep -Eiq '^WWW-Authenticate:' "$public_headers" || { echo 'public 401 has no authentication challenge' >&2; exit 1; }
if [[ "$has_api" = true ]]; then grep -Eiq '^WWW-Authenticate:.*ApiKey realm=' "$public_headers"; fi
if [[ "$has_entra" = true ]]; then grep -Eiq '^WWW-Authenticate:.*Bearer resource_metadata=' "$public_headers"; fi
if [[ "$has_api" = true ]]; then probe_api "https://$PUBLIC_HOST/mcp"; fi
if [[ "$has_entra" = true ]]; then
  curl --fail --silent --show-error --max-time 20 --max-redirs 0 \
    "https://$PUBLIC_HOST/.well-known/oauth-protected-resource/mcp" >/dev/null
fi

trap - ERR HUP INT TERM EXIT
rm -rf "$TRANSACTION"
sync -f /etc/legal-mcp
unset PROBE_API_KEY
printf '%s\n' 'authentication configured; private and public probes passed'
