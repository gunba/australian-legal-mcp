#!/usr/bin/env bash
# Run as root in a disposable Ubuntu-compatible container with the production
# legal-mcp-host-deploy helper mounted read-only at /host-deploy. The fixture
# uses real Linux capabilities and filesystem DAC checks while faking only the
# XFS, Podman image, service, and readiness boundaries.
set -euo pipefail
umask 027
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

[[ $EUID -eq 0 && -x /host-deploy ]] || {
  echo 'fixture requires root and /host-deploy' >&2
  exit 2
}
for command_name in flock getfacl groupadd setfacl setpriv useradd; do
  command -v "$command_name" >/dev/null || {
    echo "fixture dependency is missing: $command_name" >&2
    exit 2
  }
done

generation=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
previous=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
volume_uuid=11111111-2222-3333-4444-555555555555
image=ghcr.io/gunba/australian-legal-mcp@sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
upload=/srv/legal-mcp/uploads/$generation
installed=/srv/legal-mcp/generations/$generation
journal=/srv/legal-mcp/lifecycle/.deployment-transaction
pointer=/srv/legal-mcp/lifecycle/active-generation
authorization=/run/legal-mcp/authorized-upload
podman_log=/tmp/host-deploy-activation-podman.log
capability_log=/tmp/host-deploy-activation-capabilities.log

getent group legal-mcp >/dev/null || groupadd --gid 971 legal-mcp
getent passwd legal-mcp >/dev/null ||
  useradd --uid 971 --gid 971 --home-dir /nonexistent --no-create-home legal-mcp
getent group legal-mcp-publisher >/dev/null || groupadd --gid 973 legal-mcp-publisher
getent passwd legal-mcp-publisher >/dev/null ||
  useradd --uid 973 --gid 973 --home-dir /nonexistent --no-create-home legal-mcp-publisher

install -d -o root -g legal-mcp-publisher -m 0710 /run/legal-mcp
install -d -o root -g root -m 0755 /run/lock
install -o root -g legal-mcp-publisher -m 0640 /dev/null \
  /run/lock/legal-mcp-host-transaction.lock
install -d -o root -g root -m 0755 /etc/legal-mcp
printf '%s\n' "$image" > /etc/legal-mcp/image
chmod 600 /etc/legal-mcp/image
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
printf 'inactive\n' > /tmp/legal-mcp-service-activity
: > "$podman_log"
: > "$capability_log"

# The fixture is not mounted on XFS. These fakes expose the exact validated
# volume contract without changing any permission behavior under uploads.
cat > /usr/bin/findmnt <<'EOF'
#!/usr/bin/bash
if [[ "$*" == *'--output TARGET,SOURCE,FSTYPE,OPTIONS'* ]]; then
  printf '/srv/legal-mcp /dev/fixture-xfs xfs rw,noatime,nodev,noexec,nosuid\n'
else
  printf '/srv/legal-mcp\n'
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
chmod 755 /usr/bin/findmnt /usr/sbin/blkid /usr/sbin/xfs_info

mv /usr/bin/flock /usr/bin/flock.fixture-real
cat > /usr/bin/flock <<'EOF'
#!/usr/bin/bash
if [[ -p /tmp/report-activation-flock-attempt ]]; then
  printf 'attempted\n' > /tmp/report-activation-flock-attempt
fi
exec /usr/bin/flock.fixture-real "$@"
EOF
chmod 755 /usr/bin/flock

cat > /usr/bin/systemctl <<'EOF'
#!/usr/bin/bash
case "$1" in
  is-active)
    activity="$(</tmp/legal-mcp-service-activity)"
    printf '%s\n' "$activity"
    [[ "$activity" = active ]] && exit 0
    exit 3
    ;;
  stop)
    printf 'inactive\n' > /tmp/legal-mcp-service-activity
    ;;
  start)
    printf 'active\n' > /tmp/legal-mcp-service-activity
    ;;
  *)
    echo 'unexpected fixture systemctl command' >&2
    exit 2
    ;;
esac
EOF
cat > /usr/bin/curl <<'EOF'
#!/usr/bin/bash
[[ -f /srv/legal-mcp/lifecycle/active-generation ]]
generation="$(</srv/legal-mcp/lifecycle/active-generation)"
printf '{"status":"ok","generation":"%s"}\n' "$generation"
EOF
chmod 755 /usr/bin/systemctl /usr/bin/curl

# The Podman fake validates the complete one-shot security profile, then uses
# setpriv to reproduce the requested effective/bounding capability set before
# touching the real locked publisher tree.
cat > /usr/bin/podman <<'EOF'
#!/usr/bin/bash
set -euo pipefail
image=ghcr.io/gunba/australian-legal-mcp@sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
case "${1:-}" in
  image)
    case "${2:-}" in
      exists) [[ "${3:-}" = "$image" ]] ;;
      inspect) printf 'sha256:fixture-image-id\n' ;;
      *) exit 2 ;;
    esac
    exit
    ;;
  inspect)
    [[ "${2:-}" = australian-legal-mcp ]]
    printf 'sha256:fixture-image-id\n'
    exit
    ;;
  run) ;;
  *) exit 2 ;;
esac

printf '%s\n' "$*" >> /tmp/host-deploy-activation-podman.log
args=("$@")
has_arg() {
  local expected="$1" value
  for value in "${args[@]}"; do
    [[ "$value" = "$expected" ]] && return 0
  done
  return 1
}
for required in --rm --network=none --user=0:0 --read-only --cap-drop=all \
  --security-opt=no-new-privileges --pids-limit=256 --memory=6g \
  --memory-swap=6g --tmpfs=/tmp:rw,nodev,nosuid,noexec,size=64m,mode=1777 \
  --volume=/srv/legal-mcp:/var/lib/legal-mcp:rw,nodev,nosuid; do
  has_arg "$required" || {
    echo "one-shot container omitted hardening argument: $required" >&2
    exit 1
  }
done

cap_add_count=0
image_index=''
for index in "${!args[@]}"; do
  [[ "${args[$index]}" = --cap-add=* ]] && cap_add_count=$((cap_add_count + 1))
  [[ "${args[$index]}" = "$image" ]] && image_index="$index"
done
[[ -n "$image_index" ]]
command=("${args[@]:image_index + 1}")
host_deploy_pid="$PPID"

if [[ "${command[0]:-}" = activate ]]; then
  [[ $cap_add_count -eq 1 ]]
  has_arg --cap-add=dac_override
  [[ ${#command[@]} -eq 5 \
    && "${command[1]}" = --generation-dir \
    && "${command[2]}" = "/var/lib/legal-mcp/uploads/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" \
    && "${command[3]}" = --expected-generation \
    && "${command[4]}" = aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ]]
  HOST_DEPLOY_PID="$host_deploy_pid" \
    setpriv \
      --bounding-set=-all,+dac_override \
      --inh-caps=-all,+dac_override \
      --ambient-caps=-all,+dac_override \
      --no-new-privs \
      /usr/bin/bash -ceu '
        generation=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
        upload=/srv/legal-mcp/uploads/$generation
        installed=/srv/legal-mcp/generations/$generation
        pointer=/srv/legal-mcp/lifecycle/active-generation
        cap_eff="$(awk "/^CapEff:/ {print \$2}" /proc/self/status)"
        printf "activate:%s\n" "$cap_eff" >> /tmp/host-deploy-activation-capabilities.log
        [[ "$cap_eff" = 0000000000000002 ]]
        [[ "$(stat -c "%u:%g:%a" /srv/legal-mcp/uploads)" = 973:973:700 ]]
        [[ -d "$upload" && -f "$upload/payload" ]]
        case "$(</tmp/podman-activation-mode)" in
          fail-before-rename)
            exit 42
            ;;
          kill-before-rename)
            kill -KILL "$HOST_DEPLOY_PID"
            printf "child-ready\n" > /tmp/activation-child-ready
            read -r _ < /tmp/activation-child-release
            exit 137
            ;;
          normal|kill-after-rename|kill-after-pointer)
            mv -T "$upload" "$installed"
            sync -f /srv/legal-mcp/generations
            if [[ "$(</tmp/podman-activation-mode)" = kill-after-rename ]]; then
              kill -KILL "$HOST_DEPLOY_PID"
              exit 137
            fi
            printf "%s" "$generation" > "$pointer"
            sync -f /srv/legal-mcp/lifecycle
            if [[ "$(</tmp/podman-activation-mode)" = kill-after-pointer ]]; then
              kill -KILL "$HOST_DEPLOY_PID"
            fi
            ;;
          *)
            echo "unknown fixture activation mode" >&2
            exit 2
            ;;
        esac
      '
  exit
fi

[[ $cap_add_count -eq 0 ]]
setpriv \
  --bounding-set=-all \
  --inh-caps=-all \
  --ambient-caps=-all \
  --no-new-privs \
  /usr/bin/bash -ceu '
    cap_eff="$(awk "/^CapEff:/ {print \$2}" /proc/self/status)"
    printf "%s:%s\n" "$1" "$cap_eff" >> /tmp/host-deploy-activation-capabilities.log
    [[ "$cap_eff" = 0000000000000000 ]]
    case "$1" in
      verify)
        [[ "$2" = --quiet ]]
        ;;
      prune-generations)
        [[ "$2" = --keep-inactive && "$3" = 1 ]]
        ;;
      rollback)
        [[ "$2" = --generation && "$3" =~ ^[0-9a-f]{64}$ ]]
        printf "%s" "$3" > /srv/legal-mcp/lifecycle/active-generation
        ;;
      deactivate)
        [[ "$2" = --expected-generation && "$3" =~ ^[0-9a-f]{64}$ ]]
        rm -f /srv/legal-mcp/lifecycle/active-generation
        ;;
      *)
        echo "unexpected capability-free lifecycle command: $*" >&2
        exit 2
        ;;
    esac
  ' fixture "${command[@]}"
EOF
chmod 755 /usr/bin/podman

assert_parent_locked() {
  [[ "$(stat -c '%u:%g:%a' /srv/legal-mcp/uploads)" = 973:973:700 ]]
  [[ "$(getfacl --absolute-names --numeric --omit-header /srv/legal-mcp/uploads)" = \
    $'user::rwx\ngroup::---\nother::---' ]]
}

write_journal() {
  local old_generation="$1" phase="$2"
  printf '%s\n%s\n%s\n' "$generation" "$old_generation" "$phase" > "$journal"
  chown root:root "$journal"
  chmod 600 "$journal"
}

make_upload() {
  install -d -o legal-mcp-publisher -g legal-mcp-publisher -m 0700 "$upload"
  printf 'complete staged generation\n' > "$upload/payload"
  chown legal-mcp-publisher:legal-mcp-publisher "$upload/payload"
  chmod 600 "$upload/payload"
}

make_authorization() {
  printf '%s\n' "$generation" > "$authorization"
  chown root:legal-mcp-publisher "$authorization"
  chmod 440 "$authorization"
}

reset_fixture() {
  find /srv/legal-mcp/uploads -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
  find /srv/legal-mcp/generations -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
  rm -f -- "$journal" "$journal.preparing" "$pointer" "$authorization" \
    /tmp/activation-child-ready /tmp/activation-child-release \
    /tmp/report-activation-flock-attempt
  printf 'inactive\n' > /tmp/legal-mcp-service-activity
  printf 'normal\n' > /tmp/podman-activation-mode
  : > "$podman_log"
  : > "$capability_log"
}

prepare_transaction() {
  local old_generation="$1"
  make_upload
  write_journal "$old_generation" prepared
  make_authorization
}

assert_upload_restored() {
  [[ "$(stat -c '%u:%g:%a' "$upload")" = 973:973:700 ]]
  [[ "$(stat -c '%u:%g:%a' "$upload/payload")" = 973:973:600 ]]
  mapfile -t transaction < "$journal"
  [[ ${#transaction[@]} -eq 3 \
    && "${transaction[0]}" = "$generation" \
    && "${transaction[2]}" = prepared ]]
}

# Reproduce the original failure boundary directly: UID 0 with no capabilities
# cannot search the publisher-owned 0700 parent, while exactly DAC_OVERRIDE can.
reset_fixture
make_upload
assert_parent_locked
# shellcheck disable=SC2016 # The nested capability-dropped shell expands its own argv.
if setpriv --bounding-set=-all --inh-caps=-all --ambient-caps=-all \
  --no-new-privs /usr/bin/bash -ceu '[[ -d "$1" ]]' fixture "$upload"; then
  echo 'capability-free root unexpectedly traversed the publisher upload parent' >&2
  exit 1
fi
# shellcheck disable=SC2016 # The nested capability-scoped shell reads its own status and argv.
setpriv \
  --bounding-set=-all,+dac_override \
  --inh-caps=-all,+dac_override \
  --ambient-caps=-all,+dac_override \
  --no-new-privs \
  /usr/bin/bash -ceu '
    [[ "$(awk "/^CapEff:/ {print \$2}" /proc/self/status)" = 0000000000000002 ]]
    [[ -d "$1" ]]
  ' fixture "$upload"

# A pre-rename activation failure restores the complete candidate to the
# publisher-owned prepared state. A direct activation retry consumes the same
# directory inode without prepare, rsync, or abort.
reset_fixture
prepare_transaction -
staged_inode="$(stat -c '%d:%i' "$upload")"
printf 'fail-before-rename\n' > /tmp/podman-activation-mode
if /host-deploy activate "$generation" \
  >/tmp/activation-failure.stdout 2>/tmp/activation-failure.stderr; then
  echo 'fixture activation failure was unexpectedly accepted' >&2
  exit 1
fi
assert_parent_locked
assert_upload_restored
[[ ! -e "$authorization" ]]
[[ "$(stat -c '%d:%i' "$upload")" = "$staged_inode" ]]
grep -Fxq 'activate:0000000000000002' "$capability_log"
printf 'normal\n' > /tmp/podman-activation-mode
output="$(/host-deploy activate "$generation")"
[[ "$output" = activated-pending-auth ]]
assert_parent_locked
[[ ! -e "$journal" && ! -e "$upload" && -d "$installed" ]]
[[ "$(stat -c '%d:%i' "$installed")" = "$staged_inode" ]]
[[ "$(grep -Fc 'activate:0000000000000002' "$capability_log")" -eq 2 ]]
if grep -Eq '(^| )(prepare|abort)( |$)|rsync' "$podman_log"; then
  echo 'activation retry unexpectedly prepared, uploaded, or aborted staging' >&2
  exit 1
fi

# If SIGKILL lands after the activating journal is durable but before the
# one-shot process renames anything, its inherited lock prevents a retry from
# racing it. Releasing that child lets the retry consume the same normalized
# staging without reupload or abort.
reset_fixture
prepare_transaction -
staged_inode="$(stat -c '%d:%i' "$upload")"
printf 'kill-before-rename\n' > /tmp/podman-activation-mode
mkfifo /tmp/activation-child-ready /tmp/activation-child-release
set +e
/host-deploy activate "$generation" \
  >/tmp/activation-killed.stdout 2>/tmp/activation-killed.stderr &
killed_parent=$!
wait "$killed_parent"
killed_status=$?
set -e
[[ $killed_status -ne 0 ]]
read -r child_state < /tmp/activation-child-ready
[[ "$child_state" = child-ready ]]
mapfile -t transaction < "$journal"
[[ "${transaction[2]}" = activating ]]
[[ "$(stat -c '%u:%g:%a' "$upload")" = 0:971:750 ]]
[[ "$(stat -c '%d:%i' "$upload")" = "$staged_inode" ]]
assert_parent_locked
printf 'normal\n' > /tmp/podman-activation-mode
mkfifo /tmp/report-activation-flock-attempt
/host-deploy activate "$generation" > /tmp/activation-retry.stdout &
retry_pid=$!
read -r lock_state < /tmp/report-activation-flock-attempt
[[ "$lock_state" = attempted ]]
kill -0 "$retry_pid"
printf 'release\n' > /tmp/activation-child-release
wait "$retry_pid"
[[ "$(</tmp/activation-retry.stdout)" = activated-pending-auth ]]
[[ ! -e "$journal" && ! -e "$upload" && -d "$installed" ]]
[[ "$(stat -c '%d:%i' "$installed")" = "$staged_inode" ]]
assert_parent_locked
rm -f /tmp/activation-child-ready /tmp/activation-child-release \
  /tmp/report-activation-flock-attempt

# If SIGKILL lands after rename but before the pointer switch, retry uses the
# ordinary capability-free rollback path to publish the already installed
# candidate. It does not rerun privileged activation.
reset_fixture
prepare_transaction -
staged_inode="$(stat -c '%d:%i' "$upload")"
printf 'kill-after-rename\n' > /tmp/podman-activation-mode
set +e
/host-deploy activate "$generation" \
  >/tmp/activation-rename-killed.stdout 2>/tmp/activation-rename-killed.stderr
killed_status=$?
set -e
[[ $killed_status -ne 0 ]]
/usr/bin/flock.fixture-real -x /run/lock/legal-mcp-host-transaction.lock -c true
mapfile -t transaction < "$journal"
[[ "${transaction[2]}" = activating && ! -e "$pointer" ]]
[[ ! -e "$upload" && -d "$installed" ]]
[[ "$(stat -c '%d:%i' "$installed")" = "$staged_inode" ]]
activation_count="$(grep -Fc 'activate:0000000000000002' "$capability_log")"
printf 'normal\n' > /tmp/podman-activation-mode
output="$(/host-deploy activate "$generation")"
[[ "$output" = activated-pending-auth && "$(<"$pointer")" = "$generation" ]]
[[ ! -e "$journal" ]]
[[ "$(grep -Fc 'activate:0000000000000002' "$capability_log")" -eq "$activation_count" ]]
grep -Fxq 'rollback:0000000000000000' "$capability_log"
assert_parent_locked

# If SIGKILL lands after the candidate rename and pointer switch, retry
# reconciles the activating journal without running activation a second time.
reset_fixture
prepare_transaction -
staged_inode="$(stat -c '%d:%i' "$upload")"
printf 'kill-after-pointer\n' > /tmp/podman-activation-mode
set +e
/host-deploy activate "$generation" \
  >/tmp/activation-pointer-killed.stdout 2>/tmp/activation-pointer-killed.stderr
killed_status=$?
set -e
[[ $killed_status -ne 0 ]]
/usr/bin/flock.fixture-real -x /run/lock/legal-mcp-host-transaction.lock -c true
mapfile -t transaction < "$journal"
[[ "${transaction[2]}" = activating && "$(<"$pointer")" = "$generation" ]]
[[ ! -e "$upload" && -d "$installed" ]]
[[ "$(stat -c '%d:%i' "$installed")" = "$staged_inode" ]]
activation_count="$(grep -Fc 'activate:0000000000000002' "$capability_log")"
printf 'normal\n' > /tmp/podman-activation-mode
output="$(/host-deploy activate "$generation")"
[[ "$output" = activated-pending-auth ]]
[[ ! -e "$journal" ]]
[[ "$(grep -Fc 'activate:0000000000000002' "$capability_log")" -eq "$activation_count" ]]
assert_parent_locked

# In a normal replacement activation, only the exact upload activation gets
# DAC_OVERRIDE. Post-start verify and prune remain capability-free, as does the
# long-running service contract outside this one-shot helper.
reset_fixture
install -d -o root -g legal-mcp -m 0555 "/srv/legal-mcp/generations/$previous"
printf '%s' "$previous" > "$pointer"
prepare_transaction "$previous"
printf 'active\n' > /tmp/legal-mcp-service-activity
output="$(/host-deploy activate "$generation")"
[[ "$output" = activated ]]
[[ "$(<"$pointer")" = "$generation" && ! -e "$journal" ]]
assert_parent_locked
grep -Fxq 'activate:0000000000000002' "$capability_log"
grep -Fxq 'verify:0000000000000000' "$capability_log"
grep -Fxq 'prune-generations:0000000000000000' "$capability_log"
[[ "$(grep -Fc -- '--cap-add=dac_override' "$podman_log")" -eq 1 ]]
[[ "$(grep -Fc -- '--cap-drop=all' "$podman_log")" -eq 3 ]]

# The installed Quadlet is independently capability-free; the one-shot
# exception must never leak into the service definition.
[[ -r /legal-mcp.container.template ]]
grep -Fxq 'DropCapability=all' /legal-mcp.container.template
if grep -Fq 'AddCapability=' /legal-mcp.container.template \
  || grep -Fq 'CAP_DAC_OVERRIDE' /legal-mcp.container.template; then
  echo 'service Quadlet unexpectedly grants an activation capability' >&2
  exit 1
fi

echo host-deploy-activation-fixture-ok
