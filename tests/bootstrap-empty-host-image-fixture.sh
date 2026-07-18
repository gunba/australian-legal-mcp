#!/usr/bin/env bash
# Exercise the no-corpus image cutover, rollback, and explicit recovery with
# deterministic host/OCI fakes. No network or real service manager is used.
set -euo pipefail
umask 077
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

[[ $EUID -eq 0 && -e /.dockerenv \
  && -f /fixture-input/update-image.sh \
  && -f /fixture-input/legal-mcp.container.template \
  && -f /fixture-input/legal-mcp-host-deploy \
  && -f /fixture-input/legal-mcp-publisher-command \
  && -f /fixture-input/Containerfile ]] || {
  echo 'fixture requires a disposable root container and mounted inputs' >&2
  exit 2
}

version=0.19.1
revision=1111111111111111111111111111111111111111
old_revision=2222222222222222222222222222222222222222
old_digest="ghcr.io/gunba/australian-legal-mcp@sha256:$(printf 'a%.0s' {1..64})"
new_digest="ghcr.io/gunba/australian-legal-mcp@sha256:$(printf 'b%.0s' {1..64})"
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
      printf '%s\n' 'legal-mcp 0.19.1'
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
  /run/lock/legal-mcp-host-transaction.lock
install -d -o root -g legal-mcp -m 0750 /srv/legal-mcp
setfacl --remove-all /srv/legal-mcp
setfacl --modify user:legal-mcp-publisher:--x /srv/legal-mcp
install -d -o root -g legal-mcp -m 0750 \
  /srv/legal-mcp/generations /srv/legal-mcp/lifecycle
install -d -o legal-mcp -g legal-mcp -m 0700 /srv/legal-mcp/state
install -d -o legal-mcp-publisher -g legal-mcp-publisher -m 0700 /srv/legal-mcp/uploads
install -o root -g legal-mcp -m 0640 /dev/null /srv/legal-mcp/lifecycle/LOCK
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
printf '%s\n' fixture-caddy > /etc/caddy/Caddyfile
chown root:caddy /etc/caddy/Caddyfile
chmod 640 /etc/caddy/Caddyfile
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
deploy_sha="$(sha256sum /usr/local/sbin/legal-mcp-host-deploy | awk '{print $1}')"
publisher_sha="$(sha256sum /usr/local/sbin/legal-mcp-publisher-command | awk '{print $1}')"
cat > /etc/sudoers.d/legal-mcp-publisher <<EOF
Defaults:legal-mcp-publisher !requiretty
legal-mcp-publisher ALL=(root) NOPASSWD: sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^prepare [0-9a-f]{64}$, sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^activate [0-9a-f]{64}$, sha256:$deploy_sha /usr/local/sbin/legal-mcp-host-deploy ^abort [0-9a-f]{64}$
EOF
chmod 440 /etc/sudoers.d/legal-mcp-publisher
visudo -cf /etc/sudoers.d/legal-mcp-publisher >/dev/null
sudoers_sha="$(sha256sum /etc/sudoers.d/legal-mcp-publisher | awk '{print $1}')"
cat > /etc/legal-mcp/host-tools <<EOF
LEGAL_MCP_HOST_TOOLS_V1
VERSION=$version
SOURCE_COMMIT=$revision
HOST_DEPLOY_SHA256=$deploy_sha
PUBLISHER_COMMAND_SHA256=$publisher_sha
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
cat > /usr/bin/ss <<'EOF'
#!/usr/bin/bash
if [[ -e /tmp/fail-ss ]]; then exit 86; fi
if [[ -e /tmp/bootstrap-listener ]]; then
  printf 'LISTEN 0 4096 0.0.0.0:443 0.0.0.0:*\n'
fi
EOF
chmod 755 /usr/bin/findmnt /usr/sbin/blkid /usr/sbin/xfs_info /usr/bin/ss

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
  start|restart|enable)
    touch /tmp/forbidden-service-start
    exit 96
    ;;
  stop)
    rm -f /tmp/service-active
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
if [[ "$*" == '--force delete allow 80/tcp' \
  || "$*" == '--force delete allow 443/tcp' ]]; then
  [[ ! -e /tmp/fail-ufw-delete ]] || exit 88
  rm -f /tmp/ufw-web-open
  exit 0
fi
if [[ "$1" = allow ]]; then
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
case "\$1" in
  container)
    [[ "\$2" = exists ]]
    if [[ -e /tmp/podman-container-error ]]; then exit 125; fi
    [[ -e /tmp/existing-container ]]
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
        if [[ "\$format" == *'.version'* ]]; then
          if [[ "\$image" = "\$new_image" ]]; then
            if [[ -e /tmp/wrong-oci-version ]]; then printf '%s\n' 9.9.9; else printf '%s\n' 0.19.1; fi
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
        if [[ -e /tmp/wrong-oci-binary ]]; then printf '%s\n' 'legal-mcp 9.9.9'; else printf '%s\n' 'legal-mcp 0.19.1'; fi
        ;;
      verify-runtime) printf '%s\n' '{"onnx_runtime_ready":true}' ;;
      *) exit 95 ;;
    esac
    ;;
  *) exit 96 ;;
esac
EOF
chmod 755 /usr/bin/podman

/usr/bin/mv /usr/bin/install /usr/bin/install.fixture-real
/usr/bin/mv /usr/bin/find /usr/bin/find.fixture-real
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
chmod 755 /usr/bin/install /usr/bin/find /usr/bin/mv /usr/bin/rm /usr/bin/sync
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
    ! -name LOCK -exec /usr/bin/rm.fixture-real -rf -- {} +
  "$real_rm" -f /run/legal-mcp/authorized-upload /tmp/*-active /tmp/*-enabled \
    /tmp/ufw-web-open /tmp/ufw-extra-open /tmp/bootstrap-listener \
    /tmp/existing-container /tmp/missing-old-image /tmp/podman-container-error \
    /tmp/podman-image-error /tmp/fail-systemctl-* /tmp/fail-ufw-* \
    /tmp/service-wrong-enablement \
    /tmp/wrong-* /tmp/fail-daemon-reload-once /tmp/drop-old-image-on-daemon-failure \
    /tmp/fail-find /tmp/fail-ss /tmp/kill-bootstrap-after \
    /tmp/forbidden-service-start /tmp/forbidden-ufw-open
  : > "$log"
  "$real_install" -o root -g root -m 0600 /tmp/old-image "$image_file"
  "$real_install" -o root -g root -m 0644 /tmp/old-template "$installed_template"
  "$real_install" -o root -g root -m 0644 /tmp/old-quadlet "$quadlet"
  "$real_install" -o root -g root -m 0644 /fixture-input/legal-mcp.container.template "$source_template"
  printf '%s\n' "$revision" > "$bundle/SOURCE_COMMIT"
  chmod 644 "$bundle/SOURCE_COMMIT"
  chmod 755 "$updater" "$bundle/legal-mcp" \
    "$bundle/scripts/legal-mcp-host-deploy" "$bundle/scripts/legal-mcp-publisher-command"
  chmod 644 "$source_template" "$bundle/Containerfile" "$bundle/libonnxruntime.so"
  rm -f /etc/legal-mcp/host-installed /etc/legal-mcp/host-tools \
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

reset_baseline
printf '%040d\n' 3 > "$bundle/SOURCE_COMMIT"
expect_update_failed
assert_old_state

reset_baseline
touch /tmp/wrong-release-binary
expect_update_failed
assert_old_state

for failure in wrong-oci-source wrong-oci-revision wrong-oci-version wrong-oci-binary; do
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
grep -Fq 'ufw:--force delete allow 80/tcp' "$log"

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

reset_baseline
chmod 644 "$image_file"
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

echo bootstrap-empty-host-image-fixture-ok
