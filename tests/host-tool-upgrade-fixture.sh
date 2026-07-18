#!/usr/bin/env bash
# Exercise the version-matched publisher-tool upgrade and its recovery on an
# installed, empty, SSH-only host with one prepared bootstrap upload.
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

version=0.19.1
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
upload=/srv/legal-mcp/uploads/$generation
authorization=/run/legal-mcp/authorized-upload
host_deploy=/usr/local/sbin/legal-mcp-host-deploy
publisher=/usr/local/sbin/legal-mcp-publisher-command
sudoers=/etc/sudoers.d/legal-mcp-publisher

for command_name in flock getfacl groupadd mknod python3 setfacl sudo useradd visudo; do
  command -v "$command_name" >/dev/null || {
    echo "fixture dependency is missing: $command_name" >&2
    exit 2
  }
done

install -d -o root -g root -m 0755 \
  "$bundle/infra/linode" "$bundle/scripts"
install -o root -g root -m 0755 /fixture-input/install-host.sh "$installer"
install -o root -g root -m 0755 /fixture-input/legal-mcp-host-deploy \
  "$bundle/scripts/legal-mcp-host-deploy"
install -o root -g root -m 0755 /fixture-input/legal-mcp-publisher-command \
  "$bundle/scripts/legal-mcp-publisher-command"
install -o root -g root -m 0644 /fixture-input/Containerfile "$bundle/Containerfile"
printf '%s\n' "$revision" > "$bundle/SOURCE_COMMIT"
chmod 644 "$bundle/SOURCE_COMMIT"
cat > "$bundle/legal-mcp" <<'EOF'
#!/usr/bin/bash
if [[ "$1" = --version ]]; then
  if [[ -e /tmp/wrong-release-binary ]]; then
    printf '%s\n' 'legal-mcp 9.9.9'
  else
    printf '%s\n' 'legal-mcp 0.19.1'
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
printf '%s\n' '__IMAGE_DIGEST__' > /usr/local/libexec/legal-mcp/legal-mcp.container.template
chmod 644 /usr/local/libexec/legal-mcp/legal-mcp.container.template
sed 's|__IMAGE_DIGEST__|ghcr.io/gunba/australian-legal-mcp@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa|' \
  /usr/local/libexec/legal-mcp/legal-mcp.container.template \
  > /etc/containers/systemd/legal-mcp.container
chmod 644 /etc/containers/systemd/legal-mcp.container
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
cat > /tmp/old-sudoers <<'EOF'
Defaults:legal-mcp-publisher !requiretty
legal-mcp-publisher ALL=(root) NOPASSWD: /usr/local/sbin/legal-mcp-host-deploy prepare *, /usr/local/sbin/legal-mcp-host-deploy activate *
EOF
chmod 755 /tmp/old-host-deploy /tmp/old-publisher
chmod 440 /tmp/old-sudoers
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
if [[ "$*" == *'/bundle/scripts/legal-mcp-'* ]]; then
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
  build-sudoers:/etc/legal-mcp/.host-tools-transaction.building/publisher-sudoers) matched=true ;;
  build-marker:/etc/legal-mcp/.host-tools-transaction.building/marker-was-absent) matched=true ;;
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
matched=false
case "$point:$target" in
  preparation-published:/etc/legal-mcp/.host-tools-transaction.preparing) matched=true ;;
  transaction-prepared:/etc/legal-mcp/.host-tools-transaction) matched=true ;;
  publisher-installed:/usr/local/sbin/legal-mcp-publisher-command) matched=true ;;
  deploy-installed:/usr/local/sbin/legal-mcp-host-deploy) matched=true ;;
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

write_prepared_state() {
  "$real_rm" -rf -- "$upload" "$journal" "$authorization"
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

reset_old_state() {
  "$real_rm" -rf -- "$transaction" "$building" "$building_retired" \
    "$preparing" "$preparing_retired" "$retiring" "$retired" \
    "$rollback_retiring" "$rollback_retired" "$publisher_restore" \
    "$publisher_restore_retired" \
    /etc/legal-mcp/host-tools \
    /etc/legal-mcp/.publisher-sudoers-* /etc/legal-mcp/.host-tools-new.*
  "$real_install" -o root -g root -m 0755 /tmp/old-host-deploy "$host_deploy"
  "$real_install" -o root -g root -m 0755 /tmp/old-publisher "$publisher"
  "$real_install" -o root -g root -m 0440 /tmp/old-sudoers "$sudoers"
  chmod 755 "$bundle/scripts/legal-mcp-host-deploy" \
    "$bundle/scripts/legal-mcp-publisher-command" "$installer" "$bundle/legal-mcp"
  printf '%s\n' "$revision" > "$bundle/SOURCE_COMMIT"
  chmod 644 "$bundle/SOURCE_COMMIT"
  chmod 444 /etc/legal-mcp/host-installed
  "$real_rm" -f /tmp/fail-host-tool-install /tmp/fail-host-tool-restore \
    /tmp/fail-find /tmp/fail-ss /tmp/fail-systemctl-* /tmp/fail-ufw-* \
    /tmp/service-active /tmp/service-wrong-enablement /tmp/caddy-active \
    /tmp/caddy-enabled /tmp/ufw-web-open /tmp/kill-host-tool-after \
    /tmp/wrong-release-binary
  write_prepared_state
}

assert_old_tools() {
  cmp --silent /tmp/old-host-deploy "$host_deploy"
  cmp --silent /tmp/old-publisher "$publisher"
  cmp --silent /tmp/old-sudoers "$sudoers"
  [[ ! -e /etc/legal-mcp/host-tools && ! -e "$transaction" \
    && ! -e "$building" && ! -e "$building_retired" \
    && ! -e "$preparing" && ! -e "$preparing_retired" \
    && ! -e "$retiring" && ! -e "$retired" \
    && ! -e "$rollback_retiring" && ! -e "$rollback_retired" \
    && ! -e "$publisher_restore" && ! -e "$publisher_restore_retired" ]]
  [[ -d "$upload" && -f "$journal" && -f "$authorization" ]]
}

assert_new_tools() {
  cmp --silent "$bundle/scripts/legal-mcp-host-deploy" "$host_deploy"
  cmp --silent "$bundle/scripts/legal-mcp-publisher-command" "$publisher"
  grep -Fxq "VERSION=$version" /etc/legal-mcp/host-tools
  grep -Fxq "SOURCE_COMMIT=$revision" /etc/legal-mcp/host-tools
  [[ ! -e "$transaction" && ! -e "$building" && ! -e "$building_retired" \
    && ! -e "$preparing" && ! -e "$preparing_retired" \
    && ! -e "$retiring" && ! -e "$retired" \
    && ! -e "$rollback_retiring" && ! -e "$rollback_retired" \
    && ! -e "$publisher_restore" && ! -e "$publisher_restore_retired" ]]
  [[ -d "$upload" && -f "$journal" && -f "$authorization" ]]
  visudo -cf "$sudoers" >/dev/null
}

expect_upgrade_failed() {
  if "$installer" --upgrade-host-tools --version "$version" \
    >/tmp/host-tool.stdout 2>/tmp/host-tool.stderr; then
    echo 'unsafe host-tool upgrade was unexpectedly accepted' >&2
    exit 1
  fi
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

# A normal mid-install error automatically restores all three old files and the
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
  local point="$1" status
  reset_old_state
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
  build-host-deploy build-publisher build-sudoers build-metadata-written \
  build-metadata-mode build-marker build-synced; do
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
cmp --silent /tmp/old-publisher "$publisher"
"$installer" --recover-host-tools --version "$version" \
  >/tmp/host-tool-recover.stdout
grep -Fxq 'interrupted host-tool preparation rolled back' \
  /tmp/host-tool-recover.stdout
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
for point in transaction-prepared publisher-locked deploy-installed marker-installed; do
  kill_upgrade_at "$point"
  [[ -d "$transaction" && -d "$upload" && -f "$journal" && -f "$authorization" ]]
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

# A failed activation has already revoked rsync authorization. Host-tool repair
# accepts that closed state, preserves it, and leaves the prepared candidate and
# journal untouched.
reset_old_state
"$real_rm" -f -- "$authorization"
output="$("$installer" --upgrade-host-tools --version "$version")"
[[ "$output" = "host publisher tools upgraded to $version ($revision)" ]]
cmp --silent "$bundle/scripts/legal-mcp-host-deploy" "$host_deploy"
cmp --silent "$bundle/scripts/legal-mcp-publisher-command" "$publisher"
[[ -d "$upload" && -f "$journal" && ! -e "$authorization" \
  && ! -e "$transaction" ]]
visudo -cf "$sudoers" >/dev/null

# The successful authorized-state upgrade is also exact and enables a
# separately invoked forced-command abort under the digest/regex sudo policy.
reset_old_state
output="$("$installer" --upgrade-host-tools --version "$version")"
[[ "$output" = "host publisher tools upgraded to $version ($revision)" ]]
cmp --silent "$bundle/scripts/legal-mcp-host-deploy" "$host_deploy"
cmp --silent "$bundle/scripts/legal-mcp-publisher-command" "$publisher"
[[ -d "$upload" && -f "$journal" && -f "$authorization" && ! -e "$transaction" ]]
deploy_sha="$(sha256sum "$host_deploy" | awk '{print $1}')"
publisher_sha="$(sha256sum "$publisher" | awk '{print $1}')"
sudoers_sha="$(sha256sum "$sudoers" | awk '{print $1}')"
cat > /tmp/expected-host-tools <<EOF
LEGAL_MCP_HOST_TOOLS_V1
VERSION=$version
SOURCE_COMMIT=$revision
HOST_DEPLOY_SHA256=$deploy_sha
PUBLISHER_COMMAND_SHA256=$publisher_sha
SUDOERS_SHA256=$sudoers_sha
EOF
cmp --silent /tmp/expected-host-tools /etc/legal-mcp/host-tools
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
