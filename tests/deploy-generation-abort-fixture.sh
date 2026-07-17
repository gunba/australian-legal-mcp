#!/usr/bin/env bash
# Exercise the local operator's explicit abort mode with deterministic SSH and
# rsync fakes. The production deploy helper path may be passed as argv[1].
set -euo pipefail
umask 077
export LC_ALL=C

DEPLOY="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/scripts/deploy-generation.sh}"
[[ -x "$DEPLOY" ]] || {
  echo 'fixture requires an executable deploy-generation.sh' >&2
  exit 2
}

generation=1a6beead567b55babebbe253b5ae13efcd9ce2e8ab55b60c2de4106e39f180f4
fixture_root="$(mktemp -d)"
trap 'rm -rf "$fixture_root"' EXIT
bin="$fixture_root/bin"
mkdir -p "$bin"
ssh_log="$fixture_root/ssh.log"

cat > "$bin/ssh" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FIXTURE_SSH_LOG"
case "${!#}" in
  abort\ *) printf '%s\n' "${FIXTURE_ABORT_RESPONSE:-aborted}" ;;
  prepare\ *) printf '%s\n' prepared ;;
  activate\ *)
    if [[ ${FIXTURE_ACTIVATE_STATUS:-0} -ne 0 ]]; then
      exit "$FIXTURE_ACTIVATE_STATUS"
    fi
    printf '%s\n' activated
    ;;
  *) exit 91 ;;
esac
EOF
cat > "$bin/rsync" <<'EOF'
#!/usr/bin/env bash
printf 'rsync:%s\n' "$*" >> "$FIXTURE_SSH_LOG"
exit "${FIXTURE_RSYNC_STATUS:-0}"
EOF
chmod 755 "$bin/ssh" "$bin/rsync"
export PATH="$bin:/usr/bin:/bin"
export FIXTURE_SSH_LOG="$ssh_log"

output="$("$DEPLOY" --host legal-mcp-publisher@fixture.example --abort "$generation")"
[[ "$output" = "aborted generation $generation on legal-mcp-publisher@fixture.example" ]]
[[ "$(wc -l < "$ssh_log")" -eq 1 ]]
grep -Fxq -- \
  "-o BatchMode=yes -o ConnectTimeout=15 -o ServerAliveInterval=30 -o ServerAliveCountMax=120 legal-mcp-publisher@fixture.example abort $generation" \
  "$ssh_log"
if grep -q '^rsync:' "$ssh_log"; then
  echo 'explicit abort unexpectedly invoked rsync' >&2
  exit 1
fi

: > "$ssh_log"
export FIXTURE_ABORT_RESPONSE=already-aborted
output="$("$DEPLOY" --abort "$generation" --host legal-mcp-publisher@fixture.example)"
[[ "$output" = "generation $generation is already aborted on legal-mcp-publisher@fixture.example" ]]
[[ "$(wc -l < "$ssh_log")" -eq 1 ]]

: > "$ssh_log"
export FIXTURE_ABORT_RESPONSE=unexpected
if "$DEPLOY" --host legal-mcp-publisher@fixture.example --abort "$generation" \
  >/tmp/deploy-abort.stdout 2>/tmp/deploy-abort.stderr; then
  echo 'unexpected remote abort response was accepted' >&2
  exit 1
fi
[[ "$(wc -l < "$ssh_log")" -eq 1 ]]

: > "$ssh_log"
unset FIXTURE_ABORT_RESPONSE
if "$DEPLOY" --host legal-mcp-publisher@fixture.example --abort 1 \
  >/tmp/deploy-abort.stdout 2>/tmp/deploy-abort.stderr; then
  echo 'malformed explicit abort generation was accepted' >&2
  exit 1
fi
[[ ! -s "$ssh_log" ]]

# A normal failed upload remains resumable. The local helper must never infer an
# abort or issue one from an EXIT trap.
runtime="$fixture_root/runtime"
source_dir="$runtime/generations/$generation"
mkdir -p "$runtime/lifecycle" "$source_dir"
printf '%s' "$generation" > "$runtime/lifecycle/active-generation"
cat > "$fixture_root/legal-mcp" <<'EOF'
#!/usr/bin/env bash
[[ "$1" = verify && "$2" = --quiet ]]
EOF
chmod 755 "$fixture_root/legal-mcp"
: > "$ssh_log"
export FIXTURE_RSYNC_STATUS=42
if LEGAL_MCP_DATA_DIR="$runtime" LEGAL_MCP_BINARY="$fixture_root/legal-mcp" \
  "$DEPLOY" --host legal-mcp-publisher@fixture.example \
  >/tmp/deploy-normal.stdout 2>/tmp/deploy-normal.stderr; then
  echo 'failed fixture rsync was unexpectedly accepted' >&2
  exit 1
fi
grep -Fq "legal-mcp-publisher@fixture.example prepare $generation" "$ssh_log"
grep -Fq -- '--compress-choice=zstd --compress-level=3' "$ssh_log"
if grep -Fq "abort $generation" "$ssh_log"; then
  echo 'failed upload triggered an automatic abort' >&2
  exit 1
fi
if grep -Fq "activate $generation" "$ssh_log"; then
  echo 'failed upload unexpectedly reached activation' >&2
  exit 1
fi

# A lost or failed activation response also remains an explicit recovery
# decision and never triggers abort automatically.
: > "$ssh_log"
export FIXTURE_RSYNC_STATUS=0
export FIXTURE_ACTIVATE_STATUS=43
if LEGAL_MCP_DATA_DIR="$runtime" LEGAL_MCP_BINARY="$fixture_root/legal-mcp" \
  "$DEPLOY" --host legal-mcp-publisher@fixture.example \
  >/tmp/deploy-normal.stdout 2>/tmp/deploy-normal.stderr; then
  echo 'failed fixture activation was unexpectedly accepted' >&2
  exit 1
fi
grep -Fq "legal-mcp-publisher@fixture.example prepare $generation" "$ssh_log"
grep -Fq "legal-mcp-publisher@fixture.example activate $generation" "$ssh_log"
if grep -Fq "abort $generation" "$ssh_log"; then
  echo 'failed activation triggered an automatic abort' >&2
  exit 1
fi

echo deploy-generation-abort-fixture-ok
