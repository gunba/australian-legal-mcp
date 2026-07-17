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

For the one empty-host software cutover before the first corpus activation:
  sudo update-image.sh --bootstrap-empty-host \
    --image ghcr.io/gunba/australian-legal-mcp@sha256:DIGEST \
    --version X.Y.Z --template PATH
To roll back an interrupted empty-host cutover, use the same release bundle:
  sudo update-image.sh --recover --bootstrap-empty-host

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
TRANSACTION_PREPARING=${TRANSACTION}.preparing
TRANSACTION_PREPARING_RETIRED=${TRANSACTION}.preparing-retired
TRANSACTION_RETIRING=${TRANSACTION}.retiring
TRANSACTION_RETIRED=${TRANSACTION}.retired
SERVICE=legal-mcp.service
NEW_IMAGE=''
EXPECTED_VERSION=''
SOURCE_TEMPLATE=''
RECOVER=false
BOOTSTRAP_EMPTY_HOST=false
IMAGE_RETIREMENT_WAS_PENDING=false
IMAGE_PREPARATION_WAS_PENDING=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --image) NEW_IMAGE="${2:-}"; shift 2 ;;
    --version) EXPECTED_VERSION="${2:-}"; shift 2 ;;
    --template) SOURCE_TEMPLATE="${2:-}"; shift 2 ;;
    --recover) RECOVER=true; shift ;;
    --bootstrap-empty-host) BOOTSTRAP_EMPTY_HOST=true; shift ;;
    *) usage ;;
  esac
done

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
  if report="$(ufw status)"; then
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
  local port state enabled activity
  systemctl disable --now caddy.service >/dev/null 2>&1 || return 1
  for port in 80 443; do
    state="$(ufw_rule_state "$port")" || return 1
    if [[ "$state" = present ]]; then
      ufw --force delete allow "$port/tcp" >/dev/null 2>&1 || return 1
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
  [[ "$(grep -o '__IMAGE_DIGEST__' "$template" | wc -l)" = 1 ]] || {
    echo 'bootstrap Quadlet template has an invalid image placeholder' >&2
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
  local deploy_sha publisher_sha sudoers_sha expected_policy
  local -a marker
  bootstrap_require_regular /etc/legal-mcp/host-tools root root 444
  bootstrap_require_acl /etc/legal-mcp/host-tools $'user::r--\ngroup::r--\nother::r--'
  mapfile -t marker < /etc/legal-mcp/host-tools
  [[ ${#marker[@]} -eq 6 && "${marker[0]}" = LEGAL_MCP_HOST_TOOLS_V1 \
    && "${marker[1]}" = "VERSION=$BOOTSTRAP_VERSION" \
    && "${marker[2]}" = "SOURCE_COMMIT=$BOOTSTRAP_REVISION" \
    && "${marker[3]}" =~ ^HOST_DEPLOY_SHA256=([0-9a-f]{64})$ ]] || {
    echo 'installed host-tool marker does not match this release' >&2
    return 1
  }
  deploy_sha="${BASH_REMATCH[1]}"
  [[ "${marker[4]}" =~ ^PUBLISHER_COMMAND_SHA256=([0-9a-f]{64})$ ]] || return 1
  publisher_sha="${BASH_REMATCH[1]}"
  [[ "${marker[5]}" =~ ^SUDOERS_SHA256=([0-9a-f]{64})$ ]] || return 1
  sudoers_sha="${BASH_REMATCH[1]}"
  bootstrap_require_regular /usr/local/sbin/legal-mcp-host-deploy root root 755
  bootstrap_require_regular /usr/local/sbin/legal-mcp-publisher-command root root 755
  bootstrap_require_regular /etc/sudoers.d/legal-mcp-publisher root root 440
  [[ "$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}')" = "$deploy_sha" \
    && "$(sha256sum /usr/local/sbin/legal-mcp-publisher-command | awk '{print $1}')" = "$publisher_sha" \
    && "$(sha256sum /etc/sudoers.d/legal-mcp-publisher | awk '{print $1}')" = "$sudoers_sha" \
    && "$(sha256sum "$BOOTSTRAP_BUNDLE_ROOT/scripts/legal-mcp-host-deploy" | awk '{print $1}')" = "$deploy_sha" \
    && "$(sha256sum "$BOOTSTRAP_BUNDLE_ROOT/scripts/legal-mcp-publisher-command" | awk '{print $1}')" = "$publisher_sha" ]] || {
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
  bootstrap_directory_contains_only /srv/legal-mcp/lifecycle LOCK || {
    echo 'empty-host lifecycle contains unexpected state' >&2
    return 1
  }
  if ! bootstrap_path_is_absent /etc/legal-mcp/.auth-transaction \
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

restore_unit_enablement() {
  local unit="$1" flag="$2" state
  if [[ -e "$TRANSACTION/$flag" ]]; then
    if [[ "$unit" = "$SERVICE" ]]; then
      state="$(read_systemctl_enablement "$unit")" || return 1
      [[ "$state" = generated ]] || {
        echo 'restored Quadlet service is not generated' >&2
        return 1
      }
    else
      systemctl enable "$unit" >/dev/null
      state="$(read_systemctl_enablement "$unit")" || return 1
      [[ "$state" = enabled ]] || return 1
    fi
  else
    systemctl disable "$unit" >/dev/null
    state="$(read_systemctl_enablement "$unit")" || return 1
    [[ "$state" = disabled ]] || return 1
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
  local old_image_state
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
    old_image_state="$(podman_image_state "$old_image")" || return 1
    if [[ "$old_image_state" = absent ]]; then
      podman pull "$old_image"
    fi
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
  retire_image_transaction
}

if [[ "$BOOTSTRAP_EMPTY_HOST" = true ]]; then
  for command_name in awk blkid cmp find findmnt getfacl id podman python3 \
    readlink sha256sum ss stat sync systemctl ufw visudo xfs_info; do
    command -v "$command_name" >/dev/null || {
      echo "missing empty-host image dependency: $command_name" >&2
      exit 1
    }
  done
fi

finalize_image_transaction_retirement
finalize_image_preparation_retirement

if [[ "$RECOVER" = true && "$BOOTSTRAP_EMPTY_HOST" = true ]]; then
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

if [[ "$BOOTSTRAP_EMPTY_HOST" = true ]]; then
  [[ "$RECOVER" = false ]] || usage
  run_bootstrap_empty_host_update
  exit 0
fi

if [[ "$RECOVER" = true ]]; then
  [[ -z "$NEW_IMAGE$EXPECTED_VERSION$SOURCE_TEMPLATE" ]] || usage
  if image_path_is_absent "$TRANSACTION"; then
    if [[ "$IMAGE_PREPARATION_WAS_PENDING" = true ]]; then
      echo 'interrupted image preparation discarded'
      exit 0
    fi
    if [[ "$IMAGE_RETIREMENT_WAS_PENDING" = true ]]; then
      echo 'interrupted image transaction retirement completed'
      exit 0
    fi
    echo 'no image transaction exists' >&2
    exit 1
  fi
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
image_path_is_absent /srv/legal-mcp/lifecycle/.deployment-transaction || {
  echo 'a corpus deployment transaction must be completed first' >&2
  exit 1
}
image_path_is_absent /etc/legal-mcp/.auth-transaction || {
  echo 'an authentication transaction must be recovered first' >&2
  exit 1
}
if ! image_path_is_absent /etc/legal-mcp/.host-tools-transaction.preparing \
  || ! image_path_is_absent /etc/legal-mcp/.host-tools-transaction \
  || ! image_path_is_absent /etc/legal-mcp/.host-tools-transaction.retiring \
  || ! image_path_is_absent /etc/legal-mcp/.host-tools-transaction.rollback-retiring \
  || ! image_path_is_absent /etc/legal-mcp/.host-tools-transaction.rollback-retired \
  || ! image_path_is_absent /etc/legal-mcp/.host-tools-transaction.publisher-restore; then
  echo 'a host-tool transaction must be recovered first' >&2
  exit 1
fi
image_path_is_absent "$TRANSACTION" || {
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
service_enablement="$(read_systemctl_enablement "$SERVICE")" || exit 1
service_activity="$(read_systemctl_activity "$SERVICE")" || exit 1
[[ "$service_enablement" = generated && "$service_activity" = active ]] || {
  echo 'image updates require the generated legal-mcp service to be active' >&2
  exit 1
}
caddy_enablement="$(read_systemctl_enablement caddy.service)" || exit 1
caddy_activity="$(read_systemctl_activity caddy.service)" || exit 1
[[ "$caddy_enablement" = enabled || "$caddy_enablement" = disabled ]] || {
  echo 'Caddy enablement is outside the supported host contract' >&2
  exit 1
}
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

transaction_tmp="$TRANSACTION_PREPARING"
install -d -o root -g root -m 0700 "$transaction_tmp"
cp --preserve=mode,ownership,timestamps "$IMAGE_FILE" "$transaction_tmp/image"
cp --preserve=mode,ownership,timestamps "$QUADLET" "$transaction_tmp/legal-mcp.container"
cp --preserve=mode,ownership,timestamps "$TEMPLATE" "$transaction_tmp/legal-mcp.container.template"
cp --preserve=mode,ownership,timestamps "$RUNTIME_ENV" "$transaction_tmp/runtime.env"
printf '%s\n' "$EXPECTED_GENERATION" > "$transaction_tmp/expected-generation"
touch "$transaction_tmp/service-was-enabled" "$transaction_tmp/service-was-active"
if [[ "$caddy_enablement" = enabled ]]; then touch "$transaction_tmp/caddy-was-enabled"; fi
if [[ "$caddy_activity" = active ]]; then touch "$transaction_tmp/caddy-was-active"; fi
port_80_open="$(ufw_rule_state 80)" || {
  retire_image_directory_for_deletion "$transaction_tmp" "$TRANSACTION_PREPARING_RETIRED"
  exit 1
}
port_443_open="$(ufw_rule_state 443)" || {
  retire_image_directory_for_deletion "$transaction_tmp" "$TRANSACTION_PREPARING_RETIRED"
  exit 1
}
[[ "$port_80_open" = "$port_443_open" ]] || {
  echo 'UFW 80/443 state is inconsistent; refusing image update' >&2
  retire_image_directory_for_deletion "$transaction_tmp" "$TRANSACTION_PREPARING_RETIRED"
  exit 1
}
if [[ "$port_80_open" = present ]]; then
  [[ -e "$transaction_tmp/caddy-was-active" \
    && -e "$transaction_tmp/service-was-active" ]] || {
    echo 'public UFW ingress is open while Caddy or legal-mcp is inactive' >&2
    retire_image_directory_for_deletion "$transaction_tmp" "$TRANSACTION_PREPARING_RETIRED"
    exit 1
  }
  touch "$transaction_tmp/public-was-open"
fi
sync -f "$transaction_tmp"
mv -T "$transaction_tmp" "$TRANSACTION"
sync -f /etc/legal-mcp

rollback() {
  local status=$? recovery_status
  trap - ERR HUP INT TERM EXIT
  set +e
  (
    set -e
    recover_transaction
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
retire_image_transaction
unset PROBE_API_KEY
echo "container image updated to $NEW_IMAGE"
