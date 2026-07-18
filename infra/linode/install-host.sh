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

On an already installed bootstrap host, upgrade or recover only the restricted
publisher tools from an exact version-matched Linux release bundle:
  sudo infra/linode/install-host.sh --upgrade-host-tools --version X.Y.Z
  sudo infra/linode/install-host.sh --recover-host-tools --version X.Y.Z
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
PUBLISHER_SUDOERS=/etc/sudoers.d/legal-mcp-publisher
HOST_TRANSACTION_LOCK=/run/lock/legal-mcp-host-transaction.lock
HOST_TOOLS_RETIREMENT_WAS_PENDING=false
HOST_TOOLS_PREPARATION_WAS_RECOVERED=false

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

load_host_tool_bundle() {
  local expected_version="$1" binary_version
  local -a versions revisions
  HOST_TOOL_REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
  HOST_TOOL_SOURCE_DEPLOY="$HOST_TOOL_REPO_DIR/scripts/legal-mcp-host-deploy"
  HOST_TOOL_SOURCE_PUBLISHER="$HOST_TOOL_REPO_DIR/scripts/legal-mcp-publisher-command"
  HOST_TOOL_SOURCE_BINARY="$HOST_TOOL_REPO_DIR/legal-mcp"
  require_release_directory "$HOST_TOOL_REPO_DIR"
  require_release_directory "$HOST_TOOL_REPO_DIR/infra"
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
  [[ "$HOST_DEPLOY_SHA256$PUBLISHER_COMMAND_SHA256" =~ ^[0-9a-f]{128}$ ]] || return 1
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

close_public_ingress() {
  local port state enabled activity
  systemctl disable --now caddy.service >/dev/null 2>&1 || {
    echo 'could not disable and stop Caddy' >&2
    return 1
  }
  for port in 80 443; do
    state="$(ufw_rule_state "$port")" || return 1
    if [[ "$state" = present ]]; then
      ufw --force delete allow "$port/tcp" >/dev/null 2>&1 || {
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

services_and_ingress_are_off() {
  local service_enabled service_activity caddy_enabled caddy_activity
  local invalid=false listeners web_listener
  service_enabled="$(read_systemctl_enablement legal-mcp.service)" || return 1
  service_activity="$(read_systemctl_activity legal-mcp.service)" || return 1
  caddy_enabled="$(read_systemctl_enablement caddy.service)" || return 1
  caddy_activity="$(read_systemctl_activity caddy.service)" || return 1
  if [[ "$service_enabled" != generated ]]; then
    echo 'legal-mcp.service must be generated for the bootstrap host-tool upgrade' >&2
    invalid=true
  fi
  if [[ "$service_activity" != inactive ]]; then
    echo 'legal-mcp.service must be inactive for the bootstrap host-tool upgrade' >&2
    invalid=true
  fi
  if [[ "$caddy_enabled" != disabled ]]; then
    echo 'caddy.service must be disabled for the bootstrap host-tool upgrade' >&2
    invalid=true
  fi
  if [[ "$caddy_activity" != inactive ]]; then
    echo 'caddy.service must be inactive for the bootstrap host-tool upgrade' >&2
    invalid=true
  fi
  if [[ "$invalid" = true ]]; then
    close_public_ingress || return 1
    if [[ "$service_activity" = active ]]; then
      systemctl stop legal-mcp.service >/dev/null 2>&1 || return 1
      service_activity="$(read_systemctl_activity legal-mcp.service)" || return 1
      [[ "$service_activity" = inactive ]] || return 1
    fi
    return 1
  fi
  ufw_is_ssh_only || {
    close_public_ingress || return 1
    echo 'host ingress must be the exact SSH-only UFW allowlist' >&2
    return 1
  }
  listeners="$(ss --listening --tcp --numeric --no-header)" || {
    echo 'could not inspect bootstrap listening sockets' >&2
    return 1
  }
  web_listener="$(awk '$4 ~ /:(80|443|51235)$/ { print "present"; exit }' \
    <<< "$listeners")" || {
    echo 'could not evaluate bootstrap listening sockets' >&2
    return 1
  }
  if [[ -n "$web_listener" ]]; then
    echo 'bootstrap web or service ports must not be listening' >&2
    return 1
  fi
}

validate_installed_bootstrap_host() {
  local source fstype options xfs_details actual_uuid host_uuid volume_uuid directory image rendered
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
  require_regular_file "$PUBLISHER_SUDOERS" root root 440
  visudo -cf "$PUBLISHER_SUDOERS" >/dev/null
  require_regular_file /etc/legal-mcp/image root root 600
  require_regular_file /etc/legal-mcp/api-keys.json legal-mcp legal-mcp 400
  require_regular_file /etc/containers/systemd/legal-mcp.container root root 644
  require_regular_file /usr/local/libexec/legal-mcp/legal-mcp.container.template root root 644
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
  [[ ${#entries[@]} -eq 1 && "${entries[0]}" = disabled ]] || {
    echo 'bootstrap host authentication must remain disabled' >&2
    return 1
  }
  mapfile -t entries < /etc/legal-mcp/image
  [[ ${#entries[@]} -eq 1 \
    && "${entries[0]}" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ ]] || {
    echo 'installed bootstrap image pin is malformed' >&2
    return 1
  }
  image="${entries[0]}"
  [[ "$(grep -o '__IMAGE_DIGEST__' /usr/local/libexec/legal-mcp/legal-mcp.container.template | wc -l)" = 1 ]] || {
    echo 'installed bootstrap Quadlet template is malformed' >&2
    return 1
  }
  rendered="$(mktemp /run/legal-mcp-host-tools-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$image|g" \
    /usr/local/libexec/legal-mcp/legal-mcp.container.template > "$rendered"
  if ! cmp --silent "$rendered" /etc/containers/systemd/legal-mcp.container; then
    rm -f "$rendered"
    echo 'installed bootstrap image, Quadlet, and template do not agree' >&2
    return 1
  fi
  rm -f "$rendered"
  python3 - /etc/legal-mcp/api-keys.json <<'PY'
import json, pathlib, stat, sys
path = pathlib.Path(sys.argv[1])
meta = path.lstat()
if path.is_symlink() or not stat.S_ISREG(meta.st_mode) or meta.st_nlink != 1:
    raise SystemExit(1)
if json.loads(path.read_bytes()) != {"keys": [], "version": 1}:
    raise SystemExit(1)
PY
  path_is_absent /srv/legal-mcp/lifecycle/active-generation || {
    echo 'host-tool upgrade requires no active generation' >&2
    return 1
  }
  if ! directory_is_empty /srv/legal-mcp/generations \
    || ! directory_is_empty /srv/legal-mcp/state; then
    echo 'bootstrap generations and state directories must be empty' >&2
    return 1
  fi
  if ! path_is_absent /etc/legal-mcp/.auth-transaction \
    || ! path_is_absent /etc/legal-mcp/.image-transaction \
    || ! path_is_absent /etc/legal-mcp/.image-transaction.retiring; then
    echo 'auth or image transaction must be recovered before host-tool upgrade' >&2
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
    # The first parent sync makes removal of the canonical transaction name
    # durable. Only then is the directory moved to a deletion-only name.
    sync -f /etc/legal-mcp
    mv -T "$HOST_TOOLS_RETIRING" "$HOST_TOOLS_RETIRED"
    sync -f /etc/legal-mcp
  fi
  if ! path_is_absent "$HOST_TOOLS_RETIRED"; then
    HOST_TOOLS_RETIREMENT_WAS_PENDING=true
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
LEGAL_MCP_HOST_TOOLS_V1
VERSION=$HOST_TOOL_VERSION
SOURCE_COMMIT=$HOST_TOOL_REVISION
HOST_DEPLOY_SHA256=$HOST_DEPLOY_SHA256
PUBLISHER_COMMAND_SHA256=$PUBLISHER_COMMAND_SHA256
SUDOERS_SHA256=$sudoers_sha256
EOF
  chmod 444 "$destination"
}

validate_host_tools_transaction() {
  local transaction_path="${1:-$HOST_TOOLS_TRANSACTION}"
  local -a kind version revision
  [[ -d "$transaction_path" && ! -L "$transaction_path" \
    && "$(stat -c '%U:%G:%a' "$transaction_path")" = root:root:700 ]] || {
    echo 'host-tool transaction is missing or unsafe' >&2
    return 1
  }
  for name in kind target-version target-revision host-deploy publisher-command publisher-sudoers; do
    require_regular_file "$transaction_path/$name" root root 600 || return 1
  done
  mapfile -t kind < "$transaction_path/kind"
  mapfile -t version < "$transaction_path/target-version"
  mapfile -t revision < "$transaction_path/target-revision"
  [[ ${#kind[@]} -eq 1 && "${kind[0]}" = LEGAL_MCP_HOST_TOOLS_TRANSACTION_V1 \
    && ${#version[@]} -eq 1 && "${version[0]}" = "$HOST_TOOL_VERSION" \
    && ${#revision[@]} -eq 1 && "${revision[0]}" = "$HOST_TOOL_REVISION" ]] || {
    echo 'host-tool transaction identity is invalid for this release' >&2
    return 1
  }
  if [[ -e "$transaction_path/marker-was-present" ]]; then
    require_regular_file "$transaction_path/marker-was-present" root root 600
    require_regular_file "$transaction_path/host-tools-marker" root root 444
    directory_contains_only "$transaction_path" \
      host-deploy host-tools-marker kind marker-was-present publisher-command \
      publisher-sudoers target-revision target-version || {
      echo 'host-tool transaction contains unexpected state' >&2
      return 1
    }
  else
    require_regular_file "$transaction_path/marker-was-absent" root root 600
    directory_contains_only "$transaction_path" \
      host-deploy kind marker-was-absent publisher-command publisher-sudoers \
      target-revision target-version || {
      echo 'host-tool transaction contains unexpected state' >&2
      return 1
    }
  fi
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

complete_host_tools_publisher_restore() {
  validate_host_tools_transaction "$HOST_TOOLS_PUBLISHER_RESTORE"
  require_regular_file "$HOST_DEPLOY" root root 755
  require_regular_file "$PUBLISHER_COMMAND" root root 755
  require_regular_file "$PUBLISHER_SUDOERS" root root 440
  cmp --silent "$HOST_TOOLS_PUBLISHER_RESTORE/host-deploy" "$HOST_DEPLOY"
  cmp --silent "$HOST_TOOLS_PUBLISHER_RESTORE/publisher-sudoers" "$PUBLISHER_SUDOERS"
  saved_host_tools_marker_is_restored "$HOST_TOOLS_PUBLISHER_RESTORE"
  visudo -cf "$PUBLISHER_SUDOERS" >/dev/null

  # The versioned wrapper is restored last. Until this atomic replacement, the
  # new wrapper rejects the publisher-restore sentinel. After it, every old
  # host-tool file and marker has already been restored durably.
  atomic_install_file "$HOST_TOOLS_PUBLISHER_RESTORE/publisher-command" \
    "$PUBLISHER_COMMAND" root root 755
  cmp --silent "$HOST_TOOLS_PUBLISHER_RESTORE/publisher-command" "$PUBLISHER_COMMAND"
  retire_host_tools_directory_for_deletion \
    "$HOST_TOOLS_PUBLISHER_RESTORE" "$HOST_TOOLS_PUBLISHER_RESTORE_RETIRED"
}

complete_host_tools_rollback_retirement() {
  validate_host_tools_transaction "$HOST_TOOLS_ROLLBACK_RETIRED"
  require_regular_file "$PUBLISHER_COMMAND" root root 755
  cmp --silent "$HOST_TOOL_SOURCE_PUBLISHER" "$PUBLISHER_COMMAND"

  atomic_install_file "$HOST_TOOLS_ROLLBACK_RETIRED/host-deploy" \
    "$HOST_DEPLOY" root root 755
  atomic_install_file "$HOST_TOOLS_ROLLBACK_RETIRED/publisher-sudoers" \
    "$PUBLISHER_SUDOERS" root root 440
  restore_saved_host_tools_marker "$HOST_TOOLS_ROLLBACK_RETIRED"
  cmp --silent "$HOST_TOOLS_ROLLBACK_RETIRED/host-deploy" "$HOST_DEPLOY"
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
  require_regular_file "$PUBLISHER_SUDOERS" root root 440
  cmp --silent "$HOST_TOOLS_PREPARING/host-deploy" "$HOST_DEPLOY"
  cmp --silent "$HOST_TOOLS_PREPARING/publisher-sudoers" "$PUBLISHER_SUDOERS"
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
  validate_installed_bootstrap_host
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
  rm -f "$HOST_TOOL_POLICY_SOURCE" "$HOST_TOOL_MARKER_SOURCE" || true
  set +e
  (
    set -e
    recover_host_tools_transaction
  )
  recovery_status=$?
  set -e
  if [[ $recovery_status -ne 0 ]]; then
    echo 'host-tool upgrade failed and automatic rollback did not complete' >&2
    exit 1
  fi
  echo 'host-tool upgrade rolled back' >&2
  exit "$status"
}

run_host_tools_operation() {
  local operation="$1" expected_version="$2" transaction_tmp
  local policy_sha256 current_marker_ok=false
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
  reconcile_host_tools_building
  services_and_ingress_are_off
  finalize_host_tools_transaction_retirement
  reconcile_host_tools_preparation

  if [[ "$operation" = recover ]]; then
    if path_is_absent "$HOST_TOOLS_TRANSACTION"; then
      if [[ "$HOST_TOOLS_PREPARATION_WAS_RECOVERED" = true ]]; then
        echo 'interrupted host-tool preparation rolled back'
        exit 0
      fi
      if [[ "$HOST_TOOLS_RETIREMENT_WAS_PENDING" = true ]]; then
        echo 'interrupted host-tool transaction retirement completed'
        exit 0
      fi
      echo 'no host-tool transaction exists' >&2
      exit 1
    fi
    recover_host_tools_transaction
    echo 'interrupted host-tool upgrade rolled back'
    exit 0
  fi

  path_is_absent "$HOST_TOOLS_TRANSACTION" || {
    echo 'a host-tool transaction already exists; recover it first' >&2
    exit 1
  }
  validate_installed_bootstrap_host
  HOST_TOOL_POLICY_SOURCE="$(mktemp /etc/legal-mcp/.publisher-sudoers-new.XXXXXX)"
  render_publisher_sudoers "$HOST_DEPLOY_SHA256" "$HOST_TOOL_POLICY_SOURCE"
  policy_sha256="$(sha256sum "$HOST_TOOL_POLICY_SOURCE" | awk '{print $1}')"
  HOST_TOOL_MARKER_SOURCE="$(mktemp /etc/legal-mcp/.host-tools-new.XXXXXX)"
  render_host_tools_marker "$policy_sha256" "$HOST_TOOL_MARKER_SOURCE"

  if ! path_is_absent "$HOST_TOOLS_MARKER"; then
    require_regular_file "$HOST_TOOLS_MARKER" root root 444
    require_exact_acl "$HOST_TOOLS_MARKER" $'user::r--\ngroup::r--\nother::r--'
  fi
  if [[ -f "$HOST_TOOLS_MARKER" && ! -L "$HOST_TOOLS_MARKER" \
    && "$(sha256sum "$HOST_DEPLOY" | awk '{print $1}')" = "$HOST_DEPLOY_SHA256" \
    && "$(sha256sum "$PUBLISHER_COMMAND" | awk '{print $1}')" = "$PUBLISHER_COMMAND_SHA256" \
    && "$(sha256sum "$PUBLISHER_SUDOERS" | awk '{print $1}')" = "$policy_sha256" \
    && "$(<"$HOST_TOOLS_MARKER")" = "$(<"$HOST_TOOL_MARKER_SOURCE")" ]]; then
    current_marker_ok=true
  fi
  if [[ "$current_marker_ok" = true ]]; then
    rm -f "$HOST_TOOL_POLICY_SOURCE" "$HOST_TOOL_MARKER_SOURCE"
    echo "host publisher tools already match $expected_version ($HOST_TOOL_REVISION)"
    exit 0
  fi

  transaction_tmp="$HOST_TOOLS_BUILDING"
  install -d -o root -g root -m 0700 "$transaction_tmp"
  chown root:root "$transaction_tmp"
  chmod 700 "$transaction_tmp"
  install -o root -g root -m 0600 "$HOST_DEPLOY" "$transaction_tmp/host-deploy"
  install -o root -g root -m 0600 "$PUBLISHER_COMMAND" "$transaction_tmp/publisher-command"
  install -o root -g root -m 0600 "$PUBLISHER_SUDOERS" "$transaction_tmp/publisher-sudoers"
  printf '%s\n' LEGAL_MCP_HOST_TOOLS_TRANSACTION_V1 > "$transaction_tmp/kind"
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
  atomic_install_file "$HOST_TOOL_POLICY_SOURCE" "$PUBLISHER_SUDOERS" root root 440
  atomic_install_file "$HOST_TOOL_MARKER_SOURCE" "$HOST_TOOLS_MARKER" root root 444

  [[ "$(sha256sum "$HOST_DEPLOY" | awk '{print $1}')" = "$HOST_DEPLOY_SHA256" \
    && "$(sha256sum "$PUBLISHER_COMMAND" | awk '{print $1}')" = "$PUBLISHER_COMMAND_SHA256" \
    && "$(sha256sum "$PUBLISHER_SUDOERS" | awk '{print $1}')" = "$policy_sha256" ]]
  cmp --silent "$HOST_TOOL_MARKER_SOURCE" "$HOST_TOOLS_MARKER"
  visudo -cf "$PUBLISHER_SUDOERS" >/dev/null
  validate_installed_bootstrap_host
  trap - ERR HUP INT TERM EXIT
  rm -f "$HOST_TOOL_POLICY_SOURCE" "$HOST_TOOL_MARKER_SOURCE"
  retire_host_tools_transaction
  echo "host publisher tools upgraded to $expected_version ($HOST_TOOL_REVISION)"
  exit 0
}

if [[ "${1:-}" = --upgrade-host-tools || "${1:-}" = --recover-host-tools ]]; then
  [[ $# -eq 3 && "$2" = --version ]] || usage
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
  acl ca-certificates curl podman python3 rsync sudo ufw unattended-upgrades xfsprogs
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
install -o root -g root -m 0755 "$REPO_DIR/infra/hosting/configure-auth.sh" /usr/local/sbin/legal-mcp-configure-auth
install -o root -g root -m 0755 "$REPO_DIR/infra/hosting/update-image.sh" /usr/local/sbin/legal-mcp-update-image
install -o root -g root -m 0644 "$REPO_DIR/infra/hosting/legal-mcp.container.template" \
  /usr/local/libexec/legal-mcp/legal-mcp.container.template
printf '%s\n' "$IMAGE" > /etc/legal-mcp/image
chown root:root /etc/legal-mcp/image
chmod 600 /etc/legal-mcp/image
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
