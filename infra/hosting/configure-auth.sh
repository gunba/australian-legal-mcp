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
usage: sudo legal-mcp-configure-auth \
  --mode api-key|entra|entra+api-key \
  --public-host legal.example.com \
  [--api-key-file /root/api-key-verifiers.json] \
  [--tenant-id UUID --server-app-id UUID --audiences CSV \
   --scope legal.read --scope-uri api://UUID/legal.read \
   --allowed-client-ids CSV]

For modes containing api-key, stream the plaintext probe key only on standard
input. To roll back an interrupted transaction, run the same stable launcher:
  sudo legal-mcp-configure-auth --recover
If the saved prior mode contains api-key, stream a still-valid prior key to the
recovery command. Plaintext keys are never written to the journal or logs.
EOF
  exit 2
}

[[ $EUID -eq 0 ]] || { echo 'run configure-auth as root' >&2; exit 2; }

LOCK_FILE=/run/lock/legal-mcp-host-transaction.lock
[[ -f "$LOCK_FILE" && ! -L "$LOCK_FILE" \
  && "$(stat -c '%U:%G:%a:%h' "$LOCK_FILE")" = root:legal-mcp-publisher:640:1 ]] || {
  echo 'host transaction lock is missing or unsafe' >&2
  exit 1
}
# The stable launcher locks before selecting an immutable implementation. Keep
# its open file description when it uses the documented inherited descriptor;
# otherwise acquire the same lock directly (release-bundle legacy recovery).
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
PREPARE_AUTH_DISPATCH=false
FINALIZE_AUTH_READY=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --recover) RECOVER=true; shift ;;
    --prepare-auth-dispatch) PREPARE_AUTH_DISPATCH=true; shift ;;
    --finalize-auth-ready) FINALIZE_AUTH_READY=true; shift ;;
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

EXPECTED_HOST_TOOL_VERSION=0.20.0
HOST_TOOLS_MARKER=/etc/legal-mcp/host-tools
IMAGE_FILE=/etc/legal-mcp/image
RUNTIME_ENV=/etc/legal-mcp/runtime.env
API_KEYS=/etc/legal-mcp/api-keys.json
AUTH_READY=/etc/legal-mcp/auth-ready
AUTH_PERMIT=/run/legal-mcp/auth-configuring
HOST_TOOL_LAUNCHER=/usr/local/libexec/legal-mcp/host-tool-launcher
HOST_TOOL_LAUNCHER_MARKER=/etc/legal-mcp/host-tool-launcher
CONFIGURE_AUTH_POINTER=/etc/legal-mcp/configure-auth-implementation
UPDATE_IMAGE_POINTER=/etc/legal-mcp/update-image-implementation
HOST_TOOL_IMPLEMENTATION_DIR=/usr/local/libexec/legal-mcp/host-tools
HOST_TOOL_DISPATCH=/run/legal-mcp/host-tool-launcher-dispatch
TEMPLATE=/usr/local/libexec/legal-mcp/legal-mcp.container.template
QUADLET=/etc/containers/systemd/legal-mcp.container
CADDYFILE=/etc/caddy/Caddyfile
ACTIVE_GENERATION=/srv/legal-mcp/lifecycle/active-generation
SERVICE=legal-mcp.service
TRANSACTION=/etc/legal-mcp/.auth-transaction
TRANSACTION_PREPARING=${TRANSACTION}.preparing
TRANSACTION_PREPARING_RETIRED=${TRANSACTION}.preparing-retired
TRANSACTION_RETIRING=${TRANSACTION}.retiring
TRANSACTION_RETIRED=${TRANSACTION}.retired
LEGACY_PREPARING_RETIRING=${TRANSACTION}.legacy-v0192-preparing-retiring
LEGACY_PREPARING_RETIRED=${TRANSACTION}.legacy-v0192-preparing-retired
CURRENT_IMPLEMENTATION="$(readlink -f "${BASH_SOURCE[0]}")"

hidden_modes=0
[[ "$RECOVER" = false ]] || ((hidden_modes += 1))
[[ "$PREPARE_AUTH_DISPATCH" = false ]] || ((hidden_modes += 1))
[[ "$FINALIZE_AUTH_READY" = false ]] || ((hidden_modes += 1))
[[ "$hidden_modes" -le 1 ]] || usage

path_is_absent() {
  [[ ! -e "$1" && ! -L "$1" ]]
}

require_regular_file() {
  local path="$1" owner="$2" group="$3" mode="$4"
  [[ -f "$path" && ! -L "$path" \
    && "$(stat -c '%U:%G:%a:%h' "$path")" = "$owner:$group:$mode:1" ]] || {
    echo "unsafe host file: $path" >&2
    return 1
  }
}

require_directory() {
  local path="$1" owner="$2" group="$3" mode="$4"
  [[ -d "$path" && ! -L "$path" \
    && "$(stat -c '%U:%G:%a' "$path")" = "$owner:$group:$mode" ]] || {
    echo "unsafe host directory: $path" >&2
    return 1
  }
}

require_transaction_directory() {
  require_directory "$1" root root 700 || return 1
  [[ "$(getfacl --absolute-names --numeric --omit-header "$1")" \
    = $'user::rwx\ngroup::---\nother::---' ]]
}

require_transaction_file() {
  require_regular_file "$1" root root 600 || return 1
  [[ "$(getfacl --absolute-names --numeric --omit-header "$1")" \
    = $'user::rw-\ngroup::---\nother::---' ]]
}

require_auth_ready_marker() {
  require_regular_file "$AUTH_READY" root root 444 || return 1
  [[ "$(stat -c '%s' "$AUTH_READY")" = 0 \
    && "$(getfacl --absolute-names --numeric --omit-header "$AUTH_READY")" \
      = $'user::r--\ngroup::r--\nother::r--' ]]
}

require_auth_ready_state() {
  local expected="$1"
  case "$expected" in
    absent) path_is_absent "$AUTH_READY" ;;
    present) require_auth_ready_marker ;;
    *) return 1 ;;
  esac
}

directory_contains_only() {
  local directory="$1" found name
  local -a exclusions=()
  shift
  for name in "$@"; do exclusions+=('!' -name "$name"); done
  found="$(find "$directory" -mindepth 1 -maxdepth 1 \
    "${exclusions[@]}" -printf x -quit)" || {
    echo "could not inspect directory contents: $directory" >&2
    return 1
  }
  [[ -z "$found" ]]
}

atomic_install_file() {
  local source="$1" destination="$2" owner="$3" group="$4" mode="$5" temporary
  temporary="$(mktemp "$(dirname "$destination")/.$(basename "$destination").XXXXXX")"
  install -o "$owner" -g "$group" -m "$mode" "$source" "$temporary"
  sync -f "$temporary"
  mv -fT "$temporary" "$destination"
  sync -f "$(dirname "$destination")"
}


read_systemctl_enablement() {
  local unit="$1" output status
  if output="$(systemctl is-enabled "$unit" 2>/dev/null)"; then status=0; else status=$?; fi
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
  if output="$(systemctl is-active "$unit" 2>/dev/null)"; then status=0; else status=$?; fi
  case "$status:$output" in
    0:active|3:inactive) printf '%s\n' "$output" ;;
    *)
      echo "could not determine exact systemd activity for $unit (status $status, state ${output:-<empty>})" >&2
      return 1
      ;;
  esac
}

require_legal_service_state() {
  local expected_activity="$1" enablement activity
  enablement="$(read_systemctl_enablement "$SERVICE")" || return 1
  activity="$(read_systemctl_activity "$SERVICE")" || return 1
  [[ "$enablement" = generated && "$activity" = "$expected_activity" ]] || {
    echo "$SERVICE must be generated/$expected_activity, found $enablement/$activity" >&2
    return 1
  }
}

require_caddy_state() {
  local expected_enablement="$1" expected_activity="$2" enablement activity
  enablement="$(read_systemctl_enablement caddy.service)" || return 1
  activity="$(read_systemctl_activity caddy.service)" || return 1
  [[ "$enablement" = "$expected_enablement" && "$activity" = "$expected_activity" ]] || {
    echo "caddy.service must be $expected_enablement/$expected_activity, found $enablement/$activity" >&2
    return 1
  }
}

read_ufw_report() {
  local status
  if UFW_REPORT="$(ufw status verbose)"; then return 0; else status=$?; fi
  echo "could not inspect the UFW allowlist (status $status)" >&2
  return 1
}

require_ufw_state() {
  local expected="$1" admin_source
  require_regular_file /etc/legal-mcp/admin-source-ip root root 600 || return 1
  admin_source="$(</etc/legal-mcp/admin-source-ip)"
  [[ "$admin_source" =~ ^[0-9A-Fa-f:.]{2,45}$ \
    && "$(python3 - "$admin_source" <<'PY'
import ipaddress, sys
print(ipaddress.ip_address(sys.argv[1]).compressed)
PY
)" = "$admin_source" ]] || return 1
  read_ufw_report || return 1
  printf '%s\n' "$UFW_REPORT" | python3 /dev/fd/3 "$admin_source" "$expected" 3<<'PY'
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
allowed_targets = {"80/tcp", "80/tcp (v6)", "443/tcp", "443/tcp (v6)"}
if any(target not in allowed_targets or not source.startswith("Anywhere") for target, source in web):
    raise SystemExit(1)
if len({target for target, _ in web}) != len(web):
    raise SystemExit(1)
ports = {target.split("/", 1)[0] for target, _ in web}
if expected == "closed":
    if web:
        raise SystemExit(1)
elif expected == "open":
    if ports != {"80", "443"}:
        raise SystemExit(1)
else:
    raise SystemExit(1)
PY
}

ufw_has_web_rule() {
  local port="$1"
  read_ufw_report || return 1
  grep -Eq "^${port}/tcp( \(v6\))?[[:space:]]+ALLOW IN([[:space:]]|$)" <<< "$UFW_REPORT"
}

require_listener_topology() {
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
    local = fields[3]
    match = re.fullmatch(r"(.+):([0-9]+)", local)
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

close_public_ingress() {
  local port comment failed=false
  systemctl disable --now caddy.service >/dev/null 2>&1 || failed=true
  for port in 80 443; do
    if ufw_has_web_rule "$port"; then
      case "$port" in
        80) comment='Caddy ACME HTTP' ;;
        443) comment='Australian Legal MCP HTTPS' ;;
        *) failed=true; continue ;;
      esac
      ufw --force delete allow "$port/tcp" comment "$comment" >/dev/null 2>&1 || failed=true
    fi
  done
  require_caddy_state disabled inactive || failed=true
  require_ufw_state closed || failed=true
  [[ "$failed" = false ]]
}

force_everything_off() {
  local failed=false
  close_public_ingress || failed=true
  systemctl stop "$SERVICE" >/dev/null 2>&1 || failed=true
  path_is_absent "$AUTH_READY" || failed=true
  systemctl daemon-reload >/dev/null 2>&1 || failed=true
  require_legal_service_state inactive || failed=true
  require_caddy_state disabled inactive || failed=true
  require_ufw_state closed || failed=true
  require_listener_topology none || failed=true
  [[ "$failed" = false ]]
}

process_start_time() {
  python3 - "$1" <<'PY'
import pathlib, sys
value = pathlib.Path(f"/proc/{sys.argv[1]}/stat").read_text()
fields = value.rpartition(") ")[2].split()
if len(fields) < 20 or not fields[19].isdigit():
    raise SystemExit(1)
print(fields[19])
PY
}

validate_launcher_context() {
  local expected_configure_sha="$1" expected_update_sha="$2"
  local launcher_sha configure_sha update_sha permit_pid permit_start actual_start
  local uid_line cmdline name
  local -a launcher_marker
  require_regular_file "$HOST_TOOL_LAUNCHER_MARKER" root root 444 || return 1
  mapfile -t launcher_marker < "$HOST_TOOL_LAUNCHER_MARKER"
  [[ ${#launcher_marker[@]} -eq 2 \
    && "${launcher_marker[0]}" = LEGAL_MCP_HOST_TOOL_LAUNCHER_V1 \
    && "${launcher_marker[1]}" =~ ^LAUNCHER_SHA256=([0-9a-f]{64})$ ]] || {
    echo 'stable host-tool launcher marker is malformed' >&2
    return 1
  }
  launcher_sha="${BASH_REMATCH[1]}"
  require_regular_file "$HOST_TOOL_LAUNCHER" root root 755 || return 1
  [[ "$(sha256sum "$HOST_TOOL_LAUNCHER" | awk '{print $1}')" = "$launcher_sha" ]] || return 1

  for name in "$CONFIGURE_AUTH_POINTER" "$UPDATE_IMAGE_POINTER"; do
    require_regular_file "$name" root root 644 || return 1
    [[ "$(stat -c '%s' "$name")" = 64 ]] || {
      echo "host-tool implementation pointer must be exactly 64 bytes: $name" >&2
      return 1
    }
  done
  configure_sha="$(<"$CONFIGURE_AUTH_POINTER")"
  update_sha="$(<"$UPDATE_IMAGE_POINTER")"
  [[ "$configure_sha" = "$expected_configure_sha" \
    && "$update_sha" = "$expected_update_sha" \
    && "$configure_sha$update_sha" =~ ^[0-9a-f]{128}$ ]] || {
    echo 'V2 marker and immutable implementation pointers do not agree' >&2
    return 1
  }
  require_regular_file "$HOST_TOOL_IMPLEMENTATION_DIR/configure-auth.$configure_sha" root root 755 || return 1
  require_regular_file "$HOST_TOOL_IMPLEMENTATION_DIR/update-image.$update_sha" root root 755 || return 1
  require_regular_file "$CURRENT_IMPLEMENTATION" root root 755 || return 1
  require_regular_file /usr/local/sbin/legal-mcp-update-image root root 755 || return 1
  [[ "$(sha256sum "$HOST_TOOL_IMPLEMENTATION_DIR/configure-auth.$configure_sha" | awk '{print $1}')" = "$configure_sha" \
    && "$(sha256sum "$HOST_TOOL_IMPLEMENTATION_DIR/update-image.$update_sha" | awk '{print $1}')" = "$update_sha" \
    && "$(sha256sum "$CURRENT_IMPLEMENTATION" | awk '{print $1}')" = "$configure_sha" \
    && "$(sha256sum /usr/local/sbin/legal-mcp-update-image | awk '{print $1}')" = "$update_sha" ]] || {
    echo 'the running bind-mounted helper is not the immutable pointer-selected implementation' >&2
    return 1
  }

  require_regular_file "$AUTH_PERMIT" root root 400 || return 1
  read -r permit_pid permit_start < "$AUTH_PERMIT" || return 1
  [[ "$permit_pid" =~ ^[1-9][0-9]*$ && "$permit_start" =~ ^[1-9][0-9]*$ \
    && "$(wc -w < "$AUTH_PERMIT")" = 2 ]] || return 1
  actual_start="$(process_start_time "$permit_pid" 2>/dev/null)" || return 1
  [[ "$actual_start" = "$permit_start" ]] || return 1
  uid_line="$(awk '$1 == "Uid:" {print $2 ":" $3 ":" $4 ":" $5}' "/proc/$permit_pid/status")" || return 1
  [[ "$uid_line" = 0:0:0:0 ]] || return 1
  cmdline="$(tr '\0' '\n' < "/proc/$permit_pid/cmdline")" || return 1
  if ! grep -Fxq -- '--legal-mcp-launcher-internal' <<< "$cmdline" \
    || ! grep -Fxq configure-auth <<< "$cmdline"; then
    echo 'authentication permit is not tied to the live stable launcher dispatch' >&2
    return 1
  fi

  require_directory "$HOST_TOOL_DISPATCH" root root 700 || return 1
  for name in pid start-time role configure-auth update-image; do
    require_regular_file "$HOST_TOOL_DISPATCH/$name" root root 600 || return 1
  done
  directory_contains_only "$HOST_TOOL_DISPATCH" pid start-time role configure-auth update-image || return 1
  [[ "$(<"$HOST_TOOL_DISPATCH/pid")" = "$permit_pid" \
    && "$(<"$HOST_TOOL_DISPATCH/start-time")" = "$permit_start" \
    && "$(<"$HOST_TOOL_DISPATCH/role")" = configure-auth \
    && "$(<"$HOST_TOOL_DISPATCH/configure-auth")" = "$configure_sha" \
    && "$(<"$HOST_TOOL_DISPATCH/update-image")" = "$update_sha" ]] || {
    echo 'stable launcher dispatch does not match the running immutable implementation' >&2
    return 1
  }
}

validate_host_tools_v2() {
  local configure_sha image template_sha update_sha deploy_sha publisher_sha sudoers_sha rendered
  local -a marker image_lines
  require_regular_file "$HOST_TOOLS_MARKER" root root 444 || return 1
  mapfile -t marker < "$HOST_TOOLS_MARKER"
  [[ ${#marker[@]} -eq 9 \
    && "${marker[0]}" = LEGAL_MCP_HOST_TOOLS_V2 \
    && "${marker[1]}" = "VERSION=$EXPECTED_HOST_TOOL_VERSION" \
    && "${marker[2]}" =~ ^SOURCE_COMMIT=[0-9a-f]{40}$ \
    && "${marker[3]}" =~ ^HOST_DEPLOY_SHA256=([0-9a-f]{64})$ ]] || {
    echo 'installed V2 host-tool marker is not the exact v0.20.0 contract' >&2
    return 1
  }
  deploy_sha="${BASH_REMATCH[1]}"
  [[ "${marker[4]}" =~ ^PUBLISHER_COMMAND_SHA256=([0-9a-f]{64})$ ]] || return 1
  publisher_sha="${BASH_REMATCH[1]}"
  [[ "${marker[5]}" =~ ^CONFIGURE_AUTH_SHA256=([0-9a-f]{64})$ ]] || return 1
  configure_sha="${BASH_REMATCH[1]}"
  [[ "${marker[6]}" =~ ^UPDATE_IMAGE_SHA256=([0-9a-f]{64})$ ]] || return 1
  update_sha="${BASH_REMATCH[1]}"
  [[ "${marker[7]}" =~ ^CONTAINER_TEMPLATE_SHA256=([0-9a-f]{64})$ ]] || return 1
  template_sha="${BASH_REMATCH[1]}"
  [[ "${marker[8]}" =~ ^SUDOERS_SHA256=([0-9a-f]{64})$ ]] || return 1
  sudoers_sha="${BASH_REMATCH[1]}"

  validate_launcher_context "$configure_sha" "$update_sha" || return 1
  require_regular_file /usr/local/sbin/legal-mcp-host-deploy root root 755 || return 1
  require_regular_file /usr/local/sbin/legal-mcp-publisher-command root root 755 || return 1
  require_regular_file "$TEMPLATE" root root 644 || return 1
  require_regular_file "$QUADLET" root root 644 || return 1
  require_regular_file /etc/sudoers.d/legal-mcp-publisher root root 440 || return 1
  [[ "$(sha256sum "$CURRENT_IMPLEMENTATION" | awk '{print $1}')" = "$configure_sha" \
    && "$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}')" = "$deploy_sha" \
    && "$(sha256sum /usr/local/sbin/legal-mcp-publisher-command | awk '{print $1}')" = "$publisher_sha" \
    && "$(sha256sum "$TEMPLATE" | awk '{print $1}')" = "$template_sha" \
    && "$(sha256sum /etc/sudoers.d/legal-mcp-publisher | awk '{print $1}')" = "$sudoers_sha" ]] || {
    echo 'installed immutable host-tool bytes do not match the V2 marker' >&2
    return 1
  }
  [[ "$(grep -o '__IMAGE_DIGEST__' "$TEMPLATE" | wc -l)" = 1 \
    && "$(grep -Fxc 'ExecCondition=/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready' "$TEMPLATE")" = 1 \
    && "$(grep -Fxc 'PublishPort=127.0.0.1:51235:51235' "$TEMPLATE")" = 1 ]] || {
    echo 'installed Quadlet template lacks the exact launcher auth gate or loopback publication' >&2
    return 1
  }
  require_regular_file "$IMAGE_FILE" root root 600 || return 1
  mapfile -t image_lines < "$IMAGE_FILE"
  [[ ${#image_lines[@]} -eq 1 \
    && "${image_lines[0]}" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ ]] || {
    echo 'installed image pin is malformed' >&2
    return 1
  }
  image="${image_lines[0]}"
  rendered="$(mktemp /run/legal-mcp-auth-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$image|g" "$TEMPLATE" > "$rendered"
  if ! cmp --silent "$rendered" "$QUADLET"; then
    rm -f "$rendered"
    echo 'installed image, immutable template, and rendered Quadlet do not agree' >&2
    return 1
  fi
  rm -f "$rendered"
}

validate_active_generation_pointer() {
  require_regular_file "$ACTIVE_GENERATION" root root 644 || return 1
  [[ "$(stat -c '%s' "$ACTIVE_GENERATION")" = 64 ]] || {
    echo 'active-generation must be exactly 64 bytes without a trailing newline' >&2
    return 1
  }
  EXPECTED_GENERATION="$(<"$ACTIVE_GENERATION")"
  [[ "$EXPECTED_GENERATION" =~ ^[0-9a-f]{64}$ \
    && -d "/srv/legal-mcp/generations/$EXPECTED_GENERATION" \
    && ! -L "/srv/legal-mcp/generations/$EXPECTED_GENERATION" ]] || {
    echo 'active generation pointer is invalid' >&2
    return 1
  }
}

validate_caddy_contract() {
  local host="$1" adapted
  require_regular_file "$CADDYFILE" root caddy 640 || return 1
  adapted="$(mktemp /run/legal-mcp-caddy-adapted.XXXXXX)"
  if ! caddy adapt --config "$CADDYFILE" --adapter caddyfile --validate > "$adapted"; then
    rm -f "$adapted"
    echo 'Caddyfile validation/adaptation failed' >&2
    return 1
  fi
  if ! python3 - "$adapted" "$host" <<'PY'
import json, sys
path, host = sys.argv[1:]
actual = json.load(open(path, encoding="utf-8"))
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

validate_runtime_contract() {
  local path="$1" expected_host="${2:-}" output
  output="$(python3 - "$path" "$expected_host" <<'PY'
import pathlib, re, sys
path = pathlib.Path(sys.argv[1]); expected_host = sys.argv[2]
raw = path.read_bytes()
if not raw or len(raw) > 65536 or b"\x00" in raw or not raw.endswith(b"\n"):
    raise SystemExit(1)
try:
    lines = raw.decode("ascii").splitlines()
except UnicodeDecodeError:
    raise SystemExit(1)
pairs = []
for line in lines:
    if not line or "=" not in line:
        raise SystemExit(1)
    key, value = line.split("=", 1)
    pairs.append((key, value))
if len({key for key, _ in pairs}) != len(pairs):
    raise SystemExit(1)
values = dict(pairs)
mode = values.get("LEGAL_MCP_HTTP_AUTH")
base = [
    "LEGAL_MCP_HTTP_AUTH", "LEGAL_MCP_API_KEYS_FILE", "LEGAL_MCP_EXTERNAL_URL",
    "LEGAL_MCP_ALLOWED_ORIGINS", "LEGAL_MCP_HTTP_WORKERS",
    "LEGAL_MCP_SHUTDOWN_GRACE_SECONDS",
]
entra = [
    "LEGAL_MCP_ENTRA_TENANT_ID", "LEGAL_MCP_ENTRA_SERVER_APP_ID",
    "LEGAL_MCP_ENTRA_AUDIENCES", "LEGAL_MCP_ENTRA_SCOPE",
    "LEGAL_MCP_ENTRA_SCOPE_URI", "LEGAL_MCP_ENTRA_ALLOWED_CLIENT_IDS",
]
expected_keys = base + (entra if mode in ("entra", "entra+api-key") else [])
if [key for key, _ in pairs] != expected_keys or mode not in ("disabled", "api-key", "entra", "entra+api-key"):
    raise SystemExit(1)
if values["LEGAL_MCP_API_KEYS_FILE"] != "/run/secrets/legal-mcp-api-keys.json":
    raise SystemExit(1)
url = values["LEGAL_MCP_EXTERNAL_URL"]
match = re.fullmatch(r"https://([a-z0-9.-]+)/mcp", url)
if not match or values["LEGAL_MCP_ALLOWED_ORIGINS"] != f"https://{match.group(1)}":
    raise SystemExit(1)
if expected_host and match.group(1) != expected_host:
    raise SystemExit(1)
if values["LEGAL_MCP_HTTP_WORKERS"] != "4" or values["LEGAL_MCP_SHUTDOWN_GRACE_SECONDS"] != "30":
    raise SystemExit(1)
if mode in ("entra", "entra+api-key"):
    uuid = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
    tenant = values["LEGAL_MCP_ENTRA_TENANT_ID"]
    app = values["LEGAL_MCP_ENTRA_SERVER_APP_ID"]
    scope = values["LEGAL_MCP_ENTRA_SCOPE"]
    if not uuid.fullmatch(tenant) or not uuid.fullmatch(app) or not re.fullmatch(r"[A-Za-z0-9._-]{1,128}", scope):
        raise SystemExit(1)
    if values["LEGAL_MCP_ENTRA_SCOPE_URI"] != f"api://{app}/{scope}":
        raise SystemExit(1)
    audiences = values["LEGAL_MCP_ENTRA_AUDIENCES"].split(",")
    clients = values["LEGAL_MCP_ENTRA_ALLOWED_CLIENT_IDS"].split(",")
    if not audiences or len(audiences) != len(set(audiences)) or not clients or len(clients) != len(set(clients)):
        raise SystemExit(1)
    if app not in audiences and f"api://{app}" not in audiences:
        raise SystemExit(1)
    if any(not uuid.fullmatch(value) for value in clients):
        raise SystemExit(1)
print(mode)
print(url)
PY
)" || {
    echo "runtime authentication contract is malformed: $path" >&2
    return 1
  }
  mapfile -t RUNTIME_VALUES <<< "$output"
  [[ ${#RUNTIME_VALUES[@]} -eq 2 ]] || return 1
  VALIDATED_AUTH_MODE="${RUNTIME_VALUES[0]}"
  VALIDATED_EXTERNAL_URL="${RUNTIME_VALUES[1]}"
}

validate_api_key_document() {
  local path="$1" require_nonempty="$2" expected_owner="$3" expected_group="$4" expected_mode="$5"
  require_regular_file "$path" "$expected_owner" "$expected_group" "$expected_mode" || return 1
  python3 - "$path" "$require_nonempty" <<'PY'
import json, pathlib, re, sys
path = pathlib.Path(sys.argv[1]); require_nonempty = sys.argv[2] == "true"
raw = path.read_bytes()
if not 0 < len(raw) <= 65536:
    raise SystemExit(1)
try:
    value = json.loads(raw)
except (UnicodeDecodeError, json.JSONDecodeError):
    raise SystemExit(1)
if not isinstance(value, dict) or set(value) != {"version", "keys"} or value["version"] != 1:
    raise SystemExit(1)
keys = value["keys"]
if not isinstance(keys, list) or len(keys) > 32 or (require_nonempty and not keys):
    raise SystemExit(1)
seen_ids, seen_hashes = set(), set()
for item in keys:
    if not isinstance(item, dict) or set(item) != {"id", "sha256"}:
        raise SystemExit(1)
    key_id, digest = item["id"], item["sha256"]
    if not isinstance(key_id, str) or not re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,63}", key_id):
        raise SystemExit(1)
    if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise SystemExit(1)
    if key_id in seen_ids or digest in seen_hashes:
        raise SystemExit(1)
    seen_ids.add(key_id); seen_hashes.add(digest)
if [item["id"] for item in keys] != sorted(seen_ids):
    raise SystemExit(1)
canonical = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
if raw != canonical:
    raise SystemExit(1)
PY
}

validate_probe_key_for_document() {
  local path="$1"
  [[ "$PROBE_API_KEY" =~ ^[a-z0-9][a-z0-9_-]{0,63}\.[A-Za-z0-9_-]{43}$ ]] || {
    echo 'probe API key has an invalid shape' >&2
    return 1
  }
  printf '%s' "$PROBE_API_KEY" | python3 /dev/fd/3 "$path" 3<<'PY'
import hashlib, json, re, sys
key = sys.stdin.read(); value = json.load(open(sys.argv[1], encoding="utf-8"))
key_id = key.split(".", 1)[0]
if not re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,63}\.[A-Za-z0-9_-]{43}", key):
    raise SystemExit(1)
expected = hashlib.sha256(key.encode()).hexdigest()
if not any(item == {"id": key_id, "sha256": expected} for item in value["keys"]):
    raise SystemExit(1)
PY
}

validate_live_auth_files() {
  local expected_host="${1:-}"
  require_regular_file "$RUNTIME_ENV" root root 600 || return 1
  require_regular_file "$API_KEYS" legal-mcp legal-mcp 400 || return 1
  validate_runtime_contract "$RUNTIME_ENV" "$expected_host" || return 1
  if [[ "$VALIDATED_AUTH_MODE" == *api-key* ]]; then
    validate_api_key_document "$API_KEYS" true legal-mcp legal-mcp 400 || return 1
  else
    validate_api_key_document "$API_KEYS" false legal-mcp legal-mcp 400 || return 1
    [[ "$(<"$API_KEYS")" = '{"keys":[],"version":1}' ]] || {
      echo 'non-API-key mode requires the exact empty verifier document' >&2
      return 1
    }
  fi
}

validate_foreign_transactions_absent() {
  local found
  found="$(find /etc/legal-mcp -mindepth 1 -maxdepth 1 \
    \( -name '.host-tools-transaction*' -o -name '.image-transaction*' \) \
    -printf '%f\n' -quit)" || return 1
  [[ -z "$found" ]] || {
    echo "a foreign host transaction must be recovered first: $found" >&2
    return 1
  }
  for path in /srv/legal-mcp/lifecycle/.deployment-transaction \
    /srv/legal-mcp/lifecycle/.deployment-transaction.preparing; do
    path_is_absent "$path" || {
      echo 'a corpus transaction must be recovered before changing authentication' >&2
      return 1
    }
  done
}

validate_static_v2_host() {
  local host="$1"
  validate_host_tools_v2 || return 1
  validate_foreign_transactions_absent || return 1
  validate_active_generation_pointer || return 1
  validate_caddy_contract "$host" || return 1
}

wait_for_generation() {
  local expected="$1" deadline=$((SECONDS + 600))
  while (( SECONDS < deadline )); do
    if curl --fail --silent --max-time 5 http://127.0.0.1:51235/readyz 2>/dev/null |
      python3 -c 'import json,sys; value=json.load(sys.stdin); raise SystemExit(0 if value == {"status":"ok","generation":sys.argv[1]} else 1)' \
        "$expected" 2>/dev/null; then
      return 0
    fi
    [[ "$(read_systemctl_activity "$SERVICE")" = active ]] || return 1
    sleep 1
  done
  return 1
}

probe_api_key() {
  local url="$1" headers response status
  headers="$(mktemp /run/legal-mcp-api-headers.XXXXXX)"
  response="$(mktemp /run/legal-mcp-api-response.XXXXXX)"
  status="$(printf 'header = "X-API-Key: %s"\n' "$PROBE_API_KEY" |
    curl --config - --silent --show-error --dump-header "$headers" \
      --output "$response" --write-out '%{http_code}' --fail \
      --max-time 20 --max-redirs 0 --request POST \
      --header 'Accept: application/json, text/event-stream' \
      --header 'Content-Type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
      "$url")" || {
    rm -f "$headers" "$response"
    return 1
  }
  if [[ "$status" != 200 ]] || grep -Eiq '^Location:' "$headers" \
    || ! python3 - "$response" <<'PY'
import json, sys
value=json.load(open(sys.argv[1], encoding="utf-8"))
if value.get("result",{}).get("serverInfo",{}).get("name") != "australian-legal-mcp":
    raise SystemExit(1)
PY
  then
    rm -f "$headers" "$response"
    return 1
  fi
  rm -f "$headers" "$response"
}

probe_auth_boundary() {
  local mcp_url="$1" metadata_url="$2" mode="$3" external_url="$4"
  local require_positive_api="${5:-true}" headers status
  local has_api=false has_entra=false
  [[ "$mode" == *api-key* ]] && has_api=true
  [[ "$mode" == *entra* ]] && has_entra=true
  headers="$(mktemp /run/legal-mcp-auth-headers.XXXXXX)"
  status="$(curl --silent --show-error --dump-header "$headers" --output /dev/null \
    --write-out '%{http_code}' --max-time 20 --max-redirs 0 --request POST \
    --header 'Accept: application/json, text/event-stream' \
    --header 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    "$mcp_url" 2>/dev/null || true)"
  if [[ "$status" != 401 ]] \
    || ! grep -Eiq '^WWW-Authenticate:' "$headers" \
    || grep -Eiq '^Location:' "$headers" \
    || { [[ "$has_api" = true ]] && ! grep -Eiq '^WWW-Authenticate:.*ApiKey realm=' "$headers"; } \
    || { [[ "$has_entra" = true ]] && ! grep -Eiq '^WWW-Authenticate:.*Bearer resource_metadata=' "$headers"; }; then
    rm -f "$headers"
    echo "authentication boundary failed: $mcp_url" >&2
    return 1
  fi
  rm -f "$headers"
  if [[ "$has_entra" = true ]]; then
    curl --fail --silent --show-error --max-time 20 --max-redirs 0 "$metadata_url" |
      python3 -c 'import json,sys; value=json.load(sys.stdin); raise SystemExit(0 if value.get("resource") == sys.argv[1] else 1)' \
        "$external_url" || return 1
  fi
  if [[ "$has_api" = true && "$require_positive_api" = true ]]; then
    probe_api_key "$mcp_url" || return 1
  fi
}

wait_for_public_auth_boundary() {
  local mcp_url="$1" metadata_url="$2" mode="$3" external_url="$4"
  local require_positive_api="${5:-true}" deadline=$((SECONDS + 180))
  while (( SECONDS < deadline )); do
    if probe_auth_boundary "$mcp_url" "$metadata_url" "$mode" "$external_url" \
      "$require_positive_api" 2>/dev/null; then
      return 0
    fi
    require_caddy_state enabled active || return 1
    require_listener_topology public || return 1
    require_ufw_state open || return 1
    sleep 2
  done
  echo "public authentication boundary did not become ready: $mcp_url" >&2
  return 1
}

probe_negative_public_routes() {
  local origin="$1" path method status headers
  for path in / /mcp/ /.well-known/oauth-protected-resource \
    /.well-known/oauth-protected-resource/mcp/ /readyz /livez; do
    method=GET
    [[ "$path" = /mcp/ ]] && method=POST
    headers="$(mktemp /run/legal-mcp-negative-headers.XXXXXX)"
    status="$(curl --silent --show-error --dump-header "$headers" --output /dev/null \
      --write-out '%{http_code}' --max-time 20 --max-redirs 0 --request "$method" \
      "$origin$path" 2>/dev/null || true)"
    if [[ "$status" != 404 ]] || grep -Eiq '^Location:' "$headers"; then
      rm -f "$headers"
      echo "unexpected public route or redirect: $path" >&2
      return 1
    fi
    rm -f "$headers"
  done
  headers="$(mktemp /run/legal-mcp-negative-headers.XXXXXX)"
  status="$(curl --silent --show-error --dump-header "$headers" --output /dev/null \
    --write-out '%{http_code}' --max-time 20 --max-redirs 0 \
    "http://${origin#https://}/mcp" 2>/dev/null || true)"
  if [[ "$status" != 404 ]] || grep -Eiq '^Location:' "$headers"; then
    rm -f "$headers"
    echo 'HTTP MCP path must be an exact non-redirecting 404' >&2
    return 1
  fi
  rm -f "$headers"
}

validate_live_state_matrix() {
  local expected_host="$1" report_error="${2:-true}" ready_state="${3:-absent}"
  local service_activity caddy_enablement caddy_activity ufw_state
  validate_live_auth_files "$expected_host" || return 1
  service_activity="$(read_systemctl_activity "$SERVICE")" || return 1
  [[ "$(read_systemctl_enablement "$SERVICE")" = generated ]] || return 1
  caddy_enablement="$(read_systemctl_enablement caddy.service)" || return 1
  caddy_activity="$(read_systemctl_activity caddy.service)" || return 1
  if require_ufw_state closed; then ufw_state=closed
  elif require_ufw_state open; then ufw_state=open
  else return 1
  fi
  if [[ "$service_activity" = inactive && "$caddy_enablement" = disabled \
    && "$caddy_activity" = inactive && "$ufw_state" = closed \
    && "$VALIDATED_AUTH_MODE" = disabled ]]; then
    [[ "$(<"$API_KEYS")" = '{"keys":[],"version":1}' ]] || return 1
    require_listener_topology none || return 1
    LIVE_BASELINE=dark
  elif [[ "$service_activity" = active && "$caddy_enablement" = enabled \
    && "$caddy_activity" = active && "$ufw_state" = open \
    && "$VALIDATED_AUTH_MODE" != disabled ]]; then
    require_listener_topology public || return 1
    LIVE_BASELINE=public
  else
    if [[ "$report_error" = true ]]; then
      echo 'service, auth-ready gate, Caddy, UFW, and listener state are not a valid dark or public matrix' >&2
    fi
    return 1
  fi
  if [[ "$ready_state" = baseline ]]; then
    if [[ "$LIVE_BASELINE" = public ]]; then ready_state=present; else ready_state=absent; fi
  fi
  require_auth_ready_state "$ready_state" || {
    [[ "$report_error" = false ]] \
      || echo 'auth-ready does not match the exact expected live-state matrix' >&2
    return 1
  }
}

validate_closed_configured_state() {
  local expected_host="$1"
  validate_live_auth_files "$expected_host" || return 1
  [[ "$VALIDATED_AUTH_MODE" != disabled \
    && "$(read_systemctl_enablement "$SERVICE")" = generated \
    && "$(read_systemctl_activity "$SERVICE")" = inactive \
    && "$(read_systemctl_enablement caddy.service)" = disabled \
    && "$(read_systemctl_activity caddy.service)" = inactive ]] || return 1
  path_is_absent "$AUTH_READY" || return 1
  require_ufw_state closed || return 1
  require_listener_topology none || return 1
  CLOSED_AUTH_MODE="$VALIDATED_AUTH_MODE"
  CLOSED_EXTERNAL_URL="$VALIDATED_EXTERNAL_URL"
}

restore_current_configured_public() {
  local host="$1"
  read_recovery_probe_if_needed "$API_KEYS" "$CLOSED_AUTH_MODE"
  systemctl restart "$SERVICE"
  wait_for_generation "$EXPECTED_GENERATION"
  require_legal_service_state active
  require_listener_topology private
  require_ufw_state closed
  probe_auth_boundary http://127.0.0.1:51235/mcp \
    http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp \
    "$CLOSED_AUTH_MODE" "$CLOSED_EXTERNAL_URL"
  systemctl enable --now caddy.service
  require_caddy_state enabled active
  require_listener_topology public
  require_ufw_state closed
  ufw allow 80/tcp comment 'Caddy ACME HTTP' >/dev/null
  ufw allow 443/tcp comment 'Australian Legal MCP HTTPS' >/dev/null
  require_ufw_state open
  require_listener_topology public
  wait_for_public_auth_boundary "$CLOSED_EXTERNAL_URL" \
    "${CLOSED_EXTERNAL_URL%/mcp}/.well-known/oauth-protected-resource/mcp" \
    "$CLOSED_AUTH_MODE" "$CLOSED_EXTERNAL_URL"
  probe_negative_public_routes "${CLOSED_EXTERNAL_URL%/mcp}"
  validate_live_state_matrix "$host"
  [[ "$LIVE_BASELINE" = public ]]
}

render_journal_hashes() {
  local directory="$1" destination="$2"
  cat > "$destination" <<EOF
KIND_SHA256=$(sha256sum "$directory/kind" | awk '{print $1}')
BASELINE_SHA256=$(sha256sum "$directory/baseline" | awk '{print $1}')
HOST_TOOLS_SHA256=$(sha256sum "$directory/host-tools" | awk '{print $1}')
ACTIVE_GENERATION_SHA256=$(sha256sum "$directory/active-generation" | awk '{print $1}')
CADDYFILE_SHA256=$(sha256sum "$directory/Caddyfile" | awk '{print $1}')
RUNTIME_ENV_SHA256=$(sha256sum "$directory/runtime.env" | awk '{print $1}')
API_KEYS_SHA256=$(sha256sum "$directory/api-keys.json" | awk '{print $1}')
EOF
  chmod 600 "$destination"
}

create_v2_journal() {
  local baseline="$1" hashes
  path_is_absent "$TRANSACTION_PREPARING" || return 1
  install -d -o root -g root -m 0700 "$TRANSACTION_PREPARING"
  install -o root -g root -m 0600 "$HOST_TOOLS_MARKER" "$TRANSACTION_PREPARING/host-tools"
  install -o root -g root -m 0600 "$ACTIVE_GENERATION" "$TRANSACTION_PREPARING/active-generation"
  install -o root -g root -m 0600 "$CADDYFILE" "$TRANSACTION_PREPARING/Caddyfile"
  install -o root -g root -m 0600 "$RUNTIME_ENV" "$TRANSACTION_PREPARING/runtime.env"
  install -o root -g root -m 0600 "$API_KEYS" "$TRANSACTION_PREPARING/api-keys.json"
  printf '%s\n' LEGAL_MCP_AUTH_TRANSACTION_V2 > "$TRANSACTION_PREPARING/kind"
  printf '%s\n' "$baseline" > "$TRANSACTION_PREPARING/baseline"
  chmod 600 "$TRANSACTION_PREPARING/kind" "$TRANSACTION_PREPARING/baseline"
  hashes="$(mktemp /run/legal-mcp-auth-journal-hashes.XXXXXX)"
  render_journal_hashes "$TRANSACTION_PREPARING" "$hashes"
  install -o root -g root -m 0600 "$hashes" "$TRANSACTION_PREPARING/sha256"
  rm -f "$hashes"
  sync -f "$TRANSACTION_PREPARING"
  validate_v2_journal "$TRANSACTION_PREPARING"
  mv -T "$TRANSACTION_PREPARING" "$TRANSACTION"
  sync -f /etc/legal-mcp
}

render_target_hashes() {
  local directory="$1" destination="$2"
  cat > "$destination" <<EOF
KIND_SHA256=$(sha256sum "$directory/kind" | awk '{print $1}')
HOST_TOOLS_SHA256=$(sha256sum "$directory/host-tools" | awk '{print $1}')
ACTIVE_GENERATION_SHA256=$(sha256sum "$directory/active-generation" | awk '{print $1}')
CADDYFILE_SHA256=$(sha256sum "$directory/Caddyfile" | awk '{print $1}')
RUNTIME_ENV_SHA256=$(sha256sum "$directory/runtime.env" | awk '{print $1}')
API_KEYS_SHA256=$(sha256sum "$directory/api-keys.json" | awk '{print $1}')
EOF
  chmod 600 "$destination"
}

render_ready_commit() {
  local directory="$1" destination="$2"
  cat > "$destination" <<EOF
LEGAL_MCP_AUTH_READY_COMMIT_V1
TARGET_RECEIPT_SHA256=$(sha256sum "$directory/target/sha256" | awk '{print $1}')
EOF
  chmod 600 "$destination"
}

validate_ready_commit() {
  local directory="${1:-$TRANSACTION}" expected
  require_transaction_file "$directory/ready-committed" || return 1
  validate_target_receipt "$directory/target" || return 1
  expected="$(mktemp /run/legal-mcp-auth-ready-commit.XXXXXX)"
  render_ready_commit "$directory" "$expected"
  if ! cmp --silent "$expected" "$directory/ready-committed"; then
    rm -f "$expected"
    return 1
  fi
  rm -f "$expected"
}

publish_ready_commit() {
  local directory="$TRANSACTION" preparing
  preparing="$directory/ready-committing"
  if ! path_is_absent "$directory/ready-committed"; then
    validate_ready_commit "$directory"
    return
  fi
  path_is_absent "$preparing" || return 1
  render_ready_commit "$directory" "$preparing"
  chown root:root "$preparing"
  chmod 600 "$preparing"
  sync -f "$preparing"
  mv -T "$preparing" "$directory/ready-committed"
  sync -f "$directory"
  validate_ready_commit "$directory"
}

validate_target_receipt() {
  local directory="$1" expected_hashes host
  require_transaction_directory "$directory" || return 1
  for name in kind host-tools active-generation Caddyfile runtime.env api-keys.json sha256; do
    require_transaction_file "$directory/$name" || return 1
  done
  directory_contains_only "$directory" \
    Caddyfile active-generation api-keys.json host-tools kind runtime.env sha256 || return 1
  [[ "$(<"$directory/kind")" = LEGAL_MCP_AUTH_TARGET_V1 ]] || return 1
  cmp --silent "$directory/host-tools" "$HOST_TOOLS_MARKER" || return 1
  cmp --silent "$directory/active-generation" "$ACTIVE_GENERATION" || return 1
  cmp --silent "$directory/Caddyfile" "$CADDYFILE" || return 1
  expected_hashes="$(mktemp /run/legal-mcp-auth-target-hashes.XXXXXX)"
  render_target_hashes "$directory" "$expected_hashes"
  if ! cmp --silent "$expected_hashes" "$directory/sha256"; then
    rm -f "$expected_hashes"
    echo 'authentication target receipt hashes do not match' >&2
    return 1
  fi
  rm -f "$expected_hashes"
  validate_runtime_contract "$directory/runtime.env" || return 1
  [[ "$VALIDATED_AUTH_MODE" != disabled ]] || return 1
  host="${VALIDATED_EXTERNAL_URL#https://}"; host="${host%/mcp}"
  if [[ "$VALIDATED_AUTH_MODE" == *api-key* ]]; then
    validate_api_key_document "$directory/api-keys.json" true root root 600 || return 1
  else
    validate_api_key_document "$directory/api-keys.json" false root root 600 || return 1
    [[ "$(<"$directory/api-keys.json")" = '{"keys":[],"version":1}' ]] || return 1
  fi
  TARGET_AUTH_MODE="$VALIDATED_AUTH_MODE"
  TARGET_EXTERNAL_URL="$VALIDATED_EXTERNAL_URL"
  TARGET_PUBLIC_HOST="$host"
}

target_receipt_matches_live() {
  local directory="${1:-$TRANSACTION/target}"
  validate_target_receipt "$directory" || return 1
  cmp --silent "$directory/runtime.env" "$RUNTIME_ENV" \
    && cmp --silent "$directory/api-keys.json" "$API_KEYS"
}

baseline_receipt_matches_live() {
  local directory="$TRANSACTION"
  validate_v2_journal "$directory" || return 1
  cmp --silent "$directory/runtime.env" "$RUNTIME_ENV" \
    && cmp --silent "$directory/api-keys.json" "$API_KEYS"
}

publish_target_receipt() {
  local preparing="$TRANSACTION/target-preparing"
  local replaced="$TRANSACTION/target-replaced" hashes
  validate_v2_journal "$TRANSACTION" || return 1
  path_is_absent "$preparing" && path_is_absent "$replaced" \
    && path_is_absent "$TRANSACTION/ready-committing" \
    && path_is_absent "$TRANSACTION/ready-committed" || return 1
  install -d -o root -g root -m 0700 "$preparing"
  install -o root -g root -m 0600 "$HOST_TOOLS_MARKER" "$preparing/host-tools"
  install -o root -g root -m 0600 "$ACTIVE_GENERATION" "$preparing/active-generation"
  install -o root -g root -m 0600 "$CADDYFILE" "$preparing/Caddyfile"
  install -o root -g root -m 0600 "$RUNTIME_ENV" "$preparing/runtime.env"
  install -o root -g root -m 0600 "$API_KEYS" "$preparing/api-keys.json"
  printf '%s\n' LEGAL_MCP_AUTH_TARGET_V1 > "$preparing/kind"
  chmod 600 "$preparing/kind"
  hashes="$(mktemp /run/legal-mcp-auth-target-hashes.XXXXXX)"
  render_target_hashes "$preparing" "$hashes"
  install -o root -g root -m 0600 "$hashes" "$preparing/sha256"
  rm -f "$hashes"
  sync -f "$preparing"
  validate_target_receipt "$preparing"
  if ! path_is_absent "$TRANSACTION/target"; then
    mv -T "$TRANSACTION/target" "$replaced"
    sync -f "$TRANSACTION"
  fi
  mv -T "$preparing" "$TRANSACTION/target"
  sync -f "$TRANSACTION"
  if ! path_is_absent "$replaced"; then
    rm -rf --one-file-system -- "$replaced"
    path_is_absent "$replaced" || return 1
    sync -f "$TRANSACTION"
  fi
  target_receipt_matches_live
}

validate_v2_journal() {
  local directory="${1:-$TRANSACTION}" baseline expected_hashes name
  local -a allowed=(Caddyfile active-generation api-keys.json baseline host-tools kind runtime.env sha256)
  require_transaction_directory "$directory" || return 1
  for name in kind baseline host-tools active-generation Caddyfile runtime.env api-keys.json sha256; do
    require_transaction_file "$directory/$name" || return 1
  done
  [[ "$(<"$directory/kind")" = LEGAL_MCP_AUTH_TRANSACTION_V2 ]] || {
    echo 'authentication journal kind is not V2' >&2
    return 1
  }
  baseline="$(<"$directory/baseline")"
  [[ "$baseline" = dark || "$baseline" = public ]] || return 1
  JOURNAL_TARGET_READY=false
  JOURNAL_TARGET_COMMITTED=false
  for name in target target-preparing target-replaced; do
    if ! path_is_absent "$directory/$name"; then
      require_transaction_directory "$directory/$name" || return 1
      allowed+=("$name")
    fi
  done
  for name in ready-committing ready-committed; do
    if ! path_is_absent "$directory/$name"; then
      require_transaction_file "$directory/$name" || return 1
      allowed+=("$name")
    fi
  done
  [[ ! -e "$directory/ready-committing" || ! -e "$directory/ready-committed" ]] || return 1
  [[ ! -e "$directory/target" || ! -e "$directory/target-preparing" ]] || return 1
  directory_contains_only "$directory" "${allowed[@]}" || return 1
  cmp --silent "$directory/host-tools" "$HOST_TOOLS_MARKER" || {
    echo 'authentication journal is bound to different V2 host-tool bytes' >&2
    return 1
  }
  cmp --silent "$directory/active-generation" "$ACTIVE_GENERATION" || {
    echo 'authentication journal is bound to a different active generation' >&2
    return 1
  }
  cmp --silent "$directory/Caddyfile" "$CADDYFILE" || {
    echo 'authentication journal is bound to different Caddyfile bytes' >&2
    return 1
  }
  expected_hashes="$(mktemp /run/legal-mcp-auth-journal-hashes.XXXXXX)"
  render_journal_hashes "$directory" "$expected_hashes"
  if ! cmp --silent "$expected_hashes" "$directory/sha256"; then
    rm -f "$expected_hashes"
    echo 'authentication journal rollback-byte hashes do not match' >&2
    return 1
  fi
  rm -f "$expected_hashes"
  validate_runtime_contract "$directory/runtime.env" || return 1
  if [[ "$VALIDATED_AUTH_MODE" == *api-key* ]]; then
    validate_api_key_document "$directory/api-keys.json" true root root 600 || return 1
  else
    validate_api_key_document "$directory/api-keys.json" false root root 600 || return 1
    [[ "$(<"$directory/api-keys.json")" = '{"keys":[],"version":1}' ]] || return 1
  fi
  if [[ "$baseline" = dark ]]; then
    [[ "$VALIDATED_AUTH_MODE" = disabled ]] || return 1
  else
    [[ "$VALIDATED_AUTH_MODE" != disabled ]] || return 1
  fi
  JOURNAL_BASELINE="$baseline"
  JOURNAL_AUTH_MODE="$VALIDATED_AUTH_MODE"
  JOURNAL_EXTERNAL_URL="$VALIDATED_EXTERNAL_URL"
  if ! path_is_absent "$directory/target"; then
    validate_target_receipt "$directory/target" || return 1
    JOURNAL_TARGET_READY=true
  fi
  if ! path_is_absent "$directory/ready-committed"; then
    validate_ready_commit "$directory" || return 1
    JOURNAL_TARGET_COMMITTED=true
  fi
}

validate_complete_handoff() {
  local directory="${1:-$TRANSACTION}"
  validate_v2_journal "$directory" || return 1
  [[ "$JOURNAL_TARGET_READY" = true ]] \
    && path_is_absent "$directory/target-preparing" \
    && path_is_absent "$directory/target-replaced"
}

validate_committed_handoff() {
  local directory="${1:-$TRANSACTION}"
  validate_complete_handoff "$directory" \
    && [[ "$JOURNAL_TARGET_COMMITTED" = true ]] \
    && path_is_absent "$directory/ready-committing"
}

discard_incomplete_target_state() {
  local directory="$TRANSACTION" name path
  for name in target-preparing target-replaced ready-committing; do
    path="$directory/$name"
    if ! path_is_absent "$path"; then
      if [[ "$name" = ready-committing ]]; then
        require_transaction_file "$path" || return 1
        rm -f -- "$path"
      else
        require_transaction_directory "$path" || return 1
        rm -rf --one-file-system -- "$path"
      fi
      path_is_absent "$path" || return 1
      sync -f "$directory"
    fi
  done
}

delete_retired_directory() {
  local path="$1"
  path_is_absent "$path" && return 0
  require_transaction_directory "$path"
  rm -rf --one-file-system -- "$path"
  path_is_absent "$path" || return 1
  sync -f /etc/legal-mcp
}

retire_preparation() {
  path_is_absent "$TRANSACTION_PREPARING_RETIRED" || return 1
  require_transaction_directory "$TRANSACTION_PREPARING"
  mv -T "$TRANSACTION_PREPARING" "$TRANSACTION_PREPARING_RETIRED"
  sync -f /etc/legal-mcp
  delete_retired_directory "$TRANSACTION_PREPARING_RETIRED"
}

retire_active_transaction() {
  path_is_absent "$TRANSACTION_RETIRING" || return 1
  path_is_absent "$TRANSACTION_RETIRED" || return 1
  require_transaction_directory "$TRANSACTION"
  mv -T "$TRANSACTION" "$TRANSACTION_RETIRING"
  sync -f /etc/legal-mcp
  mv -T "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"
  sync -f /etc/legal-mcp
  delete_retired_directory "$TRANSACTION_RETIRED"
}

validate_auth_state_names() {
  local path name paths
  paths="$(find /etc/legal-mcp -mindepth 1 -maxdepth 1 \
    -name '.auth-transaction*' -print)" || {
    echo 'could not inspect authentication transaction states' >&2
    return 1
  }
  while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    name="${path##*/}"
    case "$name" in
      .auth-transaction.preparing|.auth-transaction.preparing-retired|\
      .auth-transaction|.auth-transaction.retiring|.auth-transaction.retired|\
      .auth-transaction.legacy-v0192-preparing-retiring|\
      .auth-transaction.legacy-v0192-preparing-retired) ;;
      .auth-transaction.preparing.*)
        [[ "$name" =~ ^\.auth-transaction\.preparing\.[1-9][0-9]*$ ]] || return 1
        ;;
      *)
        echo "unknown authentication transaction state: $name" >&2
        return 1
        ;;
    esac
  done <<< "$paths"
}

count_auth_states() {
  local path count=0
  validate_auth_state_names || return 1
  while IFS= read -r path; do
    [[ -z "$path" ]] || ((count += 1))
  done < <(find /etc/legal-mcp -mindepth 1 -maxdepth 1 \
    -name '.auth-transaction*' -print)
  printf '%s\n' "$count"
}

read_recovery_probe_if_needed() {
  local document="$1" mode="$2"
  if [[ "$mode" == *api-key* ]]; then
    IFS= read -r PROBE_API_KEY || {
      echo 'restoring the prior API-key mode requires a probe key on standard input' >&2
      return 1
    }
    validate_probe_key_for_document "$document" || {
      echo 'probe key does not match the saved API-key verifier document' >&2
      return 1
    }
  fi
}

restore_v2_journal() {
  local public_host
  validate_v2_journal
  discard_incomplete_target_state
  public_host="${JOURNAL_EXTERNAL_URL#https://}"
  public_host="${public_host%/mcp}"
  read_recovery_probe_if_needed "$TRANSACTION/api-keys.json" "$JOURNAL_AUTH_MODE"
  force_everything_off
  atomic_install_file "$TRANSACTION/runtime.env" "$RUNTIME_ENV" root root 600
  atomic_install_file "$TRANSACTION/api-keys.json" "$API_KEYS" legal-mcp legal-mcp 400
  systemctl daemon-reload
  if [[ "$JOURNAL_BASELINE" = public ]]; then
    systemctl restart "$SERVICE"
    wait_for_generation "$EXPECTED_GENERATION"
    require_legal_service_state active
    require_listener_topology private
    probe_auth_boundary http://127.0.0.1:51235/mcp \
      http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp \
      "$JOURNAL_AUTH_MODE" "$JOURNAL_EXTERNAL_URL"
    systemctl enable --now caddy.service
    require_caddy_state enabled active
    require_listener_topology public
    require_ufw_state closed
    ufw allow 80/tcp comment 'Caddy ACME HTTP' >/dev/null
    ufw allow 443/tcp comment 'Australian Legal MCP HTTPS' >/dev/null
    require_ufw_state open
    require_listener_topology public
    wait_for_public_auth_boundary "$JOURNAL_EXTERNAL_URL" \
      "${JOURNAL_EXTERNAL_URL%/mcp}/.well-known/oauth-protected-resource/mcp" \
      "$JOURNAL_AUTH_MODE" "$JOURNAL_EXTERNAL_URL"
    probe_negative_public_routes "${JOURNAL_EXTERNAL_URL%/mcp}"
  fi
  validate_live_state_matrix "$public_host"
  [[ "$LIVE_BASELINE" = "$JOURNAL_BASELINE" ]]
  if [[ "$JOURNAL_BASELINE" = public ]]; then
    # Keep the exact baseline journal until the launcher durably republishes
    # auth-ready. The baseline itself becomes the finalizable target receipt.
    publish_target_receipt
  else
    retire_active_transaction
  fi
}

restore_committed_target() {
  local host bytes_match=false
  validate_committed_handoff
  if target_receipt_matches_live; then bytes_match=true; fi
  host="$TARGET_PUBLIC_HOST"
  if [[ "$bytes_match" = true ]] \
    && validate_live_state_matrix "$host" false absent \
    && [[ "$LIVE_BASELINE" = public ]]; then
    return 0
  fi
  read_recovery_probe_if_needed "$TRANSACTION/target/api-keys.json" "$TARGET_AUTH_MODE"
  force_everything_off
  if [[ "$bytes_match" = false ]]; then
    atomic_install_file "$TRANSACTION/target/runtime.env" "$RUNTIME_ENV" root root 600
    atomic_install_file "$TRANSACTION/target/api-keys.json" "$API_KEYS" legal-mcp legal-mcp 400
  fi
  systemctl daemon-reload
  validate_closed_configured_state "$host"
  restore_current_configured_public "$host"
  target_receipt_matches_live
  validate_live_state_matrix "$host" true absent
  [[ "$LIVE_BASELINE" = public ]]
}

validate_legacy_v0192_marker() {
  local deploy_sha publisher_sha sudoers_sha path
  local -a marker
  for path in "$AUTH_READY" "$AUTH_PERMIT" "$HOST_TOOL_LAUNCHER" \
    "$HOST_TOOL_LAUNCHER_MARKER" "$CONFIGURE_AUTH_POINTER" \
    "$UPDATE_IMAGE_POINTER" "$HOST_TOOL_DISPATCH" \
    "${HOST_TOOL_DISPATCH}.retiring" "${HOST_TOOL_DISPATCH}.retired"; do
    path_is_absent "$path" || {
      echo 'legacy recovery refuses V2 launcher/auth-ready state' >&2
      return 1
    }
  done
  require_regular_file "$HOST_TOOLS_MARKER" root root 444 || return 1
  mapfile -t marker < "$HOST_TOOLS_MARKER"
  [[ ${#marker[@]} -eq 6 \
    && "${marker[0]}" = LEGAL_MCP_HOST_TOOLS_V1 \
    && "${marker[1]}" = VERSION=0.19.2 \
    && "${marker[2]}" =~ ^SOURCE_COMMIT=[0-9a-f]{40}$ \
    && "${marker[3]}" =~ ^HOST_DEPLOY_SHA256=([0-9a-f]{64})$ ]] || return 1
  deploy_sha="${BASH_REMATCH[1]}"
  [[ "${marker[4]}" =~ ^PUBLISHER_COMMAND_SHA256=([0-9a-f]{64})$ ]] || return 1
  publisher_sha="${BASH_REMATCH[1]}"
  [[ "${marker[5]}" =~ ^SUDOERS_SHA256=([0-9a-f]{64})$ ]] || return 1
  sudoers_sha="${BASH_REMATCH[1]}"
  require_regular_file /usr/local/sbin/legal-mcp-host-deploy root root 755 || return 1
  require_regular_file /usr/local/sbin/legal-mcp-publisher-command root root 755 || return 1
  require_regular_file /etc/sudoers.d/legal-mcp-publisher root root 440 || return 1
  [[ "$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}')" = "$deploy_sha" \
    && "$(sha256sum /usr/local/sbin/legal-mcp-publisher-command | awk '{print $1}')" = "$publisher_sha" \
    && "$(sha256sum /etc/sudoers.d/legal-mcp-publisher | awk '{print $1}')" = "$sudoers_sha" ]]
}

validate_legacy_header_file() {
  local path="$1"
  require_regular_file "$path" root root 600 || return 1
  [[ "$(stat -c '%s' "$path")" -le 65536 ]] || return 1
  python3 - "$path" <<'PY'
import pathlib, sys
raw = pathlib.Path(sys.argv[1]).read_bytes()
if b"\x00" in raw:
    raise SystemExit(1)
text = raw.decode("ascii", "strict").lower()
if "authorization:" in text or "x-api-key:" in text:
    raise SystemExit(1)
PY
}

validate_legacy_v0192_journal() {
  local name
  require_transaction_directory "$TRANSACTION" || return 1
  require_regular_file "$TRANSACTION/runtime.env" root root 600 || return 1
  require_regular_file "$TRANSACTION/api-keys.json" legal-mcp legal-mcp 400 || return 1
  require_regular_file "$TRANSACTION/service-was-enabled" root root 600 || return 1
  [[ "$(stat -c '%s' "$TRANSACTION/service-was-enabled")" = 0 ]] || return 1
  for name in private.headers public.headers recovery.headers; do
    if [[ -e "$TRANSACTION/$name" ]]; then validate_legacy_header_file "$TRANSACTION/$name" || return 1; fi
  done
  directory_contains_only "$TRANSACTION" runtime.env api-keys.json service-was-enabled \
    private.headers public.headers recovery.headers || {
    echo 'legacy authentication journal contains state outside the one-shot allowlist' >&2
    return 1
  }
  validate_runtime_contract "$TRANSACTION/runtime.env" || return 1
  [[ "$VALIDATED_AUTH_MODE" = disabled ]] || return 1
  validate_api_key_document "$TRANSACTION/api-keys.json" false legal-mcp legal-mcp 400 || return 1
  [[ "$(<"$TRANSACTION/api-keys.json")" = '{"keys":[],"version":1}' ]] || return 1
  validate_active_generation_pointer || return 1
  [[ "$EXPECTED_GENERATION" = a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3 ]] || {
    echo 'legacy recovery is restricted to the exact current remote v20 pointer' >&2
    return 1
  }
}

validate_legacy_v0192_dark_contract() {
  local host image rendered
  local -a image_lines
  validate_legacy_v0192_marker || return 1
  validate_foreign_transactions_absent || return 1
  validate_live_auth_files || return 1
  [[ "$VALIDATED_AUTH_MODE" = disabled \
    && "$(<"$API_KEYS")" = '{"keys":[],"version":1}' ]] || return 1
  host="${VALIDATED_EXTERNAL_URL#https://}"; host="${host%/mcp}"
  validate_active_generation_pointer || return 1
  [[ "$EXPECTED_GENERATION" = a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3 ]] || return 1
  validate_caddy_contract "$host" || return 1
  require_regular_file "$IMAGE_FILE" root root 600 || return 1
  require_regular_file "$TEMPLATE" root root 644 || return 1
  require_regular_file "$QUADLET" root root 644 || return 1
  mapfile -t image_lines < "$IMAGE_FILE"
  [[ ${#image_lines[@]} -eq 1 \
    && "${image_lines[0]}" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ ]] || return 1
  image="${image_lines[0]}"
  [[ "$(grep -o '__IMAGE_DIGEST__' "$TEMPLATE" | wc -l)" = 1 ]] || return 1
  rendered="$(mktemp /run/legal-mcp-legacy-auth-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$image|g" "$TEMPLATE" > "$rendered"
  if ! cmp --silent "$rendered" "$QUADLET"; then
    rm -f "$rendered"
    return 1
  fi
  rm -f "$rendered"
  require_legal_service_state inactive \
    && require_caddy_state disabled inactive \
    && require_ufw_state closed \
    && require_listener_topology none
}

find_legacy_v0192_preparation() {
  local path found=''
  while IFS= read -r path; do
    [[ -z "$path" ]] && continue
    [[ -z "$found" ]] || return 1
    found="$path"
  done < <(find /etc/legal-mcp -mindepth 1 -maxdepth 1 \
    -name '.auth-transaction.preparing.*' -print)
  [[ -n "$found" ]] || return 1
  LEGACY_PREPARING_PATH="$found"
  LEGACY_PREPARING_PID="${found##*.}"
  [[ "$LEGACY_PREPARING_PID" =~ ^[1-9][0-9]*$ ]]
}

recover_legacy_v0192_preparation() {
  local path
  validate_legacy_v0192_dark_contract || {
    echo 'legacy v0.19.2 preparation recovery requires the exact dark V1/v20 host contract' >&2
    return 1
  }
  if ! path_is_absent "$LEGACY_PREPARING_RETIRING"; then
    path="$LEGACY_PREPARING_RETIRING"
    require_transaction_directory "$path"
    path_is_absent "$LEGACY_PREPARING_RETIRED" || return 1
    mv -T "$path" "$LEGACY_PREPARING_RETIRED"
    sync -f /etc/legal-mcp
  elif ! path_is_absent "$LEGACY_PREPARING_RETIRED"; then
    require_transaction_directory "$LEGACY_PREPARING_RETIRED"
  else
    find_legacy_v0192_preparation || return 1
    require_transaction_directory "$LEGACY_PREPARING_PATH"
    [[ ! -e "/proc/$LEGACY_PREPARING_PID" ]] || {
      echo 'legacy v0.19.2 preparation owner is still alive' >&2
      return 1
    }
    path_is_absent "$LEGACY_PREPARING_RETIRING" \
      && path_is_absent "$LEGACY_PREPARING_RETIRED" || return 1
    mv -T "$LEGACY_PREPARING_PATH" "$LEGACY_PREPARING_RETIRING"
    sync -f /etc/legal-mcp
    mv -T "$LEGACY_PREPARING_RETIRING" "$LEGACY_PREPARING_RETIRED"
    sync -f /etc/legal-mcp
  fi
  delete_retired_directory "$LEGACY_PREPARING_RETIRED"
}

recover_legacy_v0192_journal() {
  local host image rendered
  local -a image_lines
  validate_legacy_v0192_marker || {
    echo 'unversioned auth journal is not on the exact v0.19.2 host contract' >&2
    return 1
  }
  validate_legacy_v0192_journal || {
    echo 'legacy v0.19.2 auth journal failed its one-shot contract' >&2
    return 1
  }
  validate_foreign_transactions_absent
  host="${VALIDATED_EXTERNAL_URL#https://}"; host="${host%/mcp}"
  validate_caddy_contract "$host"
  require_regular_file "$IMAGE_FILE" root root 600
  require_regular_file "$TEMPLATE" root root 644
  require_regular_file "$QUADLET" root root 644
  mapfile -t image_lines < "$IMAGE_FILE"
  [[ ${#image_lines[@]} -eq 1 \
    && "${image_lines[0]}" =~ ^ghcr\.io/gunba/australian-legal-mcp@sha256:[0-9a-f]{64}$ ]]
  image="${image_lines[0]}"
  [[ "$(grep -o '__IMAGE_DIGEST__' "$TEMPLATE" | wc -l)" = 1 ]]
  rendered="$(mktemp /run/legal-mcp-legacy-auth-quadlet.XXXXXX)"
  sed "s|__IMAGE_DIGEST__|$image|g" "$TEMPLATE" > "$rendered"
  cmp --silent "$rendered" "$QUADLET"
  rm -f "$rendered"
  force_everything_off
  atomic_install_file "$TRANSACTION/runtime.env" "$RUNTIME_ENV" root root 600
  atomic_install_file "$TRANSACTION/api-keys.json" "$API_KEYS" legal-mcp legal-mcp 400
  systemctl daemon-reload
  require_legal_service_state inactive
  require_caddy_state disabled inactive
  require_ufw_state closed
  require_listener_topology none
  retire_active_transaction
}

recover_auth_state() {
  local states host
  states="$(count_auth_states)"
  [[ "$states" -le 1 ]] || {
    echo 'authentication transaction has conflicting durable states' >&2
    return 1
  }
  if ! path_is_absent "$LEGACY_PREPARING_RETIRING" \
    || ! path_is_absent "$LEGACY_PREPARING_RETIRED" \
    || find_legacy_v0192_preparation 2>/dev/null; then
    recover_legacy_v0192_preparation
    echo 'one-shot dead-PID v0.19.2 authentication preparation discarded; upgrade V2 host tools before configuring authentication'
    return 0
  fi
  if ! path_is_absent "$TRANSACTION_PREPARING"; then
    validate_host_tools_v2
    validate_active_generation_pointer
    validate_live_auth_files
    host="${VALIDATED_EXTERNAL_URL#https://}"; host="${host%/mcp}"
    validate_caddy_contract "$host"
    if ! validate_live_state_matrix "$host" false baseline; then
      validate_closed_configured_state "$host" || {
        echo 'V2 preparation recovery found no exact prior dark/public state' >&2
        return 1
      }
      restore_current_configured_public "$host"
    fi
    retire_preparation
    echo 'interrupted V2 authentication preparation discarded'
    return 0
  fi
  if ! path_is_absent "$TRANSACTION_PREPARING_RETIRED"; then
    validate_host_tools_v2
    validate_active_generation_pointer
    validate_live_auth_files
    host="${VALIDATED_EXTERNAL_URL#https://}"; host="${host%/mcp}"
    validate_caddy_contract "$host"
    if ! validate_live_state_matrix "$host" false baseline; then
      validate_closed_configured_state "$host"
      restore_current_configured_public "$host"
    fi
    delete_retired_directory "$TRANSACTION_PREPARING_RETIRED"
    echo 'interrupted V2 authentication preparation retirement completed'
    return 0
  fi
  if ! path_is_absent "$TRANSACTION_RETIRING"; then
    validate_host_tools_v2
    validate_active_generation_pointer
    validate_v2_journal "$TRANSACTION_RETIRING"
    if ! path_is_absent "$AUTH_READY"; then
      require_auth_ready_marker
      validate_committed_handoff "$TRANSACTION_RETIRING"
      target_receipt_matches_live "$TRANSACTION_RETIRING/target"
      host="$TARGET_PUBLIC_HOST"
      validate_caddy_contract "$host"
      validate_live_state_matrix "$host" true present
    elif validate_committed_handoff "$TRANSACTION_RETIRING" \
      && target_receipt_matches_live "$TRANSACTION_RETIRING/target"; then
      host="$TARGET_PUBLIC_HOST"
      validate_caddy_contract "$host"
      if ! validate_live_state_matrix "$host" false absent; then
        validate_closed_configured_state "$host"
        restore_current_configured_public "$host"
      fi
      echo 'interrupted committed-public authentication retirement restored; auth-ready publication is pending'
      return 0
    else
      [[ "$JOURNAL_BASELINE" = dark ]]
      validate_live_auth_files
      host="${VALIDATED_EXTERNAL_URL#https://}"; host="${host%/mcp}"
      validate_caddy_contract "$host"
      validate_live_state_matrix "$host" true absent
      [[ "$LIVE_BASELINE" = dark ]]
    fi
    sync -f /etc/legal-mcp
    mv -T "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"
    sync -f /etc/legal-mcp
    delete_retired_directory "$TRANSACTION_RETIRED"
    echo 'interrupted V2 authentication transaction retirement completed'
    return 0
  fi
  if ! path_is_absent "$TRANSACTION_RETIRED"; then
    validate_host_tools_v2
    validate_active_generation_pointer
    validate_live_auth_files
    host="${VALIDATED_EXTERNAL_URL#https://}"; host="${host%/mcp}"
    validate_caddy_contract "$host"
    if ! path_is_absent "$AUTH_READY"; then
      require_auth_ready_marker
      validate_live_state_matrix "$host" true present
      [[ "$LIVE_BASELINE" = public ]]
      delete_retired_directory "$TRANSACTION_RETIRED"
      echo 'interrupted committed-public authentication retirement completed'
    elif validate_live_state_matrix "$host" false absent \
      && [[ "$LIVE_BASELINE" = dark ]]; then
      delete_retired_directory "$TRANSACTION_RETIRED"
      echo 'interrupted dark authentication retirement completed'
    else
      validate_closed_configured_state "$host"
      restore_current_configured_public "$host"
      echo 'interrupted committed-public authentication retirement restored; auth-ready publication is pending'
    fi
    return 0
  fi
  if path_is_absent "$TRANSACTION"; then
    validate_host_tools_v2
    validate_active_generation_pointer
    validate_live_auth_files
    host="${VALIDATED_EXTERNAL_URL#https://}"; host="${host%/mcp}"
    validate_caddy_contract "$host"
    if validate_live_state_matrix "$host" false baseline; then
      echo 'authentication state already has no pending transaction'
      return 0
    fi
    validate_closed_configured_state "$host" || {
      echo 'no recoverable authentication transaction or exact closed configured state exists' >&2
      return 1
    }
    create_v2_journal public
    restore_current_configured_public "$host"
    publish_target_receipt
    echo 'closed committed-public authentication restored; auth-ready publication is pending'
    return 0
  fi

  if [[ -f "$TRANSACTION/kind" && ! -L "$TRANSACTION/kind" ]]; then
    validate_host_tools_v2
    validate_v2_journal
    validate_active_generation_pointer
    if ! path_is_absent "$AUTH_READY"; then
      require_auth_ready_marker
      if validate_complete_handoff && target_receipt_matches_live; then
        host="$TARGET_PUBLIC_HOST"
        validate_caddy_contract "$host"
        validate_live_state_matrix "$host" true present
        publish_ready_commit
        retire_active_transaction
        echo 'committed-public V2 authentication handoff finalized'
      else
        [[ "$JOURNAL_TARGET_READY" = false \
          && "$JOURNAL_BASELINE" = public \
          && ! -e "$TRANSACTION/target" \
          && ! -e "$TRANSACTION/target-preparing" \
          && ! -e "$TRANSACTION/target-replaced" ]]
        baseline_receipt_matches_live
        host="${JOURNAL_EXTERNAL_URL#https://}"; host="${host%/mcp}"
        validate_caddy_contract "$host"
        validate_live_state_matrix "$host" true present
        retire_active_transaction
        echo 'pre-cutover V2 authentication preparation discarded'
      fi
    else
      if [[ "$JOURNAL_TARGET_COMMITTED" = true ]]; then
        restore_committed_target
        echo 'committed-public V2 authentication target restored; auth-ready publication is pending'
      else
        host="${JOURNAL_EXTERNAL_URL#https://}"; host="${host%/mcp}"
        validate_caddy_contract "$host"
        restore_v2_journal
        echo 'interrupted V2 authentication transaction rolled back'
      fi
    fi
  else
    recover_legacy_v0192_journal
    echo 'one-shot legacy v0.19.2 authentication transaction rolled back; upgrade V2 host tools before configuring authentication'
  fi
}

if [[ "$PREPARE_AUTH_DISPATCH" = true ]]; then
  [[ -z "$MODE$PUBLIC_HOST$API_KEY_FILE$TENANT_ID$SERVER_APP_ID$AUDIENCES$SCOPE$SCOPE_URI$ALLOWED_CLIENT_IDS" ]] || usage
  [[ "$(count_auth_states)" = 0 ]] || {
    echo 'an authentication transaction already exists; run --recover first' >&2
    exit 1
  }
  validate_live_auth_files
  prepare_host="${VALIDATED_EXTERNAL_URL#https://}"; prepare_host="${prepare_host%/mcp}"
  validate_static_v2_host "$prepare_host"
  validate_live_state_matrix "$prepare_host" true baseline
  BASELINE="$LIVE_BASELINE"
  if [[ "$BASELINE" = public ]]; then
    wait_for_generation "$EXPECTED_GENERATION"
    require_listener_topology public
    probe_auth_boundary http://127.0.0.1:51235/mcp \
      http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp \
      "$VALIDATED_AUTH_MODE" "$VALIDATED_EXTERNAL_URL" false
  fi
  create_v2_journal "$BASELINE"
  exit 0
fi

if [[ "$FINALIZE_AUTH_READY" = true ]]; then
  [[ -z "$MODE$PUBLIC_HOST$API_KEY_FILE$TENANT_ID$SERVER_APP_ID$AUDIENCES$SCOPE$SCOPE_URI$ALLOWED_CLIENT_IDS" ]] || usage
  require_auth_ready_marker
  if ! path_is_absent "$TRANSACTION"; then
    validate_host_tools_v2
    validate_active_generation_pointer
    validate_complete_handoff
    target_receipt_matches_live
    validate_caddy_contract "$TARGET_PUBLIC_HOST"
    validate_live_state_matrix "$TARGET_PUBLIC_HOST" true present
    [[ "$LIVE_BASELINE" = public ]]
    probe_auth_boundary http://127.0.0.1:51235/mcp \
      http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp \
      "$TARGET_AUTH_MODE" "$TARGET_EXTERNAL_URL" false
    publish_ready_commit
    retire_active_transaction
  elif ! path_is_absent "$TRANSACTION_RETIRING"; then
    validate_host_tools_v2
    validate_active_generation_pointer
    validate_committed_handoff "$TRANSACTION_RETIRING"
    target_receipt_matches_live "$TRANSACTION_RETIRING/target"
    validate_caddy_contract "$TARGET_PUBLIC_HOST"
    validate_live_state_matrix "$TARGET_PUBLIC_HOST" true present
    [[ "$LIVE_BASELINE" = public ]]
    mv -T "$TRANSACTION_RETIRING" "$TRANSACTION_RETIRED"
    sync -f /etc/legal-mcp
    delete_retired_directory "$TRANSACTION_RETIRED"
  elif ! path_is_absent "$TRANSACTION_RETIRED"; then
    validate_host_tools_v2
    validate_active_generation_pointer
    validate_live_auth_files
    finalize_host="${VALIDATED_EXTERNAL_URL#https://}"; finalize_host="${finalize_host%/mcp}"
    validate_caddy_contract "$finalize_host"
    validate_live_state_matrix "$finalize_host" true present
    [[ "$LIVE_BASELINE" = public ]]
    delete_retired_directory "$TRANSACTION_RETIRED"
  else
    [[ "$(count_auth_states)" = 0 ]] || {
      echo 'authentication handoff is not in a finalizable state' >&2
      exit 1
    }
    validate_host_tools_v2
    validate_active_generation_pointer
    validate_live_auth_files
    finalize_host="${VALIDATED_EXTERNAL_URL#https://}"; finalize_host="${finalize_host%/mcp}"
    validate_caddy_contract "$finalize_host"
    validate_live_state_matrix "$finalize_host" true present
    [[ "$LIVE_BASELINE" = public ]]
  fi
  exit 0
fi

if [[ "$RECOVER" = true ]]; then
  [[ -z "$MODE$PUBLIC_HOST$API_KEY_FILE$TENANT_ID$SERVER_APP_ID$AUDIENCES$SCOPE$SCOPE_URI$ALLOWED_CLIENT_IDS" ]] || usage
  # shellcheck disable=SC2317,SC2329 # invoked by the ERR/signal/EXIT trap below
  recovery_failure() {
    local status=$?
    trap - ERR HUP INT TERM EXIT
    force_everything_off >/dev/null 2>&1 || true
    unset PROBE_API_KEY
    echo 'authentication recovery failed; service and ingress remain off and the journal was retained' >&2
    exit "$status"
  }
  trap recovery_failure ERR HUP INT TERM EXIT
  recover_auth_state
  trap - ERR HUP INT TERM EXIT
  unset PROBE_API_KEY
  exit 0
fi

[[ "$(count_auth_states)" = 1 && ! -e "$TRANSACTION_PREPARING" \
  && ! -e "$TRANSACTION_PREPARING_RETIRED" && ! -e "$TRANSACTION_RETIRING" \
  && ! -e "$TRANSACTION_RETIRED" && -d "$TRANSACTION" ]] || {
  echo 'normal authentication dispatch requires its exact prepared V2 journal' >&2
  exit 1
}
[[ "$MODE" = api-key || "$MODE" = entra || "$MODE" = entra+api-key ]] || usage
[[ "$PUBLIC_HOST" =~ ^[a-z0-9.-]{3,253}$ && "$PUBLIC_HOST" == *.* ]] || usage
python3 - "$PUBLIC_HOST" <<'PY' || usage
import ipaddress, sys
host = sys.argv[1]
try:
    ipaddress.ip_address(host)
except ValueError:
    pass
else:
    raise SystemExit(1)
labels = host.split('.')
if any(not label or len(label) > 63 or label[0] == '-' or label[-1] == '-'
       or not all(character.isascii() and (character.islower() or character.isdigit() or character == '-') for character in label)
       for label in labels):
    raise SystemExit(1)
PY

has_api=false
has_entra=false
[[ "$MODE" == *api-key* ]] && has_api=true
[[ "$MODE" == *entra* ]] && has_entra=true
if [[ "$has_api" = true ]]; then
  [[ "$API_KEY_FILE" = /* ]] || usage
  validate_api_key_document "$API_KEY_FILE" true root root 600 || {
    echo 'API-key verifier input must be exact canonical root:root mode-0600 data' >&2
    exit 2
  }
  IFS= read -r PROBE_API_KEY || { echo 'API-key mode requires a probe key on standard input' >&2; exit 2; }
  validate_probe_key_for_document "$API_KEY_FILE" || {
    echo 'probe key does not match the supplied verifier document' >&2
    exit 2
  }
else
  [[ -z "$API_KEY_FILE" ]] || usage
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
  IFS=',' read -r -a audience_values <<< "$AUDIENCES"
  IFS=',' read -r -a client_ids <<< "$ALLOWED_CLIENT_IDS"
  [[ "$(printf '%s\n' "${audience_values[@]}" | sort -u | wc -l)" = "${#audience_values[@]}" \
    && "$(printf '%s\n' "${client_ids[@]}" | sort -u | wc -l)" = "${#client_ids[@]}" ]] || usage
  for client_id in "${client_ids[@]}"; do [[ "$client_id" =~ $uuid_re ]] || usage; done
else
  [[ -z "$TENANT_ID$SERVER_APP_ID$AUDIENCES$SCOPE$SCOPE_URI$ALLOWED_CLIENT_IDS" ]] || usage
fi

validate_v2_journal || {
  echo 'prepared authentication baseline journal failed validation' >&2
  exit 1
}
[[ "$JOURNAL_TARGET_READY" = false \
  && ! -e "$TRANSACTION/target-preparing" \
  && ! -e "$TRANSACTION/target-replaced" ]] || {
  echo 'prepared authentication journal already contains target handoff state' >&2
  exit 1
}
PREPARED_BASELINE="$JOURNAL_BASELINE"

validate_static_v2_host "$PUBLIC_HOST" || {
  close_public_ingress >/dev/null 2>&1 || true
  echo 'installed V2 host contract failed authentication preflight' >&2
  exit 1
}
validate_live_state_matrix "$PUBLIC_HOST" || {
  force_everything_off >/dev/null 2>&1 || true
  echo 'authentication cutover refused an invalid host state matrix' >&2
  exit 1
}
BASELINE="$LIVE_BASELINE"
[[ "$BASELINE" = "$PREPARED_BASELINE" ]] || {
  force_everything_off >/dev/null 2>&1 || true
  echo 'live authentication baseline changed after durable preparation' >&2
  exit 1
}
if [[ "$BASELINE" = public ]]; then
  wait_for_generation "$EXPECTED_GENERATION"
  require_listener_topology public
  # The target key may be new, so the existing boundary is proved negatively;
  # its exact marker still binds the current verifier bytes.
  prior_mode="$VALIDATED_AUTH_MODE"
  prior_url="$VALIDATED_EXTERNAL_URL"
  saved_probe="$PROBE_API_KEY"
  probe_auth_boundary http://127.0.0.1:51235/mcp \
    http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp \
    "$prior_mode" "$prior_url" false
  PROBE_API_KEY="$saved_probe"
fi

rollback() {
  local status=$?
  trap - ERR HUP INT TERM EXIT
  # The stable launcher treats every nonzero implementation result as a command
  # to remove auth-ready and close service/ingress. Retain the exact canonical
  # journal instead of pretending an in-process public restoration can survive
  # that outer fail-closed action. A subsequent launcher --recover invocation
  # restores and proves the saved dark/public matrix, then returns success so
  # the launcher can republish auth-ready.
  force_everything_off >/dev/null 2>&1 || true
  unset PROBE_API_KEY
  echo 'authentication cutover failed; service and ingress remain off and the V2 journal requires --recover' >&2
  exit "$status"
}
trap rollback ERR HUP INT TERM EXIT

close_public_ingress
systemctl stop "$SERVICE"
path_is_absent "$AUTH_READY"
systemctl daemon-reload
require_legal_service_state inactive
require_listener_topology none
require_ufw_state closed

if [[ "$has_api" = true ]]; then
  atomic_install_file "$API_KEY_FILE" "$API_KEYS" legal-mcp legal-mcp 400
else
  empty_keys="$(mktemp /run/legal-mcp-empty-api-keys.XXXXXX)"
  printf '%s\n' '{"keys":[],"version":1}' > "$empty_keys"
  atomic_install_file "$empty_keys" "$API_KEYS" legal-mcp legal-mcp 400
  rm -f "$empty_keys"
fi

runtime_tmp="$(mktemp /run/legal-mcp-runtime.XXXXXX)"
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
atomic_install_file "$runtime_tmp" "$RUNTIME_ENV" root root 600
rm -f "$runtime_tmp"
validate_live_auth_files "$PUBLIC_HOST"

path_is_absent "$AUTH_READY"
systemctl daemon-reload
systemctl restart "$SERVICE"
wait_for_generation "$EXPECTED_GENERATION"
require_legal_service_state active
require_listener_topology private
require_ufw_state closed
probe_auth_boundary http://127.0.0.1:51235/mcp \
  http://127.0.0.1:51235/.well-known/oauth-protected-resource/mcp \
  "$MODE" "https://$PUBLIC_HOST/mcp"

# Caddy starts and its exact listeners are proved while UFW is still closed.
# Only this complete private/static/listener preflight permits public rules.
systemctl enable --now caddy.service
require_caddy_state enabled active
require_listener_topology public
require_ufw_state closed
ufw allow 80/tcp comment 'Caddy ACME HTTP'
ufw allow 443/tcp comment 'Australian Legal MCP HTTPS'
require_ufw_state open
require_listener_topology public

external_url="https://$PUBLIC_HOST/mcp"
wait_for_public_auth_boundary "$external_url" \
  "https://$PUBLIC_HOST/.well-known/oauth-protected-resource/mcp" \
  "$MODE" "$external_url"
probe_negative_public_routes "https://$PUBLIC_HOST"
validate_live_state_matrix "$PUBLIC_HOST"
[[ "$LIVE_BASELINE" = public ]]

publish_target_receipt
trap - ERR HUP INT TERM EXIT
unset PROBE_API_KEY
printf '%s\n' 'authentication configured; exact private/public auth and route probes passed'
