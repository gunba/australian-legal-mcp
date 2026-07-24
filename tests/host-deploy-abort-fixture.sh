#!/usr/bin/env bash
# Run as root in a disposable Ubuntu-compatible container with the production
# legal-mcp-host-deploy helper mounted read-only at /host-deploy. Storage probes
# are deterministic fakes; filesystem ownership, modes, ACLs, locks, and paths
# are real.
set -euo pipefail
umask 027
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

[[ $EUID -eq 0 && -x /host-deploy ]] || {
  echo 'fixture requires root and /host-deploy' >&2
  exit 2
}
for command_name in flock getfacl groupadd setfacl useradd; do
  command -v "$command_name" >/dev/null || {
    echo "fixture dependency is missing: $command_name" >&2
    exit 2
  }
done

generation=1a6beead567b55babebbe253b5ae13efcd9ce2e8ab55b60c2de4106e39f180f4
other_generation="$(printf '%064d' 1)"
volume_uuid=11111111-2222-3333-4444-555555555555
log=/tmp/host-deploy-abort.log

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

# Keep real mutation tools behind logging wrappers so durability and deletion
# ordering can be asserted without weakening their behavior.
mv /usr/bin/rm /usr/bin/rm.fixture-real
mv /usr/bin/mv /usr/bin/mv.fixture-real
/usr/bin/mv.fixture-real /usr/bin/sync /usr/bin/sync.fixture-real
/usr/bin/mv.fixture-real /usr/bin/flock /usr/bin/flock.fixture-real
/usr/bin/mv.fixture-real /usr/bin/chown /usr/bin/chown.fixture-real
/usr/bin/mv.fixture-real /usr/bin/chmod /usr/bin/chmod.fixture-real
cat > /usr/bin/rm <<'EOF'
#!/usr/bin/bash
printf 'rm:%s\n' "$*" >> /tmp/host-deploy-abort.log
point=''
if [[ -s /tmp/kill-deploy-after ]]; then point="$(</tmp/kill-deploy-after)"; fi
if [[ "$point" = upload-mid-delete \
  && "$*" == *'/srv/legal-mcp/uploads/1a6beead567b55babebbe253b5ae13efcd9ce2e8ab55b60c2de4106e39f180f4'* ]]; then
  directory="${!#}"
  victim="$(find "$directory" -mindepth 1 -maxdepth 1 -print -quit)"
  [[ -n "$victim" ]]
  /usr/bin/rm.fixture-real -rf -- "$victim"
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
/usr/bin/rm.fixture-real "$@"
status=$?
if [[ $status -eq 0 && "$point" = journal-removed \
  && "$*" = '-f -- /srv/legal-mcp/lifecycle/.deployment-transaction' ]]; then
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
exit "$status"
EOF
cat > /usr/bin/mv <<'EOF'
#!/usr/bin/bash
printf 'mv:%s\n' "$*" >> /tmp/host-deploy-abort.log
/usr/bin/mv.fixture-real "$@"
status=$?
if [[ $status -eq 0 && -s /tmp/kill-deploy-after \
  && "$(</tmp/kill-deploy-after)" = journal-published \
  && "$*" = '-fT /srv/legal-mcp/lifecycle/.deployment-transaction.preparing /srv/legal-mcp/lifecycle/.deployment-transaction' ]]; then
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
exit "$status"
EOF
cat > /usr/bin/sync <<'EOF'
#!/usr/bin/bash
printf 'sync:%s\n' "$*" >> /tmp/host-deploy-abort.log
/usr/bin/sync.fixture-real "$@"
status=$?
if [[ $status -eq 0 && -s /tmp/kill-deploy-after ]]; then
  point="$(</tmp/kill-deploy-after)"
  if { [[ "$point" = journal-synced \
        && "$*" = '-f /srv/legal-mcp/lifecycle/.deployment-transaction.preparing' ]] \
      || [[ "$point" = journal-parent-synced \
        && "$*" = '-f /srv/legal-mcp/lifecycle' ]]; }; then
    kill -KILL "$PPID"
    sleep 1
    exit 137
  fi
fi
exit "$status"
EOF
cat > /usr/bin/chown <<'EOF'
#!/usr/bin/bash
point=''
if [[ -s /tmp/kill-deploy-after ]]; then point="$(</tmp/kill-deploy-after)"; fi
if [[ "$point" = journal-written \
  && "${!#}" = /srv/legal-mcp/lifecycle/.deployment-transaction.preparing ]]; then
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
/usr/bin/chown.fixture-real "$@"
status=$?
if [[ $status -eq 0 && "$point" = journal-chowned \
  && "${!#}" = /srv/legal-mcp/lifecycle/.deployment-transaction.preparing ]]; then
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
exit "$status"
EOF
cat > /usr/bin/chmod <<'EOF'
#!/usr/bin/bash
/usr/bin/chmod.fixture-real "$@"
status=$?
if [[ $status -eq 0 && -s /tmp/kill-deploy-after \
  && "$(</tmp/kill-deploy-after)" = journal-chmodded \
  && "${!#}" = /srv/legal-mcp/lifecycle/.deployment-transaction.preparing ]]; then
  kill -KILL "$PPID"
  sleep 1
  exit 137
fi
exit "$status"
EOF
cat > /usr/bin/flock <<'EOF'
#!/usr/bin/bash
printf 'flock-attempt:%s\n' "$*" >> /tmp/host-deploy-abort.log
if [[ -p /tmp/abort-flock-attempt ]]; then
  printf 'attempted\n' > /tmp/abort-flock-attempt
fi
/usr/bin/flock.fixture-real "$@"
status=$?
if [[ $status -eq 0 ]]; then
  printf 'flock-acquired:%s\n' "$*" >> /tmp/host-deploy-abort.log
fi
exit "$status"
EOF
/usr/bin/chmod.fixture-real 755 /usr/bin/rm /usr/bin/mv /usr/bin/sync \
  /usr/bin/flock /usr/bin/chown /usr/bin/chmod

# The fixture is not mounted on XFS. These fakes expose the exact validated host
# contract and can deterministically report a nested mount for rejection tests.
cat > /usr/bin/findmnt <<'EOF'
#!/usr/bin/bash
printf 'findmnt:%s\n' "$*" >> /tmp/host-deploy-abort.log
if [[ "$*" == *'--output TARGET,SOURCE,FSTYPE,OPTIONS'* ]]; then
  printf '/srv/legal-mcp /dev/fixture-xfs xfs rw,noatime,nodev,noexec,nosuid\n'
  exit 0
fi
target="${!#}"
printf '/srv/legal-mcp\n'
if [[ -e /tmp/report-abort-submount ]]; then
  printf '%s/nested-mount\n' "$target"
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

# Runtime commands are genuinely absent so dependency preflight as well as
# invocation regressions fail after the early foreign-transaction guard.
for forbidden in podman systemctl curl; do
  /usr/bin/rm.fixture-real -f -- "/usr/bin/$forbidden" "/usr/sbin/$forbidden"
  if command -v "$forbidden" >/dev/null; then
    echo "fixture could not remove forbidden runtime dependency: $forbidden" >&2
    exit 2
  fi
done

real_rm=/usr/bin/rm.fixture-real
upload=/srv/legal-mcp/uploads/$generation
temporary=/srv/legal-mcp/uploads/.$generation.preparing
installed=/srv/legal-mcp/generations/$generation
journal=/srv/legal-mcp/lifecycle/.deployment-transaction
journal_preparing=/srv/legal-mcp/lifecycle/.deployment-transaction.preparing
pointer=/srv/legal-mcp/lifecycle/active-generation
authorization=/run/legal-mcp/authorized-upload

make_authorization() {
  printf '%s\n' "$generation" > "$authorization"
  chown root:legal-mcp-publisher "$authorization"
  chmod 440 "$authorization"
}

write_fixture_journal() {
  local journal_generation="$1" previous="$2" phase="$3"
  printf '%s\n%s\n%s\n' "$journal_generation" "$previous" "$phase" > "$journal"
  chown root:root "$journal"
  chmod 600 "$journal"
}

make_upload() {
  install -d -o legal-mcp-publisher -g legal-mcp-publisher -m 0700 "$upload"
  printf 'partial corpus bytes\n' > "$upload/partial"
  chown legal-mcp-publisher:legal-mcp-publisher "$upload/partial"
  chmod 600 "$upload/partial"
}

reset_prepared() {
  "$real_rm" -rf -- "$upload" "$temporary" "$installed" "$journal" "$pointer" \
    "$journal_preparing" "$authorization" /tmp/report-abort-submount \
    /tmp/kill-deploy-after
  make_upload
  write_fixture_journal "$generation" - prepared
  make_authorization
  : > "$log"
}

expect_abort_failed() {
  if /host-deploy abort "$generation" >/tmp/abort.stdout 2>/tmp/abort.stderr; then
    echo 'unsafe abort state was unexpectedly accepted' >&2
    exit 1
  fi
  [[ ! -e "$authorization" && ! -L "$authorization" ]] || {
    echo 'failed abort did not revoke upload authorization first' >&2
    exit 1
  }
}

line_of_first() {
  grep -nF "$1" "$log" | head -n 1 | cut -d: -f1
}

# Journal preparation cleanup is unconditional under the lock, but a foreign
# host transaction still blocks authorization and corpus mutation immediately
# afterward.
reset_prepared
printf 'partial\n' > "$journal_preparing"
install -d -o root -g root -m 0700 /etc/legal-mcp/.auth-transaction
if /host-deploy abort "$generation" >/tmp/foreign.stdout 2>/tmp/foreign.stderr; then
  echo 'foreign transaction with journal residue unexpectedly allowed abort' >&2
  exit 1
fi
[[ ! -e "$journal_preparing" && -e "$authorization" && -d "$upload" && -f "$journal" ]]
"$real_rm" -rf -- /etc/legal-mcp/.auth-transaction

# Every publisher deploy operation rejects durable foreign host transactions
# before even revoking upload authorization. This includes the two cutover
# retirement names that can survive SIGKILL before their parent sync.
for transaction in \
  /etc/legal-mcp/.auth-transaction \
  /etc/legal-mcp/.image-transaction.preparing \
  /etc/legal-mcp/.image-transaction \
  /etc/legal-mcp/.image-transaction.retiring \
  /etc/legal-mcp/.host-tools-transaction.preparing \
  /etc/legal-mcp/.host-tools-transaction \
  /etc/legal-mcp/.host-tools-transaction.retiring \
  /etc/legal-mcp/.host-tools-transaction.rollback-retiring \
  /etc/legal-mcp/.host-tools-transaction.rollback-retired \
  /etc/legal-mcp/.host-tools-transaction.publisher-restore; do
  reset_prepared
  install -d -o root -g root -m 0700 "$transaction"
  for action in prepare activate abort; do
    if /host-deploy "$action" "$generation" \
      >/tmp/foreign.stdout 2>/tmp/foreign.stderr; then
      echo "foreign transaction unexpectedly allowed $action: $transaction" >&2
      exit 1
    fi
    grep -Fq 'a foreign host transaction must be recovered' /tmp/foreign.stderr
    [[ -e "$authorization" && -d "$upload" && -f "$journal" ]]
  done
  if grep -Fq 'rm:-f -- /run/legal-mcp/authorized-upload' "$log"; then
    echo 'foreign transaction rejection mutated upload authorization' >&2
    exit 1
  fi
  "$real_rm" -rf -- "$transaction"
done

# Prepared -> aborting -> removed is ordered durably, removes only the exact
# upload, and leaves sibling state untouched.
reset_prepared
install -d -o legal-mcp-publisher -g legal-mcp-publisher -m 0700 \
  /srv/legal-mcp/uploads/sibling
output="$(/host-deploy abort "$generation")"
[[ "$output" = aborted ]]
[[ ! -e "$authorization" && ! -e "$upload" && ! -e "$journal" ]]
[[ -d /srv/legal-mcp/uploads/sibling ]]
[[ ! -e "$pointer" && ! -e "$installed" ]]
lock_acquired="$(line_of_first 'flock-acquired:-x 9')"
authorization_rm="$(line_of_first 'rm:-f -- /run/legal-mcp/authorized-upload')"
authorization_sync="$(line_of_first 'sync:-f /run/legal-mcp')"
host_validation="$(line_of_first 'findmnt:--noheadings --raw --output TARGET,SOURCE,FSTYPE,OPTIONS --target /srv/legal-mcp')"
journal_sync="$(line_of_first 'sync:-f /srv/legal-mcp/lifecycle/.deployment-transaction.preparing')"
journal_move="$(line_of_first 'mv:-fT /srv/legal-mcp/lifecycle/.deployment-transaction.preparing /srv/legal-mcp/lifecycle/.deployment-transaction')"
mapfile -t lifecycle_syncs < <(grep -nFx 'sync:-f /srv/legal-mcp/lifecycle' "$log" | cut -d: -f1)
upload_rm="$(line_of_first "rm:-rf --one-file-system -- $upload")"
uploads_sync="$(line_of_first 'sync:-f /srv/legal-mcp/uploads')"
journal_rm="$(line_of_first 'rm:-f -- /srv/legal-mcp/lifecycle/.deployment-transaction')"
[[ ${#lifecycle_syncs[@]} -eq 2 \
  && "$lock_acquired" -lt "$authorization_rm" \
  && "$authorization_rm" -lt "$authorization_sync" \
  && "$authorization_sync" -lt "$host_validation" \
  && "$journal_sync" -lt "$journal_move" \
  && "$journal_move" -lt "${lifecycle_syncs[0]}" \
  && "${lifecycle_syncs[0]}" -lt "$upload_rm" \
  && "$upload_rm" -lt "$uploads_sync" \
  && "$uploads_sync" -lt "$journal_rm" \
  && "$journal_rm" -lt "${lifecycle_syncs[1]}" ]]

# The fixed lock-owned journal preparation is reconciled on entry before any
# abort mutation, even when SIGKILL left it only partially constructed.
reset_prepared
printf 'partial\n' > "$journal_preparing"
chmod 640 "$journal_preparing"
output="$(/host-deploy abort "$generation")"
[[ "$output" = aborted && ! -e "$journal_preparing" ]]
preparation_rm="$(line_of_first 'rm:-f -- /srv/legal-mcp/lifecycle/.deployment-transaction.preparing')"
authorization_rm="$(line_of_first 'rm:-f -- /run/legal-mcp/authorized-upload')"
[[ "$preparation_rm" -lt "$authorization_rm" ]]

# A valid non-bootstrap previous pointer is preserved exactly.
reset_prepared
write_fixture_journal "$generation" "$other_generation" prepared
install -d -o root -g legal-mcp -m 0555 "/srv/legal-mcp/generations/$other_generation"
printf '%s' "$other_generation" > "$pointer"
chmod 644 "$pointer"
output="$(/host-deploy abort "$generation")"
[[ "$output" = aborted && "$(<"$pointer")" = "$other_generation" ]]
[[ -d "/srv/legal-mcp/generations/$other_generation" ]]
"$real_rm" -rf -- "/srv/legal-mcp/generations/$other_generation" "$pointer"

# Both durable interruption points recover: after aborting was journalled but
# before deletion, and after deletion but before journal removal.
reset_prepared
write_fixture_journal "$generation" - aborting
output="$(/host-deploy abort "$generation")"
[[ "$output" = aborted && ! -e "$upload" && ! -e "$journal" ]]
reset_prepared
write_fixture_journal "$generation" - aborting
"$real_rm" -rf -- "$upload"
output="$(/host-deploy abort "$generation")"
[[ "$output" = aborted && ! -e "$journal" ]]

kill_abort_at() {
  local point="$1" status
  reset_prepared
  printf '%s\n' "$point" > /tmp/kill-deploy-after
  set +e
  /host-deploy abort "$generation" \
    >/tmp/abort-kill.stdout 2>/tmp/abort-kill.stderr
  status=$?
  set -e
  "$real_rm" -f /tmp/kill-deploy-after
  [[ $status -ne 0 ]]
}

# Every journal construction operation is SIGKILL-recoverable. Pre-publish
# kills retain the old prepared journal plus a disposable fixed temp; later
# kills retain the complete aborting journal.
for point in journal-written journal-chowned journal-chmodded journal-synced \
  journal-published journal-parent-synced; do
  kill_abort_at "$point"
  /host-deploy abort "$generation" >/tmp/abort-recovered.stdout
  grep -Fxq aborted /tmp/abort-recovered.stdout
  [[ ! -e "$journal" && ! -e "$journal_preparing" && ! -e "$upload" ]]
done

# SIGKILL after one child is actually deleted leaves the exact upload root and
# aborting journal recoverable; a kill after journal unlink is idempotent too.
kill_abort_at upload-mid-delete
[[ -d "$upload" && -f "$journal" && ! -e "$upload/partial" ]]
/host-deploy abort "$generation" >/tmp/abort-recovered.stdout
grep -Fxq aborted /tmp/abort-recovered.stdout

kill_abort_at journal-removed
[[ ! -e "$upload" && ! -e "$journal" ]]
/host-deploy abort "$generation" >/tmp/abort-recovered.stdout
grep -Fxq already-aborted /tmp/abort-recovered.stdout

# Idempotence is allowed only when every generation-specific location is clean.
reset_prepared
"$real_rm" -rf -- "$upload" "$journal"
output="$(/host-deploy abort "$generation")"
[[ "$output" = already-aborted && ! -e "$authorization" ]]
line_of_first 'sync:-f /srv/legal-mcp/uploads' >/dev/null
line_of_first 'sync:-f /srv/legal-mcp/lifecycle' >/dev/null

# An existing transaction-lock holder prevents authorization revocation and all
# cleanup until the same exclusive lock is released.
reset_prepared
mkfifo /tmp/abort-lock-ready /tmp/abort-lock-release
(
  exec 8<> /run/lock/legal-mcp-host-transaction.lock
  flock -x 8
  printf 'ready\n' > /tmp/abort-lock-ready
  read -r _ < /tmp/abort-lock-release
) &
lock_holder=$!
read -r _ < /tmp/abort-lock-ready
mkfifo /tmp/abort-flock-attempt
/host-deploy abort "$generation" >/tmp/concurrent.stdout 2>/tmp/concurrent.stderr &
abort_pid=$!
read -r _ < /tmp/abort-flock-attempt
kill -0 "$abort_pid"
[[ -e "$authorization" ]]
printf 'release\n' > /tmp/abort-lock-release
wait "$lock_holder"
wait "$abort_pid"
[[ "$(</tmp/concurrent.stdout)" = aborted && ! -e "$authorization" ]]
"$real_rm" -f -- /tmp/abort-lock-ready /tmp/abort-lock-release /tmp/abort-flock-attempt

# Authorization is revoked and synced before even a later host validation
# failure. The journal and upload remain untouched.
reset_prepared
chmod 640 /srv/legal-mcp/.legal-mcp-volume
expect_abort_failed
[[ -e "$upload" && -e "$journal" ]]
authorization_sync="$(line_of_first 'sync:-f /run/legal-mcp')"
host_validation="$(line_of_first 'findmnt:--noheadings --raw --output TARGET,SOURCE,FSTYPE,OPTIONS --target /srv/legal-mcp')"
[[ "$authorization_sync" -lt "$host_validation" ]]
chmod 444 /srv/legal-mcp/.legal-mcp-volume

# Every journal phase except prepared/aborting fails closed.
for phase in preparing activating activated rolling-back rolled-back; do
  reset_prepared
  write_fixture_journal "$generation" - "$phase"
  expect_abort_failed
done

# Wrong transaction identity and active-pointer relationships fail closed.
reset_prepared
write_fixture_journal "$other_generation" - prepared
expect_abort_failed
reset_prepared
install -d -o root -g legal-mcp -m 0555 "/srv/legal-mcp/generations/$other_generation"
printf '%s' "$other_generation" > "$pointer"
chmod 644 "$pointer"
expect_abort_failed
"$real_rm" -rf -- "/srv/legal-mcp/generations/$other_generation" "$pointer"
reset_prepared
install -d -o root -g legal-mcp -m 0555 "$installed"
printf '%s' "$generation" > "$pointer"
chmod 644 "$pointer"
expect_abort_failed

# Wrong upload type, owner, mode, ACL, mount topology, temp residue, or installed
# target is never normalized or recursively removed.
reset_prepared
chown root:root "$upload"
expect_abort_failed
[[ -d "$upload" ]]
reset_prepared
chmod 755 "$upload"
expect_abort_failed
[[ -d "$upload" ]]
reset_prepared
setfacl --modify user:legal-mcp:r-x "$upload"
chmod 700 "$upload"
expect_abort_failed
[[ -d "$upload" ]]
reset_prepared
"$real_rm" -rf -- "$upload"
ln -s /tmp "$upload"
expect_abort_failed
[[ -L "$upload" ]]
reset_prepared
touch /tmp/report-abort-submount
expect_abort_failed
[[ -d "$upload" ]]
reset_prepared
install -d -o legal-mcp-publisher -g legal-mcp-publisher -m 0700 "$temporary"
expect_abort_failed
[[ -d "$temporary" && -d "$upload" ]]
reset_prepared
install -d -o root -g legal-mcp -m 0555 "$installed"
expect_abort_failed
[[ -d "$installed" && -d "$upload" ]]
reset_prepared
"$real_rm" -rf -- "$upload"
printf 'not a directory\n' > "$upload"
chown legal-mcp-publisher:legal-mcp-publisher "$upload"
chmod 600 "$upload"
expect_abort_failed
[[ -f "$upload" ]]

# Unsafe/malformed journals and active-pointer symlinks fail closed.
reset_prepared
chmod 644 "$journal"
expect_abort_failed
reset_prepared
printf 'malformed\n' > "$journal"
chmod 600 "$journal"
expect_abort_failed
reset_prepared
ln -s "$other_generation" "$pointer"
expect_abort_failed

# Journal-free idempotence rejects residue in every generation-specific
# location, including broken symlinks.
for residue in upload temporary installed; do
  reset_prepared
  "$real_rm" -rf -- "$upload" "$journal"
  case "$residue" in
    upload) make_upload ;;
    temporary) install -d -o legal-mcp-publisher -g legal-mcp-publisher -m 0700 "$temporary" ;;
    installed) install -d -o root -g legal-mcp -m 0555 "$installed" ;;
  esac
  expect_abort_failed
done
reset_prepared
"$real_rm" -rf -- "$upload" "$journal"
ln -s /does/not/exist "$temporary"
expect_abort_failed

echo host-deploy-abort-fixture-ok
