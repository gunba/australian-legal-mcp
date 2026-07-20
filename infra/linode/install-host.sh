#!/usr/bin/env bash
# Install the provider-neutral OCI host contract on an Ubuntu 24.04 Akamai/Linode VPS.
set -euo pipefail
umask 027
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

usage() {
  cat >&2 <<'EOF'
usage: sudo infra/linode/install-host.sh \
  --image ghcr.io/gunba/australian-legal-mcp@sha256:DIGEST \
  --public-host legal.example.com \
  --volume-device /dev/disk/by-id/DEVICE \
  --publisher-key-file /root/legal-mcp-publisher.pub \
  --admin-source-ip IP \
  (--initialize-empty-volume | --expected-volume-uuid UUID)

The initialize flag formats only a signature-free, unpartitioned block device.
An existing volume is accepted only with its exact UUID and marker.

On an installed prepared-bootstrap or activated-dark host, upgrade or recover
the complete hosted tool set from an exact version-matched Linux release bundle:
  sudo infra/linode/install-host.sh --upgrade-host-tools --version X.Y.Z
  sudo infra/linode/install-host.sh --recover-host-tools --version X.Y.Z
An authenticated public host must explicitly authorize a fail-closed
transition to configured-dark before the same operation:
  sudo infra/linode/install-host.sh --upgrade-host-tools --version X.Y.Z --from-public
  sudo infra/linode/install-host.sh --recover-host-tools --version X.Y.Z --from-public

V0.19.9 can run the immutable v0.19.8 updater for its exact pending flat-int8
cutover without changing that launcher, updater, or its pointers:
  sudo infra/linode/install-host.sh --recover-v0198-flat-int8 --version 0.19.9
EOF
  exit 2
}

[[ $EUID -eq 0 ]] || { echo 'run this installer as root' >&2; exit 2; }

HOST_TOOLS_TRANSACTION=/etc/legal-mcp/.host-tools-transaction
HOST_TOOLS_BUILDING=${HOST_TOOLS_TRANSACTION}.building
HOST_TOOLS_BUILDING_RETIRED=${HOST_TOOLS_TRANSACTION}.building-retired
HOST_TOOLS_PREPARING=${HOST_TOOLS_TRANSACTION}.preparing
HOST_TOOLS_PREPARING_RETIRED=${HOST_TOOLS_TRANSACTION}.preparing-retired
HOST_TOOLS_RETIRING=${HOST_TOOLS_TRANSACTION}.retiring
HOST_TOOLS_RETIRED=${HOST_TOOLS_TRANSACTION}.retired
HOST_TOOLS_ROLLBACK_RETIRING=${HOST_TOOLS_TRANSACTION}.rollback-retiring
HOST_TOOLS_ROLLBACK_RETIRED=${HOST_TOOLS_TRANSACTION}.rollback-retired
HOST_TOOLS_PUBLISHER_RESTORE=${HOST_TOOLS_TRANSACTION}.publisher-restore
HOST_TOOLS_PUBLISHER_RESTORE_RETIRED=${HOST_TOOLS_TRANSACTION}.publisher-restore-retired
HOST_TOOLS_MARKER=/etc/legal-mcp/host-tools
HOST_DEPLOY=/usr/local/sbin/legal-mcp-host-deploy
PUBLISHER_COMMAND=/usr/local/sbin/legal-mcp-publisher-command
CONFIGURE_AUTH=/usr/local/sbin/legal-mcp-configure-auth
UPDATE_IMAGE=/usr/local/sbin/legal-mcp-update-image
CONTAINER_TEMPLATE=/usr/local/libexec/legal-mcp/legal-mcp.container.template
RENDERED_QUADLET=/etc/containers/systemd/legal-mcp.container
CADDYFILE=/etc/caddy/Caddyfile
HOST_TOOL_LAUNCHER=/usr/local/libexec/legal-mcp/host-tool-launcher
HOST_TOOL_LAUNCHER_MARKER=/etc/legal-mcp/host-tool-launcher
CONFIGURE_AUTH_POINTER=/etc/legal-mcp/configure-auth-implementation
UPDATE_IMAGE_POINTER=/etc/legal-mcp/update-image-implementation
HOST_TOOL_IMPLEMENTATION_DIR=/usr/local/libexec/legal-mcp/host-tools
AUTH_READY_MARKER=/etc/legal-mcp/auth-ready
HOST_TOOL_DISPATCH=/run/legal-mcp/host-tool-launcher-dispatch
AUTH_CONFIGURING_PERMIT=/run/legal-mcp/auth-configuring
CUTOVER_STARTING_PERMIT=/run/legal-mcp/flat-int8-cutover-starting
CUTOVER_START_ARM=/run/legal-mcp/flat-int8-cutover-start-armed
PUBLISHER_SUDOERS=/etc/sudoers.d/legal-mcp-publisher
HOST_TRANSACTION_LOCK=/run/lock/legal-mcp-host-transaction.lock
HOST_TOOLS_RETIREMENT_WAS_PENDING=false
HOST_TOOLS_PREPARATION_WAS_RECOVERED=false
HOST_TOOLS_FROM_PUBLIC=false
HOST_TOOLS_ACCEPT_CONFIGURED_DARK=false

path_is_absent() {
  [[ ! -e "$1" && ! -L "$1" ]]
}

directory_is_empty() {
  local directory="$1" found
  found="$(find "$directory" -mindepth 1 -maxdepth 1 -printf x -quit)" || {
    echo "could not inspect directory contents: $directory" >&2
    return 1
  }
  [[ -z "$found" ]]
}

directory_contains_only() {
  local directory="$1" found name
  local -a exclusions=()
  shift
  for name in "$@"; do
    exclusions+=('!' -name "$name")
  done
  found="$(find "$directory" -mindepth 1 -maxdepth 1 \
    "${exclusions[@]}" -printf x -quit)" || {
    echo "could not inspect directory contents: $directory" >&2
    return 1
  }
  [[ -z "$found" ]]
}

require_regular_file() {
  local path="$1" owner="$2" group="$3" mode="$4"
  [[ -f "$path" && ! -L "$path" \
    && "$(stat -c '%U:%G:%a:%h' "$path")" = "$owner:$group:$mode:1" ]] || {
    echo "unsafe host file: $path" >&2
    return 1
  }
}

require_empty_regular_file() {
  local path="$1" owner="$2" group="$3" mode="$4"
  require_regular_file "$path" "$owner" "$group" "$mode" || return 1
  [[ "$(stat -c '%s' "$path")" = 0 ]] || {
    echo "host contract file must be empty: $path" >&2
    return 1
  }
}

require_exact_acl() {
  local path="$1" expected="$2"
  [[ "$(getfacl --absolute-names --numeric --omit-header "$path")" = "$expected" ]] || {
    echo "unsafe access ACL: $path" >&2
    return 1
  }
}

require_safe_directory() {
  local path="$1" owner="$2" group="$3" mode
  [[ -d "$path" && ! -L "$path" \
    && "$(stat -c '%U:%G' "$path")" = "$owner:$group" ]] || {
    echo "unsafe host directory: $path" >&2
    return 1
  }
  mode="$(stat -c '%a' "$path")"
  if [[ ! "$mode" =~ ^[0-7]{3}$ ]] || (( (8#$mode & 8#022) != 0 )); then
    echo "host directory is group/other writable: $path" >&2
    return 1
  fi
}

require_exact_directory() {
  local path="$1" owner="$2" group="$3" mode="$4"
  [[ -d "$path" && ! -L "$path" \
    && "$(stat -c '%U:%G:%a' "$path")" = "$owner:$group:$mode" ]] || {
    echo "unsafe host directory: $path" >&2
    return 1
  }
}

require_release_file() {
  local path="$1" executable="${2:-false}" mode uid
  [[ -f "$path" && ! -L "$path" && "$(stat -c '%h' "$path")" = 1 ]] || {
    echo "version-matched release asset is missing or unsafe: $path" >&2
    return 1
  }
  mode="$(stat -c '%a' "$path")"
  uid="$(stat -c '%u' "$path")"
  [[ "$mode" =~ ^[0-7]{3}$ && "$uid" != 971 && "$uid" != 973 ]] || {
    echo "version-matched release asset has an unsafe identity or mode: $path" >&2
    return 1
  }
  (( (8#$mode & 8#022) == 0 )) || {
    echo "version-matched release asset is group/other writable: $path" >&2
    return 1
  }
  if [[ "$executable" = true && ! -x "$path" ]]; then
    echo "version-matched release executable is not executable: $path" >&2
    return 1
  fi
}

require_release_directory() {
  local path="$1" mode uid
  [[ -d "$path" && ! -L "$path" ]] || {
    echo "version-matched release directory is missing or unsafe: $path" >&2
    return 1
  }
  mode="$(stat -c '%a' "$path")"
  uid="$(stat -c '%u' "$path")"
  [[ "$mode" =~ ^[0-7]{3}$ && "$uid" != 971 && "$uid" != 973 ]] || return 1
  (( (8#$mode & 8#022) == 0 )) || {
    echo "version-matched release directory is group/other writable: $path" >&2
    return 1
  }
}

render_publisher_sudoers() {
  local deploy_sha256="$1" destination="$2"
  printf '%s\n' \
    'Defaults:legal-mcp-publisher !requiretty' \
    "legal-mcp-publisher ALL=(root) NOPASSWD: sha256:$deploy_sha256 $HOST_DEPLOY ^prepare [0-9a-f]{64}$, sha256:$deploy_sha256 $HOST_DEPLOY ^activate [0-9a-f]{64}$, sha256:$deploy_sha256 $HOST_DEPLOY ^abort [0-9a-f]{64}$" \
    > "$destination"
  chmod 440 "$destination"
  visudo -cf "$destination" >/dev/null
}

render_host_tool_launcher() {
  cat <<'LAUNCHER'
#!/usr/bin/env bash
# Stable lock-first dispatcher for immutable authentication and image helpers.
set -euo pipefail
umask 077
ulimit -c 0
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

LOCK_FILE=/run/lock/legal-mcp-host-transaction.lock
CANONICAL=/usr/local/libexec/legal-mcp/host-tool-launcher
CONFIGURE=/usr/local/sbin/legal-mcp-configure-auth
UPDATE=/usr/local/sbin/legal-mcp-update-image
MARKER=/etc/legal-mcp/host-tool-launcher
CONFIGURE_POINTER=/etc/legal-mcp/configure-auth-implementation
UPDATE_POINTER=/etc/legal-mcp/update-image-implementation
IMPLEMENTATION_DIR=/usr/local/libexec/legal-mcp/host-tools
HOST_TOOLS_MARKER=/etc/legal-mcp/host-tools
HOST_DEPLOY=/usr/local/sbin/legal-mcp-host-deploy
PUBLISHER=/usr/local/sbin/legal-mcp-publisher-command
SUDOERS=/etc/sudoers.d/legal-mcp-publisher
TEMPLATE=/usr/local/libexec/legal-mcp/legal-mcp.container.template
QUADLET=/etc/containers/systemd/legal-mcp.container
IMAGE=/etc/legal-mcp/image
AUTH_READY=/etc/legal-mcp/auth-ready
AUTH_PERMIT=/run/legal-mcp/auth-configuring
CUTOVER_PERMIT=/run/legal-mcp/flat-int8-cutover-starting
CUTOVER_ARM=/run/legal-mcp/flat-int8-cutover-start-armed
DISPATCH=/run/legal-mcp/host-tool-launcher-dispatch
DISPATCH_RETIRING=${DISPATCH}.retiring
DISPATCH_RETIRED=${DISPATCH}.retired

absent() { [[ ! -e "$1" && ! -L "$1" ]]; }

require_file() {
  local path="$1" owner="$2" group="$3" mode="$4" size="${5:--}"
  [[ -f "$path" && ! -L "$path" \
    && "$(stat -c '%U:%G:%a:%h' "$path")" = "$owner:$group:$mode:1" ]] || return 1
  [[ "$size" = - || "$(stat -c '%s' "$path")" = "$size" ]] || return 1
}

start_time() {
  python3 - "$1" <<'PY'
import pathlib, sys
value = pathlib.Path(f"/proc/{sys.argv[1]}/stat").read_text()
fields = value.rpartition(") ")[2].split()
if len(fields) < 20 or not fields[19].isdigit():
    raise SystemExit(1)
print(fields[19])
PY
}

strict_dispatch_permit() {
  local permit="$1" required_role="$2" required_argument="$3"
  require_file "$permit" root root 400 || return 1
  [[ "$(getfacl --absolute-names --numeric --omit-header "$permit")" \
    = $'user::r--\ngroup::---\nother::---' ]] || return 1
  local permit_pid permit_start actual_start uid_line cmdline
  read -r permit_pid permit_start < "$permit" || return 1
  [[ "$permit_pid" =~ ^[1-9][0-9]*$ && "$permit_start" =~ ^[1-9][0-9]*$ \
    && "$(wc -w < "$permit")" = 2 ]] || return 1
  actual_start="$(start_time "$permit_pid" 2>/dev/null)" || return 1
  [[ "$actual_start" = "$permit_start" ]] || return 1
  uid_line="$(awk '$1 == "Uid:" {print $2 ":" $3 ":" $4 ":" $5}' "/proc/$permit_pid/status")" || return 1
  [[ "$uid_line" = 0:0:0:0 ]] || return 1
  cmdline="$(tr '\0' '\n' < "/proc/$permit_pid/cmdline")" || return 1
  grep -Fxq -- '--legal-mcp-launcher-internal' <<< "$cmdline" \
    && grep -Fxq "$required_role" <<< "$cmdline" \
    && { [[ -z "$required_argument" ]] \
      || grep -Fxq -- "$required_argument" <<< "$cmdline"; }
}

committed_auth_ready() {
  require_file "$AUTH_READY" root root 444 0 \
    && [[ "$(getfacl --absolute-names --numeric --omit-header "$AUTH_READY")" \
      = $'user::r--\ngroup::r--\nother::r--' ]] || return 1
  local path transaction='' count=0 kind outcome
  for path in /etc/legal-mcp/.image-transaction \
    /etc/legal-mcp/.image-transaction.retiring \
    /etc/legal-mcp/.image-transaction.retired; do
    if ! absent "$path"; then
      transaction="$path"
      count=$((count + 1))
    fi
  done
  [[ $count -le 1 ]] || return 1
  [[ $count -eq 1 ]] || return 0
  [[ -d "$transaction" && ! -L "$transaction" \
    && "$(stat -c '%U:%G:%a' "$transaction")" = root:root:700 ]] || return 1
  require_file "$transaction/kind" root root 600 || return 1
  kind="$(<"$transaction/kind")"
  case "$kind" in
    LEGAL_MCP_IMAGE_TRANSACTION_V2)
      return 0
      ;;
    LEGAL_MCP_FLAT_INT8_CUTOVER_TRANSACTION_V1)
      require_file "$transaction/retirement-outcome" root root 600 || return 1
      outcome="$(<"$transaction/retirement-outcome")"
      [[ "$outcome" = saved || "$outcome" = target ]]
      ;;
    *) return 1 ;;
  esac
}

strict_auth_ready() {
  committed_auth_ready && return 0
  strict_dispatch_permit "$AUTH_PERMIT" configure-auth '' \
    || { strict_dispatch_permit "$CUTOVER_PERMIT" update-image --flat-int8-cutover \
      && require_file "$CUTOVER_ARM" root root 400 \
      && cmp --silent "$CUTOVER_PERMIT" "$CUTOVER_ARM"; }
}

if [[ "${1:-}" = --check-auth-ready ]]; then
  [[ $# -eq 1 && $EUID -eq 0 ]] || exit 1
  strict_auth_ready
  exit
fi

require_launcher_set() {
  local -a values
  require_file "$MARKER" root root 444 || return 1
  mapfile -t values < "$MARKER"
  [[ ${#values[@]} -eq 2 \
    && "${values[0]}" = LEGAL_MCP_HOST_TOOL_LAUNCHER_V1 \
    && "${values[1]}" =~ ^LAUNCHER_SHA256=([0-9a-f]{64})$ ]] || return 1
  LAUNCHER_SHA256="${BASH_REMATCH[1]}"
  local path
  for path in "$CANONICAL" "$CONFIGURE" "$UPDATE"; do
    require_file "$path" root root 755 || return 1
    [[ "$(sha256sum "$path" | awk '{print $1}')" = "$LAUNCHER_SHA256" ]] || return 1
  done
}

read_implementation() {
  local pointer="$1" name="$2" value path
  require_file "$pointer" root root 644 64 || return 1
  [[ "$(getfacl --absolute-names --numeric --omit-header "$pointer")" \
    = $'user::rw-\ngroup::r--\nother::r--' ]] || return 1
  value="$(<"$pointer")"
  [[ "$value" =~ ^[0-9a-f]{64}$ ]] || return 1
  path="$IMPLEMENTATION_DIR/$name.$value"
  require_file "$path" root root 755 || return 1
  [[ "$(sha256sum "$path" | awk '{print $1}')" = "$value" ]] || return 1
  printf '%s\n' "$value"
}

cutover_release_binding_recoverable() {
  local template_sha="$1" path transaction='' count=0 rendered
  [[ "${cutover_dispatch:-false}" = true ]] || return 1
  for path in /etc/legal-mcp/.image-transaction \
    /etc/legal-mcp/.image-transaction.retiring \
    /etc/legal-mcp/.image-transaction.retired; do
    if ! absent "$path"; then
      transaction="$path"
      count=$((count + 1))
    fi
  done
  [[ $count -eq 1 && -d "$transaction" && ! -L "$transaction" \
    && "$(stat -c '%U:%G:%a' "$transaction")" = root:root:700 ]] || return 1
  for path in kind saved-image saved-quadlet saved-template \
    target-image target-quadlet target-template; do
    require_file "$transaction/$path" root root 600 || return 1
  done
  [[ "$(<"$transaction/kind")" = LEGAL_MCP_FLAT_INT8_CUTOVER_TRANSACTION_V1 \
    && "$(sha256sum "$transaction/saved-template" | awk '{print $1}')" = "$template_sha" \
    && "$(sha256sum "$transaction/target-template" | awk '{print $1}')" = "$template_sha" ]] \
    || return 1
  { cmp --silent "$IMAGE" "$transaction/saved-image" \
      || cmp --silent "$IMAGE" "$transaction/target-image"; } \
    && { cmp --silent "$TEMPLATE" "$transaction/saved-template" \
      || cmp --silent "$TEMPLATE" "$transaction/target-template"; } \
    && { cmp --silent "$QUADLET" "$transaction/saved-quadlet" \
      || cmp --silent "$QUADLET" "$transaction/target-quadlet"; } || return 1
  rendered="$(mktemp /run/legal-mcp-launcher-cutover.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$(<"$transaction/saved-image")|g" \
    "$transaction/saved-template" > "$rendered"
  if ! cmp --silent "$rendered" "$transaction/saved-quadlet"; then
    rm -f "$rendered"
    return 1
  fi
  sed "s|__IMAGE_DIGEST__|$(<"$transaction/target-image")|g" \
    "$transaction/target-template" > "$rendered"
  if ! cmp --silent "$rendered" "$transaction/target-quadlet"; then
    rm -f "$rendered"
    return 1
  fi
  rm -f "$rendered"
}

cutover_transaction_present() {
  local path
  for path in /etc/legal-mcp/.image-transaction \
    /etc/legal-mcp/.image-transaction.retiring \
    /etc/legal-mcp/.image-transaction.retired; do
    if ! absent "$path"; then
      [[ -d "$path" && ! -L "$path" \
        && "$(stat -c '%U:%G:%a' "$path")" = root:root:700 ]] || continue
      require_file "$path/kind" root root 600 || continue
      require_file "$path/retirement-outcome" root root 600 || continue
      [[ "$(<"$path/kind")" = LEGAL_MCP_FLAT_INT8_CUTOVER_TRANSACTION_V1 \
        && "$(<"$path/retirement-outcome")" = pending ]] && return 0
    fi
  done
  return 1
}

require_v2_release_binding() {
  local configure_digest="$1" update_digest="$2" rendered
  local deploy_sha publisher_sha template_sha sudoers_sha image
  local -a values image_values
  require_file "$HOST_TOOLS_MARKER" root root 444 || return 1
  mapfile -t values < "$HOST_TOOLS_MARKER"
  [[ ${#values[@]} -eq 9 \
    && "${values[0]}" = LEGAL_MCP_HOST_TOOLS_V2 \
    && "${values[1]}" =~ ^VERSION=[0-9]+\.[0-9]+\.[0-9]+$ \
    && "${values[2]}" =~ ^SOURCE_COMMIT=[0-9a-f]{40}$ \
    && "${values[3]}" =~ ^HOST_DEPLOY_SHA256=([0-9a-f]{64})$ ]] || return 1
  deploy_sha="${BASH_REMATCH[1]}"
  [[ "${values[4]}" =~ ^PUBLISHER_COMMAND_SHA256=([0-9a-f]{64})$ ]] || return 1
  publisher_sha="${BASH_REMATCH[1]}"
  [[ "${values[5]}" = "CONFIGURE_AUTH_SHA256=$configure_digest" \
    && "${values[6]}" = "UPDATE_IMAGE_SHA256=$update_digest" \
    && "${values[7]}" =~ ^CONTAINER_TEMPLATE_SHA256=([0-9a-f]{64})$ ]] || return 1
  template_sha="${BASH_REMATCH[1]}"
  [[ "${values[8]}" =~ ^SUDOERS_SHA256=([0-9a-f]{64})$ ]] || return 1
  sudoers_sha="${BASH_REMATCH[1]}"
  require_file "$HOST_DEPLOY" root root 755 || return 1
  require_file "$PUBLISHER" root root 755 || return 1
  require_file "$SUDOERS" root root 440 || return 1
  require_file "$TEMPLATE" root root 644 || return 1
  require_file "$QUADLET" root root 644 || return 1
  require_file "$IMAGE" root root 600 || return 1
  [[ "$(sha256sum "$HOST_DEPLOY" | awk '{print $1}')" = "$deploy_sha" \
    && "$(sha256sum "$PUBLISHER" | awk '{print $1}')" = "$publisher_sha" \
    && "$(sha256sum "$SUDOERS" | awk '{print $1}')" = "$sudoers_sha" \
    && "$(sha256sum "$TEMPLATE" | awk '{print $1}')" = "$template_sha" ]] || return 1
  mapfile -t image_values < "$IMAGE"
  [[ ${#image_values[@]} -eq 1 \
    && "${image_values[0]}" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ ]] || return 1
  image="${image_values[0]}"
  [[ "$(grep -o '__IMAGE_DIGEST__' "$TEMPLATE" | wc -l)" = 1 \
    && "$(grep -Fxc 'ExecCondition=/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready' \
      "$TEMPLATE")" = 1 ]] || return 1
  rendered="$(mktemp /run/legal-mcp-launcher-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$image|g" "$TEMPLATE" > "$rendered"
  if ! cmp --silent "$rendered" "$QUADLET"; then
    rm -f "$rendered"
    cutover_release_binding_recoverable "$template_sha" || return 1
    return 0
  fi
  rm -f "$rendered"
}

require_dispatch() {
  local expected_pid="$1" expected_start="$2" expected_role="$3"
  [[ -d "$DISPATCH" && ! -L "$DISPATCH" \
    && "$(stat -c '%U:%G:%a' "$DISPATCH")" = root:root:700 ]] || return 1
  local name
  for name in pid start-time role configure-auth update-image; do
    require_file "$DISPATCH/$name" root root 600 || return 1
  done
  [[ "$(<"$DISPATCH/pid")" = "$expected_pid" \
    && "$(<"$DISPATCH/start-time")" = "$expected_start" \
    && "$(<"$DISPATCH/role")" = "$expected_role" ]] || return 1
  [[ -z "$(find "$DISPATCH" -mindepth 1 -maxdepth 1 \
    ! -name pid ! -name start-time ! -name role ! -name configure-auth \
    ! -name update-image -print -quit)" ]] || return 1
}

delete_dispatch_retirement() {
  local path="$1"
  absent "$path" && return 0
  [[ -d "$path" && ! -L "$path" \
    && "$(stat -c '%U:%G:%a' "$path")" = root:root:700 ]] || return 1
  rm -rf --one-file-system -- "$path"
  absent "$path"
  sync -f /run/legal-mcp
}

reconcile_dispatch() {
  if ! absent "$DISPATCH_RETIRING"; then
    absent "$DISPATCH_RETIRED" || return 1
    mv -T "$DISPATCH_RETIRING" "$DISPATCH_RETIRED"
    sync -f /run/legal-mcp
  fi
  delete_dispatch_retirement "$DISPATCH_RETIRED"
  if absent "$DISPATCH"; then
    rm -f -- "$AUTH_PERMIT" "$CUTOVER_PERMIT" "$CUTOVER_ARM"
    sync -f /run/legal-mcp
    return 0
  fi
  [[ -d "$DISPATCH" && ! -L "$DISPATCH" \
    && "$(stat -c '%U:%G:%a' "$DISPATCH")" = root:root:700 ]] || return 1
  require_file "$DISPATCH/pid" root root 600 || return 1
  require_file "$DISPATCH/start-time" root root 600 || return 1
  local pid saved_start live_start
  pid="$(<"$DISPATCH/pid")"
  saved_start="$(<"$DISPATCH/start-time")"
  [[ "$pid" =~ ^[1-9][0-9]*$ && "$saved_start" =~ ^[1-9][0-9]*$ ]] || return 1
  live_start="$(start_time "$pid" 2>/dev/null || true)"
  [[ -z "$live_start" || "$live_start" != "$saved_start" ]] || {
    echo 'another immutable host-tool dispatch is active' >&2
    return 1
  }
  rm -f -- "$AUTH_PERMIT" "$CUTOVER_PERMIT" "$CUTOVER_ARM"
  sync -f /run/legal-mcp
  absent "$DISPATCH_RETIRING" && absent "$DISPATCH_RETIRED" || return 1
  mv -T "$DISPATCH" "$DISPATCH_RETIRING"
  sync -f /run/legal-mcp
  mv -T "$DISPATCH_RETIRING" "$DISPATCH_RETIRED"
  sync -f /run/legal-mcp
  delete_dispatch_retirement "$DISPATCH_RETIRED"
}

retire_dispatch() {
  absent "$DISPATCH_RETIRING" && absent "$DISPATCH_RETIRED" || return 1
  mv -T "$DISPATCH" "$DISPATCH_RETIRING"
  sync -f /run/legal-mcp
  mv -T "$DISPATCH_RETIRING" "$DISPATCH_RETIRED"
  sync -f /run/legal-mcp
  delete_dispatch_retirement "$DISPATCH_RETIRED"
}

auth_contract_ready() {
  local runtime=/etc/legal-mcp/runtime.env verifier=/etc/legal-mcp/api-keys.json
  local -a auth_values
  require_file "$runtime" root root 600 || return 1
  mapfile -t auth_values < <(awk -F= '$1 == "LEGAL_MCP_HTTP_AUTH" {print $2}' "$runtime")
  [[ ${#auth_values[@]} -eq 1 \
    && ( "${auth_values[0]}" = api-key || "${auth_values[0]}" = entra \
      || "${auth_values[0]}" = entra+api-key ) ]] || return 1
  require_file "$verifier" legal-mcp legal-mcp 400 || return 1
  python3 - "$verifier" "${auth_values[0]}" <<'PY'
import json, pathlib, sys
value = json.loads(pathlib.Path(sys.argv[1]).read_bytes())
if not isinstance(value, dict) or set(value) != {"keys", "version"} or value["version"] != 1:
    raise SystemExit(1)
keys = value["keys"]
if not isinstance(keys, list) or len(keys) > 32:
    raise SystemExit(1)
if "api-key" in sys.argv[2] and not keys:
    raise SystemExit(1)
PY
}

strict_ready_marker() {
  require_file "$AUTH_READY" root root 444 0 \
    && [[ "$(getfacl --absolute-names --numeric --omit-header "$AUTH_READY")" \
      = $'user::r--\ngroup::r--\nother::r--' ]]
}

auth_journal_states_absent() {
  local found
  found="$(find /etc/legal-mcp -mindepth 1 -maxdepth 1 \
    -name '.auth-transaction*' -print -quit)" || return 1
  [[ -z "$found" ]]
}

auth_journal_state_present() {
  local found
  found="$(find /etc/legal-mcp -mindepth 1 -maxdepth 1 \
    -name '.auth-transaction*' -print -quit)" || return 1
  [[ -n "$found" ]]
}

read_enablement() {
  local unit="$1" output status
  if output="$(systemctl is-enabled "$unit" 2>/dev/null)"; then status=0; else status=$?; fi
  case "$status:$output" in
    0:generated|0:enabled|1:disabled) printf '%s\n' "$output" ;;
    *) return 1 ;;
  esac
}

read_activity() {
  local unit="$1" output status
  if output="$(systemctl is-active "$unit" 2>/dev/null)"; then status=0; else status=$?; fi
  case "$status:$output" in
    0:active|3:inactive) printf '%s\n' "$output" ;;
    *) return 1 ;;
  esac
}

require_ufw_state() {
  local expected="$1" admin report
  require_file /etc/legal-mcp/admin-source-ip root root 600 || return 1
  admin="$(</etc/legal-mcp/admin-source-ip)"
  [[ "$admin" =~ ^[0-9A-Fa-f:.]{2,45}$ ]] || return 1
  report="$(ufw status verbose)" || return 1
  printf '%s\n' "$report" | python3 /dev/fd/3 "$admin" "$expected" 3<<'PY'
import re, sys
admin, expected = sys.argv[1:]
report = sys.stdin.read().splitlines()
if "Status: active" not in report:
    raise SystemExit(1)
if not any(re.fullmatch(r"Default: deny \(incoming\), allow \(outgoing\), (?:disabled|deny) \(routed\)", line) for line in report):
    raise SystemExit(1)
rules = []
for line in report:
    for action in ("ALLOW IN", "DENY IN", "REJECT IN", "LIMIT IN"):
        if action in line:
            left, right = line.split(action, 1)
            rules.append((left.strip(), action, right.split("#", 1)[0].strip()))
            break
if any(action != "ALLOW IN" for _, action, _ in rules):
    raise SystemExit(1)
ssh = [(target, source) for target, _, source in rules if target == "22/tcp"]
if ssh != [("22/tcp", admin)]:
    raise SystemExit(1)
web = [(target, source) for target, _, source in rules if target != "22/tcp"]
allowed = {"80/tcp", "80/tcp (v6)", "443/tcp", "443/tcp (v6)"}
if any(target not in allowed or not source.startswith("Anywhere") for target, source in web):
    raise SystemExit(1)
if len({target for target, _ in web}) != len(web):
    raise SystemExit(1)
ports = {target.split("/", 1)[0] for target, _ in web}
if (expected == "closed" and web) or (expected == "open" and ports != {"80", "443"}):
    raise SystemExit(1)
if expected not in {"closed", "open"}:
    raise SystemExit(1)
PY
}

require_listener_state() {
  local expected="$1" listeners
  listeners="$(ss --listening --tcp --numeric --no-header)" || return 1
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
elif expected == "public":
    if service != [("127.0.0.1", 51235)] or {port for _, port in web} != {80, 443}:
        raise SystemExit(1)
    if any(address not in {"*", "0.0.0.0", "::"} for address, _ in web):
        raise SystemExit(1)
    if sum(port == 80 for _, port in web) not in (1, 2) or sum(port == 443 for _, port in web) not in (1, 2):
        raise SystemExit(1)
else:
    raise SystemExit(1)
PY
}

closed_surface_ready() {
  absent "$AUTH_READY" \
    && [[ "$(read_enablement legal-mcp.service)" = generated \
      && "$(read_activity legal-mcp.service)" = inactive \
      && "$(read_enablement caddy.service)" = disabled \
      && "$(read_activity caddy.service)" = inactive ]] \
    && require_ufw_state closed \
    && require_listener_state none
}

public_auth_ready() {
  local marker_state="$1"
  auth_contract_ready || return 1
  case "$marker_state" in
    present) strict_ready_marker || return 1 ;;
    absent) absent "$AUTH_READY" || return 1 ;;
    *) return 1 ;;
  esac
  [[ "$(read_enablement legal-mcp.service)" = generated \
    && "$(read_activity legal-mcp.service)" = active \
    && "$(read_enablement caddy.service)" = enabled \
    && "$(read_activity caddy.service)" = active ]] \
    && require_ufw_state open \
    && require_listener_state public
}

disabled_dark_recovery_ready() {
  local runtime=/etc/legal-mcp/runtime.env verifier=/etc/legal-mcp/api-keys.json
  local -a auth_values
  require_file "$runtime" root root 600 || return 1
  require_file "$verifier" legal-mcp legal-mcp 400 || return 1
  mapfile -t auth_values < <(awk -F= '$1 == "LEGAL_MCP_HTTP_AUTH" {print $2}' "$runtime")
  [[ ${#auth_values[@]} -eq 1 && "${auth_values[0]}" = disabled ]] || return 1
  python3 - "$verifier" <<'PY' || return 1
import json, pathlib, sys
if json.loads(pathlib.Path(sys.argv[1]).read_bytes()) != {"keys": [], "version": 1}:
    raise SystemExit(1)
PY
  auth_journal_states_absent && closed_surface_ready
}

configured_dark_recovery_ready() {
  auth_contract_ready \
    && auth_journal_states_absent \
    && closed_surface_ready
}

auth_state_snapshot() {
  local state runtime_sha verifier_sha caddy_sha
  if disabled_dark_recovery_ready; then
    state=dark
  elif configured_dark_recovery_ready; then
    state=configured-dark
  elif auth_journal_states_absent && public_auth_ready present; then
    state=public
  else
    return 1
  fi
  require_file /etc/caddy/Caddyfile root caddy 640 || return 1
  runtime_sha="$(sha256sum /etc/legal-mcp/runtime.env | awk '{print $1}')"
  verifier_sha="$(sha256sum /etc/legal-mcp/api-keys.json | awk '{print $1}')"
  caddy_sha="$(sha256sum /etc/caddy/Caddyfile | awk '{print $1}')"
  [[ "$runtime_sha$verifier_sha$caddy_sha" =~ ^[0-9a-f]{192}$ ]] || return 1
  printf '%s:%s:%s:%s\n' "$state" "$runtime_sha" "$verifier_sha" "$caddy_sha"
}

remove_auth_ready() {
  absent "$AUTH_READY" && return 0
  strict_ready_marker || return 1
  rm -f -- "$AUTH_READY"
  sync -f /etc/legal-mcp
  absent "$AUTH_READY"
}

remove_auth_permit() {
  rm -f -- "$AUTH_PERMIT"
  sync -f /run/legal-mcp
  absent "$AUTH_PERMIT"
}

remove_cutover_permit() {
  rm -f -- "$CUTOVER_PERMIT" "$CUTOVER_ARM"
  sync -f /run/legal-mcp
  absent "$CUTOVER_PERMIT" && absent "$CUTOVER_ARM"
}

write_cutover_permit() {
  local pid="$1" process_start="$2" temporary
  temporary="$(mktemp /run/legal-mcp/.flat-int8-cutover-starting.XXXXXX)"
  printf '%s %s\n' "$pid" "$process_start" > "$temporary"
  chown root:root "$temporary"
  chmod 400 "$temporary"
  sync -f "$temporary"
  mv -fT "$temporary" "$CUTOVER_PERMIT"
  sync -f /run/legal-mcp
}

force_auth_closed() {
  local failed=false
  remove_auth_ready || failed=true
  systemctl disable --now caddy.service >/dev/null 2>&1 || failed=true
  ufw --force delete allow 80/tcp comment 'Caddy ACME HTTP' >/dev/null 2>&1 || true
  ufw --force delete allow 443/tcp comment 'Australian Legal MCP HTTPS' >/dev/null 2>&1 || true
  systemctl stop legal-mcp.service >/dev/null 2>&1 || failed=true
  closed_surface_ready || failed=true
  [[ "$failed" = false ]]
}

publish_auth_ready() {
  local temporary
  absent "$AUTH_READY" || return 1
  temporary="$(mktemp /etc/legal-mcp/.auth-ready.XXXXXX)"
  : > "$temporary"
  chown root:root "$temporary"
  chmod 444 "$temporary"
  sync -f "$temporary"
  mv -fT "$temporary" "$AUTH_READY"
  sync -f /etc/legal-mcp
  strict_ready_marker
}

write_auth_permit() {
  local pid="$1" process_start="$2" temporary
  temporary="$(mktemp /run/legal-mcp/.auth-configuring.XXXXXX)"
  printf '%s %s\n' "$pid" "$process_start" > "$temporary"
  chown root:root "$temporary"
  chmod 400 "$temporary"
  sync -f "$temporary"
  mv -fT "$temporary" "$AUTH_PERMIT"
  sync -f /run/legal-mcp
}

if [[ "${1:-}" = --legal-mcp-launcher-internal ]]; then
  [[ $EUID -eq 0 && $# -ge 4 && "$(readlink -f "$0")" = "$CANONICAL" ]] || exit 1
  role="$2"
  configure_digest="$3"
  update_digest="$4"
  shift 4
  [[ ( "$role" = configure-auth || "$role" = update-image ) \
    && "$configure_digest" =~ ^[0-9a-f]{64}$ \
    && "$update_digest" =~ ^[0-9a-f]{64}$ ]] || exit 1
  current_start="$(start_time "$$")"
  configure_recover_only=false
  if [[ "$role" = configure-auth && $# -eq 1 && "$1" = --recover ]]; then
    configure_recover_only=true
  fi
  require_dispatch "$$" "$current_start" "$role"
  [[ "$(<"$DISPATCH/configure-auth")" = "$configure_digest" \
    && "$(<"$DISPATCH/update-image")" = "$update_digest" \
    && "$(read_implementation "$CONFIGURE_POINTER" configure-auth)" = "$configure_digest" \
    && "$(read_implementation "$UPDATE_POINTER" update-image)" = "$update_digest" ]] || exit 1
  mount --bind "$IMPLEMENTATION_DIR/configure-auth.$configure_digest" "$CONFIGURE"
  mount -o remount,bind,ro,nodev,nosuid "$CONFIGURE"
  mount --bind "$IMPLEMENTATION_DIR/update-image.$update_digest" "$UPDATE"
  mount -o remount,bind,ro,nodev,nosuid "$UPDATE"
  status=0
  if [[ "$role" = configure-auth ]]; then
    preserve_prior_state=false
    if [[ "$configure_recover_only" = true ]]; then
      "$CONFIGURE" --recover || status=$?
      if [[ $status -eq 0 ]]; then
        if disabled_dark_recovery_ready; then
          : # Exact disabled recovery is successful but deliberately not auth-ready.
        elif public_auth_ready present || public_auth_ready absent; then
          if absent "$AUTH_READY"; then
            publish_auth_ready || status=1
          fi
          if [[ $status -eq 0 ]] && auth_journal_state_present; then
            "$CONFIGURE" --finalize-auth-ready || status=$?
          fi
          if [[ $status -eq 0 ]] \
            && { ! public_auth_ready present || ! auth_journal_states_absent; }; then
            status=1
          fi
        else
          status=1
        fi
      fi
    else
      prior_snapshot="$(auth_state_snapshot)" || status=1
      if [[ $status -eq 0 ]]; then
        prepare_status=0
        "$CONFIGURE" --prepare-auth-dispatch || prepare_status=$?
        if [[ $prepare_status -ne 0 ]]; then
          status=$prepare_status
          if auth_journal_states_absent \
            && [[ "$(auth_state_snapshot 2>/dev/null || true)" = "$prior_snapshot" ]]; then
            preserve_prior_state=true
          fi
        elif ! auth_journal_state_present; then
          status=1
        elif ! remove_auth_ready; then
          status=1
        else
          "$CONFIGURE" "$@" || status=$?
          if [[ $status -eq 0 ]]; then
            if public_auth_ready absent && auth_journal_state_present; then
              publish_auth_ready || status=1
            else
              status=1
            fi
          fi
          if [[ $status -eq 0 ]]; then
            "$CONFIGURE" --finalize-auth-ready || status=$?
          fi
          if [[ $status -eq 0 ]] \
            && { ! public_auth_ready present || ! auth_journal_states_absent; }; then
            status=1
          fi
        fi
      fi
    fi
    remove_auth_permit || status=1
    if [[ $status -ne 0 && "$preserve_prior_state" = false ]]; then
      force_auth_closed || status=1
    fi
  else
    cutover_request=false
    for argument in "$@"; do
      [[ "$argument" = --flat-int8-cutover ]] && cutover_request=true
    done
    "$UPDATE" "$@" || status=$?
    if [[ $status -ne 0 && "$cutover_request" = true ]] \
      && cutover_transaction_present; then
      force_auth_closed || status=1
    fi
    remove_cutover_permit || status=1
  fi
  retire_dispatch || status=1
  exit "$status"
fi

[[ $EUID -eq 0 ]] || { echo 'host-tool launcher must run as root' >&2; exit 2; }
case "$(basename "$0")" in
  legal-mcp-configure-auth) role=configure-auth ;;
  legal-mcp-update-image) role=update-image ;;
  *) echo 'invoke the installed authentication or image launcher' >&2; exit 2 ;;
esac
cutover_dispatch=false
if [[ "$role" = configure-auth ]]; then
  for argument in "$@"; do
    case "$argument" in
      --prepare-auth-dispatch|--finalize-auth-ready|--legal-mcp-launcher-internal)
        echo 'internal authentication handoff command is not a public launcher argument' >&2
        exit 2
        ;;
    esac
  done
else
  cutover_arguments=0
  for argument in "$@"; do
    case "$argument" in
      --legal-mcp-launcher-internal)
        echo 'internal image handoff command is not a public launcher argument' >&2
        exit 2
        ;;
      --flat-int8-cutover)
        cutover_arguments=$((cutover_arguments + 1))
        ;;
    esac
  done
  [[ $cutover_arguments -le 1 ]] || {
    echo 'flat-int8 cutover mode may be selected only once' >&2
    exit 2
  }
  [[ $cutover_arguments -eq 1 ]] && cutover_dispatch=true
fi
require_file "$LOCK_FILE" root legal-mcp-publisher 640 || {
  echo 'host transaction lock is missing or unsafe' >&2
  exit 1
}
exec 9<>"$LOCK_FILE"
flock -x 9
require_launcher_set || { echo 'stable host-tool launcher set is invalid' >&2; exit 1; }
reconcile_dispatch || exit 1
for transaction in \
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
  absent "$transaction" || { echo 'host-tool transaction recovery is required' >&2; exit 1; }
done
configure_digest="$(read_implementation "$CONFIGURE_POINTER" configure-auth)" || {
  echo 'configure-auth implementation pointer is invalid' >&2; exit 1;
}
update_digest="$(read_implementation "$UPDATE_POINTER" update-image)" || {
  echo 'update-image implementation pointer is invalid' >&2; exit 1;
}
require_v2_release_binding "$configure_digest" "$update_digest" || {
  echo 'V2 host-tool marker, launchers, template, and rendered Quadlet are not exact' >&2
  exit 1
}
process_start="$(start_time "$$")"
preparing="${DISPATCH}.preparing.$$"
absent "$preparing" && absent "$DISPATCH" || exit 1
install -d -o root -g root -m 0700 "$preparing"
printf '%s\n' "$$" > "$preparing/pid"
printf '%s\n' "$process_start" > "$preparing/start-time"
printf '%s\n' "$role" > "$preparing/role"
printf '%s\n' "$configure_digest" > "$preparing/configure-auth"
printf '%s\n' "$update_digest" > "$preparing/update-image"
chown root:root "$preparing"/*
chmod 600 "$preparing"/*
sync -f "$preparing"
mv -T "$preparing" "$DISPATCH"
sync -f /run/legal-mcp
if [[ "$role" = configure-auth ]]; then
  write_auth_permit "$$" "$process_start"
elif [[ "$cutover_dispatch" = true ]]; then
  write_cutover_permit "$$" "$process_start"
fi
export LEGAL_MCP_HOST_TRANSACTION_LOCK_FD=9
exec /usr/bin/unshare --mount --propagation private -- \
  "$CANONICAL" --legal-mcp-launcher-internal "$role" \
  "$configure_digest" "$update_digest" "$@"
LAUNCHER
}

load_host_tool_bundle() {
  local expected_version="$1" binary_version
  local -a versions revisions
  HOST_TOOL_REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
  HOST_TOOL_SOURCE_DEPLOY="$HOST_TOOL_REPO_DIR/scripts/legal-mcp-host-deploy"
  HOST_TOOL_SOURCE_PUBLISHER="$HOST_TOOL_REPO_DIR/scripts/legal-mcp-publisher-command"
  HOST_TOOL_SOURCE_CONFIGURE_AUTH="$HOST_TOOL_REPO_DIR/infra/hosting/configure-auth.sh"
  HOST_TOOL_SOURCE_UPDATE_IMAGE="$HOST_TOOL_REPO_DIR/infra/hosting/update-image.sh"
  HOST_TOOL_SOURCE_CONTAINER_TEMPLATE="$HOST_TOOL_REPO_DIR/infra/hosting/legal-mcp.container.template"
  HOST_TOOL_SOURCE_CADDY_TEMPLATE="$HOST_TOOL_REPO_DIR/infra/hosting/Caddyfile"
  HOST_TOOL_SOURCE_BINARY="$HOST_TOOL_REPO_DIR/legal-mcp"
  require_release_directory "$HOST_TOOL_REPO_DIR"
  require_release_directory "$HOST_TOOL_REPO_DIR/infra"
  require_release_directory "$HOST_TOOL_REPO_DIR/infra/hosting"
  require_release_directory "$HOST_TOOL_REPO_DIR/infra/linode"
  require_release_directory "$HOST_TOOL_REPO_DIR/scripts"
  require_release_file "$HOST_TOOL_REPO_DIR/infra/linode/install-host.sh" true
  [[ ! -L "${BASH_SOURCE[0]}" \
    && "$(readlink -f "${BASH_SOURCE[0]}")" = "$HOST_TOOL_REPO_DIR/infra/linode/install-host.sh" ]] || {
    echo 'host-tool upgrade must run directly from the version-matched release bundle' >&2
    return 1
  }
  require_release_file "$HOST_TOOL_REPO_DIR/Containerfile"
  require_release_file "$HOST_TOOL_REPO_DIR/SOURCE_COMMIT"
  require_release_file "$HOST_TOOL_SOURCE_DEPLOY" true
  require_release_file "$HOST_TOOL_SOURCE_PUBLISHER" true
  require_release_file "$HOST_TOOL_SOURCE_CONFIGURE_AUTH" true
  require_release_file "$HOST_TOOL_SOURCE_UPDATE_IMAGE" true
  require_release_file "$HOST_TOOL_SOURCE_CONTAINER_TEMPLATE"
  require_release_file "$HOST_TOOL_SOURCE_CADDY_TEMPLATE"
  [[ "$(grep -o '__IMAGE_DIGEST__' "$HOST_TOOL_SOURCE_CONTAINER_TEMPLATE" | wc -l)" = 1 \
    && "$(grep -Fxc 'ExecCondition=/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready' \
      "$HOST_TOOL_SOURCE_CONTAINER_TEMPLATE")" = 1 ]] || {
    echo 'version-matched Quadlet template lacks its exact image or auth-ready gate' >&2
    return 1
  }
  require_release_file "$HOST_TOOL_SOURCE_BINARY" true
  mapfile -t versions < <(awk -F= '$1 == "ARG VERSION" {print $2}' "$HOST_TOOL_REPO_DIR/Containerfile")
  mapfile -t revisions < "$HOST_TOOL_REPO_DIR/SOURCE_COMMIT"
  [[ ${#versions[@]} -eq 1 && "${versions[0]}" = "$expected_version" \
    && ${#revisions[@]} -eq 1 && "${revisions[0]}" =~ ^[0-9a-f]{40}$ ]] || {
    echo 'release bundle version or SOURCE_COMMIT is invalid' >&2
    return 1
  }
  HOST_TOOL_VERSION="$expected_version"
  HOST_TOOL_REVISION="${revisions[0]}"
  binary_version="$(env -u LD_LIBRARY_PATH -u LD_PRELOAD \
    "$HOST_TOOL_SOURCE_BINARY" --version)"
  [[ "$binary_version" = "legal-mcp $expected_version" ]] || {
    echo 'release binary version does not match the requested host tools' >&2
    return 1
  }
  HOST_DEPLOY_SHA256="$(sha256sum "$HOST_TOOL_SOURCE_DEPLOY" | awk '{print $1}')"
  PUBLISHER_COMMAND_SHA256="$(sha256sum "$HOST_TOOL_SOURCE_PUBLISHER" | awk '{print $1}')"
  CONFIGURE_AUTH_SHA256="$(sha256sum "$HOST_TOOL_SOURCE_CONFIGURE_AUTH" | awk '{print $1}')"
  UPDATE_IMAGE_SHA256="$(sha256sum "$HOST_TOOL_SOURCE_UPDATE_IMAGE" | awk '{print $1}')"
  CONTAINER_TEMPLATE_SHA256="$(sha256sum "$HOST_TOOL_SOURCE_CONTAINER_TEMPLATE" | awk '{print $1}')"
  [[ "$HOST_DEPLOY_SHA256$PUBLISHER_COMMAND_SHA256$CONFIGURE_AUTH_SHA256$UPDATE_IMAGE_SHA256$CONTAINER_TEMPLATE_SHA256" \
    =~ ^[0-9a-f]{320}$ ]] || return 1
}

run_v0198_flat_int8_recovery() (
  local requested_version="$1" adapter adapter_tmp adapter_sha real_podman_fd
  local flock_adapter flock_adapter_tmp flock_adapter_sha real_flock_fd bridge_lock_fd
  local real_podman_identity real_podman_sha real_flock_identity real_flock_sha
  local status expected_marker transaction_path outcome transaction_count
  local already_complete=false partial_retired=false authorization_preparing name
  local v0198_version=0.19.8
  local v0198_revision=312646c34cff43f3154b43a6feb7e7f4306f30bc
  local v0198_deploy=4e6c6181a9528852de4e22e559b71076b7d0b8ac716f35d2c5d7264ec35a4533
  local v0198_publisher=4db458fa316e104ba4de412fdf9d4b7d5120677eba153eadd944dea37b36ad47
  local v0198_configure=3ece47e0f27525e45188130e6ac4215fa8276f1ddaa564544653f3daed84921e
  local v0198_update=01ab7064e6d759f4f71bcf7fbeef1e04262cd262bd87f0755306f5c62664eac8
  local v0198_template=d323504b206938ed713271cfe6a98c263f3ad513cc6a96593aa56686352a5225
  local v0198_sudoers=a6dd6f1ea819516df66eb3cc5f7fc4999432c36c9beb27ebdfb5d6ec3ec48d70
  local v0198_launcher=1d4bd49a571dcd9fc4c437c2cfb8470b182a556ecb381c4be5726ccaec9575da
  local prior_generation=a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3
  local target_generation=937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939
  local prior_image=ghcr.io/gunba/australian-legal-mcp@sha256:2f2abc22cc0bd0eb2aae2bc32f4e79ebc58b1ac0852316240f3acdf6a2e5efd9
  local target_image=ghcr.io/gunba/australian-legal-mcp@sha256:008de908c49b4975eba0f7601e6a554b27ede8202a9e5fe26197c6221b03e3f0
  local transaction=/etc/legal-mcp/.image-transaction
  local journal=/srv/legal-mcp/lifecycle/.deployment-transaction
  local image_file=/etc/legal-mcp/image
  local authorization=/run/legal-mcp/authorized-upload
  local -a marker journal_values expected_marker transaction_names
  transaction_names=(
    kind target-version target-revision updater-sha256 retirement-outcome
    release-sha256 saved-sha256 target-sha256 saved-metadata target-metadata state
    saved-image saved-quadlet saved-template saved-runtime.env saved-api-keys.json
    saved-Caddyfile saved-auth-ready saved-active-generation saved-deployment-journal
    saved-upload-authorization target-image target-quadlet target-template
    target-active-generation
  )

  [[ "$requested_version" = 0.19.9 ]] || {
    echo 'the v0.19.8 cutover bridge exists only in the exact v0.19.9 release' >&2
    return 1
  }
  load_host_tool_bundle "$requested_version"
  # The fixed updater line is matched literally; command substitution here
  # would invalidate the release check.
  # shellcheck disable=SC2016
  [[ "$HOST_TOOL_REVISION" != "$v0198_revision" \
    && "$(grep -Fxc '  report="$(podman top australian-legal-mcp capbnd capeff capinh capprm)" || {' \
      "$HOST_TOOL_SOURCE_UPDATE_IMAGE")" = 1 ]] || {
      echo 'the recovery release does not contain the fixed live-capability verifier' >&2
      return 1
    }
  if grep -Fq '{{json .EffectiveCaps}}' "$HOST_TOOL_SOURCE_UPDATE_IMAGE"; then
    echo 'the recovery release still trusts the incompatible EffectiveCaps field' >&2
    return 1
  fi

  require_regular_file /run/lock/legal-mcp-host-transaction.lock \
    root legal-mcp-publisher 640
  bridge_lock_fd=8
  exec 8<>/run/lock/legal-mcp-host-transaction.lock
  /usr/bin/flock -x "$bridge_lock_fd"

  require_regular_file "$HOST_TOOLS_MARKER" root root 444
  mapfile -t marker < "$HOST_TOOLS_MARKER"
  expected_marker=(
    LEGAL_MCP_HOST_TOOLS_V2
    "VERSION=$v0198_version"
    "SOURCE_COMMIT=$v0198_revision"
    "HOST_DEPLOY_SHA256=$v0198_deploy"
    "PUBLISHER_COMMAND_SHA256=$v0198_publisher"
    "CONFIGURE_AUTH_SHA256=$v0198_configure"
    "UPDATE_IMAGE_SHA256=$v0198_update"
    "CONTAINER_TEMPLATE_SHA256=$v0198_template"
    "SUDOERS_SHA256=$v0198_sudoers"
  )
  [[ "${marker[*]}" = "${expected_marker[*]}" ]] || {
    echo 'installed host tools are not the exact recoverable v0.19.8 release' >&2
    return 1
  }
  require_regular_file "$HOST_TOOL_LAUNCHER" root root 755
  require_regular_file "$CONFIGURE_AUTH" root root 755
  require_regular_file "$UPDATE_IMAGE" root root 755
  require_regular_file "$HOST_TOOL_LAUNCHER_MARKER" root root 444
  require_regular_file "$CONFIGURE_AUTH_POINTER" root root 644
  require_regular_file "$UPDATE_IMAGE_POINTER" root root 644
  [[ "$(sha256sum "$HOST_TOOL_LAUNCHER" | awk '{print $1}')" = "$v0198_launcher" \
    && "$(sha256sum "$CONFIGURE_AUTH" | awk '{print $1}')" = "$v0198_launcher" \
    && "$(sha256sum "$UPDATE_IMAGE" | awk '{print $1}')" = "$v0198_launcher" \
    && "$(<"$CONFIGURE_AUTH_POINTER")" = "$v0198_configure" \
    && "$(<"$UPDATE_IMAGE_POINTER")" = "$v0198_update" \
    && "$(sha256sum "$HOST_TOOL_IMPLEMENTATION_DIR/configure-auth.$v0198_configure" | awk '{print $1}')" = "$v0198_configure" \
    && "$(sha256sum "$HOST_TOOL_IMPLEMENTATION_DIR/update-image.$v0198_update" | awk '{print $1}')" = "$v0198_update" \
    && "$(sha256sum "$HOST_DEPLOY" | awk '{print $1}')" = "$v0198_deploy" \
    && "$(sha256sum "$PUBLISHER_COMMAND" | awk '{print $1}')" = "$v0198_publisher" \
    && "$(sha256sum "$CONTAINER_TEMPLATE" | awk '{print $1}')" = "$v0198_template" \
    && "$(sha256sum "$PUBLISHER_SUDOERS" | awk '{print $1}')" = "$v0198_sudoers" ]] || {
      echo 'recoverable v0.19.8 host-tool bytes or pointers changed' >&2
      return 1
    }
  mapfile -t marker < "$HOST_TOOL_LAUNCHER_MARKER"
  [[ ${#marker[@]} -eq 2 \
    && "${marker[0]}" = LEGAL_MCP_HOST_TOOL_LAUNCHER_V1 \
    && "${marker[1]}" = "LAUNCHER_SHA256=$v0198_launcher" ]] || return 1

  services_and_ingress_are_off
  path_is_absent "$AUTH_READY_MARKER"
  for path in /etc/legal-mcp/.auth-transaction* /etc/legal-mcp/.host-tools-transaction* \
    /etc/legal-mcp/.image-transaction.preparing \
    /etc/legal-mcp/.image-transaction.preparing-retired \
    /etc/legal-mcp/.image-transaction.flat-int8-preparing \
    /etc/legal-mcp/.image-transaction.flat-int8-preparing-retired; do
    path_is_absent "$path" || {
      echo 'a foreign host, authentication, or image transaction blocks cutover recovery' >&2
      return 1
    }
  done

  v0198_verify_saved_state() {
    local restore_authorization="${1:-false}"
    require_regular_file "$journal" root root 600 || return 1
    require_regular_file /srv/legal-mcp/lifecycle/active-generation root root 644 || return 1
    require_regular_file "$image_file" root root 600 || return 1
    mapfile -t journal_values < "$journal"
    [[ ${#journal_values[@]} -eq 3 \
      && "${journal_values[0]}" = "$target_generation" \
      && "${journal_values[1]}" = "$prior_generation" \
      && "${journal_values[2]}" = prepared \
      && "$(</srv/legal-mcp/lifecycle/active-generation)" = "$prior_generation" \
      && "$(<"$image_file")" = "$prior_image" \
      && -d "/srv/legal-mcp/uploads/$target_generation" \
      && ! -L "/srv/legal-mcp/uploads/$target_generation" \
      && ! -e "/srv/legal-mcp/generations/$target_generation" \
      && "$(stat -c '%U:%G:%a' "/srv/legal-mcp/uploads/$target_generation")" \
        = legal-mcp-publisher:legal-mcp-publisher:700 ]] || return 1
    require_exact_acl "/srv/legal-mcp/uploads/$target_generation" \
      $'user::rwx\ngroup::---\nother::---' || return 1
    directory_contains_only /srv/legal-mcp/uploads "$target_generation" || return 1
    directory_contains_only /srv/legal-mcp/lifecycle \
      .deployment-transaction active-generation LIFECYCLE_LOCK LOCK || return 1
    authorization_preparing=/run/legal-mcp/authorized-upload.v0198-preparing
    if ! path_is_absent "$authorization_preparing"; then
      require_regular_file "$authorization_preparing" \
        root legal-mcp-publisher 440 || return 1
      if [[ "$(<"$authorization_preparing")" != "$target_generation" ]]; then
        [[ "$restore_authorization" = true ]] || return 1
        rm -f -- "$authorization_preparing"
        sync -f /run/legal-mcp
      fi
    fi
    if path_is_absent "$authorization"; then
      [[ "$restore_authorization" = true \
        && -d /run/legal-mcp && ! -L /run/legal-mcp \
        && "$(stat -c '%U:%G:%a' /run/legal-mcp)" \
          = root:legal-mcp-publisher:710 ]] || return 1
      if path_is_absent "$authorization_preparing"; then
        install -o root -g legal-mcp-publisher -m 0440 /dev/null \
          "$authorization_preparing"
        printf '%s\n' "$target_generation" > "$authorization_preparing"
        sync -f "$authorization_preparing"
      fi
      mv -fT "$authorization_preparing" "$authorization"
      sync -f /run/legal-mcp
    else
      path_is_absent "$authorization_preparing" || return 1
    fi
    require_regular_file "$authorization" root legal-mcp-publisher 440 || return 1
    [[ "$(<"$authorization")" = "$target_generation" ]] || return 1
  }

  transaction_count=0
  transaction_path=
  for path in "$transaction" "$transaction.retiring" "$transaction.retired"; do
    if ! path_is_absent "$path"; then
      transaction_count=$((transaction_count + 1))
      transaction_path="$path"
    fi
  done
  [[ $transaction_count -le 1 ]] || {
    echo 'v0.19.8 cutover has conflicting retirement states' >&2
    return 1
  }

  if [[ $transaction_count -eq 0 ]]; then
    v0198_verify_saved_state true || {
      echo 'completed v0.19.8 recovery state is not exact' >&2
      return 1
    }
    already_complete=true
  else
    [[ -d "$transaction_path" && ! -L "$transaction_path" \
      && "$(stat -c '%U:%G:%a' "$transaction_path")" = root:root:700 ]] || {
        echo 'the v0.19.8 image transaction has an unsafe retirement state' >&2
        return 1
      }
    if [[ "$transaction_path" = "$transaction.retired" ]]; then
      directory_contains_only "$transaction_path" "${transaction_names[@]}" || {
        echo 'retired v0.19.8 transaction contains unexpected state' >&2
        return 1
      }
      for name in "${transaction_names[@]}"; do
        if path_is_absent "$transaction_path/$name"; then
          partial_retired=true
        else
          require_regular_file "$transaction_path/$name" root root 600
        fi
      done
    fi
    if [[ "$partial_retired" = true ]]; then
      v0198_verify_saved_state true || {
        echo 'partially deleted v0.19.8 transaction lacks its exact saved state' >&2
        return 1
      }
      for name in \
        "kind:LEGAL_MCP_FLAT_INT8_CUTOVER_TRANSACTION_V1" \
        "target-version:$v0198_version" \
        "target-revision:$v0198_revision" \
        "updater-sha256:$v0198_update" \
        'retirement-outcome:saved' \
        "saved-active-generation:$prior_generation" \
        "target-active-generation:$target_generation" \
        "saved-image:$prior_image" \
        "target-image:$target_image"; do
        path="$transaction_path/${name%%:*}"
        if ! path_is_absent "$path"; then
          [[ "$(<"$path")" = "${name#*:}" ]] || return 1
        fi
      done
      if ! path_is_absent "$transaction_path/state"; then
        grep -Fxq 'PUBLICATION_STATE=configured-dark' "$transaction_path/state"
        grep -Fxq "PRIOR_GENERATION=$prior_generation" "$transaction_path/state"
        grep -Fxq "TARGET_GENERATION=$target_generation" "$transaction_path/state"
        grep -Fxq "OLD_IMAGE=$prior_image" "$transaction_path/state"
        grep -Fxq "TARGET_IMAGE=$target_image" "$transaction_path/state"
        grep -Fxq 'UPLOAD_AUTHORIZATION=present' "$transaction_path/state"
      fi
    else
      for path in kind target-version target-revision updater-sha256 retirement-outcome \
        saved-active-generation target-active-generation saved-image target-image state; do
        require_regular_file "$transaction_path/$path" root root 600
      done
      outcome="$(<"$transaction_path/retirement-outcome")"
      [[ "$(<"$transaction_path/kind")" = LEGAL_MCP_FLAT_INT8_CUTOVER_TRANSACTION_V1 \
        && "$(<"$transaction_path/target-version")" = "$v0198_version" \
        && "$(<"$transaction_path/target-revision")" = "$v0198_revision" \
        && "$(<"$transaction_path/updater-sha256")" = "$v0198_update" \
        && ( "$outcome" = pending || "$outcome" = saved ) \
        && "$(<"$transaction_path/saved-active-generation")" = "$prior_generation" \
        && "$(<"$transaction_path/target-active-generation")" = "$target_generation" \
        && "$(<"$transaction_path/saved-image")" = "$prior_image" \
        && "$(<"$transaction_path/target-image")" = "$target_image" ]] || {
          echo 'cutover is not the exact recoverable v0.19.8 transaction' >&2
          return 1
        }
      grep -Fxq 'PUBLICATION_STATE=configured-dark' "$transaction_path/state"
      grep -Fxq "PRIOR_GENERATION=$prior_generation" "$transaction_path/state"
      grep -Fxq "TARGET_GENERATION=$target_generation" "$transaction_path/state"
      grep -Fxq "OLD_IMAGE=$prior_image" "$transaction_path/state"
      grep -Fxq "TARGET_IMAGE=$target_image" "$transaction_path/state"
      grep -Fxq 'UPLOAD_AUTHORIZATION=present' "$transaction_path/state"
      require_regular_file /srv/legal-mcp/lifecycle/active-generation root root 644
      require_regular_file "$image_file" root root 600
      [[ "$(</srv/legal-mcp/lifecycle/active-generation)" = "$prior_generation" \
        && "$(<"$image_file")" = "$prior_image" ]] || {
          echo 'v0.19.8 saved generation/image pair is not active' >&2
          return 1
        }
      if [[ "$outcome" = saved ]]; then
        v0198_verify_saved_state true || return 1
      fi
    fi
  fi

  if [[ "$already_complete" = true ]]; then
    for path in /run/legal-mcp-v0198-podman-adapter \
      /run/legal-mcp-v0198-flock-adapter; do
      if ! path_is_absent "$path"; then
        require_regular_file "$path" root root 500
        rm -f -- "$path"
      fi
    done
    sync -f /run
  fi

  adapter=/run/legal-mcp-v0198-podman-adapter
  adapter_tmp="$(mktemp /run/.legal-mcp-v0198-podman-adapter.XXXXXX)"
  cat > "$adapter_tmp" <<'ADAPTER'
#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
fd="${LEGAL_MCP_V0198_REAL_PODMAN_FD:?}"
real="/proc/$BASHPID/fd/$fd"
[[ "$fd" =~ ^[0-9]+$ && -e "$real" \
  && "$(stat -Lc '%d:%i' "$real")" = "${LEGAL_MCP_V0198_REAL_PODMAN_IDENTITY:?}" \
  && "$(sha256sum "$real" | awk '{print $1}')" = "${LEGAL_MCP_V0198_REAL_PODMAN_SHA256:?}" ]] || {
    echo 'v0.19.8 recovery lost the exact real Podman executable' >&2
    exit 1
  }
if [[ $# -eq 4 && "$1" = inspect && "$2" = australian-legal-mcp \
  && "$3" = --format && "$4" = '{{json .EffectiveCaps}}' ]]; then
  observed="$("$real" "$@")"
  [[ "$observed" = null || "$observed" = '[]' ]] || {
    echo 'v0.19.8 recovery observed an unexpected EffectiveCaps representation' >&2
    exit 1
  }
  report="$("$real" top australian-legal-mcp capbnd capeff capinh capprm)"
  python3 - "$report" <<'PY'
import sys
lines = sys.argv[1].splitlines()
expected = "BOUNDING CAPS EFFECTIVE CAPS INHERITED CAPS PERMITTED CAPS"
if not lines or " ".join(lines[0].split()) != expected or len(lines) < 2:
    raise SystemExit(1)
for line in lines[1:]:
    if line.split() != ["none", "none", "none", "none"]:
        raise SystemExit(1)
PY
  printf '%s\n' '[]'
  exit 0
fi
exec "$real" "$@"
ADAPTER
  chown root:root "$adapter_tmp"
  chmod 500 "$adapter_tmp"
  adapter_sha="$(sha256sum "$adapter_tmp" | awk '{print $1}')"
  if ! path_is_absent "$adapter"; then
    require_regular_file "$adapter" root root 500
    [[ "$(sha256sum "$adapter" | awk '{print $1}')" = "$adapter_sha" ]] || {
      rm -f "$adapter_tmp"
      echo 'stale v0.19.8 Podman adapter is not the exact recovery bridge' >&2
      return 1
    }
    rm -f "$adapter_tmp"
  else
    mv -T "$adapter_tmp" "$adapter"
    sync -f /run
  fi

  flock_adapter=/run/legal-mcp-v0198-flock-adapter
  flock_adapter_tmp="$(mktemp /run/.legal-mcp-v0198-flock-adapter.XXXXXX)"
  cat > "$flock_adapter_tmp" <<'FLOCK_ADAPTER'
#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
fd="${LEGAL_MCP_V0198_REAL_FLOCK_FD:?}"
real="/proc/$BASHPID/fd/$fd"
lock=/run/lock/legal-mcp-host-transaction.lock
bridge_fd="${LEGAL_MCP_V0198_BRIDGE_LOCK_FD:?}"
[[ "$fd" =~ ^[0-9]+$ && "$bridge_fd" =~ ^[0-9]+$ && -e "$real" \
  && "$(stat -Lc '%d:%i' "$real")" = "${LEGAL_MCP_V0198_REAL_FLOCK_IDENTITY:?}" \
  && "$(sha256sum "$real" | awk '{print $1}')" = "${LEGAL_MCP_V0198_REAL_FLOCK_SHA256:?}" ]] || {
    echo 'v0.19.8 recovery lost the exact real flock executable' >&2
    exit 1
  }
if [[ $# -eq 2 && "$1" = -x && "$2" = 9 ]]; then
  lock_identity="$(stat -Lc '%d:%i' "$lock")"
  [[ -e "/proc/$BASHPID/fd/$bridge_fd" && -e /proc/$BASHPID/fd/9 \
    && "$(stat -Lc '%d:%i' "/proc/$BASHPID/fd/$bridge_fd")" = "$lock_identity" \
    && "$(stat -Lc '%d:%i' /proc/$BASHPID/fd/9)" = "$lock_identity" ]] || {
      echo 'v0.19.8 recovery did not inherit the exact shared host lock' >&2
      exit 1
    }
  status=0
  "$real" --exclusive --nonblock "$lock" --command /usr/bin/true \
    >/dev/null 2>&1 || status=$?
  [[ $status -eq 1 ]] || {
    echo 'v0.19.8 recovery shared host lock is not held' >&2
    exit 1
  }
  exit 0
fi
exec "$real" "$@"
FLOCK_ADAPTER
  chown root:root "$flock_adapter_tmp"
  chmod 500 "$flock_adapter_tmp"
  flock_adapter_sha="$(sha256sum "$flock_adapter_tmp" | awk '{print $1}')"
  if ! path_is_absent "$flock_adapter"; then
    require_regular_file "$flock_adapter" root root 500
    [[ "$(sha256sum "$flock_adapter" | awk '{print $1}')" = "$flock_adapter_sha" ]] || {
      rm -f "$flock_adapter_tmp"
      echo 'stale v0.19.8 flock adapter is not the exact recovery bridge' >&2
      return 1
    }
    rm -f "$flock_adapter_tmp"
  else
    mv -T "$flock_adapter_tmp" "$flock_adapter"
    sync -f /run
  fi

  real_podman_fd=7
  real_flock_fd=6
  exec 7</usr/bin/podman
  exec 6</usr/bin/flock
  real_podman_identity="$(stat -Lc '%d:%i' "/proc/$BASHPID/fd/$real_podman_fd")"
  real_podman_sha="$(sha256sum "/proc/$BASHPID/fd/$real_podman_fd" | awk '{print $1}')"
  real_flock_identity="$(stat -Lc '%d:%i' "/proc/$BASHPID/fd/$real_flock_fd")"
  real_flock_sha="$(sha256sum "/proc/$BASHPID/fd/$real_flock_fd" | awk '{print $1}')"
  export LEGAL_MCP_V0198_REAL_PODMAN_FD="$real_podman_fd"
  export LEGAL_MCP_V0198_REAL_PODMAN_IDENTITY="$real_podman_identity"
  export LEGAL_MCP_V0198_REAL_PODMAN_SHA256="$real_podman_sha"
  export LEGAL_MCP_V0198_REAL_FLOCK_FD="$real_flock_fd"
  export LEGAL_MCP_V0198_REAL_FLOCK_IDENTITY="$real_flock_identity"
  export LEGAL_MCP_V0198_REAL_FLOCK_SHA256="$real_flock_sha"
  export LEGAL_MCP_V0198_BRIDGE_LOCK_FD="$bridge_lock_fd"
  export LEGAL_MCP_V0198_PODMAN_ADAPTER="$adapter"
  export LEGAL_MCP_V0198_FLOCK_ADAPTER="$flock_adapter"
  status=0
  # The recovery environment is intentionally expanded only inside the
  # private mount namespace.
  # shellcheck disable=SC2016
  /usr/bin/unshare --mount --propagation private -- /usr/bin/bash -ceu '
    mount --bind "$LEGAL_MCP_V0198_PODMAN_ADAPTER" /usr/bin/podman
    mount -o remount,bind,ro,nodev,nosuid /usr/bin/podman
    mount --bind "$LEGAL_MCP_V0198_FLOCK_ADAPTER" /usr/bin/flock
    mount -o remount,bind,ro,nodev,nosuid /usr/bin/flock
    exec /usr/local/sbin/legal-mcp-update-image --recover --flat-int8-cutover
  ' || status=$?
  rm -f "$adapter" "$flock_adapter"
  sync -f /run
  unset LEGAL_MCP_V0198_REAL_PODMAN_FD LEGAL_MCP_V0198_REAL_PODMAN_IDENTITY \
    LEGAL_MCP_V0198_REAL_PODMAN_SHA256 LEGAL_MCP_V0198_REAL_FLOCK_FD \
    LEGAL_MCP_V0198_REAL_FLOCK_IDENTITY LEGAL_MCP_V0198_REAL_FLOCK_SHA256 \
    LEGAL_MCP_V0198_BRIDGE_LOCK_FD LEGAL_MCP_V0198_PODMAN_ADAPTER \
    LEGAL_MCP_V0198_FLOCK_ADAPTER
  if [[ "$already_complete" = false && $status -ne 0 ]]; then
    echo 'v0.19.8 flat-int8 recovery failed; host remains configured-dark' >&2
    return "$status"
  fi
  path_is_absent "$transaction"
  path_is_absent "$transaction.retiring"
  path_is_absent "$transaction.retired"
  v0198_verify_saved_state true
  for path in /run/legal-mcp/host-tool-launcher-dispatch \
    /run/legal-mcp/host-tool-launcher-dispatch.retiring \
    /run/legal-mcp/host-tool-launcher-dispatch.retired \
    /run/legal-mcp/flat-int8-cutover-starting \
    /run/legal-mcp/flat-int8-cutover-start-armed \
    /run/legal-mcp/authorized-upload.v0198-preparing \
    /run/legal-mcp-v0198-podman-adapter \
    /run/legal-mcp-v0198-flock-adapter; do
    path_is_absent "$path"
  done
  services_and_ingress_are_off
  path_is_absent "$AUTH_READY_MARKER"
  if [[ "$already_complete" = true ]]; then
    echo 'exact v0.19.8 flat-int8 transaction was already recovered to the configured-dark saved pair'
  else
    echo 'exact v0.19.8 flat-int8 transaction recovered to the configured-dark saved pair'
  fi
)

render_host_tool_launcher_marker() {
  local launcher_sha256="$1" destination="$2"
  cat > "$destination" <<EOF
LEGAL_MCP_HOST_TOOL_LAUNCHER_V1
LAUNCHER_SHA256=$launcher_sha256
EOF
  chmod 444 "$destination"
}

install_immutable_host_tool_implementation() {
  local source="$1" destination="$2" expected_sha256="$3" temporary
  require_safe_directory "$HOST_TOOL_IMPLEMENTATION_DIR" root root || return 1
  if ! path_is_absent "$destination"; then
    require_regular_file "$destination" root root 755 || return 1
    [[ "$(sha256sum "$destination" | awk '{print $1}')" = "$expected_sha256" ]] || {
      echo "immutable host-tool implementation has changed: $destination" >&2
      return 1
    }
    return 0
  fi
  temporary="$(mktemp "$HOST_TOOL_IMPLEMENTATION_DIR/.implementation.XXXXXX")"
  install -o root -g root -m 0755 "$source" "$temporary"
  [[ "$(sha256sum "$temporary" | awk '{print $1}')" = "$expected_sha256" ]] || {
    rm -f "$temporary"
    return 1
  }
  sync -f "$temporary"
  mv -T "$temporary" "$destination"
  sync -f "$HOST_TOOL_IMPLEMENTATION_DIR"
  require_regular_file "$destination" root root 755
}

write_pointer_source() {
  local value="$1" destination="$2"
  [[ "$value" =~ ^[0-9a-f]{64}$ ]] || return 1
  printf '%s' "$value" > "$destination"
  chmod 644 "$destination"
  [[ "$(stat -c '%s' "$destination")" = 64 ]]
}

prepare_host_tool_runtime_sources() {
  local -a external_urls
  HOST_TOOL_LAUNCHER_SOURCE="$(mktemp /run/legal-mcp-host-tool-launcher.XXXXXX)"
  render_host_tool_launcher > "$HOST_TOOL_LAUNCHER_SOURCE"
  chmod 755 "$HOST_TOOL_LAUNCHER_SOURCE"
  HOST_TOOL_LAUNCHER_SHA256="$(sha256sum "$HOST_TOOL_LAUNCHER_SOURCE" | awk '{print $1}')"
  [[ "$HOST_TOOL_LAUNCHER_SHA256" =~ ^[0-9a-f]{64}$ ]] || return 1
  HOST_TOOL_LAUNCHER_MARKER_SOURCE="$(mktemp /etc/legal-mcp/.host-tool-launcher.XXXXXX)"
  render_host_tool_launcher_marker \
    "$HOST_TOOL_LAUNCHER_SHA256" "$HOST_TOOL_LAUNCHER_MARKER_SOURCE"
  HOST_TOOL_CONFIGURE_POINTER_SOURCE="$(mktemp /etc/legal-mcp/.configure-auth-pointer.XXXXXX)"
  HOST_TOOL_UPDATE_POINTER_SOURCE="$(mktemp /etc/legal-mcp/.update-image-pointer.XXXXXX)"
  write_pointer_source "$CONFIGURE_AUTH_SHA256" "$HOST_TOOL_CONFIGURE_POINTER_SOURCE"
  write_pointer_source "$UPDATE_IMAGE_SHA256" "$HOST_TOOL_UPDATE_POINTER_SOURCE"
  HOST_TOOL_CONFIGURE_IMPLEMENTATION="$HOST_TOOL_IMPLEMENTATION_DIR/configure-auth.$CONFIGURE_AUTH_SHA256"
  HOST_TOOL_UPDATE_IMPLEMENTATION="$HOST_TOOL_IMPLEMENTATION_DIR/update-image.$UPDATE_IMAGE_SHA256"
  install -d -o root -g root -m 0755 "$HOST_TOOL_IMPLEMENTATION_DIR"
  install_immutable_host_tool_implementation \
    "$HOST_TOOL_SOURCE_CONFIGURE_AUTH" "$HOST_TOOL_CONFIGURE_IMPLEMENTATION" \
    "$CONFIGURE_AUTH_SHA256"
  install_immutable_host_tool_implementation \
    "$HOST_TOOL_SOURCE_UPDATE_IMAGE" "$HOST_TOOL_UPDATE_IMPLEMENTATION" \
    "$UPDATE_IMAGE_SHA256"
  HOST_TOOL_RENDERED_QUADLET_SOURCE="$(mktemp /run/legal-mcp-host-tools-rendered.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$(</etc/legal-mcp/image)|g" \
    "$HOST_TOOL_SOURCE_CONTAINER_TEMPLATE" > "$HOST_TOOL_RENDERED_QUADLET_SOURCE"
  chmod 644 "$HOST_TOOL_RENDERED_QUADLET_SOURCE"
  if [[ -n "${PUBLIC_HOST:-}" ]]; then
    [[ "$PUBLIC_HOST" =~ ^[a-z0-9.-]{3,253}$ ]] || return 1
    HOST_TOOL_PUBLIC_HOST="$PUBLIC_HOST"
  else
    mapfile -t external_urls < <(
      awk -F= '$1 == "LEGAL_MCP_EXTERNAL_URL" {print $2}' /etc/legal-mcp/runtime.env
    )
    [[ ${#external_urls[@]} -eq 1 \
      && "${external_urls[0]}" =~ ^https://([a-z0-9.-]{3,253})/mcp$ ]] || {
      echo 'installed runtime has no exact public host for Caddy rendering' >&2
      return 1
    }
    HOST_TOOL_PUBLIC_HOST="${BASH_REMATCH[1]}"
  fi
  HOST_TOOL_RENDERED_CADDY_SOURCE="$(mktemp /run/legal-mcp-host-tools-caddy.XXXXXX)"
  sed "s/__PUBLIC_HOST__/$HOST_TOOL_PUBLIC_HOST/g" \
    "$HOST_TOOL_SOURCE_CADDY_TEMPLATE" > "$HOST_TOOL_RENDERED_CADDY_SOURCE"
  chmod 640 "$HOST_TOOL_RENDERED_CADDY_SOURCE"
}

validate_installed_launcher_state() {
  local launcher_sha configure_sha update_sha path
  local -a marker
  require_regular_file "$HOST_TOOL_LAUNCHER_MARKER" root root 444 || return 1
  require_exact_acl "$HOST_TOOL_LAUNCHER_MARKER" $'user::r--\ngroup::r--\nother::r--' || return 1
  mapfile -t marker < "$HOST_TOOL_LAUNCHER_MARKER"
  [[ ${#marker[@]} -eq 2 \
    && "${marker[0]}" = LEGAL_MCP_HOST_TOOL_LAUNCHER_V1 \
    && "${marker[1]}" =~ ^LAUNCHER_SHA256=([0-9a-f]{64})$ ]] || {
    echo 'installed host-tool launcher marker is malformed' >&2
    return 1
  }
  launcher_sha="${BASH_REMATCH[1]}"
  for path in "$HOST_TOOL_LAUNCHER" "$CONFIGURE_AUTH" "$UPDATE_IMAGE"; do
    require_regular_file "$path" root root 755 || return 1
    require_exact_acl "$path" $'user::rwx\ngroup::r-x\nother::r-x' || return 1
    [[ "$(sha256sum "$path" | awk '{print $1}')" = "$launcher_sha" ]] || {
      echo "installed stable launcher bytes changed: $path" >&2
      return 1
    }
  done
  require_regular_file "$CONFIGURE_AUTH_POINTER" root root 644 || return 1
  require_regular_file "$UPDATE_IMAGE_POINTER" root root 644 || return 1
  require_exact_acl "$CONFIGURE_AUTH_POINTER" $'user::rw-\ngroup::r--\nother::r--' || return 1
  require_exact_acl "$UPDATE_IMAGE_POINTER" $'user::rw-\ngroup::r--\nother::r--' || return 1
  [[ "$(stat -c '%s' "$CONFIGURE_AUTH_POINTER")" = 64 \
    && "$(stat -c '%s' "$UPDATE_IMAGE_POINTER")" = 64 ]] || {
    echo 'host-tool implementation pointers must contain exactly 64 bytes' >&2
    return 1
  }
  configure_sha="$(<"$CONFIGURE_AUTH_POINTER")"
  update_sha="$(<"$UPDATE_IMAGE_POINTER")"
  [[ "$configure_sha" =~ ^[0-9a-f]{64}$ && "$update_sha" =~ ^[0-9a-f]{64}$ ]] || return 1
  require_regular_file "$HOST_TOOL_IMPLEMENTATION_DIR/configure-auth.$configure_sha" root root 755
  require_regular_file "$HOST_TOOL_IMPLEMENTATION_DIR/update-image.$update_sha" root root 755
  require_exact_acl "$HOST_TOOL_IMPLEMENTATION_DIR/configure-auth.$configure_sha" \
    $'user::rwx\ngroup::r-x\nother::r-x' || return 1
  require_exact_acl "$HOST_TOOL_IMPLEMENTATION_DIR/update-image.$update_sha" \
    $'user::rwx\ngroup::r-x\nother::r-x' || return 1
  [[ "$(sha256sum "$HOST_TOOL_IMPLEMENTATION_DIR/configure-auth.$configure_sha" | awk '{print $1}')" = "$configure_sha" \
    && "$(sha256sum "$HOST_TOOL_IMPLEMENTATION_DIR/update-image.$update_sha" | awk '{print $1}')" = "$update_sha" ]] || {
    echo 'immutable host-tool implementation does not match its pointer' >&2
    return 1
  }
}

classify_installed_auth_image_entrypoints() {
  local launcher_paths=0 path
  for path in "$HOST_TOOL_LAUNCHER" "$HOST_TOOL_LAUNCHER_MARKER" \
    "$CONFIGURE_AUTH_POINTER" "$UPDATE_IMAGE_POINTER"; do
    if ! path_is_absent "$path"; then ((launcher_paths += 1)); fi
  done
  if [[ $launcher_paths -eq 0 ]]; then
    require_regular_file "$CONFIGURE_AUTH" root root 755 || return 1
    require_regular_file "$UPDATE_IMAGE" root root 755 || return 1
    HOST_TOOL_ENTRYPOINT_STATE=legacy
    return 0
  fi
  [[ $launcher_paths -eq 4 ]] || {
    echo 'installed host-tool launcher migration state is incomplete' >&2
    return 1
  }
  validate_installed_launcher_state || return 1
  HOST_TOOL_ENTRYPOINT_STATE=stable
}

validate_legacy_v0192_host_tool_identity() {
  local deploy_sha publisher_sha sudoers_sha
  local -a marker_values
  if path_is_absent "$HOST_TOOLS_MARKER"; then
    [[ "$HOST_TOOL_HOST_STATE" = prepared ]] || {
      echo 'activated legacy host lacks its v0.19.2 host-tool marker' >&2
      return 1
    }
    return 0
  fi
  require_regular_file "$HOST_TOOLS_MARKER" root root 444 || return 1
  require_exact_acl "$HOST_TOOLS_MARKER" $'user::r--\ngroup::r--\nother::r--' || return 1
  mapfile -t marker_values < "$HOST_TOOLS_MARKER"
  [[ ${#marker_values[@]} -eq 6 \
    && "${marker_values[0]}" = LEGAL_MCP_HOST_TOOLS_V1 \
    && "${marker_values[1]}" = VERSION=0.19.2 \
    && "${marker_values[2]}" =~ ^SOURCE_COMMIT=[0-9a-f]{40}$ \
    && "${marker_values[3]}" =~ ^HOST_DEPLOY_SHA256=([0-9a-f]{64})$ ]] || {
    echo 'legacy host-tool marker is not the exact v0.19.2 schema' >&2
    return 1
  }
  deploy_sha="${BASH_REMATCH[1]}"
  [[ "${marker_values[4]}" =~ ^PUBLISHER_COMMAND_SHA256=([0-9a-f]{64})$ ]] || return 1
  publisher_sha="${BASH_REMATCH[1]}"
  [[ "${marker_values[5]}" =~ ^SUDOERS_SHA256=([0-9a-f]{64})$ ]] || return 1
  sudoers_sha="${BASH_REMATCH[1]}"
  [[ "$(sha256sum "$HOST_DEPLOY" | awk '{print $1}')" = "$deploy_sha" \
    && "$(sha256sum "$PUBLISHER_COMMAND" | awk '{print $1}')" = "$publisher_sha" \
    && "$(sha256sum "$PUBLISHER_SUDOERS" | awk '{print $1}')" = "$sudoers_sha" ]] || {
    echo 'legacy v0.19.2 marker does not bind the installed publisher bytes' >&2
    return 1
  }
}

validate_target_launcher_state() {
  validate_installed_launcher_state || return 1
  cmp --silent "$HOST_TOOL_LAUNCHER_SOURCE" "$HOST_TOOL_LAUNCHER"
  cmp --silent "$HOST_TOOL_LAUNCHER_SOURCE" "$CONFIGURE_AUTH"
  cmp --silent "$HOST_TOOL_LAUNCHER_SOURCE" "$UPDATE_IMAGE"
  cmp --silent "$HOST_TOOL_LAUNCHER_MARKER_SOURCE" "$HOST_TOOL_LAUNCHER_MARKER"
  cmp --silent "$HOST_TOOL_CONFIGURE_POINTER_SOURCE" "$CONFIGURE_AUTH_POINTER"
  cmp --silent "$HOST_TOOL_UPDATE_POINTER_SOURCE" "$UPDATE_IMAGE_POINTER"
  cmp --silent "$HOST_TOOL_SOURCE_CONFIGURE_AUTH" "$HOST_TOOL_CONFIGURE_IMPLEMENTATION"
  cmp --silent "$HOST_TOOL_SOURCE_UPDATE_IMAGE" "$HOST_TOOL_UPDATE_IMPLEMENTATION"
}

validate_live_target_host_tools() {
  require_regular_file "$HOST_DEPLOY" root root 755 || return 1
  require_regular_file "$PUBLISHER_COMMAND" root root 755 || return 1
  require_regular_file "$PUBLISHER_SUDOERS" root root 440 || return 1
  require_regular_file "$CONTAINER_TEMPLATE" root root 644 || return 1
  require_regular_file "$RENDERED_QUADLET" root root 644 || return 1
  require_regular_file "$CADDYFILE" root caddy 640 || return 1
  require_regular_file "$HOST_TOOLS_MARKER" root root 444 || return 1
  require_exact_acl "$HOST_DEPLOY" $'user::rwx\ngroup::r-x\nother::r-x' || return 1
  require_exact_acl "$PUBLISHER_COMMAND" $'user::rwx\ngroup::r-x\nother::r-x' || return 1
  require_exact_acl "$PUBLISHER_SUDOERS" $'user::r--\ngroup::r--\nother::---' || return 1
  require_exact_acl "$CONTAINER_TEMPLATE" $'user::rw-\ngroup::r--\nother::r--' || return 1
  require_exact_acl "$RENDERED_QUADLET" $'user::rw-\ngroup::r--\nother::r--' || return 1
  require_exact_acl "$CADDYFILE" $'user::rw-\ngroup::r--\nother::---' || return 1
  require_exact_acl "$HOST_TOOLS_MARKER" $'user::r--\ngroup::r--\nother::r--' || return 1
  cmp --silent "$HOST_TOOL_SOURCE_DEPLOY" "$HOST_DEPLOY" || return 1
  cmp --silent "$HOST_TOOL_SOURCE_PUBLISHER" "$PUBLISHER_COMMAND" || return 1
  cmp --silent "$HOST_TOOL_POLICY_SOURCE" "$PUBLISHER_SUDOERS" || return 1
  cmp --silent "$HOST_TOOL_SOURCE_CONTAINER_TEMPLATE" "$CONTAINER_TEMPLATE" || return 1
  cmp --silent "$HOST_TOOL_RENDERED_QUADLET_SOURCE" "$RENDERED_QUADLET" || return 1
  cmp --silent "$HOST_TOOL_RENDERED_CADDY_SOURCE" "$CADDYFILE" || return 1
  cmp --silent "$HOST_TOOL_MARKER_SOURCE" "$HOST_TOOLS_MARKER" || return 1
  validate_target_launcher_state || return 1
  visudo -cf "$PUBLISHER_SUDOERS" >/dev/null || return 1
  validate_installed_host
}

drain_host_tool_processes() {
  (( $# > 0 && $# % 2 == 0 )) || return 1
  python3 - "$@" <<'PY'
import hashlib, os, pathlib, select, signal, stat, sys

arguments = sys.argv[1:]
targets = {arguments[index]: arguments[index + 1]
           for index in range(0, len(arguments), 2)}
if any(not pathlib.PurePosixPath(path).is_absolute()
       or len(digest) != 64
       or any(character not in "0123456789abcdef" for character in digest)
       for path, digest in targets.items()):
    raise SystemExit(1)

def matching_scripts(proc):
    matches = set()
    for fd in (proc / "fd").iterdir():
        try:
            metadata = fd.stat()
            if not stat.S_ISREG(metadata.st_mode):
                continue
            digest = hashlib.sha256(fd.read_bytes()).hexdigest()
        except (FileNotFoundError, PermissionError, OSError):
            continue
        matches.update(path for path, expected in targets.items() if digest == expected)
    return matches

self_pid = os.getpid()
candidates = []
for proc in pathlib.Path("/proc").iterdir():
    if not proc.name.isdigit() or int(proc.name) == self_pid:
        continue
    pid = int(proc.name)
    try:
        status = (proc / "status").read_text().splitlines()
        uid = next(line for line in status if line.startswith("Uid:")).split()[1:]
        argv = (proc / "cmdline").read_bytes().split(b"\0")
        executable = pathlib.PurePosixPath(os.fsdecode(argv[0])).name if argv else ""
        script = os.fsdecode(argv[1]) if len(argv) > 1 else ""
        if uid != ["0", "0", "0", "0"] \
                or executable not in {"bash", "bash.static"} \
                or script not in targets:
            continue
        matched = matching_scripts(proc)
        if script not in matched:
            continue
        pidfd = os.pidfd_open(pid)
        signal.pidfd_send_signal(pidfd, signal.SIGKILL)
        candidates.append(pidfd)
    except FileNotFoundError:
        continue

poller = select.poll()
for pidfd in candidates:
    poller.register(pidfd, select.POLLIN)
remaining = set(candidates)
while remaining:
    for pidfd, _ in poller.poll():
        remaining.discard(pidfd)
for pidfd in candidates:
    os.close(pidfd)

for proc in pathlib.Path("/proc").iterdir():
    if not proc.name.isdigit():
        continue
    try:
        status = (proc / "status").read_text().splitlines()
        uid = next(line for line in status if line.startswith("Uid:")).split()[1:]
        argv = (proc / "cmdline").read_bytes().split(b"\0")
        executable = pathlib.PurePosixPath(os.fsdecode(argv[0])).name if argv else ""
        script = os.fsdecode(argv[1]) if len(argv) > 1 else ""
        if uid == ["0", "0", "0", "0"] \
                and executable in {"bash", "bash.static"} \
                and script in targets \
                and script in matching_scripts(proc):
            raise RuntimeError("retired host-tool bytes remain open")
    except FileNotFoundError:
        continue
PY
}

drain_saved_host_tool_processes() {
  local transaction_path="$1"
  local -a targets=()
  if [[ -e "$transaction_path/launcher-was-present" ]]; then
    targets+=("$HOST_TOOL_LAUNCHER" \
      "$(sha256sum "$transaction_path/host-tool-launcher" | awk '{print $1}')")
  fi
  targets+=("$CONFIGURE_AUTH" \
    "$(sha256sum "$transaction_path/configure-auth" | awk '{print $1}')")
  targets+=("$UPDATE_IMAGE" \
    "$(sha256sum "$transaction_path/update-image" | awk '{print $1}')")
  drain_host_tool_processes "${targets[@]}"
}

drain_target_launcher_processes() {
  local target_sha
  target_sha="$(sha256sum "$HOST_TOOL_LAUNCHER_SOURCE" | awk '{print $1}')"
  drain_host_tool_processes \
    "$HOST_TOOL_LAUNCHER" "$target_sha" \
    "$CONFIGURE_AUTH" "$target_sha" \
    "$UPDATE_IMAGE" "$target_sha"
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

close_public_ingress() {
  local port state enabled activity comment
  systemctl disable --now caddy.service >/dev/null 2>&1 || {
    echo 'could not disable and stop Caddy' >&2
    return 1
  }
  for port in 80 443; do
    state="$(ufw_rule_state "$port")" || return 1
    if [[ "$state" = present ]]; then
      case "$port" in
        80) comment='Caddy ACME HTTP' ;;
        443) comment='Australian Legal MCP HTTPS' ;;
        *) return 1 ;;
      esac
      ufw --force delete allow "$port/tcp" comment "$comment" >/dev/null 2>&1 || {
        echo "could not remove UFW public rule for port $port" >&2
        return 1
      }
    fi
  done
  enabled="$(read_systemctl_enablement caddy.service)" || return 1
  activity="$(read_systemctl_activity caddy.service)" || return 1
  [[ "$enabled" = disabled && "$activity" = inactive ]] || {
    echo 'could not prove Caddy disabled and inactive' >&2
    return 1
  }
  for port in 80 443; do
    state="$(ufw_rule_state "$port")" || return 1
    [[ "$state" = absent ]] || {
      echo "could not prove UFW port $port closed" >&2
      return 1
    }
  done
}

ufw_is_ssh_only() {
  local report admin_source status
  require_regular_file /etc/legal-mcp/admin-source-ip root root 600 || return 1
  admin_source="$(</etc/legal-mcp/admin-source-ip)"
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

host_tool_foreign_transactions_are_absent() {
  local path found
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
    /etc/legal-mcp/.image-transaction.flat-int8-preparing \
    /etc/legal-mcp/.image-transaction.flat-int8-preparing-retired \
    /etc/legal-mcp/.image-transaction \
    /etc/legal-mcp/.image-transaction.retiring \
    /etc/legal-mcp/.image-transaction.retired; do
    path_is_absent "$path" || return 1
  done
  found="$(find /etc/legal-mcp -mindepth 1 -maxdepth 1 \
    -name '.auth-transaction.preparing.*' -print -quit)" || return 1
  [[ -z "$found" ]]
}

host_tool_upgrade_transaction_states_are_absent() {
  local path
  for path in \
    "$HOST_TOOLS_BUILDING" "$HOST_TOOLS_BUILDING_RETIRED" \
    "$HOST_TOOLS_PREPARING" "$HOST_TOOLS_PREPARING_RETIRED" \
    "$HOST_TOOLS_TRANSACTION" "$HOST_TOOLS_RETIRING" "$HOST_TOOLS_RETIRED" \
    "$HOST_TOOLS_ROLLBACK_RETIRING" "$HOST_TOOLS_ROLLBACK_RETIRED" \
    "$HOST_TOOLS_PUBLISHER_RESTORE" "$HOST_TOOLS_PUBLISHER_RESTORE_RETIRED"; do
    path_is_absent "$path" || return 1
  done
}

validate_configured_auth_for_host_tool_upgrade() {
  local mode
  local -a modes
  require_regular_file /etc/legal-mcp/runtime.env root root 600 || return 1
  require_regular_file /etc/legal-mcp/api-keys.json legal-mcp legal-mcp 400 || return 1
  require_regular_file "$CADDYFILE" root caddy 640 || return 1
  mapfile -t modes < <(
    awk -F= '$1 == "LEGAL_MCP_HTTP_AUTH" {print $2}' /etc/legal-mcp/runtime.env
  )
  [[ ${#modes[@]} -eq 1 \
    && ( "${modes[0]}" = api-key || "${modes[0]}" = entra \
      || "${modes[0]}" = entra+api-key ) ]] || return 1
  mode="${modes[0]}"
  python3 - /etc/legal-mcp/api-keys.json "$mode" <<'PY' || return 1
import json, pathlib, re, sys
value = json.loads(pathlib.Path(sys.argv[1]).read_bytes())
if not isinstance(value, dict) or set(value) != {"keys", "version"} or value["version"] != 1:
    raise SystemExit(1)
keys = value["keys"]
if not isinstance(keys, list) or len(keys) > 32 or ("api-key" in sys.argv[2] and not keys):
    raise SystemExit(1)
for item in keys:
    if not isinstance(item, dict) or set(item) != {"id", "sha256"}:
        raise SystemExit(1)
    if not isinstance(item["id"], str) or not re.fullmatch(r"[A-Za-z0-9._-]{1,64}", item["id"]):
        raise SystemExit(1)
    if not isinstance(item["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", item["sha256"]):
        raise SystemExit(1)
if len({item["id"] for item in keys}) != len(keys):
    raise SystemExit(1)
PY
  host_tool_foreign_transactions_are_absent
}

require_host_tool_auth_ready_marker() {
  require_empty_regular_file "$AUTH_READY_MARKER" root root 444 || return 1
  require_exact_acl "$AUTH_READY_MARKER" $'user::r--\ngroup::r--\nother::r--'
}

remove_host_tool_auth_ready() {
  path_is_absent "$AUTH_READY_MARKER" && return 0
  require_host_tool_auth_ready_marker || return 1
  rm -f -- "$AUTH_READY_MARKER"
  sync -f /etc/legal-mcp
  path_is_absent "$AUTH_READY_MARKER"
}

transition_configured_host_to_dark() {
  host_tool_upgrade_transaction_states_are_absent || {
    echo 'host-tool transaction recovery is required before a public dark transition' >&2
    return 1
  }
  validate_configured_auth_for_host_tool_upgrade || {
    echo 'public host authentication state is not exact enough for a dark transition' >&2
    return 1
  }
  close_public_ingress || return 1
  if [[ "$(read_systemctl_activity legal-mcp.service)" = active ]]; then
    systemctl stop legal-mcp.service >/dev/null 2>&1 || return 1
  fi
  remove_host_tool_auth_ready || return 1
  HOST_TOOLS_ACCEPT_CONFIGURED_DARK=true
  HOST_TOOLS_FROM_PUBLIC=false
}

services_and_ingress_are_off() {
  local service_enabled service_activity caddy_enabled caddy_activity
  local invalid=false listeners web_listener
  service_enabled="$(read_systemctl_enablement legal-mcp.service)" || return 1
  service_activity="$(read_systemctl_activity legal-mcp.service)" || return 1
  caddy_enabled="$(read_systemctl_enablement caddy.service)" || return 1
  caddy_activity="$(read_systemctl_activity caddy.service)" || return 1
  if [[ "$service_enabled" != generated ]]; then
    echo 'legal-mcp.service must be generated for the host-tool upgrade' >&2
    invalid=true
  fi
  if [[ "$service_activity" != inactive ]]; then
    echo 'legal-mcp.service must be inactive for the host-tool upgrade' >&2
    invalid=true
  fi
  if [[ "$caddy_enabled" != disabled ]]; then
    echo 'caddy.service must be disabled for the host-tool upgrade' >&2
    invalid=true
  fi
  if [[ "$caddy_activity" != inactive ]]; then
    echo 'caddy.service must be inactive for the host-tool upgrade' >&2
    invalid=true
  fi
  if [[ "$invalid" = true ]]; then
    if [[ "$HOST_TOOLS_FROM_PUBLIC" = true ]]; then
      transition_configured_host_to_dark || return 1
      services_and_ingress_are_off
      return
    fi
    if ! path_is_absent "$AUTH_READY_MARKER"; then
      require_host_tool_auth_ready_marker || return 1
      echo 'public host-tool upgrade requires explicit --from-public authorization' >&2
      return 1
    fi
    close_public_ingress || return 1
    if [[ "$service_activity" = active ]]; then
      systemctl stop legal-mcp.service >/dev/null 2>&1 || return 1
      service_activity="$(read_systemctl_activity legal-mcp.service)" || return 1
      [[ "$service_activity" = inactive ]] || return 1
    fi
    return 1
  fi
  ufw_is_ssh_only || {
    if [[ "$HOST_TOOLS_FROM_PUBLIC" = true ]]; then
      transition_configured_host_to_dark || return 1
      services_and_ingress_are_off
      return
    fi
    if ! path_is_absent "$AUTH_READY_MARKER"; then
      require_host_tool_auth_ready_marker || return 1
      echo 'public host-tool upgrade requires explicit --from-public authorization' >&2
      return 1
    fi
    close_public_ingress || return 1
    echo 'host-tool upgrade requires the exact SSH-only UFW allowlist' >&2
    return 1
  }
  listeners="$(ss --listening --tcp --numeric --no-header)" || {
    echo 'could not inspect host listening sockets' >&2
    return 1
  }
  web_listener="$(awk '$4 ~ /:(80|443|51235)$/ { print "present"; exit }' \
    <<< "$listeners")" || {
    echo 'could not evaluate host listening sockets' >&2
    return 1
  }
  if [[ -n "$web_listener" ]]; then
    if [[ "$HOST_TOOLS_FROM_PUBLIC" = true ]]; then
      transition_configured_host_to_dark || return 1
      services_and_ingress_are_off
      return
    fi
    if ! path_is_absent "$AUTH_READY_MARKER"; then
      require_host_tool_auth_ready_marker || return 1
      echo 'public host-tool upgrade requires explicit --from-public authorization' >&2
      return 1
    fi
    echo 'host-tool upgrade requires ports 80, 443, and 51235 not to be listening' >&2
    return 1
  fi
  if validate_configured_auth_for_host_tool_upgrade; then
    # --from-public is authority only for the transition. Once the exact
    # configured-dark matrix is durable, upgrade and recovery must be able to
    # consume it after SIGKILL or reboot without reopening ingress.
    HOST_TOOLS_ACCEPT_CONFIGURED_DARK=true
  fi
}

validate_installed_host() {
  local source fstype options xfs_details actual_uuid host_uuid volume_uuid directory image rendered path
  local publisher_key_re
  local -a host_marker volume_marker journal authorization entries
  require_regular_file /etc/legal-mcp/host-installed root root 444
  require_exact_acl /etc/legal-mcp/host-installed $'user::r--\ngroup::r--\nother::r--'
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
    echo 'installed corpus mount contract is invalid' >&2
    return 1
  }
  xfs_details="$(xfs_info /srv/legal-mcp)"
  if ! grep -Eq 'reflink=1([[:space:]]|$)' <<< "$xfs_details" \
    || ! grep -Eq 'ftype=1([[:space:]]|$)' <<< "$xfs_details"; then
    echo 'installed corpus volume lacks required XFS features' >&2
    return 1
  fi
  require_regular_file /srv/legal-mcp/.legal-mcp-volume root root 444
  require_exact_acl /srv/legal-mcp/.legal-mcp-volume $'user::r--\ngroup::r--\nother::r--'
  mapfile -t volume_marker < /srv/legal-mcp/.legal-mcp-volume
  [[ ${#volume_marker[@]} -eq 2 && "${volume_marker[0]}" = LEGAL_MCP_VOLUME_V1 \
    && "${volume_marker[1]}" =~ ^UUID=([0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12})$ ]] || {
    echo 'corpus volume marker is malformed' >&2
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
  require_exact_acl /srv/legal-mcp $'user::rwx\nuser:973:--x\ngroup::r-x\nmask::r-x\nother::---'
  for directory in generations lifecycle state uploads; do
    [[ -d "/srv/legal-mcp/$directory" && ! -L "/srv/legal-mcp/$directory" ]] || {
      echo "installed host directory is missing or unsafe: $directory" >&2
      return 1
    }
  done
  [[ "$(stat -c '%U:%G:%a' /srv/legal-mcp/generations)" = root:legal-mcp:750 ]]
  require_exact_acl /srv/legal-mcp/generations $'user::rwx\ngroup::r-x\nother::---'
  [[ "$(stat -c '%U:%G:%a' /srv/legal-mcp/lifecycle)" = root:legal-mcp:750 ]]
  require_exact_acl /srv/legal-mcp/lifecycle $'user::rwx\ngroup::r-x\nother::---'
  [[ "$(stat -c '%U:%G:%a' /srv/legal-mcp/state)" = legal-mcp:legal-mcp:700 ]]
  require_exact_acl /srv/legal-mcp/state $'user::rwx\ngroup::---\nother::---'
  [[ "$(stat -c '%U:%G:%a' /srv/legal-mcp/uploads)" = legal-mcp-publisher:legal-mcp-publisher:700 ]]
  require_exact_acl /srv/legal-mcp/uploads $'user::rwx\ngroup::---\nother::---'
  require_regular_file /srv/legal-mcp/lifecycle/LOCK root legal-mcp 640
  require_exact_acl /srv/legal-mcp/lifecycle/LOCK $'user::rw-\ngroup::r--\nother::---'
  require_empty_regular_file /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK root root 640
  require_exact_acl /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK $'user::rw-\ngroup::r--\nother::---'

  [[ "$(id -u legal-mcp):$(id -g legal-mcp):$(id -G legal-mcp)" = 971:971:971 \
    && "$(id -u legal-mcp-publisher):$(id -g legal-mcp-publisher):$(id -G legal-mcp-publisher)" = 973:973:973 \
    && "$(id -u legal-mcp-admin):$(id -g legal-mcp-admin):$(id -G legal-mcp-admin)" = 974:974:974 ]] || {
    echo 'installed fixed host identities are invalid' >&2
    return 1
  }
  require_regular_file "$HOST_DEPLOY" root root 755
  require_regular_file "$PUBLISHER_COMMAND" root root 755
  require_regular_file "$CONFIGURE_AUTH" root root 755
  require_regular_file "$UPDATE_IMAGE" root root 755
  classify_installed_auth_image_entrypoints || return 1
  require_regular_file "$PUBLISHER_SUDOERS" root root 440
  visudo -cf "$PUBLISHER_SUDOERS" >/dev/null
  require_regular_file /etc/legal-mcp/image root root 600
  require_regular_file /etc/legal-mcp/api-keys.json legal-mcp legal-mcp 400
  require_regular_file /etc/containers/systemd/legal-mcp.container root root 644
  require_regular_file "$CONTAINER_TEMPLATE" root root 644
  require_regular_file /etc/caddy/Caddyfile root caddy 640
  require_exact_directory /etc/legal-mcp root root 755
  require_exact_acl /etc/legal-mcp $'user::rwx\ngroup::r-x\nother::r-x'
  require_safe_directory /etc/containers/systemd root root
  require_safe_directory /usr/local/libexec/legal-mcp root root
  require_safe_directory /usr/local/sbin root root
  require_safe_directory /etc/sudoers.d root root
  require_safe_directory /etc/caddy root root
  require_safe_directory /var/lib/legal-mcp-publisher/.ssh root legal-mcp-publisher
  require_regular_file /var/lib/legal-mcp-publisher/.ssh/authorized_keys root legal-mcp-publisher 640
  publisher_key_re='^restrict,command="/usr/local/sbin/legal-mcp-publisher-command"[[:space:]]ssh-(ed25519|rsa)[[:space:]][A-Za-z0-9+/=]+([[:space:]][^[:cntrl:]]+)?$'
  mapfile -t entries < /var/lib/legal-mcp-publisher/.ssh/authorized_keys
  [[ ${#entries[@]} -eq 1 \
    && "${entries[0]}" =~ $publisher_key_re ]] || {
    echo 'installed publisher key is not bound to the exact forced command' >&2
    return 1
  }
  require_regular_file /etc/legal-mcp/runtime.env root root 600
  mapfile -t entries < <(awk -F= '$1 == "LEGAL_MCP_HTTP_AUTH" {print $2}' /etc/legal-mcp/runtime.env)
  [[ ${#entries[@]} -eq 1 ]] || return 1
  if [[ "${entries[0]}" = disabled ]]; then
    python3 - /etc/legal-mcp/api-keys.json <<'PY'
import json, pathlib, stat, sys
path = pathlib.Path(sys.argv[1])
meta = path.lstat()
if path.is_symlink() or not stat.S_ISREG(meta.st_mode) or meta.st_nlink != 1:
    raise SystemExit(1)
if json.loads(path.read_bytes()) != {"keys": [], "version": 1}:
    raise SystemExit(1)
PY
  elif [[ "$HOST_TOOLS_ACCEPT_CONFIGURED_DARK" = true \
    || "$HOST_TOOLS_FROM_PUBLIC" = true ]]; then
    validate_configured_auth_for_host_tool_upgrade || {
      echo 'configured-dark authentication is invalid for the authorized host-tool upgrade' >&2
      return 1
    }
    HOST_TOOLS_ACCEPT_CONFIGURED_DARK=true
  else
    echo 'host-tool upgrade requires disabled authentication or explicit --from-public authorization' >&2
    return 1
  fi
  for path in "$AUTH_READY_MARKER" "$HOST_TOOL_DISPATCH" \
    "${HOST_TOOL_DISPATCH}.retiring" "${HOST_TOOL_DISPATCH}.retired" \
    "$AUTH_CONFIGURING_PERMIT" "$CUTOVER_STARTING_PERMIT" \
    "$CUTOVER_START_ARM"; do
    path_is_absent "$path" || {
      echo 'host-tool upgrade requires no authentication-ready or launcher-dispatch state' >&2
      return 1
    }
  done
  mapfile -t entries < /etc/legal-mcp/image
  [[ ${#entries[@]} -eq 1 \
    && "${entries[0]}" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ ]] || {
    echo 'installed bootstrap image pin is malformed' >&2
    return 1
  }
  image="${entries[0]}"
  [[ "$(grep -o '__IMAGE_DIGEST__' "$CONTAINER_TEMPLATE" | wc -l)" = 1 ]] || {
    echo 'installed bootstrap Quadlet template is malformed' >&2
    return 1
  }
  rendered="$(mktemp /run/legal-mcp-host-tools-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$image|g" \
    "$CONTAINER_TEMPLATE" > "$rendered"
  if ! cmp --silent "$rendered" /etc/containers/systemd/legal-mcp.container; then
    rm -f "$rendered"
    echo 'installed bootstrap image, Quadlet, and template do not agree' >&2
    return 1
  fi
  rm -f "$rendered"
  if ! host_tool_foreign_transactions_are_absent; then
    echo 'auth or image transaction must be recovered before host-tool upgrade' >&2
    return 1
  fi

  if path_is_absent /srv/legal-mcp/lifecycle/active-generation; then
    HOST_TOOL_HOST_STATE=prepared
    if ! directory_is_empty /srv/legal-mcp/generations \
      || ! directory_is_empty /srv/legal-mcp/state; then
      echo 'bootstrap generations and state directories must be empty' >&2
      return 1
    fi
    require_regular_file /srv/legal-mcp/lifecycle/.deployment-transaction root root 600
    mapfile -t journal < /srv/legal-mcp/lifecycle/.deployment-transaction
    [[ ${#journal[@]} -eq 3 && "${journal[0]}" =~ ^[0-9a-f]{64}$ \
      && "${journal[1]}" = - && "${journal[2]}" = prepared ]] || {
      echo 'host-tool upgrade requires the pending bootstrap corpus transaction at prepared phase' >&2
      return 1
    }
    HOST_TOOL_PENDING_GENERATION="${journal[0]}"
    [[ -d "/srv/legal-mcp/uploads/$HOST_TOOL_PENDING_GENERATION" \
      && ! -L "/srv/legal-mcp/uploads/$HOST_TOOL_PENDING_GENERATION" \
      && "$(stat -c '%U:%G:%a' "/srv/legal-mcp/uploads/$HOST_TOOL_PENDING_GENERATION")" \
        = legal-mcp-publisher:legal-mcp-publisher:700 ]] || {
      echo 'prepared bootstrap upload is missing or unsafe' >&2
      return 1
    }
    require_exact_acl "/srv/legal-mcp/uploads/$HOST_TOOL_PENDING_GENERATION" \
      $'user::rwx\ngroup::---\nother::---'
    mapfile -t entries < <(
      findmnt --submounts --noheadings --raw --output TARGET \
        --target "/srv/legal-mcp/uploads/$HOST_TOOL_PENDING_GENERATION"
    )
    [[ ${#entries[@]} -eq 1 && "${entries[0]}" = /srv/legal-mcp ]] || {
      echo 'prepared bootstrap upload contains an unexpected mount' >&2
      return 1
    }
    directory_contains_only /srv/legal-mcp/uploads "$HOST_TOOL_PENDING_GENERATION" || {
      echo 'uploads contain state outside the one prepared bootstrap generation' >&2
      return 1
    }
    # Activation durably revokes rsync before validation. A recovered prepared
    # transaction may therefore have no authorization; absence is the closed
    # state and the upgrade must preserve it.
    if ! path_is_absent /run/legal-mcp/authorized-upload; then
      require_regular_file /run/legal-mcp/authorized-upload root legal-mcp-publisher 440
      mapfile -t authorization < /run/legal-mcp/authorized-upload
      [[ ${#authorization[@]} -eq 1 \
        && "${authorization[0]}" = "$HOST_TOOL_PENDING_GENERATION" ]] || {
        echo 'prepared upload authorization does not match its transaction' >&2
        return 1
      }
    fi
    directory_contains_only /srv/legal-mcp/lifecycle \
      .deployment-transaction LIFECYCLE_LOCK LOCK || {
      echo 'lifecycle contains unexpected bootstrap state' >&2
      return 1
    }
  else
    HOST_TOOL_HOST_STATE=activated
    local active_generation
    require_regular_file /srv/legal-mcp/lifecycle/active-generation root root 644 || return 1
    require_exact_acl /srv/legal-mcp/lifecycle/active-generation \
      $'user::rw-\ngroup::r--\nother::r--' || return 1
    [[ "$(stat -c '%s' /srv/legal-mcp/lifecycle/active-generation)" = 64 ]] || {
      echo 'active generation pointer must contain exactly 64 bytes' >&2
      return 1
    }
    active_generation="$(</srv/legal-mcp/lifecycle/active-generation)"
    [[ "$active_generation" =~ ^[0-9a-f]{64}$ \
      && -d "/srv/legal-mcp/generations/$active_generation" \
      && ! -L "/srv/legal-mcp/generations/$active_generation" ]] || {
      echo 'active generation pointer is invalid' >&2
      return 1
    }
    path_is_absent /srv/legal-mcp/lifecycle/.deployment-transaction.preparing || {
      echo 'corpus transaction preparation must be recovered before host-tool upgrade' >&2
      return 1
    }
    if path_is_absent /srv/legal-mcp/lifecycle/.deployment-transaction; then
      path_is_absent /run/legal-mcp/authorized-upload || {
        echo 'activated host must not retain upload authorization' >&2
        return 1
      }
      directory_is_empty /srv/legal-mcp/uploads || {
        echo 'activated host uploads must be empty before host-tool upgrade' >&2
        return 1
      }
      directory_contains_only /srv/legal-mcp/lifecycle \
        active-generation LIFECYCLE_LOCK LOCK || {
        echo 'activated lifecycle contains unexpected state' >&2
        return 1
      }
    else
      require_regular_file /srv/legal-mcp/lifecycle/.deployment-transaction root root 600
      mapfile -t journal < /srv/legal-mcp/lifecycle/.deployment-transaction
      [[ ${#journal[@]} -eq 3 && "${journal[0]}" =~ ^[0-9a-f]{64}$ \
        && "${journal[1]}" = "$active_generation" \
        && "${journal[2]}" = prepared ]] || {
        echo 'active host corpus transaction is not one ordinary prepared generation' >&2
        return 1
      }
      HOST_TOOL_PENDING_GENERATION="${journal[0]}"
      [[ "$HOST_TOOL_PENDING_GENERATION" != "$active_generation" \
        && -d "/srv/legal-mcp/uploads/$HOST_TOOL_PENDING_GENERATION" \
        && ! -L "/srv/legal-mcp/uploads/$HOST_TOOL_PENDING_GENERATION" \
        && "$(stat -c '%U:%G:%a' "/srv/legal-mcp/uploads/$HOST_TOOL_PENDING_GENERATION")" \
          = legal-mcp-publisher:legal-mcp-publisher:700 ]] || {
        echo 'active host prepared upload is missing or unsafe' >&2
        return 1
      }
      require_exact_acl "/srv/legal-mcp/uploads/$HOST_TOOL_PENDING_GENERATION" \
        $'user::rwx\ngroup::---\nother::---'
      mapfile -t entries < <(
        findmnt --submounts --noheadings --raw --output TARGET \
          --target "/srv/legal-mcp/uploads/$HOST_TOOL_PENDING_GENERATION"
      )
      [[ ${#entries[@]} -eq 1 && "${entries[0]}" = /srv/legal-mcp ]] || {
        echo 'active host prepared upload contains an unexpected mount' >&2
        return 1
      }
      directory_contains_only /srv/legal-mcp/uploads "$HOST_TOOL_PENDING_GENERATION" || {
        echo 'uploads contain state outside the active host prepared generation' >&2
        return 1
      }
      require_regular_file /run/legal-mcp/authorized-upload \
        root legal-mcp-publisher 440
      mapfile -t authorization < /run/legal-mcp/authorized-upload
      [[ ${#authorization[@]} -eq 1 \
        && "${authorization[0]}" = "$HOST_TOOL_PENDING_GENERATION" ]] || {
        echo 'active host upload authorization does not match its prepared transaction' >&2
        return 1
      }
      directory_contains_only /srv/legal-mcp/lifecycle \
        .deployment-transaction active-generation LIFECYCLE_LOCK LOCK || {
        echo 'active prepared lifecycle contains unexpected state' >&2
        return 1
      }
    fi
  fi
  if [[ "$HOST_TOOL_ENTRYPOINT_STATE" = legacy ]]; then
    validate_legacy_v0192_host_tool_identity || return 1
  fi
  services_and_ingress_are_off
}

atomic_install_file() {
  local source="$1" destination="$2" owner="$3" group="$4" mode="$5" temporary
  temporary="$(mktemp "$(dirname "$destination")/.$(basename "$destination").XXXXXX")"
  install -o "$owner" -g "$group" -m "$mode" "$source" "$temporary"
  sync -f "$temporary"
  mv -fT "$temporary" "$destination"
  sync -f "$(dirname "$destination")"
}

delete_retired_host_tools_directory() {
  local path="$1"
  path_is_absent "$path" && return 0
  require_exact_directory "$path" root root 700
  rm -rf --one-file-system -- "$path"
  path_is_absent "$path" || {
    echo "host-tool transaction cleanup did not complete: $path" >&2
    return 1
  }
  sync -f /etc/legal-mcp
}

retire_host_tools_directory_for_deletion() {
  local path="$1" retired_path="$2"
  path_is_absent "$retired_path" || {
    echo "host-tool deletion retirement already exists: $retired_path" >&2
    return 1
  }
  require_exact_directory "$path" root root 700
  mv -T "$path" "$retired_path"
  sync -f /etc/legal-mcp
  delete_retired_host_tools_directory "$retired_path"
}

reconcile_host_tools_building() {
  if ! path_is_absent "$HOST_TOOLS_BUILDING" \
    && ! path_is_absent "$HOST_TOOLS_BUILDING_RETIRED"; then
    echo 'host-tool build has conflicting deletion states' >&2
    return 1
  fi
  if ! path_is_absent "$HOST_TOOLS_BUILDING"; then
    retire_host_tools_directory_for_deletion \
      "$HOST_TOOLS_BUILDING" "$HOST_TOOLS_BUILDING_RETIRED"
    HOST_TOOLS_PREPARATION_WAS_RECOVERED=true
  elif ! path_is_absent "$HOST_TOOLS_BUILDING_RETIRED"; then
    delete_retired_host_tools_directory "$HOST_TOOLS_BUILDING_RETIRED"
    HOST_TOOLS_PREPARATION_WAS_RECOVERED=true
  fi
}

finalize_host_tools_transaction_retirement() {
  local path pending_states=0
  if ! path_is_absent "$HOST_TOOLS_PREPARING_RETIRED"; then
    path_is_absent "$HOST_TOOLS_PREPARING" || {
      echo 'host-tool preparation has conflicting retirement states' >&2
      return 1
    }
    HOST_TOOLS_RETIREMENT_WAS_PENDING=true
    delete_retired_host_tools_directory "$HOST_TOOLS_PREPARING_RETIRED"
  fi
  if ! path_is_absent "$HOST_TOOLS_PUBLISHER_RESTORE_RETIRED"; then
    path_is_absent "$HOST_TOOLS_PUBLISHER_RESTORE" || {
      echo 'host-tool publisher restore has conflicting retirement states' >&2
      return 1
    }
    HOST_TOOLS_RETIREMENT_WAS_PENDING=true
    delete_retired_host_tools_directory "$HOST_TOOLS_PUBLISHER_RESTORE_RETIRED"
  fi
  for path in "$HOST_TOOLS_RETIRING" "$HOST_TOOLS_RETIRED" \
    "$HOST_TOOLS_ROLLBACK_RETIRING" "$HOST_TOOLS_ROLLBACK_RETIRED" \
    "$HOST_TOOLS_PUBLISHER_RESTORE"; do
    if ! path_is_absent "$path"; then
      ((pending_states += 1))
    fi
  done
  if [[ $pending_states -gt 1 ]]; then
    echo 'host-tool transaction has conflicting retirement directories' >&2
    return 1
  fi
  if ! path_is_absent "$HOST_TOOLS_RETIRING"; then
    HOST_TOOLS_RETIREMENT_WAS_PENDING=true
    require_exact_directory "$HOST_TOOLS_RETIRING" root root 700
    validate_host_tools_transaction "$HOST_TOOLS_RETIRING"
    validate_live_target_host_tools
    # The first parent sync makes removal of the canonical transaction name
    # durable. Only then is the directory moved to a deletion-only name.
    sync -f /etc/legal-mcp
    mv -T "$HOST_TOOLS_RETIRING" "$HOST_TOOLS_RETIRED"
    sync -f /etc/legal-mcp
  fi
  if ! path_is_absent "$HOST_TOOLS_RETIRED"; then
    HOST_TOOLS_RETIREMENT_WAS_PENDING=true
    validate_live_target_host_tools
    delete_retired_host_tools_directory "$HOST_TOOLS_RETIRED"
  fi
  if ! path_is_absent "$HOST_TOOLS_ROLLBACK_RETIRING"; then
    HOST_TOOLS_RETIREMENT_WAS_PENDING=true
    require_exact_directory "$HOST_TOOLS_ROLLBACK_RETIRING" root root 700
    sync -f /etc/legal-mcp
    mv -T "$HOST_TOOLS_ROLLBACK_RETIRING" "$HOST_TOOLS_ROLLBACK_RETIRED"
    sync -f /etc/legal-mcp
  fi
  if ! path_is_absent "$HOST_TOOLS_ROLLBACK_RETIRED"; then
    HOST_TOOLS_RETIREMENT_WAS_PENDING=true
    complete_host_tools_rollback_retirement
  elif ! path_is_absent "$HOST_TOOLS_PUBLISHER_RESTORE"; then
    HOST_TOOLS_RETIREMENT_WAS_PENDING=true
    complete_host_tools_publisher_restore
  fi
}

retire_host_tools_transaction() {
  if ! path_is_absent "$HOST_TOOLS_RETIRING" \
    || ! path_is_absent "$HOST_TOOLS_RETIRED" \
    || ! path_is_absent "$HOST_TOOLS_ROLLBACK_RETIRING" \
    || ! path_is_absent "$HOST_TOOLS_ROLLBACK_RETIRED" \
    || ! path_is_absent "$HOST_TOOLS_PUBLISHER_RESTORE"; then
    echo 'host-tool transaction retirement state is not clean' >&2
    return 1
  fi
  validate_host_tools_transaction
  validate_live_target_host_tools
  mv -T "$HOST_TOOLS_TRANSACTION" "$HOST_TOOLS_RETIRING"
  sync -f /etc/legal-mcp
  mv -T "$HOST_TOOLS_RETIRING" "$HOST_TOOLS_RETIRED"
  sync -f /etc/legal-mcp
  delete_retired_host_tools_directory "$HOST_TOOLS_RETIRED"
}

retire_host_tools_rollback_transaction() {
  if ! path_is_absent "$HOST_TOOLS_RETIRING" \
    || ! path_is_absent "$HOST_TOOLS_RETIRED" \
    || ! path_is_absent "$HOST_TOOLS_ROLLBACK_RETIRING" \
    || ! path_is_absent "$HOST_TOOLS_ROLLBACK_RETIRED" \
    || ! path_is_absent "$HOST_TOOLS_PUBLISHER_RESTORE"; then
    echo 'host-tool rollback retirement state is not clean' >&2
    return 1
  fi
  mv -T "$HOST_TOOLS_TRANSACTION" "$HOST_TOOLS_ROLLBACK_RETIRING"
  sync -f /etc/legal-mcp
  mv -T "$HOST_TOOLS_ROLLBACK_RETIRING" "$HOST_TOOLS_ROLLBACK_RETIRED"
  sync -f /etc/legal-mcp
  complete_host_tools_rollback_retirement
}

render_host_tools_marker() {
  local sudoers_sha256="$1" destination="$2"
  cat > "$destination" <<EOF
LEGAL_MCP_HOST_TOOLS_V2
VERSION=$HOST_TOOL_VERSION
SOURCE_COMMIT=$HOST_TOOL_REVISION
HOST_DEPLOY_SHA256=$HOST_DEPLOY_SHA256
PUBLISHER_COMMAND_SHA256=$PUBLISHER_COMMAND_SHA256
CONFIGURE_AUTH_SHA256=$CONFIGURE_AUTH_SHA256
UPDATE_IMAGE_SHA256=$UPDATE_IMAGE_SHA256
CONTAINER_TEMPLATE_SHA256=$CONTAINER_TEMPLATE_SHA256
SUDOERS_SHA256=$sudoers_sha256
EOF
  chmod 444 "$destination"
}

host_tool_file_sha256() {
  if [[ "$1" = - ]]; then
    printf '%s\n' -
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

render_host_tools_hash_manifest() {
  local host_deploy="$1" publisher="$2" configure_auth="$3" update_image="$4"
  local container_template="$5" rendered_quadlet="$6" sudoers="$7" host_marker_file="$8"
  local launcher="$9" launcher_marker="${10}" configure_pointer="${11}"
  local update_pointer="${12}" caddyfile="${13}" destination="${14}"
  cat > "$destination" <<EOF
HOST_DEPLOY_SHA256=$(host_tool_file_sha256 "$host_deploy")
PUBLISHER_COMMAND_SHA256=$(host_tool_file_sha256 "$publisher")
CONFIGURE_AUTH_ENTRYPOINT_SHA256=$(host_tool_file_sha256 "$configure_auth")
UPDATE_IMAGE_ENTRYPOINT_SHA256=$(host_tool_file_sha256 "$update_image")
CONTAINER_TEMPLATE_SHA256=$(host_tool_file_sha256 "$container_template")
RENDERED_QUADLET_SHA256=$(host_tool_file_sha256 "$rendered_quadlet")
SUDOERS_SHA256=$(host_tool_file_sha256 "$sudoers")
HOST_TOOLS_MARKER_SHA256=$(host_tool_file_sha256 "$host_marker_file")
LAUNCHER_SHA256=$(host_tool_file_sha256 "$launcher")
LAUNCHER_MARKER_SHA256=$(host_tool_file_sha256 "$launcher_marker")
CONFIGURE_AUTH_POINTER_SHA256=$(host_tool_file_sha256 "$configure_pointer")
UPDATE_IMAGE_POINTER_SHA256=$(host_tool_file_sha256 "$update_pointer")
CADDYFILE_SHA256=$(host_tool_file_sha256 "$caddyfile")
EOF
  chmod 600 "$destination"
}

validate_saved_host_tools_hashes() {
  local transaction_path="$1" saved_marker=- launcher=- launcher_marker=-
  local configure_pointer=- update_pointer=- manifest
  if [[ -e "$transaction_path/marker-was-present" ]]; then
    saved_marker="$transaction_path/host-tools-marker"
  fi
  if [[ -e "$transaction_path/launcher-was-present" ]]; then
    launcher="$transaction_path/host-tool-launcher"
    launcher_marker="$transaction_path/launcher-marker"
    configure_pointer="$transaction_path/configure-auth-pointer"
    update_pointer="$transaction_path/update-image-pointer"
  fi
  manifest="$(mktemp /run/legal-mcp-host-tools-previous.XXXXXX)"
  render_host_tools_hash_manifest \
    "$transaction_path/host-deploy" "$transaction_path/publisher-command" \
    "$transaction_path/configure-auth" "$transaction_path/update-image" \
    "$transaction_path/container-template" "$transaction_path/rendered-quadlet" \
    "$transaction_path/publisher-sudoers" "$saved_marker" "$launcher" \
    "$launcher_marker" "$configure_pointer" "$update_pointer" \
    "$transaction_path/Caddyfile" "$manifest"
  if ! cmp --silent "$manifest" "$transaction_path/previous-sha256"; then
    rm -f "$manifest"
    echo 'saved host-tool hashes do not match the rollback files' >&2
    return 1
  fi
  rm -f "$manifest"
}

validate_host_tools_transaction() {
  local transaction_path="${1:-$HOST_TOOLS_TRANSACTION}"
  local -a kind version revision
  [[ -d "$transaction_path" && ! -L "$transaction_path" \
    && "$(stat -c '%U:%G:%a' "$transaction_path")" = root:root:700 ]] || {
    echo 'host-tool transaction is missing or unsafe' >&2
    return 1
  }
  for name in kind target-version target-revision target-sha256 previous-sha256 \
    host-deploy publisher-command configure-auth update-image container-template \
    rendered-quadlet Caddyfile publisher-sudoers host-tool-launcher-device-inode \
    configure-auth-device-inode update-image-device-inode; do
    require_regular_file "$transaction_path/$name" root root 600 || return 1
  done
  mapfile -t kind < "$transaction_path/kind"
  mapfile -t version < "$transaction_path/target-version"
  mapfile -t revision < "$transaction_path/target-revision"
  [[ ${#kind[@]} -eq 1 && "${kind[0]}" = LEGAL_MCP_HOST_TOOLS_TRANSACTION_V2 \
    && ${#version[@]} -eq 1 && "${version[0]}" = "$HOST_TOOL_VERSION" \
    && ${#revision[@]} -eq 1 && "${revision[0]}" = "$HOST_TOOL_REVISION" ]] || {
    echo 'host-tool transaction identity is invalid for this release' >&2
    return 1
  }
  local -a expected_entries=(
    configure-auth configure-auth-device-inode container-template host-deploy kind
    host-tool-launcher-device-inode
    previous-sha256 publisher-command publisher-sudoers rendered-quadlet Caddyfile
    target-revision target-sha256 target-version update-image update-image-device-inode
  )
  if [[ -e "$transaction_path/marker-was-present" ]]; then
    require_regular_file "$transaction_path/marker-was-present" root root 600
    require_regular_file "$transaction_path/host-tools-marker" root root 444
    expected_entries+=(host-tools-marker marker-was-present)
  else
    require_regular_file "$transaction_path/marker-was-absent" root root 600
    expected_entries+=(marker-was-absent)
  fi
  if [[ -e "$transaction_path/launcher-was-present" ]]; then
    require_regular_file "$transaction_path/launcher-was-present" root root 600
    require_regular_file "$transaction_path/host-tool-launcher" root root 600
    require_regular_file "$transaction_path/launcher-marker" root root 600
    require_regular_file "$transaction_path/configure-auth-pointer" root root 600
    require_regular_file "$transaction_path/update-image-pointer" root root 600
    expected_entries+=(launcher-was-present host-tool-launcher launcher-marker
      configure-auth-pointer update-image-pointer)
  else
    require_regular_file "$transaction_path/launcher-was-absent" root root 600
    expected_entries+=(launcher-was-absent)
  fi
  directory_contains_only "$transaction_path" "${expected_entries[@]}" || {
    echo 'host-tool transaction contains unexpected state' >&2
    return 1
  }
  local launcher_identity configure_identity update_identity
  launcher_identity="$(<"$transaction_path/host-tool-launcher-device-inode")"
  configure_identity="$(<"$transaction_path/configure-auth-device-inode")"
  update_identity="$(<"$transaction_path/update-image-device-inode")"
  if [[ -e "$transaction_path/launcher-was-present" ]]; then
    [[ "$launcher_identity" =~ ^[0-9]+:[0-9]+$ \
      && "$configure_identity" =~ ^[0-9]+:[0-9]+$ \
      && "$update_identity" =~ ^[0-9]+:[0-9]+$ ]] || return 1
  else
    [[ "$launcher_identity" = - \
      && "$configure_identity" =~ ^[0-9]+:[0-9]+$ \
      && "$update_identity" =~ ^[0-9]+:[0-9]+$ ]] || return 1
  fi
  cmp --silent "$HOST_TOOL_TARGET_MANIFEST" "$transaction_path/target-sha256" || {
    echo 'host-tool transaction target hashes do not match this release' >&2
    return 1
  }
  validate_saved_host_tools_hashes "$transaction_path" || return 1
  visudo -cf "$transaction_path/publisher-sudoers" >/dev/null
}

restore_saved_host_tools_marker() {
  local transaction_path="$1"
  if [[ -e "$transaction_path/marker-was-present" ]]; then
    atomic_install_file "$transaction_path/host-tools-marker" \
      "$HOST_TOOLS_MARKER" root root 444
  else
    rm -f -- "$HOST_TOOLS_MARKER"
    path_is_absent "$HOST_TOOLS_MARKER"
    sync -f /etc/legal-mcp
  fi
}

saved_host_tools_marker_is_restored() {
  local transaction_path="$1"
  if [[ -e "$transaction_path/marker-was-present" ]]; then
    require_regular_file "$HOST_TOOLS_MARKER" root root 444
    cmp --silent "$transaction_path/host-tools-marker" "$HOST_TOOLS_MARKER"
  else
    path_is_absent "$HOST_TOOLS_MARKER"
  fi
}

restore_saved_quadlet_state() {
  local transaction_path="$1"
  atomic_install_file "$transaction_path/container-template" \
    "$CONTAINER_TEMPLATE" root root 644
  atomic_install_file "$transaction_path/rendered-quadlet" \
    "$RENDERED_QUADLET" root root 644
  atomic_install_file "$transaction_path/Caddyfile" \
    "$CADDYFILE" root caddy 640
  systemctl daemon-reload
  cmp --silent "$transaction_path/container-template" "$CONTAINER_TEMPLATE"
  cmp --silent "$transaction_path/rendered-quadlet" "$RENDERED_QUADLET"
  cmp --silent "$transaction_path/Caddyfile" "$CADDYFILE"
}

restore_saved_host_tool_entrypoint() {
  local saved="$1" live="$2"
  require_regular_file "$live" root root 755
  if cmp --silent "$saved" "$live"; then
    return 0
  fi
  cmp --silent "$HOST_TOOL_LAUNCHER_SOURCE" "$live" || {
    echo "host-tool rollback found entrypoint bytes outside the saved/target set: $live" >&2
    return 1
  }
  atomic_install_file "$saved" "$live" root root 755
  cmp --silent "$saved" "$live"
}

restore_saved_auth_image_entrypoints() {
  local transaction_path="$1"
  if [[ -e "$transaction_path/launcher-was-present" ]]; then
    restore_saved_host_tool_entrypoint \
      "$transaction_path/host-tool-launcher" "$HOST_TOOL_LAUNCHER"
    restore_saved_host_tool_entrypoint \
      "$transaction_path/configure-auth" "$CONFIGURE_AUTH"
    restore_saved_host_tool_entrypoint \
      "$transaction_path/update-image" "$UPDATE_IMAGE"
    atomic_install_file "$transaction_path/launcher-marker" \
      "$HOST_TOOL_LAUNCHER_MARKER" root root 444
    atomic_install_file "$transaction_path/configure-auth-pointer" \
      "$CONFIGURE_AUTH_POINTER" root root 644
    atomic_install_file "$transaction_path/update-image-pointer" \
      "$UPDATE_IMAGE_POINTER" root root 644
    drain_target_launcher_processes
    validate_installed_launcher_state
  else
    rm -f -- "$HOST_TOOL_LAUNCHER_MARKER" "$CONFIGURE_AUTH_POINTER" \
      "$UPDATE_IMAGE_POINTER" "$HOST_TOOL_LAUNCHER"
    sync -f /etc/legal-mcp
    sync -f /usr/local/libexec/legal-mcp
    atomic_install_file "$transaction_path/configure-auth" \
      "$CONFIGURE_AUTH" root root 755
    atomic_install_file "$transaction_path/update-image" \
      "$UPDATE_IMAGE" root root 755
    drain_target_launcher_processes
  fi
}

saved_auth_image_state_is_restored() {
  local transaction_path="$1"
  cmp --silent "$transaction_path/configure-auth" "$CONFIGURE_AUTH"
  cmp --silent "$transaction_path/update-image" "$UPDATE_IMAGE"
  cmp --silent "$transaction_path/container-template" "$CONTAINER_TEMPLATE"
  cmp --silent "$transaction_path/rendered-quadlet" "$RENDERED_QUADLET"
  cmp --silent "$transaction_path/Caddyfile" "$CADDYFILE"
  if [[ -e "$transaction_path/launcher-was-present" ]]; then
    cmp --silent "$transaction_path/host-tool-launcher" "$HOST_TOOL_LAUNCHER"
    cmp --silent "$transaction_path/launcher-marker" "$HOST_TOOL_LAUNCHER_MARKER"
    cmp --silent "$transaction_path/configure-auth-pointer" "$CONFIGURE_AUTH_POINTER"
    cmp --silent "$transaction_path/update-image-pointer" "$UPDATE_IMAGE_POINTER"
  else
    path_is_absent "$HOST_TOOL_LAUNCHER"
    path_is_absent "$HOST_TOOL_LAUNCHER_MARKER"
    path_is_absent "$CONFIGURE_AUTH_POINTER"
    path_is_absent "$UPDATE_IMAGE_POINTER"
  fi
}

complete_host_tools_publisher_restore() {
  validate_host_tools_transaction "$HOST_TOOLS_PUBLISHER_RESTORE"
  require_regular_file "$HOST_DEPLOY" root root 755
  require_regular_file "$PUBLISHER_COMMAND" root root 755
  require_regular_file "$CONFIGURE_AUTH" root root 755
  require_regular_file "$UPDATE_IMAGE" root root 755
  require_regular_file "$CONTAINER_TEMPLATE" root root 644
  require_regular_file "$RENDERED_QUADLET" root root 644
  require_regular_file "$CADDYFILE" root caddy 640
  require_regular_file "$PUBLISHER_SUDOERS" root root 440
  cmp --silent "$HOST_TOOLS_PUBLISHER_RESTORE/host-deploy" "$HOST_DEPLOY"
  cmp --silent "$HOST_TOOLS_PUBLISHER_RESTORE/configure-auth" "$CONFIGURE_AUTH"
  cmp --silent "$HOST_TOOLS_PUBLISHER_RESTORE/update-image" "$UPDATE_IMAGE"
  cmp --silent "$HOST_TOOLS_PUBLISHER_RESTORE/container-template" "$CONTAINER_TEMPLATE"
  cmp --silent "$HOST_TOOLS_PUBLISHER_RESTORE/rendered-quadlet" "$RENDERED_QUADLET"
  cmp --silent "$HOST_TOOLS_PUBLISHER_RESTORE/Caddyfile" "$CADDYFILE"
  cmp --silent "$HOST_TOOLS_PUBLISHER_RESTORE/publisher-sudoers" "$PUBLISHER_SUDOERS"
  saved_auth_image_state_is_restored "$HOST_TOOLS_PUBLISHER_RESTORE"
  saved_host_tools_marker_is_restored "$HOST_TOOLS_PUBLISHER_RESTORE"
  visudo -cf "$PUBLISHER_SUDOERS" >/dev/null

  # The versioned wrapper is restored last. Until this atomic replacement, the
  # new wrapper rejects the publisher-restore sentinel. After it, every old
  # host-tool file and marker has already been restored durably.
  atomic_install_file "$HOST_TOOLS_PUBLISHER_RESTORE/publisher-command" \
    "$PUBLISHER_COMMAND" root root 755
  cmp --silent "$HOST_TOOLS_PUBLISHER_RESTORE/publisher-command" "$PUBLISHER_COMMAND"
  validate_installed_host
  retire_host_tools_directory_for_deletion \
    "$HOST_TOOLS_PUBLISHER_RESTORE" "$HOST_TOOLS_PUBLISHER_RESTORE_RETIRED"
}

complete_host_tools_rollback_retirement() {
  validate_host_tools_transaction "$HOST_TOOLS_ROLLBACK_RETIRED"
  require_regular_file "$PUBLISHER_COMMAND" root root 755
  cmp --silent "$HOST_TOOL_SOURCE_PUBLISHER" "$PUBLISHER_COMMAND"

  atomic_install_file "$HOST_TOOLS_ROLLBACK_RETIRED/host-deploy" \
    "$HOST_DEPLOY" root root 755
  restore_saved_auth_image_entrypoints "$HOST_TOOLS_ROLLBACK_RETIRED"
  restore_saved_quadlet_state "$HOST_TOOLS_ROLLBACK_RETIRED"
  atomic_install_file "$HOST_TOOLS_ROLLBACK_RETIRED/publisher-sudoers" \
    "$PUBLISHER_SUDOERS" root root 440
  restore_saved_host_tools_marker "$HOST_TOOLS_ROLLBACK_RETIRED"
  cmp --silent "$HOST_TOOLS_ROLLBACK_RETIRED/host-deploy" "$HOST_DEPLOY"
  saved_auth_image_state_is_restored "$HOST_TOOLS_ROLLBACK_RETIRED"
  cmp --silent "$HOST_TOOLS_ROLLBACK_RETIRED/publisher-sudoers" "$PUBLISHER_SUDOERS"
  saved_host_tools_marker_is_restored "$HOST_TOOLS_ROLLBACK_RETIRED"
  visudo -cf "$PUBLISHER_SUDOERS" >/dev/null

  mv -T "$HOST_TOOLS_ROLLBACK_RETIRED" "$HOST_TOOLS_PUBLISHER_RESTORE"
  sync -f /etc/legal-mcp
  complete_host_tools_publisher_restore
}

reconcile_host_tools_preparation() {
  path_is_absent "$HOST_TOOLS_PREPARING" && return 0
  path_is_absent "$HOST_TOOLS_TRANSACTION" || {
    echo 'host-tool transaction has conflicting preparation and canonical directories' >&2
    return 1
  }
  validate_host_tools_transaction "$HOST_TOOLS_PREPARING"
  require_regular_file "$HOST_DEPLOY" root root 755
  require_regular_file "$PUBLISHER_COMMAND" root root 755
  require_regular_file "$CONFIGURE_AUTH" root root 755
  require_regular_file "$UPDATE_IMAGE" root root 755
  require_regular_file "$CONTAINER_TEMPLATE" root root 644
  require_regular_file "$CADDYFILE" root caddy 640
  require_regular_file "$PUBLISHER_SUDOERS" root root 440
  cmp --silent "$HOST_TOOLS_PREPARING/host-deploy" "$HOST_DEPLOY"
  cmp --silent "$HOST_TOOLS_PREPARING/configure-auth" "$CONFIGURE_AUTH"
  cmp --silent "$HOST_TOOLS_PREPARING/update-image" "$UPDATE_IMAGE"
  cmp --silent "$HOST_TOOLS_PREPARING/container-template" "$CONTAINER_TEMPLATE"
  cmp --silent "$HOST_TOOLS_PREPARING/rendered-quadlet" "$RENDERED_QUADLET"
  cmp --silent "$HOST_TOOLS_PREPARING/Caddyfile" "$CADDYFILE"
  cmp --silent "$HOST_TOOLS_PREPARING/publisher-sudoers" "$PUBLISHER_SUDOERS"
  if [[ -e "$HOST_TOOLS_PREPARING/launcher-was-present" ]]; then
    require_regular_file "$HOST_TOOL_LAUNCHER" root root 755
    require_regular_file "$HOST_TOOL_LAUNCHER_MARKER" root root 444
    require_regular_file "$CONFIGURE_AUTH_POINTER" root root 644
    require_regular_file "$UPDATE_IMAGE_POINTER" root root 644
    cmp --silent "$HOST_TOOLS_PREPARING/host-tool-launcher" "$HOST_TOOL_LAUNCHER"
    cmp --silent "$HOST_TOOLS_PREPARING/launcher-marker" "$HOST_TOOL_LAUNCHER_MARKER"
    cmp --silent "$HOST_TOOLS_PREPARING/configure-auth-pointer" "$CONFIGURE_AUTH_POINTER"
    cmp --silent "$HOST_TOOLS_PREPARING/update-image-pointer" "$UPDATE_IMAGE_POINTER"
  else
    path_is_absent "$HOST_TOOL_LAUNCHER"
    path_is_absent "$HOST_TOOL_LAUNCHER_MARKER"
    path_is_absent "$CONFIGURE_AUTH_POINTER"
    path_is_absent "$UPDATE_IMAGE_POINTER"
  fi
  if [[ -e "$HOST_TOOLS_PREPARING/marker-was-present" ]]; then
    require_regular_file "$HOST_TOOLS_MARKER" root root 444
    cmp --silent "$HOST_TOOLS_PREPARING/host-tools-marker" "$HOST_TOOLS_MARKER"
  else
    path_is_absent "$HOST_TOOLS_MARKER"
  fi

  if cmp --silent "$HOST_TOOL_SOURCE_PUBLISHER" "$PUBLISHER_COMMAND"; then
    # Installing the new wrapper is the first live mutation. It recognizes
    # both preparation and canonical names, so completing this rename cannot
    # expose publisher operations after a hard interruption.
    mv -T "$HOST_TOOLS_PREPARING" "$HOST_TOOLS_TRANSACTION"
    sync -f /etc/legal-mcp
    recover_host_tools_transaction
    HOST_TOOLS_PREPARATION_WAS_RECOVERED=true
  elif cmp --silent "$HOST_TOOLS_PREPARING/publisher-command" "$PUBLISHER_COMMAND"; then
    # The atomic wrapper replacement did not occur. No live host-tool state was
    # changed, so this non-authoritative preparation can be discarded.
    retire_host_tools_directory_for_deletion \
      "$HOST_TOOLS_PREPARING" "$HOST_TOOLS_PREPARING_RETIRED"
    HOST_TOOLS_PREPARATION_WAS_RECOVERED=true
  else
    echo 'host-tool preparation cannot identify the installed publisher wrapper' >&2
    return 1
  fi
}

recover_host_tools_transaction() {
  local deny_policy
  validate_host_tools_transaction
  services_and_ingress_are_off
  deny_policy="$(mktemp /etc/legal-mcp/.publisher-sudoers-deny.XXXXXX)"
  printf '%s\n' 'Defaults:legal-mcp-publisher !requiretty' > "$deny_policy"
  chmod 440 "$deny_policy"
  visudo -cf "$deny_policy" >/dev/null
  atomic_install_file "$deny_policy" "$PUBLISHER_SUDOERS" root root 440
  rm -f "$deny_policy"
  retire_host_tools_rollback_transaction
}

rollback_host_tools_upgrade() {
  local status=$? recovery_status
  trap - ERR HUP INT TERM EXIT
  set +e
  (
    set -e
    recover_host_tools_transaction
  )
  recovery_status=$?
  set -e
  cleanup_host_tool_sources
  if [[ $recovery_status -ne 0 ]]; then
    echo 'host-tool upgrade failed and automatic rollback did not complete' >&2
    exit 1
  fi
  echo 'host-tool upgrade rolled back' >&2
  exit "$status"
}

cleanup_host_tool_sources() {
  rm -f "${HOST_TOOL_POLICY_SOURCE:-}" "${HOST_TOOL_MARKER_SOURCE:-}" \
    "${HOST_TOOL_TARGET_MANIFEST:-}" "${HOST_TOOL_LAUNCHER_SOURCE:-}" \
    "${HOST_TOOL_LAUNCHER_MARKER_SOURCE:-}" \
    "${HOST_TOOL_CONFIGURE_POINTER_SOURCE:-}" \
    "${HOST_TOOL_UPDATE_POINTER_SOURCE:-}" \
    "${HOST_TOOL_RENDERED_QUADLET_SOURCE:-}" \
    "${HOST_TOOL_RENDERED_CADDY_SOURCE:-}" || true
}

run_host_tools_operation() {
  local operation="$1" expected_version="$2" transaction_tmp
  local policy_sha256 current_marker_ok=false previous_manifest previous_marker
  local previous_launcher previous_launcher_marker previous_configure_pointer
  local previous_update_pointer
  [[ "$expected_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || usage
  for command_name in awk blkid cmp find findmnt flock getfacl id python3 readlink sha256sum \
    sort ss stat sync systemctl ufw visudo xfs_info; do
    command -v "$command_name" >/dev/null || {
      echo "missing host-tool upgrade dependency: $command_name" >&2
      exit 1
    }
  done
  require_regular_file "$HOST_TRANSACTION_LOCK" root legal-mcp-publisher 640
  exec 8<>"$HOST_TRANSACTION_LOCK"
  flock -x 8
  load_host_tool_bundle "$expected_version"
  prepare_host_tool_runtime_sources
  HOST_TOOL_POLICY_SOURCE="$(mktemp /etc/legal-mcp/.publisher-sudoers-new.XXXXXX)"
  render_publisher_sudoers "$HOST_DEPLOY_SHA256" "$HOST_TOOL_POLICY_SOURCE"
  policy_sha256="$(sha256sum "$HOST_TOOL_POLICY_SOURCE" | awk '{print $1}')"
  HOST_TOOL_MARKER_SOURCE="$(mktemp /etc/legal-mcp/.host-tools-new.XXXXXX)"
  render_host_tools_marker "$policy_sha256" "$HOST_TOOL_MARKER_SOURCE"
  HOST_TOOL_TARGET_MANIFEST="$(mktemp /etc/legal-mcp/.host-tools-target.XXXXXX)"
  render_host_tools_hash_manifest \
    "$HOST_TOOL_SOURCE_DEPLOY" "$HOST_TOOL_SOURCE_PUBLISHER" \
    "$HOST_TOOL_LAUNCHER_SOURCE" "$HOST_TOOL_LAUNCHER_SOURCE" \
    "$HOST_TOOL_SOURCE_CONTAINER_TEMPLATE" "$HOST_TOOL_RENDERED_QUADLET_SOURCE" \
    "$HOST_TOOL_POLICY_SOURCE" "$HOST_TOOL_MARKER_SOURCE" \
    "$HOST_TOOL_LAUNCHER_SOURCE" "$HOST_TOOL_LAUNCHER_MARKER_SOURCE" \
    "$HOST_TOOL_CONFIGURE_POINTER_SOURCE" "$HOST_TOOL_UPDATE_POINTER_SOURCE" \
    "$HOST_TOOL_RENDERED_CADDY_SOURCE" "$HOST_TOOL_TARGET_MANIFEST"
  trap cleanup_host_tool_sources EXIT
  reconcile_host_tools_building
  services_and_ingress_are_off
  finalize_host_tools_transaction_retirement
  reconcile_host_tools_preparation

  if [[ "$operation" = recover ]]; then
    if path_is_absent "$HOST_TOOLS_TRANSACTION"; then
      if [[ "$HOST_TOOLS_PREPARATION_WAS_RECOVERED" = true ]]; then
        rm -f "$HOST_TOOL_POLICY_SOURCE" "$HOST_TOOL_MARKER_SOURCE" \
          "$HOST_TOOL_TARGET_MANIFEST"
        echo 'interrupted host-tool preparation rolled back'
        exit 0
      fi
      if [[ "$HOST_TOOLS_RETIREMENT_WAS_PENDING" = true ]]; then
        rm -f "$HOST_TOOL_POLICY_SOURCE" "$HOST_TOOL_MARKER_SOURCE" \
          "$HOST_TOOL_TARGET_MANIFEST"
        echo 'interrupted host-tool transaction retirement completed'
        exit 0
      fi
      echo 'no host-tool transaction exists' >&2
      exit 1
    fi
    recover_host_tools_transaction
    rm -f "$HOST_TOOL_POLICY_SOURCE" "$HOST_TOOL_MARKER_SOURCE" \
      "$HOST_TOOL_TARGET_MANIFEST"
    echo 'interrupted host-tool upgrade rolled back'
    exit 0
  fi

  path_is_absent "$HOST_TOOLS_TRANSACTION" || {
    echo 'a host-tool transaction already exists; recover it first' >&2
    exit 1
  }
  validate_installed_host

  if ! path_is_absent "$HOST_TOOLS_MARKER"; then
    require_regular_file "$HOST_TOOLS_MARKER" root root 444
    require_exact_acl "$HOST_TOOLS_MARKER" $'user::r--\ngroup::r--\nother::r--'
  fi
  if [[ -f "$HOST_TOOLS_MARKER" && ! -L "$HOST_TOOLS_MARKER" ]] \
    && cmp --silent "$HOST_TOOL_MARKER_SOURCE" "$HOST_TOOLS_MARKER"; then
    validate_live_target_host_tools || {
      echo 'V2 host-tool marker exists but the installed target bytes are invalid' >&2
      exit 1
    }
    current_marker_ok=true
  fi
  if [[ "$current_marker_ok" = true ]]; then
    rm -f "$HOST_TOOL_POLICY_SOURCE" "$HOST_TOOL_MARKER_SOURCE" \
      "$HOST_TOOL_TARGET_MANIFEST"
    echo "host tools already match $expected_version ($HOST_TOOL_REVISION)"
    exit 0
  fi

  transaction_tmp="$HOST_TOOLS_BUILDING"
  install -d -o root -g root -m 0700 "$transaction_tmp"
  chown root:root "$transaction_tmp"
  chmod 700 "$transaction_tmp"
  install -o root -g root -m 0600 "$HOST_DEPLOY" "$transaction_tmp/host-deploy"
  install -o root -g root -m 0600 "$PUBLISHER_COMMAND" "$transaction_tmp/publisher-command"
  install -o root -g root -m 0600 "$CONFIGURE_AUTH" "$transaction_tmp/configure-auth"
  install -o root -g root -m 0600 "$UPDATE_IMAGE" "$transaction_tmp/update-image"
  install -o root -g root -m 0600 "$CONTAINER_TEMPLATE" "$transaction_tmp/container-template"
  install -o root -g root -m 0600 "$RENDERED_QUADLET" "$transaction_tmp/rendered-quadlet"
  install -o root -g root -m 0600 "$CADDYFILE" "$transaction_tmp/Caddyfile"
  install -o root -g root -m 0600 "$PUBLISHER_SUDOERS" "$transaction_tmp/publisher-sudoers"
  printf '%s\n' LEGAL_MCP_HOST_TOOLS_TRANSACTION_V2 > "$transaction_tmp/kind"
  printf '%s\n' "$HOST_TOOL_VERSION" > "$transaction_tmp/target-version"
  printf '%s\n' "$HOST_TOOL_REVISION" > "$transaction_tmp/target-revision"
  chmod 600 "$transaction_tmp/kind" \
    "$transaction_tmp/target-version" "$transaction_tmp/target-revision"
  if path_is_absent "$HOST_TOOLS_MARKER"; then
    install -o root -g root -m 0600 /dev/null "$transaction_tmp/marker-was-absent"
  else
    require_regular_file "$HOST_TOOLS_MARKER" root root 444
    install -o root -g root -m 0444 "$HOST_TOOLS_MARKER" "$transaction_tmp/host-tools-marker"
    install -o root -g root -m 0600 /dev/null "$transaction_tmp/marker-was-present"
  fi
  if [[ "$HOST_TOOL_ENTRYPOINT_STATE" = stable ]]; then
    install -o root -g root -m 0600 /dev/null "$transaction_tmp/launcher-was-present"
    install -o root -g root -m 0600 "$HOST_TOOL_LAUNCHER" \
      "$transaction_tmp/host-tool-launcher"
    install -o root -g root -m 0600 "$HOST_TOOL_LAUNCHER_MARKER" \
      "$transaction_tmp/launcher-marker"
    install -o root -g root -m 0600 "$CONFIGURE_AUTH_POINTER" \
      "$transaction_tmp/configure-auth-pointer"
    install -o root -g root -m 0600 "$UPDATE_IMAGE_POINTER" \
      "$transaction_tmp/update-image-pointer"
    stat -c '%d:%i' "$HOST_TOOL_LAUNCHER" \
      > "$transaction_tmp/host-tool-launcher-device-inode"
    stat -c '%d:%i' "$CONFIGURE_AUTH" > "$transaction_tmp/configure-auth-device-inode"
    stat -c '%d:%i' "$UPDATE_IMAGE" > "$transaction_tmp/update-image-device-inode"
  else
    install -o root -g root -m 0600 /dev/null "$transaction_tmp/launcher-was-absent"
    printf '%s\n' - > "$transaction_tmp/host-tool-launcher-device-inode"
    stat -c '%d:%i' "$CONFIGURE_AUTH" > "$transaction_tmp/configure-auth-device-inode"
    stat -c '%d:%i' "$UPDATE_IMAGE" > "$transaction_tmp/update-image-device-inode"
  fi
  chmod 600 "$transaction_tmp/host-tool-launcher-device-inode" \
    "$transaction_tmp/configure-auth-device-inode" \
    "$transaction_tmp/update-image-device-inode"
  previous_manifest="$(mktemp /run/legal-mcp-host-tools-previous.XXXXXX)"
  previous_marker=-
  previous_launcher=-
  previous_launcher_marker=-
  previous_configure_pointer=-
  previous_update_pointer=-
  if [[ -e "$transaction_tmp/marker-was-present" ]]; then
    previous_marker="$transaction_tmp/host-tools-marker"
  fi
  if [[ -e "$transaction_tmp/launcher-was-present" ]]; then
    previous_launcher="$transaction_tmp/host-tool-launcher"
    previous_launcher_marker="$transaction_tmp/launcher-marker"
    previous_configure_pointer="$transaction_tmp/configure-auth-pointer"
    previous_update_pointer="$transaction_tmp/update-image-pointer"
  fi
  render_host_tools_hash_manifest \
    "$transaction_tmp/host-deploy" "$transaction_tmp/publisher-command" \
    "$transaction_tmp/configure-auth" "$transaction_tmp/update-image" \
    "$transaction_tmp/container-template" "$transaction_tmp/rendered-quadlet" \
    "$transaction_tmp/publisher-sudoers" "$previous_marker" \
    "$previous_launcher" "$previous_launcher_marker" \
    "$previous_configure_pointer" "$previous_update_pointer" \
    "$transaction_tmp/Caddyfile" "$previous_manifest"
  install -o root -g root -m 0600 "$previous_manifest" "$transaction_tmp/previous-sha256"
  install -o root -g root -m 0600 "$HOST_TOOL_TARGET_MANIFEST" "$transaction_tmp/target-sha256"
  rm -f "$previous_manifest"
  sync -f "$transaction_tmp"
  validate_host_tools_transaction "$transaction_tmp"
  mv -T "$transaction_tmp" "$HOST_TOOLS_PREPARING"
  sync -f /etc/legal-mcp
  # The new forced-command wrapper is the atomic pre-commit guard for both
  # sudo-routed deploy actions and direct restricted rsync.
  atomic_install_file "$HOST_TOOL_SOURCE_PUBLISHER" "$PUBLISHER_COMMAND" root root 755
  mv -T "$HOST_TOOLS_PREPARING" "$HOST_TOOLS_TRANSACTION"
  sync -f /etc/legal-mcp

  trap rollback_host_tools_upgrade ERR HUP INT TERM EXIT

  deny_policy="$(mktemp /etc/legal-mcp/.publisher-sudoers-deny.XXXXXX)"
  printf '%s\n' 'Defaults:legal-mcp-publisher !requiretty' > "$deny_policy"
  chmod 440 "$deny_policy"
  visudo -cf "$deny_policy" >/dev/null
  atomic_install_file "$deny_policy" "$PUBLISHER_SUDOERS" root root 440
  rm -f "$deny_policy"
  atomic_install_file "$HOST_TOOL_SOURCE_DEPLOY" "$HOST_DEPLOY" root root 755
  atomic_install_file "$HOST_TOOL_SOURCE_CONTAINER_TEMPLATE" "$CONTAINER_TEMPLATE" root root 644
  atomic_install_file "$HOST_TOOL_RENDERED_QUADLET_SOURCE" "$RENDERED_QUADLET" root root 644
  atomic_install_file "$HOST_TOOL_RENDERED_CADDY_SOURCE" "$CADDYFILE" root caddy 640
  if [[ "$HOST_TOOL_ENTRYPOINT_STATE" = legacy ]] \
    || ! cmp --silent "$HOST_TOOL_LAUNCHER_SOURCE" "$HOST_TOOL_LAUNCHER" \
    || ! cmp --silent "$HOST_TOOL_LAUNCHER_SOURCE" "$CONFIGURE_AUTH" \
    || ! cmp --silent "$HOST_TOOL_LAUNCHER_SOURCE" "$UPDATE_IMAGE"; then
    atomic_install_file "$HOST_TOOL_LAUNCHER_SOURCE" "$HOST_TOOL_LAUNCHER" root root 755
    atomic_install_file "$HOST_TOOL_LAUNCHER_SOURCE" "$CONFIGURE_AUTH" root root 755
    atomic_install_file "$HOST_TOOL_LAUNCHER_SOURCE" "$UPDATE_IMAGE" root root 755
    drain_saved_host_tool_processes "$HOST_TOOLS_TRANSACTION"
  else
    cmp --silent "$HOST_TOOL_LAUNCHER_SOURCE" "$HOST_TOOL_LAUNCHER"
    cmp --silent "$HOST_TOOL_LAUNCHER_SOURCE" "$CONFIGURE_AUTH"
    cmp --silent "$HOST_TOOL_LAUNCHER_SOURCE" "$UPDATE_IMAGE"
  fi
  atomic_install_file "$HOST_TOOL_CONFIGURE_POINTER_SOURCE" \
    "$CONFIGURE_AUTH_POINTER" root root 644
  atomic_install_file "$HOST_TOOL_UPDATE_POINTER_SOURCE" \
    "$UPDATE_IMAGE_POINTER" root root 644
  atomic_install_file "$HOST_TOOL_LAUNCHER_MARKER_SOURCE" \
    "$HOST_TOOL_LAUNCHER_MARKER" root root 444
  atomic_install_file "$HOST_TOOL_POLICY_SOURCE" "$PUBLISHER_SUDOERS" root root 440
  atomic_install_file "$HOST_TOOL_MARKER_SOURCE" "$HOST_TOOLS_MARKER" root root 444
  systemctl daemon-reload

  validate_live_target_host_tools
  trap - ERR HUP INT TERM EXIT
  retire_host_tools_transaction
  cleanup_host_tool_sources
  echo "host tools upgraded to $expected_version ($HOST_TOOL_REVISION); service and ingress remain off"
  exit 0
}

if [[ "${1:-}" = --recover-v0198-flat-int8 ]]; then
  [[ $# -eq 3 && "$2" = --version ]] || usage
  run_v0198_flat_int8_recovery "$3"
  exit 0
fi

if [[ "${1:-}" = --upgrade-host-tools || "${1:-}" = --recover-host-tools ]]; then
  [[ ( $# -eq 3 || ( $# -eq 4 && "$4" = --from-public ) ) \
    && "$2" = --version ]] || usage
  if [[ $# -eq 4 ]]; then
    HOST_TOOLS_FROM_PUBLIC=true
  fi
  if [[ "$1" = --upgrade-host-tools ]]; then
    run_host_tools_operation upgrade "$3"
  else
    run_host_tools_operation recover "$3"
  fi
fi

path_is_absent /etc/legal-mcp/host-installed || {
  echo 'host contract is already installed; use the transactional auth/image tools' >&2
  exit 2
}
IMAGE=''
PUBLIC_HOST=''
VOLUME_DEVICE=''
PUBLISHER_KEY_FILE=''
ADMIN_SOURCE_IP=''
INITIALIZE=false
EXPECTED_UUID=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --image) IMAGE="${2:-}"; shift 2 ;;
    --public-host) PUBLIC_HOST="${2:-}"; shift 2 ;;
    --volume-device) VOLUME_DEVICE="${2:-}"; shift 2 ;;
    --publisher-key-file) PUBLISHER_KEY_FILE="${2:-}"; shift 2 ;;
    --admin-source-ip) ADMIN_SOURCE_IP="${2:-}"; shift 2 ;;
    --initialize-empty-volume) INITIALIZE=true; shift ;;
    --expected-volume-uuid) EXPECTED_UUID="${2:-}"; shift 2 ;;
    *) usage ;;
  esac
done

[[ "$IMAGE" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ ]] || usage
[[ "$PUBLIC_HOST" =~ ^[a-z0-9.-]{3,253}$ && "$PUBLIC_HOST" == *.* ]] || usage
[[ "$VOLUME_DEVICE" =~ ^/dev/disk/by-id/[A-Za-z0-9._-]{1,200}$ ]] || usage
[[ -f "$PUBLISHER_KEY_FILE" && ! -L "$PUBLISHER_KEY_FILE" ]] || usage
[[ "$ADMIN_SOURCE_IP" =~ ^[0-9A-Fa-f:.]{2,45}$ ]] || usage
ADMIN_SOURCE_IP="$(python3 - "$ADMIN_SOURCE_IP" "$PUBLIC_HOST" <<'PY'
import ipaddress, sys
address = ipaddress.ip_address(sys.argv[1])
if address.is_unspecified or address.is_multicast:
    raise SystemExit(1)
host = sys.argv[2]
try:
    ipaddress.ip_address(host)
except ValueError:
    pass
else:
    raise SystemExit(1)
labels = host.split('.')
if any(not label or len(label) > 63 or label[0] == '-' or label[-1] == '-'
       or not all(character.isascii() and (character.islower() or character.isdigit() or character == '-')
                  for character in label)
       for label in labels):
    raise SystemExit(1)
print(address.compressed)
PY
)" || usage
if [[ "$INITIALIZE" = true ]]; then
  [[ -z "$EXPECTED_UUID" ]] || usage
else
  [[ "$EXPECTED_UUID" =~ ^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$ ]] || usage
fi

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CADDY_DEB_NAME=caddy_2.11.4_linux_amd64.deb
CADDY_DEB="$REPO_DIR/$CADDY_DEB_NAME"
for path in \
  "$REPO_DIR/Containerfile" \
  "$REPO_DIR/SOURCE_COMMIT" \
  "$CADDY_DEB" \
  "$REPO_DIR/infra/hosting/Caddyfile" \
  "$REPO_DIR/infra/hosting/caddy-artifact.sha512" \
  "$REPO_DIR/infra/hosting/configure-auth.sh" \
  "$REPO_DIR/infra/hosting/update-image.sh" \
  "$REPO_DIR/infra/hosting/legal-mcp.container.template" \
  "$REPO_DIR/scripts/legal-mcp-host-deploy" \
  "$REPO_DIR/scripts/legal-mcp-publisher-command"; do
  [[ -f "$path" && ! -L "$path" ]] || { echo "required install asset missing: $path" >&2; exit 1; }
done
[[ "$(grep -o '__IMAGE_DIGEST__' "$REPO_DIR/infra/hosting/legal-mcp.container.template" | wc -l)" = 1 \
  && "$(grep -Fxc 'ExecCondition=/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready' \
    "$REPO_DIR/infra/hosting/legal-mcp.container.template")" = 1 ]] || {
  echo 'release Quadlet template lacks its exact image or auth-ready gate' >&2
  exit 1
}
(
  cd "$REPO_DIR"
  sha512sum --check --strict infra/hosting/caddy-artifact.sha512
)
publisher_key="$(<"$PUBLISHER_KEY_FILE")"
[[ "$publisher_key" =~ ^ssh-(ed25519|rsa)[[:space:]][A-Za-z0-9+/=]+([[:space:]][^[:cntrl:]]+)?$ \
  && "$publisher_key" != *$'\n'* && ${#publisher_key} -le 16384 ]] || {
  echo 'publisher key must be one bounded OpenSSH ed25519 or RSA public key' >&2
  exit 2
}

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get upgrade --yes
apt-get install --yes --no-install-recommends \
  acl ca-certificates curl podman python3 rsync sudo ufw unattended-upgrades util-linux xfsprogs
[[ -x /usr/bin/rrsync ]] || { echo 'the installed rsync package did not provide /usr/bin/rrsync' >&2; exit 1; }
systemctl enable --now unattended-upgrades.service

# Establish the host firewall before any web service package can start.
ufw --force reset
ufw default deny incoming
ufw default allow outgoing
ufw allow proto tcp from "$ADMIN_SOURCE_IP" to any port 22 comment 'restricted SSH administration'
ufw --force enable

ensure_group() {
  local name="$1" gid="$2"
  if getent group "$name" >/dev/null; then
    [[ "$(getent group "$name" | cut -d: -f3)" = "$gid" ]] || { echo "wrong GID for $name" >&2; exit 1; }
  else
    ! getent group "$gid" >/dev/null || { echo "GID $gid is already assigned" >&2; exit 1; }
    groupadd --gid "$gid" "$name"
  fi
}
ensure_user() {
  local name="$1" uid="$2" gid="$3" home="$4" shell="$5"
  if getent passwd "$name" >/dev/null; then
    IFS=: read -r _ _ actual_uid actual_gid _ actual_home actual_shell < <(getent passwd "$name")
    [[ "$actual_uid" = "$uid" && "$actual_gid" = "$gid" \
      && "$actual_home" = "$home" && "$actual_shell" = "$shell" ]] || {
      echo "wrong fixed identity for $name" >&2
      exit 1
    }
  else
    ! getent passwd "$uid" >/dev/null || { echo "UID $uid is already assigned" >&2; exit 1; }
    useradd --uid "$uid" --gid "$gid" --home-dir "$home" --shell "$shell" --no-create-home "$name"
  fi
}
ensure_group legal-mcp 971
ensure_group legal-mcp-publisher 973
ensure_group legal-mcp-admin 974
ensure_user legal-mcp 971 971 /nonexistent /usr/sbin/nologin
ensure_user legal-mcp-publisher 973 973 /var/lib/legal-mcp-publisher /bin/bash
ensure_user legal-mcp-admin 974 974 /home/legal-mcp-admin /bin/bash
[[ "$(id -G legal-mcp)" = 971 && "$(id -G legal-mcp-publisher)" = 973 ]] || {
  echo 'service and publisher identities must not have supplementary groups' >&2
  exit 1
}

cat > /etc/tmpfiles.d/legal-mcp.conf <<'EOF'
d /run/legal-mcp 0710 root legal-mcp-publisher -
f /run/lock/legal-mcp-host-transaction.lock 0640 root legal-mcp-publisher -
EOF
chown root:root /etc/tmpfiles.d/legal-mcp.conf
chmod 644 /etc/tmpfiles.d/legal-mcp.conf
systemd-tmpfiles --create /etc/tmpfiles.d/legal-mcp.conf
[[ -d /run/legal-mcp && ! -L /run/legal-mcp \
  && "$(stat -c '%U:%G:%a' /run/legal-mcp)" = root:legal-mcp-publisher:710 \
  && -f /run/lock/legal-mcp-host-transaction.lock \
  && ! -L /run/lock/legal-mcp-host-transaction.lock \
  && "$(stat -c '%U:%G:%a:%h' /run/lock/legal-mcp-host-transaction.lock)" = root:legal-mcp-publisher:640:1 ]] || {
  echo 'host transaction lock contract was not created safely' >&2
  exit 1
}

[[ -b "$VOLUME_DEVICE" && ! -L "$(readlink -f "$VOLUME_DEVICE")" ]] || {
  echo 'volume path must resolve to a block device' >&2
  exit 1
}
DEVICE="$(readlink -f "$VOLUME_DEVICE")"
[[ "$(lsblk --noheadings --nodeps --output TYPE "$DEVICE" | tr -d ' ')" = disk ]] || {
  echo 'volume device must be an unpartitioned disk' >&2
  exit 1
}
TARGET_MOUNTED=false
if mountpoint --quiet /srv/legal-mcp 2>/dev/null; then
  TARGET_MOUNTED=true
  TARGET_SOURCE="$(findmnt --noheadings --raw --output SOURCE --mountpoint /srv/legal-mcp)"
  [[ -b "$TARGET_SOURCE" \
    && "$(stat -Lc '%t:%T' "$TARGET_SOURCE")" = "$(stat -Lc '%t:%T' "$DEVICE")" ]] || {
    echo '/srv/legal-mcp is mounted from a different block device' >&2
    exit 1
  }
else
  [[ -z "$(findmnt --noheadings --source "$DEVICE" || true)" ]] || {
    echo 'selected volume device is mounted at an unexpected target' >&2
    exit 1
  }
fi

if [[ "$INITIALIZE" = true ]]; then
  [[ "$TARGET_MOUNTED" = false ]] || { echo 'refusing to initialize an already mounted volume' >&2; exit 1; }
  [[ -z "$(blkid "$DEVICE" || true)" && -z "$(wipefs --no-act "$DEVICE" 2>/dev/null || true)" ]] || {
    echo 'refusing to format a device that contains a filesystem or signature' >&2
    exit 1
  }
  mkfs.xfs -m reflink=1 "$DEVICE"
else
  [[ "$(blkid -s TYPE -o value "$DEVICE")" = xfs ]] || { echo 'existing volume is not XFS' >&2; exit 1; }
  [[ "${EXPECTED_UUID,,}" = "$(blkid -s UUID -o value "$DEVICE" | tr '[:upper:]' '[:lower:]')" ]] || {
    echo 'existing volume UUID does not match --expected-volume-uuid' >&2
    exit 1
  }
fi
UUID="$(blkid -s UUID -o value "$DEVICE")"
[[ "$UUID" =~ ^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$ ]] || {
  echo 'volume has no canonical UUID' >&2
  exit 1
}

if [[ "$TARGET_MOUNTED" = true ]]; then
  [[ -d /srv/legal-mcp && ! -L /srv/legal-mcp ]] || {
    echo 'mounted corpus target is not a safe directory' >&2
    exit 1
  }
elif [[ -e /srv/legal-mcp || -L /srv/legal-mcp ]]; then
  if [[ ! -d /srv/legal-mcp || -L /srv/legal-mcp \
    || "$(stat -c '%U:%G:%a' /srv/legal-mcp)" != root:root:755 ]] \
    || ! directory_is_empty /srv/legal-mcp; then
    echo 'unmounted corpus target has unsafe ownership or mode' >&2
    exit 1
  fi
else
  install -d -o root -g root -m 0755 /srv/legal-mcp
fi
expected_fstab="UUID=$UUID /srv/legal-mcp xfs defaults,noatime,nodev,noexec,nosuid,nofail,x-systemd.device-timeout=30s 0 2"
mapfile -t target_fstab < <(awk '!/^[[:space:]]*#/ && NF >= 2 && $2 == "/srv/legal-mcp" {print}' /etc/fstab)
fstab_needs_append=false
if [[ ${#target_fstab[@]} -eq 0 ]]; then
  awk -v uuid="UUID=$UUID" '!/^[[:space:]]*#/ && $1 == uuid {found=1} END {exit found ? 0 : 1}' /etc/fstab \
    && { echo 'the selected volume UUID already has another fstab entry' >&2; exit 1; }
  fstab_needs_append=true
elif [[ ${#target_fstab[@]} -ne 1 || "${target_fstab[0]}" != "$expected_fstab" ]]; then
  echo 'the existing /srv/legal-mcp fstab entry does not match the exact volume contract' >&2
  exit 1
fi
mounted_by_installer=false
volume_validated=false
cleanup_unvalidated_volume() {
  local status=$?
  trap - ERR HUP INT TERM EXIT
  if [[ "$volume_validated" = false && "$mounted_by_installer" = true ]]; then
    umount /srv/legal-mcp >/dev/null 2>&1 || true
  fi
  exit "$status"
}
trap cleanup_unvalidated_volume ERR HUP INT TERM EXIT
if [[ "$TARGET_MOUNTED" = false ]]; then
  mount --types xfs --options defaults,noatime,nodev,noexec,nosuid "$DEVICE" /srv/legal-mcp
  mounted_by_installer=true
fi
read -r mounted_source mounted_type mounted_options < <(
  findmnt --noheadings --raw --output SOURCE,FSTYPE,OPTIONS --mountpoint /srv/legal-mcp
)
[[ "$mounted_type" = xfs \
  && ",$mounted_options," = *,noatime,* \
  && ",$mounted_options," = *,nodev,* \
  && ",$mounted_options," = *,noexec,* \
  && ",$mounted_options," = *,nosuid,* \
  && -b "$mounted_source" \
  && "$(stat -Lc '%t:%T' "$mounted_source")" = "$(stat -Lc '%t:%T' "$DEVICE")" \
  && "$(blkid -s UUID -o value "$mounted_source" | tr '[:upper:]' '[:lower:]')" = "${UUID,,}" ]] || {
  echo 'mounted corpus source, filesystem, or UUID does not match the selected volume' >&2
  exit 1
}
xfs_details="$(xfs_info /srv/legal-mcp)"
if ! grep -Eq 'reflink=1([[:space:]]|$)' <<< "$xfs_details" \
  || ! grep -Eq 'ftype=1([[:space:]]|$)' <<< "$xfs_details"; then
  echo 'XFS reflink or directory file types are not enabled' >&2
  exit 1
fi

MARKER=/srv/legal-mcp/.legal-mcp-volume
if [[ "$INITIALIZE" = true ]]; then
  path_is_absent "$MARKER" || { echo 'new volume unexpectedly contains an identity marker' >&2; exit 1; }
  printf 'LEGAL_MCP_VOLUME_V1\nUUID=%s\n' "${UUID,,}" > "$MARKER"
  chown root:root "$MARKER"
  chmod 444 "$MARKER"
else
  [[ -f "$MARKER" && ! -L "$MARKER" \
    && "$(stat -c '%U:%G:%a:%h' "$MARKER")" = root:root:444:1 ]] || {
    echo 'existing volume identity marker is missing or unsafe' >&2
    exit 1
  }
  [[ "$(getfacl --absolute-names --numeric --omit-header "$MARKER")" \
    = $'user::r--\ngroup::r--\nother::r--' ]] || {
    echo 'existing volume marker has an unexpected access ACL' >&2
    exit 1
  }
  mapfile -t marker < "$MARKER"
  [[ ${#marker[@]} -eq 2 && "${marker[0]}" = LEGAL_MCP_VOLUME_V1 \
    && "${marker[1]}" = "UUID=${UUID,,}" ]] || { echo 'existing volume marker does not match its UUID' >&2; exit 1; }
fi

if [[ "$INITIALIZE" = true ]]; then
  chown root:legal-mcp /srv/legal-mcp
  chmod 750 /srv/legal-mcp
  setfacl --remove-all /srv/legal-mcp
  setfacl --modify user:legal-mcp-publisher:--x /srv/legal-mcp
  install -d -o root -g legal-mcp -m 0750 /srv/legal-mcp/generations /srv/legal-mcp/lifecycle
  install -d -o legal-mcp -g legal-mcp -m 0700 /srv/legal-mcp/state
  install -d -o legal-mcp-publisher -g legal-mcp-publisher -m 0700 /srv/legal-mcp/uploads
  install -o root -g legal-mcp -m 0640 /dev/null /srv/legal-mcp/lifecycle/LOCK
  install -o root -g root -m 0640 /dev/null /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK
else
  [[ "$(stat -c '%U:%G:%a' /srv/legal-mcp)" = root:legal-mcp:750 \
    && "$(getfacl --absolute-names --numeric --omit-header /srv/legal-mcp)" \
      = $'user::rwx\nuser:973:--x\ngroup::r-x\nmask::r-x\nother::---' ]] || {
    echo 'existing corpus root ownership or publisher traversal ACL is invalid' >&2
    exit 1
  }
  for contract in \
    'generations root legal-mcp 750 user::rwx|group::r-x|other::---' \
    'lifecycle root legal-mcp 750 user::rwx|group::r-x|other::---' \
    'state legal-mcp legal-mcp 700 user::rwx|group::---|other::---' \
    'uploads legal-mcp-publisher legal-mcp-publisher 700 user::rwx|group::---|other::---'; do
    read -r name owner group mode acl_pipe <<< "$contract"
    path="/srv/legal-mcp/$name"
    expected_acl="${acl_pipe//|/$'\n'}"
    [[ -d "$path" && ! -L "$path" \
      && "$(stat -c '%U:%G:%a' "$path")" = "$owner:$group:$mode" \
      && "$(getfacl --absolute-names --numeric --omit-header "$path")" = "$expected_acl" ]] || {
      echo "existing corpus directory violates the host contract: $path" >&2
      exit 1
    }
  done
fi
[[ -f /srv/legal-mcp/lifecycle/LOCK && ! -L /srv/legal-mcp/lifecycle/LOCK \
  && "$(stat -c '%U:%G:%a:%h' /srv/legal-mcp/lifecycle/LOCK)" = root:legal-mcp:640:1 \
  && "$(getfacl --absolute-names --numeric --omit-header /srv/legal-mcp/lifecycle/LOCK)" \
    = $'user::rw-\ngroup::r--\nother::---' ]] || {
  echo 'corpus lock does not satisfy the host contract' >&2
  exit 1
}
require_empty_regular_file /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK root root 640
require_exact_acl /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK \
  $'user::rw-\ngroup::r--\nother::---'
volume_validated=true
trap - ERR HUP INT TERM EXIT
if [[ "$fstab_needs_append" = true ]]; then
  [[ -f /etc/fstab && ! -L /etc/fstab ]] || { echo '/etc/fstab is unsafe' >&2; exit 1; }
  fstab_tmp="$(mktemp /etc/.fstab.XXXXXX)"
  cat /etc/fstab > "$fstab_tmp"
  printf '\n%s\n' "$expected_fstab" >> "$fstab_tmp"
  chown --reference=/etc/fstab "$fstab_tmp"
  chmod --reference=/etc/fstab "$fstab_tmp"
  sync -f "$fstab_tmp"
  mv -fT "$fstab_tmp" /etc/fstab
  sync -f /etc
  systemctl daemon-reload
fi

install -d -o root -g root -m 0755 /etc/legal-mcp /usr/local/libexec/legal-mcp
setfacl --remove-all /etc/legal-mcp
chown root:root /etc/legal-mcp
chmod 755 /etc/legal-mcp
install -o root -g root -m 0755 "$REPO_DIR/scripts/legal-mcp-host-deploy" /usr/local/sbin/legal-mcp-host-deploy
install -o root -g root -m 0755 "$REPO_DIR/scripts/legal-mcp-publisher-command" /usr/local/sbin/legal-mcp-publisher-command
install -o root -g root -m 0644 "$REPO_DIR/infra/hosting/legal-mcp.container.template" \
  /usr/local/libexec/legal-mcp/legal-mcp.container.template
printf '%s\n' "$IMAGE" > /etc/legal-mcp/image
chown root:root /etc/legal-mcp/image
chmod 600 /etc/legal-mcp/image
HOST_TOOL_SOURCE_CONFIGURE_AUTH="$REPO_DIR/infra/hosting/configure-auth.sh"
HOST_TOOL_SOURCE_UPDATE_IMAGE="$REPO_DIR/infra/hosting/update-image.sh"
HOST_TOOL_SOURCE_CONTAINER_TEMPLATE="$REPO_DIR/infra/hosting/legal-mcp.container.template"
HOST_TOOL_SOURCE_CADDY_TEMPLATE="$REPO_DIR/infra/hosting/Caddyfile"
CONFIGURE_AUTH_SHA256="$(sha256sum "$HOST_TOOL_SOURCE_CONFIGURE_AUTH" | awk '{print $1}')"
UPDATE_IMAGE_SHA256="$(sha256sum "$HOST_TOOL_SOURCE_UPDATE_IMAGE" | awk '{print $1}')"
CONTAINER_TEMPLATE_SHA256="$(sha256sum "$HOST_TOOL_SOURCE_CONTAINER_TEMPLATE" | awk '{print $1}')"
prepare_host_tool_runtime_sources
install -o root -g root -m 0755 "$HOST_TOOL_LAUNCHER_SOURCE" "$HOST_TOOL_LAUNCHER"
install -o root -g root -m 0755 "$HOST_TOOL_LAUNCHER_SOURCE" "$CONFIGURE_AUTH"
install -o root -g root -m 0755 "$HOST_TOOL_LAUNCHER_SOURCE" "$UPDATE_IMAGE"
atomic_install_file "$HOST_TOOL_CONFIGURE_POINTER_SOURCE" "$CONFIGURE_AUTH_POINTER" root root 644
atomic_install_file "$HOST_TOOL_UPDATE_POINTER_SOURCE" "$UPDATE_IMAGE_POINTER" root root 644
atomic_install_file "$HOST_TOOL_LAUNCHER_MARKER_SOURCE" "$HOST_TOOL_LAUNCHER_MARKER" root root 444
rm -f -- "$AUTH_READY_MARKER" "$AUTH_CONFIGURING_PERMIT" \
  "$CUTOVER_STARTING_PERMIT" "$CUTOVER_START_ARM"
cleanup_host_tool_sources
printf '%s\n' "$ADMIN_SOURCE_IP" > /etc/legal-mcp/admin-source-ip
chown root:root /etc/legal-mcp/admin-source-ip
chmod 600 /etc/legal-mcp/admin-source-ip
cat > /etc/legal-mcp/runtime.env <<EOF
LEGAL_MCP_HTTP_AUTH=disabled
LEGAL_MCP_API_KEYS_FILE=/run/secrets/legal-mcp-api-keys.json
LEGAL_MCP_EXTERNAL_URL=https://$PUBLIC_HOST/mcp
LEGAL_MCP_ALLOWED_ORIGINS=https://$PUBLIC_HOST
LEGAL_MCP_HTTP_WORKERS=4
LEGAL_MCP_SHUTDOWN_GRACE_SECONDS=30
EOF
chown root:root /etc/legal-mcp/runtime.env
chmod 600 /etc/legal-mcp/runtime.env
printf '{"keys":[],"version":1}\n' > /etc/legal-mcp/api-keys.json
chown legal-mcp:legal-mcp /etc/legal-mcp/api-keys.json
chmod 400 /etc/legal-mcp/api-keys.json

install -d -o root -g root -m 0755 /var/lib/legal-mcp-publisher
install -d -o root -g legal-mcp-publisher -m 0710 /var/lib/legal-mcp-publisher/.ssh
printf 'restrict,command="/usr/local/sbin/legal-mcp-publisher-command" %s\n' "$publisher_key" \
  > /var/lib/legal-mcp-publisher/.ssh/authorized_keys
chown root:legal-mcp-publisher /var/lib/legal-mcp-publisher/.ssh/authorized_keys
chmod 640 /var/lib/legal-mcp-publisher/.ssh/authorized_keys
if [[ "$(stat -c '%U:%G:%a' /var/lib/legal-mcp-publisher/.ssh)" \
      != root:legal-mcp-publisher:710 \
    || "$(stat -c '%U:%G:%a' /var/lib/legal-mcp-publisher/.ssh/authorized_keys)" \
      != root:legal-mcp-publisher:640 ]] \
  || ! runuser -u legal-mcp-publisher -- \
    test -r /var/lib/legal-mcp-publisher/.ssh/authorized_keys; then
  echo 'publisher authorized key is not safely readable by the restricted account' >&2
  exit 1
fi
[[ -s /root/.ssh/authorized_keys && ! -L /root/.ssh/authorized_keys ]] || {
  echo 'the provisioned root administrator key is missing' >&2
  exit 1
}
mapfile -t admin_keys < /root/.ssh/authorized_keys
[[ ${#admin_keys[@]} -eq 1 \
  && "${admin_keys[0]}" =~ ^ssh-(ed25519|rsa)[[:space:]][A-Za-z0-9+/=]+([[:space:]][^[:cntrl:]]+)?$ ]] || {
  echo 'expected exactly one provisioned administrator public key' >&2
  exit 1
}
install -d -o legal-mcp-admin -g legal-mcp-admin -m 0700 \
  /home/legal-mcp-admin /home/legal-mcp-admin/.ssh
install -o legal-mcp-admin -g legal-mcp-admin -m 0600 \
  /root/.ssh/authorized_keys /home/legal-mcp-admin/.ssh/authorized_keys
cat > /etc/sudoers.d/legal-mcp-admin <<'EOF'
Defaults:legal-mcp-admin !requiretty
legal-mcp-admin ALL=(ALL:ALL) NOPASSWD: ALL
EOF
chmod 440 /etc/sudoers.d/legal-mcp-admin
visudo -cf /etc/sudoers.d/legal-mcp-admin
installed_host_deploy_sha256="$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}')"
publisher_sudoers_tmp="$(mktemp /etc/sudoers.d/.legal-mcp-publisher.XXXXXX)"
render_publisher_sudoers "$installed_host_deploy_sha256" "$publisher_sudoers_tmp"
mv -fT "$publisher_sudoers_tmp" /etc/sudoers.d/legal-mcp-publisher
chown root:root /etc/sudoers.d/legal-mcp-publisher
chmod 440 /etc/sudoers.d/legal-mcp-publisher
visudo -cf /etc/sudoers.d/legal-mcp-publisher
podman pull "$IMAGE"
bundle_version="$(sed -n 's/^ARG VERSION=//p' "$REPO_DIR/Containerfile")"
bundle_revision="$(<"$REPO_DIR/SOURCE_COMMIT")"
image_version="$(podman image inspect "$IMAGE" --format '{{index .Labels "org.opencontainers.image.version"}}')"
image_source="$(podman image inspect "$IMAGE" --format '{{index .Labels "org.opencontainers.image.source"}}')"
image_revision="$(podman image inspect "$IMAGE" --format '{{index .Labels "org.opencontainers.image.revision"}}')"
[[ "$bundle_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ \
  && "$bundle_revision" =~ ^[0-9a-f]{40}$ \
  && "$image_version" = "$bundle_version" \
  && "$image_source" = https://github.com/gunba/australian-legal-mcp \
  && "$image_revision" = "$bundle_revision" ]] || {
  echo 'hosting bundle and OCI image metadata do not match' >&2
  exit 1
}
podman run --rm --network=none --read-only --cap-drop=all \
  --security-opt=no-new-privileges "$IMAGE" verify-runtime |
  grep -F '"onnx_runtime_ready":true'
if [[ -f /srv/legal-mcp/lifecycle/active-generation ]]; then
  podman run --rm --network=none --user=0:0 --read-only --cap-drop=all \
    --security-opt=no-new-privileges --pids-limit=256 --memory=6g --memory-swap=6g \
    --tmpfs=/tmp:rw,nodev,nosuid,noexec,size=64m,mode=1777 \
    --volume=/srv/legal-mcp:/var/lib/legal-mcp:ro,nodev,nosuid \
    "$IMAGE" verify --quiet >/dev/null
fi
mkdir -p /etc/containers/systemd
sed "s|__IMAGE_DIGEST__|$IMAGE|g" "$REPO_DIR/infra/hosting/legal-mcp.container.template" \
  > /etc/containers/systemd/legal-mcp.container
chown root:root /etc/containers/systemd/legal-mcp.container
chmod 644 /etc/containers/systemd/legal-mcp.container

dpkg --install "$CADDY_DEB" || apt-get install --fix-broken --yes
systemctl disable --now caddy.service
sed "s/__PUBLIC_HOST__/$PUBLIC_HOST/g" "$REPO_DIR/infra/hosting/Caddyfile" > /etc/caddy/Caddyfile
chown root:caddy /etc/caddy/Caddyfile
chmod 640 /etc/caddy/Caddyfile
caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile

systemctl daemon-reload
systemctl cat legal-mcp.service >/dev/null
systemctl stop legal-mcp.service
systemctl disable --now caddy.service
service_enablement="$(read_systemctl_enablement legal-mcp.service)"
service_activity="$(read_systemctl_activity legal-mcp.service)"
caddy_enablement="$(read_systemctl_enablement caddy.service)"
caddy_activity="$(read_systemctl_activity caddy.service)"
[[ "$service_enablement" = generated && "$service_activity" = inactive \
  && "$caddy_enablement" = disabled && "$caddy_activity" = inactive ]] || {
  echo 'installed service units do not match the generated/inactive and disabled/inactive contract' >&2
  exit 1
}

# Cut over SSH only after every runtime, storage, and proxy validation above
# has succeeded. The invoking root session remains available for the required
# second-session administrator test printed below.
cat > /etc/ssh/sshd_config.d/90-legal-mcp-hardening.conf <<'EOF'
PasswordAuthentication no
KbdInteractiveAuthentication no
AuthenticationMethods publickey
PermitRootLogin no
AllowUsers legal-mcp-admin legal-mcp-publisher
X11Forwarding no
AllowAgentForwarding no
AllowTcpForwarding no
PermitTunnel no
GatewayPorts no
PermitUserEnvironment no
MaxAuthTries 3
LogLevel VERBOSE
EOF
chmod 644 /etc/ssh/sshd_config.d/90-legal-mcp-hardening.conf
/usr/sbin/sshd -t
systemctl restart ssh.service
passwd --lock root >/dev/null

HOST_TOOL_VERSION="$bundle_version"
HOST_TOOL_REVISION="$bundle_revision"
HOST_DEPLOY_SHA256="$installed_host_deploy_sha256"
PUBLISHER_COMMAND_SHA256="$(sha256sum /usr/local/sbin/legal-mcp-publisher-command | awk '{print $1}')"
CONFIGURE_AUTH_SHA256="$(sha256sum "$REPO_DIR/infra/hosting/configure-auth.sh" | awk '{print $1}')"
UPDATE_IMAGE_SHA256="$(sha256sum "$REPO_DIR/infra/hosting/update-image.sh" | awk '{print $1}')"
CONTAINER_TEMPLATE_SHA256="$(sha256sum /usr/local/libexec/legal-mcp/legal-mcp.container.template | awk '{print $1}')"
installed_sudoers_sha256="$(sha256sum /etc/sudoers.d/legal-mcp-publisher | awk '{print $1}')"
host_tools_marker_tmp="$(mktemp /etc/legal-mcp/.host-tools.XXXXXX)"
render_host_tools_marker "$installed_sudoers_sha256" "$host_tools_marker_tmp"
atomic_install_file "$host_tools_marker_tmp" "$HOST_TOOLS_MARKER" root root 444
rm -f "$host_tools_marker_tmp"
require_regular_file "$HOST_TOOLS_MARKER" root root 444
require_exact_acl "$HOST_TOOLS_MARKER" $'user::r--\ngroup::r--\nother::r--'

# This sentinel is the final durable write. If installation is interrupted
# before it appears, the ordinary installer remains safely rerunnable.
host_installed_tmp="$(mktemp /etc/legal-mcp/.host-installed.XXXXXX)"
cat > "$host_installed_tmp" <<EOF
LEGAL_MCP_HOST_V1
VOLUME_UUID=${UUID,,}
EOF
chmod 444 "$host_installed_tmp"
atomic_install_file "$host_installed_tmp" /etc/legal-mcp/host-installed root root 444
rm -f "$host_installed_tmp"

cat <<EOF
Host installation complete.
Volume UUID: ${UUID,,}
Keep this root session open. In a second session, connect as legal-mcp-admin
and prove 'sudo -n true' before disconnecting root.
The generated application unit is inactive; Caddy is disabled and inactive.
Deploy a corpus generation first,
then configure API-key and/or Entra auth, prove readiness, and enable ingress.
Confirm the attached Akamai Cloud Firewall still allows SSH only from $ADMIN_SOURCE_IP;
add public TCP 80/443 only at the final ingress cutover, and never expose 51235.
$(if [[ -e /var/run/reboot-required ]]; then echo 'A reboot is required before corpus deployment.'; fi)
EOF
