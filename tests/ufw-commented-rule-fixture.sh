#!/usr/bin/env bash
# Exercise the exact Ubuntu 24.04 UFW representation and commented-rule
# deletion contract used by the hosted maintenance scripts.
set -euo pipefail
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

repo=${1:-/repo}
[[ $EUID -eq 0 && -e /.dockerenv && -d "$repo/infra" ]] || {
  echo 'fixture requires a disposable root container and the repository at /repo' >&2
  exit 2
}
command -v ufw >/dev/null || {
  echo 'ufw is required' >&2
  exit 2
}

# Bind the production parsers to the representation proved below. Plain
# `ufw status` says ALLOW; only verbose status includes the `ALLOW IN` token
# consumed by these strict parsers.
python3 - "$repo/infra/linode/install-host.sh" \
  "$repo/infra/hosting/update-image.sh" <<'PY'
import pathlib, sys
for value in sys.argv[1:]:
    text = pathlib.Path(value).read_text()
    start = text.index("ufw_rule_state() {")
    end = text.index("\n}\n", start)
    function = text[start:end]
    if 'report="$(ufw status verbose)"' not in function:
        raise SystemExit(f"ufw_rule_state does not use verbose status: {value}")
PY

ufw --force reset >/dev/null
trap 'ufw --force reset >/dev/null 2>&1 || true' EXIT
ufw default deny incoming >/dev/null
ufw default allow outgoing >/dev/null
ufw allow 80/tcp comment 'Caddy ACME HTTP' >/dev/null
ufw allow 443/tcp comment 'Australian Legal MCP HTTPS' >/dev/null
ufw --force enable >/dev/null

plain="$(ufw status)"
verbose="$(ufw status verbose)"
grep -Eq '^80/tcp[[:space:]]+ALLOW[[:space:]]+Anywhere' <<< "$plain"
if grep -Eq '^80/tcp[[:space:]]+ALLOW IN' <<< "$plain"; then
  echo 'plain UFW status unexpectedly included the verbose ALLOW IN token' >&2
  exit 1
fi
grep -Eq '^80/tcp[[:space:]]+ALLOW IN[[:space:]]+Anywhere' <<< "$verbose"
grep -Eq '^443/tcp[[:space:]]+ALLOW IN[[:space:]]+Anywhere' <<< "$verbose"

ufw --force delete allow 80/tcp comment 'Caddy ACME HTTP' >/dev/null
ufw --force delete allow 443/tcp comment 'Australian Legal MCP HTTPS' >/dev/null
closed="$(ufw status verbose)"
if grep -Eq '^(80|443)/tcp([[:space:]]|$)' <<< "$closed"; then
  echo 'commented UFW web rule remained after exact deletion' >&2
  exit 1
fi

printf '%s\n' real-ufw-commented-rule-fixture-ok
