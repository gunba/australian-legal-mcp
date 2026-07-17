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
EOF
  exit 2
}

[[ $EUID -eq 0 ]] || { echo 'run this installer as root' >&2; exit 2; }
[[ ! -e /etc/legal-mcp/host-installed ]] || {
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
for path in \
  "$REPO_DIR/Containerfile" \
  "$REPO_DIR/SOURCE_COMMIT" \
  "$REPO_DIR/infra/hosting/Caddyfile" \
  "$REPO_DIR/infra/hosting/configure-auth.sh" \
  "$REPO_DIR/infra/hosting/update-image.sh" \
  "$REPO_DIR/infra/hosting/legal-mcp.container.template" \
  "$REPO_DIR/scripts/legal-mcp-host-deploy" \
  "$REPO_DIR/scripts/legal-mcp-publisher-command"; do
  [[ -f "$path" && ! -L "$path" ]] || { echo "required install asset missing: $path" >&2; exit 1; }
done
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
elif [[ -e /srv/legal-mcp ]]; then
  [[ -d /srv/legal-mcp && ! -L /srv/legal-mcp \
    && "$(stat -c '%U:%G:%a' /srv/legal-mcp)" = root:root:755 \
    && -z "$(find /srv/legal-mcp -mindepth 1 -maxdepth 1 -print -quit)" ]] || {
    echo 'unmounted corpus target has unsafe ownership or mode' >&2
    exit 1
  }
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
  [[ ! -e "$MARKER" ]] || { echo 'new volume unexpectedly contains an identity marker' >&2; exit 1; }
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

install -d -o root -g root -m 0755 /var/lib/legal-mcp-publisher /var/lib/legal-mcp-publisher/.ssh
printf 'restrict,command="/usr/local/sbin/legal-mcp-publisher-command" %s\n' "$publisher_key" \
  > /var/lib/legal-mcp-publisher/.ssh/authorized_keys
chown -R root:root /var/lib/legal-mcp-publisher
chmod 700 /var/lib/legal-mcp-publisher/.ssh
chmod 600 /var/lib/legal-mcp-publisher/.ssh/authorized_keys
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
cat > /etc/sudoers.d/legal-mcp-publisher <<'EOF'
Defaults:legal-mcp-publisher !requiretty
legal-mcp-publisher ALL=(root) NOPASSWD: /usr/local/sbin/legal-mcp-host-deploy prepare *, /usr/local/sbin/legal-mcp-host-deploy activate *
EOF
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

CADDY_VERSION=2.11.4
CADDY_DEB_SHA512=5e0448ecf73056f091b7583b230b973841653581c9b6f11192acbcc048e19f0034385534ff8b25b4f782b26d7f7a67eb91391c70e3caa53dccf495bece475244
CADDY_DEB="caddy_${CADDY_VERSION}_linux_amd64.deb"
curl --fail --location --proto '=https' --tlsv1.2 --retry 5 \
  --output "/tmp/$CADDY_DEB" "https://github.com/caddyserver/caddy/releases/download/v${CADDY_VERSION}/$CADDY_DEB"
echo "$CADDY_DEB_SHA512  /tmp/$CADDY_DEB" | sha512sum --check -
dpkg --install "/tmp/$CADDY_DEB" || apt-get install --fix-broken --yes
rm -f "/tmp/$CADDY_DEB"
systemctl disable --now caddy.service || true
sed "s/__PUBLIC_HOST__/$PUBLIC_HOST/g" "$REPO_DIR/infra/hosting/Caddyfile" > /etc/caddy/Caddyfile
chown root:caddy /etc/caddy/Caddyfile
chmod 640 /etc/caddy/Caddyfile
caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile

systemctl daemon-reload
systemctl cat legal-mcp.service >/dev/null
systemctl disable --now legal-mcp.service caddy.service || true

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

cat > /etc/legal-mcp/host-installed <<EOF
LEGAL_MCP_HOST_V1
VOLUME_UUID=${UUID,,}
EOF
chown root:root /etc/legal-mcp/host-installed
chmod 444 /etc/legal-mcp/host-installed

cat <<EOF
Host installation complete.
Volume UUID: ${UUID,,}
Keep this root session open. In a second session, connect as legal-mcp-admin
and prove 'sudo -n true' before disconnecting root.
The application and Caddy remain disabled. Deploy a corpus generation first,
then configure API-key and/or Entra auth, prove readiness, and enable ingress.
Confirm the attached Akamai Cloud Firewall still allows SSH only from $ADMIN_SOURCE_IP;
add public TCP 80/443 only at the final ingress cutover, and never expose 51235.
$(if [[ -e /var/run/reboot-required ]]; then echo 'A reboot is required before corpus deployment.'; fi)
EOF
