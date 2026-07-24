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

For an explicit incompatible image and prepared-generation transition:
  sudo /usr/local/sbin/legal-mcp-update-image --pair-cutover \
    --generation TARGET_GENERATION \
    --expected-current-generation CURRENT_GENERATION \
    --image ghcr.io/gunba/australian-legal-mcp@sha256:DIGEST \
    --version X.Y.Z --template PATH [--from-public]
For an explicit transition back to a retained installed image/generation pair:
  sudo /usr/local/sbin/legal-mcp-update-image --pair-rollback \
    --generation RETAINED_GENERATION \
    --expected-current-generation CURRENT_GENERATION \
    --image ghcr.io/gunba/australian-legal-mcp@sha256:DIGEST \
    --version X.Y.Z --template PATH [--from-public]
Recover either interrupted pair operation through the installed launcher:
  sudo /usr/local/sbin/legal-mcp-update-image --recover --pair-cutover

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
TRANSACTION_PREPARING_DELETION=${TRANSACTION}.preparing-deletion
TRANSACTION_RETIRING=${TRANSACTION}.retiring
TRANSACTION_RETIRED=${TRANSACTION}.retired
TRANSACTION_DELETION=${TRANSACTION}.deletion
SERVICE=legal-mcp.service
NEW_IMAGE=''
EXPECTED_VERSION=''
SOURCE_TEMPLATE=''
RECOVER=false
BOOTSTRAP_EMPTY_HOST=false
PAIR_CUTOVER=false
PAIR_ROLLBACK=false
PAIR_FROM_PUBLIC=false
PAIR_GENERATION=''
PAIR_EXPECTED_CURRENT_GENERATION=''
PAIR_PERMIT=/run/legal-mcp/pair-cutover-starting
PAIR_START_ARM=/run/legal-mcp/pair-cutover-start-armed
PAIR_VERIFY_ROOT=/run/legal-mcp/pair-verification
PAIR_TRANSACTION_BUILD=/etc/legal-mcp/.pair-transaction-build
IMAGE_RETIREMENT_WAS_PENDING=false
IMAGE_PREPARATION_WAS_PENDING=false
PAIR_BUILD_WAS_PENDING=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --image) NEW_IMAGE="${2:-}"; shift 2 ;;
    --version) EXPECTED_VERSION="${2:-}"; shift 2 ;;
    --template) SOURCE_TEMPLATE="${2:-}"; shift 2 ;;
    --recover) RECOVER=true; shift ;;
    --bootstrap-empty-host) BOOTSTRAP_EMPTY_HOST=true; shift ;;
    --pair-cutover) PAIR_CUTOVER=true; shift ;;
    --pair-rollback) PAIR_ROLLBACK=true; shift ;;
    --generation) PAIR_GENERATION="${2:-}"; shift 2 ;;
    --expected-current-generation) PAIR_EXPECTED_CURRENT_GENERATION="${2:-}"; shift 2 ;;
    --from-public) PAIR_FROM_PUBLIC=true; shift ;;
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
  local expected="$1" deadline=$((SECONDS + 600)) activity
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

require_known_image_transaction_states() {
  local path
  while IFS= read -r -d '' path; do
    case "$path" in
      /etc/legal-mcp/.image-transaction|\
      /etc/legal-mcp/.image-transaction.preparing|\
      /etc/legal-mcp/.image-transaction.preparing-retired|\
      /etc/legal-mcp/.image-transaction.preparing-deletion|\
      /etc/legal-mcp/.image-transaction.retiring|\
      /etc/legal-mcp/.image-transaction.retired|\
      /etc/legal-mcp/.image-transaction.deletion) ;;
      *)
        echo "unknown image transaction state must be reviewed explicitly: $path" >&2
        return 1
        ;;
    esac
  done < <(find /etc/legal-mcp -mindepth 1 -maxdepth 1 \
    -name '.image-transaction*' -print0)
}

require_image_transaction_directory() {
  local path="$1"
  [[ -d "$path" && ! -L "$path" \
    && "$(stat -c '%U:%G:%a' "$path")" = root:root:700 ]] || {
    echo "unsafe image transaction directory: $path" >&2
    return 1
  }
}

validate_image_deletion_marker() {
  local marker="$1" expected_kind="$2"
  bootstrap_require_regular "$marker" root root 600 || return 1
  [[ "$(<"$marker")" = "$expected_kind" ]] || {
    echo "image deletion marker has missing, corrupt, or foreign identity: $marker" >&2
    return 1
  }
}

delete_owned_image_transaction_directory() {
  local path="$1" marker="$2" expected_kind="$3" found victim
  if ! image_path_is_absent "$marker"; then
    validate_image_deletion_marker "$marker" "$expected_kind"
    if ! image_path_is_absent "$path"; then
      require_image_transaction_directory "$path"
      found="$(find "$path" -mindepth 1 -maxdepth 1 -print -quit)"
      [[ -z "$found" ]] || {
        echo "image deletion marker conflicts with non-empty state: $path" >&2
        return 1
      }
      rmdir "$path"
      sync -f /etc/legal-mcp
    fi
    rm -f -- "$marker"
    image_path_is_absent "$marker" || return 1
    sync -f /etc/legal-mcp
    return 0
  fi

  require_image_transaction_directory "$path"
  bootstrap_require_regular "$path/kind" root root 600 || return 1
  [[ "$(<"$path/kind")" = "$expected_kind" ]] || {
    echo "image transaction deletion refuses foreign owner: $path" >&2
    return 1
  }
  while true; do
    victim="$(find "$path" -mindepth 1 -maxdepth 1 ! -name kind -print -quit)"
    [[ -n "$victim" ]] || break
    rm -rf --one-file-system -- "$victim"
    image_path_is_absent "$victim" || return 1
    sync -f "$path"
  done
  image_path_is_absent "$marker" || return 1
  mv -T "$path/kind" "$marker"
  sync -f /etc/legal-mcp
  validate_image_deletion_marker "$marker" "$expected_kind"
  rmdir "$path"
  sync -f /etc/legal-mcp
  rm -f -- "$marker"
  image_path_is_absent "$marker" || return 1
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
  local path="$1" retired_path="$2" marker="$3" expected_kind="$4" found
  image_path_is_absent "$retired_path" || {
    echo "image deletion retirement already exists: $retired_path" >&2
    return 1
  }
  image_path_is_absent "$marker" || {
    echo "image deletion marker already exists: $marker" >&2
    return 1
  }
  require_image_transaction_directory "$path"
  found="$(find "$path" -mindepth 1 -maxdepth 1 -print -quit)"
  if [[ -z "$found" ]]; then
    rmdir "$path"
    sync -f /etc/legal-mcp
    return 0
  fi
  bootstrap_require_regular "$path/kind" root root 600 || return 1
  [[ "$(<"$path/kind")" = "$expected_kind" ]] || {
    echo "image deletion retirement refuses foreign owner: $path" >&2
    return 1
  }
  mv -T "$path" "$retired_path"
  sync -f /etc/legal-mcp
  delete_owned_image_transaction_directory "$retired_path" "$marker" "$expected_kind"
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
      "$TRANSACTION_PREPARING" "$TRANSACTION_PREPARING_RETIRED" \
      "$TRANSACTION_PREPARING_DELETION" LEGAL_MCP_BOOTSTRAP_IMAGE_TRANSACTION_V1
  elif ! image_path_is_absent "$TRANSACTION_PREPARING_RETIRED" \
    || ! image_path_is_absent "$TRANSACTION_PREPARING_DELETION"; then
    IMAGE_PREPARATION_WAS_PENDING=true
    delete_owned_image_transaction_directory "$TRANSACTION_PREPARING_RETIRED" \
      "$TRANSACTION_PREPARING_DELETION" LEGAL_MCP_BOOTSTRAP_IMAGE_TRANSACTION_V1
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
  if ! image_path_is_absent "$TRANSACTION_RETIRED" \
    || ! image_path_is_absent "$TRANSACTION_DELETION"; then
    IMAGE_RETIREMENT_WAS_PENDING=true
    delete_owned_image_transaction_directory "$TRANSACTION_RETIRED" \
      "$TRANSACTION_DELETION" LEGAL_MCP_BOOTSTRAP_IMAGE_TRANSACTION_V1
  fi
}

retire_image_transaction() {
  if ! image_path_is_absent "$TRANSACTION_RETIRING" \
    || ! image_path_is_absent "$TRANSACTION_RETIRED" \
    || ! image_path_is_absent "$TRANSACTION_DELETION"; then
    echo 'image transaction retirement state is not clean' >&2
    return 1
  fi
  mv -T "$TRANSACTION" "$TRANSACTION_RETIRING"
  sync -f /etc/legal-mcp
  mv -T "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"
  sync -f /etc/legal-mcp
  delete_owned_image_transaction_directory "$TRANSACTION_RETIRED" \
    "$TRANSACTION_DELETION" LEGAL_MCP_BOOTSTRAP_IMAGE_TRANSACTION_V1
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
  if ! image_path_is_absent "$TRANSACTION_DELETION"; then
    delete_owned_image_transaction_directory "$TRANSACTION_RETIRED" \
      "$TRANSACTION_DELETION" LEGAL_MCP_IMAGE_TRANSACTION_V2
    return 0
  fi
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
  delete_owned_image_transaction_directory "$TRANSACTION_RETIRED" \
    "$TRANSACTION_DELETION" LEGAL_MCP_IMAGE_TRANSACTION_V2
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
  printf '%s\n' LEGAL_MCP_IMAGE_TRANSACTION_V2 > "$directory/kind"
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

ordinary_preparation_payload_state() {
  retired_image_payload_state "$1" \
    kind target-version target-revision updater-sha256 retirement-outcome release-sha256 \
    saved-sha256 target-sha256 saved-metadata target-metadata state \
    saved-image saved-quadlet saved-template saved-runtime.env \
    saved-api-keys.json saved-Caddyfile saved-auth-ready saved-active-generation \
    target-image target-quadlet target-template
}

ordinary_recover_pending_state() {
  local path found
  if ! image_path_is_absent "$TRANSACTION_PREPARING_DELETION"; then
    for path in "$TRANSACTION_PREPARING" "$TRANSACTION" \
      "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED" "$TRANSACTION_DELETION"; do
      image_path_is_absent "$path" || {
        echo 'image preparation deletion conflicts with another transaction phase' >&2
        return 1
      }
    done
    delete_owned_image_transaction_directory "$TRANSACTION_PREPARING_RETIRED" \
      "$TRANSACTION_PREPARING_DELETION" LEGAL_MCP_IMAGE_TRANSACTION_V2
    echo 'interrupted image preparation discarded'
    return 0
  fi
  if ! image_path_is_absent "$TRANSACTION_DELETION"; then
    for path in "$TRANSACTION_PREPARING" "$TRANSACTION_PREPARING_RETIRED" \
      "$TRANSACTION" "$TRANSACTION_RETIRING"; do
      image_path_is_absent "$path" || {
        echo 'image deletion conflicts with another transaction phase' >&2
        return 1
      }
    done
    ordinary_complete_retired_transaction
    echo 'interrupted image transaction retirement completed'
    return 0
  fi
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
    ordinary_preparation_payload_state "$TRANSACTION_PREPARING" >/dev/null
    found="$(find "$TRANSACTION_PREPARING" -mindepth 1 -maxdepth 1 -print -quit)"
    if [[ -z "$found" ]]; then
      rmdir "$TRANSACTION_PREPARING"
      sync -f /etc/legal-mcp
      echo 'interrupted image preparation discarded'
      return 0
    fi
    bootstrap_require_regular "$TRANSACTION_PREPARING/kind" root root 600
    [[ "$(<"$TRANSACTION_PREPARING/kind")" = LEGAL_MCP_IMAGE_TRANSACTION_V2 ]] || {
      echo 'ordinary recovery refuses a preparation owned by another image operation' >&2
      return 1
    }
    retire_image_directory_for_deletion \
      "$TRANSACTION_PREPARING" "$TRANSACTION_PREPARING_RETIRED" \
      "$TRANSACTION_PREPARING_DELETION" LEGAL_MCP_IMAGE_TRANSACTION_V2
    echo 'interrupted image preparation discarded'
    return 0
  fi
  if ! image_path_is_absent "$TRANSACTION_PREPARING_RETIRED" \
    || ! image_path_is_absent "$TRANSACTION_PREPARING_DELETION"; then
    delete_owned_image_transaction_directory "$TRANSACTION_PREPARING_RETIRED" \
      "$TRANSACTION_PREPARING_DELETION" LEGAL_MCP_IMAGE_TRANSACTION_V2
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

pair_require_launcher_context() {
  local permit_pid permit_start actual_start uid_line cmdline dispatch
  dispatch=/run/legal-mcp/host-tool-launcher-dispatch
  bootstrap_require_regular "$PAIR_PERMIT" root root 400 || {
    echo 'pair transition must run through the installed stable root launcher' >&2
    return 1
  }
  bootstrap_require_acl "$PAIR_PERMIT" $'user::r--\ngroup::---\nother::---' || return 1
  read -r permit_pid permit_start < "$PAIR_PERMIT" || return 1
  [[ "$permit_pid" =~ ^[1-9][0-9]*$ && "$permit_start" =~ ^[1-9][0-9]*$ \
    && "$(wc -w < "$PAIR_PERMIT")" = 2 ]] || return 1
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
  uid_line="$(awk '$1 == "Uid:" {print $2 ":" $3 ":" $4 ":" $5}' "/proc/$permit_pid/status")" \
    || return 1
  [[ "$uid_line" = 0:0:0:0 ]] || return 1
  cmdline="$(tr '\0' '\n' < "/proc/$permit_pid/cmdline")" || return 1
  grep -Fxq -- '--legal-mcp-launcher-internal' <<< "$cmdline" \
    && grep -Fxq update-image <<< "$cmdline" \
    && { grep -Fxq -- '--pair-cutover' <<< "$cmdline" \
      || grep -Fxq -- '--pair-rollback' <<< "$cmdline"; } || return 1
  [[ -d "$dispatch" && ! -L "$dispatch" \
    && "$(stat -c '%U:%G:%a' "$dispatch")" = root:root:700 ]] || return 1
  for name in pid start-time role configure-auth update-image; do
    bootstrap_require_regular "$dispatch/$name" root root 600 || return 1
  done
  [[ "$(<"$dispatch/pid")" = "$permit_pid" \
    && "$(<"$dispatch/start-time")" = "$permit_start" \
    && "$(<"$dispatch/role")" = update-image ]] || return 1
}

pair_load_target_bundle() {
  local requested_template="$1" requested_version="$2" binary_version
  local resolved_template
  local -a versions revisions
  bootstrap_require_release_file "$requested_template"
  resolved_template="$(readlink -f "$requested_template")"
  PAIR_TARGET_BUNDLE_ROOT="$(cd "$(dirname "$resolved_template")/../.." && pwd -P)"
  PAIR_TARGET_RELEASE_TEMPLATE="$PAIR_TARGET_BUNDLE_ROOT/infra/hosting/legal-mcp.container.template"
  [[ "$resolved_template" = "$PAIR_TARGET_RELEASE_TEMPLATE" ]] || {
    echo 'pair target template is not in a complete Linux release bundle' >&2
    return 1
  }
  for path in \
    "$PAIR_TARGET_BUNDLE_ROOT/Containerfile" \
    "$PAIR_TARGET_BUNDLE_ROOT/SOURCE_COMMIT" \
    "$PAIR_TARGET_BUNDLE_ROOT/libonnxruntime.so" \
    "$PAIR_TARGET_RELEASE_TEMPLATE"; do
    bootstrap_require_release_file "$path"
  done
  for path in \
    "$PAIR_TARGET_BUNDLE_ROOT/legal-mcp" \
    "$PAIR_TARGET_BUNDLE_ROOT/infra/hosting/configure-auth.sh" \
    "$PAIR_TARGET_BUNDLE_ROOT/infra/hosting/update-image.sh" \
    "$PAIR_TARGET_BUNDLE_ROOT/scripts/legal-mcp-host-deploy" \
    "$PAIR_TARGET_BUNDLE_ROOT/scripts/legal-mcp-publisher-command"; do
    bootstrap_require_release_file "$path" true
  done
  mapfile -t versions < <(
    awk -F= '$1 == "ARG VERSION" {print $2}' "$PAIR_TARGET_BUNDLE_ROOT/Containerfile"
  )
  mapfile -t revisions < "$PAIR_TARGET_BUNDLE_ROOT/SOURCE_COMMIT"
  [[ ${#versions[@]} -eq 1 && "${versions[0]}" = "$requested_version" \
    && ${#revisions[@]} -eq 1 && "${revisions[0]}" =~ ^[0-9a-f]{40}$ ]] || {
      echo 'pair target release version or SOURCE_COMMIT is invalid' >&2
      return 1
    }
  PAIR_TARGET_VERSION="${versions[0]}"
  PAIR_TARGET_REVISION="${revisions[0]}"
  pair_validate_template_contract "$PAIR_TARGET_RELEASE_TEMPLATE" || {
    echo 'pair target release lacks the hardened image service contract' >&2
    return 1
  }
  binary_version="$(env -u LD_LIBRARY_PATH -u LD_PRELOAD \
    "$PAIR_TARGET_BUNDLE_ROOT/legal-mcp" --version)"
  [[ "$binary_version" = "legal-mcp $PAIR_TARGET_VERSION" ]] || {
    echo 'pair target release binary version does not match its bundle' >&2
    return 1
  }
  env -u LD_LIBRARY_PATH -u LD_PRELOAD \
    ORT_DYLIB_PATH="$PAIR_TARGET_BUNDLE_ROOT/libonnxruntime.so" \
    "$PAIR_TARGET_BUNDLE_ROOT/legal-mcp" verify-runtime \
    | grep -Fq '"onnx_runtime_ready":true'
}

pair_require_fixed_host_identities() {
  [[ "$(id -u legal-mcp)" = 971 && "$(id -g legal-mcp)" = 971 \
    && "$(id -G legal-mcp)" = 971 ]] || {
    echo 'pair service identity does not match fixed UID/GID 971' >&2
    return 1
  }
  [[ "$(id -u legal-mcp-publisher)" = 973 \
    && "$(id -g legal-mcp-publisher)" = 973 \
    && "$(id -G legal-mcp-publisher)" = 973 ]] || {
    echo 'pair publisher identity does not match fixed UID/GID 973' >&2
    return 1
  }
}

pair_validate_mount_contract() {
  local target source fstype options xfs_details actual_uuid marker_uuid host_uuid directory
  local -a marker host_marker
  bootstrap_require_regular /etc/legal-mcp/host-installed root root 444 || return 1
  bootstrap_require_acl /etc/legal-mcp/host-installed \
    $'user::r--\ngroup::r--\nother::r--' || return 1
  mapfile -t host_marker < /etc/legal-mcp/host-installed
  [[ ${#host_marker[@]} -eq 2 && "${host_marker[0]}" = LEGAL_MCP_HOST_V1 \
    && "${host_marker[1]}" =~ ^VOLUME_UUID=([0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12})$ ]] || return 1
  host_uuid="${BASH_REMATCH[1],,}"
  read -r target source fstype options < <(
    findmnt --noheadings --raw --output TARGET,SOURCE,FSTYPE,OPTIONS \
      --target /srv/legal-mcp
  )
  [[ "$target" = /srv/legal-mcp && "$fstype" = xfs \
    && ",$options," = *,noatime,* && ",$options," = *,nodev,* \
    && ",$options," = *,noexec,* && ",$options," = *,nosuid,* ]] || {
      echo 'pair transition requires the exact mounted XFS corpus volume' >&2
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
    && "${marker[1]}" =~ ^UUID=([0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12})$ ]] || return 1
  marker_uuid="${BASH_REMATCH[1],,}"
  actual_uuid="$(blkid -s UUID -o value "$source" | tr '[:upper:]' '[:lower:]')" || return 1
  [[ "$host_uuid" = "$marker_uuid" && "$actual_uuid" = "$marker_uuid" ]] || return 1
  [[ -d /srv/legal-mcp && ! -L /srv/legal-mcp \
    && "$(stat -c '%U:%G:%a' /srv/legal-mcp)" = root:legal-mcp:750 ]] || return 1
  bootstrap_require_acl /srv/legal-mcp \
    $'user::rwx\nuser:973:--x\ngroup::r-x\nmask::r-x\nother::---' || return 1
  for directory in generations lifecycle state uploads; do
    [[ -d "/srv/legal-mcp/$directory" && ! -L "/srv/legal-mcp/$directory" ]] || return 1
  done
  [[ "$(stat -c '%U:%G:%a' /srv/legal-mcp/generations)" = root:legal-mcp:750 \
    && "$(stat -c '%U:%G:%a' /srv/legal-mcp/lifecycle)" = root:legal-mcp:750 \
    && "$(stat -c '%U:%G:%a' /srv/legal-mcp/state)" = legal-mcp:legal-mcp:700 \
    && "$(stat -c '%U:%G:%a' /srv/legal-mcp/uploads)" \
      = legal-mcp-publisher:legal-mcp-publisher:700 ]] || return 1
  bootstrap_require_acl /srv/legal-mcp/generations \
    $'user::rwx\ngroup::r-x\nother::---' || return 1
  bootstrap_require_acl /srv/legal-mcp/lifecycle \
    $'user::rwx\ngroup::r-x\nother::---' || return 1
  bootstrap_require_acl /srv/legal-mcp/state \
    $'user::rwx\ngroup::---\nother::---' || return 1
  bootstrap_require_acl /srv/legal-mcp/uploads \
    $'user::rwx\ngroup::---\nother::---' || return 1
  bootstrap_require_regular /srv/legal-mcp/lifecycle/LOCK root legal-mcp 640 || return 1
  [[ "$(stat -c '%h' /srv/legal-mcp/lifecycle/LOCK)" = 1 ]] || return 1
  bootstrap_require_acl /srv/legal-mcp/lifecycle/LOCK \
    $'user::rw-\ngroup::r--\nother::---' || return 1
  bootstrap_require_empty_regular /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK root root 640 || return 1
  [[ "$(stat -c '%h' /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK)" = 1 ]] || return 1
  bootstrap_require_acl /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK \
    $'user::rw-\ngroup::r--\nother::---' || return 1
}

pair_validate_template_contract() {
  local template="$1"
  [[ "$(grep -o '__IMAGE_DIGEST__' "$template" | wc -l)" = 1 \
    && "$(grep -Fxc 'User=971:971' "$template")" = 1 \
    && "$(grep -Fxc 'PublishPort=127.0.0.1:51235:51235' "$template")" = 1 \
    && "$(grep -Fxc 'ReadOnly=true' "$template")" = 1 \
    && "$(grep -Fxc 'DropCapability=all' "$template")" = 1 \
    && "$(grep -Fxc 'NoNewPrivileges=true' "$template")" = 1 \
    && "$(grep -Fxc 'ExecCondition=/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready' \
      "$template")" = 1 \
    && "$(grep -Fxc 'Volume=/srv/legal-mcp/generations:/var/lib/legal-mcp/generations:ro,nodev,nosuid,noexec' \
      "$template")" = 1 \
    && "$(grep -Fxc 'Volume=/srv/legal-mcp/lifecycle:/var/lib/legal-mcp/lifecycle:ro,nodev,nosuid,noexec' \
      "$template")" = 1 \
    && "$(grep -Fxc 'Volume=/srv/legal-mcp/state:/var/lib/legal-mcp/state:rw,nodev,nosuid,noexec' \
      "$template")" = 1 \
    && "$(grep -Fxc 'Volume=/etc/legal-mcp/api-keys.json:/run/secrets/legal-mcp-api-keys.json:ro,nodev,nosuid,noexec' \
      "$template")" = 1 ]]
}

pair_validate_installed_template() {
  pair_validate_template_contract "$TEMPLATE" || {
    echo 'installed pair template lacks the exact hardened mount and service contract' >&2
    return 1
  }
}

pair_read_deployment_journal() {
  local journal=/srv/legal-mcp/lifecycle/.deployment-transaction
  bootstrap_require_regular "$journal" root root 600 || return 1
  mapfile -t PAIR_DEPLOYMENT < "$journal"
  [[ ${#PAIR_DEPLOYMENT[@]} -eq 3 \
    && "${PAIR_DEPLOYMENT[0]}" =~ ^[0-9a-f]{64}$ \
    && "${PAIR_DEPLOYMENT[1]}" =~ ^[0-9a-f]{64}$ \
    && "${PAIR_DEPLOYMENT[2]}" =~ ^(prepared|activating|activated|rolling-back|rolled-back)$ ]] \
    || return 1
  PAIR_DEPLOYMENT_GENERATION="${PAIR_DEPLOYMENT[0]}"
  PAIR_DEPLOYMENT_PREVIOUS="${PAIR_DEPLOYMENT[1]}"
  PAIR_DEPLOYMENT_PHASE="${PAIR_DEPLOYMENT[2]}"
}

pair_require_no_foreign_transaction() {
  local allow_pair="$1" path found
  require_known_image_transaction_states || return 1
  for path in \
    /etc/legal-mcp/.auth-transaction.preparing \
    /etc/legal-mcp/.auth-transaction.preparing-retired \
    /etc/legal-mcp/.auth-transaction \
    /etc/legal-mcp/.auth-transaction.retiring \
    /etc/legal-mcp/.auth-transaction.retired \
    /etc/legal-mcp/.auth-transaction.legacy-v0192-preparing-retiring \
    /etc/legal-mcp/.auth-transaction.legacy-v0192-preparing-retired \
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
      echo 'a foreign auth or host-tool transaction must be recovered first' >&2
      return 1
    }
  done
  found="$(find /etc/legal-mcp -mindepth 1 -maxdepth 1 \
    -name '.auth-transaction.preparing.*' -print -quit)" || return 1
  [[ -z "$found" ]] || {
    echo 'a foreign authentication preparation must be recovered first' >&2
    return 1
  }
  if [[ "$allow_pair" = false ]]; then
    for path in "$TRANSACTION_PREPARING" "$TRANSACTION_PREPARING_RETIRED" \
      "$TRANSACTION_PREPARING_DELETION" "$TRANSACTION" \
      "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED" "$TRANSACTION_DELETION"; do
      image_path_is_absent "$path" || {
        echo 'an image transaction already exists; use its explicit recovery route' >&2
        return 1
      }
    done
  fi
}

pair_capture_image_identity() {
  local image="$1" prefix="$2" version revision image_id
  version="$(podman image inspect "$image" \
    --format '{{index .Labels "org.opencontainers.image.version"}}')"
  revision="$(podman image inspect "$image" \
    --format '{{index .Labels "org.opencontainers.image.revision"}}')"
  [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ && "$revision" =~ ^[0-9a-f]{40}$ ]] || {
    echo 'recorded pair image version or revision is malformed' >&2
    return 1
  }
  verify_image_runtime "$image" "$version" "$revision"
  image_id="$(canonical_image_id \
    "$(podman image inspect "$image" --format '{{.Id}}')")" || return 1
  printf -v "${prefix}_VERSION" '%s' "$version"
  printf -v "${prefix}_REVISION" '%s' "$revision"
  printf -v "${prefix}_IMAGE_ID" '%s' "$image_id"
}

pair_require_configured_dark() {
  local container_state running_image_id
  image_path_is_absent "$AUTH_READY" || {
    echo 'public pair maintenance requires explicit --from-public authorization' >&2
    return 1
  }
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(read_systemctl_activity "$SERVICE")" = inactive \
    && "$(read_systemctl_enablement caddy.service)" = disabled \
    && "$(read_systemctl_activity caddy.service)" = inactive \
    && "$(ufw_rule_state 80)" = absent \
    && "$(ufw_rule_state 443)" = absent ]] || {
      echo 'pair transition requires the exact configured-dark host state' >&2
      return 1
    }
  ufw_is_fail_closed
  ordinary_require_listener_topology none
  container_state="$(podman_container_state australian-legal-mcp)" || return 1
  if [[ "$container_state" = present ]]; then
    running_image_id="$(canonical_image_id \
      "$(podman inspect australian-legal-mcp --format '{{.Image}}')")" || return 1
    [[ "$running_image_id" = "$PAIR_PRIOR_IMAGE_ID" ]] || return 1
  fi
  PAIR_ORIGIN=dark
}

pair_require_public() {
  local running_image_id host
  ordinary_require_live_metadata
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(read_systemctl_activity "$SERVICE")" = active \
    && "$(read_systemctl_enablement caddy.service)" = enabled \
    && "$(read_systemctl_activity caddy.service)" = active \
    && "$(ufw_rule_state 80)" = present \
    && "$(ufw_rule_state 443)" = present ]] || {
      echo '--from-public requires the exact authenticated public host state' >&2
      return 1
    }
  ufw_is_fail_closed
  ordinary_require_listener_topology public
  wait_for_exact_generation "$PAIR_EXPECTED_CURRENT_GENERATION"
  host="${EXTERNAL_URL#https://}"; host="${host%/mcp}"
  ordinary_validate_caddy_contract "$host"
  probe_auth_boundary http://127.0.0.1:51235/mcp \
    http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp
  probe_auth_boundary "$EXTERNAL_URL" \
    "${EXTERNAL_URL%/mcp}/.well-known/oauth-protected-resource/mcp"
  ordinary_probe_negative_caddy_routes "${EXTERNAL_URL%/mcp}"
  running_image_id="$(canonical_image_id \
    "$(podman inspect australian-legal-mcp --format '{{.Image}}')")" || return 1
  [[ "$running_image_id" = "$PAIR_PRIOR_IMAGE_ID" ]] || return 1
  PAIR_ORIGIN=public
}

pair_validate_manifest_file() {
  local path="$1" owner="$2" group="$3" mode="$4"
  ordinary_require_regular "$path" "$owner" "$group" "$mode"
}

pair_capture_baseline() {
  local rendered image_state candidate_manifest authorization host
  local -a current_image
  pair_validate_mount_contract
  pair_validate_installed_template
  ordinary_require_static_live_metadata
  load_runtime_contract "$RUNTIME_ENV"
  read_probe_key
  PAIR_PRIOR_GENERATION="$(</srv/legal-mcp/lifecycle/active-generation)"
  [[ "$PAIR_PRIOR_GENERATION" = "$PAIR_EXPECTED_CURRENT_GENERATION" \
    && "$PAIR_PRIOR_GENERATION" =~ ^[0-9a-f]{64}$ \
    && "$PAIR_GENERATION" != "$PAIR_PRIOR_GENERATION" ]] || {
      echo 'live generation does not match the explicit current pair' >&2
      return 1
    }
  mapfile -t current_image < "$IMAGE_FILE"
  [[ ${#current_image[@]} -eq 1 ]]
  PAIR_PRIOR_IMAGE="${current_image[0]}"
  [[ "$PAIR_PRIOR_IMAGE" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ \
    && "$NEW_IMAGE" != "$PAIR_PRIOR_IMAGE" ]] || {
      echo 'current and target pair image pins are malformed or identical' >&2
      return 1
    }
  rendered="$(mktemp /run/legal-mcp-pair-current-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$PAIR_PRIOR_IMAGE|g" "$TEMPLATE" > "$rendered"
  if ! cmp --silent "$rendered" "$QUADLET"; then
    rm -f "$rendered"
    echo 'current pair image, installed template, and Quadlet do not agree' >&2
    return 1
  fi
  rm -f "$rendered"
  image_state="$(podman_image_state "$PAIR_PRIOR_IMAGE")" || return 1
  [[ "$image_state" = present ]] || {
    echo 'current pair image is not present' >&2
    return 1
  }
  pair_capture_image_identity "$PAIR_PRIOR_IMAGE" PAIR_PRIOR
  pair_validate_manifest_file \
    "/srv/legal-mcp/generations/$PAIR_PRIOR_GENERATION/generation.json" \
    root legal-mcp 440
  PAIR_PRIOR_MANIFEST_SHA256="$(sha256sum \
    "/srv/legal-mcp/generations/$PAIR_PRIOR_GENERATION/generation.json" | awk '{print $1}')"
  if [[ "$PAIR_FROM_PUBLIC" = true ]]; then
    pair_require_public
  else
    pair_require_configured_dark
  fi
  host="${EXTERNAL_URL#https://}"; host="${host%/mcp}"
  ordinary_validate_caddy_contract "$host"
  podman run --rm --network=none --user=971:971 --read-only --cap-drop=all \
    --security-opt=no-new-privileges --pids-limit=256 --memory=6g --memory-swap=6g \
    --tmpfs=/tmp:rw,nodev,nosuid,noexec,size=64m,mode=1777 \
    --volume=/srv/legal-mcp:/var/lib/legal-mcp:ro,nodev,nosuid,noexec \
    "$PAIR_PRIOR_IMAGE" verify --quiet >/dev/null

  authorization=/run/legal-mcp/authorized-upload
  case "$PAIR_OPERATION" in
    prepared)
      pair_read_deployment_journal
      [[ "$PAIR_DEPLOYMENT_GENERATION" = "$PAIR_GENERATION" \
        && "$PAIR_DEPLOYMENT_PREVIOUS" = "$PAIR_PRIOR_GENERATION" \
        && "$PAIR_DEPLOYMENT_PHASE" = prepared \
        && -d "/srv/legal-mcp/uploads/$PAIR_GENERATION" \
        && ! -L "/srv/legal-mcp/uploads/$PAIR_GENERATION" \
        && ! -e "/srv/legal-mcp/generations/$PAIR_GENERATION" \
        && ! -L "/srv/legal-mcp/generations/$PAIR_GENERATION" ]] || {
          echo 'pair cutover requires one exact ordinary prepared generation' >&2
          return 1
        }
      candidate_manifest="/srv/legal-mcp/uploads/$PAIR_GENERATION/generation.json"
      pair_validate_manifest_file "$candidate_manifest" \
        legal-mcp-publisher legal-mcp-publisher 600
      bootstrap_require_regular "$authorization" root legal-mcp-publisher 440
      [[ "$(<"$authorization")" = "$PAIR_GENERATION" ]] || {
        echo 'prepared upload authorization does not match the pair target' >&2
        return 1
      }
      PAIR_UPLOAD_AUTHORIZATION=present
      ;;
    installed)
      if ! image_path_is_absent /srv/legal-mcp/lifecycle/.deployment-transaction \
        || ! image_path_is_absent /srv/legal-mcp/lifecycle/.deployment-transaction.preparing \
        || ! image_path_is_absent "$authorization"; then
          echo 'pair rollback requires no corpus upload transaction' >&2
          return 1
      fi
      [[ -d "/srv/legal-mcp/generations/$PAIR_GENERATION" \
        && ! -L "/srv/legal-mcp/generations/$PAIR_GENERATION" \
        && ! -e "/srv/legal-mcp/uploads/$PAIR_GENERATION" \
        && ! -L "/srv/legal-mcp/uploads/$PAIR_GENERATION" ]] || {
          echo 'pair rollback target is not one retained installed generation' >&2
          return 1
        }
      candidate_manifest="/srv/legal-mcp/generations/$PAIR_GENERATION/generation.json"
      pair_validate_manifest_file "$candidate_manifest" root legal-mcp 440
      PAIR_UPLOAD_AUTHORIZATION=absent
      ;;
    *) return 1 ;;
  esac
  PAIR_TARGET_MANIFEST_SHA256="$(sha256sum "$candidate_manifest" | awk '{print $1}')"
  [[ "$PAIR_PRIOR_MANIFEST_SHA256$PAIR_TARGET_MANIFEST_SHA256" =~ ^[0-9a-f]{128}$ ]]
}

pair_render_state() {
  local destination="$1"
  cat > "$destination" <<EOF
ORIGIN=$PAIR_ORIGIN
AUTH_MODE=$AUTH_MODE
EXTERNAL_URL=$EXTERNAL_URL
PRIOR_GENERATION=$PAIR_PRIOR_GENERATION
TARGET_GENERATION=$PAIR_GENERATION
PRIOR_MANIFEST_SHA256=$PAIR_PRIOR_MANIFEST_SHA256
TARGET_MANIFEST_SHA256=$PAIR_TARGET_MANIFEST_SHA256
PRIOR_IMAGE=$PAIR_PRIOR_IMAGE
PRIOR_IMAGE_ID=$PAIR_PRIOR_IMAGE_ID
PRIOR_IMAGE_VERSION=$PAIR_PRIOR_VERSION
PRIOR_IMAGE_REVISION=$PAIR_PRIOR_REVISION
TARGET_IMAGE=$NEW_IMAGE
TARGET_IMAGE_ID=$PAIR_TARGET_IMAGE_ID
TARGET_IMAGE_VERSION=$PAIR_TARGET_VERSION
TARGET_IMAGE_REVISION=$PAIR_TARGET_REVISION
TARGET_RELEASE_TEMPLATE_SHA256=$(sha256sum "$TEMPLATE" | awk '{print $1}')
UPLOAD_AUTHORIZATION=$PAIR_UPLOAD_AUTHORIZATION
EOF
}

pair_discard_unpublished_build() {
  image_path_is_absent "$PAIR_TRANSACTION_BUILD" && return 0
  require_image_transaction_directory "$PAIR_TRANSACTION_BUILD"
  PAIR_BUILD_WAS_PENDING=true
  rm -rf --one-file-system -- "$PAIR_TRANSACTION_BUILD"
  image_path_is_absent "$PAIR_TRANSACTION_BUILD" || return 1
  sync -f /etc/legal-mcp
}

pair_create_transaction() {
  local directory="$PAIR_TRANSACTION_BUILD" rendered
  image_path_is_absent "$directory" || return 1
  image_path_is_absent "$TRANSACTION_PREPARING" || return 1
  install -d -o root -g root -m 0700 "$directory"
  printf '%s\n' LEGAL_MCP_IMAGE_GENERATION_PAIR_TRANSACTION_V1 > "$directory/kind"
  printf '%s\n' "$PAIR_OPERATION" > "$directory/operation"
  printf '%s\n' "$PAIR_PRIOR_GENERATION" > "$directory/prior-generation"
  printf '%s\n' "$PAIR_GENERATION" > "$directory/target-generation"
  printf '%s\n' prepared > "$directory/phase"
  printf '%s\n' pending > "$directory/retirement-outcome"
  install -o root -g root -m 0600 "$IMAGE_FILE" "$directory/saved-image"
  install -o root -g root -m 0600 "$QUADLET" "$directory/saved-quadlet"
  install -o root -g root -m 0600 "$TEMPLATE" "$directory/saved-template"
  install -o root -g root -m 0600 "$RUNTIME_ENV" "$directory/saved-runtime.env"
  install -o root -g root -m 0600 "$API_KEYS" "$directory/saved-api-keys.json"
  install -o root -g root -m 0600 "$CADDYFILE" "$directory/saved-Caddyfile"
  if image_path_is_absent "$AUTH_READY"; then
    install -o root -g root -m 0600 /dev/null "$directory/saved-auth-ready"
  else
    install -o root -g root -m 0600 "$AUTH_READY" "$directory/saved-auth-ready"
  fi
  install -o root -g root -m 0600 /srv/legal-mcp/lifecycle/active-generation \
    "$directory/saved-active-generation"
  if [[ "$PAIR_OPERATION" = prepared ]]; then
    install -o root -g root -m 0600 /srv/legal-mcp/lifecycle/.deployment-transaction \
      "$directory/saved-deployment-journal"
    install -o root -g root -m 0600 /run/legal-mcp/authorized-upload \
      "$directory/saved-upload-authorization"
  else
    install -o root -g root -m 0600 /dev/null "$directory/saved-deployment-journal"
    install -o root -g root -m 0600 /dev/null "$directory/saved-upload-authorization"
  fi
  printf '%s\n' "$NEW_IMAGE" > "$directory/target-image"
  install -o root -g root -m 0600 "$TEMPLATE" "$directory/target-template"
  rendered="$(mktemp /run/legal-mcp-pair-target-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$NEW_IMAGE|g" "$TEMPLATE" > "$rendered"
  install -o root -g root -m 0600 "$rendered" "$directory/target-quadlet"
  rm -f "$rendered"
  printf '%s' "$PAIR_GENERATION" > "$directory/target-active-generation"
  printf '%s\n' "$ORDINARY_VERSION" > "$directory/coordinator-version"
  printf '%s\n' "$ORDINARY_REVISION" > "$directory/coordinator-revision"
  printf '%s\n' "$ORDINARY_UPDATER_SHA256" > "$directory/updater-sha256"
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
  pair_render_state "$directory/state"
  chown root:root "$directory"/*
  chmod 600 "$directory"/*
  sync -f "$directory"
  pair_validate_transaction "$directory"
  mv -T "$directory" "$TRANSACTION_PREPARING"
  sync -f /etc/legal-mcp
  sync -f "$TRANSACTION_PREPARING"
  pair_validate_transaction "$TRANSACTION_PREPARING"
  mv -T "$TRANSACTION_PREPARING" "$TRANSACTION"
  sync -f /etc/legal-mcp
}

pair_validate_transaction() {
  local directory="$1" name rendered manifest metadata release_manifest
  local -a kind operation coordinator_version coordinator_revision updater outcome phase state
  require_image_transaction_directory "$directory"
  for name in kind operation prior-generation target-generation phase retirement-outcome \
    coordinator-version coordinator-revision updater-sha256 release-sha256 \
    saved-sha256 target-sha256 saved-metadata target-metadata state \
    saved-image saved-quadlet saved-template saved-runtime.env saved-api-keys.json \
    saved-Caddyfile saved-auth-ready saved-active-generation saved-deployment-journal \
    saved-upload-authorization target-image target-quadlet target-template \
    target-active-generation; do
    ordinary_require_regular "$directory/$name" root root 600 || return 1
  done
  bootstrap_directory_contains_only "$directory" \
    kind operation prior-generation target-generation phase retirement-outcome \
    coordinator-version coordinator-revision updater-sha256 release-sha256 \
    saved-sha256 target-sha256 saved-metadata target-metadata state \
    saved-image saved-quadlet saved-template saved-runtime.env saved-api-keys.json \
    saved-Caddyfile saved-auth-ready saved-active-generation saved-deployment-journal \
    saved-upload-authorization target-image target-quadlet target-template \
    target-active-generation || {
      echo 'pair transaction contains unexpected durable state' >&2
      return 1
    }
  mapfile -t kind < "$directory/kind"
  mapfile -t operation < "$directory/operation"
  mapfile -t coordinator_version < "$directory/coordinator-version"
  mapfile -t coordinator_revision < "$directory/coordinator-revision"
  mapfile -t updater < "$directory/updater-sha256"
  mapfile -t outcome < "$directory/retirement-outcome"
  mapfile -t phase < "$directory/phase"
  [[ ${#kind[@]} -eq 1 \
    && "${kind[0]}" = LEGAL_MCP_IMAGE_GENERATION_PAIR_TRANSACTION_V1 \
    && ${#operation[@]} -eq 1 && "${operation[0]}" =~ ^(prepared|installed)$ \
    && ${#coordinator_version[@]} -eq 1 \
    && "${coordinator_version[0]}" = "$ORDINARY_VERSION" \
    && ${#coordinator_revision[@]} -eq 1 \
    && "${coordinator_revision[0]}" = "$ORDINARY_REVISION" \
    && ${#updater[@]} -eq 1 && "${updater[0]}" = "$ORDINARY_UPDATER_SHA256" \
    && ${#outcome[@]} -eq 1 && "${outcome[0]}" =~ ^(pending|saved|target)$ \
    && ${#phase[@]} -eq 1 \
    && "${phase[0]}" =~ ^(prepared|darkening|dark|sealing|sealed|target-files|activating|activated|verifying|proved|committing|committed)$ ]] || {
      echo 'pair transaction does not belong to this exact installed coordinator' >&2
      return 1
    }
  PAIR_OPERATION="${operation[0]}"
  PAIR_RETIREMENT_OUTCOME="${outcome[0]}"
  PAIR_PHASE="${phase[0]}"
  release_manifest="$(mktemp /run/legal-mcp-pair-release.XXXXXX)"
  ordinary_render_release_manifest "$release_manifest"
  if ! cmp --silent "$release_manifest" "$directory/release-sha256"; then
    rm -f "$release_manifest"
    echo 'pair transaction coordinator release bytes changed' >&2
    return 1
  fi
  rm -f "$release_manifest"
  metadata="$(mktemp /run/legal-mcp-pair-metadata.XXXXXX)"
  ordinary_render_metadata_manifest "$metadata"
  if ! cmp --silent "$metadata" "$directory/saved-metadata" \
    || ! cmp --silent "$metadata" "$directory/target-metadata"; then
    rm -f "$metadata"
    echo 'pair transaction host metadata contract changed' >&2
    return 1
  fi
  rm -f "$metadata"
  manifest="$(mktemp /run/legal-mcp-pair-hashes.XXXXXX)"
  ordinary_render_hash_manifest \
    "$directory/saved-image" "$directory/saved-quadlet" \
    "$directory/saved-template" "$directory/saved-runtime.env" \
    "$directory/saved-api-keys.json" "$directory/saved-Caddyfile" \
    "$directory/saved-auth-ready" "$directory/saved-active-generation" "$manifest"
  if ! cmp --silent "$manifest" "$directory/saved-sha256"; then
    rm -f "$manifest"
    echo 'saved pair bytes do not match their hash manifest' >&2
    return 1
  fi
  ordinary_render_hash_manifest \
    "$directory/target-image" "$directory/target-quadlet" \
    "$directory/target-template" "$directory/saved-runtime.env" \
    "$directory/saved-api-keys.json" "$directory/saved-Caddyfile" \
    "$directory/saved-auth-ready" "$directory/target-active-generation" "$manifest"
  if ! cmp --silent "$manifest" "$directory/target-sha256"; then
    rm -f "$manifest"
    echo 'target pair bytes do not match their hash manifest' >&2
    return 1
  fi
  rm -f "$manifest"

  mapfile -t state < "$directory/state"
  [[ ${#state[@]} -eq 17 \
    && "${state[0]}" =~ ^ORIGIN=(dark|public)$ \
    && "${state[1]}" =~ ^AUTH_MODE=(api-key|entra|entra\+api-key)$ \
    && "${state[2]}" =~ ^EXTERNAL_URL=https://[a-z0-9.-]+/mcp$ \
    && "${state[3]}" =~ ^PRIOR_GENERATION=([0-9a-f]{64})$ ]] || {
      echo 'pair transaction state is malformed' >&2
      return 1
    }
  PAIR_ORIGIN="${state[0]#*=}"
  PAIR_AUTH_MODE="${state[1]#*=}"
  PAIR_EXTERNAL_URL="${state[2]#*=}"
  PAIR_PRIOR_GENERATION="${state[3]#*=}"
  [[ "${state[4]}" =~ ^TARGET_GENERATION=([0-9a-f]{64})$ ]]
  PAIR_TARGET_GENERATION="${state[4]#*=}"
  [[ "$PAIR_PRIOR_GENERATION" != "$PAIR_TARGET_GENERATION" \
    && "$(<"$directory/prior-generation")" = "$PAIR_PRIOR_GENERATION" \
    && "$(<"$directory/target-generation")" = "$PAIR_TARGET_GENERATION" ]]
  [[ "${state[5]}" =~ ^PRIOR_MANIFEST_SHA256=([0-9a-f]{64})$ ]]
  PAIR_PRIOR_MANIFEST_SHA256="${state[5]#*=}"
  [[ "${state[6]}" =~ ^TARGET_MANIFEST_SHA256=([0-9a-f]{64})$ ]]
  PAIR_TARGET_MANIFEST_SHA256="${state[6]#*=}"
  [[ "${state[7]}" =~ ^PRIOR_IMAGE=(ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64})$ ]]
  PAIR_PRIOR_IMAGE="${state[7]#*=}"
  [[ "${state[8]}" =~ ^PRIOR_IMAGE_ID=(sha256:[0-9a-f]{64})$ ]]
  PAIR_PRIOR_IMAGE_ID="${state[8]#*=}"
  [[ "${state[9]}" =~ ^PRIOR_IMAGE_VERSION=([0-9]+\.[0-9]+\.[0-9]+)$ ]]
  PAIR_PRIOR_VERSION="${state[9]#*=}"
  [[ "${state[10]}" =~ ^PRIOR_IMAGE_REVISION=([0-9a-f]{40})$ ]]
  PAIR_PRIOR_REVISION="${state[10]#*=}"
  [[ "${state[11]}" =~ ^TARGET_IMAGE=(ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64})$ ]]
  PAIR_TARGET_IMAGE="${state[11]#*=}"
  [[ "${state[12]}" =~ ^TARGET_IMAGE_ID=(sha256:[0-9a-f]{64})$ ]]
  PAIR_TARGET_IMAGE_ID="${state[12]#*=}"
  [[ "${state[13]}" =~ ^TARGET_IMAGE_VERSION=([0-9]+\.[0-9]+\.[0-9]+)$ ]]
  PAIR_TARGET_VERSION="${state[13]#*=}"
  [[ "${state[14]}" =~ ^TARGET_IMAGE_REVISION=([0-9a-f]{40})$ ]]
  PAIR_TARGET_REVISION="${state[14]#*=}"
  [[ "${state[15]}" =~ ^TARGET_RELEASE_TEMPLATE_SHA256=([0-9a-f]{64})$ \
    && "${state[15]#*=}" = "$(sha256sum "$directory/target-template" | awk '{print $1}')" \
    && "${state[16]}" =~ ^UPLOAD_AUTHORIZATION=(present|absent)$ ]]
  PAIR_UPLOAD_AUTHORIZATION="${state[16]#*=}"
  [[ "$PAIR_PRIOR_IMAGE" != "$PAIR_TARGET_IMAGE" \
    && "$(<"$directory/saved-image")" = "$PAIR_PRIOR_IMAGE" \
    && "$(<"$directory/target-image")" = "$PAIR_TARGET_IMAGE" \
    && "$(<"$directory/saved-active-generation")" = "$PAIR_PRIOR_GENERATION" \
    && "$(<"$directory/target-active-generation")" = "$PAIR_TARGET_GENERATION" ]]
  load_runtime_contract "$directory/saved-runtime.env"
  [[ "$AUTH_MODE" = "$PAIR_AUTH_MODE" && "$EXTERNAL_URL" = "$PAIR_EXTERNAL_URL" ]]
  pair_validate_template_contract "$directory/saved-template"
  pair_validate_template_contract "$directory/target-template"
  rendered="$(mktemp /run/legal-mcp-pair-rendered.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$PAIR_PRIOR_IMAGE|g" "$directory/saved-template" > "$rendered"
  cmp --silent "$rendered" "$directory/saved-quadlet" || { rm -f "$rendered"; return 1; }
  sed "s|__IMAGE_DIGEST__|$PAIR_TARGET_IMAGE|g" "$directory/target-template" > "$rendered"
  cmp --silent "$rendered" "$directory/target-quadlet" || { rm -f "$rendered"; return 1; }
  rm -f "$rendered"
  case "$PAIR_OPERATION" in
    prepared)
      mapfile -t PAIR_SAVED_DEPLOYMENT < "$directory/saved-deployment-journal"
      [[ ${#PAIR_SAVED_DEPLOYMENT[@]} -eq 3 \
        && "${PAIR_SAVED_DEPLOYMENT[0]}" = "$PAIR_TARGET_GENERATION" \
        && "${PAIR_SAVED_DEPLOYMENT[1]}" = "$PAIR_PRIOR_GENERATION" \
        && "${PAIR_SAVED_DEPLOYMENT[2]}" = prepared \
        && "$PAIR_UPLOAD_AUTHORIZATION" = present \
        && "$(<"$directory/saved-upload-authorization")" = "$PAIR_TARGET_GENERATION" ]]
      ;;
    installed)
      [[ "$PAIR_UPLOAD_AUTHORIZATION" = absent \
        && "$(stat -c '%s' "$directory/saved-deployment-journal")" = 0 \
        && "$(stat -c '%s' "$directory/saved-upload-authorization")" = 0 ]]
      ;;
  esac
}

pair_reconcile_field_preparations() {
  local directory="$1" path changed=false
  for path in "$directory/phase.preparing" \
    "$directory/retirement-outcome.preparing"; do
    image_path_is_absent "$path" && continue
    ordinary_require_regular "$path" root root 600 || return 1
    rm -f -- "$path"
    image_path_is_absent "$path" || return 1
    changed=true
  done
  if [[ "$changed" = true ]]; then sync -f "$directory"; fi
}

pair_replace_field() {
  local name="$1" value="$2" preparation
  preparation="$TRANSACTION/$name.preparing"
  image_path_is_absent "$preparation" || return 1
  printf '%s\n' "$value" > "$preparation"
  chown root:root "$preparation"
  chmod 600 "$preparation"
  sync -f "$preparation"
  mv -fT "$preparation" "$TRANSACTION/$name"
  sync -f "$TRANSACTION"
}

pair_write_phase() {
  local requested_phase="$1"
  [[ "$requested_phase" =~ ^(prepared|darkening|dark|sealing|sealed|target-files|activating|activated|verifying|proved|committing|committed)$ ]]
  pair_reconcile_field_preparations "$TRANSACTION"
  pair_validate_transaction "$TRANSACTION"
  pair_replace_field phase "$requested_phase"
  pair_validate_transaction "$TRANSACTION"
  [[ "$PAIR_PHASE" = "$requested_phase" ]]
}

pair_write_outcome() {
  local choice="$1"
  [[ "$choice" = saved || "$choice" = target ]]
  pair_reconcile_field_preparations "$TRANSACTION"
  pair_validate_transaction "$TRANSACTION"
  if [[ "$PAIR_RETIREMENT_OUTCOME" = "$choice" ]]; then return 0; fi
  [[ "$PAIR_RETIREMENT_OUTCOME" = pending ]] || return 1
  pair_replace_field retirement-outcome "$choice"
  pair_validate_transaction "$TRANSACTION"
  [[ "$PAIR_RETIREMENT_OUTCOME" = "$choice" ]]
}

pair_cleanup_verification_root() {
  image_path_is_absent "$PAIR_VERIFY_ROOT" && return 0
  [[ -d "$PAIR_VERIFY_ROOT" && ! -L "$PAIR_VERIFY_ROOT" \
    && "$(stat -c '%u:%g:%a' "$PAIR_VERIFY_ROOT")" = 971:971:700 ]] || {
      echo 'ephemeral pair verification root is unsafe' >&2
      return 1
    }
  rm -rf --one-file-system -- "$PAIR_VERIFY_ROOT"
  image_path_is_absent "$PAIR_VERIFY_ROOT" || return 1
  sync -f /run/legal-mcp
}

pair_clear_upload_authorization() {
  local authorization=/run/legal-mcp/authorized-upload
  image_path_is_absent "$authorization" && return 0
  bootstrap_require_regular "$authorization" root legal-mcp-publisher 440 || return 1
  [[ "$(<"$authorization")" = "$PAIR_TARGET_GENERATION" ]] || {
    echo 'upload authorization is foreign to the pair transaction' >&2
    return 1
  }
  rm -f -- "$authorization"
  image_path_is_absent "$authorization" || return 1
  sync -f /run/legal-mcp
}

pair_restore_upload_authorization() {
  local authorization=/run/legal-mcp/authorized-upload
  if [[ "$PAIR_OPERATION" = prepared && "$PAIR_UPLOAD_AUTHORIZATION" = present ]]; then
    if ! image_path_is_absent "$authorization"; then
      bootstrap_require_regular "$authorization" root legal-mcp-publisher 440 || return 1
      cmp --silent "$authorization" "$TRANSACTION/saved-upload-authorization"
      return
    fi
    ordinary_atomic_install "$TRANSACTION/saved-upload-authorization" \
      "$authorization" root legal-mcp-publisher 440
  else
    image_path_is_absent "$authorization"
  fi
}

pair_upload_authorization_matches() {
  local authorization=/run/legal-mcp/authorized-upload
  if [[ "$PAIR_OPERATION" = prepared && "$PAIR_UPLOAD_AUTHORIZATION" = present ]]; then
    bootstrap_require_regular "$authorization" root legal-mcp-publisher 440 \
      && cmp --silent "$authorization" "$TRANSACTION/saved-upload-authorization"
  else
    image_path_is_absent "$authorization"
  fi
}

pair_force_dark() {
  local activity
  if ! image_path_is_absent "$PAIR_START_ARM"; then
    bootstrap_require_regular "$PAIR_START_ARM" root root 400 || return 1
    rm -f -- "$PAIR_START_ARM"
    sync -f /run/legal-mcp
  fi
  if ! image_path_is_absent "$AUTH_READY"; then
    ordinary_require_regular "$AUTH_READY" root root 444 || return 1
    [[ "$(stat -c '%s' "$AUTH_READY")" = 0 \
      && "$(getfacl --absolute-names --numeric --omit-header "$AUTH_READY")" \
        = $'user::r--\ngroup::r--\nother::r--' ]] || return 1
    rm -f -- "$AUTH_READY"
    sync -f /etc/legal-mcp
  fi
  close_ingress
  activity="$(read_systemctl_activity "$SERVICE")" || return 1
  if [[ "$activity" = active ]]; then systemctl stop "$SERVICE"; fi
  pair_cleanup_verification_root
  pair_clear_upload_authorization
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(read_systemctl_activity "$SERVICE")" = inactive \
    && "$(read_systemctl_enablement caddy.service)" = disabled \
    && "$(read_systemctl_activity caddy.service)" = inactive \
    && "$(ufw_rule_state 80)" = absent \
    && "$(ufw_rule_state 443)" = absent ]] || return 1
  image_path_is_absent "$AUTH_READY" && image_path_is_absent "$PAIR_START_ARM"
  ordinary_require_listener_topology none
  ufw_is_fail_closed
}

pair_candidate_manifest() {
  local upload installed
  upload="/srv/legal-mcp/uploads/$PAIR_TARGET_GENERATION"
  installed="/srv/legal-mcp/generations/$PAIR_TARGET_GENERATION"
  case "$PAIR_OPERATION" in
    prepared)
      if [[ -d "$upload" && ! -L "$upload" \
        && ! -e "$installed" && ! -L "$installed" ]]; then
        printf '%s\n' "$upload/generation.json"
      elif [[ -d "$installed" && ! -L "$installed" \
        && ! -e "$upload" && ! -L "$upload" ]]; then
        printf '%s\n' "$installed/generation.json"
      else
        echo 'prepared pair target must exist in exactly one location' >&2
        return 1
      fi
      ;;
    installed)
      [[ -d "$installed" && ! -L "$installed" \
        && ! -e "$upload" && ! -L "$upload" ]] || return 1
      printf '%s\n' "$installed/generation.json"
      ;;
  esac
}

pair_validate_recoverable_live_state() {
  local pointer candidate_manifest container_state running_image_id
  ordinary_require_static_live_metadata
  ordinary_current_file_matches "$IMAGE_FILE" \
    "$TRANSACTION/saved-image" "$TRANSACTION/target-image"
  ordinary_current_file_matches "$QUADLET" \
    "$TRANSACTION/saved-quadlet" "$TRANSACTION/target-quadlet"
  ordinary_current_file_matches "$TEMPLATE" \
    "$TRANSACTION/saved-template" "$TRANSACTION/target-template"
  cmp --silent "$RUNTIME_ENV" "$TRANSACTION/saved-runtime.env"
  cmp --silent "$API_KEYS" "$TRANSACTION/saved-api-keys.json"
  cmp --silent "$CADDYFILE" "$TRANSACTION/saved-Caddyfile"
  image_path_is_absent "$AUTH_READY"
  pointer="$(</srv/legal-mcp/lifecycle/active-generation)"
  [[ "$pointer" = "$PAIR_PRIOR_GENERATION" \
    || "$pointer" = "$PAIR_TARGET_GENERATION" ]] || {
      echo 'pair recovery found a generation outside the recorded pair' >&2
      return 1
    }
  [[ "$(sha256sum "/srv/legal-mcp/generations/$PAIR_PRIOR_GENERATION/generation.json" \
    | awk '{print $1}')" = "$PAIR_PRIOR_MANIFEST_SHA256" ]] || return 1
  pair_validate_manifest_file \
    "/srv/legal-mcp/generations/$PAIR_PRIOR_GENERATION/generation.json" \
    root legal-mcp 440
  candidate_manifest="$(pair_candidate_manifest)" || return 1
  [[ "$(sha256sum "$candidate_manifest" | awk '{print $1}')" \
    = "$PAIR_TARGET_MANIFEST_SHA256" ]] || return 1
  if [[ "$candidate_manifest" == /srv/legal-mcp/uploads/* ]]; then
    local owner group mode
    read -r owner group mode < <(stat -c '%U %G %a' "$candidate_manifest")
    case "$owner:$group:$mode" in
      root:legal-mcp:600|root:legal-mcp:440|\
      legal-mcp-publisher:legal-mcp-publisher:600|\
      legal-mcp-publisher:legal-mcp-publisher:440) ;;
      *)
        echo 'pair upload manifest is outside its recoverable ownership states' >&2
        return 1
        ;;
    esac
  else
    pair_validate_manifest_file "$candidate_manifest" root legal-mcp 440
  fi
  if [[ "$PAIR_OPERATION" = prepared ]]; then
    if ! image_path_is_absent /srv/legal-mcp/lifecycle/.deployment-transaction; then
      pair_read_deployment_journal
      [[ "$PAIR_DEPLOYMENT_GENERATION" = "$PAIR_TARGET_GENERATION" \
        && "$PAIR_DEPLOYMENT_PREVIOUS" = "$PAIR_PRIOR_GENERATION" ]] || return 1
    else
      [[ "$PAIR_RETIREMENT_OUTCOME" = target \
        && "$pointer" = "$PAIR_TARGET_GENERATION" ]] || return 1
    fi
  else
    image_path_is_absent /srv/legal-mcp/lifecycle/.deployment-transaction
  fi
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(read_systemctl_activity "$SERVICE")" = inactive \
    && "$(read_systemctl_enablement caddy.service)" = disabled \
    && "$(read_systemctl_activity caddy.service)" = inactive \
    && "$(ufw_rule_state 80)" = absent \
    && "$(ufw_rule_state 443)" = absent ]] || return 1
  ordinary_require_listener_topology none
  container_state="$(podman_container_state australian-legal-mcp)" || return 1
  if [[ "$container_state" = present ]]; then
    running_image_id="$(canonical_image_id \
      "$(podman inspect australian-legal-mcp --format '{{.Image}}')")" || return 1
    [[ "$running_image_id" = "$PAIR_PRIOR_IMAGE_ID" \
      || "$running_image_id" = "$PAIR_TARGET_IMAGE_ID" ]] || return 1
  fi
  ufw_is_fail_closed
}

pair_reconcile_live_file_preparations() {
  local path expected_mode
  for path in /etc/legal-mcp/.pair-image.preparing \
    /etc/containers/systemd/.pair-quadlet.preparing \
    /usr/local/libexec/legal-mcp/.pair-template.preparing; do
    image_path_is_absent "$path" && continue
    if [[ "$path" = /etc/legal-mcp/.pair-image.preparing ]]; then
      expected_mode=600
    else
      expected_mode=644
    fi
    [[ -f "$path" && ! -L "$path" \
      && "$(stat -c '%U:%G:%a:%h' "$path")" = "root:root:$expected_mode:1" ]] || {
        echo "unsafe pair live-file preparation: $path" >&2
        return 1
      }
    rm -f -- "$path"
    image_path_is_absent "$path" || return 1
    sync -f "$(dirname "$path")"
  done
}

pair_install_live_file() {
  local source="$1" destination="$2" mode="$3" preparation
  case "$destination" in
    "$IMAGE_FILE") preparation=/etc/legal-mcp/.pair-image.preparing ;;
    "$QUADLET") preparation=/etc/containers/systemd/.pair-quadlet.preparing ;;
    "$TEMPLATE") preparation=/usr/local/libexec/legal-mcp/.pair-template.preparing ;;
    *) return 1 ;;
  esac
  image_path_is_absent "$preparation" || return 1
  install -o root -g root -m "$mode" "$source" "$preparation"
  sync -f "$preparation"
  mv -fT "$preparation" "$destination"
  sync -f "$(dirname "$destination")"
}

pair_install_files() {
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
  pair_reconcile_live_file_preparations
  pair_install_live_file "$image_source" "$IMAGE_FILE" 600
  pair_install_live_file "$quadlet_source" "$QUADLET" 644
  pair_install_live_file "$template_source" "$TEMPLATE" 644
  systemctl daemon-reload
  cmp --silent "$image_source" "$IMAGE_FILE"
  cmp --silent "$quadlet_source" "$QUADLET"
  cmp --silent "$template_source" "$TEMPLATE"
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated ]]
}

pair_require_image_runtime() {
  local image="$1" image_id="$2" version="$3" revision="$4" state resolved
  state="$(podman_image_state "$image")" || return 1
  [[ "$state" = present ]] || podman pull "$image"
  resolved="$(canonical_image_id \
    "$(podman image inspect "$image" --format '{{.Id}}')")" || return 1
  [[ "$resolved" = "$image_id" ]] || {
    echo 'pair image pin no longer resolves to its recorded image ID' >&2
    return 1
  }
  verify_image_runtime "$image" "$version" "$revision"
}

pair_verify_generation_view() {
  local image="$1" view_generation="$2" candidate="$3" status
  pair_cleanup_verification_root
  install -d -o 971 -g 971 -m 0700 "$PAIR_VERIFY_ROOT"
  install -d -o 971 -g 971 -m 0750 "$PAIR_VERIFY_ROOT/lifecycle"
  install -d -o 971 -g 971 -m 0700 "$PAIR_VERIFY_ROOT/state"
  install -o 971 -g 971 -m 0640 /dev/null "$PAIR_VERIFY_ROOT/lifecycle/LOCK"
  printf '%s' "$view_generation" > "$PAIR_VERIFY_ROOT/lifecycle/active-generation"
  chown 971:971 "$PAIR_VERIFY_ROOT/lifecycle/active-generation"
  chmod 644 "$PAIR_VERIFY_ROOT/lifecycle/active-generation"
  sync -f "$PAIR_VERIFY_ROOT/lifecycle"
  set +e
  podman run --rm --network=none --user=971:971 --read-only --cap-drop=all \
    --security-opt=no-new-privileges --pids-limit=256 --memory=6g --memory-swap=6g \
    --tmpfs=/tmp:rw,nodev,nosuid,noexec,size=64m,mode=1777 \
    --volume="$candidate:/var/lib/legal-mcp/generations/$view_generation:ro,nodev,nosuid,noexec" \
    --volume="$PAIR_VERIFY_ROOT/lifecycle:/var/lib/legal-mcp/lifecycle:ro,nodev,nosuid,noexec" \
    --volume="$PAIR_VERIFY_ROOT/state:/var/lib/legal-mcp/state:rw,nodev,nosuid,noexec" \
    "$image" verify --quiet >/dev/null
  status=$?
  set -e
  pair_cleanup_verification_root
  return "$status"
}

pair_require_incompatible_target() {
  if podman run --rm --network=none --user=971:971 --read-only --cap-drop=all \
    --security-opt=no-new-privileges --pids-limit=256 --memory=6g --memory-swap=6g \
    --tmpfs=/tmp:rw,nodev,nosuid,noexec,size=64m,mode=1777 \
    --volume=/srv/legal-mcp:/var/lib/legal-mcp:ro,nodev,nosuid,noexec \
    "$NEW_IMAGE" verify --quiet >/dev/null 2>&1; then
    echo 'target image accepts the current generation; use ordinary same-schema routes' >&2
    return 1
  fi
}

pair_call_host_deploy() {
  LEGAL_MCP_HOST_TRANSACTION_LOCK_FD="$HOST_LOCK_FD" \
  LEGAL_MCP_PAIR_COORDINATOR=1 \
    /usr/local/sbin/legal-mcp-host-deploy "$1" "$PAIR_TARGET_GENERATION"
}

pair_arm_private_start() {
  local temporary
  bootstrap_require_regular "$PAIR_PERMIT" root root 400 || return 1
  image_path_is_absent "$PAIR_START_ARM" || return 1
  temporary="$(mktemp /run/legal-mcp/.pair-cutover-start-armed.XXXXXX)"
  install -o root -g root -m 0400 "$PAIR_PERMIT" "$temporary"
  sync -f "$temporary"
  mv -fT "$temporary" "$PAIR_START_ARM"
  sync -f /run/legal-mcp
  bootstrap_require_regular "$PAIR_START_ARM" root root 400
  cmp --silent "$PAIR_PERMIT" "$PAIR_START_ARM"
}

pair_disarm_private_start() {
  image_path_is_absent "$PAIR_START_ARM" && return 0
  bootstrap_require_regular "$PAIR_START_ARM" root root 400 || return 1
  rm -f -- "$PAIR_START_ARM"
  sync -f /run/legal-mcp
  image_path_is_absent "$PAIR_START_ARM"
}

pair_verify_running_capabilities() {
  local report
  report="$(podman top australian-legal-mcp capbnd capeff capinh capprm)" || return 1
  python3 - "$report" <<'PY'
import sys
lines = sys.argv[1].splitlines()
if not lines or " ".join(lines[0].split()) != "BOUNDING CAPS EFFECTIVE CAPS INHERITED CAPS PERMITTED CAPS" or len(lines) < 2:
    raise SystemExit(1)
for line in lines[1:]:
    if line.split() != ["none", "none", "none", "none"]:
        raise SystemExit(1)
PY
}

pair_verify_running_constraints() {
  local expected_image_id="$1" image_id user root_read_only network port mounts binds
  image_id="$(canonical_image_id \
    "$(podman inspect australian-legal-mcp --format '{{.Image}}')")" || return 1
  user="$(podman inspect australian-legal-mcp --format '{{.Config.User}}')" || return 1
  root_read_only="$(podman inspect australian-legal-mcp --format '{{.HostConfig.ReadonlyRootfs}}')" \
    || return 1
  network="$(podman inspect australian-legal-mcp --format '{{.HostConfig.NetworkMode}}')" \
    || return 1
  port="$(podman port australian-legal-mcp 51235/tcp)" || return 1
  mounts="$(podman inspect australian-legal-mcp --format '{{json .Mounts}}')" || return 1
  binds="$(podman inspect australian-legal-mcp --format '{{json .HostConfig.Binds}}')" || return 1
  [[ "$image_id" = "$expected_image_id" && "$user" = 971:971 \
    && "$root_read_only" = true && "$network" = bridge \
    && "$port" = 127.0.0.1:51235 ]] || {
      echo 'running pair container violates its exact image, identity, or loopback contract' >&2
      return 1
    }
  pair_verify_running_capabilities
  python3 - "$mounts" "$binds" <<'PY'
import json, sys
mounts = json.loads(sys.argv[1])
binds = json.loads(sys.argv[2])
expected = {
    "/var/lib/legal-mcp/generations": ("/srv/legal-mcp/generations", False),
    "/var/lib/legal-mcp/lifecycle": ("/srv/legal-mcp/lifecycle", False),
    "/var/lib/legal-mcp/state": ("/srv/legal-mcp/state", True),
    "/run/secrets/legal-mcp-api-keys.json": ("/etc/legal-mcp/api-keys.json", False),
}
expected_binds = {
    "/var/lib/legal-mcp/generations": (
        "/srv/legal-mcp/generations", {"ro", "nodev", "nosuid", "noexec"}
    ),
    "/var/lib/legal-mcp/lifecycle": (
        "/srv/legal-mcp/lifecycle", {"ro", "nodev", "nosuid", "noexec"}
    ),
    "/var/lib/legal-mcp/state": (
        "/srv/legal-mcp/state", {"rw", "nodev", "nosuid", "noexec"}
    ),
    "/run/secrets/legal-mcp-api-keys.json": (
        "/etc/legal-mcp/api-keys.json", {"ro", "nodev", "nosuid", "noexec"}
    ),
}
podman_bind_options = {"rbind", "rprivate"}
seen = {}
for item in mounts:
    destination = item.get("Destination")
    if destination in expected:
        if destination in seen:
            raise SystemExit(1)
        seen[destination] = (item.get("Source"), item.get("RW"))
if seen != expected or len(mounts) != len(expected):
    raise SystemExit(1)
if not isinstance(binds, list) or len(binds) != len(expected_binds):
    raise SystemExit(1)
seen_binds = set()
for bind in binds:
    if not isinstance(bind, str):
        raise SystemExit(1)
    parts = bind.split(":", 2)
    if len(parts) != 3:
        raise SystemExit(1)
    source, destination, raw_options = parts
    if destination in seen_binds or destination not in expected_binds:
        raise SystemExit(1)
    seen_binds.add(destination)
    expected_source, required_options = expected_binds[destination]
    options = raw_options.split(",")
    option_set = set(options)
    if (source != expected_source or len(options) != len(option_set)
            or not required_options.issubset(option_set)
            or not option_set.difference(required_options).issubset(podman_bind_options)):
        raise SystemExit(1)
if seen_binds != set(expected_binds):
    raise SystemExit(1)
PY
}

pair_verify_dark() {
  local choice="$1" expected_manifest expected_image expected_id expected_generation
  local version revision manifest
  case "$choice" in
    saved)
      expected_manifest="$TRANSACTION/saved-sha256"
      expected_image="$PAIR_PRIOR_IMAGE"
      expected_id="$PAIR_PRIOR_IMAGE_ID"
      expected_generation="$PAIR_PRIOR_GENERATION"
      version="$PAIR_PRIOR_VERSION"
      revision="$PAIR_PRIOR_REVISION"
      ;;
    target)
      expected_manifest="$TRANSACTION/target-sha256"
      expected_image="$PAIR_TARGET_IMAGE"
      expected_id="$PAIR_TARGET_IMAGE_ID"
      expected_generation="$PAIR_TARGET_GENERATION"
      version="$PAIR_TARGET_VERSION"
      revision="$PAIR_TARGET_REVISION"
      ;;
    *) return 1 ;;
  esac
  ordinary_require_static_live_metadata
  manifest="$(mktemp /run/legal-mcp-pair-live.XXXXXX)"
  ordinary_render_hash_manifest "$IMAGE_FILE" "$QUADLET" "$TEMPLATE" \
    "$RUNTIME_ENV" "$API_KEYS" "$CADDYFILE" "$TRANSACTION/saved-auth-ready" \
    /srv/legal-mcp/lifecycle/active-generation "$manifest"
  if ! cmp --silent "$manifest" "$expected_manifest"; then
    rm -f "$manifest"
    echo 'configured-dark pair does not match its exact transaction manifest' >&2
    return 1
  fi
  rm -f "$manifest"
  pair_require_image_runtime "$expected_image" "$expected_id" "$version" "$revision"
  [[ "$(</srv/legal-mcp/lifecycle/active-generation)" = "$expected_generation" \
    && "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(read_systemctl_activity "$SERVICE")" = inactive \
    && "$(read_systemctl_enablement caddy.service)" = disabled \
    && "$(read_systemctl_activity caddy.service)" = inactive \
    && "$(ufw_rule_state 80)" = absent && "$(ufw_rule_state 443)" = absent ]]
  image_path_is_absent "$AUTH_READY" && image_path_is_absent "$PAIR_START_ARM"
  ordinary_require_listener_topology none
  ufw_is_fail_closed
  pair_validate_mount_contract
}

pair_start_and_prove() {
  local choice="$1" expected_id expected_generation
  case "$choice" in
    saved)
      expected_id="$PAIR_PRIOR_IMAGE_ID"
      expected_generation="$PAIR_PRIOR_GENERATION"
      ;;
    target)
      expected_id="$PAIR_TARGET_IMAGE_ID"
      expected_generation="$PAIR_TARGET_GENERATION"
      ;;
    *) return 1 ;;
  esac
  TRANSACTION_GENERATION="$expected_generation"
  TRANSACTION_AUTH_MODE="$PAIR_AUTH_MODE"
  TRANSACTION_EXTERNAL_URL="$PAIR_EXTERNAL_URL"
  load_runtime_contract "$TRANSACTION/saved-runtime.env"
  pair_arm_private_start
  systemctl start "$SERVICE"
  ordinary_verify_private_runtime "$expected_id"
  pair_verify_running_constraints "$expected_id"
  systemctl stop "$SERVICE"
  [[ "$(read_systemctl_activity "$SERVICE")" = inactive ]]
  pair_disarm_private_start
  pair_verify_dark "$choice"
}

pair_verify_offline() {
  local choice="$1" image image_id version revision generation manifest_sha
  case "$choice" in
    saved)
      image="$PAIR_PRIOR_IMAGE"; image_id="$PAIR_PRIOR_IMAGE_ID"
      version="$PAIR_PRIOR_VERSION"; revision="$PAIR_PRIOR_REVISION"
      generation="$PAIR_PRIOR_GENERATION"; manifest_sha="$PAIR_PRIOR_MANIFEST_SHA256"
      ;;
    target)
      image="$PAIR_TARGET_IMAGE"; image_id="$PAIR_TARGET_IMAGE_ID"
      version="$PAIR_TARGET_VERSION"; revision="$PAIR_TARGET_REVISION"
      generation="$PAIR_TARGET_GENERATION"; manifest_sha="$PAIR_TARGET_MANIFEST_SHA256"
      ;;
    *) return 1 ;;
  esac
  pair_require_image_runtime "$image" "$image_id" "$version" "$revision"
  [[ "$(</srv/legal-mcp/lifecycle/active-generation)" = "$generation" \
    && "$(sha256sum "/srv/legal-mcp/generations/$generation/generation.json" \
      | awk '{print $1}')" = "$manifest_sha" ]]
  podman run --rm --network=none --user=971:971 --read-only --cap-drop=all \
    --security-opt=no-new-privileges --pids-limit=256 --memory=6g --memory-swap=6g \
    --tmpfs=/tmp:rw,nodev,nosuid,noexec,size=64m,mode=1777 \
    --volume=/srv/legal-mcp:/var/lib/legal-mcp:ro,nodev,nosuid,noexec \
    "$image" verify --quiet >/dev/null
}

pair_read_transaction_probe_key() {
  load_runtime_contract "$TRANSACTION/saved-runtime.env"
  [[ "$AUTH_MODE" = "$PAIR_AUTH_MODE" && "$EXTERNAL_URL" = "$PAIR_EXTERNAL_URL" ]]
  if [[ "$HAS_API" = true \
    && "${PROBE_API_KEY:-}" =~ ^[a-z0-9][a-z0-9_-]{0,63}\.[A-Za-z0-9_-]{43}$ ]]; then
    return 0
  fi
  read_probe_key
}

pair_verify_committed() {
  local choice="$1"
  pair_verify_dark "$choice"
  case "$PAIR_OPERATION:$choice" in
    prepared:saved)
      pair_read_deployment_journal
      [[ "$PAIR_DEPLOYMENT_GENERATION" = "$PAIR_TARGET_GENERATION" \
        && "$PAIR_DEPLOYMENT_PREVIOUS" = "$PAIR_PRIOR_GENERATION" \
        && "$PAIR_DEPLOYMENT_PHASE" = prepared \
        && -d "/srv/legal-mcp/uploads/$PAIR_TARGET_GENERATION" \
        && ! -L "/srv/legal-mcp/uploads/$PAIR_TARGET_GENERATION" \
        && "$(stat -c '%U:%G:%a' "/srv/legal-mcp/uploads/$PAIR_TARGET_GENERATION")" \
          = legal-mcp-publisher:legal-mcp-publisher:700 \
        && ! -e "/srv/legal-mcp/generations/$PAIR_TARGET_GENERATION" \
        && ! -L "/srv/legal-mcp/generations/$PAIR_TARGET_GENERATION" ]]
      pair_upload_authorization_matches
      ;;
    prepared:target)
      image_path_is_absent /srv/legal-mcp/lifecycle/.deployment-transaction
      image_path_is_absent /run/legal-mcp/authorized-upload
      [[ -d "/srv/legal-mcp/generations/$PAIR_PRIOR_GENERATION" \
        && ! -L "/srv/legal-mcp/generations/$PAIR_PRIOR_GENERATION" \
        && -d "/srv/legal-mcp/generations/$PAIR_TARGET_GENERATION" \
        && ! -L "/srv/legal-mcp/generations/$PAIR_TARGET_GENERATION" ]]
      ;;
    installed:saved|installed:target)
      image_path_is_absent /srv/legal-mcp/lifecycle/.deployment-transaction
      image_path_is_absent /run/legal-mcp/authorized-upload
      [[ -d "/srv/legal-mcp/generations/$PAIR_PRIOR_GENERATION" \
        && ! -L "/srv/legal-mcp/generations/$PAIR_PRIOR_GENERATION" \
        && -d "/srv/legal-mcp/generations/$PAIR_TARGET_GENERATION" \
        && ! -L "/srv/legal-mcp/generations/$PAIR_TARGET_GENERATION" ]]
      ;;
    *) return 1 ;;
  esac
}

pair_retired_payload_state() {
  retired_image_payload_state "$1" \
    kind operation prior-generation target-generation phase retirement-outcome \
    coordinator-version coordinator-revision updater-sha256 release-sha256 \
    saved-sha256 target-sha256 saved-metadata target-metadata state \
    saved-image saved-quadlet saved-template saved-runtime.env saved-api-keys.json \
    saved-Caddyfile saved-auth-ready saved-active-generation saved-deployment-journal \
    saved-upload-authorization target-image target-quadlet target-template \
    target-active-generation
}

pair_validate_deletion_marker() {
  bootstrap_require_regular "$TRANSACTION_DELETION" root root 600 || return 1
  [[ "$(<"$TRANSACTION_DELETION")" \
    = LEGAL_MCP_IMAGE_GENERATION_PAIR_TRANSACTION_V1 ]] || {
      echo 'pair deletion marker has missing, corrupt, or foreign identity' >&2
      return 1
    }
}

pair_delete_retired_transaction() {
  local found victim
  if ! image_path_is_absent "$TRANSACTION_DELETION"; then
    pair_validate_deletion_marker
    if ! image_path_is_absent "$TRANSACTION_RETIRED"; then
      require_image_transaction_directory "$TRANSACTION_RETIRED"
      found="$(find "$TRANSACTION_RETIRED" -mindepth 1 -maxdepth 1 -print -quit)"
      [[ -z "$found" ]] || {
        echo 'pair deletion marker conflicts with non-empty retired state' >&2
        return 1
      }
      rmdir "$TRANSACTION_RETIRED"
      sync -f /etc/legal-mcp
    fi
    rm -f -- "$TRANSACTION_DELETION"
    image_path_is_absent "$TRANSACTION_DELETION" || return 1
    sync -f /etc/legal-mcp
    return 0
  fi

  require_image_transaction_directory "$TRANSACTION_RETIRED"
  bootstrap_directory_contains_only "$TRANSACTION_RETIRED" \
    kind operation prior-generation target-generation phase retirement-outcome \
    coordinator-version coordinator-revision updater-sha256 release-sha256 \
    saved-sha256 target-sha256 saved-metadata target-metadata state \
    saved-image saved-quadlet saved-template saved-runtime.env saved-api-keys.json \
    saved-Caddyfile saved-auth-ready saved-active-generation saved-deployment-journal \
    saved-upload-authorization target-image target-quadlet target-template \
    target-active-generation || return 1
  bootstrap_require_regular "$TRANSACTION_RETIRED/kind" root root 600 || return 1
  [[ "$(<"$TRANSACTION_RETIRED/kind")" \
    = LEGAL_MCP_IMAGE_GENERATION_PAIR_TRANSACTION_V1 ]] || {
      echo 'retired pair transaction identity is missing, corrupt, or foreign' >&2
      return 1
    }
  while true; do
    victim="$(find "$TRANSACTION_RETIRED" -mindepth 1 -maxdepth 1 \
      ! -name kind -print -quit)"
    [[ -n "$victim" ]] || break
    rm -rf --one-file-system -- "$victim"
    image_path_is_absent "$victim" || return 1
    sync -f "$TRANSACTION_RETIRED"
  done
  mv -T "$TRANSACTION_RETIRED/kind" "$TRANSACTION_DELETION"
  sync -f /etc/legal-mcp
  pair_validate_deletion_marker
  rmdir "$TRANSACTION_RETIRED"
  sync -f /etc/legal-mcp
  rm -f -- "$TRANSACTION_DELETION"
  image_path_is_absent "$TRANSACTION_DELETION" || return 1
  sync -f /etc/legal-mcp
}

pair_complete_retired_transaction() {
  local payload_state saved_transaction="$TRANSACTION"
  payload_state="$(pair_retired_payload_state "$TRANSACTION_RETIRED")"
  if [[ "$payload_state" = complete ]]; then
    TRANSACTION="$TRANSACTION_RETIRED"
    pair_validate_transaction "$TRANSACTION"
    [[ "$PAIR_RETIREMENT_OUTCOME" = saved || "$PAIR_RETIREMENT_OUTCOME" = target ]]
    pair_read_transaction_probe_key
    pair_verify_committed "$PAIR_RETIREMENT_OUTCOME"
  fi
  pair_delete_retired_transaction
  TRANSACTION="$saved_transaction"
}

pair_complete_retiring_transaction() {
  local saved_transaction="$TRANSACTION"
  TRANSACTION="$TRANSACTION_RETIRING"
  pair_validate_transaction "$TRANSACTION"
  [[ "$PAIR_RETIREMENT_OUTCOME" = saved || "$PAIR_RETIREMENT_OUTCOME" = target ]]
  pair_read_transaction_probe_key
  pair_verify_committed "$PAIR_RETIREMENT_OUTCOME"
  image_path_is_absent "$TRANSACTION_RETIRED"
  mv -T "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"
  sync -f /etc/legal-mcp
  TRANSACTION="$saved_transaction"
  pair_complete_retired_transaction
}

pair_retire_transaction() {
  local choice="$1"
  pair_validate_transaction "$TRANSACTION"
  [[ "$PAIR_RETIREMENT_OUTCOME" = "$choice" ]]
  pair_verify_committed "$choice"
  image_path_is_absent "$TRANSACTION_RETIRING"
  image_path_is_absent "$TRANSACTION_RETIRED"
  mv -T "$TRANSACTION" "$TRANSACTION_RETIRING"
  sync -f /etc/legal-mcp
  pair_complete_retiring_transaction
}

pair_restore_saved() {
  pair_force_dark
  pair_validate_recoverable_live_state
  pair_write_phase committing
  pair_install_files saved
  pair_call_host_deploy pair-restore >/dev/null
  pair_verify_offline saved
  pair_start_and_prove saved
  pair_write_outcome saved
  pair_call_host_deploy pair-commit >/dev/null
  pair_restore_upload_authorization
  pair_write_phase committed
  pair_verify_committed saved
  pair_retire_transaction saved
}

pair_finish_target() {
  pair_force_dark
  pair_validate_recoverable_live_state
  pair_write_phase target-files
  pair_install_files target
  if [[ "$(</srv/legal-mcp/lifecycle/active-generation)" != "$PAIR_TARGET_GENERATION" ]]; then
    pair_write_phase activating
    pair_call_host_deploy pair-activate >/dev/null
  elif [[ "$PAIR_OPERATION" = prepared \
    && ! -e /srv/legal-mcp/lifecycle/.deployment-transaction ]]; then
    [[ "$PAIR_RETIREMENT_OUTCOME" = target ]] || return 1
  fi
  pair_write_phase activated
  pair_verify_offline target
  pair_write_phase verifying
  pair_start_and_prove target
  pair_write_phase proved
  pair_write_outcome target
  pair_write_phase committing
  if [[ "$PAIR_OPERATION" = installed \
    || -e /srv/legal-mcp/lifecycle/.deployment-transaction ]]; then
    pair_call_host_deploy pair-commit >/dev/null
  fi
  pair_write_phase committed
  pair_verify_committed target
  pair_retire_transaction target
}

pair_failure_recovery() {
  local status=$? recovery_status
  trap - ERR HUP INT TERM EXIT
  set +e
  (
    set -e
    pair_reconcile_field_preparations "$TRANSACTION"
    pair_reconcile_live_file_preparations
    pair_validate_transaction "$TRANSACTION"
    pair_read_transaction_probe_key
    case "$PAIR_RETIREMENT_OUTCOME" in
      target) pair_finish_target ;;
      saved|pending) pair_restore_saved ;;
      *) exit 1 ;;
    esac
  )
  recovery_status=$?
  set -e
  unset PROBE_API_KEY
  if [[ $recovery_status -ne 0 ]]; then
    echo 'pair transition failed; service and ingress remain off and explicit pair recovery is required' >&2
    exit 1
  fi
  if [[ "$PAIR_RETIREMENT_OUTCOME" = target ]]; then
    echo 'pair transition had committed; recovery completed the target pair' >&2
  else
    echo 'pair transition rolled back the image and generation together' >&2
  fi
  exit "$status"
}

pair_force_preparation_dark() {
  local activity
  if ! image_path_is_absent "$PAIR_START_ARM"; then
    bootstrap_require_regular "$PAIR_START_ARM" root root 400 || return 1
    rm -f -- "$PAIR_START_ARM"
    sync -f /run/legal-mcp
  fi
  if ! image_path_is_absent "$AUTH_READY"; then
    ordinary_require_regular "$AUTH_READY" root root 444 || return 1
    [[ "$(stat -c '%s' "$AUTH_READY")" = 0 \
      && "$(getfacl --absolute-names --numeric --omit-header "$AUTH_READY")" \
        = $'user::r--\ngroup::r--\nother::r--' ]] || return 1
    rm -f -- "$AUTH_READY"
    sync -f /etc/legal-mcp
  fi
  close_ingress
  activity="$(read_systemctl_activity "$SERVICE")" || return 1
  if [[ "$activity" = active ]]; then systemctl stop "$SERVICE"; fi
  pair_cleanup_verification_root
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(read_systemctl_activity "$SERVICE")" = inactive \
    && "$(read_systemctl_enablement caddy.service)" = disabled \
    && "$(read_systemctl_activity caddy.service)" = inactive \
    && "$(ufw_rule_state 80)" = absent \
    && "$(ufw_rule_state 443)" = absent ]] || return 1
  image_path_is_absent "$AUTH_READY" && image_path_is_absent "$PAIR_START_ARM"
  ordinary_require_listener_topology none
  ufw_is_fail_closed
}

pair_discard_preparation() {
  local path="$1" found owner_kind=''
  require_image_transaction_directory "$path"
  pair_retired_payload_state "$path" >/dev/null
  found="$(find "$path" -mindepth 1 -maxdepth 1 -print -quit)" || return 1
  if [[ -n "$found" ]]; then
    if [[ -f "$path/kind" && ! -L "$path/kind" ]]; then
      ordinary_require_regular "$path/kind" root root 600 || return 1
      owner_kind="$(<"$path/kind")"
      [[ "$owner_kind" = LEGAL_MCP_IMAGE_GENERATION_PAIR_TRANSACTION_V1 ]] || {
        echo 'pair recovery refuses a preparation owned by another image operation' >&2
        return 1
      }
    else
      echo 'non-empty image preparation has no recoverable owner marker' >&2
      return 1
    fi
  fi
  pair_force_preparation_dark
  retire_image_directory_for_deletion "$path" "$TRANSACTION_PREPARING_RETIRED" \
    "$TRANSACTION_PREPARING_DELETION" \
    LEGAL_MCP_IMAGE_GENERATION_PAIR_TRANSACTION_V1
}

pair_recover_pending_state() {
  local saved_transaction="$TRANSACTION" payload_state path state_count=0
  if ! image_path_is_absent "$TRANSACTION_PREPARING_DELETION"; then
    for path in "$TRANSACTION_PREPARING" "$TRANSACTION" \
      "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED" "$TRANSACTION_DELETION"; do
      image_path_is_absent "$path" || {
        echo 'pair preparation deletion conflicts with another transaction phase' >&2
        return 1
      }
    done
    pair_force_preparation_dark
    delete_owned_image_transaction_directory "$TRANSACTION_PREPARING_RETIRED" \
      "$TRANSACTION_PREPARING_DELETION" \
      LEGAL_MCP_IMAGE_GENERATION_PAIR_TRANSACTION_V1
    echo 'interrupted pair transaction preparation retirement completed'
    return 0
  fi
  for path in "$TRANSACTION_PREPARING" "$TRANSACTION_PREPARING_RETIRED" \
    "$TRANSACTION" "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"; do
    image_path_is_absent "$path" || state_count=$((state_count + 1))
  done
  if ! image_path_is_absent "$TRANSACTION_DELETION"; then
    if (( state_count > 1 )) \
      || { (( state_count == 1 )) \
        && image_path_is_absent "$TRANSACTION_RETIRED"; }; then
      echo 'pair deletion marker conflicts with another transaction phase' >&2
      return 1
    fi
    pair_delete_retired_transaction
    echo 'interrupted pair transaction retirement completed'
    return 0
  fi
  [[ $state_count -le 1 ]] || {
    echo 'pair transaction has conflicting durable phases' >&2
    return 1
  }
  if ! image_path_is_absent "$TRANSACTION_PREPARING" \
    && ! image_path_is_absent "$TRANSACTION_PREPARING_RETIRED"; then
    echo 'pair preparation has conflicting recovery states' >&2
    return 1
  fi
  if ! image_path_is_absent "$TRANSACTION_RETIRING" \
    && ! image_path_is_absent "$TRANSACTION_RETIRED"; then
    echo 'pair retirement has conflicting recovery states' >&2
    return 1
  fi
  if ! image_path_is_absent "$TRANSACTION_PREPARING"; then
    pair_discard_preparation "$TRANSACTION_PREPARING"
    echo 'interrupted pair transaction preparation discarded before host mutation'
    return 0
  fi
  if ! image_path_is_absent "$TRANSACTION_PREPARING_RETIRED" \
    || ! image_path_is_absent "$TRANSACTION_PREPARING_DELETION"; then
    pair_force_preparation_dark
    if image_path_is_absent "$TRANSACTION_PREPARING_DELETION"; then
      pair_retired_payload_state "$TRANSACTION_PREPARING_RETIRED" >/dev/null
    fi
    delete_owned_image_transaction_directory "$TRANSACTION_PREPARING_RETIRED" \
      "$TRANSACTION_PREPARING_DELETION" \
      LEGAL_MCP_IMAGE_GENERATION_PAIR_TRANSACTION_V1
    echo 'interrupted pair transaction preparation retirement completed'
    return 0
  fi
  if ! image_path_is_absent "$TRANSACTION_RETIRING"; then
    pair_complete_retiring_transaction
    echo 'interrupted pair transaction retirement completed'
    return 0
  fi
  if ! image_path_is_absent "$TRANSACTION_RETIRED"; then
    payload_state="$(pair_retired_payload_state "$TRANSACTION_RETIRED")"
    if [[ "$payload_state" = complete ]]; then
      TRANSACTION="$TRANSACTION_RETIRED"
      pair_validate_transaction "$TRANSACTION"
      TRANSACTION="$saved_transaction"
    fi
    pair_complete_retired_transaction
    echo 'interrupted pair transaction retirement completed'
    return 0
  fi
  if image_path_is_absent "$TRANSACTION"; then
    if [[ "$PAIR_BUILD_WAS_PENDING" = true ]]; then
      pair_force_preparation_dark
      echo 'interrupted unpublished pair preparation discarded before host mutation'
      return 0
    fi
    echo 'no pair transaction exists' >&2
    return 1
  fi
  bootstrap_require_regular "$TRANSACTION/kind" root root 600
  [[ "$(<"$TRANSACTION/kind")" \
    = LEGAL_MCP_IMAGE_GENERATION_PAIR_TRANSACTION_V1 ]] || {
      echo 'pair recovery refuses a transaction owned by another image operation' >&2
      return 1
    }
  pair_reconcile_live_file_preparations
  pair_reconcile_field_preparations "$TRANSACTION"
  pair_validate_transaction "$TRANSACTION"
  pair_read_transaction_probe_key
  case "$PAIR_RETIREMENT_OUTCOME" in
    target)
      pair_finish_target
      echo 'interrupted pair transaction completed the committed target pair'
      ;;
    saved)
      pair_restore_saved
      echo 'interrupted pair transaction completed the committed saved pair'
      ;;
    pending)
      pair_restore_saved
      echo 'interrupted pair transaction rolled back the image and generation together'
      ;;
  esac
  unset PROBE_API_KEY
}

run_pair_operation() {
  [[ "$PAIR_GENERATION" =~ ^[0-9a-f]{64}$ \
    && "$PAIR_EXPECTED_CURRENT_GENERATION" =~ ^[0-9a-f]{64}$ \
    && "$PAIR_GENERATION" != "$PAIR_EXPECTED_CURRENT_GENERATION" \
    && "$NEW_IMAGE" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ \
    && "$EXPECTED_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ \
    && -n "$SOURCE_TEMPLATE" ]] || usage
  pair_require_no_foreign_transaction false
  ordinary_load_release_bundle '' ''
  pair_load_target_bundle "$SOURCE_TEMPLATE" "$EXPECTED_VERSION"
  pair_capture_baseline
  podman pull "$NEW_IMAGE"
  verify_image_runtime "$NEW_IMAGE" "$PAIR_TARGET_VERSION" "$PAIR_TARGET_REVISION"
  PAIR_TARGET_IMAGE_ID="$(canonical_image_id \
    "$(podman image inspect "$NEW_IMAGE" --format '{{.Id}}')")"
  pair_require_incompatible_target
  pair_create_transaction
  trap pair_failure_recovery ERR HUP INT TERM EXIT
  pair_write_phase darkening
  pair_force_dark
  pair_write_phase dark
  pair_validate_recoverable_live_state
  pair_write_phase sealing
  pair_call_host_deploy pair-seal >/dev/null
  candidate_manifest="$(pair_candidate_manifest)"
  [[ "$(sha256sum "$candidate_manifest" | awk '{print $1}')" \
    = "$PAIR_TARGET_MANIFEST_SHA256" ]]
  pair_verify_generation_view "$PAIR_TARGET_IMAGE" "$PAIR_TARGET_GENERATION" \
    "$(dirname "$candidate_manifest")"
  pair_write_phase sealed
  pair_finish_target
  trap - ERR HUP INT TERM EXIT
  unset PROBE_API_KEY
  echo "image/generation pair committed: $PAIR_TARGET_IMAGE $PAIR_TARGET_GENERATION; public ingress remains closed"
}

if [[ "$PAIR_CUTOVER" = true || "$PAIR_ROLLBACK" = true ]]; then
  [[ "$BOOTSTRAP_EMPTY_HOST" = false \
    && ! ( "$PAIR_CUTOVER" = true && "$PAIR_ROLLBACK" = true ) ]] || usage
  for command_name in awk blkid caddy cmp curl find findmnt flock getfacl grep id \
    install mktemp mv podman python3 readlink rmdir sha256sum ss stat sync systemctl \
    ufw visudo xfs_info; do
    command -v "$command_name" >/dev/null || {
      echo "missing image/generation pair dependency: $command_name" >&2
      exit 1
    }
  done
  pair_discard_unpublished_build
  require_known_image_transaction_states
  pair_require_launcher_context
  pair_require_fixed_host_identities
  if [[ "$RECOVER" = true ]]; then
    [[ "$PAIR_CUTOVER" = true && "$PAIR_ROLLBACK" = false \
      && -z "$NEW_IMAGE$EXPECTED_VERSION$SOURCE_TEMPLATE$PAIR_GENERATION$PAIR_EXPECTED_CURRENT_GENERATION" \
      && "$PAIR_FROM_PUBLIC" = false ]] || usage
    ordinary_load_release_bundle '' ''
    pair_require_no_foreign_transaction true
    pair_recover_pending_state
  else
    if [[ "$PAIR_CUTOVER" = true ]]; then
      PAIR_OPERATION=prepared
    else
      PAIR_OPERATION=installed
    fi
    run_pair_operation
  fi
  exit 0
fi

if [[ -n "$PAIR_GENERATION$PAIR_EXPECTED_CURRENT_GENERATION" \
  || "$PAIR_FROM_PUBLIC" = true ]]; then
  usage
fi

if [[ "$BOOTSTRAP_EMPTY_HOST" = true ]]; then
  for command_name in awk blkid cmp find findmnt getfacl id podman python3 \
    readlink sha256sum ss stat sync systemctl ufw visudo xfs_info; do
    command -v "$command_name" >/dev/null || {
      echo "missing empty-host image dependency: $command_name" >&2
      exit 1
    }
  done
  pair_discard_unpublished_build
  require_known_image_transaction_states
  image_path_is_absent "$TRANSACTION_PREPARING_DELETION" || {
    validate_image_deletion_marker "$TRANSACTION_PREPARING_DELETION" \
      LEGAL_MCP_BOOTSTRAP_IMAGE_TRANSACTION_V1
  }
  image_path_is_absent "$TRANSACTION_DELETION" || {
    validate_image_deletion_marker "$TRANSACTION_DELETION" \
      LEGAL_MCP_BOOTSTRAP_IMAGE_TRANSACTION_V1
  }
  for path in "$TRANSACTION_PREPARING" "$TRANSACTION_PREPARING_RETIRED" \
    "$TRANSACTION" "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"; do
    if [[ -f "$path/kind" && ! -L "$path/kind" ]]; then
      bootstrap_require_regular "$path/kind" root root 600
      [[ "$(<"$path/kind")" = LEGAL_MCP_BOOTSTRAP_IMAGE_TRANSACTION_V1 ]] || {
        echo 'empty-host recovery refuses an image transaction owned by another operation' >&2
        exit 1
      }
    fi
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
pair_discard_unpublished_build
require_known_image_transaction_states

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
  "$TRANSACTION_PREPARING_DELETION" "$TRANSACTION" \
  "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED" "$TRANSACTION_DELETION"; do
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
