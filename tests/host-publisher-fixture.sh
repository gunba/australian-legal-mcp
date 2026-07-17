#!/usr/bin/env bash
# Run inside a disposable Debian-compatible container as root. The production
# publisher wrapper must be mounted read-only at /publisher.
set -euo pipefail
umask 077
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

[[ $EUID -eq 0 && -x /publisher ]] || {
  echo 'fixture requires root and /publisher' >&2
  exit 2
}

generation="$(printf '%064d' 0)"
other_generation="$(printf '%064d' 1)"
groupadd --gid 973 legal-mcp-publisher
useradd --uid 973 --gid 973 --home-dir /nonexistent --no-create-home legal-mcp-publisher
install -d -o root -g legal-mcp-publisher -m 0710 /run/legal-mcp
install -o root -g legal-mcp-publisher -m 0640 /dev/null \
  /run/lock/legal-mcp-host-transaction.lock
printf '%s\n' "$generation" > /run/legal-mcp/authorized-upload
chown root:legal-mcp-publisher /run/legal-mcp/authorized-upload
chmod 440 /run/legal-mcp/authorized-upload

cat > /usr/bin/rrsync <<'EOF'
#!/usr/bin/bash
if flock -n /run/lock/legal-mcp-host-transaction.lock -c true; then
  echo 'publisher wrapper did not hold the transaction lock' >&2
  exit 1
fi
printf 'rrsync-ok:%s\n' "$*"
EOF
cat > /usr/bin/sudo <<'EOF'
#!/usr/bin/bash
printf 'sudo-ok:%s\n' "$*"
EOF
chmod 755 /usr/bin/rrsync /usr/bin/sudo

run_publisher() {
  local command="$1"
  runuser -u legal-mcp-publisher -- env SSH_ORIGINAL_COMMAND="$command" /publisher
}

expect_rejected() {
  if run_publisher "$1" >/dev/null 2>&1; then
    echo "publisher command was unexpectedly accepted: $1" >&2
    exit 1
  fi
}

output="$(run_publisher "prepare $generation")"
[[ "$output" = "sudo-ok:-n /usr/local/sbin/legal-mcp-host-deploy prepare $generation" ]]
output="$(run_publisher "activate $generation")"
[[ "$output" = "sudo-ok:-n /usr/local/sbin/legal-mcp-host-deploy activate $generation" ]]
output="$(run_publisher "abort $generation")"
[[ "$output" = "sudo-ok:-n /usr/local/sbin/legal-mcp-host-deploy abort $generation" ]]
output="$(run_publisher "rsync --server -vlogDtpre.iLsfxCIvu . $generation/")"
[[ "$output" = 'rrsync-ok:-wo /srv/legal-mcp/uploads' ]]

expect_rejected "rsync --server -vlogDtpre.iLsfxCIvu . $other_generation/"
expect_rejected 'rsync --server -vlogDtpre.iLsfxCIvu . ../escape/'
expect_rejected "prepare $generation extra"
expect_rejected "abort $generation extra"
expect_rejected "Abort $generation"
expect_rejected 'abort 1'

rm -f /run/legal-mcp/authorized-upload
expect_rejected "rsync --server -vlogDtpre.iLsfxCIvu . $generation/"

echo publisher-forced-command-fixture-ok
