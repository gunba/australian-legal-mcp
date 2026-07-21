#!/usr/bin/env bash
# Prove that the v0.19.8 bridge's private file bind can execute an exact
# adapter sourced from production's noexec /run-style tmpfs without making the
# source mount executable.
set -euo pipefail
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

[[ $EUID -eq 0 && -e /.dockerenv \
  && ${LEGAL_MCP_DISPOSABLE_MOUNT_FIXTURE:-} = 1 ]] || {
  echo 'fixture requires an explicitly authorized disposable privileged container' >&2
  exit 2
}

source_mount=/fixture-noexec-source
target_mount=/fixture-exec-target
cleanup() {
  umount "$target_mount/adapter" >/dev/null 2>&1 || true
  umount "$source_mount" >/dev/null 2>&1 || true
  rm -rf "$source_mount" "$target_mount"
}
trap cleanup EXIT
cleanup
mkdir "$source_mount" "$target_mount"
mount -t tmpfs -o noexec,nodev,nosuid tmpfs "$source_mount"
cat > "$source_mount/adapter" <<'ADAPTER'
#!/usr/bin/bash
printf '%s\n' exact-noexec-adapter-ok
ADAPTER
chmod 500 "$source_mount/adapter"
install -o root -g root -m 0500 /dev/null "$target_mount/adapter"

if "$source_mount/adapter" >/dev/null 2>&1; then
  echo 'noexec source unexpectedly executed directly' >&2
  exit 1
fi
mount --bind "$source_mount/adapter" "$target_mount/adapter"
mount -o remount,bind,ro,nodev,nosuid,exec "$target_mount/adapter"
options="$(findmnt --noheadings --raw --output OPTIONS --target "$target_mount/adapter")"
[[ ",$options," != *,noexec,* && ",$options," = *,ro,* \
  && ",$options," = *,nodev,* && ",$options," = *,nosuid,* ]]
[[ "$("$target_mount/adapter")" = exact-noexec-adapter-ok ]]
if "$source_mount/adapter" >/dev/null 2>&1; then
  echo 'private executable bind changed the noexec source mount' >&2
  exit 1
fi

printf '%s\n' v0198-bridge-noexec-mount-fixture-ok
