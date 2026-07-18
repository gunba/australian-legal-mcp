#!/usr/bin/env bash
# Exercise the version-matched V2 host-tool upgrade and its recovery on both
# prepared-bootstrap and activated-but-dark SSH-only hosts.
set -euo pipefail
umask 027
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

[[ $EUID -eq 0 && -e /.dockerenv \
  && -f /fixture-input/install-host.sh \
  && -f /fixture-input/legal-mcp-host-deploy \
  && -f /fixture-input/legal-mcp-publisher-command \
  && -f /fixture-input/Containerfile ]] || {
  echo 'fixture requires a disposable root container and mounted inputs' >&2
  exit 2
}

version=0.19.6
revision=1111111111111111111111111111111111111111
generation=1a6beead567b55babebbe253b5ae13efcd9ce2e8ab55b60c2de4106e39f180f4
volume_uuid=11111111-2222-3333-4444-555555555555
bundle=/bundle
installer=$bundle/infra/linode/install-host.sh
transaction=/etc/legal-mcp/.host-tools-transaction
building=${transaction}.building
building_retired=${transaction}.building-retired
preparing=${transaction}.preparing
preparing_retired=${transaction}.preparing-retired
retiring=${transaction}.retiring
retired=${transaction}.retired
rollback_retiring=${transaction}.rollback-retiring
rollback_retired=${transaction}.rollback-retired
publisher_restore=${transaction}.publisher-restore
publisher_restore_retired=${transaction}.publisher-restore-retired
journal=/srv/legal-mcp/lifecycle/.deployment-transaction
lifecycle_lock=/srv/legal-mcp/lifecycle/LIFECYCLE_LOCK
upload=/srv/legal-mcp/uploads/$generation
authorization=/run/legal-mcp/authorized-upload
host_deploy=/usr/local/sbin/legal-mcp-host-deploy
publisher=/usr/local/sbin/legal-mcp-publisher-command
configure_auth=/usr/local/sbin/legal-mcp-configure-auth
update_image=/usr/local/sbin/legal-mcp-update-image
container_template=/usr/local/libexec/legal-mcp/legal-mcp.container.template
rendered_quadlet=/etc/containers/systemd/legal-mcp.container
host_tool_launcher=/usr/local/libexec/legal-mcp/host-tool-launcher
launcher_marker=/etc/legal-mcp/host-tool-launcher
configure_pointer=/etc/legal-mcp/configure-auth-implementation
update_pointer=/etc/legal-mcp/update-image-implementation
implementation_dir=/usr/local/libexec/legal-mcp/host-tools
auth_ready=/etc/legal-mcp/auth-ready
sudoers=/etc/sudoers.d/legal-mcp-publisher

for command_name in flock getfacl groupadd mknod python3 setfacl sudo useradd visudo; do
  command -v "$command_name" >/dev/null || {
    echo "fixture dependency is missing: $command_name" >&2
    exit 2
  }
done

install -d -o root -g root -m 0755 \
  "$bundle/infra/hosting" "$bundle/infra/linode" "$bundle/scripts"
install -o root -g root -m 0755 /fixture-input/install-host.sh "$installer"
install -o root -g root -m 0755 /fixture-input/legal-mcp-host-deploy \
  "$bundle/scripts/legal-mcp-host-deploy"
install -o root -g root -m 0755 /fixture-input/legal-mcp-publisher-command \
  "$bundle/scripts/legal-mcp-publisher-command"
install -o root -g root -m 0644 /fixture-input/Containerfile "$bundle/Containerfile"
cat > /tmp/new-configure-auth <<'EOF'
#!/usr/bin/bash
printf '%s\n' new-configure-auth
EOF
cat > /tmp/new-update-image <<'EOF'
#!/usr/bin/bash
printf '%s\n' new-update-image
EOF
cat > /tmp/new-container-template <<'EOF'
Image=__IMAGE_DIGEST__
[Service]
ExecCondition=/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready
EOF
install -o root -g root -m 0755 /tmp/new-configure-auth \
  "$bundle/infra/hosting/configure-auth.sh"
install -o root -g root -m 0755 /tmp/new-update-image \
  "$bundle/infra/hosting/update-image.sh"
install -o root -g root -m 0644 /tmp/new-container-template \
  "$bundle/infra/hosting/legal-mcp.container.template"
printf '%s\n' "$revision" > "$bundle/SOURCE_COMMIT"
chmod 644 "$bundle/SOURCE_COMMIT"
cat > "$bundle/legal-mcp" <<'EOF'
#!/usr/bin/bash
if [[ "$1" = --version ]]; then
  if [[ -e /tmp/wrong-release-binary ]]; then
    printf '%s\n' 'legal-mcp 9.9.9'
  else
    printf '%s\n' 'legal-mcp 0.19.6'
  fi
  exit 0
fi
exit 91
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
  /etc/legal-mcp /etc/sudoers.d /etc/containers/systemd /etc/caddy \
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
install -o root -g root -m 0640 /dev/null "$lifecycle_lock"
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
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
EOF
chmod 600 /etc/legal-mcp/runtime.env
printf '%s\n' 'ghcr.io/gunba/australian-legal-mcp@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' \
  > /etc/legal-mcp/image
chmod 600 /etc/legal-mcp/image
printf '%s\n' '__IMAGE_DIGEST__' > "$container_template"
chmod 644 "$container_template"
sed 's|__IMAGE_DIGEST__|ghcr.io/gunba/australian-legal-mcp@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa|' \
  "$container_template" \
  > "$rendered_quadlet"
chmod 644 "$rendered_quadlet"
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

[[ -b /dev/fixture-xfs ]] || mknod /dev/fixture-xfs b 7 240
cat > /usr/bin/findmnt <<'EOF'
#!/usr/bin/bash
if [[ "$*" == *'--output TARGET,SOURCE,FSTYPE,OPTIONS'* ]]; then
  printf '/srv/legal-mcp /dev/fixture-xfs xfs rw,noatime,nodev,noexec,nosuid\n'
elif [[ "$*" == *'--output SOURCE,FSTYPE,OPTIONS'* ]]; then
  printf '/dev/fixture-xfs xfs rw,noatime,nodev,noexec,nosuid\n'
elif [[ "$*" == *'--submounts'* ]]; then
  printf '/srv/legal-mcp\n'
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
cat > /usr/bin/systemctl <<'EOF'
#!/usr/bin/bash
unit_flag() {
  case "$1" in
    legal-mcp.service) printf '%s' service ;;
    caddy.service) printf '%s' caddy ;;
    *) printf '%s' unknown ;;
  esac
}
case "$1" in
  is-enabled)
    [[ ! -e /tmp/fail-systemctl-enabled ]] || exit 86
    flag="$(unit_flag "$2")"
    if [[ "$flag" = service ]]; then
      if [[ -e /tmp/service-wrong-enablement ]]; then printf '%s\n' enabled; else printf '%s\n' generated; fi
      exit 0
    fi
    if [[ -e /tmp/caddy-enabled ]]; then printf '%s\n' enabled; exit 0; fi
    printf '%s\n' disabled
    exit 1
    ;;
  is-active)
    [[ ! -e /tmp/fail-systemctl-active ]] || exit 87
    flag="$(unit_flag "$2")"
    if [[ -e "/tmp/${flag}-active" ]]; then printf '%s\n' active; exit 0; fi
    printf '%s\n' inactive
    exit 3
    ;;
  disable)
    [[ ! -e /tmp/fail-systemctl-disable ]] || exit 88
    rm -f /tmp/caddy-enabled /tmp/caddy-active
    exit 0
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
if [[ "$1" = status ]]; then
  [[ ! -e /tmp/fail-ufw-status ]] || exit 89
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
  exit 0
fi
if [[ "$*" == '--force delete allow 80/tcp' \
  || "$*" == '--force delete allow 443/tcp' ]]; then
  [[ ! -e /tmp/fail-ufw-delete ]] || exit 90
  rm -f /tmp/ufw-web-open
  exit 0
fi
exit 91
EOF
cat > /usr/bin/ss <<'EOF'
#!/usr/bin/bash
if [[ -e /tmp/fail-ss ]]; then exit 86; fi
if [[ -e /tmp/web-listener ]]; then
  printf '%s\n' 'LISTEN 0 4096 127.0.0.1:51235 0.0.0.0:*'
fi
if [[ -e /tmp/service-active ]]; then
  printf '%s\n' 'LISTEN 0 4096 127.0.0.1:51235 0.0.0.0:*'
fi
if [[ -e /tmp/caddy-active ]]; then
  printf '%s\n' \
    'LISTEN 0 4096 *:80 *:*' \
    'LISTEN 0 4096 *:443 *:*'
fi
exit 0
EOF
chmod 755 /usr/bin/findmnt /usr/sbin/blkid /usr/sbin/xfs_info \
  /usr/bin/systemctl /usr/sbin/ufw /usr/bin/ss

cat > /tmp/old-host-deploy <<'EOF'
#!/usr/bin/bash
printf '%s\n' old-host-deploy
EOF
cat > /tmp/old-publisher <<'EOF'
#!/usr/bin/bash
printf '%s\n' old-publisher
EOF
cat > /tmp/old-configure-auth <<'EOF'
#!/usr/bin/bash
if [[ -n "${LEGAL_MCP_FIXTURE_READY_FIFO:-}" ]]; then
  printf '%s\n' ready > "$LEGAL_MCP_FIXTURE_READY_FIFO"
  exec 9<>/run/lock/legal-mcp-host-transaction.lock
  flock -x 9
  touch /tmp/old-configure-auth-resumed
fi
printf '%s\n' old-configure-auth
EOF
cat > /tmp/old-update-image <<'EOF'
#!/usr/bin/bash
if [[ -n "${LEGAL_MCP_FIXTURE_READY_FIFO:-}" ]]; then
  printf '%s\n' ready > "$LEGAL_MCP_FIXTURE_READY_FIFO"
  exec 9<>/run/lock/legal-mcp-host-transaction.lock
  flock -x 9
  touch /tmp/old-update-image-resumed
fi
printf '%s\n' old-update-image
EOF
printf '%s\n' '__IMAGE_DIGEST__' > /tmp/old-container-template
cat > /tmp/old-sudoers <<'EOF'
Defaults:legal-mcp-publisher !requiretty
legal-mcp-publisher ALL=(root) NOPASSWD: /usr/local/sbin/legal-mcp-host-deploy prepare *, /usr/local/sbin/legal-mcp-host-deploy activate *
EOF
cat > /tmp/old-host-tools-marker <<EOF
LEGAL_MCP_HOST_TOOLS_V1
VERSION=0.19.2
SOURCE_COMMIT=2222222222222222222222222222222222222222
HOST_DEPLOY_SHA256=$(sha256sum /tmp/old-host-deploy | awk '{print $1}')
PUBLISHER_COMMAND_SHA256=$(sha256sum /tmp/old-publisher | awk '{print $1}')
SUDOERS_SHA256=$(sha256sum /tmp/old-sudoers | awk '{print $1}')
EOF
chmod 755 /tmp/old-host-deploy /tmp/old-publisher /tmp/old-configure-auth \
  /tmp/old-update-image
chmod 644 /tmp/old-container-template
chmod 440 /tmp/old-sudoers
chmod 444 /tmp/old-host-tools-marker
visudo -cf /tmp/old-sudoers >/dev/null

/usr/bin/mv /usr/bin/install /usr/bin/install.fixture-real
/usr/bin/mv /usr/bin/chown /usr/bin/chown.fixture-real
/usr/bin/mv /usr/bin/chmod /usr/bin/chmod.fixture-real
/usr/bin/mv /usr/bin/find /usr/bin/find.fixture-real
/usr/bin/mv /usr/bin/rm /usr/bin/rm.fixture-real
/usr/bin/mv /usr/bin/sync /usr/bin/sync.fixture-real
/usr/bin/mv /usr/bin/mv /usr/bin/mv.fixture-real
cat > /usr/bin/install <<'EOF'
#!/usr/bin/bash
if [[ "$*" == *'/bundle/scripts/legal-mcp-'* \
  || "$*" == *'/bundle/infra/hosting/'* ]]; then
  if flock -n /run/lock/legal-mcp-host-transaction.lock -c true; then
    echo 'host-tool install ran without the shared lock' >&2
    exit 95
  fi
fi
if [[ -e /tmp/fail-host-tool-install && "$*" == *'/bundle/scripts/legal-mcp-host-deploy'* ]]; then
  exit 88
fi
if [[ -e /tmp/fail-host-tool-restore \
  && "$*" == *'/.publisher-sudoers-deny.'* ]]; then
  exit 89
fi
/usr/bin/install.fixture-real "$@"
status=$?
[[ $status -eq 0 && -s /tmp/kill-host-tool-after ]] || exit "$status"
point="$(</tmp/kill-host-tool-after)"
target="${!#}"
matched=false
case "$point:$target" in
  build-directory:/etc/legal-mcp/.host-tools-transaction.building) matched=true ;;
  build-host-deploy:/etc/legal-mcp/.host-tools-transaction.building/host-deploy) matched=true ;;
  build-publisher:/etc/legal-mcp/.host-tools-transaction.building/publisher-command) matched=true ;;
  build-configure-auth:/etc/legal-mcp/.host-tools-transaction.building/configure-auth) matched=true ;;
  build-update-image:/etc/legal-mcp/.host-tools-transaction.building/update-image) matched=true ;;
  build-container-template:/etc/legal-mcp/.host-tools-transaction.building/container-template) matched=true ;;
  build-sudoers:/etc/legal-mcp/.host-tools-transaction.building/publisher-sudoers) matched=true ;;
  build-marker:/etc/legal-mcp/.host-tools-transaction.building/marker-was-absent) matched=true ;;
  build-previous-manifest:/etc/legal-mcp/.host-tools-transaction.building/previous-sha256) matched=true ;;
  build-target-manifest:/etc/legal-mcp/.host-tools-transaction.building/target-sha256) matched=true ;;
esac
if [[ "$matched" = true ]]; then
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
exit 0
EOF
cat > /usr/bin/chown <<'EOF'
#!/usr/bin/bash
/usr/bin/chown.fixture-real "$@"
status=$?
if [[ $status -eq 0 && -s /tmp/kill-host-tool-after \
  && "$(</tmp/kill-host-tool-after)" = build-directory-owner \
  && "${!#}" = /etc/legal-mcp/.host-tools-transaction.building ]]; then
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
exit "$status"
EOF
cat > /usr/bin/chmod <<'EOF'
#!/usr/bin/bash
point=''
if [[ -s /tmp/kill-host-tool-after ]]; then point="$(</tmp/kill-host-tool-after)"; fi
if [[ "$point" = build-metadata-written \
  && "$*" == *'/etc/legal-mcp/.host-tools-transaction.building/kind'* ]]; then
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
/usr/bin/chmod.fixture-real "$@"
status=$?
[[ $status -eq 0 ]] || exit "$status"
target="${!#}"
if { [[ "$point" = build-directory-mode \
      && "$target" = /etc/legal-mcp/.host-tools-transaction.building ]] \
    || [[ "$point" = build-metadata-mode \
      && "$target" = /etc/legal-mcp/.host-tools-transaction.building/target-revision ]]; }; then
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
exit 0
EOF
cat > /usr/bin/find <<'EOF'
#!/usr/bin/bash
if [[ -e /tmp/fail-find ]]; then exit 87; fi
exec /usr/bin/find.fixture-real "$@"
EOF
cat > /usr/bin/mv <<'EOF'
#!/usr/bin/bash
/usr/bin/mv.fixture-real "$@"
status=$?
[[ $status -eq 0 && -s /tmp/kill-host-tool-after ]] || exit "$status"
point="$(</tmp/kill-host-tool-after)"
target="${!#}"
if [[ "$point:$target" = pause-after-deploy:/usr/local/sbin/legal-mcp-host-deploy ]]; then
  printf '%s\n' ready > /tmp/migration-installer-ready
  read -r release < /tmp/migration-installer-release
  [[ "$release" = release ]] || exit 96
fi
matched=false
case "$point:$target" in
  preparation-published:/etc/legal-mcp/.host-tools-transaction.preparing) matched=true ;;
  transaction-prepared:/etc/legal-mcp/.host-tools-transaction) matched=true ;;
  publisher-installed:/usr/local/sbin/legal-mcp-publisher-command) matched=true ;;
  deploy-installed:/usr/local/sbin/legal-mcp-host-deploy) matched=true ;;
  launcher-installed:/usr/local/libexec/legal-mcp/host-tool-launcher) matched=true ;;
  configure-launcher-installed:/usr/local/sbin/legal-mcp-configure-auth)
    cmp --silent /usr/local/libexec/legal-mcp/host-tool-launcher "$target" && matched=true
    ;;
  update-launcher-installed:/usr/local/sbin/legal-mcp-update-image)
    cmp --silent /usr/local/libexec/legal-mcp/host-tool-launcher "$target" && matched=true
    ;;
  container-template-installed:/usr/local/libexec/legal-mcp/legal-mcp.container.template)
    cmp --silent /tmp/new-container-template "$target" && matched=true
    ;;
  rendered-quadlet-installed:/etc/containers/systemd/legal-mcp.container) matched=true ;;
  configure-pointer-installed:/etc/legal-mcp/configure-auth-implementation) matched=true ;;
  update-pointer-installed:/etc/legal-mcp/update-image-implementation) matched=true ;;
  launcher-marker-installed:/etc/legal-mcp/host-tool-launcher) matched=true ;;
  configure-auth-restored:/usr/local/sbin/legal-mcp-configure-auth)
    cmp --silent /tmp/old-configure-auth "$target" && matched=true
    ;;
  update-image-restored:/usr/local/sbin/legal-mcp-update-image)
    cmp --silent /tmp/old-update-image "$target" && matched=true
    ;;
  container-template-restored:/usr/local/libexec/legal-mcp/legal-mcp.container.template)
    cmp --silent /tmp/old-container-template "$target" && matched=true
    ;;
  rendered-quadlet-restored:/etc/containers/systemd/legal-mcp.container)
    cmp --silent /tmp/expected-old-rendered "$target" && matched=true
    ;;
  marker-installed:/etc/legal-mcp/host-tools) matched=true ;;
  transaction-retiring:/etc/legal-mcp/.host-tools-transaction.retiring) matched=true ;;
  transaction-retired:/etc/legal-mcp/.host-tools-transaction.retired) matched=true ;;
  rollback-retiring:/etc/legal-mcp/.host-tools-transaction.rollback-retiring) matched=true ;;
  rollback-retired:/etc/legal-mcp/.host-tools-transaction.rollback-retired) matched=true ;;
  publisher-restore:/etc/legal-mcp/.host-tools-transaction.publisher-restore) matched=true ;;
  publisher-restore-retired:/etc/legal-mcp/.host-tools-transaction.publisher-restore-retired) matched=true ;;
  publisher-restored:/usr/local/sbin/legal-mcp-publisher-command)
    cmp --silent /tmp/old-publisher "$target" && matched=true
    ;;
  publisher-locked:/etc/sudoers.d/legal-mcp-publisher)
    [[ "$(wc -l < "$target")" -eq 1 ]] && matched=true
    ;;
  policy-installed:/etc/sudoers.d/legal-mcp-publisher)
    [[ "$(wc -l < "$target")" -eq 2 ]] && matched=true
    ;;
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
if [[ -f /tmp/kill-host-tool-after ]]; then point="$(</tmp/kill-host-tool-after)"; fi
if { [[ "$point" = building-delete \
      && "$*" == *'/etc/legal-mcp/.host-tools-transaction.building-retired'* ]] \
    || [[ "$point" = preparation-delete \
      && "$*" == *'/etc/legal-mcp/.host-tools-transaction.preparing-retired'* ]] \
    || [[ "$point" = transaction-delete \
      && "$*" == *'/etc/legal-mcp/.host-tools-transaction.retired'* ]] \
    || [[ "$point" = rollback-delete \
      && "$*" == *'/etc/legal-mcp/.host-tools-transaction.publisher-restore-retired'* ]]; }; then
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
if [[ $status -eq 0 && -s /tmp/kill-host-tool-after \
  && "$(</tmp/kill-host-tool-after)" = build-synced \
  && "$*" = '-f /etc/legal-mcp/.host-tools-transaction.building' ]]; then
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
exit "$status"
EOF
/usr/bin/chmod.fixture-real 755 /usr/bin/install /usr/bin/chown /usr/bin/chmod /usr/bin/find \
  /usr/bin/mv /usr/bin/rm /usr/bin/sync
real_install=/usr/bin/install.fixture-real
real_rm=/usr/bin/rm.fixture-real
expected_host_state=prepared
expected_old_marker=absent

write_prepared_state() {
  expected_host_state=prepared
  "$real_rm" -rf -- /srv/legal-mcp/generations/* /srv/legal-mcp/state/* \
    /srv/legal-mcp/uploads/* "$journal" \
    /srv/legal-mcp/lifecycle/.deployment-transaction.preparing \
    /srv/legal-mcp/lifecycle/active-generation "$authorization"
  "$real_install" -d -o legal-mcp-publisher -g legal-mcp-publisher -m 0700 "$upload"
  printf '%s\n' partial-v19 > "$upload/partial"
  chown legal-mcp-publisher:legal-mcp-publisher "$upload/partial"
  chmod 600 "$upload/partial"
  printf '%s\n-\nprepared\n' "$generation" > "$journal"
  chown root:root "$journal"
  chmod 600 "$journal"
  printf '%s\n' "$generation" > "$authorization"
  chown root:legal-mcp-publisher "$authorization"
  chmod 440 "$authorization"
}

write_activated_dark_state() {
  expected_host_state=activated
  "$real_rm" -rf -- /srv/legal-mcp/generations/* /srv/legal-mcp/state/* \
    /srv/legal-mcp/uploads/* "$journal" \
    /srv/legal-mcp/lifecycle/.deployment-transaction.preparing "$authorization"
  "$real_install" -d -o root -g legal-mcp -m 0750 \
    "/srv/legal-mcp/generations/$generation"
  printf '%s' "$generation" > /srv/legal-mcp/lifecycle/active-generation
  chown root:root /srv/legal-mcp/lifecycle/active-generation
  chmod 644 /srv/legal-mcp/lifecycle/active-generation
  "$real_install" -o root -g root -m 0444 /tmp/old-host-tools-marker \
    /etc/legal-mcp/host-tools
  expected_old_marker=present
}

reset_old_state() {
  "$real_rm" -rf -- "$transaction" "$building" "$building_retired" \
    "$preparing" "$preparing_retired" "$retiring" "$retired" \
    "$rollback_retiring" "$rollback_retired" "$publisher_restore" \
    "$publisher_restore_retired" \
    /etc/legal-mcp/host-tools \
    "$host_tool_launcher" "$launcher_marker" "$configure_pointer" \
    "$update_pointer" "$implementation_dir" "$auth_ready" \
    /run/legal-mcp/host-tool-launcher-dispatch \
    /run/legal-mcp/host-tool-launcher-dispatch.retiring \
    /run/legal-mcp/host-tool-launcher-dispatch.retired \
    /run/legal-mcp/auth-configuring \
    /etc/legal-mcp/.publisher-sudoers-* /etc/legal-mcp/.host-tools-new.* \
    /etc/legal-mcp/.host-tools-target.* /etc/legal-mcp/.auth-transaction \
    /etc/legal-mcp/.auth-transaction.preparing \
    /etc/legal-mcp/.auth-transaction.preparing-retired \
    /etc/legal-mcp/.auth-transaction.preparing.* \
    /etc/legal-mcp/.image-transaction /etc/legal-mcp/.image-transaction.preparing \
    /etc/legal-mcp/.image-transaction.preparing-retired \
    /etc/legal-mcp/.image-transaction.retiring /etc/legal-mcp/.image-transaction.retired
  "$real_rm" -f -- "$lifecycle_lock" /tmp/LIFECYCLE_LOCK.hardlink
  "$real_install" -o root -g root -m 0640 /dev/null "$lifecycle_lock"
  "$real_install" -o root -g root -m 0755 /tmp/old-host-deploy "$host_deploy"
  "$real_install" -o root -g root -m 0755 /tmp/old-publisher "$publisher"
  "$real_install" -o root -g root -m 0755 /tmp/old-configure-auth "$configure_auth"
  "$real_install" -o root -g root -m 0755 /tmp/old-update-image "$update_image"
  "$real_install" -o root -g root -m 0644 /tmp/old-container-template "$container_template"
  sed 's|__IMAGE_DIGEST__|ghcr.io/gunba/australian-legal-mcp@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa|' \
    /tmp/old-container-template > "$rendered_quadlet"
  cp "$rendered_quadlet" /tmp/expected-old-rendered
  chmod 644 "$rendered_quadlet"
  "$real_install" -o root -g root -m 0440 /tmp/old-sudoers "$sudoers"
  "$real_install" -o root -g root -m 0755 /fixture-input/install-host.sh "$installer"
  "$real_install" -o root -g root -m 0755 /fixture-input/legal-mcp-host-deploy \
    "$bundle/scripts/legal-mcp-host-deploy"
  "$real_install" -o root -g root -m 0755 /fixture-input/legal-mcp-publisher-command \
    "$bundle/scripts/legal-mcp-publisher-command"
  "$real_install" -o root -g root -m 0755 /tmp/new-configure-auth \
    "$bundle/infra/hosting/configure-auth.sh"
  "$real_install" -o root -g root -m 0755 /tmp/new-update-image \
    "$bundle/infra/hosting/update-image.sh"
  "$real_install" -o root -g root -m 0644 /tmp/new-container-template \
    "$bundle/infra/hosting/legal-mcp.container.template"
  "$real_install" -o root -g root -m 0644 /fixture-input/Containerfile "$bundle/Containerfile"
  chmod 755 "$installer" "$bundle/legal-mcp"
  printf '%s\n' "$revision" > "$bundle/SOURCE_COMMIT"
  chmod 644 "$bundle/SOURCE_COMMIT"
  cat > /etc/legal-mcp/runtime.env <<'EOF'
LEGAL_MCP_HTTP_AUTH=disabled
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
EOF
  chown root:root /etc/legal-mcp/runtime.env
  chmod 600 /etc/legal-mcp/runtime.env
  printf '{"keys":[],"version":1}\n' > /etc/legal-mcp/api-keys.json
  chown legal-mcp:legal-mcp /etc/legal-mcp/api-keys.json
  chmod 400 /etc/legal-mcp/api-keys.json
  chmod 444 /etc/legal-mcp/host-installed
  "$real_rm" -f /tmp/fail-host-tool-install /tmp/fail-host-tool-restore \
    /tmp/fail-find /tmp/fail-ss /tmp/fail-systemctl-* /tmp/fail-ufw-* \
    /tmp/service-active /tmp/service-wrong-enablement /tmp/caddy-active \
    /tmp/caddy-enabled /tmp/ufw-web-open /tmp/kill-host-tool-after \
    /tmp/web-listener /tmp/wrong-release-binary \
    /tmp/old-configure-auth-resumed /tmp/old-update-image-resumed \
    /tmp/migration-installer-ready /tmp/migration-installer-release \
    /tmp/migration-configure-ready /tmp/migration-image-ready
  expected_old_marker=absent
  write_prepared_state
}

assert_host_state() {
  if [[ "$expected_host_state" = prepared ]]; then
    [[ -d "$upload" && -f "$journal" && -f "$authorization" \
      && ! -e /srv/legal-mcp/lifecycle/active-generation ]]
  else
    [[ -f /srv/legal-mcp/lifecycle/active-generation \
      && "$(</srv/legal-mcp/lifecycle/active-generation)" = "$generation" \
      && -d "/srv/legal-mcp/generations/$generation" \
      && ! -e "$journal" && ! -e "$authorization" ]]
    [[ -z "$(find /srv/legal-mcp/uploads -mindepth 1 -maxdepth 1 -print -quit)" ]]
  fi
}

assert_old_tools() {
  cmp --silent /tmp/old-host-deploy "$host_deploy"
  cmp --silent /tmp/old-publisher "$publisher"
  cmp --silent /tmp/old-configure-auth "$configure_auth"
  cmp --silent /tmp/old-update-image "$update_image"
  cmp --silent /tmp/old-container-template "$container_template"
  sed 's|__IMAGE_DIGEST__|ghcr.io/gunba/australian-legal-mcp@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa|' \
    /tmp/old-container-template > /tmp/expected-old-rendered
  cmp --silent /tmp/expected-old-rendered "$rendered_quadlet"
  cmp --silent /tmp/old-sudoers "$sudoers"
  [[ "$(stat -c '%U:%G:%a' "$configure_auth")" = root:root:755 \
    && "$(stat -c '%U:%G:%a' "$update_image")" = root:root:755 \
    && "$(stat -c '%U:%G:%a' "$container_template")" = root:root:644 ]]
  if [[ "$expected_old_marker" = present ]]; then
    cmp --silent /tmp/old-host-tools-marker /etc/legal-mcp/host-tools
  else
    [[ ! -e /etc/legal-mcp/host-tools ]]
  fi
  [[ ! -e "$host_tool_launcher" && ! -e "$launcher_marker" \
    && ! -e "$configure_pointer" && ! -e "$update_pointer" \
    && ! -e "$auth_ready" ]]
  [[ ! -e "$transaction" \
    && ! -e "$building" && ! -e "$building_retired" \
    && ! -e "$preparing" && ! -e "$preparing_retired" \
    && ! -e "$retiring" && ! -e "$retired" \
    && ! -e "$rollback_retiring" && ! -e "$rollback_retired" \
    && ! -e "$publisher_restore" && ! -e "$publisher_restore_retired" ]]
  assert_host_state
}

assert_new_tools() {
  cmp --silent "$bundle/scripts/legal-mcp-host-deploy" "$host_deploy"
  cmp --silent "$bundle/scripts/legal-mcp-publisher-command" "$publisher"
  cmp --silent "$host_tool_launcher" "$configure_auth"
  cmp --silent "$host_tool_launcher" "$update_image"
  cmp --silent "$bundle/infra/hosting/legal-mcp.container.template" "$container_template"
  sed 's|__IMAGE_DIGEST__|ghcr.io/gunba/australian-legal-mcp@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa|' \
    "$bundle/infra/hosting/legal-mcp.container.template" > /tmp/expected-new-rendered
  cmp --silent /tmp/expected-new-rendered "$rendered_quadlet"
  [[ "$(stat -c '%U:%G:%a' "$configure_auth")" = root:root:755 \
    && "$(stat -c '%U:%G:%a' "$update_image")" = root:root:755 \
    && "$(stat -c '%U:%G:%a' "$container_template")" = root:root:644 ]]
  [[ "$(stat -c '%U:%G:%a:%h:%s' "$configure_pointer")" = root:root:644:1:64 \
    && "$(stat -c '%U:%G:%a:%h:%s' "$update_pointer")" = root:root:644:1:64 ]]
  configure_sha="$(<"$configure_pointer")"
  update_sha="$(<"$update_pointer")"
  [[ "$configure_sha" = "$(sha256sum "$bundle/infra/hosting/configure-auth.sh" | awk '{print $1}')" \
    && "$update_sha" = "$(sha256sum "$bundle/infra/hosting/update-image.sh" | awk '{print $1}')" ]]
  cmp --silent "$bundle/infra/hosting/configure-auth.sh" \
    "$implementation_dir/configure-auth.$configure_sha"
  cmp --silent "$bundle/infra/hosting/update-image.sh" \
    "$implementation_dir/update-image.$update_sha"
  [[ "$(stat -c '%U:%G:%a:%h' "$implementation_dir/configure-auth.$configure_sha")" = root:root:755:1 \
    && "$(stat -c '%U:%G:%a:%h' "$implementation_dir/update-image.$update_sha")" = root:root:755:1 ]]
  grep -Fxq 'LEGAL_MCP_HOST_TOOL_LAUNCHER_V1' "$launcher_marker"
  [[ ! -e "$auth_ready" ]]
  grep -Fxq 'LEGAL_MCP_HOST_TOOLS_V2' /etc/legal-mcp/host-tools
  grep -Fxq "VERSION=$version" /etc/legal-mcp/host-tools
  grep -Fxq "SOURCE_COMMIT=$revision" /etc/legal-mcp/host-tools
  [[ ! -e "$transaction" && ! -e "$building" && ! -e "$building_retired" \
    && ! -e "$preparing" && ! -e "$preparing_retired" \
    && ! -e "$retiring" && ! -e "$retired" \
    && ! -e "$rollback_retiring" && ! -e "$rollback_retired" \
    && ! -e "$publisher_restore" && ! -e "$publisher_restore_retired" ]]
  assert_host_state
  visudo -cf "$sudoers" >/dev/null
}

expect_upgrade_failed() {
  if "$installer" --upgrade-host-tools --version "$version" \
    >/tmp/host-tool.stdout 2>/tmp/host-tool.stderr; then
    echo 'unsafe host-tool upgrade was unexpectedly accepted' >&2
    exit 1
  fi
}

write_expected_hash_manifest() {
  local deploy_file="$1" publisher_file="$2" auth_file="$3" image_file="$4"
  local template_file="$5" rendered_file="$6" sudoers_file="$7" marker_file="$8"
  local launcher_file="$9" launcher_marker_file="${10}" auth_pointer_file="${11}"
  local image_pointer_file="${12}" destination="${13}"
  file_hash() { if [[ "$1" = - ]]; then printf '%s\n' -; else sha256sum "$1" | awk '{print $1}'; fi; }
  cat > "$destination" <<EOF
HOST_DEPLOY_SHA256=$(file_hash "$deploy_file")
PUBLISHER_COMMAND_SHA256=$(file_hash "$publisher_file")
CONFIGURE_AUTH_ENTRYPOINT_SHA256=$(file_hash "$auth_file")
UPDATE_IMAGE_ENTRYPOINT_SHA256=$(file_hash "$image_file")
CONTAINER_TEMPLATE_SHA256=$(file_hash "$template_file")
RENDERED_QUADLET_SHA256=$(file_hash "$rendered_file")
SUDOERS_SHA256=$(file_hash "$sudoers_file")
HOST_TOOLS_MARKER_SHA256=$(file_hash "$marker_file")
LAUNCHER_SHA256=$(file_hash "$launcher_file")
LAUNCHER_MARKER_SHA256=$(file_hash "$launcher_marker_file")
CONFIGURE_AUTH_POINTER_SHA256=$(file_hash "$auth_pointer_file")
UPDATE_IMAGE_POINTER_SHA256=$(file_hash "$image_pointer_file")
EOF
}

write_expected_v2_target_files() {
  local deploy_sha publisher_sha auth_sha image_sha template_sha sudoers_sha
  deploy_sha="$(sha256sum "$bundle/scripts/legal-mcp-host-deploy" | awk '{print $1}')"
  publisher_sha="$(sha256sum "$bundle/scripts/legal-mcp-publisher-command" | awk '{print $1}')"
  auth_sha="$(sha256sum "$bundle/infra/hosting/configure-auth.sh" | awk '{print $1}')"
  image_sha="$(sha256sum "$bundle/infra/hosting/update-image.sh" | awk '{print $1}')"
  template_sha="$(sha256sum "$bundle/infra/hosting/legal-mcp.container.template" | awk '{print $1}')"
  cat > /tmp/expected-new-sudoers <<EOF
Defaults:legal-mcp-publisher !requiretty
legal-mcp-publisher ALL=(root) NOPASSWD: sha256:$deploy_sha $host_deploy ^prepare [0-9a-f]{64}$, sha256:$deploy_sha $host_deploy ^activate [0-9a-f]{64}$, sha256:$deploy_sha $host_deploy ^abort [0-9a-f]{64}$
EOF
  chmod 440 /tmp/expected-new-sudoers
  sudoers_sha="$(sha256sum /tmp/expected-new-sudoers | awk '{print $1}')"
  cat > /tmp/expected-new-marker <<EOF
LEGAL_MCP_HOST_TOOLS_V2
VERSION=$version
SOURCE_COMMIT=$revision
HOST_DEPLOY_SHA256=$deploy_sha
PUBLISHER_COMMAND_SHA256=$publisher_sha
CONFIGURE_AUTH_SHA256=$auth_sha
UPDATE_IMAGE_SHA256=$image_sha
CONTAINER_TEMPLATE_SHA256=$template_sha
SUDOERS_SHA256=$sudoers_sha
EOF
  chmod 444 /tmp/expected-new-marker
  sed 's|__IMAGE_DIGEST__|ghcr.io/gunba/australian-legal-mcp@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa|' \
    "$bundle/infra/hosting/legal-mcp.container.template" > /tmp/expected-new-rendered
}

assert_v2_transaction() {
  local path="$1" marker_state="${2:-absent}" previous_marker=- name
  [[ "$(stat -c '%U:%G:%a' "$path")" = root:root:700 ]]
  for name in kind target-version target-revision target-sha256 previous-sha256 \
    host-deploy publisher-command configure-auth update-image container-template \
    rendered-quadlet publisher-sudoers configure-auth-device-inode \
    update-image-device-inode; do
    [[ "$(stat -c '%U:%G:%a:%h' "$path/$name")" = root:root:600:1 ]]
  done
  [[ "$(<"$path/kind")" = LEGAL_MCP_HOST_TOOLS_TRANSACTION_V2 \
    && "$(<"$path/target-version")" = "$version" \
    && "$(<"$path/target-revision")" = "$revision" ]]
  cmp --silent /tmp/old-host-deploy "$path/host-deploy"
  cmp --silent /tmp/old-publisher "$path/publisher-command"
  cmp --silent /tmp/old-configure-auth "$path/configure-auth"
  cmp --silent /tmp/old-update-image "$path/update-image"
  cmp --silent /tmp/old-container-template "$path/container-template"
  cmp --silent /tmp/expected-old-rendered "$path/rendered-quadlet"
  cmp --silent /tmp/old-sudoers "$path/publisher-sudoers"
  [[ -f "$path/launcher-was-absent" \
    && "$(<"$path/configure-auth-device-inode")" =~ ^[0-9]+:[0-9]+$ \
    && "$(<"$path/update-image-device-inode")" =~ ^[0-9]+:[0-9]+$ ]]
  if [[ "$marker_state" = present ]]; then
    [[ -f "$path/marker-was-present" && ! -e "$path/marker-was-absent" \
      && "$(stat -c '%U:%G:%a:%h' "$path/host-tools-marker")" = root:root:444:1 ]]
    cmp --silent /tmp/old-host-tools-marker "$path/host-tools-marker"
    previous_marker=/tmp/old-host-tools-marker
  else
    [[ -f "$path/marker-was-absent" && ! -e "$path/marker-was-present" ]]
  fi
  write_expected_v2_target_files
  grep -Fxq "HOST_DEPLOY_SHA256=$(sha256sum "$bundle/scripts/legal-mcp-host-deploy" | awk '{print $1}')" \
    "$path/target-sha256"
  grep -Fxq "PUBLISHER_COMMAND_SHA256=$(sha256sum "$bundle/scripts/legal-mcp-publisher-command" | awk '{print $1}')" \
    "$path/target-sha256"
  grep -Fxq "CONTAINER_TEMPLATE_SHA256=$(sha256sum "$bundle/infra/hosting/legal-mcp.container.template" | awk '{print $1}')" \
    "$path/target-sha256"
  grep -Fxq "RENDERED_QUADLET_SHA256=$(sha256sum /tmp/expected-new-rendered | awk '{print $1}')" \
    "$path/target-sha256"
  launcher_sha="$(awk -F= '$1 == "LAUNCHER_SHA256" {print $2}' "$path/target-sha256")"
  [[ "$launcher_sha" =~ ^[0-9a-f]{64}$ ]]
  grep -Fxq "CONFIGURE_AUTH_ENTRYPOINT_SHA256=$launcher_sha" "$path/target-sha256"
  grep -Fxq "UPDATE_IMAGE_ENTRYPOINT_SHA256=$launcher_sha" "$path/target-sha256"
  write_expected_hash_manifest /tmp/old-host-deploy /tmp/old-publisher \
    /tmp/old-configure-auth /tmp/old-update-image /tmp/old-container-template \
    /tmp/expected-old-rendered /tmp/old-sudoers "$previous_marker" \
    - - - - /tmp/expected-previous-sha256
  cmp --silent /tmp/expected-previous-sha256 "$path/previous-sha256"
}

reset_old_state
expect_upgrade_failed_version=false
if "$installer" --upgrade-host-tools --version 0.19.2 \
  >/tmp/host-tool.stdout 2>/tmp/host-tool.stderr; then
  expect_upgrade_failed_version=true
fi
[[ "$expect_upgrade_failed_version" = false ]]
assert_old_tools

reset_old_state
printf '%s\n' malformed > "$bundle/SOURCE_COMMIT"
expect_upgrade_failed
assert_old_tools

reset_old_state
touch /tmp/wrong-release-binary
expect_upgrade_failed
assert_old_tools

# Every v0.19.6 release asset is mandatory, has its exact executable/data
# mode, and must be a single safe file from the version-matched bundle.
for release_asset in \
  "$bundle/infra/hosting/configure-auth.sh" \
  "$bundle/infra/hosting/update-image.sh" \
  "$bundle/infra/hosting/legal-mcp.container.template"; do
  reset_old_state
  chmod 777 "$release_asset"
  expect_upgrade_failed
  assert_old_tools
done

reset_old_state
"$real_rm" -f "$bundle/infra/hosting/configure-auth.sh"
expect_upgrade_failed
assert_old_tools

reset_old_state
ln "$bundle/infra/hosting/update-image.sh" /tmp/release-hardlink
expect_upgrade_failed
"$real_rm" -f /tmp/release-hardlink
assert_old_tools

reset_old_state
chown legal-mcp:legal-mcp "$bundle/infra/hosting/legal-mcp.container.template"
expect_upgrade_failed
assert_old_tools

# The V2 transaction may change both template and rendered Quadlet, but never
# accepts a release template that removes the strict auth-ready boot gate.
reset_old_state
printf 'changed=1\n__IMAGE_DIGEST__\n' \
  > "$bundle/infra/hosting/legal-mcp.container.template"
chmod 644 "$bundle/infra/hosting/legal-mcp.container.template"
expect_upgrade_failed
assert_old_tools

# Installed host-tool identities and modes are equally exact.
for installed_asset in "$configure_auth" "$update_image" "$container_template"; do
  reset_old_state
  chmod 700 "$installed_asset"
  expect_upgrade_failed
  chmod "$(if [[ "$installed_asset" = "$container_template" ]]; then printf 644; else printf 755; fi)" \
    "$installed_asset"
  assert_old_tools
done

reset_old_state
printf '%s\n-\nactivating\n' "$generation" > "$journal"
chmod 600 "$journal"
expect_upgrade_failed
cmp --silent /tmp/old-host-deploy "$host_deploy"
[[ -d "$upload" && -f "$journal" ]]

reset_old_state
printf '%064d\n' 2 > "$authorization"
expect_upgrade_failed
assert_old_tools

# LIFECYCLE_LOCK was durably created by the failed v0.19.0 activation before
# generation validation. V0.19.6 treats that exact empty root-owned file as
# installed state, while rejecting every unsafe identity and representation.
reset_old_state
"$real_rm" -f -- "$lifecycle_lock"
expect_upgrade_failed
assert_old_tools

reset_old_state
"$real_rm" -f -- "$lifecycle_lock"
ln -s /dev/null "$lifecycle_lock"
expect_upgrade_failed
assert_old_tools

reset_old_state
ln "$lifecycle_lock" /tmp/LIFECYCLE_LOCK.hardlink
expect_upgrade_failed
assert_old_tools

reset_old_state
printf 'unexpected content\n' > "$lifecycle_lock"
expect_upgrade_failed
assert_old_tools

reset_old_state
chown root:legal-mcp "$lifecycle_lock"
expect_upgrade_failed
assert_old_tools

reset_old_state
chmod 600 "$lifecycle_lock"
expect_upgrade_failed
assert_old_tools

reset_old_state
setfacl --modify user:legal-mcp-publisher:r-- "$lifecycle_lock"
chmod 640 "$lifecycle_lock"
expect_upgrade_failed
assert_old_tools

reset_old_state
chmod 777 "$bundle/scripts/legal-mcp-publisher-command"
expect_upgrade_failed
assert_old_tools

reset_old_state
chmod 640 /etc/legal-mcp/host-installed
expect_upgrade_failed
cmp --silent /tmp/old-host-deploy "$host_deploy"
chmod 444 /etc/legal-mcp/host-installed

# Failed socket and directory probes are errors, never evidence of absence.
reset_old_state
touch /tmp/fail-ss
expect_upgrade_failed
"$real_rm" -f /tmp/fail-ss
assert_old_tools

reset_old_state
touch /tmp/fail-find
expect_upgrade_failed
"$real_rm" -f /tmp/fail-find
assert_old_tools

# Exact generated/inactive Quadlet and disabled/inactive Caddy are accepted,
# while every systemd and UFW probe or deletion error fails closed.
for failure in fail-systemctl-enabled fail-systemctl-active fail-ufw-status; do
  reset_old_state
  touch "/tmp/$failure"
  expect_upgrade_failed
  "$real_rm" -f "/tmp/$failure"
  assert_old_tools
done

reset_old_state
touch /tmp/caddy-enabled /tmp/caddy-active /tmp/ufw-web-open /tmp/fail-ufw-delete
expect_upgrade_failed
[[ -e /tmp/ufw-web-open ]]
"$real_rm" -f /tmp/caddy-enabled /tmp/caddy-active /tmp/ufw-web-open /tmp/fail-ufw-delete
assert_old_tools

# An activated host is eligible only while it is completely dark and has no
# corpus, auth, or image transaction. Unsafe exposure is closed where
# possible, but the caller must rerun after observing the rejection.
reset_old_state
write_activated_dark_state
touch /tmp/service-active
expect_upgrade_failed
[[ ! -e /tmp/service-active ]]
assert_old_tools

reset_old_state
write_activated_dark_state
touch /tmp/caddy-enabled /tmp/caddy-active
expect_upgrade_failed
[[ ! -e /tmp/caddy-enabled && ! -e /tmp/caddy-active ]]
assert_old_tools

reset_old_state
write_activated_dark_state
touch /tmp/ufw-web-open
expect_upgrade_failed
[[ ! -e /tmp/ufw-web-open ]]
assert_old_tools

reset_old_state
write_activated_dark_state
touch /tmp/web-listener
expect_upgrade_failed
"$real_rm" -f /tmp/web-listener
assert_old_tools

reset_old_state
write_activated_dark_state
sed -i 's/LEGAL_MCP_HTTP_AUTH=disabled/LEGAL_MCP_HTTP_AUTH=api-key/' \
  /etc/legal-mcp/runtime.env
expect_upgrade_failed
sed -i 's/LEGAL_MCP_HTTP_AUTH=api-key/LEGAL_MCP_HTTP_AUTH=disabled/' \
  /etc/legal-mcp/runtime.env
assert_old_tools

reset_old_state
write_activated_dark_state
printf '{"keys":[{"id":"old","sha256":"%064d"}],"version":1}\n' 0 \
  > /etc/legal-mcp/api-keys.json
expect_upgrade_failed
printf '{"keys":[],"version":1}\n' > /etc/legal-mcp/api-keys.json
assert_old_tools

# Activated pointers are one canonical shared read contract: exact 64 bytes,
# root:root, mode 0644, one link, and no newline.
reset_old_state
write_activated_dark_state
printf '%s\n' "$generation" > /srv/legal-mcp/lifecycle/active-generation
expect_upgrade_failed
printf '%s' "$generation" > /srv/legal-mcp/lifecycle/active-generation
assert_old_tools

reset_old_state
write_activated_dark_state
chmod 600 /srv/legal-mcp/lifecycle/active-generation
expect_upgrade_failed
chmod 644 /srv/legal-mcp/lifecycle/active-generation
assert_old_tools

reset_old_state
write_activated_dark_state
sed -i 's/VERSION=0.19.2/VERSION=0.19.1/' /etc/legal-mcp/host-tools
expect_upgrade_failed
"$real_install" -o root -g root -m 0444 /tmp/old-host-tools-marker \
  /etc/legal-mcp/host-tools
assert_old_tools

reset_old_state
write_activated_dark_state
printf '%s\n' changed-old-deploy > "$host_deploy"
chmod 755 "$host_deploy"
expect_upgrade_failed
"$real_install" -o root -g root -m 0755 /tmp/old-host-deploy "$host_deploy"
assert_old_tools

for corpus_transaction in \
  /srv/legal-mcp/lifecycle/.deployment-transaction \
  /srv/legal-mcp/lifecycle/.deployment-transaction.preparing; do
  reset_old_state
  write_activated_dark_state
  printf '%s\n' invalid > "$corpus_transaction"
  chown root:root "$corpus_transaction"
  chmod 600 "$corpus_transaction"
  expect_upgrade_failed
  "$real_rm" -f "$corpus_transaction"
  assert_old_tools
done

for host_transaction in \
  /etc/legal-mcp/.auth-transaction \
  /etc/legal-mcp/.auth-transaction.preparing \
  /etc/legal-mcp/.auth-transaction.preparing-retired \
  /etc/legal-mcp/.auth-transaction.preparing.123 \
  /etc/legal-mcp/.image-transaction \
  /etc/legal-mcp/.image-transaction.preparing \
  /etc/legal-mcp/.image-transaction.preparing-retired \
  /etc/legal-mcp/.image-transaction.retiring \
  /etc/legal-mcp/.image-transaction.retired; do
  reset_old_state
  write_activated_dark_state
  "$real_install" -d -o root -g root -m 0700 "$host_transaction"
  expect_upgrade_failed
  "$real_rm" -rf "$host_transaction"
  assert_old_tools
done

# The one-time migration closes the public old paths before releasing the host
# lock, then terminates root Bash processes that already had the exact retired
# v0.19.2 helper inodes open while blocked on that lock. They can never resume
# old code after the V2 marker commits.
reset_old_state
mkfifo /tmp/migration-installer-ready /tmp/migration-installer-release \
  /tmp/migration-configure-ready /tmp/migration-image-ready
printf '%s\n' pause-after-deploy > /tmp/kill-host-tool-after
"$installer" --upgrade-host-tools --version "$version" \
  >/tmp/migration-upgrade.stdout 2>/tmp/migration-upgrade.stderr &
migration_upgrade_pid=$!
read -r migration_ready < /tmp/migration-installer-ready
[[ "$migration_ready" = ready ]]
LEGAL_MCP_FIXTURE_READY_FIFO=/tmp/migration-configure-ready \
  "$configure_auth" >/tmp/migration-configure.stdout 2>/tmp/migration-configure.stderr &
old_configure_pid=$!
read -r migration_ready < /tmp/migration-configure-ready
[[ "$migration_ready" = ready ]]
LEGAL_MCP_FIXTURE_READY_FIFO=/tmp/migration-image-ready \
  "$update_image" >/tmp/migration-image.stdout 2>/tmp/migration-image.stderr &
old_image_pid=$!
read -r migration_ready < /tmp/migration-image-ready
[[ "$migration_ready" = ready ]]
printf '%s\n' release > /tmp/migration-installer-release
wait "$migration_upgrade_pid"
set +e
wait "$old_configure_pid"
old_configure_status=$?
wait "$old_image_pid"
old_image_status=$?
set -e
"$real_rm" -f /tmp/kill-host-tool-after /tmp/migration-*-ready \
  /tmp/migration-installer-release
[[ $old_configure_status -ne 0 && $old_image_status -ne 0 \
  && ! -e /tmp/old-configure-auth-resumed \
  && ! -e /tmp/old-update-image-resumed ]]
assert_new_tools

# A normal mid-install error automatically restores every old file and the
# absence of the old host-tools marker while preserving the prepared upload.
reset_old_state
touch /tmp/fail-host-tool-install
expect_upgrade_failed
"$real_rm" -f /tmp/fail-host-tool-install
assert_old_tools

# A failed rollback command cannot be ignored under an errexit-disabled
# conditional. The complete canonical transaction remains and a later recovery
# from the same bundle restores the old tools.
reset_old_state
touch /tmp/fail-host-tool-install /tmp/fail-host-tool-restore
expect_upgrade_failed
"$real_rm" -f /tmp/fail-host-tool-install /tmp/fail-host-tool-restore
[[ -d "$transaction" && -d "$upload" && -f "$journal" && -f "$authorization" ]]
"$installer" --recover-host-tools --version "$version" >/tmp/host-tool-recover.stdout
grep -Fxq 'interrupted host-tool upgrade rolled back' /tmp/host-tool-recover.stdout
assert_old_tools

kill_upgrade_at() {
  local point="$1" reset="${2:-reset}" status
  if [[ "$reset" = reset ]]; then
    reset_old_state
  fi
  printf '%s\n' "$point" > /tmp/kill-host-tool-after
  set +e
  "$installer" --upgrade-host-tools --version "$version" \
    >/tmp/host-tool-kill.stdout 2>/tmp/host-tool-kill.stderr
  status=$?
  set -e
  "$real_rm" -f /tmp/kill-host-tool-after
  [[ $status -ne 0 ]]
}

# SIGKILL after every construction operation leaves only a deletion-only build
# directory. Recovery never validates its partial contents; it atomically
# retires and removes them before any live host-tool mutation.
for point in build-directory build-directory-owner build-directory-mode \
  build-host-deploy build-publisher build-configure-auth build-update-image \
  build-container-template build-sudoers build-metadata-written \
  build-metadata-mode build-marker build-previous-manifest \
  build-target-manifest build-synced; do
  kill_upgrade_at "$point"
  [[ -d "$building" && ! -e "$preparing" && ! -e "$transaction" ]]
  cmp --silent /tmp/old-publisher "$publisher"
  "$installer" --recover-host-tools --version "$version" \
    >/tmp/host-tool-recover.stdout
  grep -Fxq 'interrupted host-tool preparation rolled back' \
    /tmp/host-tool-recover.stdout
  assert_old_tools
done

# Only the complete, synced journal is atomically published as preparation.
# A kill before the first live wrapper replacement makes that complete state
# discardable through its own deletion-only retirement name.
kill_upgrade_at preparation-published
[[ -d "$preparing" && ! -e "$building" && ! -e "$transaction" ]]
assert_v2_transaction "$preparing"
cmp --silent /tmp/old-publisher "$publisher"
"$installer" --recover-host-tools --version "$version" \
  >/tmp/host-tool-recover.stdout
grep -Fxq 'interrupted host-tool preparation rolled back' \
  /tmp/host-tool-recover.stdout
assert_old_tools

# A pre-V2 marker is rollback data, not an alternate current contract. Its
# exact bytes are hashed into the previous manifest and restored on recovery.
reset_old_state
"$real_install" -o root -g root -m 0444 /tmp/old-host-tools-marker \
  /etc/legal-mcp/host-tools
expected_old_marker=present
kill_upgrade_at transaction-prepared preserve
assert_v2_transaction "$transaction" present
"$installer" --recover-host-tools --version "$version" \
  >/tmp/host-tool-recover.stdout
grep -Fxq 'interrupted host-tool upgrade rolled back' /tmp/host-tool-recover.stdout
assert_old_tools

# Recursive deletion is attempted only after an atomic rename to a
# deletion-only state. Even a kill after deleting one journal member is safely
# resumed without validating the deliberately partial retired directory.
kill_upgrade_at build-host-deploy
printf '%s\n' building-delete > /tmp/kill-host-tool-after
set +e
"$installer" --recover-host-tools --version "$version" \
  >/tmp/host-tool-delete-kill.stdout 2>/tmp/host-tool-delete-kill.stderr
delete_kill_status=$?
set -e
"$real_rm" -f /tmp/kill-host-tool-after
[[ $delete_kill_status -ne 0 && -d "$building_retired" ]]
"$installer" --recover-host-tools --version "$version" \
  >/tmp/host-tool-recover.stdout
assert_old_tools

kill_upgrade_at preparation-published
printf '%s\n' preparation-delete > /tmp/kill-host-tool-after
set +e
"$installer" --recover-host-tools --version "$version" \
  >/tmp/host-tool-delete-kill.stdout 2>/tmp/host-tool-delete-kill.stderr
delete_kill_status=$?
set -e
"$real_rm" -f /tmp/kill-host-tool-after
[[ $delete_kill_status -ne 0 && -d "$preparing_retired" ]]
"$installer" --recover-host-tools --version "$version" \
  >/tmp/host-tool-recover.stdout
assert_old_tools

# Every durable pre-commit mutation point is SIGKILL recoverable without phase
# files or temporaries inside the exact-whitelisted transaction directory.
for point in transaction-prepared publisher-locked deploy-installed \
  container-template-installed rendered-quadlet-installed launcher-installed \
  configure-launcher-installed update-launcher-installed configure-pointer-installed \
  update-pointer-installed launcher-marker-installed marker-installed; do
  kill_upgrade_at "$point"
  [[ -d "$transaction" && -d "$upload" && -f "$journal" && -f "$authorization" ]]
  assert_v2_transaction "$transaction"
  "$installer" --recover-host-tools --version "$version" \
    >/tmp/host-tool-recover.stdout
  grep -Fxq 'interrupted host-tool upgrade rolled back' /tmp/host-tool-recover.stdout
  assert_old_tools
done

# The new forced-command wrapper is installed before the preparation directory
# becomes canonical. If SIGKILL lands in that exact window, the wrapper rejects
# deploy and rsync against the deterministic preparation name, and recovery
# atomically canonicalizes then rolls back the saved old tools.
kill_upgrade_at publisher-installed
[[ -d "$preparing" && ! -e "$transaction" ]]
for command in \
  "prepare $generation" \
  "activate $generation" \
  "abort $generation" \
  "rsync --server -vlogDtpre.iLsfxCIvu . $generation/"; do
  if runuser -u legal-mcp-publisher -- \
    env SSH_ORIGINAL_COMMAND="$command" "$publisher" \
    >/tmp/foreign.stdout 2>/tmp/foreign.stderr; then
    echo "pending host-tool preparation unexpectedly allowed: $command" >&2
    exit 1
  fi
  grep -Fq 'foreign host transaction' /tmp/foreign.stderr
  [[ -d "$upload" && -f "$journal" && -f "$authorization" ]]
done
"$installer" --recover-host-tools --version "$version" \
  >/tmp/host-tool-recover.stdout
grep -Fxq 'interrupted host-tool preparation rolled back' \
  /tmp/host-tool-recover.stdout
assert_old_tools

# Kill exactly after the new digest-pinned sudo policy is installed. All four
# publisher surfaces remain denied by the durable host-tool transaction until
# recovery, and none can alter the prepared v19 upload or authorization.
kill_upgrade_at policy-installed
[[ -d "$transaction" && "$(wc -l < "$sudoers")" -eq 2 ]]
for command in \
  "prepare $generation" \
  "activate $generation" \
  "abort $generation" \
  "rsync --server -vlogDtpre.iLsfxCIvu . $generation/"; do
  if runuser -u legal-mcp-publisher -- \
    env SSH_ORIGINAL_COMMAND="$command" "$publisher" \
    >/tmp/foreign.stdout 2>/tmp/foreign.stderr; then
    echo "pending host-tool transaction unexpectedly allowed: $command" >&2
    exit 1
  fi
  grep -Eq 'foreign host transaction|host transaction must be recovered' \
    /tmp/foreign.stderr
  [[ -d "$upload" && -f "$journal" && -f "$authorization" ]]
done
"$installer" --recover-host-tools --version "$version" \
  >/tmp/host-tool-recover.stdout
assert_old_tools

# Recovery itself is restartable at every retirement boundary. The new wrapper
# remains installed and rejects publisher actions until all old tool state is
# durable; it is restored last behind a publisher-restore sentinel.
for point in rollback-retiring rollback-retired publisher-restore \
  configure-auth-restored update-image-restored container-template-restored \
  rendered-quadlet-restored \
  publisher-restored publisher-restore-retired rollback-delete; do
  kill_upgrade_at policy-installed
  printf '%s\n' "$point" > /tmp/kill-host-tool-after
  set +e
  "$installer" --recover-host-tools --version "$version" \
    >/tmp/host-tool-recovery-kill.stdout 2>/tmp/host-tool-recovery-kill.stderr
  recovery_kill_status=$?
  set -e
  "$real_rm" -f /tmp/kill-host-tool-after
  [[ $recovery_kill_status -ne 0 ]]
  if [[ "$point" = rollback-retiring || "$point" = rollback-retired \
    || "$point" = publisher-restore ]]; then
    if runuser -u legal-mcp-publisher -- \
      env SSH_ORIGINAL_COMMAND="abort $generation" "$publisher" \
      >/tmp/foreign.stdout 2>/tmp/foreign.stderr; then
      echo "incomplete host-tool recovery unexpectedly allowed abort: $point" >&2
      exit 1
    fi
    grep -Fq 'foreign host transaction' /tmp/foreign.stderr
  fi
  "$installer" --recover-host-tools --version "$version" \
    >/tmp/host-tool-retire.stdout
  grep -Fxq 'interrupted host-tool transaction retirement completed' \
    /tmp/host-tool-retire.stdout
  assert_old_tools
done

# Once verification has completed, transaction retirement itself has two
# atomic names. SIGKILL before either parent sync or before deletion is resumed
# without rolling back the already-validated tool set.
for point in transaction-retiring transaction-retired transaction-delete; do
  kill_upgrade_at "$point"
  [[ -d "$retiring" || -d "$retired" ]]
  "$installer" --recover-host-tools --version "$version" \
    >/tmp/host-tool-retire.stdout
  grep -Fxq 'interrupted host-tool transaction retirement completed' \
    /tmp/host-tool-retire.stdout
  assert_new_tools
done

# A committed retirement is deletion-only only after the exact live target is
# re-bound to the supplied bundle. Changed live bytes leave the retirement in
# place and are never accepted merely because the canonical journal vanished.
kill_upgrade_at transaction-retired
printf '%s\n' changed-live-deploy > "$host_deploy"
chmod 755 "$host_deploy"
if "$installer" --recover-host-tools --version "$version" \
  >/tmp/changed-live-retirement.stdout 2>/tmp/changed-live-retirement.stderr; then
  echo 'changed live host tools were accepted during committed retirement' >&2
  exit 1
fi
[[ -d "$retired" ]]

expect_recovery_failed() {
  if "$installer" --recover-host-tools --version "$version" \
    >/tmp/wrong-transaction.stdout 2>/tmp/wrong-transaction.stderr; then
    echo 'inexact host-tool transaction was unexpectedly recovered' >&2
    exit 1
  fi
  [[ -d "$transaction" ]]
}

# Recovery has one hard-cut V2 identity. V1 journals, release identity drift,
# noncanonical file modes, target-manifest drift, and any changed rollback
# byte are rejected rather than interpreted or upgraded in place.
kill_upgrade_at transaction-prepared
printf '%s\n' LEGAL_MCP_HOST_TOOLS_TRANSACTION_V1 > "$transaction/kind"
expect_recovery_failed

kill_upgrade_at transaction-prepared
printf '%s\n' 0.19.2 > "$transaction/target-version"
expect_recovery_failed

kill_upgrade_at transaction-prepared
printf '%040d\n' 2 > "$transaction/target-revision"
expect_recovery_failed

kill_upgrade_at transaction-prepared
chmod 640 "$transaction/kind"
expect_recovery_failed

kill_upgrade_at transaction-prepared
printf 'HOST_DEPLOY_SHA256=%064d\n' 0 > "$transaction/target-sha256"
expect_recovery_failed

kill_upgrade_at transaction-prepared
printf 'HOST_DEPLOY_SHA256=%064d\n' 0 > "$transaction/previous-sha256"
expect_recovery_failed

kill_upgrade_at transaction-prepared
printf '%s\n' altered-rollback-auth > "$transaction/configure-auth"
chmod 600 "$transaction/configure-auth"
expect_recovery_failed

# A failed activation has already revoked rsync authorization. Host-tool repair
# accepts that closed state, preserves it, and leaves the prepared candidate and
# journal untouched.
reset_old_state
"$real_rm" -f -- "$authorization"
output="$("$installer" --upgrade-host-tools --version "$version")"
[[ "$output" = "host tools upgraded to $version ($revision); service and ingress remain off" ]]
cmp --silent "$bundle/scripts/legal-mcp-host-deploy" "$host_deploy"
cmp --silent "$bundle/scripts/legal-mcp-publisher-command" "$publisher"
cmp --silent "$host_tool_launcher" "$configure_auth"
cmp --silent "$host_tool_launcher" "$update_image"
cmp --silent "$bundle/infra/hosting/legal-mcp.container.template" "$container_template"
[[ -d "$upload" && -f "$journal" && ! -e "$authorization" \
  && ! -e "$transaction" ]]
write_expected_v2_target_files
cmp --silent /tmp/expected-new-marker /etc/legal-mcp/host-tools
visudo -cf "$sudoers" >/dev/null

# The successful activated-dark path has no upload, corpus transaction, or
# authorization to preserve. It installs the same exact V2 file set while
# leaving service and ingress off.
reset_old_state
write_activated_dark_state
output="$("$installer" --upgrade-host-tools --version "$version")"
[[ "$output" = "host tools upgraded to $version ($revision); service and ingress remain off" ]]
assert_new_tools
write_expected_v2_target_files
cmp --silent /tmp/expected-new-marker /etc/legal-mcp/host-tools
launcher_identity="$(stat -c '%d:%i' "$host_tool_launcher")"
configure_identity="$(stat -c '%d:%i' "$configure_auth")"
update_identity="$(stat -c '%d:%i' "$update_image")"

# The generated Quadlet condition accepts only an exact empty root marker (or
# the launcher's live, PID/start-time-bound transient configuration permit).
if "$host_tool_launcher" --check-auth-ready; then
  echo 'auth-ready check accepted an absent marker' >&2
  exit 1
fi
"$real_install" -o root -g root -m 0444 /dev/null "$auth_ready"
"$host_tool_launcher" --check-auth-ready
printf x > "$auth_ready"
chmod 444 "$auth_ready"
if "$host_tool_launcher" --check-auth-ready; then
  echo 'auth-ready check accepted a nonempty marker' >&2
  exit 1
fi
"$real_rm" -f "$auth_ready"
ln -s /dev/null "$auth_ready"
if "$host_tool_launcher" --check-auth-ready; then
  echo 'auth-ready check accepted a symlink' >&2
  exit 1
fi
"$real_rm" -f "$auth_ready"
output="$("$installer" --upgrade-host-tools --version "$version")"
[[ "$output" = "host tools already match $version ($revision)" ]]
assert_new_tools
[[ "$(stat -c '%d:%i' "$host_tool_launcher")" = "$launcher_identity" \
  && "$(stat -c '%d:%i' "$configure_auth")" = "$configure_identity" \
  && "$(stat -c '%d:%i' "$update_image")" = "$update_identity" ]]

# A later implementation cutover changes only exact 64-byte pointers and adds
# a new content-addressed implementation. The three launcher inodes are never
# replaced, and the old immutable implementation remains available to any
# already selected invocation.
old_update_sha="$(<"$update_pointer")"
cat > "$bundle/infra/hosting/update-image.sh" <<'EOF'
#!/usr/bin/bash
printf '%s\n' new-update-image-second-bundle
EOF
chmod 755 "$bundle/infra/hosting/update-image.sh"
output="$("$installer" --upgrade-host-tools --version "$version")"
[[ "$output" = "host tools upgraded to $version ($revision); service and ingress remain off" ]]
assert_new_tools
new_update_sha="$(<"$update_pointer")"
[[ "$new_update_sha" != "$old_update_sha" \
  && -f "$implementation_dir/update-image.$old_update_sha" \
  && -f "$implementation_dir/update-image.$new_update_sha" \
  && "$(stat -c '%d:%i' "$host_tool_launcher")" = "$launcher_identity" \
  && "$(stat -c '%d:%i' "$configure_auth")" = "$configure_identity" \
  && "$(stat -c '%d:%i' "$update_image")" = "$update_identity" ]]
cp /etc/legal-mcp/host-tools /tmp/stable-host-tools-before-recovery
cat > "$bundle/infra/hosting/update-image.sh" <<'EOF'
#!/usr/bin/bash
printf '%s\n' interrupted-third-update-image
EOF
chmod 755 "$bundle/infra/hosting/update-image.sh"
kill_upgrade_at update-pointer-installed preserve
[[ -d "$transaction" ]]
"$installer" --recover-host-tools --version "$version" \
  >/tmp/stable-pointer-recovery.stdout
grep -Fxq 'interrupted host-tool upgrade rolled back' \
  /tmp/stable-pointer-recovery.stdout
[[ "$(<"$update_pointer")" = "$new_update_sha" \
  && "$(stat -c '%d:%i' "$host_tool_launcher")" = "$launcher_identity" \
  && "$(stat -c '%d:%i' "$configure_auth")" = "$configure_identity" \
  && "$(stat -c '%d:%i' "$update_image")" = "$update_identity" ]]
cmp --silent /tmp/stable-host-tools-before-recovery /etc/legal-mcp/host-tools

# Execute the stable launcher in the unprivileged fixture with deterministic
# mount-namespace adapters. The mock implementations prove a second contender
# cannot acquire the real host lock at any point during implementation work.
/usr/bin/mv.fixture-real /usr/bin/unshare /usr/bin/unshare.fixture-real
/usr/bin/mv.fixture-real /usr/bin/mount /usr/bin/mount.fixture-real
cat > /usr/bin/unshare <<'EOF'
#!/usr/bin/bash
[[ "$1" = --mount && "$2" = --propagation && "$3" = private && "$4" = -- ]] || exit 97
shift 4
exec "$@"
EOF
cat > /usr/bin/mount <<'EOF'
#!/usr/bin/bash
if [[ "$1" = --bind && $# -eq 3 ]]; then
  owner="$(stat -c '%U' "$2")"
  group="$(stat -c '%G' "$2")"
  mode="$(stat -c '%a' "$2")"
  exec /usr/bin/install.fixture-real -o "$owner" -g "$group" -m "$mode" "$2" "$3"
fi
if [[ "$1" = -o && "$2" = remount,bind,ro,nodev,nosuid && $# -eq 3 ]]; then
  exit 0
fi
exit 98
EOF
/usr/bin/chmod.fixture-real 755 /usr/bin/unshare /usr/bin/mount

reset_old_state
write_activated_dark_state
cat > "$bundle/infra/hosting/configure-auth.sh" <<'EOF'
#!/usr/bin/bash
set -euo pipefail
[[ "${LEGAL_MCP_HOST_TRANSACTION_LOCK_FD:-}" = 9 && -e /proc/$$/fd/9 ]]
if flock -n /run/lock/legal-mcp-host-transaction.lock -c true; then
  echo 'configure implementation ran without the launcher lock' >&2
  exit 91
fi
/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready
case "${1:-}" in
  --prepare-auth-dispatch)
    [[ $# -eq 1 ]]
    install -d -o root -g root -m 0700 /etc/legal-mcp/.auth-transaction
    printf '%s\n' prepared > /etc/legal-mcp/.auth-transaction/kind
    chmod 600 /etc/legal-mcp/.auth-transaction/kind
    exit 0
    ;;
  --finalize-auth-ready)
    [[ $# -eq 1 && -f /etc/legal-mcp/auth-ready ]]
    rm -rf /etc/legal-mcp/.auth-transaction
    exit 0
    ;;
esac
cat > /etc/legal-mcp/runtime.env <<'ENV'
LEGAL_MCP_HTTP_AUTH=entra
LEGAL_MCP_API_KEYS_FILE=/run/secrets/legal-mcp-api-keys.json
LEGAL_MCP_EXTERNAL_URL=https://legal.example.com/mcp
LEGAL_MCP_ALLOWED_ORIGINS=https://legal.example.com
LEGAL_MCP_HTTP_WORKERS=4
LEGAL_MCP_SHUTDOWN_GRACE_SECONDS=30
ENV
chown root:root /etc/legal-mcp/runtime.env
chmod 600 /etc/legal-mcp/runtime.env
printf '{"keys":[],"version":1}\n' > /etc/legal-mcp/api-keys.json
chown legal-mcp:legal-mcp /etc/legal-mcp/api-keys.json
chmod 400 /etc/legal-mcp/api-keys.json
touch /tmp/service-active /tmp/caddy-active /tmp/caddy-enabled /tmp/ufw-web-open
printf '%s\n' configure-lock-held
EOF
chmod 755 "$bundle/infra/hosting/configure-auth.sh"
"$installer" --upgrade-host-tools --version "$version" >/tmp/launcher-lock-upgrade.stdout
[[ "$("$configure_auth")" = configure-lock-held ]]
[[ "$(stat -c '%U:%G:%a:%h:%s' "$auth_ready")" = root:root:444:1:0 ]]

# Exactly `--recover` may successfully restore the disabled baseline without
# publishing auth-ready, provided every service, listener, and firewall
# postcondition is still dark.
reset_old_state
write_activated_dark_state
cat > "$bundle/infra/hosting/configure-auth.sh" <<'EOF'
#!/usr/bin/bash
set -euo pipefail
[[ $# -eq 1 && "$1" = --recover \
  && "${LEGAL_MCP_HOST_TRANSACTION_LOCK_FD:-}" = 9 && -e /proc/$$/fd/9 ]]
if flock -n /run/lock/legal-mcp-host-transaction.lock -c true; then exit 91; fi
/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready
printf '%s\n' disabled-recovery-complete
EOF
chmod 755 "$bundle/infra/hosting/configure-auth.sh"
"$installer" --upgrade-host-tools --version "$version" >/tmp/disabled-recovery-upgrade.stdout
[[ "$("$configure_auth" --recover)" = disabled-recovery-complete ]]
[[ ! -e "$auth_ready" && ! -e /run/legal-mcp/auth-configuring \
  && ! -e /tmp/service-active && ! -e /tmp/caddy-active \
  && ! -e /tmp/caddy-enabled && ! -e /tmp/ufw-web-open ]]
if "$host_tool_launcher" --check-auth-ready; then
  echo 'disabled recovery unexpectedly published auth-ready' >&2
  exit 1
fi

# No argument superset is treated as the disabled recovery exception.
reset_old_state
write_activated_dark_state
cat > "$bundle/infra/hosting/configure-auth.sh" <<'EOF'
#!/usr/bin/bash
set -euo pipefail
[[ "${LEGAL_MCP_HOST_TRANSACTION_LOCK_FD:-}" = 9 && -e /proc/$$/fd/9 ]]
if flock -n /run/lock/legal-mcp-host-transaction.lock -c true; then exit 91; fi
exit 0
EOF
chmod 755 "$bundle/infra/hosting/configure-auth.sh"
"$installer" --upgrade-host-tools --version "$version" >/tmp/inexact-recovery-upgrade.stdout
if "$configure_auth" --recover extra \
  >/tmp/inexact-recovery.stdout 2>/tmp/inexact-recovery.stderr; then
  echo 'inexact disabled recovery arguments were unexpectedly accepted' >&2
  exit 1
fi
[[ ! -e "$auth_ready" && ! -e /run/legal-mcp/auth-configuring \
  && ! -e /tmp/service-active && ! -e /tmp/caddy-active \
  && ! -e /tmp/caddy-enabled && ! -e /tmp/ufw-web-open ]]

# The updater receives the same inherited, still-locked fd 9 contract as the
# authentication implementation.
reset_old_state
write_activated_dark_state
cat > "$bundle/infra/hosting/update-image.sh" <<'EOF'
#!/usr/bin/bash
set -euo pipefail
[[ "${LEGAL_MCP_HOST_TRANSACTION_LOCK_FD:-}" = 9 && -e /proc/$$/fd/9 ]]
if flock -n /run/lock/legal-mcp-host-transaction.lock -c true; then
  echo 'real launcher lock was not retained during image implementation' >&2
  exit 92
fi
printf '%s\n' update-lock-held
EOF
chmod 755 "$bundle/infra/hosting/update-image.sh"
"$installer" --upgrade-host-tools --version "$version" >/tmp/update-lock-upgrade.stdout
[[ "$("$update_image")" = update-lock-held ]]

/usr/bin/rm.fixture-real -f /usr/bin/unshare /usr/bin/mount
/usr/bin/mv.fixture-real /usr/bin/unshare.fixture-real /usr/bin/unshare
/usr/bin/mv.fixture-real /usr/bin/mount.fixture-real /usr/bin/mount

# The successful prepared-bootstrap authorized-state upgrade is also exact and
# enables a separately invoked forced-command abort under the digest/regex sudo
# policy.
reset_old_state
output="$("$installer" --upgrade-host-tools --version "$version")"
[[ "$output" = "host tools upgraded to $version ($revision); service and ingress remain off" ]]
assert_new_tools
write_expected_v2_target_files
cmp --silent /tmp/expected-new-marker /etc/legal-mcp/host-tools
visudo -cf "$sudoers" >/dev/null
if runuser -u legal-mcp-publisher -- sudo -n "$host_deploy" abort 1 \
  >/tmp/unsafe-sudo.stdout 2>/tmp/unsafe-sudo.stderr; then
  echo 'sandboxed sudo policy accepted a malformed generation' >&2
  exit 1
fi
abort_output="$(runuser -u legal-mcp-publisher -- \
  env SSH_ORIGINAL_COMMAND="abort $generation" "$publisher")"
[[ "$abort_output" = aborted ]]
[[ ! -e "$upload" && ! -e "$journal" && ! -e "$authorization" ]]

# Unexpected durable transaction content is never guessed during recovery.
reset_old_state
kill_upgrade_at transaction-prepared
printf '%s\n' unknown > "$transaction/unexpected"
chmod 600 "$transaction/unexpected"
if "$installer" --recover-host-tools --version "$version" \
  >/tmp/wrong-transaction.stdout 2>/tmp/wrong-transaction.stderr; then
  echo 'malformed host-tool transaction was unexpectedly recovered' >&2
  exit 1
fi
[[ -d "$transaction" && -d "$upload" && -f "$journal" ]]

echo host-tool-upgrade-fixture-ok
