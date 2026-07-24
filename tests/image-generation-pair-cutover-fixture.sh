#!/usr/bin/env bash
# Exhaustive disposable-host fixture for the generic incompatible image and
# generation pair transition. Deterministic system fakes exercise the production
# updater, host-deploy helper, and generated stable launcher.
set -euo pipefail
umask 027
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

[[ $EUID -eq 0 && -x /update-image && -x /host-deploy && -f /install-host \
  && -f /container-template && -f /Caddyfile && -f /publisher-command ]] || {
  echo 'fixture requires the production hosting inputs in a disposable root container' >&2
  exit 2
}

version=0.20.0
revision=1111111111111111111111111111111111111111
old_version=0.19.11
old_revision=893b06c20e5fc2f33ca7633e636023ccb5762745
old_generation=937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939
target_generation=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
probe_key="fixture.$(printf 'A%.0s' {1..43})"
old_image="ghcr.io/gunba/australian-legal-mcp@sha256:43be03afbdd78c509053200d0f61b35a1519e9d95f303b917f8023f4ae2a7470"
target_image="ghcr.io/gunba/australian-legal-mcp@sha256:008de908c49b4975eba0f7601e6a554b27ede8202a9e5fe26197c6221b03e3f0"
old_image_id="sha256:dd543ce13fafad18d522652ba80404e3fd870f277f9644c3820fe726aa5584c3"
target_image_id="sha256:a1800bbc97dab9ebf158dec851d737d1dbeb4e42a4facc360696ea12353b98e7"
transaction=/etc/legal-mcp/.image-transaction
journal=/srv/legal-mcp/lifecycle/.deployment-transaction
pointer=/srv/legal-mcp/lifecycle/active-generation
auth_ready=/etc/legal-mcp/auth-ready
image_file=/etc/legal-mcp/image
template=/usr/local/libexec/legal-mcp/legal-mcp.container.template
quadlet=/etc/containers/systemd/legal-mcp.container
launcher=/usr/local/libexec/legal-mcp/host-tool-launcher
update_entry=/usr/local/sbin/legal-mcp-update-image
configure_entry=/usr/local/sbin/legal-mcp-configure-auth
implementation_dir=/usr/local/libexec/legal-mcp/host-tools
bundle=/bundle
rollback_bundle=/rollback-bundle

getent group legal-mcp >/dev/null || groupadd --gid 971 legal-mcp
getent passwd legal-mcp >/dev/null ||
  useradd --uid 971 --gid 971 --home-dir /nonexistent --no-create-home legal-mcp
getent group legal-mcp-publisher >/dev/null || groupadd --gid 973 legal-mcp-publisher
getent passwd legal-mcp-publisher >/dev/null ||
  useradd --uid 973 --gid 973 --home-dir /nonexistent --no-create-home legal-mcp-publisher
getent group legal-mcp-admin >/dev/null || groupadd --gid 974 legal-mcp-admin
getent passwd legal-mcp-admin >/dev/null ||
  useradd --uid 974 --gid 974 --home-dir /nonexistent --no-create-home legal-mcp-admin
getent group caddy >/dev/null || groupadd --system caddy

install -d -o root -g root -m 0755 /etc/legal-mcp /etc/containers/systemd \
  /usr/local/libexec/legal-mcp "$implementation_dir" /etc/caddy /etc/sudoers.d \
  /run/lock
install -d -o root -g legal-mcp-publisher -m 0710 /run/legal-mcp
install -o root -g legal-mcp-publisher -m 0640 /dev/null \
  /run/lock/legal-mcp-host-transaction.lock
install -d -o root -g legal-mcp -m 0750 /srv/legal-mcp \
  /srv/legal-mcp/generations /srv/legal-mcp/lifecycle
setfacl --remove-all /srv/legal-mcp
setfacl --modify user:legal-mcp-publisher:--x /srv/legal-mcp
install -d -o legal-mcp -g legal-mcp -m 0700 /srv/legal-mcp/state
install -d -o legal-mcp-publisher -g legal-mcp-publisher -m 0700 /srv/legal-mcp/uploads
install -o root -g legal-mcp -m 0640 /dev/null /srv/legal-mcp/lifecycle/LOCK
install -o root -g root -m 0640 /dev/null /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK
printf 'LEGAL_MCP_VOLUME_V1\nUUID=11111111-2222-3333-4444-555555555555\n' \
  > /srv/legal-mcp/.legal-mcp-volume
chmod 444 /srv/legal-mcp/.legal-mcp-volume
printf 'LEGAL_MCP_HOST_V1\nVOLUME_UUID=11111111-2222-3333-4444-555555555555\n' \
  > /etc/legal-mcp/host-installed
chmod 444 /etc/legal-mcp/host-installed

# Real mutation utilities are retained behind wrappers that expose every
# durable phase boundary as a SIGKILL point.
mv /usr/bin/install /usr/bin/install.fixture-real
mv /usr/bin/mv /usr/bin/mv.fixture-real
/usr/bin/mv.fixture-real /usr/bin/id /usr/bin/id.fixture-real
/usr/bin/mv.fixture-real /usr/bin/chown /usr/bin/chown.fixture-real
/usr/bin/mv.fixture-real /usr/bin/find /usr/bin/find.fixture-real
/usr/bin/mv.fixture-real /usr/bin/rm /usr/bin/rm.fixture-real
/usr/bin/mv.fixture-real /usr/bin/rmdir /usr/bin/rmdir.fixture-real
/usr/bin/mv.fixture-real /usr/bin/sync /usr/bin/sync.fixture-real
cat > /usr/local/sbin/fixture-kill <<'EOF'
#!/usr/bin/bash
point="$1"
[[ -s /tmp/kill-cutover-at && "$(</tmp/kill-cutover-at)" = "$point" ]] || exit 0
/usr/bin/rm.fixture-real -f /tmp/kill-cutover-at
# Find the mounted updater implementation rather than a command-substitution
# subshell, leaving the completed durable mutation and no opportunity for its
# in-process trap.
candidate="$PPID"
updater_pid=''
launcher_pid=''
kill_pids=()
updater_depth=0
launcher_depth=0
while [[ "$candidate" =~ ^[1-9][0-9]*$ && "$candidate" -gt 1 ]]; do
  kill_pids+=("$candidate")
  mapfile -d '' -t command < "/proc/$candidate/cmdline"
  if printf '%s\n' "${command[@]}" \
    | grep -Fxq /usr/local/sbin/legal-mcp-update-image \
    && { printf '%s\n' "${command[@]}" | grep -Fxq -- --pair-cutover \
      || printf '%s\n' "${command[@]}" | grep -Fxq -- --pair-rollback; }; then
    updater_pid="$candidate"
    updater_depth="${#kill_pids[@]}"
  fi
  if printf '%s\n' "${command[@]}" \
    | grep -Fxq -- '--legal-mcp-launcher-internal'; then
    launcher_pid="$candidate"
    launcher_depth="${#kill_pids[@]}"
  fi
  candidate="$(awk '$1 == "PPid:" {print $2}' "/proc/$candidate/status")"
done
[[ "$updater_pid" =~ ^[1-9][0-9]*$ \
  && "$launcher_pid" =~ ^[1-9][0-9]*$ \
  && "$launcher_depth" -gt "$updater_depth" ]]
# Kill through the stable outer launcher in one signal operation. This leaves
# no launcher process available to run EXIT cleanup, matching abrupt host loss.
kill -KILL "${kill_pids[@]:0:launcher_depth}"
# Most cases model reboot. Selected cases retain /run and mocked service state
# to prove recovery also works after only the launcher is killed.
if [[ ! -e /tmp/preserve-run-after-kill ]]; then
  /usr/bin/rm.fixture-real -rf /tmp/service-active /tmp/caddy-active \
    /run/legal-mcp/host-tool-launcher-dispatch* \
    /run/legal-mcp/pair-cutover-starting /run/legal-mcp/pair-cutover-start-armed
fi
sleep 1
EOF
chmod 755 /usr/local/sbin/fixture-kill

cat > /usr/bin/id <<'EOF'
#!/usr/bin/bash
if [[ -e /tmp/invalid-service-identity && "$*" = '-u legal-mcp' ]]; then
  printf '%s\n' 970
  exit 0
fi
exec /usr/bin/id.fixture-real "$@"
EOF

cat > /usr/bin/install <<'EOF'
#!/usr/bin/bash
/usr/bin/install.fixture-real "$@"
status=$?
if [[ $status -eq 0 ]]; then
  case "${!#}" in
    /etc/legal-mcp/.pair-transaction-build)
      /usr/local/sbin/fixture-kill transaction-created ;;
    /etc/legal-mcp/.pair-transaction-build/saved-template)
      /usr/local/sbin/fixture-kill transaction-build-partial
      /usr/local/sbin/fixture-kill transaction-build-torn ;;
  esac
fi
exit "$status"
EOF
cat > /usr/bin/mv <<'EOF'
#!/usr/bin/bash
target="${!#}"
/usr/bin/mv.fixture-real "$@"
status=$?
if [[ $status -eq 0 ]]; then
  case "$target" in
    /etc/legal-mcp/.image-transaction) point=transaction-published ;;
    /srv/legal-mcp/lifecycle/.deployment-transaction) point=deployment-journal-published ;;
    /srv/legal-mcp/uploads/*) point=sealed-upload-moved ;;
    /etc/legal-mcp/image) point=image-switched ;;
    /etc/containers/systemd/legal-mcp.container) point=quadlet-switched ;;
    /usr/local/libexec/legal-mcp/legal-mcp.container.template) point=template-switched ;;
    /etc/legal-mcp/.image-transaction/phase) point="phase-$(<"$target")" ;;
    /etc/legal-mcp/auth-ready) point=auth-ready-published ;;
    /etc/legal-mcp/.image-transaction/retirement-outcome) point=target-committed ;;
    /etc/legal-mcp/.image-transaction.retiring) point=transaction-retiring ;;
    /etc/legal-mcp/.image-transaction.retired) point=transaction-retired ;;
    /etc/legal-mcp/.image-transaction.deletion) point=transaction-deletion-marked ;;
    /etc/legal-mcp/.image-transaction.preparing-deletion) point=preparation-deletion-marked ;;
    *) point='' ;;
  esac
  [[ -z "$point" ]] || /usr/local/sbin/fixture-kill "$point"
fi
exit "$status"
EOF
cat > /usr/bin/rm <<'EOF'
#!/usr/bin/bash
if [[ -s /tmp/kill-cutover-at ]]; then
  case "$(</tmp/kill-cutover-at):${!#}" in
    transaction-delete-partial:/etc/legal-mcp/.image-transaction.retired/*)
      [[ "${!#}" = /etc/legal-mcp/.image-transaction.retired/kind ]] || {
        /usr/bin/rm.fixture-real "$@"
        /usr/local/sbin/fixture-kill transaction-delete-partial
      }
      ;;
    preparation-delete-partial:/etc/legal-mcp/.image-transaction.preparing-retired/*)
      [[ "${!#}" = /etc/legal-mcp/.image-transaction.preparing-retired/kind ]] || {
        /usr/bin/rm.fixture-real "$@"
        /usr/local/sbin/fixture-kill preparation-delete-partial
      }
      ;;
  esac
fi
/usr/bin/rm.fixture-real "$@"
status=$?
if [[ $status -eq 0 ]]; then
  case "$*" in
    *'/etc/legal-mcp/auth-ready'*) /usr/local/sbin/fixture-kill auth-ready-dark ;;
    *'/run/legal-mcp/authorized-upload'*) /usr/local/sbin/fixture-kill upload-revoked ;;
    *'/srv/legal-mcp/lifecycle/.deployment-transaction'*)
      /usr/local/sbin/fixture-kill corpus-committed ;;
    *'/etc/legal-mcp/.image-transaction.deletion'*) /usr/local/sbin/fixture-kill transaction-deleted ;;
    *'/etc/legal-mcp/.image-transaction.preparing-deletion'*) /usr/local/sbin/fixture-kill preparation-deleted ;;
  esac
fi
exit "$status"
EOF
cat > /usr/bin/rmdir <<'EOF'
#!/usr/bin/bash
/usr/bin/rmdir.fixture-real "$@"
status=$?
if [[ $status -eq 0 ]]; then
  case "${!#}" in
    /etc/legal-mcp/.image-transaction.retired)
      /usr/local/sbin/fixture-kill transaction-retired-removed ;;
    /etc/legal-mcp/.image-transaction.preparing-retired)
      /usr/local/sbin/fixture-kill preparation-retired-removed ;;
  esac
fi
exit "$status"
EOF
cat > /usr/bin/sync <<'EOF'
#!/usr/bin/bash
/usr/bin/sync.fixture-real "$@"
status=$?
if [[ $status -eq 0 ]]; then
  case "$*" in
    '-f /etc/legal-mcp/.image-transaction.preparing')
      /usr/local/sbin/fixture-kill transaction-synced ;;
    '-f /srv/legal-mcp/lifecycle/.deployment-transaction.preparing')
      /usr/local/sbin/fixture-kill deployment-journal-prepared ;;
    '-f /etc/legal-mcp/.image-transaction/phase.preparing')
      /usr/local/sbin/fixture-kill phase-prepared ;;
    '-f /etc/legal-mcp/.image-transaction/retirement-outcome.preparing')
      /usr/local/sbin/fixture-kill outcome-prepared ;;
    '-f /etc/legal-mcp/.pair-image.preparing')
      /usr/local/sbin/fixture-kill image-prepared ;;
    '-f /etc/containers/systemd/.pair-quadlet.preparing')
      /usr/local/sbin/fixture-kill quadlet-prepared ;;
    '-f /usr/local/libexec/legal-mcp/.pair-template.preparing')
      /usr/local/sbin/fixture-kill template-prepared ;;
  esac
fi
exit "$status"
EOF
cat > /usr/bin/chown <<'EOF'
#!/usr/bin/bash
/usr/bin/chown.fixture-real "$@"
status=$?
if [[ $status -eq 0 ]]; then
  case "$*" in
    '-R root:legal-mcp /srv/legal-mcp/uploads/'*)
      /usr/local/sbin/fixture-kill upload-sealed-owner ;;
    '-R legal-mcp-publisher:legal-mcp-publisher /srv/legal-mcp/uploads/'*)
      /usr/local/sbin/fixture-kill upload-owner-restored ;;
  esac
fi
exit "$status"
EOF
cat > /usr/bin/find <<'EOF'
#!/usr/bin/bash
/usr/bin/find.fixture-real "$@"
status=$?
if [[ $status -eq 0 && "$*" == '/srv/legal-mcp/uploads/'*'-type d -exec chmod 700 {} +' ]]; then
  /usr/local/sbin/fixture-kill upload-directories-restored
fi
exit "$status"
EOF
chmod 755 /usr/bin/id /usr/bin/install /usr/bin/mv /usr/bin/chown /usr/bin/find \
  /usr/bin/rm /usr/bin/rmdir /usr/bin/sync

cat > /usr/bin/findmnt <<'EOF'
#!/usr/bin/bash
if [[ "$*" == *'--output TARGET,SOURCE,FSTYPE,OPTIONS'* ]]; then
  printf '/srv/legal-mcp /dev/fixture-xfs xfs rw,noatime,nodev,noexec,nosuid\n'
elif [[ "$*" == *'--output SOURCE,FSTYPE,OPTIONS'* ]]; then
  printf '/dev/fixture-xfs xfs rw,noatime,nodev,noexec,nosuid\n'
else
  printf '/srv/legal-mcp\n'
fi
EOF
cat > /usr/sbin/blkid <<'EOF'
#!/usr/bin/bash
printf '%s\n' 11111111-2222-3333-4444-555555555555
EOF
cat > /usr/sbin/xfs_info <<'EOF'
#!/usr/bin/bash
printf 'meta-data=/dev/fixture-xfs ftype=1\ndata = bsize=4096 reflink=1\n'
EOF
chmod 755 /usr/bin/findmnt /usr/sbin/blkid /usr/sbin/xfs_info

cat > /usr/bin/systemctl <<'EOF'
#!/usr/bin/bash
unit="${2:-}"
case "$1" in
  is-enabled)
    if [[ "$unit" = legal-mcp.service ]]; then printf 'generated\n'; exit 0; fi
    [[ -e /tmp/caddy-enabled ]] && { printf 'enabled\n'; exit 0; }
    printf 'disabled\n'; exit 1
    ;;
  is-active)
    if [[ "$unit" = legal-mcp.service ]]; then state=/tmp/service-active; else state=/tmp/caddy-active; fi
    [[ -e "$state" ]] && { printf 'active\n'; exit 0; }
    printf 'inactive\n'; exit 3
    ;;
  disable)
    rm -f /tmp/caddy-enabled /tmp/caddy-active
    /usr/local/sbin/fixture-kill ingress-dark
    ;;
  enable)
    touch /tmp/caddy-enabled
    ;;
  stop)
    if [[ "$unit" = legal-mcp.service ]]; then
      rm -f /tmp/service-active
      /usr/local/sbin/fixture-kill service-dark
    else
      rm -f /tmp/caddy-active
    fi
    ;;
  start|restart)
    if [[ "$unit" = legal-mcp.service ]]; then
      /usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready
      touch /tmp/service-active
      /usr/local/sbin/fixture-kill service-started
    else
      [[ ( -e /etc/legal-mcp/auth-ready || -e /run/legal-mcp/auth-configuring ) \
        && -e /tmp/service-active ]]
      touch /tmp/caddy-enabled /tmp/caddy-active
      /usr/local/sbin/fixture-kill caddy-started
    fi
    ;;
  daemon-reload) ;;
  *) echo "unexpected systemctl: $*" >&2; exit 2 ;;
esac
EOF

cat > /usr/sbin/ufw <<'EOF'
#!/usr/bin/bash
if [[ "$1" = status ]]; then
  printf '%s\n' 'Status: active' \
    'Default: deny (incoming), allow (outgoing), disabled (routed)' \
    '22/tcp                     ALLOW IN    203.0.113.10'
  if [[ -e /tmp/ufw-web-open ]]; then
    printf '%s\n' \
      '80/tcp                     ALLOW IN    Anywhere' \
      '443/tcp                    ALLOW IN    Anywhere' \
      '80/tcp (v6)                ALLOW IN    Anywhere (v6)' \
      '443/tcp (v6)               ALLOW IN    Anywhere (v6)'
  fi
  exit 0
fi
if [[ "$*" = '--force delete allow 80/tcp comment Caddy ACME HTTP' \
  || "$*" = '--force delete allow 443/tcp comment Australian Legal MCP HTTPS' ]]; then
  rm -f /tmp/ufw-web-open
  /usr/local/sbin/fixture-kill ufw-dark
  exit 0
fi
if [[ "$1" = allow ]]; then
  touch /tmp/ufw-web-open
  [[ "$2" != 443/tcp ]] || /usr/local/sbin/fixture-kill ingress-restored
  exit 0
fi
exit 2
EOF

cat > /usr/bin/ss <<'EOF'
#!/usr/bin/bash
[[ -e /tmp/service-active ]] && printf 'LISTEN 0 4096 127.0.0.1:51235 0.0.0.0:*\n'
if [[ -e /tmp/caddy-active ]]; then
  printf 'LISTEN 0 4096 0.0.0.0:80 0.0.0.0:*\n'
  printf 'LISTEN 0 4096 0.0.0.0:443 0.0.0.0:*\n'
fi
EOF
chmod 755 /usr/bin/systemctl /usr/sbin/ufw /usr/bin/ss

# Exact Caddy adaptation expected by update-image.sh.
python3 - /tmp/caddy-adapted.json <<'PY'
import json, sys
host = "legal.example.com"
timeouts = {
    "read_timeout": 30_000_000_000, "read_header_timeout": 10_000_000_000,
    "write_timeout": 300_000_000_000, "idle_timeout": 300_000_000_000,
}
https_routes = [
    {"handle": [{"encodings": {"gzip": {}, "zstd": {}}, "handler": "encode", "prefer": ["zstd", "gzip"]}]},
    {"group": "group2", "handle": [{"handler": "subroute", "routes": [{"handle": [
        {"handler": "headers", "response": {"deferred": True, "delete": ["Server"], "set": {
            "Cache-Control": ["no-store"], "Strict-Transport-Security": ["max-age=31536000"],
            "X-Content-Type-Options": ["nosniff"],
        }}},
        {"handler": "request_body", "max_size": 1_000_000},
        {"flush_interval": -1, "handler": "reverse_proxy", "transport": {
            "dial_timeout": 5_000_000_000, "max_conns_per_host": 8, "protocol": "http",
            "read_timeout": 310_000_000_000, "response_header_timeout": 310_000_000_000,
            "write_timeout": 310_000_000_000,
        }, "upstreams": [{"dial": "127.0.0.1:51235"}]},
    ]}]}], "match": [{"path": ["/mcp", "/.well-known/oauth-protected-resource/mcp"]}]},
    {"group": "group2", "handle": [{"handler": "subroute", "routes": [{"handle": [
        {"body": "not found", "handler": "static_response", "status_code": 404}
    ]}]}]},
]
logging = {"logs": {"default": {"encoder": {"fields": {"request": {"filter": "delete"}},
    "format": "filter", "wrap": {"format": "json"}}}}}
value = {"apps": {"http": {"servers": {
    "srv0": {"listen": [":443"], **timeouts, "routes": [{"match": [{"host": [host]}],
        "handle": [{"handler": "subroute", "routes": https_routes}], "terminal": True}]},
    "srv1": {"listen": [":80"], **timeouts, "routes": [{"match": [{"host": [host]}],
        "handle": [{"handler": "subroute", "routes": [{"handle": [
            {"body": "not found", "handler": "static_response", "status_code": 404}
        ]}]}], "terminal": True}]},
}}}, "logging": logging}
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    json.dump(value, handle, separators=(",", ":"))
PY
cat > /usr/bin/caddy <<'EOF'
#!/usr/bin/bash
[[ "$1" = adapt && "$2" = --config && "$4" = --adapter && "$5" = caddyfile && "$6" = --validate ]]
cat /tmp/caddy-adapted.json
EOF
chmod 755 /usr/bin/caddy

cat > /usr/bin/curl <<'EOF'
#!/usr/bin/bash
headers=''
url=''
write_status=false
method=GET
has_data=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dump-header) headers="$2"; shift 2 ;;
    --write-out) write_status=true; shift 2 ;;
    --request) method="$2"; shift 2 ;;
    --data) has_data=true; shift 2 ;;
    --resolve|--header|--max-time|--max-redirs|--output) shift 2 ;;
    http://*|https://*) url="$1"; shift ;;
    *) shift ;;
  esac
done
status=200
body=''
case "$url" in
  http://127.0.0.1:51235/readyz)
    [[ -e /tmp/service-active ]] || exit 7
    body="{\"status\":\"ok\",\"generation\":\"$(</srv/legal-mcp/lifecycle/active-generation)\"}"
    ;;
  */.well-known/oauth-protected-resource/mcp)
    body='{"resource":"https://legal.example.com/mcp"}'
    ;;
  http://127.0.0.1:51235/mcp|https://legal.example.com/mcp)
    status=401
    [[ -z "$headers" ]] || printf '%s\r\n' \
      'HTTP/1.1 401 Unauthorized' \
      'WWW-Authenticate: ApiKey realm="australian-legal-mcp"' \
      'WWW-Authenticate: Bearer resource_metadata="https://legal.example.com/.well-known/oauth-protected-resource/mcp"' \
      > "$headers"
    if [[ "$has_data" = true && -e /etc/legal-mcp/.image-transaction ]]; then
      if [[ "$url" = http://* ]]; then
        /usr/local/sbin/fixture-kill private-proved
      else
        /usr/local/sbin/fixture-kill public-proved
      fi
    fi
    ;;
  http://legal.example.com/mcp|https://legal.example.com/*|http://legal.example.com/*)
    status=404
    ;;
  *) status=404 ;;
esac
[[ -z "$headers" || -s "$headers" ]] || printf 'HTTP/1.1 %s Fixture\r\n' "$status" > "$headers"
[[ -z "$body" ]] || printf '%s\n' "$body"
[[ "$write_status" = false ]] || printf '%s' "$status"
[[ "$status" -lt 400 ]]
EOF
chmod 755 /usr/bin/curl

cat > /usr/bin/podman <<EOF
#!/usr/bin/bash
old_image='$old_image'
target_image='$target_image'
old_id='$old_image_id'
target_id='$target_image_id'
current_image() { cat /etc/legal-mcp/image; }
image_id() { [[ "\$1" = "\$target_image" ]] && printf '%s\n' "\$target_id" || printf '%s\n' "\$old_id"; }
case "\$1" in
  image)
    case "\$2" in
      exists) [[ "\$3" = "\$old_image" || "\$3" = "\$target_image" ]] ;;
      inspect)
        image="\$3"; format="\${!#}"
        case "\$format" in
          '{{.Id}}') image_id "\$image" ;;
          '{{.Digest}}') printf '%s\n' "\${image##*@}" ;;
          *'.title'*) printf '%s\n' 'Australian Legal MCP' ;;
          *'.description'*) printf '%s\n' 'Source-grounded Australian legal MCP server' ;;
          *'.source'*) printf '%s\n' https://github.com/gunba/australian-legal-mcp ;;
          *'.version'*) [[ "\$image" = "\$target_image" ]] && printf '%s\n' '$version' || printf '%s\n' '$old_version' ;;
          *'.revision'*) [[ "\$image" = "\$target_image" ]] && printf '%s\n' '$revision' || printf '%s\n' '$old_revision' ;;
          *'.licenses'*) printf '%s\n' MIT ;;
          *'io.australian-legal-mcp.ann-format'*) printf '%s\n' flat-int8-v1 ;;
          *) exit 91 ;;
        esac
        ;;
      *) exit 91 ;;
    esac
    ;;
  pull)
    if flock -n /run/lock/legal-mcp-host-transaction.lock -c true; then
      echo 'cutover image pull escaped the shared host lock' >&2
      exit 1
    fi
    [[ "\$2" = "\$old_image" || "\$2" = "\$target_image" ]]
    ;;
  run)
    image=''; command=''; image_index=0; arguments=("\$@")
    for index in "\${!arguments[@]}"; do
      argument="\${arguments[\$index]}"
      if [[ "\$argument" = "\$old_image" || "\$argument" = "\$target_image" ]]; then
        image="\$argument"; image_index="\$index"
      fi
    done
    [[ -n "\$image" ]]
    command="\${arguments[\$((image_index + 1))]:-}"
    case "\$command" in
      --version)
        [[ "\$image" = "\$target_image" ]] \
          && printf 'legal-mcp %s\n' '$version' \
          || printf 'legal-mcp %s\n' '$old_version'
        ;;
      verify-runtime)
        printf '%s\n' '{"onnx_runtime_ready":true}'
        ;;
      activate)
        upload='/srv/legal-mcp/uploads/$target_generation'
        installed='/srv/legal-mcp/generations/$target_generation'
        [[ -d "\$upload" && ! -e "\$installed" ]]
        mv -T "\$upload" "\$installed"
        find "\$installed" -type d -exec chmod 550 {} +
        find "\$installed" -type f -exec chmod 440 {} +
        printf '%s' '$target_generation' > '$pointer'
        chown root:root '$pointer'; chmod 644 '$pointer'
        /usr/local/sbin/fixture-kill generation-switched
        ;;
      rollback)
        [[ "\${arguments[\$((image_index + 2))]}" = --generation ]]
        generation="\${arguments[\$((image_index + 3))]}"
        [[ -d "/srv/legal-mcp/generations/\$generation" ]]
        printf '%s' "\$generation" > '$pointer'
        chown root:root '$pointer'; chmod 644 '$pointer'
        /usr/local/sbin/fixture-kill generation-switched
        ;;
      verify)
        [[ " \${arguments[*]} " == *' --user=971:971 '* ]]
        [[ "\${arguments[\$((image_index + 2))]:-}" = --quiet ]]
        generation="\$(<'$pointer')"
        for argument in "\${arguments[@]}"; do
          if [[ "\$argument" == --volume=/run/legal-mcp/pair-verification/lifecycle:* ]]; then
            generation="\$(</run/legal-mcp/pair-verification/lifecycle/active-generation)"
          fi
        done
        if [[ -e /tmp/fail-target-verify && "\$image" = "\$target_image" \
          && "\$generation" = '$target_generation' ]]; then exit 86; fi
        if [[ "\$image" = "\$target_image" && "\$generation" = '$old_generation' ]]; then
          [[ -e /tmp/target-accepts-current ]] || exit 1
        elif [[ "\$image" = "\$old_image" && "\$generation" = '$target_generation' ]]; then
          exit 1
        elif [[ "\$image" = "\$target_image" && "\$generation" = '$target_generation' ]]; then
          touch /tmp/target-generation-verified
        elif [[ "\$image" != "\$old_image" || "\$generation" != '$old_generation' ]]; then
          exit 1
        fi
        ;;
      *) exit 94 ;;
    esac
    ;;
  container)
    [[ "\$2" = exists && "\$3" = australian-legal-mcp ]]
    ;;
  inspect)
    [[ "\$2" = australian-legal-mcp ]]
    format="\${!#}"
    case "\$format" in
      '{{.Image}}') image_id "\$(current_image)" ;;
      '{{.Config.User}}') printf '%s\n' 971:971 ;;
      '{{.HostConfig.ReadonlyRootfs}}') printf '%s\n' true ;;
      '{{.HostConfig.NetworkMode}}') printf '%s\n' bridge ;;
      '{{json .EffectiveCaps}}')
        touch /tmp/effective-caps-queried
        printf '%s\n' null
        ;;
      '{{json .Mounts}}') printf '%s\n' '[{"Source":"/srv/legal-mcp/generations","Destination":"/var/lib/legal-mcp/generations","RW":false},{"Source":"/srv/legal-mcp/lifecycle","Destination":"/var/lib/legal-mcp/lifecycle","RW":false},{"Source":"/srv/legal-mcp/state","Destination":"/var/lib/legal-mcp/state","RW":true},{"Source":"/etc/legal-mcp/api-keys.json","Destination":"/run/secrets/legal-mcp-api-keys.json","RW":false}]' ;;
      '{{json .HostConfig.Binds}}')
        if [[ -e /tmp/invalid-bind-options ]]; then
          printf '%s\n' '["/srv/legal-mcp/generations:/var/lib/legal-mcp/generations:ro,nodev,nosuid","/srv/legal-mcp/lifecycle:/var/lib/legal-mcp/lifecycle:ro,nodev,nosuid,noexec","/srv/legal-mcp/state:/var/lib/legal-mcp/state:rw,nodev,nosuid,noexec","/etc/legal-mcp/api-keys.json:/run/secrets/legal-mcp-api-keys.json:ro,nodev,nosuid,noexec"]'
        else
          printf '%s\n' '["/srv/legal-mcp/generations:/var/lib/legal-mcp/generations:ro,nodev,nosuid,noexec,rprivate,rbind","/srv/legal-mcp/lifecycle:/var/lib/legal-mcp/lifecycle:ro,nodev,nosuid,noexec,rprivate,rbind","/srv/legal-mcp/state:/var/lib/legal-mcp/state:rw,nodev,nosuid,noexec,rprivate,rbind","/etc/legal-mcp/api-keys.json:/run/secrets/legal-mcp-api-keys.json:ro,nodev,nosuid,noexec,rprivate,rbind"]'
        fi
        ;;
      *) exit 92 ;;
    esac
    ;;
  top)
    [[ "\$2" = australian-legal-mcp && "\$3" = capbnd \
      && "\$4" = capeff && "\$5" = capinh && "\$6" = capprm ]]
    printf '%s\n' 'BOUNDING CAPS  EFFECTIVE CAPS  INHERITED CAPS  PERMITTED CAPS'
    if [[ -e /tmp/capability-present ]]; then
      printf '%s\n' 'CAP_NET_RAW   none            none            none'
    elif [[ -e /tmp/capability-malformed ]]; then
      printf '%s\n' 'none none none'
    else
      printf '%s\n' 'none           none            none            none' \
        'none           none            none            none'
    fi
    ;;
  port)
    [[ "\$2" = australian-legal-mcp && "\$3" = 51235/tcp ]]
    printf '%s\n' 127.0.0.1:51235
    ;;
  *) exit 93 ;;
esac
EOF
chmod 755 /usr/bin/podman

cat > /tmp/legal-mcp-configure-auth <<'EOF'
#!/usr/bin/bash
set -euo pipefail
[[ $# -eq 1 && "$1" = --recover ]]
systemctl start legal-mcp.service
systemctl enable caddy.service
systemctl start caddy.service
ufw allow 80/tcp comment 'Caddy ACME HTTP' >/dev/null
ufw allow 443/tcp comment 'Australian Legal MCP HTTPS' >/dev/null
printf '%s\n' configured-dark-published
EOF
cat > /tmp/legal-mcp-publisher-command <<'EOF'
#!/usr/bin/bash
exit 99
EOF
chmod 755 /tmp/legal-mcp-configure-auth /tmp/legal-mcp-publisher-command

# Version-matched release bundle and stable V2 launcher set.
install -d -o root -g root -m 0755 \
  "$bundle/infra/hosting" "$bundle/infra/linode" "$bundle/scripts"
install -o root -g root -m 0755 /update-image "$bundle/infra/hosting/update-image.sh"
install -o root -g root -m 0755 /tmp/legal-mcp-configure-auth "$bundle/infra/hosting/configure-auth.sh"
install -o root -g root -m 0755 /host-deploy "$bundle/scripts/legal-mcp-host-deploy"
install -o root -g root -m 0755 /tmp/legal-mcp-publisher-command "$bundle/scripts/legal-mcp-publisher-command"
install -o root -g root -m 0644 /container-template "$bundle/infra/hosting/legal-mcp.container.template"
install -o root -g root -m 0644 /Caddyfile "$bundle/infra/hosting/Caddyfile"
install -o root -g root -m 0755 /install-host "$bundle/infra/linode/install-host.sh"
cat > "$bundle/Containerfile" <<EOF
ARG VERSION=$version
EOF
printf '%s\n' "$revision" > "$bundle/SOURCE_COMMIT"
cat > "$bundle/legal-mcp" <<EOF
#!/usr/bin/bash
case "\$1" in
  --version) printf '%s\n' 'legal-mcp $version' ;;
  verify-runtime) printf '%s\n' '{"onnx_runtime_ready":true}' ;;
  *) exit 1 ;;
esac
EOF
chmod 755 "$bundle/legal-mcp"
printf '%s\n' fixture > "$bundle/libonnxruntime.so"
cp -a "$bundle" "$rollback_bundle"
install -o root -g root -m 0644 /v01911-container-template \
  "$rollback_bundle/infra/hosting/legal-mcp.container.template"
cat > "$rollback_bundle/Containerfile" <<EOF
ARG VERSION=$old_version
EOF
printf '%s\n' "$old_revision" > "$rollback_bundle/SOURCE_COMMIT"
cat > "$rollback_bundle/legal-mcp" <<EOF
#!/usr/bin/bash
case "\$1" in
  --version) printf '%s\n' 'legal-mcp $old_version' ;;
  verify-runtime) printf '%s\n' '{"onnx_runtime_ready":true}' ;;
  *) exit 1 ;;
esac
EOF
chmod 755 "$rollback_bundle/legal-mcp"

real_launcher=/tmp/real-host-tool-launcher
awk '
  /^  cat <<'\''LAUNCHER'\''$/ { in_launcher=1; next }
  in_launcher && /^LAUNCHER$/ { exit }
  in_launcher { print }
' /install-host > "$real_launcher"
chmod 755 "$real_launcher"
launcher_sha="$(sha256sum "$real_launcher" | awk '{print $1}')"
configure_sha="$(sha256sum "$bundle/infra/hosting/configure-auth.sh" | awk '{print $1}')"
update_sha="$(sha256sum "$bundle/infra/hosting/update-image.sh" | awk '{print $1}')"
install -o root -g root -m 0755 "$bundle/infra/hosting/configure-auth.sh" \
  "$implementation_dir/configure-auth.$configure_sha"
install -o root -g root -m 0755 "$bundle/infra/hosting/update-image.sh" \
  "$implementation_dir/update-image.$update_sha"
printf '%s' "$configure_sha" > /etc/legal-mcp/configure-auth-implementation
printf '%s' "$update_sha" > /etc/legal-mcp/update-image-implementation
chmod 644 /etc/legal-mcp/configure-auth-implementation /etc/legal-mcp/update-image-implementation
cat > /etc/legal-mcp/host-tool-launcher <<EOF
LEGAL_MCP_HOST_TOOL_LAUNCHER_V1
LAUNCHER_SHA256=$launcher_sha
EOF
chmod 444 /etc/legal-mcp/host-tool-launcher
install -o root -g root -m 0755 /host-deploy /usr/local/sbin/legal-mcp-host-deploy
install -o root -g root -m 0755 /tmp/legal-mcp-publisher-command /usr/local/sbin/legal-mcp-publisher-command

cat > /usr/bin/unshare <<'EOF'
#!/usr/bin/bash
[[ "$1" = --mount && "$2" = --propagation && "$3" = private && "$4" = -- ]]
shift 4
exec "$@"
EOF
cat > /usr/bin/mount <<'EOF'
#!/usr/bin/bash
if [[ "$1" = --bind && $# -eq 3 ]]; then
  install -o root -g root -m 0755 "$2" "$3"
  exit 0
fi
if [[ "$1" = -o && $# -eq 3 ]]; then
  [[ "$2" = remount,bind,ro,nodev,nosuid ]]
  exit 0
fi
exit 1
EOF
chmod 755 /usr/bin/unshare /usr/bin/mount

write_generation_manifest() {
  local path="$1" schema="$2" generation="$3"
  printf '{"schema_version":%s,"generation":"%s"}\n' \
    "$schema" "$generation" > "$path"
}

restore_public_launchers() {
  install -o root -g root -m 0755 "$real_launcher" "$launcher"
  install -o root -g root -m 0755 "$real_launcher" "$configure_entry"
  install -o root -g root -m 0755 "$real_launcher" "$update_entry"
}

write_host_tool_marker() {
  local deploy_sha publisher_sha template_sha sudoers_sha
  deploy_sha="$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}')"
  publisher_sha="$(sha256sum /usr/local/sbin/legal-mcp-publisher-command | awk '{print $1}')"
  template_sha="$(sha256sum "$template" | awk '{print $1}')"
  sudoers_sha="$(sha256sum /etc/sudoers.d/legal-mcp-publisher | awk '{print $1}')"
  cat > /etc/legal-mcp/host-tools <<EOF
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
  chmod 444 /etc/legal-mcp/host-tools
}

reset_baseline() {
  /usr/bin/rm.fixture-real -rf -- "$transaction" "${transaction}.preparing" \
    "${transaction}.preparing-retired" "${transaction}.preparing-deletion" \
    "${transaction}.retiring" \
    "${transaction}.retired" "${transaction}.deletion" \
    "${transaction}.unrecognised" /etc/legal-mcp/.pair-transaction-build \
    /etc/legal-mcp/.pair-image.preparing \
    /etc/containers/systemd/.pair-quadlet.preparing \
    /usr/local/libexec/legal-mcp/.pair-template.preparing \
    /etc/legal-mcp/.auth-transaction* /etc/legal-mcp/.host-tools-transaction* \
    /run/legal-mcp/* /srv/legal-mcp/generations/* /srv/legal-mcp/uploads/* \
    /srv/legal-mcp/state/* "$journal" /srv/legal-mcp/lifecycle/.deployment-transaction.preparing \
    "$pointer" "$auth_ready" /tmp/kill-cutover-at /tmp/fail-target-verify \
    /tmp/target-accepts-current /tmp/target-generation-verified \
    /tmp/capability-present /tmp/capability-malformed /tmp/effective-caps-queried \
    /tmp/invalid-service-identity /tmp/invalid-bind-options \
    /tmp/service-active /tmp/caddy-active /tmp/caddy-enabled /tmp/ufw-web-open
  restore_public_launchers
  install -o root -g root -m 0755 /tmp/legal-mcp-publisher-command \
    /usr/local/sbin/legal-mcp-publisher-command
  printf '%s' "$configure_sha" > /etc/legal-mcp/configure-auth-implementation
  printf '%s' "$update_sha" > /etc/legal-mcp/update-image-implementation
  chmod 644 /etc/legal-mcp/configure-auth-implementation \
    /etc/legal-mcp/update-image-implementation
  install -o root -g root -m 0644 /container-template "$template"
  sed "s|__IMAGE_DIGEST__|$old_image|g" "$template" > "$quadlet"
  chown root:root "$quadlet"; chmod 644 "$quadlet"
  printf '%s\n' "$old_image" > "$image_file"; chmod 600 "$image_file"
  cat > /etc/legal-mcp/runtime.env <<'EOF'
LEGAL_MCP_HTTP_AUTH=entra
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
EOF
  chmod 600 /etc/legal-mcp/runtime.env
  printf '%s\n' '{"keys":[],"version":1}' > /etc/legal-mcp/api-keys.json
  chown legal-mcp:legal-mcp /etc/legal-mcp/api-keys.json; chmod 400 /etc/legal-mcp/api-keys.json
  sed 's/__PUBLIC_HOST__/legal.example.com/g' /Caddyfile > /etc/caddy/Caddyfile
  chown root:caddy /etc/caddy/Caddyfile; chmod 640 /etc/caddy/Caddyfile
  printf '%s\n' 203.0.113.10 > /etc/legal-mcp/admin-source-ip; chmod 600 /etc/legal-mcp/admin-source-ip
  install -d -o root -g legal-mcp -m 0550 "/srv/legal-mcp/generations/$old_generation"
  write_generation_manifest "/srv/legal-mcp/generations/$old_generation/generation.json" \
    11 "$old_generation"
  chown root:legal-mcp "/srv/legal-mcp/generations/$old_generation/generation.json"
  chmod 440 "/srv/legal-mcp/generations/$old_generation/generation.json"
  install -d -o legal-mcp-publisher -g legal-mcp-publisher -m 0700 "/srv/legal-mcp/uploads/$target_generation"
  write_generation_manifest "/srv/legal-mcp/uploads/$target_generation/generation.json" \
    12 "$target_generation"
  chown legal-mcp-publisher:legal-mcp-publisher "/srv/legal-mcp/uploads/$target_generation/generation.json"
  chmod 600 "/srv/legal-mcp/uploads/$target_generation/generation.json"
  printf '%s' "$old_generation" > "$pointer"; chown root:root "$pointer"; chmod 644 "$pointer"
  printf '%s\n%s\nprepared\n' "$target_generation" "$old_generation" > "$journal"
  chown root:root "$journal"; chmod 600 "$journal"
  printf '%s\n' "$target_generation" > /run/legal-mcp/authorized-upload
  chown root:legal-mcp-publisher /run/legal-mcp/authorized-upload
  chmod 440 /run/legal-mcp/authorized-upload
  cat > /etc/sudoers.d/legal-mcp-publisher <<EOF
Defaults:legal-mcp-publisher !requiretty
legal-mcp-publisher ALL=(root) NOPASSWD: sha256:$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}') /usr/local/sbin/legal-mcp-host-deploy ^prepare [0-9a-f]{64}\$, sha256:$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}') /usr/local/sbin/legal-mcp-host-deploy ^activate [0-9a-f]{64}\$, sha256:$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}') /usr/local/sbin/legal-mcp-host-deploy ^abort [0-9a-f]{64}\$
EOF
  chmod 440 /etc/sudoers.d/legal-mcp-publisher
  visudo -cf /etc/sudoers.d/legal-mcp-publisher >/dev/null
  write_host_tool_marker
}

reset_installed_target() {
  reset_baseline
  /usr/bin/rm.fixture-real -f "$journal" /run/legal-mcp/authorized-upload
  /usr/bin/mv.fixture-real "/srv/legal-mcp/uploads/$target_generation" \
    "/srv/legal-mcp/generations/$target_generation"
  chown -R root:legal-mcp "/srv/legal-mcp/generations/$target_generation"
  find "/srv/legal-mcp/generations/$target_generation" -type d -exec chmod 550 {} +
  find "/srv/legal-mcp/generations/$target_generation" -type f -exec chmod 440 {} +
  printf '%s' "$target_generation" > "$pointer"
  printf '%s\n' "$target_image" > "$image_file"
  sed "s|__IMAGE_DIGEST__|$target_image|g" "$template" > "$quadlet"
  chown root:root "$pointer" "$image_file" "$quadlet"
  chmod 644 "$pointer" "$quadlet"
  chmod 600 "$image_file"
}

run_cutover() {
  local status
  restore_public_launchers
  if "$update_entry" --pair-cutover \
    --generation "$target_generation" \
    --expected-current-generation "$old_generation" \
    --image "$target_image" --version "$version" \
    --template "$bundle/infra/hosting/legal-mcp.container.template" \
    <<< "$probe_key"; then
    status=0
  else
    status=$?
  fi
  restore_public_launchers
  return "$status"
}

run_cutover_from_public() {
  local status
  restore_public_launchers
  if "$update_entry" --pair-cutover --from-public \
    --generation "$target_generation" \
    --expected-current-generation "$old_generation" \
    --image "$target_image" --version "$version" \
    --template "$bundle/infra/hosting/legal-mcp.container.template" \
    <<< "$probe_key"; then
    status=0
  else
    status=$?
  fi
  restore_public_launchers
  return "$status"
}

run_pair_rollback() {
  local status
  restore_public_launchers
  if "$update_entry" --pair-rollback "$@" \
    --generation "$old_generation" \
    --expected-current-generation "$target_generation" \
    --image "$old_image" --version "$old_version" \
    --template "$rollback_bundle/infra/hosting/legal-mcp.container.template" \
    <<< "$probe_key"; then
    status=0
  else
    status=$?
  fi
  restore_public_launchers
  return "$status"
}

run_recovery() {
  local status
  if [[ ! -e /tmp/preserve-run-after-kill ]]; then
    /usr/bin/rm.fixture-real -rf /run/legal-mcp/host-tool-launcher-dispatch* \
      /run/legal-mcp/pair-cutover-starting /run/legal-mcp/pair-cutover-start-armed
  fi
  restore_public_launchers
  if "$update_entry" --recover --pair-cutover \
    <<< "$probe_key"; then status=0; else status=$?; fi
  restore_public_launchers
  return "$status"
}

make_public() {
  install -o root -g root -m 0444 /dev/null "$auth_ready"
  touch /tmp/service-active /tmp/caddy-active /tmp/caddy-enabled /tmp/ufw-web-open
}

assert_dark_pair() {
  local expected="$1" expected_image expected_generation
  if [[ "$expected" = saved ]]; then
    expected_image="$old_image"; expected_generation="$old_generation"
    mapfile -t deployment < "$journal"
    [[ ${#deployment[@]} -eq 3 \
      && "${deployment[0]}" = "$target_generation" \
      && "${deployment[1]}" = "$old_generation" \
      && "${deployment[2]}" = prepared \
      && -d "/srv/legal-mcp/uploads/$target_generation" \
      && ! -e "/srv/legal-mcp/generations/$target_generation" \
      && "$(</run/legal-mcp/authorized-upload)" = "$target_generation" \
      && "$(stat -c '%U:%G:%a' /run/legal-mcp/authorized-upload)" \
        = root:legal-mcp-publisher:440 \
      && "$(stat -c '%U:%G:%a' "/srv/legal-mcp/uploads/$target_generation")" \
        = legal-mcp-publisher:legal-mcp-publisher:700 ]]
  elif [[ "$expected" = target ]]; then
    expected_image="$target_image"; expected_generation="$target_generation"
    [[ ! -e "$journal" && ! -e /run/legal-mcp/authorized-upload \
      && -d "/srv/legal-mcp/generations/$old_generation" \
      && -d "/srv/legal-mcp/generations/$target_generation" ]]
  else
    [[ "$expected" = rolled-back ]]
    expected_image="$old_image"; expected_generation="$old_generation"
    [[ ! -e "$journal" && ! -e /run/legal-mcp/authorized-upload \
      && -d "/srv/legal-mcp/generations/$old_generation" \
      && -d "/srv/legal-mcp/generations/$target_generation" ]]
  fi
  [[ "$(<"$image_file")" = "$expected_image" \
    && "$(<"$pointer")" = "$expected_generation" \
    && ! -e "$auth_ready" && ! -e /tmp/service-active \
    && ! -e /tmp/caddy-active && ! -e /tmp/caddy-enabled && ! -e /tmp/ufw-web-open \
    && ! -e "$transaction" && ! -e "${transaction}.preparing" \
    && ! -e "${transaction}.preparing-retired" \
    && ! -e "${transaction}.preparing-deletion" \
    && ! -e "${transaction}.retiring" && ! -e "${transaction}.retired" \
    && ! -e "${transaction}.deletion" \
    && ! -e /etc/legal-mcp/.pair-transaction-build \
    && ! -e /etc/legal-mcp/.pair-image.preparing \
    && ! -e /etc/containers/systemd/.pair-quadlet.preparing \
    && ! -e /usr/local/libexec/legal-mcp/.pair-template.preparing \
    && ! -e /run/legal-mcp/pair-cutover-starting \
    && ! -e /run/legal-mcp/pair-cutover-start-armed \
    && ! -e /run/legal-mcp/host-tool-launcher-dispatch ]]
  rendered="$(mktemp)"
  sed "s|__IMAGE_DIGEST__|$expected_image|g" "$template" > "$rendered"
  cmp --silent "$rendered" "$quadlet"
  /usr/bin/rm.fixture-real -f "$rendered"
}

assert_published_target() {
  [[ "$(<"$image_file")" = "$target_image" \
    && "$(<"$pointer")" = "$target_generation" \
    && -e "$auth_ready" && -e /tmp/service-active \
    && -e /tmp/caddy-active && -e /tmp/caddy-enabled \
    && -e /tmp/ufw-web-open && ! -e "$transaction" \
    && ! -e "$journal" && ! -e /run/legal-mcp/authorized-upload ]]
}

assert_published_old() {
  [[ "$(<"$image_file")" = "$old_image" \
    && "$(<"$pointer")" = "$old_generation" \
    && -e "$auth_ready" && -e /tmp/service-active \
    && -e /tmp/caddy-active && -e /tmp/caddy-enabled \
    && -e /tmp/ufw-web-open && ! -e "$transaction" \
    && -e "$journal" && "$(</run/legal-mcp/authorized-upload)" = "$target_generation" ]]
}

assert_published_failure_is_closed() {
  local expected_service="${1:-inactive}" directory
  [[ "$expected_service" = inactive || "$expected_service" = active ]]
  for directory in "$transaction" "${transaction}.retiring" \
    "${transaction}.retired"; do
    if [[ -f "$directory/retirement-outcome" \
      && "$(<"$directory/retirement-outcome")" = pending ]]; then
      [[ ! -e "$auth_ready" \
        && ! -e /tmp/caddy-active && ! -e /tmp/caddy-enabled \
        && ! -e /tmp/ufw-web-open ]]
      if [[ "$expected_service" = active ]]; then
        [[ -e /tmp/service-active ]]
      else
        [[ ! -e /tmp/service-active ]]
      fi
    fi
  done
}

# Foreign work and every unknown image-transaction suffix fail before mutation.
for foreign in /etc/legal-mcp/.auth-transaction \
  /etc/legal-mcp/.image-transaction.preparing /etc/legal-mcp/.image-transaction \
  /etc/legal-mcp/.image-transaction.unrecognised \
  /etc/legal-mcp/.host-tools-transaction; do
  reset_baseline
  mkdir -m 700 "$foreign"
  if run_cutover >/tmp/foreign.stdout 2>/tmp/foreign.stderr; then
    echo "accepted foreign transaction: $foreign" >&2
    exit 1
  fi
  [[ ! -e "$auth_ready" && ! -e /tmp/service-active && -e "$journal" \
    && "$(</run/legal-mcp/authorized-upload)" = "$target_generation" \
    && "$(<"$pointer")" = "$old_generation" && "$(<"$image_file")" = "$old_image" ]]
done

# Ordinary publisher and corpus routes also reject an unknown image state.
reset_baseline
mkdir -m 700 /etc/legal-mcp/.image-transaction.unrecognised
if SSH_ORIGINAL_COMMAND="prepare $target_generation" /publisher-command \
  >/tmp/publisher-foreign.stdout 2>/tmp/publisher-foreign.stderr; then
  echo 'publisher accepted unknown image transaction state' >&2
  exit 1
fi
if /host-deploy prepare "$target_generation" \
  >/tmp/deploy-foreign.stdout 2>/tmp/deploy-foreign.stderr; then
  echo 'ordinary host deploy accepted unknown image transaction state' >&2
  exit 1
fi

# Explicit current-pair and incompatibility checks reject mismatches and replay.
reset_baseline
restore_public_launchers
if "$update_entry" --pair-cutover \
  --generation "$target_generation" \
  --expected-current-generation cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc \
  --image "$target_image" --version "$version" \
  --template "$bundle/infra/hosting/legal-mcp.container.template" \
  <<< "$probe_key" >/tmp/current-mismatch.stdout 2>/tmp/current-mismatch.stderr; then
  echo 'pair cutover accepted a mismatched current generation' >&2
  exit 1
fi
restore_public_launchers
[[ ! -e "$transaction" && "$(<"$pointer")" = "$old_generation" ]]

reset_baseline
printf 'LEGAL_MCP_HOST_V1\nVOLUME_UUID=99999999-2222-3333-4444-555555555555\n' \
  > /etc/legal-mcp/host-installed
chmod 444 /etc/legal-mcp/host-installed
if run_cutover >/tmp/foreign-volume.stdout 2>/tmp/foreign-volume.stderr; then
  echo 'pair cutover accepted a corpus volume bound to another host' >&2
  exit 1
fi
printf 'LEGAL_MCP_HOST_V1\nVOLUME_UUID=11111111-2222-3333-4444-555555555555\n' \
  > /etc/legal-mcp/host-installed
chmod 444 /etc/legal-mcp/host-installed
[[ ! -e "$transaction" && "$(<"$pointer")" = "$old_generation" ]]

reset_baseline
touch /tmp/target-accepts-current
if run_cutover >/tmp/compatible-target.stdout 2>/tmp/compatible-target.stderr; then
  echo 'pair cutover accepted an image compatible with the current generation' >&2
  exit 1
fi
grep -Fq 'target image accepts the current generation' /tmp/compatible-target.stderr
[[ ! -e "$transaction" && "$(<"$pointer")" = "$old_generation" ]]

reset_baseline
/usr/bin/rm.fixture-real -f /run/legal-mcp/authorized-upload
if run_cutover >/tmp/missing-authorization.stdout 2>/tmp/missing-authorization.stderr; then
  echo 'pair cutover accepted an unauthorised prepared upload' >&2
  exit 1
fi
[[ ! -e "$transaction" && "$(<"$pointer")" = "$old_generation" ]]

# Recovery never adopts a foreign preparation or another transaction kind.
reset_baseline
mkdir -m 700 /etc/legal-mcp/.image-transaction.preparing
printf '%s\n' foreign > /etc/legal-mcp/.image-transaction.preparing/payload
if run_recovery >/tmp/foreign-recovery.stdout 2>/tmp/foreign-recovery.stderr; then
  echo 'pair recovery adopted a foreign image preparation' >&2
  exit 1
fi
[[ -f /etc/legal-mcp/.image-transaction.preparing/payload ]]

reset_baseline
mkdir -m 700 "$transaction"
printf '%s\n' LEGAL_MCP_IMAGE_TRANSACTION_V2 > "$transaction/kind"
chmod 600 "$transaction/kind"
if run_recovery >/tmp/kind-recovery.stdout 2>/tmp/kind-recovery.stderr; then
  echo 'pair recovery adopted an ordinary image transaction' >&2
  exit 1
fi
[[ "$(<"$transaction/kind")" = LEGAL_MCP_IMAGE_TRANSACTION_V2 ]]

reset_baseline
printf '%s\n' FOREIGN_IMAGE_OPERATION > "${transaction}.deletion"
chmod 600 "${transaction}.deletion"
if run_recovery >/tmp/foreign-deletion.stdout 2>/tmp/foreign-deletion.stderr; then
  echo 'pair recovery adopted a foreign deletion owner marker' >&2
  exit 1
fi
[[ "$(<"${transaction}.deletion")" = FOREIGN_IMAGE_OPERATION ]]

reset_baseline
mkdir -m 700 "${transaction}.retired"
if run_recovery >/tmp/missing-retired-owner.stdout \
  2>/tmp/missing-retired-owner.stderr; then
  echo 'pair recovery guessed the owner of empty retired state' >&2
  exit 1
fi
[[ -d "${transaction}.retired" ]]

# Fixed host identities are checked before the first pair mutation.
reset_baseline
touch /tmp/invalid-service-identity
if run_cutover >/tmp/identity-drift.stdout 2>/tmp/identity-drift.stderr; then
  echo 'pair cutover accepted fixed host identity drift' >&2
  exit 1
fi
/usr/bin/rm.fixture-real -f /tmp/invalid-service-identity
assert_dark_pair saved

# The committed live container must expose all four exact hardened bind mounts.
reset_baseline
touch /tmp/invalid-bind-options
if run_cutover >/tmp/invalid-bind.stdout 2>/tmp/invalid-bind.stderr; then
  echo 'pair cutover accepted a live bind without nodev,nosuid,noexec' >&2
  exit 1
fi
/usr/bin/rm.fixture-real -f /tmp/invalid-bind-options
if [[ -e "$transaction" || -e "${transaction}.retiring" \
  || -e "${transaction}.retired" || -e "${transaction}.deletion" ]]; then
  recovery_output="$(run_recovery)"
  [[ "$recovery_output" == *'rolled back the image and generation together'* \
    || "$recovery_output" == *'transaction retirement completed'* ]]
fi
assert_dark_pair saved

# A target validation failure restores the exact ordinary prepared upload.
reset_baseline
touch /tmp/fail-target-verify
if run_cutover >/tmp/validation-rollback.stdout 2>/tmp/validation-rollback.stderr; then
  echo 'target generation validation failure unexpectedly committed' >&2
  exit 1
fi
assert_dark_pair saved

reset_baseline
touch /tmp/invalid-bind-options
printf '%s\n' sealed-upload-moved > /tmp/kill-cutover-at
if run_cutover >/tmp/sealed-upload-moved.stdout 2>/tmp/sealed-upload-moved.stderr; then
  echo 'sealed upload move kill point unexpectedly returned success' >&2
  exit 1
fi
/usr/bin/rm.fixture-real -f /tmp/kill-cutover-at /tmp/invalid-bind-options
recovery_output="$(run_recovery)"
[[ "$recovery_output" == *'rolled back the image and generation together'* ]]
assert_dark_pair saved

for point in upload-owner-restored upload-directories-restored; do
  reset_baseline
  touch /tmp/fail-target-verify
  printf '%s\n' "$point" > /tmp/kill-cutover-at
  if run_cutover >"/tmp/$point.stdout" 2>"/tmp/$point.stderr"; then
    echo "pair rollback ownership kill point unexpectedly returned success: $point" >&2
    exit 1
  fi
  /usr/bin/rm.fixture-real -f /tmp/kill-cutover-at /tmp/fail-target-verify
  recovery_output="$(run_recovery)"
  [[ "$recovery_output" == *'rolled back the image and generation together'* ]]
  assert_dark_pair saved
done

# Malformed or non-empty live capability evidence never commits. Recovery is
# possible after the evidence fault is removed and keeps ingress closed.
for capability_state in capability-present capability-malformed; do
  reset_baseline
  touch "/tmp/$capability_state"
  if run_cutover >"/tmp/$capability_state.stdout" \
    2>"/tmp/$capability_state.stderr"; then
    echo "pair cutover accepted invalid capability evidence: $capability_state" >&2
    exit 1
  fi
  [[ -d "$transaction" && "$(<"$transaction/retirement-outcome")" = pending ]]
  /usr/bin/rm.fixture-real -f "/tmp/$capability_state"
  recovery_output="$(run_recovery)"
  [[ "$recovery_output" == *'rolled back the image and generation together'* ]]
  assert_dark_pair saved
done

# Preparation retirement keeps its owner marker until an identity-preserving
# deletion marker is durable. Repeated power loss at every deletion step resumes.
for point in preparation-delete-partial preparation-deletion-marked \
  preparation-retired-removed preparation-deleted; do
  printf 'pair fixture preparation-retirement kill point: %s\n' "$point" >&2
  reset_baseline
  printf '%s\n' transaction-synced > /tmp/kill-cutover-at
  if run_cutover >/tmp/preparation-seed.stdout 2>/tmp/preparation-seed.stderr; then
    echo 'preparation seed kill unexpectedly returned success' >&2
    exit 1
  fi
  printf '%s\n' "$point" > /tmp/kill-cutover-at
  if run_recovery >/tmp/preparation-retire.stdout 2>/tmp/preparation-retire.stderr; then
    echo "preparation retirement kill unexpectedly returned success: $point" >&2
    exit 1
  fi
  /usr/bin/rm.fixture-real -f /tmp/kill-cutover-at
  if [[ "$point" != preparation-deleted ]]; then
    recovery_output="$(run_recovery)"
    [[ "$recovery_output" == *'preparation retirement completed'* \
      || "$recovery_output" == *'preparation discarded before host mutation'* ]]
  else
    /usr/bin/rm.fixture-real -rf /run/legal-mcp/host-tool-launcher-dispatch* \
      /run/legal-mcp/pair-cutover-starting /run/legal-mcp/pair-cutover-start-armed
  fi
  assert_dark_pair saved
done

# Every durable phase before the outcome decision recovers the prior pair.
# Alternating cases remove /run state to model reboot; temporary permits never
# become durable authority.
precommit_points=(
  transaction-created transaction-build-partial transaction-build-torn transaction-synced
  transaction-published phase-prepared
  phase-darkening ingress-dark upload-revoked phase-dark
  phase-sealing upload-sealed-owner phase-sealed phase-target-files
  image-prepared image-switched quadlet-prepared quadlet-switched
  template-prepared template-switched phase-activating
  deployment-journal-prepared deployment-journal-published generation-switched
  phase-activated phase-verifying service-started private-proved service-dark
  phase-proved outcome-prepared
)
index=0
for point in "${precommit_points[@]}"; do
  printf 'pair fixture pre-commit kill point: %s\n' "$point" >&2
  reset_baseline
  if (( index % 2 == 0 )); then
    touch /tmp/preserve-run-after-kill
  else
    /usr/bin/rm.fixture-real -f /tmp/preserve-run-after-kill
  fi
  printf '%s\n' "$point" > /tmp/kill-cutover-at
  if run_cutover >/tmp/kill.stdout 2>/tmp/kill.stderr; then
    echo "pair kill point unexpectedly returned success: $point" >&2
    exit 1
  fi
  /usr/bin/rm.fixture-real -f /tmp/kill-cutover-at
  if [[ "$point" = transaction-build-torn ]]; then
    printf '%s\n' 'torn-owner-marker' \
      > /etc/legal-mcp/.pair-transaction-build/kind
  fi
  if [[ "$point" = service-started && -e /tmp/preserve-run-after-kill ]]; then
    # A launcher-only SIGKILL cannot synchronously stop the already-started
    # private loopback service. Recovery below must stop it before returning.
    assert_published_failure_is_closed active
  else
    assert_published_failure_is_closed
  fi
  if (( index % 2 == 1 )); then
    /usr/bin/rm.fixture-real -rf /run/legal-mcp/host-tool-launcher-dispatch* \
      /run/legal-mcp/pair-cutover-starting /run/legal-mcp/pair-cutover-start-armed
  fi
  if [[ ! -e /tmp/preserve-run-after-kill && -e "$transaction" \
    && "$(<"$transaction/retirement-outcome")" = pending ]]; then
    /usr/bin/rm.fixture-real -rf /run/legal-mcp/host-tool-launcher-dispatch* \
      /run/legal-mcp/pair-cutover-starting /run/legal-mcp/pair-cutover-start-armed
    if "$launcher" --check-auth-ready; then
      echo "pending pair transaction survived reboot as start authority: $point" >&2
      exit 1
    fi
  fi
  recovery_output="$(run_recovery)"
  [[ "$recovery_output" == *'preparation discarded before host mutation'* \
    || "$recovery_output" == *'rolled back the image and generation together'* ]]
  assert_dark_pair saved
  /usr/bin/rm.fixture-real -f /tmp/preserve-run-after-kill
  index=$((index + 1))
done

# Once the outcome is durable, recovery finishes the already-proved target.
postcommit_points=(
  target-committed phase-committing corpus-committed phase-committed
  transaction-retiring transaction-retired transaction-delete-partial
  transaction-deletion-marked transaction-retired-removed transaction-deleted
)
index=0
for point in "${postcommit_points[@]}"; do
  printf 'pair fixture post-commit kill point: %s\n' "$point" >&2
  reset_baseline
  if (( index % 2 == 0 )); then
    touch /tmp/preserve-run-after-kill
  else
    /usr/bin/rm.fixture-real -f /tmp/preserve-run-after-kill
  fi
  printf '%s\n' "$point" > /tmp/kill-cutover-at
  if run_cutover >/tmp/kill.stdout 2>/tmp/kill.stderr; then
    echo "pair post-commit kill point unexpectedly returned success: $point" >&2
    exit 1
  fi
  /usr/bin/rm.fixture-real -f /tmp/kill-cutover-at
  assert_published_failure_is_closed
  if [[ "$point" = transaction-deleted ]]; then
    /usr/bin/rm.fixture-real -rf /run/legal-mcp/host-tool-launcher-dispatch* \
      /run/legal-mcp/pair-cutover-starting /run/legal-mcp/pair-cutover-start-armed
    assert_dark_pair target
    /usr/bin/rm.fixture-real -f /tmp/preserve-run-after-kill
    index=$((index + 1))
    continue
  fi
  recovery_output="$(run_recovery)"
  [[ "$recovery_output" == *'completed the committed target pair'* \
    || "$recovery_output" == *'transaction retirement completed'* ]]
  assert_dark_pair target
  /usr/bin/rm.fixture-real -f /tmp/preserve-run-after-kill
  index=$((index + 1))
done

# Public-to-dark has additional durable mutations. Exercise every public-only
# darkening boundary; all later pair phases are covered by the common matrix.
for point in auth-ready-dark service-dark ingress-dark ufw-dark; do
  printf 'pair fixture public darkening kill point: %s\n' "$point" >&2
  reset_baseline
  make_public
  printf '%s\n' "$point" > /tmp/kill-cutover-at
  if run_cutover_from_public >/tmp/public-kill.stdout 2>/tmp/public-kill.stderr; then
    echo "public pair kill point unexpectedly returned success: $point" >&2
    exit 1
  fi
  /usr/bin/rm.fixture-real -f /tmp/kill-cutover-at
  recovery_output="$(run_recovery)"
  [[ "$recovery_output" == *'rolled back the image and generation together'* ]]
  assert_dark_pair saved
done

# Public maintenance requires explicit authority and always returns dark.
reset_baseline
make_public
printf '%s\n' transaction-created > /tmp/kill-cutover-at
if run_cutover_from_public >/tmp/public-preparation-kill.stdout \
  2>/tmp/public-preparation-kill.stderr; then
  echo 'public pair preparation kill point unexpectedly returned success' >&2
  exit 1
fi
/usr/bin/rm.fixture-real -f /tmp/kill-cutover-at
recovery_output="$(run_recovery)"
[[ "$recovery_output" == *'preparation discarded before host mutation'* ]]
assert_dark_pair saved

reset_baseline
make_public
if run_cutover >/tmp/public-without-flag.stdout 2>/tmp/public-without-flag.stderr; then
  echo 'pair cutover darkened a public host without --from-public' >&2
  exit 1
fi
assert_published_old
success_output="$(run_cutover_from_public)"
[[ "$success_output" = "image/generation pair committed: $target_image $target_generation; public ingress remains closed" ]]
[[ -e /tmp/target-generation-verified && ! -e /tmp/effective-caps-queried ]]
assert_dark_pair target

# Replaying the consumed prepared cutover is rejected without changing target.
if run_cutover >/tmp/replay.stdout 2>/tmp/replay.stderr; then
  echo 'completed prepared pair transaction was replayed' >&2
  exit 1
fi
assert_dark_pair target

# Installed-pair rollback uses the same crash-safe coordinator. Every durable
# phase before its decision restores the installed schema-12 pair.
rollback_precommit_points=(
  transaction-created transaction-build-partial transaction-synced
  transaction-published phase-prepared
  phase-darkening ingress-dark phase-dark phase-sealing phase-sealed
  phase-target-files image-prepared image-switched quadlet-prepared
  quadlet-switched template-prepared template-switched phase-activating
  generation-switched phase-activated phase-verifying service-started
  private-proved service-dark phase-proved outcome-prepared
)
for point in "${rollback_precommit_points[@]}"; do
  printf 'pair fixture rollback pre-commit kill point: %s\n' "$point" >&2
  reset_installed_target
  printf '%s\n' "$point" > /tmp/kill-cutover-at
  if run_pair_rollback >/tmp/rollback-kill.stdout 2>/tmp/rollback-kill.stderr; then
    echo "pair rollback kill point unexpectedly returned success: $point" >&2
    exit 1
  fi
  /usr/bin/rm.fixture-real -f /tmp/kill-cutover-at
  recovery_output="$(run_recovery)"
  [[ "$recovery_output" == *'preparation discarded before host mutation'* \
    || "$recovery_output" == *'rolled back the image and generation together'* ]]
  assert_dark_pair target
done

# After the rollback decision, recovery completes the already-proved v0.19.11
# image/schema-11 pair and retains both immutable generations.
rollback_postcommit_points=(
  target-committed phase-committing phase-committed transaction-retiring
  transaction-retired transaction-delete-partial transaction-deletion-marked
  transaction-retired-removed transaction-deleted
)
for point in "${rollback_postcommit_points[@]}"; do
  printf 'pair fixture rollback post-commit kill point: %s\n' "$point" >&2
  reset_installed_target
  printf '%s\n' "$point" > /tmp/kill-cutover-at
  if run_pair_rollback >/tmp/rollback-post-kill.stdout 2>/tmp/rollback-post-kill.stderr; then
    echo "pair rollback post-commit kill unexpectedly returned success: $point" >&2
    exit 1
  fi
  /usr/bin/rm.fixture-real -f /tmp/kill-cutover-at
  if [[ "$point" != transaction-deleted ]]; then
    recovery_output="$(run_recovery)"
    [[ "$recovery_output" == *'completed the committed target pair'* \
      || "$recovery_output" == *'transaction retirement completed'* ]]
  else
    /usr/bin/rm.fixture-real -rf /run/legal-mcp/host-tool-launcher-dispatch* \
      /run/legal-mcp/pair-cutover-starting /run/legal-mcp/pair-cutover-start-armed
  fi
  assert_dark_pair rolled-back
done

# The explicit paired rollback retains both immutable generations, supports
# public-to-dark maintenance, and restores the v0.19.11/schema-11 pair.
reset_installed_target
make_public
if run_pair_rollback >/tmp/rollback-public-without-flag.stdout \
  2>/tmp/rollback-public-without-flag.stderr; then
  echo 'pair rollback darkened a public host without --from-public' >&2
  exit 1
fi
assert_published_target
rollback_output="$(run_pair_rollback --from-public)"
[[ "$rollback_output" = "image/generation pair committed: $old_image $old_generation; public ingress remains closed" ]]
assert_dark_pair rolled-back

echo image-generation-pair-cutover-fixture-ok
