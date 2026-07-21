#!/usr/bin/env bash
# Exercise the no-corpus image cutover, rollback, and explicit recovery with
# deterministic host/OCI fakes. No network or real service manager is used.
set -euo pipefail
umask 077
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

[[ $EUID -eq 0 && -e /.dockerenv \
  && -f /fixture-input/update-image.sh \
  && -f /fixture-input/configure-auth.sh \
  && -f /fixture-input/legal-mcp.container.template \
  && -f /fixture-input/legal-mcp-host-deploy \
  && -f /fixture-input/legal-mcp-publisher-command \
  && -f /fixture-input/Containerfile ]] || {
  echo 'fixture requires a disposable root container and mounted inputs' >&2
  exit 2
}

version=0.19.11
revision=1111111111111111111111111111111111111111
old_revision=2222222222222222222222222222222222222222
old_digest="ghcr.io/gunba/australian-legal-mcp@sha256:$(printf 'a%.0s' {1..64})"
new_digest="ghcr.io/gunba/australian-legal-mcp@sha256:$(printf 'b%.0s' {1..64})"
generation="$(printf 'c%.0s' {1..64})"
old_image_id="sha256:$(printf 'd%.0s' {1..64})"
target_image_id="sha256:$(printf 'e%.0s' {1..64})"
probe_key='automation.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'
volume_uuid=11111111-2222-3333-4444-555555555555
bundle=/bundle
updater=$bundle/infra/hosting/update-image.sh
source_template=$bundle/infra/hosting/legal-mcp.container.template
transaction=/etc/legal-mcp/.image-transaction
preparing=${transaction}.preparing
preparing_retired=${transaction}.preparing-retired
retiring=${transaction}.retiring
retired=${transaction}.retired
image_file=/etc/legal-mcp/image
quadlet=/etc/containers/systemd/legal-mcp.container
installed_template=/usr/local/libexec/legal-mcp/legal-mcp.container.template
host_tool_launcher=/usr/local/libexec/legal-mcp/host-tool-launcher
host_tool_launcher_marker=/etc/legal-mcp/host-tool-launcher
configure_auth_pointer=/etc/legal-mcp/configure-auth-implementation
update_image_pointer=/etc/legal-mcp/update-image-implementation
implementation_dir=/usr/local/libexec/legal-mcp/host-tools
host_transaction_lock=/run/lock/legal-mcp-host-transaction.lock
log=/tmp/bootstrap-host-actions.log

for command_name in flock getfacl groupadd mknod rrsync setfacl sudo useradd visudo; do
  command -v "$command_name" >/dev/null || {
    echo "fixture dependency is missing: $command_name" >&2
    exit 2
  }
done

install -d -o root -g root -m 0755 \
  "$bundle/infra/hosting" "$bundle/scripts"
install -o root -g root -m 0755 /fixture-input/update-image.sh "$updater"
install -o root -g root -m 0755 /fixture-input/configure-auth.sh \
  "$bundle/infra/hosting/configure-auth.sh"
install -o root -g root -m 0644 /fixture-input/legal-mcp.container.template "$source_template"
install -o root -g root -m 0755 /fixture-input/legal-mcp-host-deploy \
  "$bundle/scripts/legal-mcp-host-deploy"
install -o root -g root -m 0755 /fixture-input/legal-mcp-publisher-command \
  "$bundle/scripts/legal-mcp-publisher-command"
install -o root -g root -m 0644 /fixture-input/Containerfile "$bundle/Containerfile"
printf '%s\n' "$revision" > "$bundle/SOURCE_COMMIT"
chmod 644 "$bundle/SOURCE_COMMIT"
printf '%s\n' fixture-onnx-runtime > "$bundle/libonnxruntime.so"
chmod 644 "$bundle/libonnxruntime.so"
cat > "$bundle/legal-mcp" <<'EOF'
#!/usr/bin/bash
case "$1" in
  --version)
    if [[ -e /tmp/wrong-release-binary ]]; then
      printf '%s\n' 'legal-mcp 9.9.9'
    else
      printf '%s\n' 'legal-mcp 0.19.11'
    fi
    ;;
  verify-runtime)
    printf '%s\n' '{"onnx_runtime_ready":true}'
    ;;
  *) exit 91 ;;
esac
EOF
chmod 755 "$bundle/legal-mcp"

getent group legal-mcp >/dev/null || groupadd --gid 971 legal-mcp
getent passwd legal-mcp >/dev/null ||
  useradd --uid 971 --gid 971 --home-dir /nonexistent --no-create-home legal-mcp
getent group legal-mcp-publisher >/dev/null || groupadd --gid 973 legal-mcp-publisher
getent passwd legal-mcp-publisher >/dev/null ||
  useradd --uid 973 --gid 973 --home-dir /var/lib/legal-mcp-publisher --no-create-home legal-mcp-publisher
getent group legal-mcp-admin >/dev/null || groupadd --gid 974 legal-mcp-admin
getent passwd legal-mcp-admin >/dev/null ||
  useradd --uid 974 --gid 974 --home-dir /home/legal-mcp-admin --no-create-home legal-mcp-admin
getent group caddy >/dev/null || groupadd --gid 975 caddy

install -d -o root -g root -m 0755 \
  /etc/legal-mcp /etc/containers/systemd /etc/caddy /etc/sudoers.d \
  /usr/local/libexec/legal-mcp /usr/local/sbin
install -d -o root -g legal-mcp-publisher -m 0710 /run/legal-mcp
install -d -o root -g root -m 0755 /run/lock
install -o root -g legal-mcp-publisher -m 0640 /dev/null \
  "$host_transaction_lock"
install -d -o root -g legal-mcp -m 0750 /srv/legal-mcp
setfacl --remove-all /srv/legal-mcp
setfacl --modify user:legal-mcp-publisher:--x /srv/legal-mcp
install -d -o root -g legal-mcp -m 0750 \
  /srv/legal-mcp/generations /srv/legal-mcp/lifecycle
install -d -o legal-mcp -g legal-mcp -m 0700 /srv/legal-mcp/state
install -d -o legal-mcp-publisher -g legal-mcp-publisher -m 0700 /srv/legal-mcp/uploads
install -o root -g legal-mcp -m 0640 /dev/null /srv/legal-mcp/lifecycle/LOCK
install -o root -g root -m 0640 /dev/null /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK
printf 'LEGAL_MCP_VOLUME_V1\nUUID=%s\n' "$volume_uuid" > /srv/legal-mcp/.legal-mcp-volume
chown root:root /srv/legal-mcp/.legal-mcp-volume
chmod 444 /srv/legal-mcp/.legal-mcp-volume
printf 'LEGAL_MCP_HOST_V1\nVOLUME_UUID=%s\n' "$volume_uuid" > /etc/legal-mcp/host-installed
chown root:root /etc/legal-mcp/host-installed
chmod 444 /etc/legal-mcp/host-installed
printf '%s\n' 192.0.2.1 > /etc/legal-mcp/admin-source-ip
chown root:root /etc/legal-mcp/admin-source-ip
chmod 600 /etc/legal-mcp/admin-source-ip
cat > /etc/legal-mcp/runtime.env <<'EOF'
LEGAL_MCP_HTTP_AUTH=disabled
LEGAL_MCP_API_KEYS_FILE=/run/secrets/legal-mcp-api-keys.json
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
LEGAL_MCP_ALLOWED_ORIGINS=https://legal.example.com
LEGAL_MCP_HTTP_WORKERS=4
LEGAL_MCP_SHUTDOWN_GRACE_SECONDS=30
EOF
chmod 600 /etc/legal-mcp/runtime.env
printf '{"keys":[],"version":1}\n' > /etc/legal-mcp/api-keys.json
chown legal-mcp:legal-mcp /etc/legal-mcp/api-keys.json
chmod 400 /etc/legal-mcp/api-keys.json
cat > /etc/caddy/Caddyfile <<'EOF'
{
	log default {
		format filter {
			request delete
			wrap json
		}
	}

	servers {
		timeouts {
			read_body 30s
			read_header 10s
			write 5m
			idle 5m
		}
	}
}

http://legal.example.com {
	respond "not found" 404
}

https://legal.example.com {
	encode zstd gzip

	@mcp path /mcp /.well-known/oauth-protected-resource/mcp
	handle @mcp {
		request_body {
			max_size 1MB
		}
		header {
			-Server
			Cache-Control "no-store"
			Strict-Transport-Security "max-age=31536000"
			X-Content-Type-Options "nosniff"
		}
		reverse_proxy 127.0.0.1:51235 {
			flush_interval -1
			transport http {
				dial_timeout 5s
				response_header_timeout 310s
				read_timeout 310s
				write_timeout 310s
				max_conns_per_host 8
			}
		}
	}

	handle {
		respond "not found" 404
	}
}
EOF
chown root:caddy /etc/caddy/Caddyfile
chmod 640 /etc/caddy/Caddyfile
install -o root -g caddy -m 0640 /etc/caddy/Caddyfile /tmp/expected-Caddyfile
install -d -o root -g legal-mcp-publisher -m 0710 /var/lib/legal-mcp-publisher/.ssh
printf '%s\n' \
  'restrict,command="/usr/local/sbin/legal-mcp-publisher-command" ssh-ed25519 AAAA fixture' \
  > /var/lib/legal-mcp-publisher/.ssh/authorized_keys
chown root:legal-mcp-publisher /var/lib/legal-mcp-publisher/.ssh/authorized_keys
chmod 640 /var/lib/legal-mcp-publisher/.ssh/authorized_keys

install -o root -g root -m 0755 "$bundle/scripts/legal-mcp-host-deploy" \
  /usr/local/sbin/legal-mcp-host-deploy
install -o root -g root -m 0755 "$bundle/scripts/legal-mcp-publisher-command" \
  /usr/local/sbin/legal-mcp-publisher-command
install -o root -g root -m 0644 "$source_template" "$installed_template"
deploy_sha="$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}')"
publisher_sha="$(sha256sum /usr/local/sbin/legal-mcp-publisher-command | awk '{print $1}')"
configure_auth_sha="$(sha256sum "$bundle/infra/hosting/configure-auth.sh" | awk '{print $1}')"
update_image_sha="$(sha256sum "$bundle/infra/hosting/update-image.sh" | awk '{print $1}')"
container_template_sha="$(sha256sum "$installed_template" | awk '{print $1}')"
cat > /tmp/host-tool-launcher <<'EOF'
#!/usr/bin/bash
echo 'fixture stable launcher must not execute inside the implementation test' >&2
exit 99
EOF
chmod 755 /tmp/host-tool-launcher
launcher_sha="$(sha256sum /tmp/host-tool-launcher | awk '{print $1}')"
install -d -o root -g root -m 0755 "$implementation_dir"
install -o root -g root -m 0755 /tmp/host-tool-launcher "$host_tool_launcher"
install -o root -g root -m 0755 /tmp/host-tool-launcher \
  /usr/local/sbin/legal-mcp-configure-auth
install -o root -g root -m 0755 /tmp/host-tool-launcher \
  /usr/local/sbin/legal-mcp-update-image
install -o root -g root -m 0755 "$bundle/infra/hosting/configure-auth.sh" \
  "$implementation_dir/configure-auth.$configure_auth_sha"
install -o root -g root -m 0755 "$bundle/infra/hosting/update-image.sh" \
  "$implementation_dir/update-image.$update_image_sha"
printf '%s' "$configure_auth_sha" > "$configure_auth_pointer"
printf '%s' "$update_image_sha" > "$update_image_pointer"
chmod 644 "$configure_auth_pointer" "$update_image_pointer"
cat > "$host_tool_launcher_marker" <<EOF
LEGAL_MCP_HOST_TOOL_LAUNCHER_V1
LAUNCHER_SHA256=$launcher_sha
EOF
chown root:root "$host_tool_launcher_marker"
chmod 444 "$host_tool_launcher_marker"
install -o root -g root -m 0444 "$host_tool_launcher_marker" \
  /tmp/expected-host-tool-launcher-marker
install -o root -g root -m 0644 "$configure_auth_pointer" \
  /tmp/expected-configure-auth-pointer
install -o root -g root -m 0644 "$update_image_pointer" \
  /tmp/expected-update-image-pointer
cat > /etc/sudoers.d/legal-mcp-publisher <<EOF
Defaults:legal-mcp-publisher !requiretty
legal-mcp-publisher ALL=(root) NOPASSWD: sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^prepare [0-9a-f]{64}$, sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^activate [0-9a-f]{64}$, sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^abort [0-9a-f]{64}$
EOF
chmod 440 /etc/sudoers.d/legal-mcp-publisher
visudo -cf /etc/sudoers.d/legal-mcp-publisher >/dev/null
sudoers_sha="$(sha256sum /etc/sudoers.d/legal-mcp-publisher | awk '{print $1}')"
cat > /etc/legal-mcp/host-tools <<EOF
LEGAL_MCP_HOST_TOOLS_V2
VERSION=$version
SOURCE_COMMIT=$revision
HOST_DEPLOY_SHA256=$deploy_sha
PUBLISHER_COMMAND_SHA256=$publisher_sha
CONFIGURE_AUTH_SHA256=$configure_auth_sha
UPDATE_IMAGE_SHA256=$update_image_sha
CONTAINER_TEMPLATE_SHA256=$container_template_sha
SUDOERS_SHA256=$sudoers_sha
EOF
chown root:root /etc/legal-mcp/host-tools
chmod 444 /etc/legal-mcp/host-tools
install -o root -g root -m 0444 /etc/legal-mcp/host-tools /tmp/expected-host-tools

[[ -b /dev/fixture-xfs ]] || mknod /dev/fixture-xfs b 7 240
cat > /usr/bin/findmnt <<'EOF'
#!/usr/bin/bash
if [[ "$*" == *'--output SOURCE,FSTYPE,OPTIONS'* ]]; then
  printf '/dev/fixture-xfs xfs rw,noatime,nodev,noexec,nosuid\n'
else
  exit 92
fi
EOF
cat > /usr/sbin/blkid <<EOF
#!/usr/bin/bash
printf '%s\n' '$volume_uuid'
EOF
cat > /usr/sbin/xfs_info <<'EOF'
#!/usr/bin/bash
printf 'meta-data=/dev/fixture-xfs ftype=1\ndata = bsize=4096 reflink=1\n'
EOF
python3 - /tmp/caddy-adapted.json /tmp/caddy-overbroad.json <<'PY'
import copy, json, sys
host = "legal.example.com"
timeouts = {
    "read_timeout": 30_000_000_000,
    "read_header_timeout": 10_000_000_000,
    "write_timeout": 300_000_000_000,
    "idle_timeout": 300_000_000_000,
}
https_routes = [
    {"handle": [{"encodings": {"gzip": {}, "zstd": {}}, "handler": "encode", "prefer": ["zstd", "gzip"]}]},
    {"group": "group2", "handle": [{"handler": "subroute", "routes": [{"handle": [
        {"handler": "headers", "response": {"deferred": True, "delete": ["Server"], "set": {
            "Cache-Control": ["no-store"], "Strict-Transport-Security": ["max-age=31536000"],
            "X-Content-Type-Options": ["nosniff"]}}},
        {"handler": "request_body", "max_size": 1_000_000},
        {"flush_interval": -1, "handler": "reverse_proxy", "transport": {
            "dial_timeout": 5_000_000_000, "max_conns_per_host": 8, "protocol": "http",
            "read_timeout": 310_000_000_000, "response_header_timeout": 310_000_000_000,
            "write_timeout": 310_000_000_000}, "upstreams": [{"dial": "127.0.0.1:51235"}]},
    ]}]}], "match": [{"path": ["/mcp", "/.well-known/oauth-protected-resource/mcp"]}]},
    {"group": "group2", "handle": [{"handler": "subroute", "routes": [{"handle": [
        {"body": "not found", "handler": "static_response", "status_code": 404}
    ]}]}]},
]
logging = {"logs": {"default": {"encoder": {
    "fields": {"request": {"filter": "delete"}}, "format": "filter",
    "wrap": {"format": "json"},
}}}}
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
overbroad = copy.deepcopy(value)
overbroad["apps"]["http"]["servers"]["srv0"]["routes"][0]["handle"][0]["routes"][1]["match"][0]["path"] = ["/*"]
with open(sys.argv[2], "w", encoding="utf-8") as handle:
    json.dump(overbroad, handle, separators=(",", ":"))
PY
cat > /usr/bin/caddy <<'EOF'
#!/usr/bin/bash
printf 'caddy:%s\n' "$*" >> /tmp/bootstrap-host-actions.log
[[ "$1" = adapt && "$*" == *'--validate'* ]] || exit 91
if [[ -e /tmp/overbroad-caddy ]]; then
  cat /tmp/caddy-overbroad.json
else
  cmp --silent /tmp/expected-Caddyfile /etc/caddy/Caddyfile || exit 92
  cat /tmp/caddy-adapted.json
fi
EOF
cat > /usr/bin/ss <<'EOF'
#!/usr/bin/bash
if [[ -e /tmp/fail-ss ]]; then exit 86; fi
if [[ -e /tmp/bootstrap-listener ]]; then
  printf 'LISTEN 0 4096 0.0.0.0:443 0.0.0.0:*\n'
fi
if [[ -e /tmp/ordinary-mode && -e /tmp/service-active ]]; then
  if [[ -e /tmp/overbroad-service-listener ]]; then
    printf 'LISTEN 0 4096 0.0.0.0:51235 0.0.0.0:*\n'
  else
    printf 'LISTEN 0 4096 127.0.0.1:51235 0.0.0.0:*\n'
  fi
fi
if [[ -e /tmp/ordinary-mode && -e /tmp/caddy-active ]]; then
  printf '%s\n' \
    'LISTEN 0 4096 *:80 *:*' \
    'LISTEN 0 4096 *:443 *:*'
  [[ ! -e /tmp/extra-web-listener ]] || \
    printf '%s\n' 'LISTEN 0 4096 127.0.0.1:443 0.0.0.0:*'
fi
EOF
chmod 755 /usr/bin/findmnt /usr/sbin/blkid /usr/sbin/xfs_info \
  /usr/bin/caddy /usr/bin/ss

cat > /usr/bin/systemctl <<'EOF'
#!/usr/bin/bash
printf 'systemctl:%s\n' "$*" >> /tmp/bootstrap-host-actions.log
unit_flag() {
  case "$1" in
    legal-mcp.service) printf '%s' service ;;
    caddy.service) printf '%s' caddy ;;
    *) printf '%s' unknown ;;
  esac
}
case "$1" in
  is-enabled)
    [[ ! -e /tmp/fail-systemctl-enabled ]] || exit 84
    flag="$(unit_flag "$2")"
    if [[ "$flag" = service ]]; then
      if [[ -e /tmp/service-wrong-enablement ]]; then printf '%s\n' enabled; else printf '%s\n' generated; fi
      exit 0
    fi
    if [[ -e "/tmp/${flag}-enabled" ]]; then printf '%s\n' enabled; exit 0; fi
    printf '%s\n' disabled
    exit 1
    ;;
  is-active)
    [[ ! -e /tmp/fail-systemctl-active ]] || exit 85
    if [[ "$2" = --quiet ]]; then unit="$3"; else unit="$2"; fi
    flag="$(unit_flag "$unit")"
    if [[ -e "/tmp/${flag}-active" ]]; then printf '%s\n' active; exit 0; fi
    printf '%s\n' inactive
    exit 3
    ;;
  disable)
    [[ ! -e /tmp/fail-systemctl-disable ]] || exit 86
    for argument in "$@"; do
      flag="$(unit_flag "$argument")"
      [[ "$flag" = caddy ]] || continue
      rm -f /tmp/caddy-enabled /tmp/caddy-active
    done
    ;;
  daemon-reload)
    if [[ -e /tmp/fail-daemon-reload-once ]]; then
      rm -f /tmp/fail-daemon-reload-once
      if [[ -e /tmp/drop-old-image-on-daemon-failure ]]; then
        touch /tmp/missing-old-image
      fi
      exit 87
    fi
    ;;
  enable)
    if [[ -e /tmp/ordinary-mode && "$2" = caddy.service ]]; then
      touch /tmp/caddy-enabled
      [[ "$*" != *--now* ]] || touch /tmp/caddy-active
      exit 0
    fi
    touch /tmp/forbidden-service-start
    exit 96
    ;;
  start)
    if [[ -e /tmp/ordinary-mode && "$2" = caddy.service ]]; then
      touch /tmp/caddy-active
      exit 0
    fi
    touch /tmp/forbidden-service-start
    exit 96
    ;;
  restart)
    if [[ -e /tmp/ordinary-mode && "$2" = legal-mcp.service ]]; then
      if [[ -e /tmp/fail-service-restart-once ]]; then
        rm -f /tmp/fail-service-restart-once /tmp/service-active
        exit 89
      fi
      if grep -Fq 'sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' \
        /etc/legal-mcp/image; then
        cp /tmp/target-image-id /tmp/running-image-id
      else
        cp /tmp/old-image-id /tmp/running-image-id
      fi
      touch /tmp/service-active
      exit 0
    fi
    touch /tmp/forbidden-service-start
    exit 96
    ;;
  stop)
    if [[ "$2" = caddy.service ]]; then
      rm -f /tmp/caddy-active
    else
      rm -f /tmp/service-active
    fi
    exit 0
    ;;
  *) exit 0 ;;
esac
EOF
cat > /usr/sbin/ufw <<'EOF'
#!/usr/bin/bash
printf 'ufw:%s\n' "$*" >> /tmp/bootstrap-host-actions.log
if [[ "$1" = status ]]; then
  [[ ! -e /tmp/fail-ufw-status ]] || exit 87
  cat <<'STATUS'
Status: active
Default: deny (incoming), allow (outgoing), disabled (routed)
22/tcp                     ALLOW IN    192.0.2.1                 # restricted SSH administration
STATUS
  if [[ -e /tmp/ufw-web-open ]]; then
    printf '%s\n' \
      '80/tcp                     ALLOW IN    Anywhere' \
      '443/tcp                    ALLOW IN    Anywhere'
  fi
  if [[ -e /tmp/ufw-extra-open ]]; then
    printf '%s\n' '9999/tcp                   ALLOW IN    Anywhere'
  fi
  exit 0
fi
if [[ "$*" == '--force delete allow 80/tcp comment Caddy ACME HTTP' \
  || "$*" == '--force delete allow 443/tcp comment Australian Legal MCP HTTPS' ]]; then
  [[ ! -e /tmp/fail-ufw-delete ]] || exit 88
  rm -f /tmp/ufw-web-open
  exit 0
fi
if [[ "$1" = allow ]]; then
  if [[ -e /tmp/ordinary-mode ]]; then
    touch /tmp/ufw-web-open
    exit 0
  fi
  touch /tmp/forbidden-ufw-open
  exit 97
fi
exit 0
EOF
chmod 755 /usr/bin/systemctl /usr/sbin/ufw

cat > /usr/bin/podman <<EOF
#!/usr/bin/bash
printf 'podman:%s\n' "\$*" >> /tmp/bootstrap-host-actions.log
old_image='$old_digest'
new_image='$new_digest'
emit_image_id() {
  if [[ -e /tmp/podman-bare-image-ids ]]; then
    sed 's/^sha256://' "\$1"
  else
    cat "\$1"
  fi
}
case "\$1" in
  container)
    [[ "\$2" = exists ]]
    if [[ -e /tmp/podman-container-error ]]; then exit 125; fi
    [[ -e /tmp/existing-container || -e /tmp/ordinary-mode ]]
    exit
    ;;
  image)
    case "\$2" in
      exists)
        if [[ -e /tmp/podman-image-error ]]; then exit 125; fi
        if [[ -e /tmp/missing-old-image && "\$3" = "\$old_image" ]]; then exit 1; fi
        [[ "\$3" = "\$old_image" || "\$3" = "\$new_image" ]]
        ;;
      inspect)
        image="\$3"
        format="\${!#}"
        if [[ "\$format" = '{{.Id}}' ]]; then
          if [[ "\$image" = "\$new_image" ]]; then
            emit_image_id /tmp/target-image-id
          else
            emit_image_id /tmp/old-image-id
          fi
        elif [[ "\$format" = '{{.Digest}}' ]]; then
          if [[ -e /tmp/wrong-oci-digest ]]; then printf 'sha256:%064d\n' 9; else printf '%s\n' "\${image##*@}"; fi
        elif [[ "\$format" == *'.title'* ]]; then
          if [[ -e /tmp/wrong-oci-title ]]; then printf '%s\n' Wrong; else printf '%s\n' 'Australian Legal MCP'; fi
        elif [[ "\$format" == *'.description'* ]]; then
          if [[ -e /tmp/wrong-oci-description ]]; then printf '%s\n' Wrong; else printf '%s\n' 'Source-grounded Australian legal MCP server'; fi
        elif [[ "\$format" == *'.version'* ]]; then
          if [[ "\$image" = "\$new_image" ]]; then
            if [[ -e /tmp/wrong-oci-version ]]; then printf '%s\n' 9.9.9; else printf '%s\n' 0.19.11; fi
          else
            printf '%s\n' 0.18.1
          fi
        elif [[ "\$format" == *'.source'* ]]; then
          if [[ "\$image" = "\$new_image" && -e /tmp/wrong-oci-source ]]; then
            printf '%s\n' https://github.com/example/wrong
          else
            printf '%s\n' https://github.com/gunba/australian-legal-mcp
          fi
        elif [[ "\$format" == *'.revision'* ]]; then
          if [[ "\$image" = "\$new_image" ]]; then
            if [[ -e /tmp/wrong-oci-revision ]]; then printf '%040d\n' 9; else printf '%s\n' '$revision'; fi
          else
            printf '%s\n' '$old_revision'
          fi
        elif [[ "\$format" == *'.licenses'* ]]; then
          if [[ -e /tmp/wrong-oci-licenses ]]; then printf '%s\n' Wrong; else printf '%s\n' MIT; fi
        elif [[ "\$format" == *'io.australian-legal-mcp.ann-format'* ]]; then
          if [[ -e /tmp/wrong-oci-ann-format ]]; then printf '%s\n' arroy; else printf '%s\n' flat-int8-v1; fi
        else
          exit 93
        fi
        ;;
      *) exit 93 ;;
    esac
    ;;
  pull)
    if flock -n /run/lock/legal-mcp-host-transaction.lock -c true; then
      echo 'image pull ran without the shared host lock' >&2
      exit 94
    fi
    [[ "\$2" = "\$new_image" ]]
    ;;
  run)
    command=''
    for argument in "\$@"; do
      if [[ "\$argument" = --version || "\$argument" = verify-runtime ]]; then command="\$argument"; fi
    done
    case "\$command" in
      --version)
        if [[ -e /tmp/wrong-oci-binary ]]; then printf '%s\n' 'legal-mcp 9.9.9'; else printf '%s\n' 'legal-mcp 0.19.11'; fi
        ;;
      verify-runtime) printf '%s\n' '{"onnx_runtime_ready":true}' ;;
      '')
        [[ "\${*: -2}" = 'verify --quiet' ]]
        ;;
      *) exit 95 ;;
    esac
    ;;
  inspect)
    [[ "\$2" = australian-legal-mcp && "\${!#}" = '{{.Image}}' ]]
    emit_image_id /tmp/running-image-id
    ;;
  *) exit 96 ;;
esac
EOF
chmod 755 /usr/bin/podman

cat > /usr/bin/curl <<'EOF'
#!/usr/bin/bash
headers=''
url=''
write_status=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dump-header) headers="$2"; shift 2 ;;
    --write-out) write_status=true; shift 2 ;;
    http://*|https://*) url="$1"; shift ;;
    --header|--data|--request|--max-time|--max-redirs|--output|--resolve) shift 2 ;;
    *) shift ;;
  esac
done
printf 'curl:%s\n' "$url" >> /tmp/bootstrap-host-actions.log
case "$url" in
  http://127.0.0.1:51235/readyz)
    [[ -e /tmp/service-active && ! -e /tmp/fail-ready ]]
    printf '{"generation":"%s","status":"ok"}\n' \
      "$(</srv/legal-mcp/lifecycle/active-generation)"
    ;;
  http://legal.example.com/mcp|https://legal.example.com/|https://legal.example.com/mcp/|https://legal.example.com/.well-known/oauth-protected-resource|https://legal.example.com/.well-known/oauth-protected-resource/mcp/|https://legal.example.com/readyz|https://legal.example.com/livez)
    if [[ -e /tmp/overbroad-public-route ]]; then status=200; else status=404; fi
    [[ -z "$headers" ]] || printf 'HTTP/1.1 %s Fixture\r\n\r\n' "$status" > "$headers"
    [[ "$write_status" = false ]] || printf '%s' "$status"
    ;;
  */.well-known/oauth-protected-resource/mcp)
    printf '{"resource":"https://legal.example.com/mcp"}\n'
    ;;
  */mcp)
    [[ ! -e /tmp/fail-auth-boundary ]]
    if [[ -n "$headers" ]]; then
      printf 'HTTP/1.1 401 Unauthorized\r\n' > "$headers"
      mode="$(awk -F= '$1 == "LEGAL_MCP_HTTP_AUTH" {print $2}' /etc/legal-mcp/runtime.env)"
      [[ "$mode" != *api-key* ]] || \
        printf 'WWW-Authenticate: ApiKey realm="australian-legal-mcp"\r\n' >> "$headers"
      [[ "$mode" != *entra* ]] || \
        printf 'WWW-Authenticate: Bearer resource_metadata="https://legal.example.com/.well-known/oauth-protected-resource/mcp"\r\n' >> "$headers"
      printf '\r\n' >> "$headers"
    fi
    [[ "$write_status" = false ]] || printf 401
    ;;
  *) exit 1 ;;
esac
EOF
chmod 755 /usr/bin/curl

/usr/bin/mv /usr/bin/install /usr/bin/install.fixture-real
/usr/bin/mv /usr/bin/find /usr/bin/find.fixture-real
/usr/bin/mv /usr/bin/flock /usr/bin/flock.fixture-real
/usr/bin/mv /usr/bin/rm /usr/bin/rm.fixture-real
/usr/bin/mv /usr/bin/sync /usr/bin/sync.fixture-real
/usr/bin/mv /usr/bin/mv /usr/bin/mv.fixture-real
cat > /usr/bin/install <<'EOF'
#!/usr/bin/bash
exec /usr/bin/install.fixture-real "$@"
EOF
cat > /usr/bin/find <<'EOF'
#!/usr/bin/bash
if [[ -e /tmp/fail-find ]]; then exit 88; fi
exec /usr/bin/find.fixture-real "$@"
EOF
cat > /usr/bin/flock <<'EOF'
#!/usr/bin/bash
if [[ -n "${LEGAL_MCP_FIXTURE_FLOCK_RECORD:-}" \
  && $# -eq 2 && "$1" = -x && "$2" =~ ^[0-9]+$ ]]; then
  printf '%s:%s\n' "$2" "$(stat -Lc '%d:%i' "/proc/$$/fd/$2")" \
    > "$LEGAL_MCP_FIXTURE_FLOCK_RECORD"
fi
exec /usr/bin/flock.fixture-real "$@"
EOF
cat > /usr/bin/mv <<'EOF'
#!/usr/bin/bash
/usr/bin/mv.fixture-real "$@"
status=$?
[[ $status -eq 0 && -s /tmp/kill-bootstrap-after ]] || exit "$status"
point="$(</tmp/kill-bootstrap-after)"
target="${!#}"
matched=false
case "$point:$target" in
  transaction-prepared:/etc/legal-mcp/.image-transaction) matched=true ;;
  image-pinned:/etc/legal-mcp/image) matched=true ;;
  quadlet-installed:/etc/containers/systemd/legal-mcp.container) matched=true ;;
  template-installed:/usr/local/libexec/legal-mcp/legal-mcp.container.template) matched=true ;;
  transaction-retiring:/etc/legal-mcp/.image-transaction.retiring) matched=true ;;
  transaction-retired:/etc/legal-mcp/.image-transaction.retired) matched=true ;;
esac
if [[ "$matched" = true ]]; then
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
exit 0
EOF
cat > /usr/bin/rm <<'EOF'
#!/usr/bin/bash
point=''
if [[ -f /tmp/kill-bootstrap-after ]]; then point="$(</tmp/kill-bootstrap-after)"; fi
if { [[ "$point" = preparation-delete \
      && "$*" == *'/etc/legal-mcp/.image-transaction.preparing-retired'* ]] \
    || [[ "$point" = transaction-delete \
      && "$*" == *'/etc/legal-mcp/.image-transaction.retired'* ]]; }; then
  directory="${!#}"
  victim="$(/usr/bin/find.fixture-real "$directory" -mindepth 1 -maxdepth 1 -print -quit)"
  [[ -n "$victim" ]]
  /usr/bin/rm.fixture-real -rf -- "$victim"
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
exec /usr/bin/rm.fixture-real "$@"
EOF
cat > /usr/bin/sync <<'EOF'
#!/usr/bin/bash
/usr/bin/sync.fixture-real "$@"
status=$?
if [[ $status -eq 0 && -s /tmp/kill-bootstrap-after \
  && "$(</tmp/kill-bootstrap-after)" = preparation-synced \
  && "$*" = '-f /etc/legal-mcp/.image-transaction.preparing' ]]; then
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
exit "$status"
EOF
chmod 755 /usr/bin/install /usr/bin/find /usr/bin/flock /usr/bin/mv \
  /usr/bin/rm /usr/bin/sync
real_install=/usr/bin/install.fixture-real
real_rm=/usr/bin/rm.fixture-real

"$real_install" -o root -g root -m 0644 "$source_template" /tmp/old-template
printf '%s\n' "$old_digest" > /tmp/old-image
sed "s|__IMAGE_DIGEST__|$old_digest|g" /tmp/old-template > /tmp/old-quadlet
chmod 600 /tmp/old-image
chmod 644 /tmp/old-template /tmp/old-quadlet

reset_baseline() {
  "$real_rm" -rf -- "$transaction" "$preparing" "$preparing_retired" \
    "$retiring" "$retired" \
    /etc/legal-mcp/.auth-transaction /etc/legal-mcp/.host-tools-transaction \
    /etc/legal-mcp/.host-tools-transaction.preparing \
    /etc/legal-mcp/.host-tools-transaction.retiring \
    /etc/legal-mcp/.host-tools-transaction.retired \
    /etc/legal-mcp/.host-tools-transaction.rollback-retiring \
    /etc/legal-mcp/.host-tools-transaction.rollback-retired \
    /etc/legal-mcp/.host-tools-transaction.publisher-restore
  /usr/bin/find.fixture-real /srv/legal-mcp/generations -mindepth 1 -maxdepth 1 \
    -exec /usr/bin/rm.fixture-real -rf -- {} +
  /usr/bin/find.fixture-real /srv/legal-mcp/uploads -mindepth 1 -maxdepth 1 \
    -exec /usr/bin/rm.fixture-real -rf -- {} +
  /usr/bin/find.fixture-real /srv/legal-mcp/state -mindepth 1 -maxdepth 1 \
    -exec /usr/bin/rm.fixture-real -rf -- {} +
  /usr/bin/find.fixture-real /srv/legal-mcp/lifecycle -mindepth 1 -maxdepth 1 \
    ! -name LOCK ! -name LIFECYCLE_LOCK -exec /usr/bin/rm.fixture-real -rf -- {} +
  "$real_install" -o root -g root -m 0640 /dev/null \
    /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK
  "$real_rm" -f /run/legal-mcp/authorized-upload /tmp/*-active /tmp/*-enabled \
    /tmp/ufw-web-open /tmp/ufw-extra-open /tmp/bootstrap-listener \
    /tmp/existing-container /tmp/missing-old-image /tmp/podman-container-error \
    /tmp/podman-image-error /tmp/fail-systemctl-* /tmp/fail-ufw-* \
    /tmp/service-wrong-enablement \
    /tmp/wrong-* /tmp/fail-daemon-reload-once /tmp/drop-old-image-on-daemon-failure \
    /tmp/fail-find /tmp/fail-ss /tmp/kill-bootstrap-after \
    /tmp/forbidden-service-start /tmp/forbidden-ufw-open \
    /tmp/ordinary-mode /tmp/running-image-id /tmp/old-image-id \
    /tmp/target-image-id /tmp/fail-service-restart-once /tmp/fail-ready \
    /tmp/fail-auth-boundary /tmp/observed-api-key \
    /tmp/exact-public-api-key-observed /tmp/overbroad-caddy \
    /tmp/overbroad-public-route /tmp/overbroad-service-listener \
    /tmp/extra-web-listener
  : > "$log"
  "$real_install" -o root -g root -m 0600 /tmp/old-image "$image_file"
  "$real_install" -o root -g root -m 0644 /tmp/old-template "$installed_template"
  "$real_install" -o root -g root -m 0644 /tmp/old-quadlet "$quadlet"
  "$real_install" -o root -g root -m 0644 /fixture-input/legal-mcp.container.template "$source_template"
  "$real_install" -o root -g caddy -m 0640 /tmp/expected-Caddyfile \
    /etc/caddy/Caddyfile
  "$real_install" -o root -g root -m 0755 /tmp/host-tool-launcher \
    "$host_tool_launcher"
  "$real_install" -o root -g root -m 0755 /tmp/host-tool-launcher \
    /usr/local/sbin/legal-mcp-configure-auth
  "$real_install" -o root -g root -m 0755 /tmp/host-tool-launcher \
    /usr/local/sbin/legal-mcp-update-image
  "$real_install" -o root -g root -m 0755 /fixture-input/configure-auth.sh \
    "$implementation_dir/configure-auth.$configure_auth_sha"
  "$real_install" -o root -g root -m 0755 /fixture-input/update-image.sh \
    "$implementation_dir/update-image.$update_image_sha"
  "$real_install" -o root -g root -m 0444 \
    /tmp/expected-host-tool-launcher-marker "$host_tool_launcher_marker"
  "$real_install" -o root -g root -m 0644 \
    /tmp/expected-configure-auth-pointer "$configure_auth_pointer"
  "$real_install" -o root -g root -m 0644 \
    /tmp/expected-update-image-pointer "$update_image_pointer"
  printf '%s\n' "$revision" > "$bundle/SOURCE_COMMIT"
  chmod 644 "$bundle/SOURCE_COMMIT"
  chmod 755 "$updater" "$bundle/legal-mcp" \
    "$bundle/infra/hosting/configure-auth.sh" \
    "$bundle/scripts/legal-mcp-host-deploy" "$bundle/scripts/legal-mcp-publisher-command"
  chmod 644 "$source_template" "$bundle/Containerfile" "$bundle/libonnxruntime.so"
  rm -f /etc/legal-mcp/host-installed /etc/legal-mcp/host-tools \
    /etc/legal-mcp/auth-ready \
    /srv/legal-mcp/.legal-mcp-volume
  printf 'LEGAL_MCP_HOST_V1\nVOLUME_UUID=%s\n' "$volume_uuid" > /etc/legal-mcp/host-installed
  chown root:root /etc/legal-mcp/host-installed
  chmod 444 /etc/legal-mcp/host-installed
  "$real_install" -o root -g root -m 0444 /tmp/expected-host-tools /etc/legal-mcp/host-tools
  printf 'LEGAL_MCP_VOLUME_V1\nUUID=%s\n' "$volume_uuid" > /srv/legal-mcp/.legal-mcp-volume
  chown root:root /srv/legal-mcp/.legal-mcp-volume
  chmod 444 /srv/legal-mcp/.legal-mcp-volume
  cat > /etc/legal-mcp/runtime.env <<'EOF'
LEGAL_MCP_HTTP_AUTH=disabled
LEGAL_MCP_API_KEYS_FILE=/run/secrets/legal-mcp-api-keys.json
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
LEGAL_MCP_ALLOWED_ORIGINS=https://legal.example.com
LEGAL_MCP_HTTP_WORKERS=4
LEGAL_MCP_SHUTDOWN_GRACE_SECONDS=30
EOF
  chown root:root /etc/legal-mcp/runtime.env
  chmod 600 /etc/legal-mcp/runtime.env
  printf '{"keys":[],"version":1}\n' > /etc/legal-mcp/api-keys.json
  chown legal-mcp:legal-mcp /etc/legal-mcp/api-keys.json
  chmod 400 /etc/legal-mcp/api-keys.json
}

assert_old_state() {
  cmp --silent /tmp/old-image "$image_file"
  cmp --silent /tmp/old-template "$installed_template"
  cmp --silent /tmp/old-quadlet "$quadlet"
  [[ ! -e "$transaction" && ! -e "$preparing" && ! -e "$preparing_retired" \
    && ! -e "$retiring" && ! -e "$retired" && ! -e /tmp/service-active \
    && ! -e /tmp/caddy-active && ! -e /tmp/ufw-web-open ]]
  [[ ! -e /srv/legal-mcp/lifecycle/active-generation \
    && ! -e /srv/legal-mcp/lifecycle/.deployment-transaction ]]
}

assert_new_state() {
  [[ "$(<"$image_file")" = "$new_digest" ]]
  cmp --silent "$source_template" "$installed_template"
  sed "s|__IMAGE_DIGEST__|$new_digest|g" "$source_template" \
    > /tmp/expected-new-quadlet
  cmp --silent /tmp/expected-new-quadlet "$quadlet"
  [[ ! -e "$transaction" && ! -e "$preparing" && ! -e "$preparing_retired" \
    && ! -e "$retiring" && ! -e "$retired" \
    && ! -e /tmp/service-active && ! -e /tmp/service-enabled \
    && ! -e /tmp/caddy-active && ! -e /tmp/caddy-enabled \
    && ! -e /tmp/ufw-web-open && ! -e /tmp/forbidden-service-start \
    && ! -e /tmp/forbidden-ufw-open ]]
}

run_update() {
  "$updater" --bootstrap-empty-host \
    --image "$new_digest" --version "$version" --template "$source_template"
}

run_normal_update() {
  printf '%s\n' "$probe_key" | "$updater" \
    --image "$new_digest" --version "$version" --template "${1:-$source_template}"
}

expect_update_failed() {
  if run_update >/tmp/bootstrap.stdout 2>/tmp/bootstrap.stderr; then
    echo 'unsafe empty-host image update was unexpectedly accepted' >&2
    exit 1
  fi
}

kill_update_at() {
  local point="$1" status
  reset_baseline
  printf '%s\n' "$point" > /tmp/kill-bootstrap-after
  set +e
  run_update >/tmp/bootstrap-kill.stdout 2>/tmp/bootstrap-kill.stderr
  status=$?
  set -e
  "$real_rm" -f /tmp/kill-bootstrap-after
  [[ $status -ne 0 ]]
}

reset_baseline
if "$updater" --bootstrap-empty-host --image "$new_digest" \
  --version 0.19.2 --template "$source_template" >/dev/null 2>&1; then
  echo 'wrong requested release version was unexpectedly accepted' >&2
  exit 1
fi
assert_old_state

# The ordinary authenticated image path also requires the exact V2 installed
# host tools and release template before it inspects active runtime state.
reset_baseline
# A caller-supplied descriptor is not trusted by number: when it resolves to
# another inode, the direct release implementation opens and locks the exact
# host lock instead.
: > /tmp/foreign-host-lock
exec {foreign_lock_fd}<>/tmp/foreign-host-lock
export LEGAL_MCP_HOST_TRANSACTION_LOCK_FD="$foreign_lock_fd"
export LEGAL_MCP_FIXTURE_FLOCK_RECORD=/tmp/direct-host-lock-record
if run_normal_update >/tmp/normal-image.stdout 2>/tmp/normal-image.stderr; then
  echo 'empty host unexpectedly accepted a normal image update' >&2
  exit 1
fi
unset LEGAL_MCP_HOST_TRANSACTION_LOCK_FD LEGAL_MCP_FIXTURE_FLOCK_RECORD
exec {foreign_lock_fd}>&-
direct_lock_record="$(</tmp/direct-host-lock-record)"
[[ "${direct_lock_record%%:*}" != "$foreign_lock_fd" \
  && "${direct_lock_record#*:}" = "$(stat -Lc '%d:%i' "$host_transaction_lock")" ]]
grep -Fq 'required host file is missing or unsafe: /srv/legal-mcp/lifecycle/active-generation' \
  /tmp/normal-image.stderr
printf '%s\n' 'malicious=1' '__IMAGE_DIGEST__' \
  > "$bundle/infra/hosting/alternate.container.template"
chmod 644 "$bundle/infra/hosting/alternate.container.template"
if run_normal_update "$bundle/infra/hosting/alternate.container.template" \
  >/tmp/normal-template.stdout 2>/tmp/normal-template.stderr; then
  echo 'normal image update accepted an unbound release template' >&2
  exit 1
fi
grep -Fq 'Quadlet template is not in a complete Linux release bundle' \
  /tmp/normal-template.stderr
rm -f "$bundle/infra/hosting/alternate.container.template"
assert_old_state

reset_baseline
printf '%040d\n' 3 > "$bundle/SOURCE_COMMIT"
expect_update_failed
assert_old_state

reset_baseline
touch /tmp/wrong-release-binary
expect_update_failed
assert_old_state

for failure in wrong-oci-title wrong-oci-description wrong-oci-source \
  wrong-oci-revision wrong-oci-version wrong-oci-licenses \
  wrong-oci-ann-format wrong-oci-digest wrong-oci-binary; do
  reset_baseline
  touch "/tmp/$failure"
  expect_update_failed
  assert_old_state
done

reset_baseline
if "$updater" --bootstrap-empty-host --image ghcr.io/gunba/australian-legal-mcp:latest \
  --version "$version" --template "$source_template" >/dev/null 2>&1; then
  echo 'tagged image was unexpectedly accepted' >&2
  exit 1
fi
assert_old_state

# Public ingress is closed before rejection, even when Caddy starts in an
# invalid active/enabled state.
reset_baseline
touch /tmp/ufw-web-open /tmp/caddy-active /tmp/caddy-enabled
expect_update_failed
assert_old_state
grep -Fq 'ufw:--force delete allow 80/tcp comment Caddy ACME HTTP' "$log"

reset_baseline
touch /tmp/service-active
expect_update_failed
assert_old_state

reset_baseline
printf '%064d' 1 > /srv/legal-mcp/lifecycle/active-generation
chmod 644 /srv/legal-mcp/lifecycle/active-generation
expect_update_failed
cmp --silent /tmp/old-image "$image_file"

for phase in prepared activating; do
  reset_baseline
  printf '%064d\n-\n%s\n' 1 "$phase" > /srv/legal-mcp/lifecycle/.deployment-transaction
  chown root:root /srv/legal-mcp/lifecycle/.deployment-transaction
  chmod 600 /srv/legal-mcp/lifecycle/.deployment-transaction
  expect_update_failed
  cmp --silent /tmp/old-image "$image_file"
done

reset_baseline
install -d -o root -g root -m 0700 /etc/legal-mcp/.auth-transaction
expect_update_failed
assert_old_state

reset_baseline
install -d -o root -g root -m 0700 /etc/legal-mcp/.host-tools-transaction
expect_update_failed
assert_old_state

# The V2 marker binds the release implementations selected by the stable
# launchers and the installed version-matched template. Old marker schemas and
# installed template drift fail closed.
reset_baseline
sed -i 's/LEGAL_MCP_HOST_TOOLS_V2/LEGAL_MCP_HOST_TOOLS_V1/' \
  /etc/legal-mcp/host-tools
expect_update_failed
assert_old_state

reset_baseline
printf '%s\n' '# unexpected drift' >> "$installed_template"
expect_update_failed
cmp --silent /tmp/old-image "$image_file"
[[ ! -e "$transaction" ]]

reset_baseline
chmod 644 "$image_file"
expect_update_failed
cmp --silent /tmp/old-image "$image_file"

reset_baseline
printf 'unexpected content\n' > /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK
expect_update_failed
cmp --silent /tmp/old-image "$image_file"

reset_baseline
rm -f /etc/legal-mcp/host-installed
ln -s /tmp/old-image /etc/legal-mcp/host-installed
expect_update_failed
cmp --silent /tmp/old-image "$image_file"

reset_baseline
printf 'LEGAL_MCP_VOLUME_V1\nUUID=%s\n' 99999999-2222-3333-4444-555555555555 \
  > /srv/legal-mcp/.legal-mcp-volume
chmod 444 /srv/legal-mcp/.legal-mcp-volume
expect_update_failed
cmp --silent /tmp/old-image "$image_file"

reset_baseline
sed -i 's/LEGAL_MCP_HTTP_AUTH=disabled/LEGAL_MCP_HTTP_AUTH=api-key/' \
  /etc/legal-mcp/runtime.env
expect_update_failed
cmp --silent /tmp/old-image "$image_file"

reset_baseline
printf '{"keys":[{"id":"configured","sha256":"%064d"}],"version":1}\n' 1 \
  > /etc/legal-mcp/api-keys.json
chown legal-mcp:legal-mcp /etc/legal-mcp/api-keys.json
chmod 400 /etc/legal-mcp/api-keys.json
expect_update_failed
cmp --silent /tmp/old-image "$image_file"

reset_baseline
touch /tmp/existing-container
expect_update_failed
assert_old_state

reset_baseline
touch /tmp/bootstrap-listener
expect_update_failed
assert_old_state

# Socket, directory, and container absence is accepted only after a successful
# probe and Podman's explicit status 1.
reset_baseline
touch /tmp/fail-ss
expect_update_failed
"$real_rm" -f /tmp/fail-ss
assert_old_state

reset_baseline
touch /tmp/fail-find
expect_update_failed
"$real_rm" -f /tmp/fail-find
assert_old_state

reset_baseline
touch /tmp/podman-container-error
expect_update_failed
"$real_rm" -f /tmp/podman-container-error
assert_old_state

# Exact generated/inactive Quadlet and disabled/inactive Caddy are the real
# Ubuntu Quadlet state. Probe errors are never interpreted as off or absent.
for failure in fail-systemctl-enabled fail-systemctl-active fail-ufw-status \
  podman-image-error; do
  reset_baseline
  touch "/tmp/$failure"
  expect_update_failed
  "$real_rm" -f "/tmp/$failure"
  assert_old_state
done

reset_baseline
touch /tmp/caddy-active /tmp/caddy-enabled /tmp/ufw-web-open /tmp/fail-ufw-delete
expect_update_failed
[[ -e /tmp/ufw-web-open ]]
"$real_rm" -f /tmp/caddy-active /tmp/caddy-enabled /tmp/ufw-web-open /tmp/fail-ufw-delete
assert_old_state

reset_baseline
touch /tmp/caddy-active /tmp/caddy-enabled /tmp/fail-systemctl-disable
expect_update_failed
"$real_rm" -f /tmp/caddy-active /tmp/caddy-enabled /tmp/fail-systemctl-disable
assert_old_state

reset_baseline
rm -f "$source_template"
ln -s /tmp/old-template "$source_template"
expect_update_failed
assert_old_state

# Failure after all three files have changed rolls back automatically to the
# byte-for-byte old state and never starts either service.
reset_baseline
touch /tmp/fail-daemon-reload-once
expect_update_failed
assert_old_state
[[ ! -e /tmp/forbidden-service-start && ! -e /tmp/forbidden-ufw-open ]]

# A rollback that cannot prove the old image present must stop immediately and
# retain the complete canonical transaction. A later successful recovery uses
# the same journal and keeps ingress closed.
reset_baseline
touch /tmp/fail-daemon-reload-once /tmp/drop-old-image-on-daemon-failure
expect_update_failed
[[ -d "$transaction" && ! -e "$retiring" && ! -e "$retired" ]]
"$real_rm" -f /tmp/missing-old-image /tmp/drop-old-image-on-daemon-failure
recovery_output="$("$updater" --recover --bootstrap-empty-host)"
[[ "$recovery_output" = 'interrupted empty-host image cutover rolled back; service and ingress remain off' ]]
assert_old_state

# A SIGKILL after the deterministic transaction preparation is synced has not
# changed any live image file. Recovery safely discards that exact temp name.
kill_update_at preparation-synced
[[ -d "$preparing" && ! -e "$transaction" ]]
cmp --silent /tmp/old-image "$image_file"
recovery_output="$("$updater" --recover --bootstrap-empty-host)"
[[ "$recovery_output" = 'interrupted empty-host image preparation discarded; service and ingress remain off' ]]
assert_old_state

# Preparation and committed-transaction cleanup first rename to deletion-only
# state. A real partial recursive deletion is therefore safe to resume.
kill_update_at preparation-synced
printf '%s\n' preparation-delete > /tmp/kill-bootstrap-after
set +e
"$updater" --recover --bootstrap-empty-host \
  >/tmp/bootstrap-delete-kill.stdout 2>/tmp/bootstrap-delete-kill.stderr
delete_kill_status=$?
set -e
"$real_rm" -f /tmp/kill-bootstrap-after
[[ $delete_kill_status -ne 0 && -d "$preparing_retired" ]]
recovery_output="$("$updater" --recover --bootstrap-empty-host)"
[[ "$recovery_output" = 'interrupted empty-host image preparation discarded; service and ingress remain off' ]]
assert_old_state

# Every pre-commit durable mutation point is SIGKILL recoverable without phase
# files or in-transaction temporaries.
for point in transaction-prepared quadlet-installed template-installed; do
  kill_update_at "$point"
  [[ -d "$transaction" ]]
  recovery_output="$("$updater" --recover --bootstrap-empty-host)"
  [[ "$recovery_output" = 'interrupted empty-host image cutover rolled back; service and ingress remain off' ]]
  assert_old_state
done

# A pending bootstrap image journal blocks every publisher deploy operation and
# restricted rsync before they can mutate corpus or authorization state.
kill_update_at image-pinned
[[ -d "$transaction" && "$(<"$image_file")" = "$new_digest" ]]
for action in prepare activate abort; do
  if /usr/local/sbin/legal-mcp-host-deploy "$action" "$(printf '%064d' 1)" \
    >/tmp/foreign.stdout 2>/tmp/foreign.stderr; then
    echo "bootstrap image transaction unexpectedly allowed $action" >&2
    exit 1
  fi
  grep -Fq 'a foreign host transaction must be recovered' /tmp/foreign.stderr
done
if runuser -u legal-mcp-publisher -- \
  env SSH_ORIGINAL_COMMAND="rsync --server -vlogDtpre.iLsfxCIvu . $(printf '%064d' 1)/" \
  /usr/local/sbin/legal-mcp-publisher-command \
  >/tmp/foreign.stdout 2>/tmp/foreign.stderr; then
  echo 'bootstrap image transaction unexpectedly allowed rsync' >&2
  exit 1
fi
grep -Fq 'a foreign host transaction must be recovered' /tmp/foreign.stderr
[[ -z "$(/usr/bin/find.fixture-real /srv/legal-mcp/generations \
  /srv/legal-mcp/uploads /srv/legal-mcp/state -mindepth 1 -maxdepth 1 \
  -printf x -quit)" ]]
recovery_output="$("$updater" --recover --bootstrap-empty-host)"
[[ "$recovery_output" = 'interrupted empty-host image cutover rolled back; service and ingress remain off' ]]
assert_old_state
[[ "$(stat -c '%U:%G:%a' "$image_file")" = root:root:600 \
  && "$(stat -c '%U:%G:%a' "$quadlet")" = root:root:644 \
  && "$(stat -c '%U:%G:%a' "$installed_template")" = root:root:644 ]]

# After all validation, retirement has a durable intermediate name. SIGKILL
# before either parent sync or before deletion resumes without undoing the
# already-verified image cutover.
for point in transaction-retiring transaction-retired transaction-delete; do
  kill_update_at "$point"
  [[ -d "$retiring" || -d "$retired" ]]
  recovery_output="$("$updater" --recover --bootstrap-empty-host)"
  [[ "$recovery_output" = 'interrupted empty-host image transaction retirement completed; service and ingress remain off' ]]
  assert_new_state
done

# Recovery refuses another source revision and unexpected durable content. Both
# cases first force public ingress and the service off and retain the journal.
kill_update_at transaction-prepared
printf '%040d\n' 3 > "$bundle/SOURCE_COMMIT"
touch /tmp/ufw-web-open /tmp/caddy-active /tmp/caddy-enabled
if "$updater" --recover --bootstrap-empty-host >/tmp/recover-source.stdout 2>/tmp/recover-source.stderr; then
  echo 'recovery from a different release source was unexpectedly accepted' >&2
  exit 1
fi
[[ -d "$transaction" && ! -e /tmp/ufw-web-open && ! -e /tmp/caddy-active ]]
printf '%s\n' "$revision" > "$bundle/SOURCE_COMMIT"
printf '%s\n' unknown > "$transaction/unexpected"
chmod 600 "$transaction/unexpected"
if "$updater" --recover --bootstrap-empty-host \
  >/tmp/recover-transaction.stdout 2>/tmp/recover-transaction.stderr; then
  echo 'unexpected image transaction content was accepted' >&2
  exit 1
fi
[[ -d "$transaction" && ! -e /tmp/service-active && ! -e /tmp/caddy-active ]]

# A successful cutover pins only software/template state. Corpus, auth, service,
# Caddy, and ingress remain exactly empty/disabled for the schema-11 upload.
reset_baseline
output="$(run_update)"
[[ "$output" = "empty bootstrap host pinned to $new_digest; service and ingress remain off" ]]
assert_new_state
[[ "$(awk -F= '$1 == "LEGAL_MCP_HTTP_AUTH" {print $2}' /etc/legal-mcp/runtime.env)" = disabled ]]
[[ "$(</etc/legal-mcp/api-keys.json)" = '{"keys":[],"version":1}' ]]
[[ -z "$(/usr/bin/find.fixture-real /srv/legal-mcp/generations /srv/legal-mcp/uploads /srv/legal-mcp/state \
  -mindepth 1 -maxdepth 1 -print -quit)" ]]
if grep -Eq '^systemctl:(start|restart|enable)([[:space:]]|$)' "$log"; then
  echo 'empty-host image cutover attempted to start or enable a service' >&2
  exit 1
fi

# The ordinary path uses a real authenticated HTTP probe server. It accepts
# only the exact configured key; curl remains deterministic for readiness and
# unauthenticated 401 checks.
python3 - <<'PY' &
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

EXPECTED = "automation.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/mcp" or self.headers.get("X-API-Key") != EXPECTED:
            self.send_response(401)
            self.end_headers()
            return
        open("/tmp/exact-api-key-observed", "wb").close()
        body = json.dumps({"result": {"serverInfo": {"name": "australian-legal-mcp"}}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass

HTTPServer(("127.0.0.1", 51235), Handler).serve_forever()
PY
api_server_pid=$!
trap 'kill "$api_server_pid" >/dev/null 2>&1 || true' EXIT
for _ in $(seq 1 100); do
  python3 -c 'import socket; s=socket.create_connection(("127.0.0.1",51235),.1); s.close()' \
    2>/dev/null && break
  kill -0 "$api_server_pid"
  sleep 0.02
done
kill -0 "$api_server_pid"

# urllib performs the real private API-key request above. The public URL is
# represented by the deterministic Caddy/TLS fake, so intercept only that one
# Python probe while still requiring the exact key bytes.
/usr/bin/mv /usr/bin/python3 /usr/bin/python3.fixture-real
cat > /usr/bin/python3 <<'EOF'
#!/usr/bin/bash
if [[ "$1" = -c && "${!#}" = https://legal.example.com/mcp ]]; then
  IFS= read -r key || [[ -n "$key" ]]
  [[ "$key" = automation.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA ]] || exit 1
  touch /tmp/exact-public-api-key-observed
  exit 0
fi
exec /usr/bin/python3.fixture-real "$@"
EOF
chmod 755 /usr/bin/python3

reset_ordinary_baseline() {
  local verifier
  reset_baseline
  # Model the launcher's private mount namespace after it bind-mounts the two
  # immutable implementations over the stable public entrypoints.
  "$real_install" -o root -g root -m 0755 /fixture-input/configure-auth.sh \
    /usr/local/sbin/legal-mcp-configure-auth
  "$real_install" -o root -g root -m 0755 /fixture-input/update-image.sh \
    /usr/local/sbin/legal-mcp-update-image
  touch /tmp/ordinary-mode /tmp/service-active /tmp/caddy-enabled \
    /tmp/caddy-active /tmp/ufw-web-open /tmp/podman-bare-image-ids
  printf '%s\n' "$old_image_id" > /tmp/old-image-id
  printf '%s\n' "$target_image_id" > /tmp/target-image-id
  printf '%s\n' "$old_image_id" > /tmp/running-image-id
  printf '%s' "$generation" > /srv/legal-mcp/lifecycle/active-generation
  chown root:root /srv/legal-mcp/lifecycle/active-generation
  chmod 644 /srv/legal-mcp/lifecycle/active-generation
  "$real_install" -d -o root -g legal-mcp -m 0750 \
    "/srv/legal-mcp/generations/$generation"
  "$real_install" -o root -g root -m 0444 /dev/null /etc/legal-mcp/auth-ready
  cat > /etc/legal-mcp/runtime.env <<'EOF'
LEGAL_MCP_HTTP_AUTH=api-key
LEGAL_MCP_API_KEYS_FILE=/run/secrets/legal-mcp-api-keys.json
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
LEGAL_MCP_ALLOWED_ORIGINS=https://legal.example.com
LEGAL_MCP_HTTP_WORKERS=4
LEGAL_MCP_SHUTDOWN_GRACE_SECONDS=30
EOF
  chown root:root /etc/legal-mcp/runtime.env
  chmod 600 /etc/legal-mcp/runtime.env
  verifier="$(printf '%s' "$probe_key" | sha256sum | awk '{print $1}')"
  printf '{"keys":[{"id":"automation","sha256":"%s"}],"version":1}\n' "$verifier" \
    > /etc/legal-mcp/api-keys.json
  chown legal-mcp:legal-mcp /etc/legal-mcp/api-keys.json
  chmod 400 /etc/legal-mcp/api-keys.json
  "$real_rm" -f /tmp/exact-api-key-observed /tmp/exact-public-api-key-observed
  : > "$log"
}

assert_ordinary_old_state() {
  cmp --silent /tmp/old-image "$image_file"
  cmp --silent /tmp/old-template "$installed_template"
  cmp --silent /tmp/old-quadlet "$quadlet"
  [[ "$(</tmp/running-image-id)" = "$old_image_id" \
    && -e /tmp/service-active && -e /tmp/caddy-active \
    && -e /tmp/caddy-enabled && -e /tmp/ufw-web-open \
    && ! -e "$transaction" && ! -e "$preparing" \
    && ! -e "$preparing_retired" && ! -e "$retiring" && ! -e "$retired" ]]
  [[ "$(stat -c '%U:%G:%a:%s' /srv/legal-mcp/lifecycle/active-generation)" \
    = root:root:644:64 ]]
  [[ "$(stat -c '%U:%G:%a:%s' /etc/legal-mcp/auth-ready)" = root:root:444:0 ]]
  [[ "$(</srv/legal-mcp/lifecycle/active-generation)" = "$generation" ]]
}

assert_ordinary_new_state() {
  [[ "$(<"$image_file")" = "$new_digest" \
    && "$(</tmp/running-image-id)" = "$target_image_id" ]]
  cmp --silent "$source_template" "$installed_template"
  sed "s|__IMAGE_DIGEST__|$new_digest|g" "$source_template" \
    > /tmp/expected-ordinary-quadlet
  cmp --silent /tmp/expected-ordinary-quadlet "$quadlet"
  [[ -e /tmp/service-active && -e /tmp/caddy-active \
    && -e /tmp/caddy-enabled && -e /tmp/ufw-web-open \
    && ! -e "$transaction" && ! -e "$preparing" \
    && ! -e "$preparing_retired" && ! -e "$retiring" && ! -e "$retired" \
    && -e /tmp/exact-api-key-observed \
    && -e /tmp/exact-public-api-key-observed ]]
  [[ "$(stat -c '%U:%G:%a:%s' /srv/legal-mcp/lifecycle/active-generation)" \
    = root:root:644:64 ]]
  [[ "$(stat -c '%U:%G:%a:%s' /etc/legal-mcp/auth-ready)" = root:root:444:0 ]]
}

run_ordinary_update() {
  printf '%s\n' "$probe_key" | /usr/local/sbin/legal-mcp-update-image \
    --image "$new_digest" --version "$version" --template "$source_template"
}

recover_ordinary_update() {
  printf '%s\n' "$probe_key" | /usr/local/sbin/legal-mcp-update-image --recover
}

use_outer_host_tool_entrypoints() {
  "$real_install" -o root -g root -m 0755 /tmp/host-tool-launcher \
    /usr/local/sbin/legal-mcp-configure-auth
  "$real_install" -o root -g root -m 0755 /tmp/host-tool-launcher \
    /usr/local/sbin/legal-mcp-update-image
}

run_direct_ordinary_update() {
  printf '%s\n' "$probe_key" | "$updater" \
    --image "$new_digest" --version "$version" --template "$source_template"
}

recover_direct_ordinary_update() {
  printf '%s\n' "$probe_key" | "$updater" --recover
}

kill_ordinary_at() {
  local point="$1" status
  reset_ordinary_baseline
  printf '%s\n' "$point" > /tmp/kill-bootstrap-after
  set +e
  run_ordinary_update >/tmp/ordinary-kill.stdout 2>/tmp/ordinary-kill.stderr
  status=$?
  set -e
  "$real_rm" -f /tmp/kill-bootstrap-after
  [[ $status -ne 0 ]]
}

kill_direct_ordinary_at() {
  local point="$1" status
  reset_ordinary_baseline
  use_outer_host_tool_entrypoints
  printf '%s\n' "$point" > /tmp/kill-bootstrap-after
  set +e
  run_direct_ordinary_update >/tmp/ordinary-direct-kill.stdout \
    2>/tmp/ordinary-direct-kill.stderr
  status=$?
  set -e
  "$real_rm" -f /tmp/kill-bootstrap-after
  [[ $status -ne 0 ]]
}

# Exact active-pointer bytes, installed-template rendering, and the running
# image ID are mandatory before the V2 journal can be published.
reset_ordinary_baseline
printf '%s\n' "$generation" > /srv/legal-mcp/lifecycle/active-generation
if run_ordinary_update >/tmp/ordinary-pointer.stdout 2>/tmp/ordinary-pointer.stderr; then
  echo 'newline-terminated generation pointer was unexpectedly accepted' >&2
  exit 1
fi
[[ ! -e "$transaction" && ! -e "$preparing" ]]

reset_ordinary_baseline
chmod 600 /srv/legal-mcp/lifecycle/active-generation
if run_ordinary_update >/tmp/ordinary-pointer.stdout 2>/tmp/ordinary-pointer.stderr; then
  echo 'wrong active-generation metadata was unexpectedly accepted' >&2
  exit 1
fi
[[ ! -e "$transaction" && ! -e "$preparing" ]]

reset_ordinary_baseline
printf '%s\n\n' "$old_digest" > "$image_file"
if run_ordinary_update >/tmp/ordinary-image-pin.stdout 2>/tmp/ordinary-image-pin.stderr; then
  echo 'inexact old image pin bytes were unexpectedly accepted' >&2
  exit 1
fi
[[ ! -e "$transaction" && ! -e "$preparing" ]]

reset_ordinary_baseline
chmod 644 /etc/legal-mcp/auth-ready
if run_ordinary_update >/tmp/ordinary-auth-ready.stdout 2>/tmp/ordinary-auth-ready.stderr; then
  echo 'unsafe authentication-ready marker was unexpectedly accepted' >&2
  exit 1
fi
grep -Fq 'required host file is missing or unsafe: /etc/legal-mcp/auth-ready' \
  /tmp/ordinary-auth-ready.stderr
[[ ! -e "$transaction" && ! -e "$preparing" ]]

reset_ordinary_baseline
chmod 644 /etc/caddy/Caddyfile
if run_ordinary_update >/tmp/ordinary-caddy-mode.stdout 2>/tmp/ordinary-caddy-mode.stderr; then
  echo 'unsafe Caddyfile metadata was unexpectedly accepted' >&2
  exit 1
fi
grep -Fq 'required host file is missing or unsafe: /etc/caddy/Caddyfile' \
  /tmp/ordinary-caddy-mode.stderr
[[ ! -e "$transaction" && ! -e "$preparing" ]]

reset_ordinary_baseline
sed -i 's|@mcp path /mcp /.well-known/oauth-protected-resource/mcp|@mcp path /*|' \
  /etc/caddy/Caddyfile
touch /tmp/overbroad-caddy
if run_ordinary_update >/tmp/ordinary-caddy-route.stdout 2>/tmp/ordinary-caddy-route.stderr; then
  echo 'overbroad adapted Caddy route object was unexpectedly accepted' >&2
  exit 1
fi
grep -Fq 'adapted Caddy routes do not match the exact MCP-only contract' \
  /tmp/ordinary-caddy-route.stderr
[[ ! -e "$transaction" && ! -e "$preparing" ]]

reset_ordinary_baseline
touch /tmp/overbroad-service-listener
if run_ordinary_update >/tmp/ordinary-listener.stdout 2>/tmp/ordinary-listener.stderr; then
  echo 'non-loopback service listener was unexpectedly accepted' >&2
  exit 1
fi
[[ ! -e "$transaction" && ! -e "$preparing" ]]

reset_ordinary_baseline
touch /tmp/extra-web-listener
if run_ordinary_update >/tmp/ordinary-web-listener.stdout \
  2>/tmp/ordinary-web-listener.stderr; then
  echo 'unintended additional web listener was unexpectedly accepted' >&2
  exit 1
fi
[[ ! -e "$transaction" && ! -e "$preparing" ]]

reset_ordinary_baseline
touch /tmp/overbroad-public-route
if run_ordinary_update >/tmp/ordinary-negative-route.stdout 2>/tmp/ordinary-negative-route.stderr; then
  echo 'unexpected public route was accepted before journalling' >&2
  exit 1
fi
grep -Fq 'unexpected public route or redirect' /tmp/ordinary-negative-route.stderr
[[ ! -e "$transaction" && ! -e "$preparing" ]]

reset_ordinary_baseline
printf '%s\n' drift >> "$quadlet"
if run_ordinary_update >/tmp/ordinary-quadlet.stdout 2>/tmp/ordinary-quadlet.stderr; then
  echo 'unrendered old Quadlet was unexpectedly accepted' >&2
  exit 1
fi
grep -Fq 'installed Quadlet is not the installed template rendered with /etc/legal-mcp/image' \
  /tmp/ordinary-quadlet.stderr
[[ ! -e "$transaction" && ! -e "$preparing" ]]

reset_ordinary_baseline
printf '%s\n' "$target_image_id" > /tmp/running-image-id
if run_ordinary_update >/tmp/ordinary-running.stdout 2>/tmp/ordinary-running.stderr; then
  echo 'running image outside the old pin was unexpectedly accepted' >&2
  exit 1
fi
grep -Fq 'running container does not use the image pinned by /etc/legal-mcp/image' \
  /tmp/ordinary-running.stderr
[[ ! -e "$transaction" && ! -e "$preparing" ]]

# A well-shaped but incorrect key is rejected by the real positive probe and
# is never echoed to stdout/stderr or persisted by the fixture server.
reset_ordinary_baseline
wrong_key='automation.BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB'
if printf '%s\n' "$wrong_key" | /usr/local/sbin/legal-mcp-update-image \
  --image "$new_digest" --version "$version" --template "$source_template" \
  >/tmp/ordinary-key.stdout 2>/tmp/ordinary-key.stderr; then
  echo 'incorrect API key was unexpectedly accepted' >&2
  exit 1
fi
if grep -Fq "$wrong_key" /tmp/ordinary-key.stdout /tmp/ordinary-key.stderr; then
  echo 'incorrect API key leaked to image-update output' >&2
  exit 1
fi
[[ ! -e "$transaction" && ! -e "$preparing" ]]

# A preparing pathname is not deletion authorization by itself. It must be a
# complete exact V2 image journal belonging to this release.
reset_ordinary_baseline
install -d -o root -g root -m 0700 "$preparing"
printf '%s\n' arbitrary > "$preparing/kind"
chmod 600 "$preparing/kind"
: > "$log"
if recover_ordinary_update >/tmp/ordinary-loose-preparing.stdout \
  2>/tmp/ordinary-loose-preparing.stderr; then
  echo 'loose image preparation directory was unexpectedly deleted' >&2
  exit 1
fi
[[ -d "$preparing" && ! -s "$log" ]]

# A complete synced preparation is deletion-only. Recovery first validates
# the exact release implementation/marker, then retires the preparation.
kill_ordinary_at preparation-synced
[[ -d "$preparing" && ! -e "$transaction" ]]
recovery_output="$(recover_ordinary_update)"
[[ "$recovery_output" = 'interrupted image preparation discarded' ]]
assert_ordinary_old_state

# The canonical V2 journal contains exact release, saved/target hash and
# metadata manifests plus all service/auth/image identities.
kill_ordinary_at transaction-prepared
[[ -d "$transaction" && "$(<"$transaction/kind")" = LEGAL_MCP_IMAGE_TRANSACTION_V2 ]]
[[ "$(<"$transaction/target-version")" = "$version" \
  && "$(<"$transaction/target-revision")" = "$revision" \
  && "$(<"$transaction/retirement-outcome")" = pending ]]
grep -Fxq "EXPECTED_GENERATION=$generation" "$transaction/state"
grep -Fxq "OLD_IMAGE_ID=$old_image_id" "$transaction/state"
grep -Fxq "TARGET_IMAGE_ID=$target_image_id" "$transaction/state"
grep -Fxq 'AUTH_MODE=api-key' "$transaction/state"
grep -Fxq 'SERVICE_ENABLEMENT=generated' "$transaction/state"
grep -Fxq 'SERVICE_ACTIVITY=active' "$transaction/state"
grep -Fxq 'CADDY_ENABLEMENT=enabled' "$transaction/state"
grep -Fxq 'CADDY_ACTIVITY=active' "$transaction/state"
grep -Fxq 'UFW_80=present' "$transaction/state"
grep -Fxq 'UFW_443=present' "$transaction/state"
grep -Fxq 'AUTH_READY=root:root:444:1:0' "$transaction/saved-metadata"
grep -Eq '^AUTH_READY_SHA256=[0-9a-f]{64}$' "$transaction/saved-sha256"
[[ "$(stat -c '%U:%G:%a:%h' "$transaction/saved-sha256")" = root:root:600:1 \
  && "$(stat -c '%U:%G:%a:%h' "$transaction/target-sha256")" = root:root:600:1 ]]
recovery_output="$(recover_ordinary_update)"
[[ "$recovery_output" = 'interrupted image transaction rolled back' ]]
assert_ordinary_old_state

# Recovery validates the immutable implementation and installed V2 marker
# before it closes ingress or restores any file. Another SOURCE_COMMIT leaves the exact
# canonical journal untouched and performs no service/firewall action.
kill_ordinary_at transaction-prepared
sed -i 's/^SOURCE_COMMIT=.*/SOURCE_COMMIT=3333333333333333333333333333333333333333/' \
  /etc/legal-mcp/host-tools
touch /tmp/caddy-enabled /tmp/caddy-active /tmp/ufw-web-open
: > "$log"
if recover_ordinary_update >/tmp/ordinary-release.stdout 2>/tmp/ordinary-release.stderr; then
  echo 'ordinary recovery from a different release was unexpectedly accepted' >&2
  exit 1
fi
[[ -d "$transaction" && -e /tmp/caddy-active && -e /tmp/ufw-web-open \
  && ! -s "$log" ]]
"$real_install" -o root -g root -m 0444 /tmp/expected-host-tools \
  /etc/legal-mcp/host-tools
recovery_output="$(recover_ordinary_update)"
[[ "$recovery_output" = 'interrupted image transaction rolled back' ]]
assert_ordinary_old_state

# Changed rollback bytes are rejected against the saved hash manifest before
# any live mutation; the complete journal remains for operator diagnosis.
kill_ordinary_at transaction-prepared
printf '%s\n' tampered > "$transaction/saved-image"
: > "$log"
if recover_ordinary_update >/tmp/ordinary-hash.stdout 2>/tmp/ordinary-hash.stderr; then
  echo 'tampered saved image state was unexpectedly recovered' >&2
  exit 1
fi
grep -Fq 'saved image transaction bytes do not match their hash manifest' \
  /tmp/ordinary-hash.stderr
[[ -d "$transaction" && ! -s "$log" ]]

# A runtime failure after the target pin is installed automatically restores
# the exact old authenticated service, image ID, template, Quadlet and state.
reset_ordinary_baseline
touch /tmp/fail-service-restart-once
if run_ordinary_update >/tmp/ordinary-rollback.stdout 2>/tmp/ordinary-rollback.stderr; then
  echo 'injected ordinary restart failure unexpectedly succeeded' >&2
  exit 1
fi
grep -Fq 'container image update rolled back' /tmp/ordinary-rollback.stderr
assert_ordinary_old_state
[[ -e /tmp/exact-api-key-observed ]]

# SIGKILL after the image pin changed leaves the exact active V2 journal.
# Recovery from the same release restores the old image and authenticated API.
kill_ordinary_at image-pinned
[[ -d "$transaction" && "$(<"$image_file")" = "$new_digest" ]]
recovery_output="$(recover_ordinary_update)"
[[ "$recovery_output" = 'interrupted image transaction rolled back' ]]
assert_ordinary_old_state
[[ -e /tmp/exact-api-key-observed ]]

# SIGKILL in committed retirement is not rollback: same-release recovery
# revalidates the complete target host/API state before completing deletion.
kill_ordinary_at transaction-retiring
[[ -d "$retiring" && ! -e "$transaction" ]]
recovery_output="$(recover_ordinary_update)"
[[ "$recovery_output" = 'interrupted image transaction retirement completed' ]]
assert_ordinary_new_state

# SIGKILL after the exact journal reaches .retired leaves a deletion-only
# state, but deletion is authorized only after revalidating the complete V2
# journal and unchanged live target. Tampered live metadata leaves it intact.
kill_ordinary_at transaction-retired
[[ -d "$retired" && ! -e "$retiring" && ! -e "$transaction" \
  && "$(<"$retired/retirement-outcome")" = target ]]
chmod 644 /etc/caddy/Caddyfile
: > "$log"
if recover_ordinary_update >/tmp/ordinary-retired-tamper.stdout \
  2>/tmp/ordinary-retired-tamper.stderr; then
  echo 'retired image journal ignored tampered live target metadata' >&2
  exit 1
fi
[[ -d "$retired" ]]
if grep -Eq '^(systemctl|ufw):' "$log"; then
  echo 'retired live-state rejection mutated service or firewall state' >&2
  exit 1
fi
chmod 640 /etc/caddy/Caddyfile
printf '%s\n' "$old_image_id" > /tmp/running-image-id
if recover_ordinary_update >/tmp/ordinary-retired-image.stdout \
  2>/tmp/ordinary-retired-image.stderr; then
  echo 'retired image journal ignored a different running image ID' >&2
  exit 1
fi
[[ -d "$retired" ]]
printf '%s\n' "$target_image_id" > /tmp/running-image-id
recovery_output="$(recover_ordinary_update)"
[[ "$recovery_output" = 'interrupted image transaction retirement completed' ]]
assert_ordinary_new_state

# The documented exact release-bundle path runs outside the launcher's mount
# namespace, so it proves the outer stable entrypoints while binding the
# executing bundle bytes to the same installed V2 marker. Both update and
# same-bundle recovery remain supported.
reset_ordinary_baseline
use_outer_host_tool_entrypoints
direct_output="$(run_direct_ordinary_update)"
[[ "$direct_output" = "container image updated to $new_digest" ]]
assert_ordinary_new_state

kill_direct_ordinary_at image-pinned
[[ -d "$transaction" && "$(<"$image_file")" = "$new_digest" ]]
recovery_output="$(recover_direct_ordinary_update)"
[[ "$recovery_output" = 'interrupted image transaction rolled back' ]]
assert_ordinary_old_state

# The ordinary authenticated success path leaves no transaction state and
# preserves the exact 64-byte pointer, API verifier, Caddy and UFW baseline.
# Model the stable launcher holding the real lock: the implementation must
# select that exact inherited open file description rather than reopening the
# path (which would deadlock against its launcher).
reset_ordinary_baseline
exec {inherited_lock_fd}<>"$host_transaction_lock"
flock -x "$inherited_lock_fd"
export LEGAL_MCP_HOST_TRANSACTION_LOCK_FD="$inherited_lock_fd"
export LEGAL_MCP_FIXTURE_FLOCK_RECORD=/tmp/inherited-host-lock-record
ordinary_output="$(run_ordinary_update)"
unset LEGAL_MCP_HOST_TRANSACTION_LOCK_FD LEGAL_MCP_FIXTURE_FLOCK_RECORD
inherited_lock_record="$(</tmp/inherited-host-lock-record)"
[[ "${inherited_lock_record%%:*}" = "$inherited_lock_fd" \
  && "${inherited_lock_record#*:}" = "$(stat -Lc '%d:%i' "$host_transaction_lock")" ]]
exec {inherited_lock_fd}>&-
[[ "$ordinary_output" = "container image updated to $new_digest" ]]
assert_ordinary_new_state
verifier="$(printf '%s' "$probe_key" | sha256sum | awk '{print $1}')"
grep -Fq "\"sha256\":\"$verifier\"" /etc/legal-mcp/api-keys.json
if grep -Fq "$probe_key" /etc/legal-mcp/api-keys.json "$log"; then
  echo 'plaintext API key leaked to a persisted host file or action log' >&2
  exit 1
fi

kill "$api_server_pid"
wait "$api_server_pid" 2>/dev/null || true
trap - EXIT

echo bootstrap-empty-host-image-fixture-ok
