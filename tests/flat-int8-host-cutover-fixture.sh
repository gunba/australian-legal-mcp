#!/usr/bin/env bash
# Exhaustive disposable-host fixture for the one coordinated Arroy-v20 to
# flat-int8 image/generation transition. System boundaries are deterministic
# fakes; the production updater, host-deploy helper, and generated stable
# launcher are exercised.
set -euo pipefail
umask 027
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

[[ $EUID -eq 0 && -x /update-image && -x /host-deploy && -f /install-host \
  && -f /container-template && -f /Caddyfile && -f /publisher-command \
  && -x /v0198-update-image && -x /v0198-configure-auth ]] || {
  echo 'fixture requires the production hosting inputs in a disposable root container' >&2
  exit 2
}

version=0.19.9
revision=1111111111111111111111111111111111111111
old_revision=2222222222222222222222222222222222222222
old_generation=a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3
target_generation=937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939
probe_key="fixture.$(printf 'A%.0s' {1..43})"
old_image="ghcr.io/gunba/australian-legal-mcp@sha256:2f2abc22cc0bd0eb2aae2bc32f4e79ebc58b1ac0852316240f3acdf6a2e5efd9"
target_image="ghcr.io/gunba/australian-legal-mcp@sha256:008de908c49b4975eba0f7601e6a554b27ede8202a9e5fe26197c6221b03e3f0"
old_image_id="sha256:dd543ce13fafad18d522652ba80404e3fd870f277f9644c3820fe726aa5584c3"
target_image_id="sha256:a1800bbc97dab9ebf158dec851d737d1dbeb4e42a4facc360696ea12353b98e7"
transaction=/etc/legal-mcp/.image-transaction
journal=/srv/legal-mcp/lifecycle/.deployment-transaction
pointer=/srv/legal-mcp/lifecycle/active-generation
auth_ready=/etc/legal-mcp/auth-ready
authorization=/run/legal-mcp/authorized-upload
image_file=/etc/legal-mcp/image
template=/usr/local/libexec/legal-mcp/legal-mcp.container.template
quadlet=/etc/containers/systemd/legal-mcp.container
launcher=/usr/local/libexec/legal-mcp/host-tool-launcher
update_entry=/usr/local/sbin/legal-mcp-update-image
configure_entry=/usr/local/sbin/legal-mcp-configure-auth
implementation_dir=/usr/local/libexec/legal-mcp/host-tools
bundle=/bundle

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

# Real mutation utilities are retained behind wrappers that expose every
# durable phase boundary as a SIGKILL point.
mv /usr/bin/install /usr/bin/install.fixture-real
mv /usr/bin/mv /usr/bin/mv.fixture-real
/usr/bin/mv.fixture-real /usr/bin/chown /usr/bin/chown.fixture-real
/usr/bin/mv.fixture-real /usr/bin/find /usr/bin/find.fixture-real
/usr/bin/mv.fixture-real /usr/bin/rm /usr/bin/rm.fixture-real
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
kill_pids=()
updater_depth=0
while [[ "$candidate" =~ ^[1-9][0-9]*$ && "$candidate" -gt 1 ]]; do
  kill_pids+=("$candidate")
  mapfile -d '' -t command < "/proc/$candidate/cmdline"
  if printf '%s\n' "${command[@]}" \
    | grep -Fxq /usr/local/sbin/legal-mcp-update-image \
    && printf '%s\n' "${command[@]}" | grep -Fxq -- --flat-int8-cutover; then
    updater_pid="$candidate"
    updater_depth="${#kill_pids[@]}"
  fi
  candidate="$(awk '$1 == "PPid:" {print $2}' "/proc/$candidate/status")"
done
[[ "$updater_pid" =~ ^[1-9][0-9]*$ ]]
kill -KILL "${kill_pids[@]:0:updater_depth}"
sleep 1
EOF
chmod 755 /usr/local/sbin/fixture-kill

cat > /usr/bin/install <<'EOF'
#!/usr/bin/bash
/usr/bin/install.fixture-real "$@"
status=$?
if [[ $status -eq 0 && "${!#}" = /etc/legal-mcp/.image-transaction.flat-int8-preparing ]]; then
  /usr/local/sbin/fixture-kill transaction-created
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
    /etc/legal-mcp/auth-ready) point=auth-ready-published ;;
    /etc/legal-mcp/.image-transaction/retirement-outcome) point=target-committed ;;
    /etc/legal-mcp/.image-transaction.retiring) point=transaction-retiring ;;
    /etc/legal-mcp/.image-transaction.retired) point=transaction-retired ;;
    *) point='' ;;
  esac
  [[ -z "$point" ]] || /usr/local/sbin/fixture-kill "$point"
fi
exit "$status"
EOF
cat > /usr/bin/rm <<'EOF'
#!/usr/bin/bash
if [[ -s /tmp/kill-cutover-at \
  && "$(</tmp/kill-cutover-at)" = transaction-delete-partial \
  && "$*" == *'/etc/legal-mcp/.image-transaction.retired'* ]]; then
  directory="${!#}"
  victim="$(/usr/bin/find.fixture-real "$directory" -mindepth 1 -maxdepth 1 -print -quit)"
  [[ -n "$victim" ]]
  /usr/bin/rm.fixture-real -rf -- "$victim"
  /usr/local/sbin/fixture-kill transaction-delete-partial
fi
/usr/bin/rm.fixture-real "$@"
status=$?
if [[ $status -eq 0 ]]; then
  case "$*" in
    *'/etc/legal-mcp/auth-ready'*) /usr/local/sbin/fixture-kill auth-ready-dark ;;
    *'/run/legal-mcp/authorized-upload'*) /usr/local/sbin/fixture-kill upload-revoked ;;
    *'/srv/legal-mcp/lifecycle/.deployment-transaction'*)
      /usr/local/sbin/fixture-kill corpus-committed ;;
    *'/etc/legal-mcp/.image-transaction.retired'*) /usr/local/sbin/fixture-kill transaction-deleted ;;
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
    '-f /etc/legal-mcp/.image-transaction.flat-int8-preparing')
      /usr/local/sbin/fixture-kill transaction-synced ;;
    '-f /srv/legal-mcp/lifecycle/.deployment-transaction.preparing')
      /usr/local/sbin/fixture-kill deployment-journal-prepared ;;
    '-f /etc/legal-mcp/.image-transaction/retirement-outcome.preparing')
      /usr/local/sbin/fixture-kill outcome-prepared ;;
  esac
fi
exit "$status"
EOF
cat > /usr/bin/chown <<'EOF'
#!/usr/bin/bash
/usr/bin/chown.fixture-real "$@"
status=$?
if [[ $status -eq 0 && "$*" == '-R legal-mcp-publisher:legal-mcp-publisher /srv/legal-mcp/uploads/'* ]]; then
  /usr/local/sbin/fixture-kill upload-owner-restored
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
chmod 755 /usr/bin/install /usr/bin/mv /usr/bin/chown /usr/bin/find \
  /usr/bin/rm /usr/bin/sync

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
          *'.version'*) [[ "\$image" = "\$target_image" ]] && printf '%s\n' '$version' || printf '%s\n' 0.19.0 ;;
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
          || printf 'legal-mcp %s\n' 0.19.0
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
        ;;
      verify)
        [[ "\${arguments[\$((image_index + 2))]:-}" = --quiet ]]
        if [[ ! -e /tmp/target-accepts-arroy && "\$image" = "\$target_image" \
          && "\$(<'$pointer')" = '$old_generation' ]]; then exit 1; fi
        if [[ -e /tmp/fail-target-verify && "\$image" = "\$target_image" ]]; then exit 86; fi
        ;;
      prune-generations) ;;
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
  if [[ "$3" = /usr/bin/podman || "$3" = /usr/bin/flock ]]; then
    name="${3##*/}"
    cp -p "$3" "/tmp/${name}-before-v0198-adapter"
    temporary="$(mktemp "/usr/bin/.${name}-adapter.XXXXXX")"
    install -o root -g root -m 0500 "$2" "$temporary"
    /usr/bin/mv.fixture-real -fT "$temporary" "$3"
  else
    install -o root -g root -m 0755 "$2" "$3"
  fi
  exit 0
fi
[[ "$1" = -o && "$2" = remount,bind,ro,nodev,nosuid && $# -eq 3 ]]
EOF
chmod 755 /usr/bin/unshare /usr/bin/mount

write_generation_manifest() {
  local path="$1" format="$2" source version metric extra
  if [[ "$format" = arroy ]]; then
    version=3; metric=cosine-f32-candidates+dot-i8-rerank
    extra=',"library":"arroy","library_version":"0.6.4"'
  else
    version=1; metric=signed-int8-dot-exact; extra=''
  fi
  {
    printf '{"schema_version":11,"ann":{'
    first=true
    for source in ato frl federal-court high-court nsw-caselaw nsw-legislation \
      qld-legislation wa-legislation sa-legislation tas-legislation; do
      [[ "$first" = true ]] || printf ','
      first=false
      printf '"%s":{"source_id":"%s","format":"%s","format_version":%s,"path":"ann/%s.ann","id_encoding":"sqlite-chunk-id-u32","metric":"%s"%s}' \
        "$source" "$source" "${format/arroy/arroy-cosine-f32}" "$version" "$source" "$metric" "$extra"
    done
    printf '}}\n'
  } > "$path"
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
    "${transaction}.preparing-retired" "${transaction}.flat-int8-preparing" \
    "${transaction}.flat-int8-preparing-retired" \
    "${transaction}.retiring" "${transaction}.retired" \
    /etc/legal-mcp/.auth-transaction* /etc/legal-mcp/.host-tools-transaction* \
    /run/legal-mcp/* /srv/legal-mcp/generations/* /srv/legal-mcp/uploads/* \
    /srv/legal-mcp/state/* "$journal" /srv/legal-mcp/lifecycle/.deployment-transaction.preparing \
    "$pointer" "$auth_ready" /tmp/kill-cutover-at /tmp/fail-target-verify \
    /tmp/target-accepts-arroy /tmp/capability-present /tmp/capability-malformed \
    /tmp/effective-caps-queried \
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
  write_generation_manifest "/srv/legal-mcp/generations/$old_generation/generation.json" arroy
  chown root:legal-mcp "/srv/legal-mcp/generations/$old_generation/generation.json"
  chmod 440 "/srv/legal-mcp/generations/$old_generation/generation.json"
  install -d -o legal-mcp-publisher -g legal-mcp-publisher -m 0700 "/srv/legal-mcp/uploads/$target_generation"
  write_generation_manifest "/srv/legal-mcp/uploads/$target_generation/generation.json" flat-int8
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

run_cutover() {
  local status
  restore_public_launchers
  if "$update_entry" --flat-int8-cutover \
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

run_recovery() {
  local status
  restore_public_launchers
  if "$update_entry" --recover --flat-int8-cutover \
    <<< "$probe_key"; then status=0; else status=$?; fi
  restore_public_launchers
  return "$status"
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
  else
    expected_image="$target_image"; expected_generation="$target_generation"
    [[ ! -e "$journal" && ! -e /run/legal-mcp/authorized-upload \
      && -d "/srv/legal-mcp/generations/$target_generation" ]]
  fi
  [[ "$(<"$image_file")" = "$expected_image" \
    && "$(<"$pointer")" = "$expected_generation" \
    && ! -e "$auth_ready" && ! -e /tmp/service-active \
    && ! -e /tmp/caddy-active && ! -e /tmp/caddy-enabled && ! -e /tmp/ufw-web-open \
    && ! -e "$transaction" && ! -e "${transaction}.preparing" \
    && ! -e "${transaction}.flat-int8-preparing" \
    && ! -e "${transaction}.retiring" && ! -e "${transaction}.retired" \
    && ! -e /run/legal-mcp/flat-int8-cutover-starting \
    && ! -e /run/legal-mcp/flat-int8-cutover-start-armed \
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

assert_published_failure_is_closed() {
  local directory
  for directory in "$transaction" "${transaction}.retiring" \
    "${transaction}.retired"; do
    if [[ -f "$directory/retirement-outcome" \
      && "$(<"$directory/retirement-outcome")" = pending ]]; then
      [[ ! -e "$auth_ready" && ! -e /tmp/service-active \
        && ! -e /tmp/caddy-active && ! -e /tmp/caddy-enabled \
        && ! -e /tmp/ufw-web-open ]]
    fi
  done
}

bind_pending_transaction_to_v0198() {
  local v0198_revision=312646c34cff43f3154b43a6feb7e7f4306f30bc
  local v0198_configure=3ece47e0f27525e45188130e6ac4215fa8276f1ddaa564544653f3daed84921e
  local v0198_update=01ab7064e6d759f4f71bcf7fbeef1e04262cd262bd87f0755306f5c62664eac8
  local deploy_sha publisher_sha template_sha sudoers_sha launcher_sha
  install -o root -g root -m 0755 /v0198-configure-auth \
    "$implementation_dir/configure-auth.$v0198_configure"
  install -o root -g root -m 0755 /v0198-update-image \
    "$implementation_dir/update-image.$v0198_update"
  install -o root -g root -m 0755 /publisher-command \
    /usr/local/sbin/legal-mcp-publisher-command
  printf '%s' "$v0198_configure" > /etc/legal-mcp/configure-auth-implementation
  printf '%s' "$v0198_update" > /etc/legal-mcp/update-image-implementation
  chmod 644 /etc/legal-mcp/configure-auth-implementation \
    /etc/legal-mcp/update-image-implementation
  deploy_sha="$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}')"
  publisher_sha="$(sha256sum /usr/local/sbin/legal-mcp-publisher-command | awk '{print $1}')"
  template_sha="$(sha256sum "$template" | awk '{print $1}')"
  sudoers_sha="$(sha256sum /etc/sudoers.d/legal-mcp-publisher | awk '{print $1}')"
  launcher_sha="$(sha256sum "$launcher" | awk '{print $1}')"
  [[ "$deploy_sha" = 4e6c6181a9528852de4e22e559b71076b7d0b8ac716f35d2c5d7264ec35a4533 \
    && "$publisher_sha" = 4db458fa316e104ba4de412fdf9d4b7d5120677eba153eadd944dea37b36ad47 \
    && "$template_sha" = d323504b206938ed713271cfe6a98c263f3ad513cc6a96593aa56686352a5225 \
    && "$sudoers_sha" = a6dd6f1ea819516df66eb3cc5f7fc4999432c36c9beb27ebdfb5d6ec3ec48d70 \
    && "$launcher_sha" = 1d4bd49a571dcd9fc4c437c2cfb8470b182a556ecb381c4be5726ccaec9575da ]]
  cat > /etc/legal-mcp/host-tools <<EOF
LEGAL_MCP_HOST_TOOLS_V2
VERSION=0.19.8
SOURCE_COMMIT=$v0198_revision
HOST_DEPLOY_SHA256=$deploy_sha
PUBLISHER_COMMAND_SHA256=$publisher_sha
CONFIGURE_AUTH_SHA256=$v0198_configure
UPDATE_IMAGE_SHA256=$v0198_update
CONTAINER_TEMPLATE_SHA256=$template_sha
SUDOERS_SHA256=$sudoers_sha
EOF
  chmod 444 /etc/legal-mcp/host-tools
  printf '%s\n' 0.19.8 > "$transaction/target-version"
  printf '%s\n' "$v0198_revision" > "$transaction/target-revision"
  printf '%s\n' "$v0198_update" > "$transaction/updater-sha256"
  cat > "$transaction/release-sha256" <<EOF
UPDATE_IMAGE_SHA256=$v0198_update
CONFIGURE_AUTH_SHA256=$v0198_configure
HOST_DEPLOY_SHA256=$deploy_sha
PUBLISHER_COMMAND_SHA256=$publisher_sha
CONTAINER_TEMPLATE_SHA256=$template_sha
HOST_TOOLS_MARKER_SHA256=$(sha256sum /etc/legal-mcp/host-tools | awk '{print $1}')
HOST_TOOL_LAUNCHER_SHA256=$launcher_sha
HOST_TOOL_LAUNCHER_MARKER_SHA256=$(sha256sum /etc/legal-mcp/host-tool-launcher | awk '{print $1}')
CONFIGURE_AUTH_POINTER_SHA256=$(sha256sum /etc/legal-mcp/configure-auth-implementation | awk '{print $1}')
UPDATE_IMAGE_POINTER_SHA256=$(sha256sum /etc/legal-mcp/update-image-implementation | awk '{print $1}')
EOF
  chmod 600 "$transaction/target-version" "$transaction/target-revision" \
    "$transaction/updater-sha256" "$transaction/release-sha256"
}

# Foreign auth/image/host-tool work is rejected before any darkening or corpus mutation.
for foreign in /etc/legal-mcp/.auth-transaction \
  /etc/legal-mcp/.image-transaction.preparing /etc/legal-mcp/.image-transaction \
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

reset_baseline
touch /tmp/target-accepts-arroy
if run_cutover >/tmp/not-flat-only.stdout 2>/tmp/not-flat-only.stderr; then
  echo 'cutover accepted an image that can serve the Arroy generation' >&2
  exit 1
fi
grep -Fq 'is not flat-only' /tmp/not-flat-only.stderr
[[ ! -e "$auth_ready" && ! -e /tmp/service-active && -e "$journal" \
  && "$(</run/legal-mcp/authorized-upload)" = "$target_generation" \
  && ! -e "$transaction" && "$(<"$pointer")" = "$old_generation" ]]

# Recovery never adopts or removes the ambiguous preparation namespace owned
# by the ordinary image-update state machine.
reset_baseline
mkdir -m 700 /etc/legal-mcp/.image-transaction.preparing
printf '%s\n' foreign > /etc/legal-mcp/.image-transaction.preparing/payload
if run_recovery >/tmp/foreign-recovery.stdout 2>/tmp/foreign-recovery.stderr; then
  echo 'flat-int8 recovery adopted an ordinary image preparation' >&2
  exit 1
fi
[[ -f /etc/legal-mcp/.image-transaction.preparing/payload \
  && ! -e "$auth_ready" && ! -e /tmp/service-active \
  && "$(</run/legal-mcp/authorized-upload)" = "$target_generation" ]]

# A normal coordinated success consumes only the prepared corpus journal and
# commits the exact flat target pair while preserving configured-dark.
for capability_state in capability-present capability-malformed; do
  reset_baseline
  touch "/tmp/$capability_state"
  if run_cutover >"/tmp/$capability_state.stdout" \
    2>"/tmp/$capability_state.stderr"; then
    echo "cutover accepted invalid live capability evidence: $capability_state" >&2
    exit 1
  fi
  [[ -d "$transaction" && "$(<"$transaction/retirement-outcome")" = pending \
    && "$(<"$pointer")" = "$old_generation" \
    && "$(<"$image_file")" = "$old_image" ]]
  /usr/bin/rm.fixture-real -f "/tmp/$capability_state"
  recovery_output="$(run_recovery)"
  [[ "$recovery_output" == *'rolled back both generation and image/template'* ]]
  assert_dark_pair saved
done

restore_v0198_adapter_targets() {
  local name
  for name in podman flock; do
    if [[ -e "/tmp/${name}-before-v0198-adapter" ]]; then
      /usr/bin/mv.fixture-real -fT "/tmp/${name}-before-v0198-adapter" "/usr/bin/$name"
    fi
  done
}

# The immutable v0.19.8 updater used Podman's null EffectiveCaps field and
# therefore cannot retire its own otherwise-valid rollback. The v0.19.9
# release bridge runs that exact updater through its stable launcher, changing
# only the one incompatible observation after a live four-set capability proof.
# Production uses API-key authentication. Prove that the bridge propagates its
# standard input into the unchanged updater, while still rejecting a live
# capability after the valid key is accepted.
reset_baseline
/usr/bin/mv.fixture-real /usr/bin/python3 /usr/bin/python3.fixture-real
cat > /usr/bin/python3 <<'EOF'
#!/usr/bin/bash
if [[ "${1:-}" = -c && "${2:-}" = *urllib.request* ]]; then
  cat >/dev/null
  exit 0
fi
exec /usr/bin/python3.fixture-real "$@"
EOF
chmod 755 /usr/bin/python3
sed -i 's/LEGAL_MCP_HTTP_AUTH=entra/LEGAL_MCP_HTTP_AUTH=api-key/' \
  /etc/legal-mcp/runtime.env
touch /tmp/capability-present
if run_cutover >/tmp/v0198-api-pending.stdout 2>/tmp/v0198-api-pending.stderr; then
  echo 'failed to create the API-key pending cutover recovery state' >&2
  exit 1
fi
/usr/bin/rm.fixture-real -f /tmp/capability-present
bind_pending_transaction_to_v0198
if "$bundle/infra/linode/install-host.sh" --recover-v0198-flat-int8 \
  --version 0.19.9 </dev/null >/tmp/v0198-bridge-no-key.stdout \
  2>/tmp/v0198-bridge-no-key.stderr; then
  echo 'v0.19.8 bridge accepted missing API-key probe input' >&2
  exit 1
fi
restore_v0198_adapter_targets
restore_public_launchers
grep -Fq 'requires a probe key on standard input' /tmp/v0198-bridge-no-key.stderr
touch /tmp/capability-present
if "$bundle/infra/linode/install-host.sh" --recover-v0198-flat-int8 \
  --version 0.19.9 >/tmp/v0198-bridge-cap.stdout \
  2>/tmp/v0198-bridge-cap.stderr <<< "$probe_key"; then
  echo 'v0.19.8 bridge accepted a live process capability' >&2
  exit 1
fi
restore_v0198_adapter_targets
restore_public_launchers
[[ -e /tmp/effective-caps-queried \
  && -d "$transaction" && "$(<"$transaction/retirement-outcome")" = pending ]]
/usr/bin/rm.fixture-real -f /tmp/capability-present
/usr/bin/rm.fixture-real -f /usr/bin/python3
/usr/bin/mv.fixture-real /usr/bin/python3.fixture-real /usr/bin/python3

reset_baseline
touch /tmp/capability-present
if run_cutover >/tmp/v0198-pending.stdout 2>/tmp/v0198-pending.stderr; then
  echo 'failed to create the exact pending cutover recovery state' >&2
  exit 1
fi
/usr/bin/rm.fixture-real -f /tmp/capability-present
bind_pending_transaction_to_v0198
expect_v0198_bridge_rejected() {
  local requested_version="${1:-0.19.9}"
  if "$bundle/infra/linode/install-host.sh" --recover-v0198-flat-int8 \
    --version "$requested_version" </dev/null >/tmp/v0198-negative.stdout \
    2>/tmp/v0198-negative.stderr; then
    echo 'unsafe v0.19.8 bridge state was accepted' >&2
    exit 1
  fi
  restore_v0198_adapter_targets
  restore_public_launchers
  [[ -d "$transaction" && "$(<"$transaction/retirement-outcome")" = pending ]]
}

expect_v0198_bridge_rejected 0.19.8
printf '%s\n' 0000000000000000000000000000000000000000 \
  > "$transaction/target-revision"
expect_v0198_bridge_rejected
printf '%s\n' 312646c34cff43f3154b43a6feb7e7f4306f30bc \
  > "$transaction/target-revision"
printf '%s\n' changed >> \
  "$implementation_dir/update-image.01ab7064e6d759f4f71bcf7fbeef1e04262cd262bd87f0755306f5c62664eac8"
expect_v0198_bridge_rejected
install -o root -g root -m 0755 /v0198-update-image \
  "$implementation_dir/update-image.01ab7064e6d759f4f71bcf7fbeef1e04262cd262bd87f0755306f5c62664eac8"
mkdir -m 700 /etc/legal-mcp/.auth-transaction
expect_v0198_bridge_rejected
/usr/bin/rm.fixture-real -rf /etc/legal-mcp/.auth-transaction
printf '%s\n' invalid > "$journal"
expect_v0198_bridge_rejected
printf '%s\n%s\nrolled-back\nflat-int8-cutover\n' \
  "$target_generation" "$old_generation" > "$journal"
printf '%s\n' stale > /run/legal-mcp-v0198-podman-adapter
chmod 500 /run/legal-mcp-v0198-podman-adapter
expect_v0198_bridge_rejected
/usr/bin/rm.fixture-real -f /run/legal-mcp-v0198-podman-adapter
touch /tmp/capability-malformed
expect_v0198_bridge_rejected
/usr/bin/rm.fixture-real -f /tmp/capability-malformed

launcher_before="$(sha256sum "$launcher" | awk '{print $1}')"
old_updater_before="$(sha256sum "$implementation_dir/update-image.01ab7064e6d759f4f71bcf7fbeef1e04262cd262bd87f0755306f5c62664eac8" | awk '{print $1}')"
/usr/bin/rm.fixture-real -rf /tmp/v0198-transaction-copy
cp -a "$transaction" /tmp/v0198-transaction-copy
printf '%s\n' transaction-retiring > /tmp/kill-cutover-at
if "$bundle/infra/linode/install-host.sh" --recover-v0198-flat-int8 \
  --version 0.19.9 >/tmp/v0198-bridge-kill.stdout \
  2>/tmp/v0198-bridge-kill.stderr <<< "$probe_key"; then
  echo 'v0.19.8 bridge transaction-retiring kill point returned success' >&2
  exit 1
fi
restore_v0198_adapter_targets
restore_public_launchers
[[ -d "$transaction.retiring" \
  && "$(<"$transaction.retiring/retirement-outcome")" = saved ]]
recovery_output="$("$bundle/infra/linode/install-host.sh" \
  --recover-v0198-flat-int8 --version 0.19.9 <<< "$probe_key")"
restore_v0198_adapter_targets
restore_public_launchers
[[ "$recovery_output" == *'exact v0.19.8 flat-int8 transaction recovered'* \
  && -e /tmp/effective-caps-queried \
  && "$(sha256sum "$launcher" | awk '{print $1}')" = "$launcher_before" \
  && "$(sha256sum "$implementation_dir/update-image.01ab7064e6d759f4f71bcf7fbeef1e04262cd262bd87f0755306f5c62664eac8" | awk '{print $1}')" \
    = "$old_updater_before" ]]
assert_dark_pair saved
# A SIGKILL can interrupt recursive deletion after arbitrary transaction files
# are gone. The unchanged updater owns deletion-only resumption; the bridge
# restores volatile upload authorization after a reboot before invoking it.
install -d -o root -g root -m 0700 "$transaction.retired"
install -o root -g root -m 0600 /tmp/v0198-transaction-copy/kind \
  "$transaction.retired/kind"
/usr/bin/rm.fixture-real -f "$authorization"
partial_output="$("$bundle/infra/linode/install-host.sh" \
  --recover-v0198-flat-int8 --version 0.19.9 </dev/null)"
restore_v0198_adapter_targets
restore_public_launchers
[[ "$partial_output" == *'transaction recovered'* \
  && ! -e "$transaction.retired" \
  && "$(<"$authorization")" = "$target_generation" ]]

# If the whole launcher is killed after transaction deletion, a later bridge
# run must reconcile its dispatch/permit rather than merely accepting corpus
# postconditions. It must also recreate authorization lost with /run on reboot.
install -d -o root -g root -m 0700 /run/legal-mcp/host-tool-launcher-dispatch
printf '%s\n' 999999 > /run/legal-mcp/host-tool-launcher-dispatch/pid
printf '%s\n' 1 > /run/legal-mcp/host-tool-launcher-dispatch/start-time
chown root:root /run/legal-mcp/host-tool-launcher-dispatch/*
chmod 600 /run/legal-mcp/host-tool-launcher-dispatch/*
printf '%s\n' '999999 1' > /run/legal-mcp/flat-int8-cutover-starting
chown root:root /run/legal-mcp/flat-int8-cutover-starting
chmod 400 /run/legal-mcp/flat-int8-cutover-starting
/usr/bin/rm.fixture-real -f "$authorization"
install -o root -g legal-mcp-publisher -m 0440 /dev/null \
  /run/legal-mcp/authorized-upload.v0198-preparing
printf '%s\n' interrupted-adapter > /run/legal-mcp-v0198-podman-adapter
printf '%s\n' interrupted-adapter > /run/legal-mcp-v0198-flock-adapter
chmod 500 /run/legal-mcp-v0198-podman-adapter \
  /run/legal-mcp-v0198-flock-adapter
idempotent_output="$("$bundle/infra/linode/install-host.sh" \
  --recover-v0198-flat-int8 --version 0.19.9 <<< "$probe_key")"
[[ "$idempotent_output" == *'was already recovered'* \
  && "$(<"$authorization")" = "$target_generation" \
  && ! -e /run/legal-mcp/authorized-upload.v0198-preparing \
  && ! -e /run/legal-mcp/host-tool-launcher-dispatch \
  && ! -e /run/legal-mcp/flat-int8-cutover-starting \
  && ! -e /run/legal-mcp-v0198-podman-adapter \
  && ! -e /run/legal-mcp-v0198-flock-adapter ]]
restore_v0198_adapter_targets
restore_public_launchers
assert_dark_pair saved

reset_baseline
success_output="$(run_cutover)"
[[ "$success_output" = "flat-int8 cutover committed generation $target_generation with $target_image" ]]
[[ ! -e /tmp/effective-caps-queried ]]
assert_dark_pair target
publication_output="$("$configure_entry" --recover)"
[[ "$publication_output" = configured-dark-published ]]
assert_published_target

# A target validation failure performs an in-process rollback of both domains
# and restores the exact ordinary prepared upload transaction.
reset_baseline
touch /tmp/fail-target-verify
if run_cutover >/tmp/rollback.stdout 2>/tmp/rollback.stderr; then
  echo 'target verification failure unexpectedly committed' >&2
  exit 1
fi
assert_dark_pair saved
[[ -d "/srv/legal-mcp/uploads/$target_generation" ]]

reset_baseline
/usr/bin/rm.fixture-real -f /run/legal-mcp/authorized-upload
touch /tmp/fail-target-verify
if run_cutover >/tmp/rollback-no-authorization.stdout \
  2>/tmp/rollback-no-authorization.stderr; then
  echo 'target verification failure without upload authorization unexpectedly committed' >&2
  exit 1
fi
[[ ! -e /run/legal-mcp/authorized-upload && ! -e "$auth_ready" \
  && "$(<"$pointer")" = "$old_generation" \
  && -d "/srv/legal-mcp/uploads/$target_generation" && ! -e "$transaction" ]]

# Production host-deploy rollback moves a sealed generation into uploads before
# recursively restoring publisher ownership and modes. Every observable
# intermediary is recoverable by the same coordinator transaction.
for point in sealed-upload-moved upload-owner-restored upload-directories-restored; do
  printf 'cutover fixture host-deploy rollback kill point: %s\n' "$point" >&2
  reset_baseline
  touch /tmp/fail-target-verify
  printf '%s\n' "$point" > /tmp/kill-cutover-at
  if run_cutover >/tmp/rollback-kill.stdout 2>/tmp/rollback-kill.stderr; then
    echo "cutover rollback kill point unexpectedly returned success: $point" >&2
    exit 1
  fi
  /usr/bin/rm.fixture-real -f /tmp/kill-cutover-at
  manifest="/srv/legal-mcp/uploads/$target_generation/generation.json"
  case "$point" in
    sealed-upload-moved)
      [[ "$(stat -c '%U:%G:%a' "$manifest")" = root:legal-mcp:440 ]]
      ;;
    upload-owner-restored|upload-directories-restored)
      [[ "$(stat -c '%U:%G:%a' "$manifest")" \
        = legal-mcp-publisher:legal-mcp-publisher:440 ]]
      ;;
  esac
  recovery_output="$(run_recovery)"
  [[ "$recovery_output" == *'rolled back both generation and image/template'* ]]
  assert_dark_pair saved
done

# Every durable pre-commit phase recovers the prior pair. Post-decision phases
# finish the already-proved target. Clearing /run on alternating cases models a
# reboot: neither the dispatch nor temporary start permit is authority.
precommit_points=(
  transaction-created transaction-synced transaction-published ingress-dark
  upload-revoked image-switched quadlet-switched template-switched
  deployment-journal-prepared deployment-journal-published generation-switched
  service-started private-proved service-dark outcome-prepared
)
postcommit_points=(
  target-committed corpus-committed transaction-retiring transaction-retired
  transaction-delete-partial transaction-deleted
)
index=0
for point in "${precommit_points[@]}"; do
  printf 'cutover fixture kill point: %s\n' "$point" >&2
  reset_baseline
  printf '%s\n' "$point" > /tmp/kill-cutover-at
  if run_cutover >/tmp/kill.stdout 2>/tmp/kill.stderr; then
    echo "cutover kill point unexpectedly returned success: $point" >&2
    exit 1
  fi
  /usr/bin/rm.fixture-real -f /tmp/kill-cutover-at
  assert_published_failure_is_closed
  if (( index % 2 == 1 )); then
    /usr/bin/rm.fixture-real -rf /run/legal-mcp/host-tool-launcher-dispatch* \
      /run/legal-mcp/flat-int8-cutover-starting \
      /run/legal-mcp/flat-int8-cutover-start-armed
  fi
  if [[ -e "$transaction" \
    && "$(<"$transaction/retirement-outcome")" = pending ]]; then
    /usr/bin/rm.fixture-real -rf /run/legal-mcp/host-tool-launcher-dispatch* \
      /run/legal-mcp/flat-int8-cutover-starting \
      /run/legal-mcp/flat-int8-cutover-start-armed
    if "$launcher" --check-auth-ready; then
      echo "pending cutover marker survived reboot as authority: $point" >&2
      exit 1
    fi
  fi
  recovery_output="$(run_recovery)"
  [[ "$recovery_output" == *'prior pair remains configured-dark'* \
    || "$recovery_output" == *'rolled back both generation and image/template'* ]]
  assert_dark_pair saved
  index=$((index + 1))
done

for point in "${postcommit_points[@]}"; do
  printf 'cutover fixture kill point: %s\n' "$point" >&2
  reset_baseline
  printf '%s\n' "$point" > /tmp/kill-cutover-at
  if run_cutover >/tmp/kill.stdout 2>/tmp/kill.stderr; then
    echo "cutover post-commit kill point unexpectedly returned success: $point" >&2
    exit 1
  fi
  /usr/bin/rm.fixture-real -f /tmp/kill-cutover-at
  assert_published_failure_is_closed
  if [[ "$point" = transaction-deleted ]]; then
    # Deletion is the final commit action: no recovery authority remains or is
    # needed once the exact target pair and both journals are committed.
    assert_dark_pair target
    continue
  fi
  if [[ -e "$auth_ready" ]]; then "$launcher" --check-auth-ready; fi
  recovery_output="$(run_recovery)"
  [[ "$recovery_output" == *'committed target pair'* \
    || "$recovery_output" == *'commit retirement completed'* ]]
  assert_dark_pair target
done

echo flat-int8-host-cutover-fixture-ok
