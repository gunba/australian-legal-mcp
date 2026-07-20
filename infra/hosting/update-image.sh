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
usage: sudo /usr/local/sbin/legal-mcp-update-image \
  --image ghcr.io/gunba/australian-legal-mcp@sha256:DIGEST \
  --version X.Y.Z \
  --template PATH

For the one empty-host software cutover before the first corpus activation:
  sudo update-image.sh --bootstrap-empty-host \
    --image ghcr.io/gunba/australian-legal-mcp@sha256:DIGEST \
    --version X.Y.Z --template PATH
To roll back an interrupted empty-host cutover, use the same release bundle:
  sudo update-image.sh --recover --bootstrap-empty-host

For the one hard Arroy-v20 to flat-int8 hosted cutover, after the publisher has
prepared and uploaded the target generation:
  sudo /usr/local/sbin/legal-mcp-update-image --flat-int8-cutover \
    --generation TARGET_GENERATION \
    --expected-current-generation ARROY_V20_GENERATION \
    --image ghcr.io/gunba/australian-legal-mcp@sha256:DIGEST \
    --version X.Y.Z --template PATH
Recover an interrupted cutover through the same stable launcher:
  sudo /usr/local/sbin/legal-mcp-update-image \
    --recover --flat-int8-cutover

If the configured mode contains api-key, stream one valid plaintext probe key
on standard input. To roll back an update interrupted by power loss or SIGKILL:
  sudo /usr/local/sbin/legal-mcp-update-image --recover \
    [</root/one-time-probe-key]
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
# The stable launcher locks the real host transaction inode before selecting
# and mounting an immutable implementation. Reuse only that documented open
# file description; direct release-bundle/bootstrap calls acquire the real
# lock themselves.
INHERITED_LOCK_FD="${LEGAL_MCP_HOST_TRANSACTION_LOCK_FD:-9}"
if [[ "$INHERITED_LOCK_FD" =~ ^[0-9]+$ \
  && -e "/proc/$$/fd/$INHERITED_LOCK_FD" \
  && "$(stat -Lc '%d:%i' "/proc/$$/fd/$INHERITED_LOCK_FD" 2>/dev/null || true)" \
    = "$(stat -Lc '%d:%i' "$LOCK_FILE")" ]]; then
  HOST_LOCK_FD="$INHERITED_LOCK_FD"
else
  exec {HOST_LOCK_FD}<>"$LOCK_FILE"
fi
flock -x "$HOST_LOCK_FD"

IMAGE_FILE=/etc/legal-mcp/image
QUADLET=/etc/containers/systemd/legal-mcp.container
TEMPLATE=/usr/local/libexec/legal-mcp/legal-mcp.container.template
RUNTIME_ENV=/etc/legal-mcp/runtime.env
API_KEYS=/etc/legal-mcp/api-keys.json
CADDYFILE=/etc/caddy/Caddyfile
HOST_TOOLS_MARKER=/etc/legal-mcp/host-tools
HOST_TOOL_LAUNCHER=/usr/local/libexec/legal-mcp/host-tool-launcher
HOST_TOOL_LAUNCHER_MARKER=/etc/legal-mcp/host-tool-launcher
CONFIGURE_AUTH_POINTER=/etc/legal-mcp/configure-auth-implementation
UPDATE_IMAGE_POINTER=/etc/legal-mcp/update-image-implementation
HOST_TOOL_IMPLEMENTATION_DIR=/usr/local/libexec/legal-mcp/host-tools
AUTH_READY=/etc/legal-mcp/auth-ready
TRANSACTION=/etc/legal-mcp/.image-transaction
TRANSACTION_PREPARING=${TRANSACTION}.preparing
TRANSACTION_PREPARING_RETIRED=${TRANSACTION}.preparing-retired
CUTOVER_TRANSACTION_PREPARING=${TRANSACTION}.flat-int8-preparing
CUTOVER_TRANSACTION_PREPARING_RETIRED=${TRANSACTION}.flat-int8-preparing-retired
TRANSACTION_RETIRING=${TRANSACTION}.retiring
TRANSACTION_RETIRED=${TRANSACTION}.retired
SERVICE=legal-mcp.service
NEW_IMAGE=''
EXPECTED_VERSION=''
SOURCE_TEMPLATE=''
RECOVER=false
BOOTSTRAP_EMPTY_HOST=false
FLAT_INT8_CUTOVER=false
CUTOVER_GENERATION=''
CUTOVER_EXPECTED_CURRENT_GENERATION=''
CUTOVER_START_ARM=/run/legal-mcp/flat-int8-cutover-start-armed
CUTOVER_UPLOAD_AUTHORIZATION=''
IMAGE_RETIREMENT_WAS_PENDING=false
IMAGE_PREPARATION_WAS_PENDING=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --image) NEW_IMAGE="${2:-}"; shift 2 ;;
    --version) EXPECTED_VERSION="${2:-}"; shift 2 ;;
    --template) SOURCE_TEMPLATE="${2:-}"; shift 2 ;;
    --recover) RECOVER=true; shift ;;
    --bootstrap-empty-host) BOOTSTRAP_EMPTY_HOST=true; shift ;;
    --flat-int8-cutover) FLAT_INT8_CUTOVER=true; shift ;;
    --generation) CUTOVER_GENERATION="${2:-}"; shift 2 ;;
    --expected-current-generation) CUTOVER_EXPECTED_CURRENT_GENERATION="${2:-}"; shift 2 ;;
    *) usage ;;
  esac
done

canonical_image_id() {
  local value="$1"
  [[ "$value" =~ ^(sha256:)?([0-9a-f]{64})$ ]] || {
    echo 'Podman returned a malformed image ID' >&2
    return 1
  }
  printf 'sha256:%s\n' "${BASH_REMATCH[2]}"
}

read_systemctl_enablement() {
  local unit="$1" output status
  if output="$(systemctl is-enabled "$unit" 2>/dev/null)"; then
    status=0
  else
    status=$?
  fi
  case "$status:$output" in
    0:enabled|0:generated|1:disabled) printf '%s\n' "$output" ;;
    *)
      echo "could not determine exact systemd enablement for $unit (status $status, state ${output:-<empty>})" >&2
      return 1
      ;;
  esac
}

read_systemctl_activity() {
  local unit="$1" output status
  if output="$(systemctl is-active "$unit" 2>/dev/null)"; then
    status=0
  else
    status=$?
  fi
  case "$status:$output" in
    0:active|3:inactive) printf '%s\n' "$output" ;;
    *)
      echo "could not determine exact systemd activity for $unit (status $status, state ${output:-<empty>})" >&2
      return 1
      ;;
  esac
}

ufw_rule_state() {
  local port="$1" report status
  if report="$(ufw status verbose)"; then
    :
  else
    status=$?
    echo "could not inspect UFW rule state for port $port (status $status)" >&2
    return 1
  fi
  if grep -Eq "^${port}/tcp[[:space:]]+ALLOW IN([[:space:]]|$)" <<< "$report"; then
    printf '%s\n' present
    return 0
  else
    status=$?
  fi
  if [[ $status -eq 1 ]]; then
    printf '%s\n' absent
    return 0
  fi
  echo "could not evaluate UFW rule state for port $port" >&2
  return 1
}

podman_image_state() {
  local image="$1" status
  if podman image exists "$image"; then
    printf '%s\n' present
    return 0
  else
    status=$?
  fi
  if [[ $status -eq 1 ]]; then
    printf '%s\n' absent
    return 0
  fi
  echo "could not inspect Podman image state for $image (status $status)" >&2
  return 1
}

podman_container_state() {
  local container="$1" status
  if podman container exists "$container"; then
    printf '%s\n' present
    return 0
  else
    status=$?
  fi
  if [[ $status -eq 1 ]]; then
    printf '%s\n' absent
    return 0
  fi
  echo "could not inspect Podman container state for $container (status $status)" >&2
  return 1
}

ufw_is_fail_closed() {
  local report admin_source_file=/etc/legal-mcp/admin-source-ip admin_source status
  [[ -f "$admin_source_file" && ! -L "$admin_source_file" \
    && "$(stat -c '%U:%G:%a:%h' "$admin_source_file")" = root:root:600:1 ]] || return 1
  admin_source="$(<"$admin_source_file")"
  [[ "$admin_source" =~ ^[0-9A-Fa-f:.]{2,45}$ ]] || return 1
  if report="$(ufw status verbose)"; then
    :
  else
    status=$?
    echo "could not inspect the UFW allowlist (status $status)" >&2
    return 1
  fi
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
  local port state enabled activity comment
  systemctl disable --now caddy.service >/dev/null 2>&1 || return 1
  for port in 80 443; do
    state="$(ufw_rule_state "$port")" || return 1
    if [[ "$state" = present ]]; then
      case "$port" in
        80) comment='Caddy ACME HTTP' ;;
        443) comment='Australian Legal MCP HTTPS' ;;
        *) return 1 ;;
      esac
      ufw --force delete allow "$port/tcp" comment "$comment" >/dev/null 2>&1 || return 1
    fi
  done
  enabled="$(read_systemctl_enablement caddy.service)" || return 1
  activity="$(read_systemctl_activity caddy.service)" || return 1
  [[ "$enabled" = disabled && "$activity" = inactive ]] || {
    echo 'failed to prove Caddy disabled and inactive' >&2
    return 1
  }
  for port in 80 443; do
    state="$(ufw_rule_state "$port")" || return 1
    [[ "$state" = absent ]] || {
      echo 'failed to prove Caddy and UFW public ingress closed' >&2
      return 1
    }
  done
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
  local expected="$1" deadline=$((SECONDS + 180)) activity
  while (( SECONDS < deadline )); do
    if curl --fail --silent --max-time 5 http://127.0.0.1:51235/readyz 2>/dev/null |
      python3 -c 'import json,sys; value=json.load(sys.stdin); expected=sys.argv[1]; raise SystemExit(0 if value == {"status":"ok","generation":expected} else 1)' \
        "$expected" 2>/dev/null; then
      return 0
    fi
    activity="$(read_systemctl_activity "$SERVICE")" || return 1
    [[ "$activity" = active ]] || return 1
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
  local image_title image_description image_version image_source image_revision
  local image_licenses image_ann_format image_digest expected_digest binary_version
  image_title="$(podman image inspect "$image" --format '{{index .Labels "org.opencontainers.image.title"}}')"
  image_description="$(podman image inspect "$image" --format '{{index .Labels "org.opencontainers.image.description"}}')"
  image_version="$(podman image inspect "$image" --format '{{index .Labels "org.opencontainers.image.version"}}')"
  image_source="$(podman image inspect "$image" --format '{{index .Labels "org.opencontainers.image.source"}}')"
  image_revision="$(podman image inspect "$image" --format '{{index .Labels "org.opencontainers.image.revision"}}')"
  image_licenses="$(podman image inspect "$image" --format '{{index .Labels "org.opencontainers.image.licenses"}}')"
  image_ann_format="$(podman image inspect "$image" --format '{{index .Labels "io.australian-legal-mcp.ann-format"}}')"
  image_digest="$(podman image inspect "$image" --format '{{.Digest}}')"
  expected_digest="${image##*@}"
  [[ "$image_title" = 'Australian Legal MCP' \
    && "$image_description" = 'Source-grounded Australian legal MCP server' \
    && "$image_version" = "$expected_version" \
    && "$image_source" = https://github.com/gunba/australian-legal-mcp \
    && "$image_revision" = "$expected_revision" \
    && "$image_licenses" = MIT \
    && "$image_ann_format" = flat-int8-v1 \
    && "$image_digest" = "$expected_digest" ]] || {
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

image_path_is_absent() {
  [[ ! -e "$1" && ! -L "$1" ]]
}

require_image_transaction_directory() {
  local path="$1"
  [[ -d "$path" && ! -L "$path" \
    && "$(stat -c '%U:%G:%a' "$path")" = root:root:700 ]] || {
    echo "unsafe image transaction directory: $path" >&2
    return 1
  }
}

delete_retired_image_directory() {
  local path="$1"
  image_path_is_absent "$path" && return 0
  require_image_transaction_directory "$path"
  rm -rf --one-file-system -- "$path"
  image_path_is_absent "$path" || {
    echo "image transaction cleanup did not complete: $path" >&2
    return 1
  }
  sync -f /etc/legal-mcp
}

retired_image_payload_state() {
  local directory="$1" name
  shift
  bootstrap_directory_contains_only "$directory" "$@" || {
    echo "retired image transaction contains unexpected state: $directory" >&2
    return 1
  }
  for name in "$@"; do
    if image_path_is_absent "$directory/$name"; then
      printf '%s\n' partial
      return 0
    fi
  done
  printf '%s\n' complete
}

retire_image_directory_for_deletion() {
  local path="$1" retired_path="$2"
  image_path_is_absent "$retired_path" || {
    echo "image deletion retirement already exists: $retired_path" >&2
    return 1
  }
  require_image_transaction_directory "$path"
  mv -T "$path" "$retired_path"
  sync -f /etc/legal-mcp
  delete_retired_image_directory "$retired_path"
}

finalize_image_preparation_retirement() {
  if ! image_path_is_absent "$TRANSACTION_PREPARING" \
    && ! image_path_is_absent "$TRANSACTION_PREPARING_RETIRED"; then
    echo 'image preparation has conflicting deletion states' >&2
    return 1
  fi
  if ! image_path_is_absent "$TRANSACTION_PREPARING"; then
    IMAGE_PREPARATION_WAS_PENDING=true
    retire_image_directory_for_deletion \
      "$TRANSACTION_PREPARING" "$TRANSACTION_PREPARING_RETIRED"
  elif ! image_path_is_absent "$TRANSACTION_PREPARING_RETIRED"; then
    IMAGE_PREPARATION_WAS_PENDING=true
    delete_retired_image_directory "$TRANSACTION_PREPARING_RETIRED"
  fi
}

finalize_image_transaction_retirement() {
  if ! image_path_is_absent "$TRANSACTION_RETIRING"; then
    IMAGE_RETIREMENT_WAS_PENDING=true
    require_image_transaction_directory "$TRANSACTION_RETIRING"
    image_path_is_absent "$TRANSACTION_RETIRED" || {
      echo 'image transaction has conflicting retirement directories' >&2
      return 1
    }
    # Make removal of the canonical transaction name durable before exposing
    # the directory as deletion-only state.
    sync -f /etc/legal-mcp
    mv -T "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"
    sync -f /etc/legal-mcp
  fi
  if ! image_path_is_absent "$TRANSACTION_RETIRED"; then
    IMAGE_RETIREMENT_WAS_PENDING=true
    delete_retired_image_directory "$TRANSACTION_RETIRED"
  fi
}

retire_image_transaction() {
  if ! image_path_is_absent "$TRANSACTION_RETIRING" \
    || ! image_path_is_absent "$TRANSACTION_RETIRED"; then
    echo 'image transaction retirement state is not clean' >&2
    return 1
  fi
  mv -T "$TRANSACTION" "$TRANSACTION_RETIRING"
  sync -f /etc/legal-mcp
  mv -T "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"
  sync -f /etc/legal-mcp
  delete_retired_image_directory "$TRANSACTION_RETIRED"
}

bootstrap_path_is_absent() {
  image_path_is_absent "$1"
}

bootstrap_directory_is_empty() {
  local directory="$1" found
  found="$(find "$directory" -mindepth 1 -maxdepth 1 -printf x -quit)" || {
    echo "could not inspect bootstrap directory contents: $directory" >&2
    return 1
  }
  [[ -z "$found" ]]
}

bootstrap_directory_contains_only() {
  local directory="$1" found name
  local -a exclusions=()
  shift
  for name in "$@"; do
    exclusions+=('!' -name "$name")
  done
  found="$(find "$directory" -mindepth 1 -maxdepth 1 \
    "${exclusions[@]}" -printf x -quit)" || {
    echo "could not inspect bootstrap directory contents: $directory" >&2
    return 1
  }
  [[ -z "$found" ]]
}

bootstrap_require_regular() {
  local path="$1" owner="$2" group="$3" mode="$4"
  [[ -f "$path" && ! -L "$path" \
    && "$(stat -c '%U:%G:%a:%h' "$path")" = "$owner:$group:$mode:1" ]] || {
    echo "unsafe bootstrap host file: $path" >&2
    return 1
  }
}

bootstrap_require_empty_regular() {
  local path="$1" owner="$2" group="$3" mode="$4"
  bootstrap_require_regular "$path" "$owner" "$group" "$mode" || return 1
  [[ "$(stat -c '%s' "$path")" = 0 ]] || {
    echo "bootstrap host contract file must be empty: $path" >&2
    return 1
  }
}

bootstrap_require_acl() {
  local path="$1" expected="$2"
  [[ "$(getfacl --absolute-names --numeric --omit-header "$path")" = "$expected" ]] || {
    echo "unsafe bootstrap host ACL: $path" >&2
    return 1
  }
}

bootstrap_require_safe_directory() {
  local path="$1" owner="$2" group="$3" mode
  [[ -d "$path" && ! -L "$path" \
    && "$(stat -c '%U:%G' "$path")" = "$owner:$group" ]] || {
    echo "unsafe bootstrap host directory: $path" >&2
    return 1
  }
  mode="$(stat -c '%a' "$path")"
  if [[ ! "$mode" =~ ^[0-7]{3}$ ]] || (( (8#$mode & 8#022) != 0 )); then
    echo "bootstrap host directory is group/other writable: $path" >&2
    return 1
  fi
}

bootstrap_require_exact_directory() {
  local path="$1" owner="$2" group="$3" mode="$4"
  [[ -d "$path" && ! -L "$path" \
    && "$(stat -c '%U:%G:%a' "$path")" = "$owner:$group:$mode" ]] || {
    echo "unsafe bootstrap host directory: $path" >&2
    return 1
  }
}

bootstrap_require_release_file() {
  local path="$1" executable="${2:-false}" mode uid
  [[ -f "$path" && ! -L "$path" && "$(stat -c '%h' "$path")" = 1 ]] || {
    echo "bootstrap release asset is missing or unsafe: $path" >&2
    return 1
  }
  mode="$(stat -c '%a' "$path")"
  uid="$(stat -c '%u' "$path")"
  [[ "$mode" =~ ^[0-7]{3}$ && "$uid" != 971 && "$uid" != 973 ]] || {
    echo "bootstrap release asset has an unsafe identity or mode: $path" >&2
    return 1
  }
  (( (8#$mode & 8#022) == 0 )) || {
    echo "bootstrap release asset is group/other writable: $path" >&2
    return 1
  }
  if [[ "$executable" = true && ! -x "$path" ]]; then
    echo "bootstrap release executable is not executable: $path" >&2
    return 1
  fi
}

bootstrap_require_release_directory() {
  local path="$1" mode uid
  [[ -d "$path" && ! -L "$path" ]] || {
    echo "bootstrap release directory is missing or unsafe: $path" >&2
    return 1
  }
  mode="$(stat -c '%a' "$path")"
  uid="$(stat -c '%u' "$path")"
  [[ "$mode" =~ ^[0-7]{3}$ && "$uid" != 971 && "$uid" != 973 ]] || return 1
  (( (8#$mode & 8#022) == 0 )) || {
    echo "bootstrap release directory is group/other writable: $path" >&2
    return 1
  }
}

bootstrap_load_bundle() {
  local template="$1" expected_version="$2" binary_version
  local -a versions revisions
  bootstrap_require_release_file "$template"
  [[ "$(grep -o '__IMAGE_DIGEST__' "$template" | wc -l)" = 1 \
    && "$(grep -Fxc 'ExecCondition=/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready' \
      "$template")" = 1 ]] || {
    echo 'bootstrap Quadlet template lacks its exact image or auth-ready gate' >&2
    return 1
  }
  BOOTSTRAP_SOURCE_TEMPLATE="$(readlink -f "$template")"
  BOOTSTRAP_BUNDLE_ROOT="$(cd "$(dirname "$BOOTSTRAP_SOURCE_TEMPLATE")/../.." && pwd -P)"
  [[ "$BOOTSTRAP_SOURCE_TEMPLATE" = "$BOOTSTRAP_BUNDLE_ROOT/infra/hosting/legal-mcp.container.template" ]] || {
    echo 'bootstrap template is not in a complete Linux release bundle' >&2
    return 1
  }
  bootstrap_require_release_directory "$BOOTSTRAP_BUNDLE_ROOT"
  bootstrap_require_release_directory "$BOOTSTRAP_BUNDLE_ROOT/infra"
  bootstrap_require_release_directory "$BOOTSTRAP_BUNDLE_ROOT/infra/hosting"
  bootstrap_require_release_directory "$BOOTSTRAP_BUNDLE_ROOT/scripts"
  bootstrap_require_release_file "$BOOTSTRAP_BUNDLE_ROOT/infra/hosting/update-image.sh" true
  bootstrap_require_release_file "$BOOTSTRAP_BUNDLE_ROOT/infra/hosting/configure-auth.sh" true
  [[ ! -L "${BASH_SOURCE[0]}" \
    && "$(readlink -f "${BASH_SOURCE[0]}")" = "$BOOTSTRAP_BUNDLE_ROOT/infra/hosting/update-image.sh" ]] || {
    echo 'empty-host image cutover must run directly from the version-matched release bundle' >&2
    return 1
  }
  BOOTSTRAP_BUNDLE_BINARY="$BOOTSTRAP_BUNDLE_ROOT/legal-mcp"
  bootstrap_require_release_file "$BOOTSTRAP_BUNDLE_ROOT/Containerfile"
  bootstrap_require_release_file "$BOOTSTRAP_BUNDLE_ROOT/SOURCE_COMMIT"
  bootstrap_require_release_file "$BOOTSTRAP_BUNDLE_BINARY" true
  bootstrap_require_release_file "$BOOTSTRAP_BUNDLE_ROOT/libonnxruntime.so"
  bootstrap_require_release_file "$BOOTSTRAP_BUNDLE_ROOT/scripts/legal-mcp-host-deploy" true
  bootstrap_require_release_file "$BOOTSTRAP_BUNDLE_ROOT/scripts/legal-mcp-publisher-command" true
  mapfile -t versions < <(awk -F= '$1 == "ARG VERSION" {print $2}' "$BOOTSTRAP_BUNDLE_ROOT/Containerfile")
  mapfile -t revisions < "$BOOTSTRAP_BUNDLE_ROOT/SOURCE_COMMIT"
  [[ ${#versions[@]} -eq 1 && "${versions[0]}" = "$expected_version" \
    && ${#revisions[@]} -eq 1 && "${revisions[0]}" =~ ^[0-9a-f]{40}$ ]] || {
    echo 'bootstrap release version or SOURCE_COMMIT is invalid' >&2
    return 1
  }
  BOOTSTRAP_VERSION="$expected_version"
  BOOTSTRAP_REVISION="${revisions[0]}"
  binary_version="$(env -u LD_LIBRARY_PATH -u LD_PRELOAD \
    "$BOOTSTRAP_BUNDLE_BINARY" --version)"
  [[ "$binary_version" = "legal-mcp $expected_version" ]] || {
    echo 'bootstrap release binary version does not match the bundle' >&2
    return 1
  }
  env -u LD_LIBRARY_PATH -u LD_PRELOAD \
    ORT_DYLIB_PATH="$BOOTSTRAP_BUNDLE_ROOT/libonnxruntime.so" \
    "$BOOTSTRAP_BUNDLE_BINARY" verify-runtime |
    grep -Fq '"onnx_runtime_ready":true' || {
      echo 'bootstrap release binary failed runtime verification' >&2
      return 1
    }
}

bootstrap_ufw_is_ssh_only() {
  local report admin_source status
  bootstrap_require_regular /etc/legal-mcp/admin-source-ip root root 600 || return 1
  admin_source="$(</etc/legal-mcp/admin-source-ip)"
  [[ "$admin_source" =~ ^[0-9A-Fa-f:.]{2,45}$ ]] || return 1
  if report="$(ufw status verbose)"; then
    :
  else
    status=$?
    echo "could not inspect the bootstrap UFW allowlist (status $status)" >&2
    return 1
  fi
  grep -Fxq 'Status: active' <<< "$report" \
    && grep -Eq '^Default: deny \(incoming\), allow \(outgoing\), (disabled|deny) \(routed\)$' <<< "$report" \
    && awk -v admin="$admin_source" '
      /(DENY|REJECT|LIMIT) IN/ { bad=1 }
      /ALLOW IN/ {
        target=$1
        source=""
        for (i=1; i<NF; i++) if ($i == "IN") source=$(i+1)
        if (target != "22/tcp" || source != admin) bad=1
        ssh++
      }
      END { exit !bad && ssh == 1 ? 0 : 1 }
    ' <<< "$report"
}

bootstrap_services_are_off() {
  local service_enabled service_activity caddy_enabled caddy_activity
  local invalid=false listeners web_listener container_state
  service_enabled="$(read_systemctl_enablement "$SERVICE")" || return 1
  service_activity="$(read_systemctl_activity "$SERVICE")" || return 1
  caddy_enabled="$(read_systemctl_enablement caddy.service)" || return 1
  caddy_activity="$(read_systemctl_activity caddy.service)" || return 1
  if [[ "$service_enabled" != generated ]]; then
    echo "$SERVICE must be generated for an empty-host image cutover" >&2
    invalid=true
  fi
  if [[ "$service_activity" != inactive ]]; then
    echo "$SERVICE must be inactive for an empty-host image cutover" >&2
    invalid=true
  fi
  if [[ "$caddy_enabled" != disabled ]]; then
    echo 'caddy.service must be disabled for an empty-host image cutover' >&2
    invalid=true
  fi
  if [[ "$caddy_activity" != inactive ]]; then
    echo 'caddy.service must be inactive for an empty-host image cutover' >&2
    invalid=true
  fi
  if [[ "$invalid" = true ]]; then
    close_ingress || return 1
    if [[ "$service_activity" = active ]]; then
      systemctl stop "$SERVICE" >/dev/null 2>&1 || return 1
      service_activity="$(read_systemctl_activity "$SERVICE")" || return 1
      [[ "$service_activity" = inactive ]] || return 1
    fi
    return 1
  fi
  bootstrap_ufw_is_ssh_only || {
    close_ingress || return 1
    echo 'empty-host image cutover requires the exact SSH-only UFW allowlist' >&2
    return 1
  }
  listeners="$(ss --listening --tcp --numeric --no-header)" || {
    echo 'could not inspect empty-host listening sockets' >&2
    return 1
  }
  web_listener="$(awk '$4 ~ /:(80|443|51235)$/ { print "present"; exit }' \
    <<< "$listeners")" || {
    echo 'could not evaluate empty-host listening sockets' >&2
    return 1
  }
  if [[ -n "$web_listener" ]]; then
    echo 'empty-host web or service ports must not be listening' >&2
    return 1
  fi
  container_state="$(podman_container_state australian-legal-mcp)" || return 1
  if [[ "$container_state" = present ]]; then
    echo 'empty-host image cutover refuses an existing service container' >&2
    return 1
  fi
}

bootstrap_validate_host_tools() {
  local deploy_sha publisher_sha configure_auth_sha update_image_sha
  local container_template_sha sudoers_sha expected_policy
  local -a marker
  bootstrap_require_regular /etc/legal-mcp/host-tools root root 444
  bootstrap_require_acl /etc/legal-mcp/host-tools $'user::r--\ngroup::r--\nother::r--'
  mapfile -t marker < /etc/legal-mcp/host-tools
  [[ ${#marker[@]} -eq 9 && "${marker[0]}" = LEGAL_MCP_HOST_TOOLS_V2 \
    && "${marker[1]}" = "VERSION=$BOOTSTRAP_VERSION" \
    && "${marker[2]}" = "SOURCE_COMMIT=$BOOTSTRAP_REVISION" \
    && "${marker[3]}" =~ ^HOST_DEPLOY_SHA256=([0-9a-f]{64})$ ]] || {
    echo 'installed host-tool marker does not match this release' >&2
    return 1
  }
  deploy_sha="${BASH_REMATCH[1]}"
  [[ "${marker[4]}" =~ ^PUBLISHER_COMMAND_SHA256=([0-9a-f]{64})$ ]] || return 1
  publisher_sha="${BASH_REMATCH[1]}"
  [[ "${marker[5]}" =~ ^CONFIGURE_AUTH_SHA256=([0-9a-f]{64})$ ]] || return 1
  configure_auth_sha="${BASH_REMATCH[1]}"
  [[ "${marker[6]}" =~ ^UPDATE_IMAGE_SHA256=([0-9a-f]{64})$ ]] || return 1
  update_image_sha="${BASH_REMATCH[1]}"
  [[ "${marker[7]}" =~ ^CONTAINER_TEMPLATE_SHA256=([0-9a-f]{64})$ ]] || return 1
  container_template_sha="${BASH_REMATCH[1]}"
  [[ "${marker[8]}" =~ ^SUDOERS_SHA256=([0-9a-f]{64})$ ]] || return 1
  sudoers_sha="${BASH_REMATCH[1]}"
  bootstrap_require_regular /usr/local/sbin/legal-mcp-host-deploy root root 755
  bootstrap_require_regular /usr/local/sbin/legal-mcp-publisher-command root root 755
  bootstrap_require_regular /usr/local/sbin/legal-mcp-configure-auth root root 755
  bootstrap_require_regular /usr/local/sbin/legal-mcp-update-image root root 755
  bootstrap_require_regular "$TEMPLATE" root root 644
  bootstrap_require_regular /etc/sudoers.d/legal-mcp-publisher root root 440
  [[ "$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}')" = "$deploy_sha" \
    && "$(sha256sum /usr/local/sbin/legal-mcp-publisher-command | awk '{print $1}')" = "$publisher_sha" \
    && "$(sha256sum "$TEMPLATE" | awk '{print $1}')" = "$container_template_sha" \
    && "$(sha256sum /etc/sudoers.d/legal-mcp-publisher | awk '{print $1}')" = "$sudoers_sha" \
    && "$(sha256sum "$BOOTSTRAP_BUNDLE_ROOT/scripts/legal-mcp-host-deploy" | awk '{print $1}')" = "$deploy_sha" \
    && "$(sha256sum "$BOOTSTRAP_BUNDLE_ROOT/scripts/legal-mcp-publisher-command" | awk '{print $1}')" = "$publisher_sha" \
    && "$(sha256sum "$BOOTSTRAP_BUNDLE_ROOT/infra/hosting/configure-auth.sh" | awk '{print $1}')" = "$configure_auth_sha" \
    && "$(sha256sum "$BOOTSTRAP_BUNDLE_ROOT/infra/hosting/update-image.sh" | awk '{print $1}')" = "$update_image_sha" \
    && "$(sha256sum "${BASH_SOURCE[0]}" | awk '{print $1}')" = "$update_image_sha" \
    && "$(sha256sum "$BOOTSTRAP_SOURCE_TEMPLATE" | awk '{print $1}')" = "$container_template_sha" ]] || {
    echo 'installed host tools are not the exact version-matched release helpers' >&2
    return 1
  }
  expected_policy="$(mktemp /run/legal-mcp-bootstrap-sudoers.XXXXXX)"
  printf '%s\n' \
    'Defaults:legal-mcp-publisher !requiretty' \
    "legal-mcp-publisher ALL=(root) NOPASSWD: sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^prepare [0-9a-f]{64}$, sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^activate [0-9a-f]{64}$, sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^abort [0-9a-f]{64}$" \
    > "$expected_policy"
  if ! cmp --silent "$expected_policy" /etc/sudoers.d/legal-mcp-publisher; then
    rm -f "$expected_policy"
    echo 'installed publisher sudo policy is not exact and sandboxed' >&2
    return 1
  fi
  rm -f "$expected_policy"
  visudo -cf /etc/sudoers.d/legal-mcp-publisher >/dev/null
  validate_installed_host_tool_launchers \
    "$configure_auth_sha" "$update_image_sha" outer
}

validate_installed_host_tool_launchers() {
  local expected_configure_sha="$1" expected_update_sha="$2"
  local expected_entrypoint_state="$3"
  local launcher_sha configure_sha update_sha path
  local -a marker
  bootstrap_require_regular "$HOST_TOOL_LAUNCHER_MARKER" root root 444
  bootstrap_require_acl "$HOST_TOOL_LAUNCHER_MARKER" $'user::r--\ngroup::r--\nother::r--'
  mapfile -t marker < "$HOST_TOOL_LAUNCHER_MARKER"
  [[ ${#marker[@]} -eq 2 \
    && "${marker[0]}" = LEGAL_MCP_HOST_TOOL_LAUNCHER_V1 \
    && "${marker[1]}" =~ ^LAUNCHER_SHA256=([0-9a-f]{64})$ ]] || {
      echo 'installed host-tool launcher marker is malformed' >&2
      return 1
    }
  launcher_sha="${BASH_REMATCH[1]}"
  bootstrap_require_regular "$HOST_TOOL_LAUNCHER" root root 755
  [[ "$(sha256sum "$HOST_TOOL_LAUNCHER" | awk '{print $1}')" = "$launcher_sha" ]] || {
    echo 'installed canonical host-tool launcher changed' >&2
    return 1
  }
  bootstrap_require_regular /usr/local/sbin/legal-mcp-configure-auth root root 755
  bootstrap_require_regular /usr/local/sbin/legal-mcp-update-image root root 755
  case "$expected_entrypoint_state" in
    outer)
      [[ "$(sha256sum /usr/local/sbin/legal-mcp-configure-auth | awk '{print $1}')" = "$launcher_sha" \
        && "$(sha256sum /usr/local/sbin/legal-mcp-update-image | awk '{print $1}')" = "$launcher_sha" ]]
      ;;
    internal)
      [[ "$(sha256sum /usr/local/sbin/legal-mcp-configure-auth | awk '{print $1}')" \
          = "$expected_configure_sha" \
        && "$(sha256sum /usr/local/sbin/legal-mcp-update-image | awk '{print $1}')" \
          = "$expected_update_sha" ]] || {
          echo 'host-tool implementation bind mounts are not exact' >&2
          return 1
        }
      ;;
    *) return 1 ;;
  esac
  for path in "$CONFIGURE_AUTH_POINTER" "$UPDATE_IMAGE_POINTER"; do
    bootstrap_require_regular "$path" root root 644
    bootstrap_require_acl "$path" $'user::rw-\ngroup::r--\nother::r--'
    [[ "$(stat -c '%s' "$path")" = 64 ]] || {
      echo "host-tool implementation pointer is not exactly 64 bytes: $path" >&2
      return 1
    }
  done
  configure_sha="$(<"$CONFIGURE_AUTH_POINTER")"
  update_sha="$(<"$UPDATE_IMAGE_POINTER")"
  [[ "$configure_sha" = "$expected_configure_sha" \
    && "$update_sha" = "$expected_update_sha" ]] || {
      echo 'installed host-tool implementation pointers do not match this release' >&2
      return 1
    }
  bootstrap_require_regular \
    "$HOST_TOOL_IMPLEMENTATION_DIR/configure-auth.$configure_sha" root root 755
  bootstrap_require_regular \
    "$HOST_TOOL_IMPLEMENTATION_DIR/update-image.$update_sha" root root 755
  [[ "$(sha256sum "$HOST_TOOL_IMPLEMENTATION_DIR/configure-auth.$configure_sha" | awk '{print $1}')" \
      = "$configure_sha" \
    && "$(sha256sum "$HOST_TOOL_IMPLEMENTATION_DIR/update-image.$update_sha" | awk '{print $1}')" \
      = "$update_sha" ]] || {
      echo 'immutable host-tool implementation changed' >&2
      return 1
    }
}

validate_v2_host_tool_release() {
  local bundle_root="$1" expected_version="$2" expected_revision="$3" source_template="$4"
  local expected_entrypoint_state="$5"
  local deploy_sha publisher_sha configure_auth_sha update_image_sha
  local container_template_sha sudoers_sha expected_policy
  local -a marker
  for path in \
    "$bundle_root/infra/hosting/configure-auth.sh" \
    "$bundle_root/infra/hosting/update-image.sh" \
    "$bundle_root/scripts/legal-mcp-host-deploy" \
    "$bundle_root/scripts/legal-mcp-publisher-command"; do
    bootstrap_require_release_file "$path" true
  done
  bootstrap_require_release_file "$source_template"
  bootstrap_require_regular /etc/legal-mcp/host-tools root root 444
  bootstrap_require_acl /etc/legal-mcp/host-tools $'user::r--\ngroup::r--\nother::r--'
  mapfile -t marker < /etc/legal-mcp/host-tools
  [[ ${#marker[@]} -eq 9 && "${marker[0]}" = LEGAL_MCP_HOST_TOOLS_V2 \
    && "${marker[1]}" = "VERSION=$expected_version" \
    && "${marker[2]}" = "SOURCE_COMMIT=$expected_revision" \
    && "${marker[3]}" =~ ^HOST_DEPLOY_SHA256=([0-9a-f]{64})$ ]] || {
    echo 'installed V2 host-tool identity does not match the image release' >&2
    return 1
  }
  deploy_sha="${BASH_REMATCH[1]}"
  [[ "${marker[4]}" =~ ^PUBLISHER_COMMAND_SHA256=([0-9a-f]{64})$ ]] || return 1
  publisher_sha="${BASH_REMATCH[1]}"
  [[ "${marker[5]}" =~ ^CONFIGURE_AUTH_SHA256=([0-9a-f]{64})$ ]] || return 1
  configure_auth_sha="${BASH_REMATCH[1]}"
  [[ "${marker[6]}" =~ ^UPDATE_IMAGE_SHA256=([0-9a-f]{64})$ ]] || return 1
  update_image_sha="${BASH_REMATCH[1]}"
  [[ "${marker[7]}" =~ ^CONTAINER_TEMPLATE_SHA256=([0-9a-f]{64})$ ]] || return 1
  container_template_sha="${BASH_REMATCH[1]}"
  [[ "${marker[8]}" =~ ^SUDOERS_SHA256=([0-9a-f]{64})$ ]] || return 1
  sudoers_sha="${BASH_REMATCH[1]}"

  bootstrap_require_regular /usr/local/sbin/legal-mcp-host-deploy root root 755
  bootstrap_require_regular /usr/local/sbin/legal-mcp-publisher-command root root 755
  bootstrap_require_regular /usr/local/sbin/legal-mcp-configure-auth root root 755
  bootstrap_require_regular /usr/local/sbin/legal-mcp-update-image root root 755
  bootstrap_require_regular "$TEMPLATE" root root 644
  bootstrap_require_regular /etc/sudoers.d/legal-mcp-publisher root root 440
  [[ "$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}')" = "$deploy_sha" \
    && "$(sha256sum /usr/local/sbin/legal-mcp-publisher-command | awk '{print $1}')" = "$publisher_sha" \
    && "$(sha256sum "$TEMPLATE" | awk '{print $1}')" = "$container_template_sha" \
    && "$(sha256sum /etc/sudoers.d/legal-mcp-publisher | awk '{print $1}')" = "$sudoers_sha" \
    && "$(sha256sum "$bundle_root/scripts/legal-mcp-host-deploy" | awk '{print $1}')" = "$deploy_sha" \
    && "$(sha256sum "$bundle_root/scripts/legal-mcp-publisher-command" | awk '{print $1}')" = "$publisher_sha" \
    && "$(sha256sum "$bundle_root/infra/hosting/configure-auth.sh" | awk '{print $1}')" = "$configure_auth_sha" \
    && "$(sha256sum "$bundle_root/infra/hosting/update-image.sh" | awk '{print $1}')" = "$update_image_sha" \
    && "$(sha256sum "$source_template" | awk '{print $1}')" = "$container_template_sha" \
    && "$(sha256sum "${BASH_SOURCE[0]}" | awk '{print $1}')" = "$update_image_sha" ]] || {
    echo 'installed and release-bundled V2 host tools do not match exactly' >&2
    return 1
  }
  expected_policy="$(mktemp /run/legal-mcp-image-sudoers.XXXXXX)"
  printf '%s\n' \
    'Defaults:legal-mcp-publisher !requiretty' \
    "legal-mcp-publisher ALL=(root) NOPASSWD: sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^prepare [0-9a-f]{64}$, sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^activate [0-9a-f]{64}$, sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^abort [0-9a-f]{64}$" \
    > "$expected_policy"
  if ! cmp --silent "$expected_policy" /etc/sudoers.d/legal-mcp-publisher; then
    rm -f "$expected_policy"
    echo 'installed publisher sudo policy is not the V2 release policy' >&2
    return 1
  fi
  rm -f "$expected_policy"
  visudo -cf /etc/sudoers.d/legal-mcp-publisher >/dev/null
  validate_installed_host_tool_launchers \
    "$configure_auth_sha" "$update_image_sha" "$expected_entrypoint_state"
}

bootstrap_validate_static_host() {
  local allow_image_transaction="$1" source fstype options xfs_details
  local host_uuid volume_uuid actual_uuid directory auth_mode external_url
  local publisher_key_re
  local -a host_marker volume_marker entries
  bootstrap_require_regular /etc/legal-mcp/host-installed root root 444
  bootstrap_require_acl /etc/legal-mcp/host-installed $'user::r--\ngroup::r--\nother::r--'
  mapfile -t host_marker < /etc/legal-mcp/host-installed
  [[ ${#host_marker[@]} -eq 2 && "${host_marker[0]}" = LEGAL_MCP_HOST_V1 \
    && "${host_marker[1]}" =~ ^VOLUME_UUID=([0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12})$ ]] || {
    echo 'installed host marker is malformed' >&2
    return 1
  }
  host_uuid="${BASH_REMATCH[1],,}"
  read -r source fstype options < <(
    findmnt --noheadings --raw --output SOURCE,FSTYPE,OPTIONS --mountpoint /srv/legal-mcp
  )
  [[ -b "$source" && "$fstype" = xfs && ",${options}," = *,noatime,* \
    && ",${options}," = *,nodev,* && ",${options}," = *,noexec,* \
    && ",${options}," = *,nosuid,* ]] || {
    echo 'mounted corpus volume violates the bootstrap host contract' >&2
    return 1
  }
  xfs_details="$(xfs_info /srv/legal-mcp)"
  if ! grep -Eq 'reflink=1([[:space:]]|$)' <<< "$xfs_details" \
    || ! grep -Eq 'ftype=1([[:space:]]|$)' <<< "$xfs_details"; then
    echo 'mounted corpus volume lacks required XFS features' >&2
    return 1
  fi
  bootstrap_require_regular /srv/legal-mcp/.legal-mcp-volume root root 444
  bootstrap_require_acl /srv/legal-mcp/.legal-mcp-volume $'user::r--\ngroup::r--\nother::r--'
  mapfile -t volume_marker < /srv/legal-mcp/.legal-mcp-volume
  [[ ${#volume_marker[@]} -eq 2 && "${volume_marker[0]}" = LEGAL_MCP_VOLUME_V1 \
    && "${volume_marker[1]}" =~ ^UUID=([0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12})$ ]] || {
    echo 'mounted corpus volume marker is malformed' >&2
    return 1
  }
  volume_uuid="${BASH_REMATCH[1],,}"
  actual_uuid="$(blkid -s UUID -o value "$source" | tr '[:upper:]' '[:lower:]')"
  [[ "$host_uuid" = "$volume_uuid" && "$volume_uuid" = "$actual_uuid" ]] || {
    echo 'host and mounted volume identities do not match' >&2
    return 1
  }

  [[ -d /srv/legal-mcp && ! -L /srv/legal-mcp \
    && "$(stat -c '%U:%G:%a' /srv/legal-mcp)" = root:legal-mcp:750 ]] || return 1
  bootstrap_require_acl /srv/legal-mcp $'user::rwx\nuser:973:--x\ngroup::r-x\nmask::r-x\nother::---'
  for directory in generations lifecycle state uploads; do
    [[ -d "/srv/legal-mcp/$directory" && ! -L "/srv/legal-mcp/$directory" ]] || {
      echo "bootstrap host directory is missing or unsafe: $directory" >&2
      return 1
    }
  done
  [[ "$(stat -c '%U:%G:%a' /srv/legal-mcp/generations)" = root:legal-mcp:750 ]]
  bootstrap_require_acl /srv/legal-mcp/generations $'user::rwx\ngroup::r-x\nother::---'
  [[ "$(stat -c '%U:%G:%a' /srv/legal-mcp/lifecycle)" = root:legal-mcp:750 ]]
  bootstrap_require_acl /srv/legal-mcp/lifecycle $'user::rwx\ngroup::r-x\nother::---'
  [[ "$(stat -c '%U:%G:%a' /srv/legal-mcp/state)" = legal-mcp:legal-mcp:700 ]]
  bootstrap_require_acl /srv/legal-mcp/state $'user::rwx\ngroup::---\nother::---'
  [[ "$(stat -c '%U:%G:%a' /srv/legal-mcp/uploads)" = legal-mcp-publisher:legal-mcp-publisher:700 ]]
  bootstrap_require_acl /srv/legal-mcp/uploads $'user::rwx\ngroup::---\nother::---'
  bootstrap_require_regular /srv/legal-mcp/lifecycle/LOCK root legal-mcp 640
  bootstrap_require_acl /srv/legal-mcp/lifecycle/LOCK $'user::rw-\ngroup::r--\nother::---'
  bootstrap_require_empty_regular /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK root root 640
  bootstrap_require_acl /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK $'user::rw-\ngroup::r--\nother::---'

  [[ "$(id -u legal-mcp):$(id -g legal-mcp):$(id -G legal-mcp)" = 971:971:971 \
    && "$(id -u legal-mcp-publisher):$(id -g legal-mcp-publisher):$(id -G legal-mcp-publisher)" = 973:973:973 \
    && "$(id -u legal-mcp-admin):$(id -g legal-mcp-admin):$(id -G legal-mcp-admin)" = 974:974:974 ]] || {
    echo 'fixed host identities do not match the installed contract' >&2
    return 1
  }
  [[ -d /run/legal-mcp && ! -L /run/legal-mcp \
    && "$(stat -c '%U:%G:%a' /run/legal-mcp)" = root:legal-mcp-publisher:710 ]] || return 1
  bootstrap_require_regular "$LOCK_FILE" root legal-mcp-publisher 640
  bootstrap_require_regular "$IMAGE_FILE" root root 600
  bootstrap_require_regular "$QUADLET" root root 644
  bootstrap_require_regular "$TEMPLATE" root root 644
  bootstrap_require_regular "$RUNTIME_ENV" root root 600
  bootstrap_require_regular /etc/legal-mcp/api-keys.json legal-mcp legal-mcp 400
  bootstrap_require_regular /etc/caddy/Caddyfile root caddy 640
  bootstrap_require_exact_directory /etc/legal-mcp root root 755
  bootstrap_require_acl /etc/legal-mcp $'user::rwx\ngroup::r-x\nother::r-x'
  bootstrap_require_safe_directory /etc/containers/systemd root root
  bootstrap_require_safe_directory /usr/local/libexec/legal-mcp root root
  bootstrap_require_safe_directory /usr/local/sbin root root
  bootstrap_require_safe_directory /etc/sudoers.d root root
  bootstrap_require_safe_directory /etc/caddy root root
  bootstrap_require_safe_directory /var/lib/legal-mcp-publisher/.ssh root legal-mcp-publisher
  bootstrap_require_regular /var/lib/legal-mcp-publisher/.ssh/authorized_keys root legal-mcp-publisher 640
  publisher_key_re='^restrict,command="/usr/local/sbin/legal-mcp-publisher-command"[[:space:]]ssh-(ed25519|rsa)[[:space:]][A-Za-z0-9+/=]+([[:space:]][^[:cntrl:]]+)?$'
  mapfile -t entries < /var/lib/legal-mcp-publisher/.ssh/authorized_keys
  [[ ${#entries[@]} -eq 1 \
    && "${entries[0]}" =~ $publisher_key_re ]] || {
    echo 'installed publisher key is not bound to the exact forced command' >&2
    return 1
  }
  bootstrap_validate_host_tools

  mapfile -t entries < <(awk -F= '$1 == "LEGAL_MCP_HTTP_AUTH" {print $2}' "$RUNTIME_ENV")
  [[ ${#entries[@]} -eq 1 ]]
  auth_mode="${entries[0]}"
  mapfile -t entries < <(awk -F= '$1 == "LEGAL_MCP_EXTERNAL_URL" {print $2}' "$RUNTIME_ENV")
  [[ ${#entries[@]} -eq 1 ]]
  external_url="${entries[0]}"
  [[ "$auth_mode" = disabled && "$external_url" =~ ^https://[a-z0-9.-]+/mcp$ ]] || {
    echo 'empty-host image cutover requires disabled authentication' >&2
    return 1
  }
  python3 - /etc/legal-mcp/api-keys.json <<'PY'
import json, pathlib, stat, sys
path = pathlib.Path(sys.argv[1])
meta = path.lstat()
if path.is_symlink() or not stat.S_ISREG(meta.st_mode) or meta.st_nlink != 1:
    raise SystemExit(1)
if json.loads(path.read_bytes()) != {"keys": [], "version": 1}:
    raise SystemExit(1)
PY

  bootstrap_path_is_absent /srv/legal-mcp/lifecycle/active-generation || {
    echo 'empty-host image cutover refuses an active generation' >&2
    return 1
  }
  bootstrap_path_is_absent /srv/legal-mcp/lifecycle/.deployment-transaction || {
    echo 'empty-host image cutover requires the corpus transaction to be explicitly aborted' >&2
    return 1
  }
  bootstrap_path_is_absent /run/legal-mcp/authorized-upload || {
    echo 'empty-host image cutover refuses residual upload authorization' >&2
    return 1
  }
  if ! bootstrap_directory_is_empty /srv/legal-mcp/generations \
    || ! bootstrap_directory_is_empty /srv/legal-mcp/uploads \
    || ! bootstrap_directory_is_empty /srv/legal-mcp/state; then
    echo 'empty-host image cutover refuses corpus, upload, or runtime state' >&2
    return 1
  fi
  bootstrap_directory_contains_only /srv/legal-mcp/lifecycle LIFECYCLE_LOCK LOCK || {
    echo 'empty-host lifecycle contains unexpected state' >&2
    return 1
  }
  if ! bootstrap_path_is_absent /etc/legal-mcp/.auth-transaction \
    || ! bootstrap_path_is_absent /etc/legal-mcp/.image-transaction.flat-int8-preparing \
    || ! bootstrap_path_is_absent /etc/legal-mcp/.image-transaction.flat-int8-preparing-retired \
    || ! bootstrap_path_is_absent /etc/legal-mcp/.host-tools-transaction.preparing \
    || ! bootstrap_path_is_absent /etc/legal-mcp/.host-tools-transaction \
    || ! bootstrap_path_is_absent /etc/legal-mcp/.host-tools-transaction.retiring \
    || ! bootstrap_path_is_absent /etc/legal-mcp/.host-tools-transaction.rollback-retiring \
    || ! bootstrap_path_is_absent /etc/legal-mcp/.host-tools-transaction.rollback-retired \
    || ! bootstrap_path_is_absent /etc/legal-mcp/.host-tools-transaction.publisher-restore; then
    echo 'another host transaction must be recovered before empty-host image cutover' >&2
    return 1
  fi
  if [[ "$allow_image_transaction" = false ]]; then
    bootstrap_path_is_absent "$TRANSACTION" || {
      echo 'an image transaction already exists; recover it first' >&2
      return 1
    }
  fi
  bootstrap_services_are_off
}

bootstrap_validate_current_image_files() {
  local rendered image_version image_source image_revision image_state
  local -a current_image
  mapfile -t current_image < "$IMAGE_FILE"
  [[ ${#current_image[@]} -eq 1 \
    && "${current_image[0]}" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ ]] || {
    echo 'currently pinned bootstrap image is malformed' >&2
    return 1
  }
  BOOTSTRAP_OLD_IMAGE="${current_image[0]}"
  [[ "$(grep -o '__IMAGE_DIGEST__' "$TEMPLATE" | wc -l)" = 1 ]] || {
    echo 'installed Quadlet template is malformed' >&2
    return 1
  }
  rendered="$(mktemp /run/legal-mcp-bootstrap-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$BOOTSTRAP_OLD_IMAGE|g" "$TEMPLATE" > "$rendered"
  if ! cmp --silent "$rendered" "$QUADLET"; then
    rm -f "$rendered"
    echo 'installed image, Quadlet, and template do not agree' >&2
    return 1
  fi
  rm -f "$rendered"
  image_state="$(podman_image_state "$BOOTSTRAP_OLD_IMAGE")" || return 1
  [[ "$image_state" = present ]] || {
    echo 'currently pinned bootstrap image is not present' >&2
    return 1
  }
  image_version="$(podman image inspect "$BOOTSTRAP_OLD_IMAGE" --format '{{index .Labels "org.opencontainers.image.version"}}')"
  image_source="$(podman image inspect "$BOOTSTRAP_OLD_IMAGE" --format '{{index .Labels "org.opencontainers.image.source"}}')"
  image_revision="$(podman image inspect "$BOOTSTRAP_OLD_IMAGE" --format '{{index .Labels "org.opencontainers.image.revision"}}')"
  [[ "$image_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ \
    && "$image_source" = https://github.com/gunba/australian-legal-mcp \
    && "$image_revision" =~ ^[0-9a-f]{40}$ ]] || {
    echo 'currently pinned bootstrap image identity is invalid' >&2
    return 1
  }
}

bootstrap_atomic_install() {
  local source="$1" destination="$2" owner="$3" group="$4" mode="$5" temporary
  temporary="$(mktemp "$(dirname "$destination")/.$(basename "$destination").XXXXXX")"
  install -o "$owner" -g "$group" -m "$mode" "$source" "$temporary"
  sync -f "$temporary"
  mv -fT "$temporary" "$destination"
  sync -f "$(dirname "$destination")"
}

bootstrap_create_transaction() {
  local transaction_tmp template_sha256
  transaction_tmp="$TRANSACTION_PREPARING"
  install -d -o root -g root -m 0700 "$transaction_tmp"
  chown root:root "$transaction_tmp"
  chmod 700 "$transaction_tmp"
  install -o root -g root -m 0600 "$IMAGE_FILE" "$transaction_tmp/old-image"
  install -o root -g root -m 0600 "$QUADLET" "$transaction_tmp/old-quadlet"
  install -o root -g root -m 0600 "$TEMPLATE" "$transaction_tmp/old-template"
  template_sha256="$(sha256sum "$BOOTSTRAP_SOURCE_TEMPLATE" | awk '{print $1}')"
  printf '%s\n' LEGAL_MCP_BOOTSTRAP_IMAGE_TRANSACTION_V1 > "$transaction_tmp/kind"
  printf '%s\n' "$NEW_IMAGE" > "$transaction_tmp/target-image"
  printf '%s\n' "$BOOTSTRAP_VERSION" > "$transaction_tmp/target-version"
  printf '%s\n' "$BOOTSTRAP_REVISION" > "$transaction_tmp/target-revision"
  printf '%s\n' "$template_sha256" > "$transaction_tmp/target-template-sha256"
  chmod 600 "$transaction_tmp/kind" \
    "$transaction_tmp/target-image" "$transaction_tmp/target-version" \
    "$transaction_tmp/target-revision" "$transaction_tmp/target-template-sha256"
  sync -f "$transaction_tmp"
  mv -T "$transaction_tmp" "$TRANSACTION"
  sync -f /etc/legal-mcp
}

bootstrap_validate_transaction() {
  local template_sha256 old_rendered old_image
  local -a kind target_image target_version target_revision target_template
  [[ -d "$TRANSACTION" && ! -L "$TRANSACTION" \
    && "$(stat -c '%U:%G:%a' "$TRANSACTION")" = root:root:700 ]] || {
    echo 'bootstrap image transaction is missing or unsafe' >&2
    return 1
  }
  for name in kind target-image target-version target-revision target-template-sha256 \
    old-image old-quadlet old-template; do
    bootstrap_require_regular "$TRANSACTION/$name" root root 600 || return 1
  done
  bootstrap_directory_contains_only "$TRANSACTION" \
    kind old-image old-quadlet old-template target-image target-revision \
    target-template-sha256 target-version || {
    echo 'bootstrap image transaction contains unexpected state' >&2
    return 1
  }
  mapfile -t kind < "$TRANSACTION/kind"
  mapfile -t target_image < "$TRANSACTION/target-image"
  mapfile -t target_version < "$TRANSACTION/target-version"
  mapfile -t target_revision < "$TRANSACTION/target-revision"
  mapfile -t target_template < "$TRANSACTION/target-template-sha256"
  template_sha256="$(sha256sum "$BOOTSTRAP_SOURCE_TEMPLATE" | awk '{print $1}')"
  [[ ${#kind[@]} -eq 1 && "${kind[0]}" = LEGAL_MCP_BOOTSTRAP_IMAGE_TRANSACTION_V1 \
    && ${#target_image[@]} -eq 1 \
    && "${target_image[0]}" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ \
    && ${#target_version[@]} -eq 1 && "${target_version[0]}" = "$BOOTSTRAP_VERSION" \
    && ${#target_revision[@]} -eq 1 && "${target_revision[0]}" = "$BOOTSTRAP_REVISION" \
    && ${#target_template[@]} -eq 1 && "${target_template[0]}" = "$template_sha256" ]] || {
    echo 'bootstrap image transaction identity does not match this release' >&2
    return 1
  }
  mapfile -t target_image < "$TRANSACTION/old-image"
  [[ ${#target_image[@]} -eq 1 \
    && "${target_image[0]}" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ ]] || {
    echo 'saved bootstrap image pin is malformed' >&2
    return 1
  }
  old_image="${target_image[0]}"
  [[ "$(grep -o '__IMAGE_DIGEST__' "$TRANSACTION/old-template" | wc -l)" = 1 ]] || {
    echo 'saved bootstrap Quadlet template is malformed' >&2
    return 1
  }
  old_rendered="$(mktemp /run/legal-mcp-bootstrap-old-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$old_image|g" "$TRANSACTION/old-template" > "$old_rendered"
  if ! cmp --silent "$old_rendered" "$TRANSACTION/old-quadlet"; then
    rm -f "$old_rendered"
    echo 'saved bootstrap image state is internally inconsistent' >&2
    return 1
  fi
  rm -f "$old_rendered"
  BOOTSTRAP_TRANSACTION_OLD_IMAGE="$old_image"
}

bootstrap_force_off() {
  local service_enabled service_activity
  close_ingress || return 1
  systemctl stop "$SERVICE" >/dev/null 2>&1 || return 1
  service_enabled="$(read_systemctl_enablement "$SERVICE")" || return 1
  service_activity="$(read_systemctl_activity "$SERVICE")" || return 1
  [[ "$service_enabled" = generated && "$service_activity" = inactive ]] || {
    echo 'could not prove the generated legal-mcp service inactive' >&2
    return 1
  }
}

bootstrap_recover_transaction() {
  local old_image_state
  bootstrap_force_off
  bootstrap_validate_static_host true
  bootstrap_validate_transaction
  bootstrap_atomic_install "$TRANSACTION/old-image" "$IMAGE_FILE" root root 600
  bootstrap_atomic_install "$TRANSACTION/old-quadlet" "$QUADLET" root root 644
  bootstrap_atomic_install "$TRANSACTION/old-template" "$TEMPLATE" root root 644
  systemctl daemon-reload
  cmp --silent "$TRANSACTION/old-image" "$IMAGE_FILE"
  cmp --silent "$TRANSACTION/old-quadlet" "$QUADLET"
  cmp --silent "$TRANSACTION/old-template" "$TEMPLATE"
  old_image_state="$(podman_image_state "$BOOTSTRAP_TRANSACTION_OLD_IMAGE")" || return 1
  [[ "$old_image_state" = present ]] || {
    echo 'saved bootstrap image is not present' >&2
    return 1
  }
  bootstrap_services_are_off
  retire_image_transaction
}

rollback_bootstrap_image_update() {
  local status=$? recovery_status
  trap - ERR HUP INT TERM EXIT
  set +e
  (
    set -e
    bootstrap_recover_transaction
  )
  recovery_status=$?
  set -e
  if [[ $recovery_status -ne 0 ]]; then
    echo 'empty-host image cutover failed and automatic rollback did not complete' >&2
    exit 1
  fi
  echo 'empty-host image cutover rolled back' >&2
  exit "$status"
}

run_bootstrap_empty_host_update() {
  local rendered image_value
  [[ "$NEW_IMAGE" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ \
    && "$EXPECTED_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || usage
  [[ -n "$SOURCE_TEMPLATE" ]] || usage
  bootstrap_services_are_off
  bootstrap_load_bundle "$SOURCE_TEMPLATE" "$EXPECTED_VERSION"
  bootstrap_validate_static_host false
  bootstrap_validate_current_image_files
  [[ "$NEW_IMAGE" != "$BOOTSTRAP_OLD_IMAGE" ]] || {
    echo 'empty-host image cutover requires a new digest' >&2
    return 1
  }
  podman pull "$NEW_IMAGE"
  verify_image_runtime "$NEW_IMAGE" "$EXPECTED_VERSION" "$BOOTSTRAP_REVISION"
  bootstrap_create_transaction
  trap rollback_bootstrap_image_update ERR HUP INT TERM EXIT

  image_value="$(mktemp /etc/legal-mcp/.bootstrap-image.XXXXXX)"
  printf '%s\n' "$NEW_IMAGE" > "$image_value"
  bootstrap_atomic_install "$image_value" "$IMAGE_FILE" root root 600
  rm -f "$image_value"
  rendered="$(mktemp /etc/containers/systemd/.legal-mcp.container.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$NEW_IMAGE|g" "$BOOTSTRAP_SOURCE_TEMPLATE" > "$rendered"
  bootstrap_atomic_install "$rendered" "$QUADLET" root root 644
  rm -f "$rendered"
  bootstrap_atomic_install "$BOOTSTRAP_SOURCE_TEMPLATE" "$TEMPLATE" root root 644
  systemctl daemon-reload
  bootstrap_validate_static_host true
  bootstrap_validate_current_image_files
  [[ "$BOOTSTRAP_OLD_IMAGE" = "$NEW_IMAGE" ]]
  verify_image_runtime "$NEW_IMAGE" "$EXPECTED_VERSION" "$BOOTSTRAP_REVISION"

  trap - ERR HUP INT TERM EXIT
  retire_image_transaction
  echo "empty bootstrap host pinned to $NEW_IMAGE; service and ingress remain off"
}

ordinary_require_regular() {
  local path="$1" owner="$2" group="$3" mode="$4"
  [[ -f "$path" && ! -L "$path" \
    && "$(stat -c '%U:%G:%a:%h' "$path")" = "$owner:$group:$mode:1" ]] || {
    echo "required host file is missing or unsafe: $path" >&2
    return 1
  }
}

ordinary_render_metadata_manifest() {
  local destination="$1"
  cat > "$destination" <<'EOF'
IMAGE=root:root:600:1
QUADLET=root:root:644:1
TEMPLATE=root:root:644:1
RUNTIME_ENV=root:root:600:1
API_KEYS=legal-mcp:legal-mcp:400:1
CADDYFILE=root:caddy:640:1
AUTH_READY=root:root:444:1:0
ACTIVE_GENERATION=root:root:644:1:64
EOF
}

ordinary_render_hash_manifest() {
  local image="$1" quadlet="$2" template="$3" runtime="$4"
  local api_keys="$5" caddyfile="$6" auth_ready="$7" generation="$8"
  local destination="$9"
  cat > "$destination" <<EOF
IMAGE_SHA256=$(sha256sum "$image" | awk '{print $1}')
QUADLET_SHA256=$(sha256sum "$quadlet" | awk '{print $1}')
TEMPLATE_SHA256=$(sha256sum "$template" | awk '{print $1}')
RUNTIME_ENV_SHA256=$(sha256sum "$runtime" | awk '{print $1}')
API_KEYS_SHA256=$(sha256sum "$api_keys" | awk '{print $1}')
CADDYFILE_SHA256=$(sha256sum "$caddyfile" | awk '{print $1}')
AUTH_READY_SHA256=$(sha256sum "$auth_ready" | awk '{print $1}')
ACTIVE_GENERATION_SHA256=$(sha256sum "$generation" | awk '{print $1}')
EOF
}

ordinary_render_release_manifest() {
  local destination="$1"
  cat > "$destination" <<EOF
UPDATE_IMAGE_SHA256=$(sha256sum "$ORDINARY_RELEASE_UPDATE_IMAGE" | awk '{print $1}')
CONFIGURE_AUTH_SHA256=$(sha256sum "$ORDINARY_RELEASE_CONFIGURE_AUTH" | awk '{print $1}')
HOST_DEPLOY_SHA256=$(sha256sum "$ORDINARY_RELEASE_HOST_DEPLOY" | awk '{print $1}')
PUBLISHER_COMMAND_SHA256=$(sha256sum "$ORDINARY_RELEASE_PUBLISHER" | awk '{print $1}')
CONTAINER_TEMPLATE_SHA256=$(sha256sum "$ORDINARY_SOURCE_TEMPLATE" | awk '{print $1}')
HOST_TOOLS_MARKER_SHA256=$(sha256sum "$HOST_TOOLS_MARKER" | awk '{print $1}')
HOST_TOOL_LAUNCHER_SHA256=$(sha256sum "$HOST_TOOL_LAUNCHER" | awk '{print $1}')
HOST_TOOL_LAUNCHER_MARKER_SHA256=$(sha256sum "$HOST_TOOL_LAUNCHER_MARKER" | awk '{print $1}')
CONFIGURE_AUTH_POINTER_SHA256=$(sha256sum "$CONFIGURE_AUTH_POINTER" | awk '{print $1}')
UPDATE_IMAGE_POINTER_SHA256=$(sha256sum "$UPDATE_IMAGE_POINTER" | awk '{print $1}')
EOF
}

ordinary_load_installed_release_identity() {
  local script="$1" deploy_sha publisher_sha configure_sha update_sha
  local template_sha sudoers_sha expected_policy
  local -a marker
  bootstrap_require_regular "$HOST_TOOLS_MARKER" root root 444
  bootstrap_require_acl "$HOST_TOOLS_MARKER" $'user::r--\ngroup::r--\nother::r--'
  mapfile -t marker < "$HOST_TOOLS_MARKER"
  [[ ${#marker[@]} -eq 9 && "${marker[0]}" = LEGAL_MCP_HOST_TOOLS_V2 \
    && "${marker[1]}" =~ ^VERSION=([0-9]+\.[0-9]+\.[0-9]+)$ ]] || {
      echo 'installed V2 host-tool release identity is malformed' >&2
      return 1
    }
  ORDINARY_VERSION="${BASH_REMATCH[1]}"
  [[ "${marker[2]}" =~ ^SOURCE_COMMIT=([0-9a-f]{40})$ ]]
  ORDINARY_REVISION="${BASH_REMATCH[1]}"
  [[ "${marker[3]}" =~ ^HOST_DEPLOY_SHA256=([0-9a-f]{64})$ ]]
  deploy_sha="${BASH_REMATCH[1]}"
  [[ "${marker[4]}" =~ ^PUBLISHER_COMMAND_SHA256=([0-9a-f]{64})$ ]]
  publisher_sha="${BASH_REMATCH[1]}"
  [[ "${marker[5]}" =~ ^CONFIGURE_AUTH_SHA256=([0-9a-f]{64})$ ]]
  configure_sha="${BASH_REMATCH[1]}"
  [[ "${marker[6]}" =~ ^UPDATE_IMAGE_SHA256=([0-9a-f]{64})$ ]]
  update_sha="${BASH_REMATCH[1]}"
  [[ "${marker[7]}" =~ ^CONTAINER_TEMPLATE_SHA256=([0-9a-f]{64})$ ]]
  template_sha="${BASH_REMATCH[1]}"
  [[ "${marker[8]}" =~ ^SUDOERS_SHA256=([0-9a-f]{64})$ ]]
  sudoers_sha="${BASH_REMATCH[1]}"
  ORDINARY_SOURCE_TEMPLATE="$TEMPLATE"
  ORDINARY_RELEASE_HOST_DEPLOY=/usr/local/sbin/legal-mcp-host-deploy
  ORDINARY_RELEASE_PUBLISHER=/usr/local/sbin/legal-mcp-publisher-command
  ORDINARY_RELEASE_CONFIGURE_AUTH="$HOST_TOOL_IMPLEMENTATION_DIR/configure-auth.$configure_sha"
  ORDINARY_RELEASE_UPDATE_IMAGE="$HOST_TOOL_IMPLEMENTATION_DIR/update-image.$update_sha"
  bootstrap_require_regular "$ORDINARY_RELEASE_HOST_DEPLOY" root root 755
  bootstrap_require_regular "$ORDINARY_RELEASE_PUBLISHER" root root 755
  bootstrap_require_regular "$ORDINARY_SOURCE_TEMPLATE" root root 644
  bootstrap_require_regular /etc/sudoers.d/legal-mcp-publisher root root 440
  [[ "$(sha256sum "$ORDINARY_RELEASE_HOST_DEPLOY" | awk '{print $1}')" = "$deploy_sha" \
    && "$(sha256sum "$ORDINARY_RELEASE_PUBLISHER" | awk '{print $1}')" = "$publisher_sha" \
    && "$(sha256sum "$ORDINARY_SOURCE_TEMPLATE" | awk '{print $1}')" = "$template_sha" \
    && "$(sha256sum /etc/sudoers.d/legal-mcp-publisher | awk '{print $1}')" = "$sudoers_sha" \
    && "$(sha256sum "$script" | awk '{print $1}')" = "$update_sha" ]] || {
      echo 'installed V2 release bytes do not match their marker' >&2
      return 1
    }
  validate_installed_host_tool_launchers "$configure_sha" "$update_sha" internal
  expected_policy="$(mktemp /run/legal-mcp-image-installed-sudoers.XXXXXX)"
  printf '%s\n' \
    'Defaults:legal-mcp-publisher !requiretty' \
    "legal-mcp-publisher ALL=(root) NOPASSWD: sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^prepare [0-9a-f]{64}$, sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^activate [0-9a-f]{64}$, sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^abort [0-9a-f]{64}$" \
    > "$expected_policy"
  if ! cmp --silent "$expected_policy" /etc/sudoers.d/legal-mcp-publisher; then
    rm -f "$expected_policy"
    echo 'installed publisher sudo policy is not exact' >&2
    return 1
  fi
  rm -f "$expected_policy"
  visudo -cf /etc/sudoers.d/legal-mcp-publisher >/dev/null
}

ordinary_load_release_bundle() {
  local requested_template="$1" requested_version="$2" script resolved_template
  local entrypoint_state
  local -a versions revisions
  [[ ! -L "${BASH_SOURCE[0]}" ]] || {
    echo 'image update must run from a real version-matched release implementation' >&2
    return 1
  }
  script="$(readlink -f "${BASH_SOURCE[0]}")"
  bootstrap_require_release_file "$script" true
  if [[ -n "$requested_template" ]]; then
    bootstrap_require_release_file "$requested_template"
    resolved_template="$(readlink -f "$requested_template")"
    ORDINARY_BUNDLE_ROOT="$(cd "$(dirname "$resolved_template")/../.." && pwd -P)"
    [[ "$resolved_template" = "$ORDINARY_BUNDLE_ROOT/infra/hosting/legal-mcp.container.template" ]] || {
      echo 'Quadlet template is not in a complete Linux release bundle' >&2
      return 1
    }
  else
    ORDINARY_BUNDLE_ROOT="$(cd "$(dirname "$script")/../.." && pwd -P)"
    if [[ "$script" != "$ORDINARY_BUNDLE_ROOT/infra/hosting/update-image.sh" ]]; then
      ordinary_load_installed_release_identity "$script"
      [[ "$(grep -o '__IMAGE_DIGEST__' "$ORDINARY_SOURCE_TEMPLATE" | wc -l)" = 1 \
        && "$(grep -Fxc 'ExecCondition=/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready' \
          "$ORDINARY_SOURCE_TEMPLATE")" = 1 ]] || {
          echo 'installed release template lacks its exact image or auth-ready gate' >&2
          return 1
        }
      ORDINARY_UPDATER_SHA256="$(sha256sum "$script" | awk '{print $1}')"
      return 0
    fi
  fi
  ORDINARY_SOURCE_TEMPLATE="$ORDINARY_BUNDLE_ROOT/infra/hosting/legal-mcp.container.template"
  for path in \
    "$ORDINARY_BUNDLE_ROOT/Containerfile" \
    "$ORDINARY_BUNDLE_ROOT/SOURCE_COMMIT" \
    "$ORDINARY_BUNDLE_ROOT/libonnxruntime.so" \
    "$ORDINARY_SOURCE_TEMPLATE"; do
    bootstrap_require_release_file "$path"
  done
  bootstrap_require_release_file "$ORDINARY_BUNDLE_ROOT/legal-mcp" true
  mapfile -t versions < <(awk -F= '$1 == "ARG VERSION" {print $2}' "$ORDINARY_BUNDLE_ROOT/Containerfile")
  mapfile -t revisions < "$ORDINARY_BUNDLE_ROOT/SOURCE_COMMIT"
  [[ ${#versions[@]} -eq 1 && "${versions[0]}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ \
    && ${#revisions[@]} -eq 1 && "${revisions[0]}" =~ ^[0-9a-f]{40}$ ]] || {
    echo 'release bundle version or source revision is malformed' >&2
    return 1
  }
  ORDINARY_VERSION="${versions[0]}"
  ORDINARY_REVISION="${revisions[0]}"
  if [[ -n "$requested_version" && "$requested_version" != "$ORDINARY_VERSION" ]]; then
    echo 'release bundle version does not match the request' >&2
    return 1
  fi
  if [[ -n "$requested_template" ]]; then
    [[ "$resolved_template" = "$ORDINARY_SOURCE_TEMPLATE" ]] || {
      echo 'Quadlet template is not the exact template in this release bundle' >&2
      return 1
    }
  fi
  [[ "$(grep -o '__IMAGE_DIGEST__' "$ORDINARY_SOURCE_TEMPLATE" | wc -l)" = 1 \
    && "$(grep -Fxc 'ExecCondition=/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready' \
      "$ORDINARY_SOURCE_TEMPLATE")" = 1 ]] || {
    echo 'version-matched Quadlet template lacks its exact image or auth-ready gate' >&2
    return 1
  }
  if [[ "$script" = "$ORDINARY_BUNDLE_ROOT/infra/hosting/update-image.sh" ]]; then
    entrypoint_state=outer
  else
    entrypoint_state=internal
  fi
  validate_v2_host_tool_release \
    "$ORDINARY_BUNDLE_ROOT" "$ORDINARY_VERSION" "$ORDINARY_REVISION" \
    "$ORDINARY_SOURCE_TEMPLATE" "$entrypoint_state"
  ORDINARY_RELEASE_HOST_DEPLOY="$ORDINARY_BUNDLE_ROOT/scripts/legal-mcp-host-deploy"
  ORDINARY_RELEASE_PUBLISHER="$ORDINARY_BUNDLE_ROOT/scripts/legal-mcp-publisher-command"
  ORDINARY_RELEASE_CONFIGURE_AUTH="$ORDINARY_BUNDLE_ROOT/infra/hosting/configure-auth.sh"
  ORDINARY_RELEASE_UPDATE_IMAGE="$ORDINARY_BUNDLE_ROOT/infra/hosting/update-image.sh"
  ORDINARY_UPDATER_SHA256="$(sha256sum "$script" | awk '{print $1}')"
}

ordinary_require_static_live_metadata() {
  ordinary_require_regular "$IMAGE_FILE" root root 600
  ordinary_require_regular "$QUADLET" root root 644
  ordinary_require_regular "$TEMPLATE" root root 644
  ordinary_require_regular "$RUNTIME_ENV" root root 600
  ordinary_require_regular "$API_KEYS" legal-mcp legal-mcp 400
  ordinary_require_regular "$CADDYFILE" root caddy 640
  ordinary_require_regular /srv/legal-mcp/lifecycle/active-generation root root 644
  [[ "$(stat -c '%s' /srv/legal-mcp/lifecycle/active-generation)" = 64 ]] || {
    echo 'active generation pointer must contain exactly 64 bytes' >&2
    return 1
  }
}

ordinary_require_live_metadata() {
  ordinary_require_static_live_metadata
  ordinary_require_regular "$AUTH_READY" root root 444
  [[ "$(stat -c '%s' "$AUTH_READY")" = 0 \
    && "$(getfacl --absolute-names --numeric --omit-header "$AUTH_READY")" \
      = $'user::r--\ngroup::r--\nother::r--' ]] || {
      echo 'authentication-ready marker is not exact' >&2
      return 1
    }
}

ordinary_validate_caddy_contract() {
  local host="$1" adapted
  ordinary_require_regular "$CADDYFILE" root caddy 640 || return 1
  adapted="$(mktemp /run/legal-mcp-image-caddy-adapted.XXXXXX)"
  if ! caddy adapt --config "$CADDYFILE" --adapter caddyfile --validate > "$adapted"; then
    rm -f "$adapted"
    echo 'Caddyfile validation/adaptation failed' >&2
    return 1
  fi
  if ! python3 - "$adapted" "$host" <<'PY'
import json, sys
path, host = sys.argv[1:]
with open(path, encoding="utf-8") as handle:
    actual = json.load(handle)
timeouts = {
    "read_timeout": 30_000_000_000,
    "read_header_timeout": 10_000_000_000,
    "write_timeout": 300_000_000_000,
    "idle_timeout": 300_000_000_000,
}
https_routes = [
    {"handle": [{"encodings": {"gzip": {}, "zstd": {}}, "handler": "encode", "prefer": ["zstd", "gzip"]}]},
    {
        "group": "group2",
        "handle": [{"handler": "subroute", "routes": [{"handle": [
            {"handler": "headers", "response": {"deferred": True, "delete": ["Server"], "set": {
                "Cache-Control": ["no-store"],
                "Strict-Transport-Security": ["max-age=31536000"],
                "X-Content-Type-Options": ["nosniff"],
            }}},
            {"handler": "request_body", "max_size": 1_000_000},
            {"flush_interval": -1, "handler": "reverse_proxy", "transport": {
                "dial_timeout": 5_000_000_000,
                "max_conns_per_host": 8,
                "protocol": "http",
                "read_timeout": 310_000_000_000,
                "response_header_timeout": 310_000_000_000,
                "write_timeout": 310_000_000_000,
            }, "upstreams": [{"dial": "127.0.0.1:51235"}]},
        ]}]}],
        "match": [{"path": ["/mcp", "/.well-known/oauth-protected-resource/mcp"]}],
    },
    {"group": "group2", "handle": [{"handler": "subroute", "routes": [{"handle": [
        {"body": "not found", "handler": "static_response", "status_code": 404}
    ]}]}]},
]
logging = {"logs": {"default": {"encoder": {
    "fields": {"request": {"filter": "delete"}},
    "format": "filter",
    "wrap": {"format": "json"},
}}}}
expected = {"apps": {"http": {"servers": {
    "srv0": {
        "listen": [":443"], **timeouts,
        "routes": [{"match": [{"host": [host]}], "handle": [{"handler": "subroute", "routes": https_routes}], "terminal": True}],
    },
    "srv1": {
        "listen": [":80"], **timeouts,
        "routes": [{"match": [{"host": [host]}], "handle": [{"handler": "subroute", "routes": [{"handle": [
            {"body": "not found", "handler": "static_response", "status_code": 404}
        ]}]}], "terminal": True}],
    },
}}}, "logging": logging}
if actual != expected:
    raise SystemExit(1)
PY
  then
    rm -f "$adapted"
    echo 'adapted Caddy routes do not match the exact MCP-only contract' >&2
    return 1
  fi
  rm -f "$adapted"
}

ordinary_require_listener_topology() {
  local expected="$1" listeners
  listeners="$(ss --listening --tcp --numeric --no-header)" || {
    echo 'could not inspect listening TCP sockets' >&2
    return 1
  }
  printf '%s\n' "$listeners" | python3 /dev/fd/3 "$expected" 3<<'PY'
import re, sys
expected = sys.argv[1]
entries = []
for line in sys.stdin.read().splitlines():
    if not line.strip():
        continue
    fields = line.split()
    if len(fields) < 4:
        raise SystemExit(1)
    match = re.fullmatch(r"(.+):([0-9]+)", fields[3])
    if not match:
        raise SystemExit(1)
    address, port = match.group(1), int(match.group(2))
    if address.startswith("[") and address.endswith("]"):
        address = address[1:-1]
    if port in (80, 443, 51235):
        entries.append((address, port))
if len(entries) != len(set(entries)):
    raise SystemExit(1)
service = [(address, port) for address, port in entries if port == 51235]
web = [(address, port) for address, port in entries if port in (80, 443)]
if expected == "none":
    if entries:
        raise SystemExit(1)
elif expected == "private":
    if service != [("127.0.0.1", 51235)] or web:
        raise SystemExit(1)
elif expected == "public":
    if service != [("127.0.0.1", 51235)]:
        raise SystemExit(1)
    if {port for _, port in web} != {80, 443}:
        raise SystemExit(1)
    if any(address not in {"*", "0.0.0.0", "::"} for address, _ in web):
        raise SystemExit(1)
    if sum(port == 80 for _, port in web) not in (1, 2) or sum(port == 443 for _, port in web) not in (1, 2):
        raise SystemExit(1)
else:
    raise SystemExit(1)
PY
}

ordinary_probe_negative_caddy_routes() {
  local origin="$1" host path method status headers
  host="${origin#https://}"
  for path in / /mcp/ /.well-known/oauth-protected-resource \
    /.well-known/oauth-protected-resource/mcp/ /readyz /livez; do
    method=GET
    [[ "$path" = /mcp/ ]] && method=POST
    headers="$(mktemp /run/legal-mcp-image-negative-headers.XXXXXX)"
    status="$(curl --silent --show-error --dump-header "$headers" --output /dev/null \
      --write-out '%{http_code}' --max-time 20 --max-redirs 0 --request "$method" \
      --resolve "$host:443:127.0.0.1" "$origin$path" 2>/dev/null || true)"
    if [[ "$status" != 404 ]] || grep -Eiq '^Location:' "$headers"; then
      rm -f "$headers"
      echo "unexpected public route or redirect: $path" >&2
      return 1
    fi
    rm -f "$headers"
  done
  headers="$(mktemp /run/legal-mcp-image-negative-headers.XXXXXX)"
  status="$(curl --silent --show-error --dump-header "$headers" --output /dev/null \
    --write-out '%{http_code}' --max-time 20 --max-redirs 0 \
    --resolve "$host:80:127.0.0.1" "http://$host/mcp" 2>/dev/null || true)"
  if [[ "$status" != 404 ]] || grep -Eiq '^Location:' "$headers"; then
    rm -f "$headers"
    echo 'HTTP MCP path must be an exact non-redirecting 404' >&2
    return 1
  fi
  rm -f "$headers"
}

ordinary_require_no_foreign_transaction() {
  local path auth_preparation
  for path in \
    /srv/legal-mcp/lifecycle/.deployment-transaction \
    /srv/legal-mcp/lifecycle/.deployment-transaction.preparing \
    /etc/legal-mcp/.auth-transaction \
    /etc/legal-mcp/.auth-transaction.preparing \
    /etc/legal-mcp/.auth-transaction.preparing-retired \
    /etc/legal-mcp/.auth-transaction.retiring \
    /etc/legal-mcp/.auth-transaction.retired \
    /etc/legal-mcp/.image-transaction.flat-int8-preparing \
    /etc/legal-mcp/.image-transaction.flat-int8-preparing-retired \
    /etc/legal-mcp/.host-tools-transaction.building \
    /etc/legal-mcp/.host-tools-transaction.building-retired \
    /etc/legal-mcp/.host-tools-transaction.preparing \
    /etc/legal-mcp/.host-tools-transaction.preparing-retired \
    /etc/legal-mcp/.host-tools-transaction \
    /etc/legal-mcp/.host-tools-transaction.retiring \
    /etc/legal-mcp/.host-tools-transaction.retired \
    /etc/legal-mcp/.host-tools-transaction.rollback-retiring \
    /etc/legal-mcp/.host-tools-transaction.rollback-retired \
    /etc/legal-mcp/.host-tools-transaction.publisher-restore \
    /etc/legal-mcp/.host-tools-transaction.publisher-restore-retired; do
    image_path_is_absent "$path" || {
      echo 'a foreign host transaction must be recovered first' >&2
      return 1
    }
  done
  auth_preparation="$(find /etc/legal-mcp -mindepth 1 -maxdepth 1 \
    -name '.auth-transaction.preparing.*' -print -quit)" || {
      echo 'could not inspect authentication transaction preparations' >&2
      return 1
    }
  [[ -z "$auth_preparation" ]] || {
    echo 'an authentication transaction preparation must be recovered first' >&2
    return 1
  }
}

ordinary_validate_transaction() {
  local directory="$1" rendered manifest metadata release_manifest name
  local -a kind version revision updater outcome state old_image target_image generation
  require_image_transaction_directory "$directory"
  for name in kind target-version target-revision updater-sha256 retirement-outcome release-sha256 \
    saved-sha256 target-sha256 saved-metadata target-metadata state \
    saved-image saved-quadlet saved-template saved-runtime.env \
    saved-api-keys.json saved-Caddyfile saved-auth-ready saved-active-generation \
    target-image target-quadlet target-template; do
    ordinary_require_regular "$directory/$name" root root 600 || return 1
  done
  bootstrap_directory_contains_only "$directory" \
    kind target-version target-revision updater-sha256 retirement-outcome release-sha256 \
    saved-sha256 target-sha256 saved-metadata target-metadata state \
    saved-image saved-quadlet saved-template saved-runtime.env \
    saved-api-keys.json saved-Caddyfile saved-auth-ready saved-active-generation \
    target-image target-quadlet target-template || {
      echo 'image transaction contains unexpected durable state' >&2
      return 1
    }
  mapfile -t kind < "$directory/kind"
  mapfile -t version < "$directory/target-version"
  mapfile -t revision < "$directory/target-revision"
  mapfile -t updater < "$directory/updater-sha256"
  mapfile -t outcome < "$directory/retirement-outcome"
  [[ ${#kind[@]} -eq 1 && "${kind[0]}" = LEGAL_MCP_IMAGE_TRANSACTION_V2 \
    && ${#version[@]} -eq 1 && "${version[0]}" = "$ORDINARY_VERSION" \
    && ${#revision[@]} -eq 1 && "${revision[0]}" = "$ORDINARY_REVISION" \
    && ${#updater[@]} -eq 1 && "${updater[0]}" = "$ORDINARY_UPDATER_SHA256" \
    && ${#outcome[@]} -eq 1 && "${outcome[0]}" =~ ^(pending|saved|target)$ ]] || {
    echo 'image transaction does not belong to this exact release updater' >&2
    return 1
  }
  TRANSACTION_RETIREMENT_OUTCOME="${outcome[0]}"
  release_manifest="$(mktemp /run/legal-mcp-image-release.XXXXXX)"
  ordinary_render_release_manifest "$release_manifest"
  if ! cmp --silent "$release_manifest" "$directory/release-sha256"; then
    rm -f "$release_manifest"
    echo 'image transaction release bundle hash manifest does not match' >&2
    return 1
  fi
  rm -f "$release_manifest"
  metadata="$(mktemp /run/legal-mcp-image-metadata.XXXXXX)"
  ordinary_render_metadata_manifest "$metadata"
  if ! cmp --silent "$metadata" "$directory/saved-metadata" \
    || ! cmp --silent "$metadata" "$directory/target-metadata"; then
    rm -f "$metadata"
    echo 'image transaction host metadata manifest is not exact' >&2
    return 1
  fi
  rm -f "$metadata"
  manifest="$(mktemp /run/legal-mcp-image-hashes.XXXXXX)"
  ordinary_render_hash_manifest \
    "$directory/saved-image" "$directory/saved-quadlet" \
    "$directory/saved-template" "$directory/saved-runtime.env" \
    "$directory/saved-api-keys.json" "$directory/saved-Caddyfile" \
    "$directory/saved-auth-ready" "$directory/saved-active-generation" "$manifest"
  if ! cmp --silent "$manifest" "$directory/saved-sha256"; then
    rm -f "$manifest"
    echo 'saved image transaction bytes do not match their hash manifest' >&2
    return 1
  fi
  ordinary_render_hash_manifest \
    "$directory/target-image" "$directory/target-quadlet" \
    "$directory/target-template" "$directory/saved-runtime.env" \
    "$directory/saved-api-keys.json" "$directory/saved-Caddyfile" \
    "$directory/saved-auth-ready" "$directory/saved-active-generation" "$manifest"
  if ! cmp --silent "$manifest" "$directory/target-sha256"; then
    rm -f "$manifest"
    echo 'target image transaction bytes do not match their hash manifest' >&2
    return 1
  fi
  rm -f "$manifest"

  mapfile -t state < "$directory/state"
  [[ ${#state[@]} -eq 13 \
    && "${state[0]}" = SERVICE_ENABLEMENT=generated \
    && "${state[1]}" = SERVICE_ACTIVITY=active \
    && "${state[2]}" = CADDY_ENABLEMENT=enabled \
    && "${state[3]}" = CADDY_ACTIVITY=active \
    && "${state[4]}" = UFW_80=present \
    && "${state[5]}" = UFW_443=present \
    && "${state[6]}" =~ ^AUTH_MODE=(api-key|entra|entra\+api-key)$ \
    && "${state[7]}" =~ ^EXTERNAL_URL=https://[a-z0-9.-]+/mcp$ \
    && "${state[8]}" =~ ^EXPECTED_GENERATION=([0-9a-f]{64})$ ]] || {
    echo 'image transaction service/authentication state is malformed' >&2
    return 1
  }
  TRANSACTION_CADDY_ENABLEMENT="${state[2]#*=}"
  TRANSACTION_CADDY_ACTIVITY="${state[3]#*=}"
  TRANSACTION_UFW_80="${state[4]#*=}"
  TRANSACTION_UFW_443="${state[5]#*=}"
  TRANSACTION_AUTH_MODE="${state[6]#*=}"
  TRANSACTION_EXTERNAL_URL="${state[7]#*=}"
  TRANSACTION_GENERATION="${state[8]#*=}"
  [[ "${state[9]}" =~ ^OLD_IMAGE=(ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64})$ ]]
  TRANSACTION_OLD_IMAGE="${state[9]#*=}"
  [[ "${state[10]}" =~ ^OLD_IMAGE_ID=(sha256:[0-9a-f]{64})$ ]]
  TRANSACTION_OLD_IMAGE_ID="${state[10]#*=}"
  [[ "${state[11]}" =~ ^TARGET_IMAGE=(ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64})$ ]]
  TRANSACTION_TARGET_IMAGE="${state[11]#*=}"
  [[ "${state[12]}" =~ ^TARGET_IMAGE_ID=(sha256:[0-9a-f]{64})$ ]]
  TRANSACTION_TARGET_IMAGE_ID="${state[12]#*=}"
  [[ "$TRANSACTION_OLD_IMAGE" != "$TRANSACTION_TARGET_IMAGE" \
    && "$TRANSACTION_UFW_80" = "$TRANSACTION_UFW_443" ]] || {
    echo 'image transaction image or UFW state is inconsistent' >&2
    return 1
  }
  if [[ "$TRANSACTION_UFW_80" = present ]]; then
    [[ "$TRANSACTION_CADDY_ENABLEMENT" = enabled \
      && "$TRANSACTION_CADDY_ACTIVITY" = active ]] || {
        echo 'image transaction records public ingress without active enabled Caddy' >&2
        return 1
      }
  fi
  mapfile -t old_image < "$directory/saved-image"
  mapfile -t target_image < "$directory/target-image"
  [[ ${#old_image[@]} -eq 1 && "${old_image[0]}" = "$TRANSACTION_OLD_IMAGE" \
    && "$(stat -c '%s' "$directory/saved-image")" \
      = "$(( ${#TRANSACTION_OLD_IMAGE} + 1 ))" \
    && ${#target_image[@]} -eq 1 && "${target_image[0]}" = "$TRANSACTION_TARGET_IMAGE" \
    && "$(stat -c '%s' "$directory/target-image")" \
      = "$(( ${#TRANSACTION_TARGET_IMAGE} + 1 ))" ]] || {
    echo 'image transaction pins do not match their state record' >&2
    return 1
  }
  [[ "$(stat -c '%s' "$directory/saved-active-generation")" = 64 ]]
  mapfile -t generation < "$directory/saved-active-generation"
  [[ ${#generation[@]} -eq 1 && "${generation[0]}" = "$TRANSACTION_GENERATION" ]]
  load_runtime_contract "$directory/saved-runtime.env"
  [[ "$AUTH_MODE" = "$TRANSACTION_AUTH_MODE" \
    && "$EXTERNAL_URL" = "$TRANSACTION_EXTERNAL_URL" ]] || {
      echo 'saved runtime authentication metadata does not match the transaction' >&2
      return 1
    }
  [[ "$(grep -o '__IMAGE_DIGEST__' "$directory/saved-template" | wc -l)" = 1 ]]
  rendered="$(mktemp /run/legal-mcp-image-saved-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$TRANSACTION_OLD_IMAGE|g" \
    "$directory/saved-template" > "$rendered"
  if ! cmp --silent "$rendered" "$directory/saved-quadlet"; then
    rm -f "$rendered"
    echo 'saved image pin, template, and rendered Quadlet are inconsistent' >&2
    return 1
  fi
  sed "s|__IMAGE_DIGEST__|$TRANSACTION_TARGET_IMAGE|g" \
    "$directory/target-template" > "$rendered"
  if ! cmp --silent "$rendered" "$directory/target-quadlet" \
    || ! cmp --silent "$directory/target-template" "$ORDINARY_SOURCE_TEMPLATE"; then
    rm -f "$rendered"
    echo 'target image pin, release template, and rendered Quadlet are inconsistent' >&2
    return 1
  fi
  rm -f "$rendered"
}

ordinary_current_file_matches() {
  local live="$1" saved="$2" target="$3"
  cmp --silent "$live" "$saved" || cmp --silent "$live" "$target"
}

ordinary_validate_recoverable_live_state() {
  local activity container_state running_image_id host
  ordinary_require_live_metadata
  host="${TRANSACTION_EXTERNAL_URL#https://}"
  host="${host%/mcp}"
  ordinary_validate_caddy_contract "$host"
  ordinary_current_file_matches "$IMAGE_FILE" \
    "$TRANSACTION/saved-image" "$TRANSACTION/target-image" || return 1
  ordinary_current_file_matches "$QUADLET" \
    "$TRANSACTION/saved-quadlet" "$TRANSACTION/target-quadlet" || return 1
  ordinary_current_file_matches "$TEMPLATE" \
    "$TRANSACTION/saved-template" "$TRANSACTION/target-template" || return 1
  cmp --silent "$RUNTIME_ENV" "$TRANSACTION/saved-runtime.env"
  cmp --silent "$API_KEYS" "$TRANSACTION/saved-api-keys.json"
  cmp --silent "$CADDYFILE" "$TRANSACTION/saved-Caddyfile"
  cmp --silent "$AUTH_READY" "$TRANSACTION/saved-auth-ready"
  cmp --silent /srv/legal-mcp/lifecycle/active-generation \
    "$TRANSACTION/saved-active-generation"
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated ]]
  activity="$(read_systemctl_activity "$SERVICE")" || return 1
  [[ "$activity" = active || "$activity" = inactive ]]
  if [[ "$activity" = active ]]; then
    ordinary_require_listener_topology private
  else
    ordinary_require_listener_topology none
  fi
  container_state="$(podman_container_state australian-legal-mcp)" || return 1
  if [[ "$container_state" = present ]]; then
    running_image_id="$(canonical_image_id \
      "$(podman inspect australian-legal-mcp --format '{{.Image}}')")" || return 1
    [[ "$running_image_id" = "$TRANSACTION_OLD_IMAGE_ID" \
      || "$running_image_id" = "$TRANSACTION_TARGET_IMAGE_ID" ]] || {
        echo 'recovery found a running container outside the saved/target image set' >&2
        return 1
      }
  fi
  ufw_is_fail_closed || {
    echo 'recovery requires the exact fail-closed UFW allowlist' >&2
    return 1
  }
  [[ "$(ufw_rule_state 80)" = absent && "$(ufw_rule_state 443)" = absent ]]
}

ordinary_atomic_install() {
  local source="$1" destination="$2" owner="$3" group="$4" mode="$5" temporary
  temporary="$(mktemp "$(dirname "$destination")/.$(basename "$destination").XXXXXX")"
  install -o "$owner" -g "$group" -m "$mode" "$source" "$temporary"
  sync -f "$temporary"
  mv -fT "$temporary" "$destination"
  sync -f "$(dirname "$destination")"
}

ordinary_restore_caddy_and_ufw() {
  local host
  if [[ "$TRANSACTION_CADDY_ENABLEMENT" = enabled ]]; then
    systemctl enable caddy.service >/dev/null
  else
    systemctl disable caddy.service >/dev/null
  fi
  if [[ "$TRANSACTION_CADDY_ACTIVITY" = active ]]; then
    systemctl start caddy.service
  else
    systemctl stop caddy.service >/dev/null
  fi
  [[ "$(read_systemctl_enablement caddy.service)" = "$TRANSACTION_CADDY_ENABLEMENT" \
    && "$(read_systemctl_activity caddy.service)" = "$TRANSACTION_CADDY_ACTIVITY" ]]
  if [[ "$TRANSACTION_UFW_80" = present ]]; then
    host="${TRANSACTION_EXTERNAL_URL#https://}"
    host="${host%/mcp}"
    ufw_is_fail_closed
    ordinary_validate_caddy_contract "$host"
    ordinary_require_listener_topology public
    ordinary_probe_negative_caddy_routes "${TRANSACTION_EXTERNAL_URL%/mcp}"
    ufw allow 80/tcp comment 'Caddy ACME HTTP' >/dev/null
    ufw allow 443/tcp comment 'Australian Legal MCP HTTPS' >/dev/null
  else
    ordinary_require_listener_topology private
  fi
}

ordinary_verify_private_runtime() {
  local expected_image_id="$1" running_image_id host
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(read_systemctl_activity "$SERVICE")" = active \
    && "$(ufw_rule_state 80)" = absent \
    && "$(ufw_rule_state 443)" = absent ]]
  wait_for_exact_generation "$TRANSACTION_GENERATION"
  host="${TRANSACTION_EXTERNAL_URL#https://}"
  host="${host%/mcp}"
  ordinary_validate_caddy_contract "$host"
  ordinary_require_listener_topology private
  probe_auth_boundary http://127.0.0.1:51235/mcp \
    http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp
  running_image_id="$(canonical_image_id \
    "$(podman inspect australian-legal-mcp --format '{{.Image}}')")" || return 1
  [[ "$running_image_id" = "$expected_image_id" ]] || {
    echo 'private service does not use the transaction image ID' >&2
    return 1
  }
}

ordinary_verify_final_state() {
  local expected_manifest="$1" expected_image_id="$2" manifest running_image_id host
  ordinary_require_live_metadata
  manifest="$(mktemp /run/legal-mcp-image-live.XXXXXX)"
  ordinary_render_hash_manifest "$IMAGE_FILE" "$QUADLET" "$TEMPLATE" \
    "$RUNTIME_ENV" "$API_KEYS" "$CADDYFILE" "$AUTH_READY" \
    /srv/legal-mcp/lifecycle/active-generation "$manifest"
  if ! cmp --silent "$manifest" "$expected_manifest"; then
    rm -f "$manifest"
    echo 'live host files do not match the exact image transaction manifest' >&2
    return 1
  fi
  rm -f "$manifest"
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(read_systemctl_activity "$SERVICE")" = active \
    && "$(read_systemctl_enablement caddy.service)" = "$TRANSACTION_CADDY_ENABLEMENT" \
    && "$(read_systemctl_activity caddy.service)" = "$TRANSACTION_CADDY_ACTIVITY" \
    && "$(ufw_rule_state 80)" = "$TRANSACTION_UFW_80" \
    && "$(ufw_rule_state 443)" = "$TRANSACTION_UFW_443" ]]
  ufw_is_fail_closed
  wait_for_exact_generation "$TRANSACTION_GENERATION"
  probe_auth_boundary http://127.0.0.1:51235/mcp \
    http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp
  running_image_id="$(canonical_image_id \
    "$(podman inspect australian-legal-mcp --format '{{.Image}}')")" || return 1
  [[ "$running_image_id" = "$expected_image_id" ]] || {
    echo 'running container image ID does not match the transaction' >&2
    return 1
  }
  if [[ "$TRANSACTION_UFW_80" = present ]]; then
    host="${TRANSACTION_EXTERNAL_URL#https://}"
    host="${host%/mcp}"
    ordinary_validate_caddy_contract "$host"
    ordinary_require_listener_topology public
    ordinary_probe_negative_caddy_routes "${TRANSACTION_EXTERNAL_URL%/mcp}"
    probe_auth_boundary "$TRANSACTION_EXTERNAL_URL" \
      "${TRANSACTION_EXTERNAL_URL%/mcp}/.well-known/oauth-protected-resource/mcp"
  else
    ordinary_require_listener_topology private
  fi
}

ordinary_restore_saved_state() {
  local old_image_state old_image_id
  systemctl stop "$SERVICE" >/dev/null
  ordinary_atomic_install "$TRANSACTION/saved-image" "$IMAGE_FILE" root root 600
  ordinary_atomic_install "$TRANSACTION/saved-quadlet" "$QUADLET" root root 644
  ordinary_atomic_install "$TRANSACTION/saved-template" "$TEMPLATE" root root 644
  ordinary_atomic_install "$TRANSACTION/saved-runtime.env" "$RUNTIME_ENV" root root 600
  ordinary_atomic_install "$TRANSACTION/saved-api-keys.json" "$API_KEYS" legal-mcp legal-mcp 400
  ordinary_atomic_install "$TRANSACTION/saved-Caddyfile" "$CADDYFILE" root caddy 640
  ordinary_atomic_install "$TRANSACTION/saved-auth-ready" "$AUTH_READY" root root 444
  old_image_state="$(podman_image_state "$TRANSACTION_OLD_IMAGE")" || return 1
  if [[ "$old_image_state" = absent ]]; then podman pull "$TRANSACTION_OLD_IMAGE"; fi
  old_image_id="$(canonical_image_id \
    "$(podman image inspect "$TRANSACTION_OLD_IMAGE" --format '{{.Id}}')")" || return 1
  [[ "$old_image_id" = "$TRANSACTION_OLD_IMAGE_ID" ]] || {
    echo 'saved image pin no longer resolves to its recorded image ID' >&2
    return 1
  }
  systemctl daemon-reload
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated ]]
  systemctl restart "$SERVICE"
  ordinary_verify_private_runtime "$TRANSACTION_OLD_IMAGE_ID"
  ordinary_restore_caddy_and_ufw
  ordinary_verify_final_state "$TRANSACTION/saved-sha256" "$TRANSACTION_OLD_IMAGE_ID"
}

ordinary_read_transaction_probe_key() {
  load_runtime_contract "$TRANSACTION/saved-runtime.env"
  [[ "$AUTH_MODE" = "$TRANSACTION_AUTH_MODE" \
    && "$EXTERNAL_URL" = "$TRANSACTION_EXTERNAL_URL" ]]
  if [[ "$HAS_API" = true \
    && "${PROBE_API_KEY:-}" =~ ^[a-z0-9][a-z0-9_-]{0,63}\.[A-Za-z0-9_-]{43}$ ]]; then
    return 0
  fi
  read_probe_key
}

ordinary_recover_transaction() {
  ordinary_validate_transaction "$TRANSACTION"
  ordinary_read_transaction_probe_key
  close_ingress || {
    echo 'image recovery could not close public ingress' >&2
    return 1
  }
  ordinary_validate_recoverable_live_state || {
    echo 'live image state is outside the exact saved/target transaction states' >&2
    return 1
  }
  ordinary_restore_saved_state
  ordinary_retire_transaction saved
}

ordinary_verify_retirement_outcome() {
  local pinned_image expected_image_id expected_manifest resolved_image_id
  case "$TRANSACTION_RETIREMENT_OUTCOME" in
    saved)
      pinned_image="$TRANSACTION_OLD_IMAGE"
      expected_image_id="$TRANSACTION_OLD_IMAGE_ID"
      expected_manifest="$TRANSACTION/saved-sha256"
      ;;
    target)
      pinned_image="$TRANSACTION_TARGET_IMAGE"
      expected_image_id="$TRANSACTION_TARGET_IMAGE_ID"
      expected_manifest="$TRANSACTION/target-sha256"
      ;;
    *)
      echo 'image transaction is not authorized for retirement' >&2
      return 1
      ;;
  esac
  resolved_image_id="$(canonical_image_id \
    "$(podman image inspect "$pinned_image" --format '{{.Id}}')")" || return 1
  [[ "$resolved_image_id" = "$expected_image_id" ]] || {
    echo 'retiring image pin no longer resolves to its recorded image ID' >&2
    return 1
  }
  ordinary_verify_final_state "$expected_manifest" "$expected_image_id"
}

ordinary_complete_retiring_transaction() {
  ordinary_validate_transaction "$TRANSACTION_RETIRING"
  [[ "$TRANSACTION_RETIREMENT_OUTCOME" != pending ]]
  TRANSACTION="$TRANSACTION_RETIRING"
  ordinary_read_transaction_probe_key
  ordinary_verify_retirement_outcome
  mv -T "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"
  sync -f /etc/legal-mcp
  TRANSACTION=/etc/legal-mcp/.image-transaction
  ordinary_complete_retired_transaction
}

ordinary_complete_retired_transaction() {
  local payload_state saved_transaction="$TRANSACTION"
  # .retired is published only after the complete transaction and selected
  # live state were reverified and the parent rename was synced. Reverify while
  # the payload is complete; after recursive deletion removes its first member,
  # the allowlisted remainder is deletion-only and can always be resumed.
  payload_state="$(retired_image_payload_state "$TRANSACTION_RETIRED" \
    kind target-version target-revision updater-sha256 retirement-outcome release-sha256 \
    saved-sha256 target-sha256 saved-metadata target-metadata state \
    saved-image saved-quadlet saved-template saved-runtime.env \
    saved-api-keys.json saved-Caddyfile saved-auth-ready saved-active-generation \
    target-image target-quadlet target-template)"
  if [[ "$payload_state" = complete ]]; then
    TRANSACTION="$TRANSACTION_RETIRED"
    ordinary_validate_transaction "$TRANSACTION"
    [[ "$TRANSACTION_RETIREMENT_OUTCOME" != pending ]]
    ordinary_read_transaction_probe_key
    ordinary_verify_retirement_outcome
  fi
  delete_retired_image_directory "$TRANSACTION_RETIRED"
  TRANSACTION="$saved_transaction"
}

ordinary_retire_transaction() {
  local retirement_choice="$1" outcome_source
  [[ "$retirement_choice" = saved || "$retirement_choice" = target ]]
  image_path_is_absent "$TRANSACTION_RETIRING"
  image_path_is_absent "$TRANSACTION_RETIRED"
  ordinary_validate_transaction "$TRANSACTION"
  [[ "$TRANSACTION_RETIREMENT_OUTCOME" = pending ]]
  outcome_source="$(mktemp /run/legal-mcp-image-retirement-outcome.XXXXXX)"
  printf '%s\n' "$retirement_choice" > "$outcome_source"
  ordinary_atomic_install "$outcome_source" \
    "$TRANSACTION/retirement-outcome" root root 600
  rm -f "$outcome_source"
  ordinary_validate_transaction "$TRANSACTION"
  ordinary_verify_retirement_outcome
  mv -T "$TRANSACTION" "$TRANSACTION_RETIRING"
  sync -f /etc/legal-mcp
  mv -T "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"
  sync -f /etc/legal-mcp
  ordinary_complete_retired_transaction
}

ordinary_create_transaction() {
  local directory="$TRANSACTION_PREPARING" rendered
  image_path_is_absent "$directory" || return 1
  install -d -o root -g root -m 0700 "$directory"
  install -o root -g root -m 0600 "$IMAGE_FILE" "$directory/saved-image"
  install -o root -g root -m 0600 "$QUADLET" "$directory/saved-quadlet"
  install -o root -g root -m 0600 "$TEMPLATE" "$directory/saved-template"
  install -o root -g root -m 0600 "$RUNTIME_ENV" "$directory/saved-runtime.env"
  install -o root -g root -m 0600 "$API_KEYS" "$directory/saved-api-keys.json"
  install -o root -g root -m 0600 "$CADDYFILE" "$directory/saved-Caddyfile"
  install -o root -g root -m 0600 "$AUTH_READY" "$directory/saved-auth-ready"
  install -o root -g root -m 0600 /srv/legal-mcp/lifecycle/active-generation \
    "$directory/saved-active-generation"
  printf '%s\n' "$NEW_IMAGE" > "$directory/target-image"
  install -o root -g root -m 0600 "$ORDINARY_SOURCE_TEMPLATE" "$directory/target-template"
  rendered="$(mktemp /run/legal-mcp-image-target-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$NEW_IMAGE|g" "$ORDINARY_SOURCE_TEMPLATE" > "$rendered"
  install -o root -g root -m 0600 "$rendered" "$directory/target-quadlet"
  rm -f "$rendered"
  printf '%s\n' LEGAL_MCP_IMAGE_TRANSACTION_V2 > "$directory/kind"
  printf '%s\n' "$ORDINARY_VERSION" > "$directory/target-version"
  printf '%s\n' "$ORDINARY_REVISION" > "$directory/target-revision"
  printf '%s\n' "$ORDINARY_UPDATER_SHA256" > "$directory/updater-sha256"
  printf '%s\n' pending > "$directory/retirement-outcome"
  ordinary_render_release_manifest "$directory/release-sha256"
  ordinary_render_metadata_manifest "$directory/saved-metadata"
  ordinary_render_metadata_manifest "$directory/target-metadata"
  ordinary_render_hash_manifest \
    "$directory/saved-image" "$directory/saved-quadlet" \
    "$directory/saved-template" "$directory/saved-runtime.env" \
    "$directory/saved-api-keys.json" "$directory/saved-Caddyfile" \
    "$directory/saved-auth-ready" "$directory/saved-active-generation" \
    "$directory/saved-sha256"
  ordinary_render_hash_manifest \
    "$directory/target-image" "$directory/target-quadlet" \
    "$directory/target-template" "$directory/saved-runtime.env" \
    "$directory/saved-api-keys.json" "$directory/saved-Caddyfile" \
    "$directory/saved-auth-ready" "$directory/saved-active-generation" \
    "$directory/target-sha256"
  cat > "$directory/state" <<EOF
SERVICE_ENABLEMENT=generated
SERVICE_ACTIVITY=active
CADDY_ENABLEMENT=$CADDY_ENABLEMENT
CADDY_ACTIVITY=$CADDY_ACTIVITY
UFW_80=$UFW_80
UFW_443=$UFW_443
AUTH_MODE=$AUTH_MODE
EXTERNAL_URL=$EXTERNAL_URL
EXPECTED_GENERATION=$EXPECTED_GENERATION
OLD_IMAGE=$OLD_IMAGE
OLD_IMAGE_ID=$OLD_IMAGE_ID
TARGET_IMAGE=$NEW_IMAGE
TARGET_IMAGE_ID=$TARGET_IMAGE_ID
EOF
  chmod 600 "$directory"/*
  sync -f "$directory"
  ordinary_validate_transaction "$directory"
  mv -T "$directory" "$TRANSACTION"
  sync -f /etc/legal-mcp
}

ordinary_capture_baseline() {
  local rendered image_state container_state running_image_id host
  local -a current_image
  ordinary_require_live_metadata
  EXPECTED_GENERATION="$(</srv/legal-mcp/lifecycle/active-generation)"
  [[ "$EXPECTED_GENERATION" =~ ^[0-9a-f]{64}$ ]]
  mapfile -t current_image < "$IMAGE_FILE"
  [[ ${#current_image[@]} -eq 1 ]]
  OLD_IMAGE="${current_image[0]}"
  [[ "$(stat -c '%s' "$IMAGE_FILE")" = "$(( ${#OLD_IMAGE} + 1 ))" \
    && "$OLD_IMAGE" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ \
    && "$NEW_IMAGE" != "$OLD_IMAGE" ]] || {
      echo 'current and target image pins are malformed or identical' >&2
      return 1
    }
  [[ "$(grep -o '__IMAGE_DIGEST__' "$TEMPLATE" | wc -l)" = 1 ]]
  rendered="$(mktemp /run/legal-mcp-image-current-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$OLD_IMAGE|g" "$TEMPLATE" > "$rendered"
  if ! cmp --silent "$rendered" "$QUADLET"; then
    rm -f "$rendered"
    echo 'installed Quadlet is not the installed template rendered with /etc/legal-mcp/image' >&2
    return 1
  fi
  rm -f "$rendered"
  image_state="$(podman_image_state "$OLD_IMAGE")" || return 1
  [[ "$image_state" = present ]] || {
    echo 'currently pinned image is not present' >&2
    return 1
  }
  OLD_IMAGE_ID="$(canonical_image_id \
    "$(podman image inspect "$OLD_IMAGE" --format '{{.Id}}')")" || return 1
  container_state="$(podman_container_state australian-legal-mcp)" || return 1
  [[ "$container_state" = present ]] || {
    echo 'ordinary image update requires the running service container' >&2
    return 1
  }
  running_image_id="$(canonical_image_id \
    "$(podman inspect australian-legal-mcp --format '{{.Image}}')")" || return 1
  [[ "$running_image_id" = "$OLD_IMAGE_ID" ]] || {
    echo 'running container does not use the image pinned by /etc/legal-mcp/image' >&2
    return 1
  }
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(read_systemctl_activity "$SERVICE")" = active ]] || {
      echo 'ordinary image updates require the generated service active' >&2
      return 1
    }
  CADDY_ENABLEMENT="$(read_systemctl_enablement caddy.service)" || return 1
  CADDY_ACTIVITY="$(read_systemctl_activity caddy.service)" || return 1
  [[ "$CADDY_ENABLEMENT" = enabled && "$CADDY_ACTIVITY" = active ]] || {
    echo 'ordinary image updates require Caddy enabled and active' >&2
    return 1
  }
  UFW_80="$(ufw_rule_state 80)" || return 1
  UFW_443="$(ufw_rule_state 443)" || return 1
  [[ "$UFW_80" = "$UFW_443" ]] || {
    echo 'UFW 80/443 state is inconsistent' >&2
    return 1
  }
  [[ "$UFW_80" = present ]] || {
    echo 'ordinary image updates require exact public UFW ingress' >&2
    return 1
  }
  ufw_is_fail_closed
  wait_for_exact_generation "$EXPECTED_GENERATION"
  host="${EXTERNAL_URL#https://}"
  host="${host%/mcp}"
  ordinary_validate_caddy_contract "$host"
  ordinary_require_listener_topology public
  probe_auth_boundary http://127.0.0.1:51235/mcp \
    http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp
  probe_auth_boundary "$EXTERNAL_URL" \
    "${EXTERNAL_URL%/mcp}/.well-known/oauth-protected-resource/mcp"
  ordinary_probe_negative_caddy_routes "${EXTERNAL_URL%/mcp}"
}

ordinary_rollback() {
  local status=$? recovery_status
  trap - ERR HUP INT TERM EXIT
  set +e
  (
    set -e
    ordinary_recover_transaction
  )
  recovery_status=$?
  set -e
  if [[ $recovery_status -ne 0 ]]; then
    echo 'container image update failed and automatic rollback did not complete' >&2
    exit 1
  fi
  echo 'container image update rolled back' >&2
  exit "$status"
}

ordinary_recover_pending_state() {
  local path
  if ! image_path_is_absent "$TRANSACTION_PREPARING" \
    && ! image_path_is_absent "$TRANSACTION_PREPARING_RETIRED"; then
    echo 'image preparation has conflicting recovery states' >&2
    return 1
  fi
  if ! image_path_is_absent "$TRANSACTION_RETIRING" \
    && ! image_path_is_absent "$TRANSACTION_RETIRED"; then
    echo 'image retirement has conflicting recovery states' >&2
    return 1
  fi
  if ! image_path_is_absent "$TRANSACTION_PREPARING"; then
    ordinary_validate_transaction "$TRANSACTION_PREPARING"
    retire_image_directory_for_deletion \
      "$TRANSACTION_PREPARING" "$TRANSACTION_PREPARING_RETIRED"
    echo 'interrupted image preparation discarded'
    return 0
  fi
  if ! image_path_is_absent "$TRANSACTION_PREPARING_RETIRED"; then
    delete_retired_image_directory "$TRANSACTION_PREPARING_RETIRED"
    echo 'interrupted image preparation discarded'
    return 0
  fi
  if ! image_path_is_absent "$TRANSACTION_RETIRING"; then
    ordinary_complete_retiring_transaction
    echo 'interrupted image transaction retirement completed'
    return 0
  fi
  if ! image_path_is_absent "$TRANSACTION_RETIRED"; then
    ordinary_complete_retired_transaction
    echo 'interrupted image transaction retirement completed'
    return 0
  fi
  path="$TRANSACTION"
  image_path_is_absent "$path" && {
    echo 'no image transaction exists' >&2
    return 1
  }
  ordinary_validate_transaction "$path"
  if [[ "$TRANSACTION_RETIREMENT_OUTCOME" = pending ]]; then
    ordinary_recover_transaction
    unset PROBE_API_KEY
    echo 'interrupted image transaction rolled back'
    return 0
  fi
  ordinary_read_transaction_probe_key
  ordinary_verify_retirement_outcome
  mv -T "$TRANSACTION" "$TRANSACTION_RETIRING"
  sync -f /etc/legal-mcp
  ordinary_complete_retiring_transaction
  unset PROBE_API_KEY
  echo 'interrupted image transaction retirement completed'
}

cutover_path_is_absent() {
  image_path_is_absent "$1"
}

cutover_require_launcher_context() {
  local permit=/run/legal-mcp/flat-int8-cutover-starting
  local dispatch=/run/legal-mcp/host-tool-launcher-dispatch
  local permit_pid permit_start actual_start uid_line cmdline
  bootstrap_require_regular "$permit" root root 400 || {
    echo 'flat-int8 cutover must run through the installed stable root launcher' >&2
    return 1
  }
  bootstrap_require_acl "$permit" $'user::r--\ngroup::---\nother::---' || return 1
  read -r permit_pid permit_start < "$permit" || return 1
  [[ "$permit_pid" =~ ^[1-9][0-9]*$ && "$permit_start" =~ ^[1-9][0-9]*$ \
    && "$(wc -w < "$permit")" = 2 ]] || return 1
  actual_start="$(python3 - "$permit_pid" <<'PY'
import pathlib, sys
value = pathlib.Path(f"/proc/{sys.argv[1]}/stat").read_text()
fields = value.rpartition(") ")[2].split()
if len(fields) < 20 or not fields[19].isdigit():
    raise SystemExit(1)
print(fields[19])
PY
)" || return 1
  [[ "$actual_start" = "$permit_start" ]] || return 1
  uid_line="$(awk '$1 == "Uid:" {print $2 ":" $3 ":" $4 ":" $5}' "/proc/$permit_pid/status")" || return 1
  [[ "$uid_line" = 0:0:0:0 ]] || return 1
  cmdline="$(tr '\0' '\n' < "/proc/$permit_pid/cmdline")" || return 1
  grep -Fxq -- '--legal-mcp-launcher-internal' <<< "$cmdline" \
    && grep -Fxq update-image <<< "$cmdline" \
    && grep -Fxq -- '--flat-int8-cutover' <<< "$cmdline" || return 1
  [[ -d "$dispatch" && ! -L "$dispatch" \
    && "$(stat -c '%U:%G:%a' "$dispatch")" = root:root:700 ]] || return 1
  for name in pid start-time role configure-auth update-image; do
    bootstrap_require_regular "$dispatch/$name" root root 600 || return 1
  done
  [[ "$(<"$dispatch/pid")" = "$permit_pid" \
    && "$(<"$dispatch/start-time")" = "$permit_start" \
    && "$(<"$dispatch/role")" = update-image ]] || return 1
}

cutover_require_no_foreign_transaction() {
  local allow_cutover="$1" path found
  for path in \
    /etc/legal-mcp/.auth-transaction.preparing \
    /etc/legal-mcp/.auth-transaction.preparing-retired \
    /etc/legal-mcp/.auth-transaction \
    /etc/legal-mcp/.auth-transaction.retiring \
    /etc/legal-mcp/.auth-transaction.retired \
    /etc/legal-mcp/.auth-transaction.legacy-v0192-preparing-retiring \
    /etc/legal-mcp/.auth-transaction.legacy-v0192-preparing-retired \
    /etc/legal-mcp/.image-transaction.preparing \
    /etc/legal-mcp/.image-transaction.preparing-retired \
    /etc/legal-mcp/.host-tools-transaction.building \
    /etc/legal-mcp/.host-tools-transaction.building-retired \
    /etc/legal-mcp/.host-tools-transaction.preparing \
    /etc/legal-mcp/.host-tools-transaction.preparing-retired \
    /etc/legal-mcp/.host-tools-transaction \
    /etc/legal-mcp/.host-tools-transaction.retiring \
    /etc/legal-mcp/.host-tools-transaction.retired \
    /etc/legal-mcp/.host-tools-transaction.rollback-retiring \
    /etc/legal-mcp/.host-tools-transaction.rollback-retired \
    /etc/legal-mcp/.host-tools-transaction.publisher-restore \
    /etc/legal-mcp/.host-tools-transaction.publisher-restore-retired; do
    cutover_path_is_absent "$path" || {
      echo 'a foreign auth, host-tool, or corpus preparation transaction must be recovered first' >&2
      return 1
    }
  done
  found="$(find /etc/legal-mcp -mindepth 1 -maxdepth 1 \
    -name '.auth-transaction.preparing.*' -print -quit)" || return 1
  [[ -z "$found" ]] || {
    echo 'a foreign authentication preparation must be recovered first' >&2
    return 1
  }
  if [[ "$allow_cutover" = false ]]; then
    for path in "$CUTOVER_TRANSACTION_PREPARING" \
      "$CUTOVER_TRANSACTION_PREPARING_RETIRED" "$TRANSACTION" \
      "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"; do
      cutover_path_is_absent "$path" || {
        echo 'an image transaction already exists; use explicit flat-int8 recovery' >&2
        return 1
      }
    done
  fi
}

cutover_validate_mount_contract() {
  local target source fstype options xfs_details actual_uuid marker_uuid directory
  local -a marker
  read -r target source fstype options < <(
    findmnt --noheadings --raw --output TARGET,SOURCE,FSTYPE,OPTIONS \
      --target /srv/legal-mcp
  )
  [[ "$target" = /srv/legal-mcp && "$fstype" = xfs \
    && ",$options," = *,noatime,* && ",$options," = *,nodev,* \
    && ",$options," = *,noexec,* && ",$options," = *,nosuid,* ]] || {
      echo 'flat-int8 cutover requires the exact mounted XFS corpus volume' >&2
      return 1
    }
  xfs_details="$(xfs_info /srv/legal-mcp)" || return 1
  grep -Eq 'reflink=1([[:space:]]|$)' <<< "$xfs_details" \
    && grep -Eq 'ftype=1([[:space:]]|$)' <<< "$xfs_details" || return 1
  bootstrap_require_regular /srv/legal-mcp/.legal-mcp-volume root root 444 || return 1
  bootstrap_require_acl /srv/legal-mcp/.legal-mcp-volume \
    $'user::r--\ngroup::r--\nother::r--' || return 1
  mapfile -t marker < /srv/legal-mcp/.legal-mcp-volume
  [[ ${#marker[@]} -eq 2 && "${marker[0]}" = LEGAL_MCP_VOLUME_V1 \
    && "${marker[1]}" =~ ^UUID=([0-9A-Fa-f-]{36})$ ]] || return 1
  marker_uuid="${BASH_REMATCH[1],,}"
  actual_uuid="$(blkid -s UUID -o value "$source" | tr '[:upper:]' '[:lower:]')" || return 1
  [[ "$actual_uuid" = "$marker_uuid" ]] || return 1
  [[ -d /srv/legal-mcp && ! -L /srv/legal-mcp \
    && "$(stat -c '%U:%G:%a' /srv/legal-mcp)" = root:legal-mcp:750 ]] || return 1
  bootstrap_require_acl /srv/legal-mcp \
    $'user::rwx\nuser:973:--x\ngroup::r-x\nmask::r-x\nother::---' || return 1
  for directory in generations lifecycle state uploads; do
    [[ -d "/srv/legal-mcp/$directory" && ! -L "/srv/legal-mcp/$directory" ]] || return 1
  done
  bootstrap_require_regular /srv/legal-mcp/lifecycle/LOCK root legal-mcp 640 || return 1
  bootstrap_require_empty_regular /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK root root 640 || return 1
}

cutover_validate_generation_manifest() {
  local manifest="$1" expected_format="$2"
  bootstrap_require_regular "$manifest" "$3" "$4" "$5" || return 1
  python3 - "$manifest" "$expected_format" <<'PY'
import json, pathlib, sys
path, expected = sys.argv[1:]
value = json.loads(pathlib.Path(path).read_bytes())
sources = {
    "ato", "frl", "federal-court", "high-court", "nsw-caselaw",
    "nsw-legislation", "qld-legislation", "wa-legislation",
    "sa-legislation", "tas-legislation",
}
if value.get("schema_version") != 11 or set(value.get("ann", {})) != sources:
    raise SystemExit(1)
for source, ann in value["ann"].items():
    if ann.get("source_id") != source or ann.get("path") != f"ann/{source}.ann":
        raise SystemExit(1)
    if ann.get("id_encoding") != "sqlite-chunk-id-u32":
        raise SystemExit(1)
    if expected == "arroy":
        if not (ann.get("format") == "arroy-cosine-f32" and ann.get("format_version") == 3
                and ann.get("library") == "arroy" and ann.get("library_version") == "0.6.4"):
            raise SystemExit(1)
    elif expected == "flat-int8":
        if not (ann.get("format") == "flat-int8" and ann.get("format_version") == 1
                and ann.get("metric") == "signed-int8-dot-exact"):
            raise SystemExit(1)
        if any(key in ann for key in ("library", "library_version", "trees", "seed", "rng")):
            raise SystemExit(1)
    else:
        raise SystemExit(1)
PY
}

cutover_read_deployment_journal() {
  local journal=/srv/legal-mcp/lifecycle/.deployment-transaction
  bootstrap_require_regular "$journal" root root 600 || return 1
  mapfile -t CUTOVER_DEPLOYMENT < "$journal"
  [[ ( ${#CUTOVER_DEPLOYMENT[@]} -eq 3 \
      || ( ${#CUTOVER_DEPLOYMENT[@]} -eq 4 \
        && "${CUTOVER_DEPLOYMENT[3]}" = flat-int8-cutover ) ) \
    && "${CUTOVER_DEPLOYMENT[0]}" =~ ^[0-9a-f]{64}$ \
    && "${CUTOVER_DEPLOYMENT[1]}" =~ ^[0-9a-f]{64}$ \
    && "${CUTOVER_DEPLOYMENT[2]}" =~ ^(prepared|activating|activated|rolling-back|rolled-back)$ ]] \
    || return 1
  CUTOVER_DEPLOYMENT_GENERATION="${CUTOVER_DEPLOYMENT[0]}"
  CUTOVER_DEPLOYMENT_PREVIOUS="${CUTOVER_DEPLOYMENT[1]}"
  CUTOVER_DEPLOYMENT_PHASE="${CUTOVER_DEPLOYMENT[2]}"
  if [[ ${#CUTOVER_DEPLOYMENT[@]} -eq 4 ]]; then
    CUTOVER_DEPLOYMENT_KIND=flat-int8-cutover
  else
    CUTOVER_DEPLOYMENT_KIND=ordinary
  fi
}

cutover_candidate_manifest() {
  local target="$1" upload installed
  upload="/srv/legal-mcp/uploads/$target"
  installed="/srv/legal-mcp/generations/$target"
  if [[ -d "$upload" && ! -L "$upload" \
    && ! -e "$installed" && ! -L "$installed" ]]; then
    printf '%s\n' "$upload/generation.json"
  elif [[ -d "$installed" && ! -L "$installed" \
    && ! -e "$upload" && ! -L "$upload" ]]; then
    printf '%s\n' "$installed/generation.json"
  else
    echo 'cutover target must exist in exactly one prepared or installed location' >&2
    return 1
  fi
}

cutover_render_state() {
  local destination="$1"
  cat > "$destination" <<EOF
PUBLICATION_STATE=configured-dark
SERVICE_ENABLEMENT=generated
SERVICE_ACTIVITY=inactive
CADDY_ENABLEMENT=disabled
CADDY_ACTIVITY=inactive
UFW_80=absent
UFW_443=absent
AUTH_MODE=$AUTH_MODE
EXTERNAL_URL=$EXTERNAL_URL
PRIOR_GENERATION=$CUTOVER_EXPECTED_CURRENT_GENERATION
TARGET_GENERATION=$CUTOVER_GENERATION
OLD_IMAGE=$OLD_IMAGE
OLD_IMAGE_ID=$OLD_IMAGE_ID
OLD_IMAGE_VERSION=$CUTOVER_OLD_IMAGE_VERSION
OLD_IMAGE_REVISION=$CUTOVER_OLD_IMAGE_REVISION
TARGET_IMAGE=$NEW_IMAGE
TARGET_IMAGE_ID=$TARGET_IMAGE_ID
PRIOR_MANIFEST_SHA256=$CUTOVER_PRIOR_MANIFEST_SHA256
TARGET_MANIFEST_SHA256=$CUTOVER_TARGET_MANIFEST_SHA256
UPLOAD_AUTHORIZATION=$CUTOVER_UPLOAD_AUTHORIZATION
EOF
}

cutover_create_transaction() {
  local directory="$CUTOVER_TRANSACTION_PREPARING" rendered manifest
  cutover_path_is_absent "$directory" || return 1
  install -d -o root -g root -m 0700 "$directory"
  install -o root -g root -m 0600 "$IMAGE_FILE" "$directory/saved-image"
  install -o root -g root -m 0600 "$QUADLET" "$directory/saved-quadlet"
  install -o root -g root -m 0600 "$TEMPLATE" "$directory/saved-template"
  install -o root -g root -m 0600 "$RUNTIME_ENV" "$directory/saved-runtime.env"
  install -o root -g root -m 0600 "$API_KEYS" "$directory/saved-api-keys.json"
  install -o root -g root -m 0600 "$CADDYFILE" "$directory/saved-Caddyfile"
  cutover_path_is_absent "$AUTH_READY"
  install -o root -g root -m 0600 /dev/null "$directory/saved-auth-ready"
  install -o root -g root -m 0600 /srv/legal-mcp/lifecycle/active-generation \
    "$directory/saved-active-generation"
  install -o root -g root -m 0600 /srv/legal-mcp/lifecycle/.deployment-transaction \
    "$directory/saved-deployment-journal"
  if [[ "$CUTOVER_UPLOAD_AUTHORIZATION" = present ]]; then
    install -o root -g root -m 0600 /run/legal-mcp/authorized-upload \
      "$directory/saved-upload-authorization"
  else
    : > "$directory/saved-upload-authorization"
  fi
  printf '%s\n' "$NEW_IMAGE" > "$directory/target-image"
  install -o root -g root -m 0600 "$ORDINARY_SOURCE_TEMPLATE" "$directory/target-template"
  rendered="$(mktemp /run/legal-mcp-cutover-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$NEW_IMAGE|g" "$ORDINARY_SOURCE_TEMPLATE" > "$rendered"
  install -o root -g root -m 0600 "$rendered" "$directory/target-quadlet"
  rm -f "$rendered"
  printf '%s' "$CUTOVER_GENERATION" > "$directory/target-active-generation"
  printf '%s\n' LEGAL_MCP_FLAT_INT8_CUTOVER_TRANSACTION_V1 > "$directory/kind"
  printf '%s\n' "$ORDINARY_VERSION" > "$directory/target-version"
  printf '%s\n' "$ORDINARY_REVISION" > "$directory/target-revision"
  printf '%s\n' "$ORDINARY_UPDATER_SHA256" > "$directory/updater-sha256"
  printf '%s\n' pending > "$directory/retirement-outcome"
  ordinary_render_release_manifest "$directory/release-sha256"
  ordinary_render_metadata_manifest "$directory/saved-metadata"
  ordinary_render_metadata_manifest "$directory/target-metadata"
  ordinary_render_hash_manifest \
    "$directory/saved-image" "$directory/saved-quadlet" \
    "$directory/saved-template" "$directory/saved-runtime.env" \
    "$directory/saved-api-keys.json" "$directory/saved-Caddyfile" \
    "$directory/saved-auth-ready" "$directory/saved-active-generation" \
    "$directory/saved-sha256"
  ordinary_render_hash_manifest \
    "$directory/target-image" "$directory/target-quadlet" \
    "$directory/target-template" "$directory/saved-runtime.env" \
    "$directory/saved-api-keys.json" "$directory/saved-Caddyfile" \
    "$directory/saved-auth-ready" "$directory/target-active-generation" \
    "$directory/target-sha256"
  cutover_render_state "$directory/state"
  chmod 600 "$directory"/*
  sync -f "$directory"
  cutover_validate_transaction "$directory"
  mv -T "$directory" "$TRANSACTION"
  sync -f /etc/legal-mcp
}

cutover_validate_transaction() {
  local directory="$1" name rendered manifest metadata release_manifest
  local -a kind version revision updater outcome state saved_deployment saved_authorization
  require_image_transaction_directory "$directory"
  cutover_reconcile_outcome_preparation "$directory"
  for name in kind target-version target-revision updater-sha256 retirement-outcome \
    release-sha256 saved-sha256 target-sha256 saved-metadata target-metadata state \
    saved-image saved-quadlet saved-template saved-runtime.env saved-api-keys.json \
    saved-Caddyfile saved-auth-ready saved-active-generation saved-deployment-journal \
    saved-upload-authorization target-image target-quadlet target-template \
    target-active-generation; do
    ordinary_require_regular "$directory/$name" root root 600 || return 1
  done
  bootstrap_directory_contains_only "$directory" \
    kind target-version target-revision updater-sha256 retirement-outcome \
    release-sha256 saved-sha256 target-sha256 saved-metadata target-metadata state \
    saved-image saved-quadlet saved-template saved-runtime.env saved-api-keys.json \
    saved-Caddyfile saved-auth-ready saved-active-generation saved-deployment-journal \
    saved-upload-authorization target-image target-quadlet target-template \
    target-active-generation || {
      echo 'flat-int8 cutover transaction contains unexpected state' >&2
      return 1
    }
  mapfile -t kind < "$directory/kind"
  mapfile -t version < "$directory/target-version"
  mapfile -t revision < "$directory/target-revision"
  mapfile -t updater < "$directory/updater-sha256"
  mapfile -t outcome < "$directory/retirement-outcome"
  [[ ${#kind[@]} -eq 1 \
    && "${kind[0]}" = LEGAL_MCP_FLAT_INT8_CUTOVER_TRANSACTION_V1 \
    && ${#version[@]} -eq 1 && "${version[0]}" = "$ORDINARY_VERSION" \
    && ${#revision[@]} -eq 1 && "${revision[0]}" = "$ORDINARY_REVISION" \
    && ${#updater[@]} -eq 1 && "${updater[0]}" = "$ORDINARY_UPDATER_SHA256" \
    && ${#outcome[@]} -eq 1 && "${outcome[0]}" =~ ^(pending|saved|target)$ ]] || {
      echo 'flat-int8 cutover transaction is not bound to this exact release updater' >&2
      return 1
    }
  CUTOVER_RETIREMENT_OUTCOME="${outcome[0]}"
  release_manifest="$(mktemp /run/legal-mcp-cutover-release.XXXXXX)"
  ordinary_render_release_manifest "$release_manifest"
  if ! cmp --silent "$release_manifest" "$directory/release-sha256"; then
    rm -f "$release_manifest"
    echo 'flat-int8 cutover release-byte manifest changed' >&2
    return 1
  fi
  rm -f "$release_manifest"
  metadata="$(mktemp /run/legal-mcp-cutover-metadata.XXXXXX)"
  ordinary_render_metadata_manifest "$metadata"
  if ! cmp --silent "$metadata" "$directory/saved-metadata" \
    || ! cmp --silent "$metadata" "$directory/target-metadata"; then
    rm -f "$metadata"
    echo 'flat-int8 cutover metadata contract changed' >&2
    return 1
  fi
  rm -f "$metadata"
  manifest="$(mktemp /run/legal-mcp-cutover-hashes.XXXXXX)"
  ordinary_render_hash_manifest \
    "$directory/saved-image" "$directory/saved-quadlet" \
    "$directory/saved-template" "$directory/saved-runtime.env" \
    "$directory/saved-api-keys.json" "$directory/saved-Caddyfile" \
    "$directory/saved-auth-ready" "$directory/saved-active-generation" "$manifest"
  if ! cmp --silent "$manifest" "$directory/saved-sha256"; then
    rm -f "$manifest"
    echo 'saved flat-int8 cutover bytes do not match their hashes' >&2
    return 1
  fi
  ordinary_render_hash_manifest \
    "$directory/target-image" "$directory/target-quadlet" \
    "$directory/target-template" "$directory/saved-runtime.env" \
    "$directory/saved-api-keys.json" "$directory/saved-Caddyfile" \
    "$directory/saved-auth-ready" "$directory/target-active-generation" "$manifest"
  if ! cmp --silent "$manifest" "$directory/target-sha256"; then
    rm -f "$manifest"
    echo 'target flat-int8 cutover bytes do not match their hashes' >&2
    return 1
  fi
  rm -f "$manifest"

  mapfile -t state < "$directory/state"
  [[ ${#state[@]} -eq 20 \
    && "${state[0]}" = PUBLICATION_STATE=configured-dark \
    && "${state[1]}" = SERVICE_ENABLEMENT=generated \
    && "${state[2]}" = SERVICE_ACTIVITY=inactive \
    && "${state[3]}" = CADDY_ENABLEMENT=disabled \
    && "${state[4]}" = CADDY_ACTIVITY=inactive \
    && "${state[5]}" = UFW_80=absent \
    && "${state[6]}" = UFW_443=absent \
    && "${state[7]}" =~ ^AUTH_MODE=(api-key|entra|entra\+api-key)$ \
    && "${state[8]}" =~ ^EXTERNAL_URL=https://[a-z0-9.-]+/mcp$ \
    && "${state[9]}" =~ ^PRIOR_GENERATION=([0-9a-f]{64})$ ]] || {
      echo 'flat-int8 cutover saved service state is malformed' >&2
      return 1
    }
  CUTOVER_PRIOR_GENERATION="${state[9]#*=}"
  [[ "${state[10]}" =~ ^TARGET_GENERATION=([0-9a-f]{64})$ ]]
  CUTOVER_TARGET_GENERATION="${state[10]#*=}"
  [[ "$CUTOVER_PRIOR_GENERATION" != "$CUTOVER_TARGET_GENERATION" ]]
  [[ "${state[11]}" =~ ^OLD_IMAGE=(ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64})$ ]]
  CUTOVER_OLD_IMAGE="${state[11]#*=}"
  [[ "${state[12]}" =~ ^OLD_IMAGE_ID=(sha256:[0-9a-f]{64})$ ]]
  CUTOVER_OLD_IMAGE_ID="${state[12]#*=}"
  [[ "${state[13]}" =~ ^OLD_IMAGE_VERSION=([0-9]+\.[0-9]+\.[0-9]+)$ ]]
  CUTOVER_OLD_IMAGE_VERSION="${state[13]#*=}"
  [[ "${state[14]}" =~ ^OLD_IMAGE_REVISION=([0-9a-f]{40})$ ]]
  CUTOVER_OLD_IMAGE_REVISION="${state[14]#*=}"
  [[ "${state[15]}" =~ ^TARGET_IMAGE=(ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64})$ ]]
  CUTOVER_TARGET_IMAGE="${state[15]#*=}"
  [[ "${state[16]}" =~ ^TARGET_IMAGE_ID=(sha256:[0-9a-f]{64})$ ]]
  CUTOVER_TARGET_IMAGE_ID="${state[16]#*=}"
  [[ "${state[17]}" =~ ^PRIOR_MANIFEST_SHA256=([0-9a-f]{64})$ ]]
  CUTOVER_PRIOR_MANIFEST_SHA256="${state[17]#*=}"
  [[ "${state[18]}" =~ ^TARGET_MANIFEST_SHA256=([0-9a-f]{64})$ ]]
  CUTOVER_TARGET_MANIFEST_SHA256="${state[18]#*=}"
  [[ "${state[19]}" =~ ^UPLOAD_AUTHORIZATION=(present|absent)$ ]]
  CUTOVER_UPLOAD_AUTHORIZATION="${state[19]#*=}"
  [[ "$CUTOVER_OLD_IMAGE" != "$CUTOVER_TARGET_IMAGE" ]]
  TRANSACTION_CADDY_ENABLEMENT=disabled
  TRANSACTION_CADDY_ACTIVITY=inactive
  TRANSACTION_UFW_80=absent
  TRANSACTION_UFW_443=absent
  TRANSACTION_AUTH_MODE="${state[7]#*=}"
  TRANSACTION_EXTERNAL_URL="${state[8]#*=}"
  TRANSACTION_OLD_IMAGE="$CUTOVER_OLD_IMAGE"
  TRANSACTION_OLD_IMAGE_ID="$CUTOVER_OLD_IMAGE_ID"
  TRANSACTION_TARGET_IMAGE="$CUTOVER_TARGET_IMAGE"
  TRANSACTION_TARGET_IMAGE_ID="$CUTOVER_TARGET_IMAGE_ID"
  mapfile -t saved_deployment < "$directory/saved-deployment-journal"
  [[ ( ${#saved_deployment[@]} -eq 3 \
      || ( ${#saved_deployment[@]} -eq 4 \
        && "${saved_deployment[3]}" = flat-int8-cutover ) ) \
    && "${saved_deployment[0]}" = "$CUTOVER_TARGET_GENERATION" \
    && "${saved_deployment[1]}" = "$CUTOVER_PRIOR_GENERATION" \
    && ( ( ${#saved_deployment[@]} -eq 3 && "${saved_deployment[2]}" = prepared ) \
      || ( ${#saved_deployment[@]} -eq 4 && "${saved_deployment[2]}" = rolled-back ) ) ]] \
    || {
      echo 'saved corpus transaction is not an exact prepared cutover binding' >&2
      return 1
    }
  [[ "$(<"$directory/saved-active-generation")" = "$CUTOVER_PRIOR_GENERATION" \
    && "$(<"$directory/target-active-generation")" = "$CUTOVER_TARGET_GENERATION" \
    && "$(<"$directory/saved-image")" = "$CUTOVER_OLD_IMAGE" \
    && "$(<"$directory/target-image")" = "$CUTOVER_TARGET_IMAGE" ]] || return 1
  if [[ "$CUTOVER_UPLOAD_AUTHORIZATION" = present ]]; then
    mapfile -t saved_authorization < "$directory/saved-upload-authorization"
    [[ ${#saved_authorization[@]} -eq 1 \
      && "${saved_authorization[0]}" = "$CUTOVER_TARGET_GENERATION" ]] || return 1
  else
    [[ "$(stat -c '%s' "$directory/saved-upload-authorization")" = 0 ]] || return 1
  fi
  [[ "$(grep -o '__IMAGE_DIGEST__' "$directory/saved-template" | wc -l)" = 1 \
    && "$(grep -o '__IMAGE_DIGEST__' "$directory/target-template" | wc -l)" = 1 ]]
  rendered="$(mktemp /run/legal-mcp-cutover-rendered.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$CUTOVER_OLD_IMAGE|g" "$directory/saved-template" > "$rendered"
  cmp --silent "$rendered" "$directory/saved-quadlet" || {
    rm -f "$rendered"; return 1;
  }
  sed "s|__IMAGE_DIGEST__|$CUTOVER_TARGET_IMAGE|g" "$directory/target-template" > "$rendered"
  if ! cmp --silent "$rendered" "$directory/target-quadlet" \
    || ! cmp --silent "$directory/target-template" "$ORDINARY_SOURCE_TEMPLATE"; then
    rm -f "$rendered"
    return 1
  fi
  rm -f "$rendered"
  load_runtime_contract "$directory/saved-runtime.env"
  [[ "$AUTH_MODE" = "$TRANSACTION_AUTH_MODE" \
    && "$EXTERNAL_URL" = "$TRANSACTION_EXTERNAL_URL" ]]
}

cutover_reconcile_outcome_preparation() {
  local directory="$1" preparing="$1/retirement-outcome.preparing"
  cutover_path_is_absent "$preparing" && return 0
  require_image_transaction_directory "$directory"
  [[ -f "$preparing" || -L "$preparing" ]] || {
    echo 'flat-int8 outcome preparation has an unsafe file type' >&2
    return 1
  }
  rm -f -- "$preparing"
  cutover_path_is_absent "$preparing" || return 1
  sync -f "$directory"
}

cutover_validate_template_contract() {
  local template="$1"
  [[ "$(grep -Fxc 'User=971:971' "$template")" = 1 \
    && "$(grep -Fxc 'PublishPort=127.0.0.1:51235:51235' "$template")" = 1 \
    && "$(grep -Fxc 'ReadOnly=true' "$template")" = 1 \
    && "$(grep -Fxc 'DropCapability=all' "$template")" = 1 \
    && "$(grep -Fxc 'NoNewPrivileges=true' "$template")" = 1 \
    && "$(grep -Fxc 'ExecCondition=/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready' "$template")" = 1 \
    && "$(grep -Fc 'Volume=/srv/legal-mcp/generations:/var/lib/legal-mcp/generations:ro,nodev,nosuid,noexec' "$template")" = 1 \
    && "$(grep -Fc 'Volume=/srv/legal-mcp/lifecycle:/var/lib/legal-mcp/lifecycle:ro,nodev,nosuid,noexec' "$template")" = 1 \
    && "$(grep -Fc 'Volume=/srv/legal-mcp/state:/var/lib/legal-mcp/state:rw,nodev,nosuid,noexec' "$template")" = 1 ]] || {
      echo 'flat-int8 cutover template lacks the exact hardened mount/service contract' >&2
      return 1
    }
}

cutover_clear_upload_authorization() {
  local authorization=/run/legal-mcp/authorized-upload
  cutover_path_is_absent "$authorization" && return 0
  bootstrap_require_regular "$authorization" root legal-mcp-publisher 440 || return 1
  [[ "$(<"$authorization")" = "$CUTOVER_TARGET_GENERATION" ]] || {
    echo 'upload authorization is foreign to the coordinated generation' >&2
    return 1
  }
  rm -f -- "$authorization"
  cutover_path_is_absent "$authorization" || return 1
  sync -f /run/legal-mcp
}

cutover_restore_upload_authorization() {
  local authorization=/run/legal-mcp/authorized-upload
  if [[ "$CUTOVER_UPLOAD_AUTHORIZATION" = present ]]; then
    if ! cutover_path_is_absent "$authorization"; then
      bootstrap_require_regular "$authorization" root legal-mcp-publisher 440 || return 1
      cmp --silent "$authorization" "$TRANSACTION/saved-upload-authorization"
      return
    fi
    ordinary_atomic_install "$TRANSACTION/saved-upload-authorization" \
      "$authorization" root legal-mcp-publisher 440
    bootstrap_require_regular "$authorization" root legal-mcp-publisher 440
    cmp --silent "$authorization" "$TRANSACTION/saved-upload-authorization"
  else
    cutover_path_is_absent "$authorization"
  fi
}

cutover_upload_authorization_matches() {
  local authorization=/run/legal-mcp/authorized-upload
  if [[ "$CUTOVER_UPLOAD_AUTHORIZATION" = present ]]; then
    bootstrap_require_regular "$authorization" root legal-mcp-publisher 440
    cmp --silent "$authorization" "$TRANSACTION/saved-upload-authorization"
  else
    cutover_path_is_absent "$authorization"
  fi
}

cutover_force_dark() {
  local activity
  if ! cutover_path_is_absent "$CUTOVER_START_ARM"; then
    bootstrap_require_regular "$CUTOVER_START_ARM" root root 400 || return 1
    rm -f -- "$CUTOVER_START_ARM"
    sync -f /run/legal-mcp
  fi
  if ! cutover_path_is_absent "$AUTH_READY"; then
    ordinary_require_regular "$AUTH_READY" root root 444 || return 1
    [[ "$(stat -c '%s' "$AUTH_READY")" = 0 \
      && "$(getfacl --absolute-names --numeric --omit-header "$AUTH_READY")" \
        = $'user::r--\ngroup::r--\nother::r--' ]] || return 1
    if [[ -d "$TRANSACTION" && ! -L "$TRANSACTION" ]]; then
      cmp --silent "$AUTH_READY" "$TRANSACTION/saved-auth-ready" || return 1
    fi
    rm -f -- "$AUTH_READY"
    sync -f /etc/legal-mcp
  fi
  close_ingress
  activity="$(read_systemctl_activity "$SERVICE")" || return 1
  if [[ "$activity" = active ]]; then
    systemctl stop "$SERVICE"
  fi
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(read_systemctl_activity "$SERVICE")" = inactive \
    && "$(read_systemctl_enablement caddy.service)" = disabled \
    && "$(read_systemctl_activity caddy.service)" = inactive \
    && "$(ufw_rule_state 80)" = absent \
    && "$(ufw_rule_state 443)" = absent ]] || return 1
  cutover_path_is_absent "$AUTH_READY" || return 1
  ordinary_require_listener_topology none
  cutover_clear_upload_authorization
}

cutover_arm_private_start() {
  local context=/run/legal-mcp/flat-int8-cutover-starting temporary
  bootstrap_require_regular "$context" root root 400 || return 1
  cutover_path_is_absent "$CUTOVER_START_ARM" || return 1
  temporary="$(mktemp /run/legal-mcp/.flat-int8-cutover-start-armed.XXXXXX)"
  install -o root -g root -m 0400 "$context" "$temporary"
  sync -f "$temporary"
  mv -fT "$temporary" "$CUTOVER_START_ARM"
  sync -f /run/legal-mcp
  bootstrap_require_regular "$CUTOVER_START_ARM" root root 400
  cmp --silent "$context" "$CUTOVER_START_ARM"
}

cutover_current_file_matches() {
  ordinary_current_file_matches "$1" "$2" "$3"
}

cutover_validate_upload_manifest() {
  local manifest="$1" owner group mode
  read -r owner group mode < <(stat -c '%U %G %a' "$manifest") || return 1
  case "$owner:$group:$mode" in
    root:legal-mcp:440|root:legal-mcp:640|\
    legal-mcp-publisher:legal-mcp-publisher:440|\
    legal-mcp-publisher:legal-mcp-publisher:600|\
    legal-mcp-publisher:legal-mcp-publisher:640) ;;
    *)
      echo 'cutover upload manifest is outside the sealed/normalizing/publisher restoration states' >&2
      return 1
      ;;
  esac
  cutover_validate_generation_manifest "$manifest" flat-int8 "$owner" "$group" "$mode"
}

cutover_validate_recoverable_live_state() {
  local pointer candidate_manifest container_state running_image_id
  ordinary_require_regular "$IMAGE_FILE" root root 600
  ordinary_require_regular "$QUADLET" root root 644
  ordinary_require_regular "$TEMPLATE" root root 644
  ordinary_require_regular "$RUNTIME_ENV" root root 600
  ordinary_require_regular "$API_KEYS" legal-mcp legal-mcp 400
  ordinary_require_regular "$CADDYFILE" root caddy 640
  ordinary_require_regular /srv/legal-mcp/lifecycle/active-generation root root 644
  [[ "$(stat -c '%s' /srv/legal-mcp/lifecycle/active-generation)" = 64 ]]
  cutover_current_file_matches "$IMAGE_FILE" \
    "$TRANSACTION/saved-image" "$TRANSACTION/target-image"
  cutover_current_file_matches "$QUADLET" \
    "$TRANSACTION/saved-quadlet" "$TRANSACTION/target-quadlet"
  cutover_current_file_matches "$TEMPLATE" \
    "$TRANSACTION/saved-template" "$TRANSACTION/target-template"
  cmp --silent "$RUNTIME_ENV" "$TRANSACTION/saved-runtime.env"
  cmp --silent "$API_KEYS" "$TRANSACTION/saved-api-keys.json"
  cmp --silent "$CADDYFILE" "$TRANSACTION/saved-Caddyfile"
  cutover_path_is_absent "$AUTH_READY"
  pointer="$(</srv/legal-mcp/lifecycle/active-generation)"
  [[ "$pointer" = "$CUTOVER_PRIOR_GENERATION" \
    || "$pointer" = "$CUTOVER_TARGET_GENERATION" ]] || {
      echo 'cutover recovery found a generation outside the prior/target pair' >&2
      return 1
    }
  [[ "$(sha256sum "/srv/legal-mcp/generations/$CUTOVER_PRIOR_GENERATION/generation.json" | awk '{print $1}')" \
    = "$CUTOVER_PRIOR_MANIFEST_SHA256" ]] || return 1
  cutover_validate_generation_manifest \
    "/srv/legal-mcp/generations/$CUTOVER_PRIOR_GENERATION/generation.json" \
    arroy root legal-mcp 440 || return 1
  if [[ -e /srv/legal-mcp/lifecycle/.deployment-transaction \
    || -L /srv/legal-mcp/lifecycle/.deployment-transaction ]]; then
    cutover_read_deployment_journal || return 1
    [[ "$CUTOVER_DEPLOYMENT_GENERATION" = "$CUTOVER_TARGET_GENERATION" \
      && "$CUTOVER_DEPLOYMENT_PREVIOUS" = "$CUTOVER_PRIOR_GENERATION" ]] || return 1
    candidate_manifest="$(cutover_candidate_manifest "$CUTOVER_TARGET_GENERATION")" || return 1
    [[ "$(sha256sum "$candidate_manifest" | awk '{print $1}')" \
      = "$CUTOVER_TARGET_MANIFEST_SHA256" ]] || return 1
    if [[ "$candidate_manifest" == /srv/legal-mcp/uploads/* ]]; then
      cutover_validate_upload_manifest "$candidate_manifest"
    else
      cutover_validate_generation_manifest "$candidate_manifest" flat-int8 root legal-mcp 440
    fi
  else
    [[ "$CUTOVER_RETIREMENT_OUTCOME" = target \
      && "$pointer" = "$CUTOVER_TARGET_GENERATION" ]] || {
        echo 'cutover corpus journal disappeared before the target commit decision' >&2
        return 1
      }
    [[ "$(sha256sum "/srv/legal-mcp/generations/$CUTOVER_TARGET_GENERATION/generation.json" | awk '{print $1}')" \
      = "$CUTOVER_TARGET_MANIFEST_SHA256" ]] || return 1
  fi
  [[ "$(read_systemctl_activity "$SERVICE")" = inactive \
    && "$(read_systemctl_enablement caddy.service)" = disabled \
    && "$(read_systemctl_activity caddy.service)" = inactive \
    && "$(ufw_rule_state 80)" = absent \
    && "$(ufw_rule_state 443)" = absent ]] || return 1
  ordinary_require_listener_topology none
  container_state="$(podman_container_state australian-legal-mcp)" || return 1
  if [[ "$container_state" = present ]]; then
    running_image_id="$(canonical_image_id \
      "$(podman inspect australian-legal-mcp --format '{{.Image}}')")" || return 1
    [[ "$running_image_id" = "$CUTOVER_OLD_IMAGE_ID" \
      || "$running_image_id" = "$CUTOVER_TARGET_IMAGE_ID" ]] || return 1
  fi
  ufw_is_fail_closed
}

cutover_call_host_deploy() {
  LEGAL_MCP_HOST_TRANSACTION_LOCK_FD="$HOST_LOCK_FD" \
  LEGAL_MCP_FLAT_INT8_CUTOVER=1 \
    /usr/local/sbin/legal-mcp-host-deploy "$1" "$CUTOVER_TARGET_GENERATION"
}

cutover_install_pair_files() {
  local choice="$1" image_source quadlet_source template_source
  case "$choice" in
    saved)
      image_source="$TRANSACTION/saved-image"
      quadlet_source="$TRANSACTION/saved-quadlet"
      template_source="$TRANSACTION/saved-template"
      ;;
    target)
      image_source="$TRANSACTION/target-image"
      quadlet_source="$TRANSACTION/target-quadlet"
      template_source="$TRANSACTION/target-template"
      ;;
    *) return 1 ;;
  esac
  ordinary_atomic_install "$image_source" "$IMAGE_FILE" root root 600
  ordinary_atomic_install "$quadlet_source" "$QUADLET" root root 644
  ordinary_atomic_install "$template_source" "$TEMPLATE" root root 644
  systemctl daemon-reload
  cmp --silent "$image_source" "$IMAGE_FILE"
  cmp --silent "$quadlet_source" "$QUADLET"
  cmp --silent "$template_source" "$TEMPLATE"
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated ]]
}

cutover_require_image_id() {
  local image="$1" expected_id="$2" state resolved
  state="$(podman_image_state "$image")" || return 1
  [[ "$state" = present ]] || podman pull "$image"
  resolved="$(canonical_image_id \
    "$(podman image inspect "$image" --format '{{.Id}}')")" || return 1
  [[ "$resolved" = "$expected_id" ]] || {
    echo 'cutover image pin no longer resolves to its recorded image ID' >&2
    return 1
  }
}

cutover_verify_offline_pair() {
  local choice="$1" image image_id generation expected_manifest expected_hash format
  case "$choice" in
    saved)
      image="$CUTOVER_OLD_IMAGE"
      image_id="$CUTOVER_OLD_IMAGE_ID"
      generation="$CUTOVER_PRIOR_GENERATION"
      expected_manifest="$TRANSACTION/saved-sha256"
      expected_hash="$CUTOVER_PRIOR_MANIFEST_SHA256"
      format=arroy
      ;;
    target)
      image="$CUTOVER_TARGET_IMAGE"
      image_id="$CUTOVER_TARGET_IMAGE_ID"
      generation="$CUTOVER_TARGET_GENERATION"
      expected_manifest="$TRANSACTION/target-sha256"
      expected_hash="$CUTOVER_TARGET_MANIFEST_SHA256"
      format=flat-int8
      ;;
    *) return 1 ;;
  esac
  cutover_require_image_id "$image" "$image_id"
  [[ "$(</srv/legal-mcp/lifecycle/active-generation)" = "$generation" \
    && "$(sha256sum "/srv/legal-mcp/generations/$generation/generation.json" | awk '{print $1}')" \
      = "$expected_hash" ]] || return 1
  cutover_validate_generation_manifest \
    "/srv/legal-mcp/generations/$generation/generation.json" "$format" root legal-mcp 440
  local live_manifest
  live_manifest="$(mktemp /run/legal-mcp-cutover-live.XXXXXX)"
  ordinary_render_hash_manifest "$IMAGE_FILE" "$QUADLET" "$TEMPLATE" \
    "$RUNTIME_ENV" "$API_KEYS" "$CADDYFILE" "$TRANSACTION/saved-auth-ready" \
    /srv/legal-mcp/lifecycle/active-generation "$live_manifest"
  if ! cmp --silent "$live_manifest" "$expected_manifest"; then
    rm -f "$live_manifest"
    return 1
  fi
  rm -f "$live_manifest"
  podman run --rm --network=none --user=0:0 --read-only --cap-drop=all \
    --security-opt=no-new-privileges --pids-limit=256 --memory=6g --memory-swap=6g \
    --tmpfs=/tmp:rw,nodev,nosuid,noexec,size=64m,mode=1777 \
    --volume=/srv/legal-mcp:/var/lib/legal-mcp:ro,nodev,nosuid \
    "$image" verify --quiet >/dev/null
}

cutover_verify_running_constraints() {
  local expected_image_id="$1" image_id user root_read_only network port mounts caps
  image_id="$(canonical_image_id \
    "$(podman inspect australian-legal-mcp --format '{{.Image}}')")" || return 1
  user="$(podman inspect australian-legal-mcp --format '{{.Config.User}}')" || return 1
  root_read_only="$(podman inspect australian-legal-mcp --format '{{.HostConfig.ReadonlyRootfs}}')" || return 1
  network="$(podman inspect australian-legal-mcp --format '{{.HostConfig.NetworkMode}}')" || return 1
  port="$(podman port australian-legal-mcp 51235/tcp)" || return 1
  mounts="$(podman inspect australian-legal-mcp --format '{{json .Mounts}}')" || return 1
  caps="$(podman inspect australian-legal-mcp --format '{{json .EffectiveCaps}}')" || return 1
  [[ "$image_id" = "$expected_image_id" \
    && "$user" = 971:971 && "$root_read_only" = true && "$network" = bridge \
    && "$port" = 127.0.0.1:51235 && "$caps" = '[]' ]] || {
      echo 'running cutover container violates its exact image/user/network/capability contract' >&2
      return 1
    }
  python3 - "$mounts" <<'PY'
import json, sys
mounts = json.loads(sys.argv[1])
expected = {
    "/var/lib/legal-mcp/generations": ("/srv/legal-mcp/generations", False),
    "/var/lib/legal-mcp/lifecycle": ("/srv/legal-mcp/lifecycle", False),
    "/var/lib/legal-mcp/state": ("/srv/legal-mcp/state", True),
    "/run/secrets/legal-mcp-api-keys.json": ("/etc/legal-mcp/api-keys.json", False),
}
if len(mounts) != len(expected):
    raise SystemExit(1)
seen = {}
for item in mounts:
    destination = item.get("Destination")
    if destination in expected:
        if destination in seen:
            raise SystemExit(1)
        seen[destination] = (item.get("Source"), item.get("RW"))
if seen != expected:
    raise SystemExit(1)
PY
}

cutover_disarm_private_start() {
  cutover_path_is_absent "$CUTOVER_START_ARM" && return 0
  bootstrap_require_regular "$CUTOVER_START_ARM" root root 400 || return 1
  rm -f -- "$CUTOVER_START_ARM"
  sync -f /run/legal-mcp
  cutover_path_is_absent "$CUTOVER_START_ARM"
}

cutover_verify_dark_pair() {
  local expected_manifest="$1" expected_id="$2" expected_generation="$3"
  local manifest container_state running_image_id
  ordinary_require_static_live_metadata
  manifest="$(mktemp /run/legal-mcp-cutover-dark-live.XXXXXX)"
  ordinary_render_hash_manifest "$IMAGE_FILE" "$QUADLET" "$TEMPLATE" \
    "$RUNTIME_ENV" "$API_KEYS" "$CADDYFILE" "$TRANSACTION/saved-auth-ready" \
    /srv/legal-mcp/lifecycle/active-generation "$manifest"
  if ! cmp --silent "$manifest" "$expected_manifest"; then
    rm -f "$manifest"
    echo 'configured-dark cutover pair does not match its exact manifest' >&2
    return 1
  fi
  rm -f "$manifest"
  cutover_require_image_id "$(<"$IMAGE_FILE")" "$expected_id"
  [[ "$(</srv/legal-mcp/lifecycle/active-generation)" = "$expected_generation" \
    && "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(read_systemctl_activity "$SERVICE")" = inactive \
    && "$(read_systemctl_enablement caddy.service)" = disabled \
    && "$(read_systemctl_activity caddy.service)" = inactive \
    && "$(ufw_rule_state 80)" = absent \
    && "$(ufw_rule_state 443)" = absent ]] || return 1
  cutover_path_is_absent "$AUTH_READY"
  cutover_path_is_absent "$CUTOVER_START_ARM"
  ordinary_require_listener_topology none
  ufw_is_fail_closed
  container_state="$(podman_container_state australian-legal-mcp)" || return 1
  if [[ "$container_state" = present ]]; then
    running_image_id="$(canonical_image_id \
      "$(podman inspect australian-legal-mcp --format '{{.Image}}')")" || return 1
    [[ "$running_image_id" = "$expected_id" ]] || return 1
  fi
  cutover_validate_mount_contract
}

cutover_start_and_prove_pair() {
  local choice="$1" expected_id expected_generation expected_manifest
  case "$choice" in
    saved)
      expected_id="$CUTOVER_OLD_IMAGE_ID"
      expected_generation="$CUTOVER_PRIOR_GENERATION"
      expected_manifest="$TRANSACTION/saved-sha256"
      ;;
    target)
      expected_id="$CUTOVER_TARGET_IMAGE_ID"
      expected_generation="$CUTOVER_TARGET_GENERATION"
      expected_manifest="$TRANSACTION/target-sha256"
      ;;
    *) return 1 ;;
  esac
  TRANSACTION_GENERATION="$expected_generation"
  load_runtime_contract "$TRANSACTION/saved-runtime.env"
  [[ "$AUTH_MODE" = "$TRANSACTION_AUTH_MODE" \
    && "$EXTERNAL_URL" = "$TRANSACTION_EXTERNAL_URL" ]]
  [[ "$(read_systemctl_activity "$SERVICE")" = inactive \
    && "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(ufw_rule_state 80)" = absent \
    && "$(ufw_rule_state 443)" = absent \
    && "$(read_systemctl_activity caddy.service)" = inactive \
    && "$(read_systemctl_enablement caddy.service)" = disabled ]]
  cutover_arm_private_start
  systemctl start "$SERVICE"
  ordinary_verify_private_runtime "$expected_id"
  cutover_verify_running_constraints "$expected_id"
  systemctl stop "$SERVICE"
  [[ "$(read_systemctl_activity "$SERVICE")" = inactive ]]
  cutover_disarm_private_start
  cutover_verify_dark_pair "$expected_manifest" "$expected_id" "$expected_generation"
}

cutover_write_outcome() {
  local choice="$1" preparing="$TRANSACTION/retirement-outcome.preparing"
  [[ "$choice" = saved || "$choice" = target ]]
  cutover_validate_transaction "$TRANSACTION"
  if [[ "$CUTOVER_RETIREMENT_OUTCOME" = "$choice" ]]; then return 0; fi
  [[ "$CUTOVER_RETIREMENT_OUTCOME" = pending ]] || return 1
  cutover_path_is_absent "$preparing" || return 1
  install -o root -g root -m 0600 /dev/null "$preparing"
  printf '%s\n' "$choice" > "$preparing"
  chown root:root "$preparing"
  chmod 600 "$preparing"
  sync -f "$preparing"
  mv -fT "$preparing" "$TRANSACTION/retirement-outcome"
  sync -f "$TRANSACTION"
  cutover_validate_transaction "$TRANSACTION"
  [[ "$CUTOVER_RETIREMENT_OUTCOME" = "$choice" ]]
}

cutover_verify_committed_pair() {
  local choice="$1" expected_manifest expected_id expected_generation
  case "$choice" in
    saved)
      expected_manifest="$TRANSACTION/saved-sha256"
      expected_id="$CUTOVER_OLD_IMAGE_ID"
      expected_generation="$CUTOVER_PRIOR_GENERATION"
      [[ -e /srv/legal-mcp/lifecycle/.deployment-transaction \
        && ! -L /srv/legal-mcp/lifecycle/.deployment-transaction ]] || return 1
      cutover_read_deployment_journal
      [[ "$CUTOVER_DEPLOYMENT_GENERATION" = "$CUTOVER_TARGET_GENERATION" \
        && "$CUTOVER_DEPLOYMENT_PREVIOUS" = "$CUTOVER_PRIOR_GENERATION" \
        && "$CUTOVER_DEPLOYMENT_KIND" = ordinary \
        && "$CUTOVER_DEPLOYMENT_PHASE" = prepared ]] || return 1
      cutover_validate_generation_manifest \
        "/srv/legal-mcp/uploads/$CUTOVER_TARGET_GENERATION/generation.json" \
        flat-int8 legal-mcp-publisher legal-mcp-publisher 600
      [[ "$(sha256sum "/srv/legal-mcp/uploads/$CUTOVER_TARGET_GENERATION/generation.json" \
        | awk '{print $1}')" = "$CUTOVER_TARGET_MANIFEST_SHA256" ]]
      cutover_upload_authorization_matches
      ;;
    target)
      expected_manifest="$TRANSACTION/target-sha256"
      expected_id="$CUTOVER_TARGET_IMAGE_ID"
      expected_generation="$CUTOVER_TARGET_GENERATION"
      cutover_path_is_absent /srv/legal-mcp/lifecycle/.deployment-transaction
      cutover_path_is_absent /run/legal-mcp/authorized-upload
      ;;
    *) return 1 ;;
  esac
  TRANSACTION_GENERATION="$expected_generation"
  ordinary_read_transaction_probe_key
  cutover_verify_dark_pair "$expected_manifest" "$expected_id" "$expected_generation"
}

cutover_delete_retired_transaction() {
  delete_retired_image_directory "$1"
}

cutover_retire_transaction() {
  local choice="$1"
  cutover_validate_transaction "$TRANSACTION"
  [[ "$CUTOVER_RETIREMENT_OUTCOME" = "$choice" ]]
  cutover_verify_committed_pair "$choice"
  cutover_path_is_absent "$TRANSACTION_RETIRING"
  cutover_path_is_absent "$TRANSACTION_RETIRED"
  mv -T "$TRANSACTION" "$TRANSACTION_RETIRING"
  sync -f /etc/legal-mcp
  mv -T "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"
  sync -f /etc/legal-mcp
  cutover_delete_retired_transaction "$TRANSACTION_RETIRED"
}

cutover_restore_pair() {
  cutover_force_dark
  cutover_validate_recoverable_live_state
  cutover_install_pair_files saved
  cutover_call_host_deploy cutover-rollback >/dev/null
  cutover_verify_offline_pair saved
  cutover_start_and_prove_pair saved
  cutover_write_outcome saved
  cutover_call_host_deploy cutover-commit >/dev/null
  cutover_restore_upload_authorization
  cutover_verify_committed_pair saved
  cutover_retire_transaction saved
}

cutover_finish_target_pair() {
  cutover_force_dark
  cutover_validate_recoverable_live_state
  cutover_install_pair_files target
  if [[ -e /srv/legal-mcp/lifecycle/.deployment-transaction \
    && ! -L /srv/legal-mcp/lifecycle/.deployment-transaction ]]; then
    cutover_call_host_deploy cutover-activate >/dev/null
  else
    [[ "$CUTOVER_RETIREMENT_OUTCOME" = target \
      && "$(</srv/legal-mcp/lifecycle/active-generation)" \
        = "$CUTOVER_TARGET_GENERATION" ]] || return 1
  fi
  cutover_verify_offline_pair target
  cutover_start_and_prove_pair target
  cutover_write_outcome target
  if [[ -e /srv/legal-mcp/lifecycle/.deployment-transaction \
    && ! -L /srv/legal-mcp/lifecycle/.deployment-transaction ]]; then
    cutover_call_host_deploy cutover-commit >/dev/null
  fi
  cutover_verify_committed_pair target
  cutover_retire_transaction target
}

cutover_failure_rollback() {
  local status=$? recovery_status
  trap - ERR HUP INT TERM EXIT
  set +e
  (
    set -e
    cutover_validate_transaction "$TRANSACTION"
    ordinary_read_transaction_probe_key
    if [[ "$CUTOVER_RETIREMENT_OUTCOME" = target ]]; then
      cutover_finish_target_pair
    else
      cutover_restore_pair
    fi
  )
  recovery_status=$?
  set -e
  unset PROBE_API_KEY
  if [[ $recovery_status -ne 0 ]]; then
    echo 'flat-int8 cutover failed; service and ingress remain off and explicit recovery is required' >&2
    exit 1
  fi
  if [[ "$CUTOVER_RETIREMENT_OUTCOME" = target ]]; then
    echo 'flat-int8 cutover had committed and recovery completed the target pair' >&2
  else
    echo 'flat-int8 cutover rolled both generation and image/template back' >&2
  fi
  exit "$status"
}

cutover_capture_configured_dark_image_baseline() {
  local rendered image_state container_state running_image_id host
  local -a current_image
  ordinary_require_static_live_metadata
  cutover_path_is_absent "$AUTH_READY" || {
    echo 'flat-int8 cutover requires configured authentication with auth-ready absent' >&2
    return 1
  }
  EXPECTED_GENERATION="$(</srv/legal-mcp/lifecycle/active-generation)"
  [[ "$EXPECTED_GENERATION" =~ ^[0-9a-f]{64}$ ]]
  mapfile -t current_image < "$IMAGE_FILE"
  [[ ${#current_image[@]} -eq 1 ]]
  OLD_IMAGE="${current_image[0]}"
  [[ "$(stat -c '%s' "$IMAGE_FILE")" = "$(( ${#OLD_IMAGE} + 1 ))" \
    && "$OLD_IMAGE" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ \
    && "$NEW_IMAGE" != "$OLD_IMAGE" ]] || {
      echo 'current and target cutover image pins are malformed or identical' >&2
      return 1
    }
  [[ "$(grep -o '__IMAGE_DIGEST__' "$TEMPLATE" | wc -l)" = 1 ]]
  rendered="$(mktemp /run/legal-mcp-cutover-current-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$OLD_IMAGE|g" "$TEMPLATE" > "$rendered"
  if ! cmp --silent "$rendered" "$QUADLET"; then
    rm -f "$rendered"
    echo 'configured-dark image, template, and rendered Quadlet do not agree' >&2
    return 1
  fi
  rm -f "$rendered"
  image_state="$(podman_image_state "$OLD_IMAGE")" || return 1
  [[ "$image_state" = present ]] || {
    echo 'configured-dark rollback image is not present' >&2
    return 1
  }
  OLD_IMAGE_ID="$(canonical_image_id \
    "$(podman image inspect "$OLD_IMAGE" --format '{{.Id}}')")" || return 1
  container_state="$(podman_container_state australian-legal-mcp)" || return 1
  if [[ "$container_state" = present ]]; then
    running_image_id="$(canonical_image_id \
      "$(podman inspect australian-legal-mcp --format '{{.Image}}')")" || return 1
    [[ "$running_image_id" = "$OLD_IMAGE_ID" ]] || {
      echo 'configured-dark container is outside the rollback image identity' >&2
      return 1
    }
  fi
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(read_systemctl_activity "$SERVICE")" = inactive \
    && "$(read_systemctl_enablement caddy.service)" = disabled \
    && "$(read_systemctl_activity caddy.service)" = inactive \
    && "$(ufw_rule_state 80)" = absent \
    && "$(ufw_rule_state 443)" = absent ]] || {
      echo 'flat-int8 cutover requires the exact configured-dark service and ingress matrix' >&2
      return 1
    }
  ufw_is_fail_closed
  ordinary_require_listener_topology none
  host="${EXTERNAL_URL#https://}"
  host="${host%/mcp}"
  ordinary_validate_caddy_contract "$host"
  CADDY_ENABLEMENT=disabled
  CADDY_ACTIVITY=inactive
  UFW_80=absent
  UFW_443=absent
}

cutover_capture_baseline() {
  local candidate_manifest old_source old_title old_description old_licenses
  local old_digest old_binary_version authorization
  cutover_validate_mount_contract
  cutover_validate_template_contract "$ORDINARY_SOURCE_TEMPLATE"
  ordinary_require_static_live_metadata
  load_runtime_contract "$RUNTIME_ENV"
  read_probe_key
  cutover_capture_configured_dark_image_baseline
  [[ "$EXPECTED_GENERATION" = "$CUTOVER_EXPECTED_CURRENT_GENERATION" ]] || {
    echo 'live generation is not the explicitly expected Arroy v20 generation' >&2
    return 1
  }
  cutover_read_deployment_journal
  [[ "$CUTOVER_DEPLOYMENT_GENERATION" = "$CUTOVER_GENERATION" \
    && "$CUTOVER_DEPLOYMENT_PREVIOUS" = "$CUTOVER_EXPECTED_CURRENT_GENERATION" \
    && ( ( "$CUTOVER_DEPLOYMENT_KIND" = ordinary \
        && "$CUTOVER_DEPLOYMENT_PHASE" = prepared ) \
      || ( "$CUTOVER_DEPLOYMENT_KIND" = flat-int8-cutover \
        && "$CUTOVER_DEPLOYMENT_PHASE" = rolled-back ) ) ]] || {
      echo 'corpus deployment transaction is not the exact prepared flat-int8 target' >&2
      return 1
    }
  candidate_manifest="$(cutover_candidate_manifest "$CUTOVER_GENERATION")" || return 1
  if [[ "$CUTOVER_DEPLOYMENT_KIND" = ordinary ]]; then
    [[ "$candidate_manifest" == /srv/legal-mcp/uploads/* ]] || return 1
    cutover_validate_generation_manifest "$candidate_manifest" flat-int8 \
      legal-mcp-publisher legal-mcp-publisher 600
  else
    [[ "$candidate_manifest" == /srv/legal-mcp/generations/* ]] || return 1
    cutover_validate_generation_manifest "$candidate_manifest" flat-int8 root legal-mcp 440
  fi
  cutover_validate_generation_manifest \
    "/srv/legal-mcp/generations/$CUTOVER_EXPECTED_CURRENT_GENERATION/generation.json" \
    arroy root legal-mcp 440
  CUTOVER_PRIOR_MANIFEST_SHA256="$(sha256sum \
    "/srv/legal-mcp/generations/$CUTOVER_EXPECTED_CURRENT_GENERATION/generation.json" | awk '{print $1}')"
  CUTOVER_TARGET_MANIFEST_SHA256="$(sha256sum "$candidate_manifest" | awk '{print $1}')"
  [[ "$CUTOVER_PRIOR_MANIFEST_SHA256$CUTOVER_TARGET_MANIFEST_SHA256" \
    =~ ^[0-9a-f]{128}$ ]]
  authorization=/run/legal-mcp/authorized-upload
  CUTOVER_UPLOAD_AUTHORIZATION=absent
  if ! cutover_path_is_absent "$authorization"; then
    bootstrap_require_regular "$authorization" root legal-mcp-publisher 440
    [[ "$(<"$authorization")" = "$CUTOVER_GENERATION" ]] || {
      echo 'upload authorization belongs to a foreign generation' >&2
      return 1
    }
    CUTOVER_UPLOAD_AUTHORIZATION=present
  fi
  old_title="$(podman image inspect "$OLD_IMAGE" --format '{{index .Labels "org.opencontainers.image.title"}}')"
  old_description="$(podman image inspect "$OLD_IMAGE" --format '{{index .Labels "org.opencontainers.image.description"}}')"
  old_source="$(podman image inspect "$OLD_IMAGE" --format '{{index .Labels "org.opencontainers.image.source"}}')"
  CUTOVER_OLD_IMAGE_VERSION="$(podman image inspect "$OLD_IMAGE" --format '{{index .Labels "org.opencontainers.image.version"}}')"
  CUTOVER_OLD_IMAGE_REVISION="$(podman image inspect "$OLD_IMAGE" --format '{{index .Labels "org.opencontainers.image.revision"}}')"
  old_licenses="$(podman image inspect "$OLD_IMAGE" --format '{{index .Labels "org.opencontainers.image.licenses"}}')"
  old_digest="$(podman image inspect "$OLD_IMAGE" --format '{{.Digest}}')"
  [[ "$old_title" = 'Australian Legal MCP' \
    && "$old_description" = 'Source-grounded Australian legal MCP server' \
    && "$old_source" = https://github.com/gunba/australian-legal-mcp \
    && "$CUTOVER_OLD_IMAGE_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ \
    && "$CUTOVER_OLD_IMAGE_REVISION" =~ ^[0-9a-f]{40}$ \
    && "$old_licenses" = MIT && "$old_digest" = "${OLD_IMAGE##*@}" ]] || {
      echo 'live Arroy image labels are not exact enough for coordinated rollback' >&2
      return 1
    }
  old_binary_version="$(podman run --rm --network=none --read-only --cap-drop=all \
    --security-opt=no-new-privileges "$OLD_IMAGE" --version)"
  [[ "$old_binary_version" = "legal-mcp $CUTOVER_OLD_IMAGE_VERSION" ]] || {
    echo 'live Arroy image binary does not match its exact rollback labels' >&2
    return 1
  }
  podman run --rm --network=none --read-only --cap-drop=all \
    --security-opt=no-new-privileges "$OLD_IMAGE" verify-runtime \
    | grep -Fq '"onnx_runtime_ready":true'
}

cutover_require_flat_only_target() {
  [[ "$(</srv/legal-mcp/lifecycle/active-generation)" \
    = "$CUTOVER_EXPECTED_CURRENT_GENERATION" ]] || return 1
  if podman run --rm --network=none --user=0:0 --read-only --cap-drop=all \
    --security-opt=no-new-privileges --pids-limit=256 --memory=6g --memory-swap=6g \
    --tmpfs=/tmp:rw,nodev,nosuid,noexec,size=64m,mode=1777 \
    --volume=/srv/legal-mcp:/var/lib/legal-mcp:ro,nodev,nosuid \
    "$NEW_IMAGE" verify --quiet >/dev/null 2>&1; then
    echo 'target image accepted the live Arroy generation and is not flat-only' >&2
    return 1
  fi
}

cutover_discard_incomplete_preparation() {
  local path="$1"
  require_image_transaction_directory "$path"
  rm -rf --one-file-system -- "$path"
  cutover_path_is_absent "$path"
  sync -f /etc/legal-mcp
}

cutover_verify_unchanged_configured_dark_baseline() {
  ordinary_require_static_live_metadata
  load_runtime_contract "$RUNTIME_ENV"
  read_probe_key
  cutover_capture_configured_dark_image_baseline
}

cutover_complete_retirement() {
  local directory="$1" choice payload_state saved_transaction="$TRANSACTION"
  if [[ "$directory" = "$TRANSACTION_RETIRED" ]]; then
    payload_state="$(retired_image_payload_state "$TRANSACTION_RETIRED" \
      kind target-version target-revision updater-sha256 retirement-outcome \
      release-sha256 saved-sha256 target-sha256 saved-metadata target-metadata state \
      saved-image saved-quadlet saved-template saved-runtime.env saved-api-keys.json \
      saved-Caddyfile saved-auth-ready saved-active-generation saved-deployment-journal \
      saved-upload-authorization target-image target-quadlet target-template \
      target-active-generation)"
    if [[ "$payload_state" = complete ]]; then
      TRANSACTION="$TRANSACTION_RETIRED"
      cutover_validate_transaction "$TRANSACTION"
      choice="$CUTOVER_RETIREMENT_OUTCOME"
      [[ "$choice" = saved || "$choice" = target ]]
      if [[ "$choice" = saved ]]; then cutover_restore_upload_authorization; fi
      ordinary_read_transaction_probe_key
      cutover_verify_committed_pair "$choice"
    fi
    delete_retired_image_directory "$TRANSACTION_RETIRED"
    TRANSACTION="$saved_transaction"
    return 0
  fi
  TRANSACTION="$directory"
  cutover_validate_transaction "$TRANSACTION"
  choice="$CUTOVER_RETIREMENT_OUTCOME"
  [[ "$choice" = saved || "$choice" = target ]]
  if [[ "$choice" = saved ]]; then cutover_restore_upload_authorization; fi
  ordinary_read_transaction_probe_key
  cutover_verify_committed_pair "$choice"
  if [[ "$directory" = "$TRANSACTION_RETIRING" ]]; then
    cutover_path_is_absent "$TRANSACTION_RETIRED"
    mv -T "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"
    sync -f /etc/legal-mcp
    TRANSACTION="$TRANSACTION_RETIRED"
    cutover_validate_transaction "$TRANSACTION"
    [[ "$CUTOVER_RETIREMENT_OUTCOME" = "$choice" ]]
    cutover_verify_committed_pair "$choice"
  fi
  delete_retired_image_directory "$TRANSACTION"
  TRANSACTION="$saved_transaction"
}

cutover_recover_pending_state() {
  local path outcome
  if ! cutover_path_is_absent "$CUTOVER_TRANSACTION_PREPARING" \
    && ! cutover_path_is_absent "$CUTOVER_TRANSACTION_PREPARING_RETIRED"; then
    echo 'flat-int8 cutover preparation has conflicting recovery states' >&2
    return 1
  fi
  if ! cutover_path_is_absent "$TRANSACTION_RETIRING" \
    && ! cutover_path_is_absent "$TRANSACTION_RETIRED"; then
    echo 'flat-int8 cutover retirement has conflicting recovery states' >&2
    return 1
  fi
  if ! cutover_path_is_absent "$CUTOVER_TRANSACTION_PREPARING"; then
    cutover_discard_incomplete_preparation "$CUTOVER_TRANSACTION_PREPARING"
    cutover_verify_unchanged_configured_dark_baseline
    echo 'interrupted flat-int8 cutover preparation discarded; prior pair remains configured-dark'
    return 0
  fi
  if ! cutover_path_is_absent "$CUTOVER_TRANSACTION_PREPARING_RETIRED"; then
    cutover_discard_incomplete_preparation "$CUTOVER_TRANSACTION_PREPARING_RETIRED"
    cutover_verify_unchanged_configured_dark_baseline
    echo 'interrupted flat-int8 cutover preparation retirement completed; prior pair remains configured-dark'
    return 0
  fi
  if ! cutover_path_is_absent "$TRANSACTION_RETIRING"; then
    cutover_complete_retirement "$TRANSACTION_RETIRING"
    echo 'interrupted flat-int8 cutover commit retirement completed'
    return 0
  fi
  if ! cutover_path_is_absent "$TRANSACTION_RETIRED"; then
    cutover_complete_retirement "$TRANSACTION_RETIRED"
    echo 'interrupted flat-int8 cutover commit retirement completed'
    return 0
  fi
  path="$TRANSACTION"
  cutover_path_is_absent "$path" && {
    echo 'no flat-int8 cutover transaction exists' >&2
    return 1
  }
  cutover_validate_transaction "$path"
  outcome="$CUTOVER_RETIREMENT_OUTCOME"
  ordinary_read_transaction_probe_key
  if [[ "$outcome" = target ]]; then
    cutover_finish_target_pair
    echo 'interrupted flat-int8 cutover recovered the committed target pair'
  else
    cutover_restore_pair
    echo 'interrupted flat-int8 cutover rolled back both generation and image/template'
  fi
}

run_flat_int8_cutover() {
  [[ "$CUTOVER_GENERATION" =~ ^[0-9a-f]{64}$ \
    && "$CUTOVER_EXPECTED_CURRENT_GENERATION" =~ ^[0-9a-f]{64}$ \
    && "$CUTOVER_GENERATION" != "$CUTOVER_EXPECTED_CURRENT_GENERATION" \
    && "$NEW_IMAGE" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ \
    && "$EXPECTED_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ \
    && -n "$SOURCE_TEMPLATE" ]] || usage
  cutover_require_no_foreign_transaction false
  ordinary_load_release_bundle "$SOURCE_TEMPLATE" "$EXPECTED_VERSION"
  cutover_capture_baseline
  podman pull "$NEW_IMAGE"
  verify_image_runtime "$NEW_IMAGE" "$ORDINARY_VERSION" "$ORDINARY_REVISION"
  cutover_require_flat_only_target
  TARGET_IMAGE_ID="$(canonical_image_id \
    "$(podman image inspect "$NEW_IMAGE" --format '{{.Id}}')")"
  CUTOVER_TARGET_GENERATION="$CUTOVER_GENERATION"
  CUTOVER_PRIOR_GENERATION="$CUTOVER_EXPECTED_CURRENT_GENERATION"
  cutover_create_transaction
  trap cutover_failure_rollback ERR HUP INT TERM EXIT
  cutover_validate_transaction "$TRANSACTION"
  cutover_force_dark
  cutover_validate_recoverable_live_state
  cutover_install_pair_files target
  cutover_call_host_deploy cutover-activate >/dev/null
  cutover_verify_offline_pair target
  cutover_start_and_prove_pair target
  cutover_write_outcome target
  cutover_call_host_deploy cutover-commit >/dev/null
  cutover_verify_committed_pair target
  trap - ERR HUP INT TERM EXIT
  cutover_retire_transaction target
  unset PROBE_API_KEY
  echo "flat-int8 cutover committed generation $CUTOVER_TARGET_GENERATION with $CUTOVER_TARGET_IMAGE"
}

if [[ "$FLAT_INT8_CUTOVER" = false \
  && -n "$CUTOVER_GENERATION$CUTOVER_EXPECTED_CURRENT_GENERATION" ]]; then
  usage
fi

if [[ "$FLAT_INT8_CUTOVER" = true ]]; then
  [[ "$BOOTSTRAP_EMPTY_HOST" = false ]] || usage
  for command_name in awk blkid caddy cmp curl find findmnt flock getfacl grep \
    install mktemp mv podman python3 readlink sha256sum ss stat sync systemctl \
    ufw visudo xfs_info; do
    command -v "$command_name" >/dev/null || {
      echo "missing flat-int8 cutover dependency: $command_name" >&2
      exit 1
    }
  done
  cutover_require_launcher_context
  if [[ "$RECOVER" = true ]]; then
    [[ -z "$NEW_IMAGE$EXPECTED_VERSION$SOURCE_TEMPLATE$CUTOVER_GENERATION$CUTOVER_EXPECTED_CURRENT_GENERATION" ]] \
      || usage
    ordinary_load_release_bundle '' ''
    cutover_require_no_foreign_transaction true
    cutover_recover_pending_state
  else
    run_flat_int8_cutover
  fi
  exit 0
fi

if [[ "$BOOTSTRAP_EMPTY_HOST" = true ]]; then
  for command_name in awk blkid cmp find findmnt getfacl id podman python3 \
    readlink sha256sum ss stat sync systemctl ufw visudo xfs_info; do
    command -v "$command_name" >/dev/null || {
      echo "missing empty-host image dependency: $command_name" >&2
      exit 1
    }
  done
  finalize_image_transaction_retirement
  finalize_image_preparation_retirement
  if [[ "$RECOVER" = true ]]; then
    [[ -z "$NEW_IMAGE$EXPECTED_VERSION$SOURCE_TEMPLATE" ]] || usage
    bootstrap_force_off
    [[ -d "$TRANSACTION" && ! -L "$TRANSACTION" ]] || {
      if [[ "$IMAGE_PREPARATION_WAS_PENDING" = true ]]; then
        echo 'interrupted empty-host image preparation discarded; service and ingress remain off'
        exit 0
      fi
      if [[ "$IMAGE_RETIREMENT_WAS_PENDING" = true ]]; then
        echo 'interrupted empty-host image transaction retirement completed; service and ingress remain off'
        exit 0
      fi
      echo 'no safe empty-host image transaction exists' >&2
      exit 1
    }
    bootstrap_require_regular "$TRANSACTION/target-version" root root 600
    recovery_version="$(<"$TRANSACTION/target-version")"
    [[ "$recovery_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
      echo 'empty-host image transaction version is malformed' >&2
      exit 1
    }
    recovery_script="$(readlink -f "${BASH_SOURCE[0]}")"
    bootstrap_require_release_file "$recovery_script" true
    recovery_template="$(dirname "$recovery_script")/legal-mcp.container.template"
    bootstrap_load_bundle "$recovery_template" "$recovery_version"
    bootstrap_recover_transaction
    echo 'interrupted empty-host image cutover rolled back; service and ingress remain off'
    exit 0
  fi
  run_bootstrap_empty_host_update
  exit 0
fi

for command_name in awk caddy cmp curl find flock getfacl grep install mktemp mv \
  podman python3 readlink sha256sum ss stat sync systemctl ufw visudo; do
  command -v "$command_name" >/dev/null || {
    echo "missing image update dependency: $command_name" >&2
    exit 1
  }
done

if [[ "$RECOVER" = true ]]; then
  [[ -z "$NEW_IMAGE$EXPECTED_VERSION$SOURCE_TEMPLATE" ]] || usage
  ordinary_load_release_bundle '' ''
  ordinary_require_no_foreign_transaction
  ordinary_recover_pending_state
  exit 0
fi

[[ "$NEW_IMAGE" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ \
  && "$EXPECTED_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ \
  && -n "$SOURCE_TEMPLATE" ]] || usage
ordinary_load_release_bundle "$SOURCE_TEMPLATE" "$EXPECTED_VERSION"
ordinary_require_no_foreign_transaction
for path in "$TRANSACTION_PREPARING" "$TRANSACTION_PREPARING_RETIRED" \
  "$TRANSACTION" "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"; do
  image_path_is_absent "$path" || {
    echo 'an image transaction already exists; recover it with this exact release bundle first' >&2
    exit 1
  }
done
ordinary_require_live_metadata
load_runtime_contract "$RUNTIME_ENV"
read_probe_key
ordinary_capture_baseline

podman pull "$NEW_IMAGE"
verify_image_runtime "$NEW_IMAGE" "$ORDINARY_VERSION" "$ORDINARY_REVISION"
TARGET_IMAGE_ID="$(canonical_image_id \
  "$(podman image inspect "$NEW_IMAGE" --format '{{.Id}}')")"
podman run --rm --network=none --user=0:0 --read-only --cap-drop=all \
  --security-opt=no-new-privileges --pids-limit=256 --memory=6g --memory-swap=6g \
  --tmpfs=/tmp:rw,nodev,nosuid,noexec,size=64m,mode=1777 \
  --volume=/srv/legal-mcp:/var/lib/legal-mcp:ro,nodev,nosuid \
  "$NEW_IMAGE" verify --quiet >/dev/null

ordinary_create_transaction
trap ordinary_rollback ERR HUP INT TERM EXIT
close_ingress
ordinary_atomic_install "$TRANSACTION/target-image" "$IMAGE_FILE" root root 600
ordinary_atomic_install "$TRANSACTION/target-quadlet" "$QUADLET" root root 644
ordinary_atomic_install "$TRANSACTION/target-template" "$TEMPLATE" root root 644
systemctl daemon-reload
[[ "$(read_systemctl_enablement "$SERVICE")" = generated ]]
systemctl restart "$SERVICE"
ordinary_verify_private_runtime "$TRANSACTION_TARGET_IMAGE_ID"
ordinary_restore_caddy_and_ufw
ordinary_verify_final_state "$TRANSACTION/target-sha256" "$TRANSACTION_TARGET_IMAGE_ID"

trap - ERR HUP INT TERM EXIT
ordinary_retire_transaction target
unset PROBE_API_KEY
echo "container image updated to $NEW_IMAGE"
