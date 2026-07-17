#!/usr/bin/env bash
# Run inside Ubuntu 24.04 after installing the distribution rsync and python3
# packages. A protocol EOF is expected; an rrsync policy rejection is not.
set -euo pipefail
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

[[ -x /usr/bin/rrsync && -x /usr/bin/rsync && -x /usr/bin/python3 ]] || {
  echo 'Ubuntu rsync/python3 packages are required' >&2
  exit 2
}

mkdir -p /srv/legal-mcp/uploads
generation="$(printf '%064d' 0)"
command="rsync --server -ltrce.iLsfxCIvu --delete-delay --safe-links --inplace . $generation/"
set +e
SSH_ORIGINAL_COMMAND="$command" timeout 5 /usr/bin/rrsync -wo \
  /srv/legal-mcp/uploads </dev/null >/tmp/rrsync.stdout 2>/tmp/rrsync.stderr
status=$?
set -e
[[ $status -ne 0 ]]
grep -Fq 'rsync protocol data stream' /tmp/rrsync.stderr
if grep -Eqi 'option.*refused|restricted|invalid command|not allowed' /tmp/rrsync.stderr; then
  echo 'Ubuntu rrsync rejected the production option set' >&2
  exit 1
fi

echo ubuntu-rrsync-options-fixture-ok
