#!/usr/bin/env bash
# Transactionally replace the pinned OCI digest and roll back on any runtime,
# corpus, identity, authentication, or public-boundary failure.
set -euo pipefail
umask 077
ulimit -c 0
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

usage() {
  cat >&2 <<'EOF'
usage: sudo update-image.sh \
  --image ghcr.io/gunba/australian-legal-mcp@sha256:DIGEST \
  --version X.Y.Z \
  --template PATH

If the configured mode contains api-key, stream one valid plaintext probe key
on standard input. To roll back an update interrupted by power loss or SIGKILL:
  sudo update-image.sh --recover [</root/one-time-probe-key]
EOF
  exit 2
}

[[ $EUID -eq 0 ]] || { echo 'run update-image.sh as root' >&2; exit 2; }
LOCK_FILE=/run/lock/legal-mcp-host-transaction.lock
[[ -f "$LOCK_FILE" && ! -L "$LOCK_FILE" \
  && "$(stat -c '%U:%G:%a:%h' "$LOCK_FILE")" = root:legal-mcp-publisher:640:1 ]] || {
  echo 'host transaction lock is missing or unsafe' >&2
  exit 1
}
exec 9<>"$LOCK_FILE"
flock -x 9

IMAGE_FILE=/etc/legal-mcp/image
QUADLET=/etc/containers/systemd/legal-mcp.container
TEMPLATE=/usr/local/libexec/legal-mcp/legal-mcp.container.template
RUNTIME_ENV=/etc/legal-mcp/runtime.env
TRANSACTION=/etc/legal-mcp/.image-transaction
SERVICE=legal-mcp.service
NEW_IMAGE=''
EXPECTED_VERSION=''
SOURCE_TEMPLATE=''
RECOVER=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --image) NEW_IMAGE="${2:-}"; shift 2 ;;
    --version) EXPECTED_VERSION="${2:-}"; shift 2 ;;
    --template) SOURCE_TEMPLATE="${2:-}"; shift 2 ;;
    --recover) RECOVER=true; shift ;;
    *) usage ;;
  esac
done

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

close_ingress() {
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

load_runtime_contract() {
  local path="$1"
  [[ -f "$path" && ! -L "$path" ]] || { echo 'runtime environment is missing or unsafe' >&2; return 1; }
  mapfile -t auth_values < <(awk -F= '$1 == "LEGAL_MCP_HTTP_AUTH" {print $2}' "$path")
  mapfile -t url_values < <(awk -F= '$1 == "LEGAL_MCP_EXTERNAL_URL" {print $2}' "$path")
  [[ ${#auth_values[@]} -eq 1 && ${#url_values[@]} -eq 1 ]] || {
    echo 'runtime auth contract must contain exactly one mode and external URL' >&2
    return 1
  }
  AUTH_MODE="${auth_values[0]}"
  EXTERNAL_URL="${url_values[0]}"
  [[ "$AUTH_MODE" = api-key || "$AUTH_MODE" = entra || "$AUTH_MODE" = entra+api-key ]] || {
    echo 'image updates require configured hosted authentication' >&2
    return 1
  }
  [[ "$EXTERNAL_URL" =~ ^https://[a-z0-9.-]+/mcp$ ]] || {
    echo 'runtime external URL is not canonical HTTPS /mcp' >&2
    return 1
  }
  HAS_API=false
  HAS_ENTRA=false
  [[ "$AUTH_MODE" == *api-key* ]] && HAS_API=true
  [[ "$AUTH_MODE" == *entra* ]] && HAS_ENTRA=true
  return 0
}

read_probe_key() {
  PROBE_API_KEY=''
  if [[ "$HAS_API" = true ]]; then
    IFS= read -r PROBE_API_KEY || {
      echo 'the configured API-key mode requires a probe key on standard input' >&2
      return 1
    }
    [[ "$PROBE_API_KEY" =~ ^[a-z0-9][a-z0-9_-]{0,63}\.[A-Za-z0-9_-]{43}$ ]] || {
      echo 'probe API key has an invalid shape' >&2
      return 1
    }
  fi
  return 0
}

wait_for_exact_generation() {
  local expected="$1" deadline=$((SECONDS + 180))
  while (( SECONDS < deadline )); do
    if curl --fail --silent --max-time 5 http://127.0.0.1:51235/readyz 2>/dev/null |
      python3 -c 'import json,sys; value=json.load(sys.stdin); expected=sys.argv[1]; raise SystemExit(0 if value == {"status":"ok","generation":expected} else 1)' \
        "$expected" 2>/dev/null; then
      return 0
    fi
    systemctl is-active --quiet "$SERVICE" || return 1
    sleep 1
  done
  return 1
}

probe_api_key() {
  local url="$1"
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

probe_auth_boundary() {
  local mcp_url="$1" metadata_url="$2" headers status
  headers="$(mktemp /run/legal-mcp-image-auth.XXXXXX)"
  status="$(curl --silent --show-error --dump-header "$headers" --output /dev/null \
    --write-out '%{http_code}' --max-time 20 --max-redirs 0 --request POST \
    --header 'Accept: application/json, text/event-stream' \
    --header 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    "$mcp_url" 2>/dev/null || true)"
  if [[ "$status" != 401 ]] \
    || ! grep -Eiq '^WWW-Authenticate:' "$headers" \
    || grep -Eiq '^Location:' "$headers"; then
    rm -f "$headers"
    echo "authentication boundary failed its unauthenticated probe: $mcp_url" >&2
    return 1
  fi
  if [[ "$HAS_API" = true ]] && ! grep -Eiq '^WWW-Authenticate:.*ApiKey realm=' "$headers"; then
    rm -f "$headers"
    return 1
  fi
  if [[ "$HAS_ENTRA" = true ]] && ! grep -Eiq '^WWW-Authenticate:.*Bearer resource_metadata=' "$headers"; then
    rm -f "$headers"
    return 1
  fi
  rm -f "$headers"
  if [[ "$HAS_ENTRA" = true ]]; then
    if ! curl --fail --silent --show-error --max-time 20 --max-redirs 0 "$metadata_url" |
      python3 -c 'import json,sys; value=json.load(sys.stdin); raise SystemExit(0 if value.get("resource") == sys.argv[1] else 1)' \
        "$EXTERNAL_URL"; then
      return 1
    fi
  fi
  if [[ "$HAS_API" = true ]] && ! probe_api_key "$mcp_url"; then return 1; fi
  return 0
}

verify_image_runtime() {
  local image="$1" expected_version="$2" expected_revision="$3"
  local image_version image_source image_revision binary_version
  image_version="$(podman image inspect "$image" --format '{{index .Labels "org.opencontainers.image.version"}}')"
  image_source="$(podman image inspect "$image" --format '{{index .Labels "org.opencontainers.image.source"}}')"
  image_revision="$(podman image inspect "$image" --format '{{index .Labels "org.opencontainers.image.revision"}}')"
  [[ "$image_version" = "$expected_version" \
    && "$image_source" = https://github.com/gunba/australian-legal-mcp \
    && "$image_revision" = "$expected_revision" ]] || {
    echo 'OCI identity labels do not match the requested release' >&2
    return 1
  }
  binary_version="$(podman run --rm --network=none --read-only --cap-drop=all \
    --security-opt=no-new-privileges "$image" --version)"
  [[ "$binary_version" = "legal-mcp $expected_version" ]] || {
    echo 'OCI binary version does not match its identity label' >&2
    return 1
  }
  podman run --rm --network=none --read-only --cap-drop=all \
    --security-opt=no-new-privileges "$image" verify-runtime |
    grep -Fq '"onnx_runtime_ready":true'
}

restore_unit_enablement() {
  local unit="$1" flag="$2"
  if [[ -e "$TRANSACTION/$flag" ]]; then
    systemctl enable "$unit" >/dev/null
  else
    systemctl disable "$unit" >/dev/null
  fi
}

open_recorded_ingress() {
  if [[ -e "$TRANSACTION/caddy-was-active" ]]; then
    systemctl start caddy.service
  else
    systemctl stop caddy.service >/dev/null 2>&1 || true
  fi
  if [[ -e "$TRANSACTION/public-was-open" ]]; then
    if ! ufw allow 80/tcp comment 'Caddy ACME HTTP' >/dev/null \
      || ! ufw allow 443/tcp comment 'Australian Legal MCP HTTPS' >/dev/null \
      || ! probe_auth_boundary "$EXTERNAL_URL" "${EXTERNAL_URL%/mcp}/.well-known/oauth-protected-resource/mcp"; then
      close_ingress
      echo 'public authentication boundary failed; ingress remains closed' >&2
      return 1
    fi
  fi
  return 0
}

recover_transaction() {
  close_ingress || {
    echo 'image recovery could not close public ingress' >&2
    return 1
  }
  if ! ufw_is_fail_closed; then
    echo 'UFW is not active with default-deny incoming; image recovery refuses to start Caddy' >&2
    return 1
  fi
  [[ -d "$TRANSACTION" && ! -L "$TRANSACTION" \
    && "$(stat -c '%U:%G:%a' "$TRANSACTION")" = root:root:700 ]] || {
    echo 'image transaction is incomplete or unsafe; leaving ingress closed' >&2
    close_ingress
    return 1
  }
  for name in image legal-mcp.container legal-mcp.container.template runtime.env expected-generation; do
    [[ -f "$TRANSACTION/$name" && ! -L "$TRANSACTION/$name" ]] || {
      echo "image transaction is missing $name; leaving ingress closed" >&2
      close_ingress
      return 1
    }
  done
  install -o root -g root -m 0600 "$TRANSACTION/image" "$IMAGE_FILE"
  install -o root -g root -m 0644 "$TRANSACTION/legal-mcp.container" "$QUADLET"
  install -o root -g root -m 0644 "$TRANSACTION/legal-mcp.container.template" "$TEMPLATE"
  install -o root -g root -m 0600 "$TRANSACTION/runtime.env" "$RUNTIME_ENV"
  expected="$(<"$TRANSACTION/expected-generation")"
  old_image="$(<"$TRANSACTION/image")"
  [[ "$expected" =~ ^[0-9a-f]{64}$ \
    && "$old_image" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ ]]
  load_runtime_contract "$TRANSACTION/runtime.env"
  systemctl daemon-reload
  restore_unit_enablement "$SERVICE" service-was-enabled
  restore_unit_enablement caddy.service caddy-was-enabled
  if [[ -e "$TRANSACTION/service-was-active" ]]; then
    podman image exists "$old_image" || podman pull "$old_image"
    systemctl restart "$SERVICE"
    wait_for_exact_generation "$expected"
    probe_auth_boundary http://127.0.0.1:51235/mcp \
      http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp
    expected_image_id="$(podman image inspect "$old_image" --format '{{.Id}}')"
    running_image_id="$(podman inspect australian-legal-mcp --format '{{.Image}}')"
    [[ "$running_image_id" = "$expected_image_id" ]]
  else
    systemctl stop "$SERVICE" >/dev/null 2>&1 || true
  fi
  open_recorded_ingress
  rm -rf "$TRANSACTION"
  sync -f /etc/legal-mcp
}

if [[ "$RECOVER" = true ]]; then
  [[ -z "$NEW_IMAGE$EXPECTED_VERSION$SOURCE_TEMPLATE" ]] || usage
  [[ -e "$TRANSACTION" ]] || { echo 'no image transaction exists' >&2; exit 1; }
  close_ingress || { echo 'could not close ingress before image recovery' >&2; exit 1; }
  load_runtime_contract "$TRANSACTION/runtime.env"
  read_probe_key
  recover_transaction
  unset PROBE_API_KEY
  echo 'interrupted image transaction rolled back'
  exit 0
fi

[[ "$NEW_IMAGE" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ \
  && "$EXPECTED_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || usage
[[ -f "$SOURCE_TEMPLATE" && ! -L "$SOURCE_TEMPLATE" ]] || {
  echo 'version-matched Quadlet template is missing or unsafe' >&2
  exit 2
}
[[ "$(grep -o '__IMAGE_DIGEST__' "$SOURCE_TEMPLATE" | wc -l)" = 1 ]] || {
  echo 'version-matched Quadlet template has an invalid image placeholder' >&2
  exit 2
}
SOURCE_TEMPLATE="$(readlink -f "$SOURCE_TEMPLATE")"
BUNDLE_ROOT="$(cd "$(dirname "$SOURCE_TEMPLATE")/../.." && pwd -P)"
[[ -f "$BUNDLE_ROOT/Containerfile" && ! -L "$BUNDLE_ROOT/Containerfile" \
  && -f "$BUNDLE_ROOT/SOURCE_COMMIT" && ! -L "$BUNDLE_ROOT/SOURCE_COMMIT" ]] || {
  echo 'image update requires the complete version-matched Linux release bundle' >&2
  exit 2
}
bundle_version="$(sed -n 's/^ARG VERSION=//p' "$BUNDLE_ROOT/Containerfile")"
EXPECTED_REVISION="$(<"$BUNDLE_ROOT/SOURCE_COMMIT")"
[[ "$bundle_version" = "$EXPECTED_VERSION" && "$EXPECTED_REVISION" =~ ^[0-9a-f]{40}$ ]] || {
  echo 'release bundle version or source revision does not match the request' >&2
  exit 2
}
for path in "$IMAGE_FILE" "$QUADLET" "$TEMPLATE" "$RUNTIME_ENV" \
  /srv/legal-mcp/lifecycle/active-generation; do
  [[ -f "$path" && ! -L "$path" ]] || { echo "required host file is missing or unsafe: $path" >&2; exit 1; }
done
[[ ! -e /srv/legal-mcp/lifecycle/.deployment-transaction ]] || {
  echo 'a corpus deployment transaction must be completed first' >&2
  exit 1
}
[[ ! -e /etc/legal-mcp/.auth-transaction ]] || {
  echo 'an authentication transaction must be recovered first' >&2
  exit 1
}
[[ ! -e "$TRANSACTION" ]] || {
  echo 'an image transaction already exists; run this command with --recover first' >&2
  exit 1
}
EXPECTED_GENERATION="$(</srv/legal-mcp/lifecycle/active-generation)"
[[ "$EXPECTED_GENERATION" =~ ^[0-9a-f]{64}$ ]] || { echo 'active generation is malformed' >&2; exit 1; }
OLD_IMAGE="$(<"$IMAGE_FILE")"
[[ "$OLD_IMAGE" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ ]] || {
  echo 'currently pinned image is malformed' >&2
  exit 1
}
load_runtime_contract "$RUNTIME_ENV"
read_probe_key
ufw_is_fail_closed || {
  close_ingress || true
  echo 'UFW must match the exact fail-closed allowlist; image update was refused' >&2
  exit 1
}
systemctl is-active --quiet "$SERVICE" || { echo 'image updates require an active service' >&2; exit 1; }
wait_for_exact_generation "$EXPECTED_GENERATION"
probe_auth_boundary http://127.0.0.1:51235/mcp \
  http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp

podman pull "$NEW_IMAGE"
verify_image_runtime "$NEW_IMAGE" "$EXPECTED_VERSION" "$EXPECTED_REVISION"
podman run --rm --network=none --user=0:0 --read-only --cap-drop=all \
  --security-opt=no-new-privileges --pids-limit=256 --memory=6g --memory-swap=6g \
  --tmpfs=/tmp:rw,nodev,nosuid,noexec,size=64m,mode=1777 \
  --volume=/srv/legal-mcp:/var/lib/legal-mcp:ro,nodev,nosuid \
  "$NEW_IMAGE" verify --quiet >/dev/null

transaction_tmp="${TRANSACTION}.preparing.$$"
install -d -o root -g root -m 0700 "$transaction_tmp"
cp --preserve=mode,ownership,timestamps "$IMAGE_FILE" "$transaction_tmp/image"
cp --preserve=mode,ownership,timestamps "$QUADLET" "$transaction_tmp/legal-mcp.container"
cp --preserve=mode,ownership,timestamps "$TEMPLATE" "$transaction_tmp/legal-mcp.container.template"
cp --preserve=mode,ownership,timestamps "$RUNTIME_ENV" "$transaction_tmp/runtime.env"
printf '%s\n' "$EXPECTED_GENERATION" > "$transaction_tmp/expected-generation"
systemctl is-enabled --quiet "$SERVICE" && touch "$transaction_tmp/service-was-enabled"
systemctl is-active --quiet "$SERVICE" && touch "$transaction_tmp/service-was-active"
systemctl is-enabled --quiet caddy.service && touch "$transaction_tmp/caddy-was-enabled"
systemctl is-active --quiet caddy.service && touch "$transaction_tmp/caddy-was-active"
port_80_open=false
port_443_open=false
ufw_rule_exists 80 && port_80_open=true
ufw_rule_exists 443 && port_443_open=true
[[ "$port_80_open" = "$port_443_open" ]] || {
  echo 'UFW 80/443 state is inconsistent; refusing image update' >&2
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
  echo 'container image update rolled back' >&2
  exit "$status"
}
trap rollback ERR HUP INT TERM EXIT

close_ingress
printf '%s\n' "$NEW_IMAGE" > "$IMAGE_FILE"
chown root:root "$IMAGE_FILE"
chmod 600 "$IMAGE_FILE"
rendered="$(mktemp /etc/containers/systemd/.legal-mcp.container.XXXXXX)"
sed "s|__IMAGE_DIGEST__|$NEW_IMAGE|g" "$SOURCE_TEMPLATE" > "$rendered"
chown root:root "$rendered"
chmod 644 "$rendered"
mv -fT "$rendered" "$QUADLET"
install -o root -g root -m 0644 "$SOURCE_TEMPLATE" "$TEMPLATE"
systemctl daemon-reload
systemctl restart "$SERVICE"
wait_for_exact_generation "$EXPECTED_GENERATION"
probe_auth_boundary http://127.0.0.1:51235/mcp \
  http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp
expected_image_id="$(podman image inspect "$NEW_IMAGE" --format '{{.Id}}')"
running_image_id="$(podman inspect australian-legal-mcp --format '{{.Image}}')"
[[ "$running_image_id" = "$expected_image_id" ]] || {
  echo 'running container does not use the requested image digest' >&2
  exit 1
}
restore_unit_enablement "$SERVICE" service-was-enabled
restore_unit_enablement caddy.service caddy-was-enabled
open_recorded_ingress

trap - ERR HUP INT TERM EXIT
rm -rf "$TRANSACTION"
sync -f /etc/legal-mcp
unset PROBE_API_KEY
echo "container image updated to $NEW_IMAGE"
