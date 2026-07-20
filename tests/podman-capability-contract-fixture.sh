#!/usr/bin/env bash
# Prove the rootful Podman runtime representation used by the hosted
# live-capability verifier. Run only on a disposable CI host or maintainer host.
set -euo pipefail
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

repo=${1:-/repo}
[[ $EUID -eq 0 && ${LEGAL_MCP_DISPOSABLE_PODMAN_FIXTURE:-} = 1 \
  && -x "$(command -v podman)" \
  && -f "$repo/infra/hosting/update-image.sh" ]] || {
  echo 'fixture requires root, explicit disposable-Podman authorization, and the repository at /repo' >&2
  exit 2
}

updater="$repo/infra/hosting/update-image.sh"
podman_version="$(podman version --format '{{.Client.Version}}')"
if [[ -n ${LEGAL_MCP_EXPECT_PODMAN_MAJOR_MINOR:-} ]]; then
  [[ "$podman_version" = "$LEGAL_MCP_EXPECT_PODMAN_MAJOR_MINOR".* ]] || {
    echo "expected Podman $LEGAL_MCP_EXPECT_PODMAN_MAJOR_MINOR.x, got $podman_version" >&2
    exit 1
  }
fi
grep -Fq 'podman top australian-legal-mcp capbnd capeff capinh capprm' "$updater"
if grep -Fq '{{json .EffectiveCaps}}' "$updater"; then
  echo 'updater still trusts the Podman EffectiveCaps inspection field' >&2
  exit 1
fi

image='docker.io/library/debian@sha256:63a496b5d3b99214b39f5ed70eb71a61e590a77979c79cbee4faf991f8c0783e'
dropped=legal-mcp-capability-dropped
control=legal-mcp-capability-control
cleanup() {
  podman rm -f "$dropped" "$control" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

validate_empty_report() {
  python3 - "$1" <<'PY'
import sys
lines = sys.argv[1].splitlines()
expected = "BOUNDING CAPS EFFECTIVE CAPS INHERITED CAPS PERMITTED CAPS"
if not lines or " ".join(lines[0].split()) != expected or len(lines) < 2:
    raise SystemExit(1)
for line in lines[1:]:
    if line.split() != ["none", "none", "none", "none"]:
        raise SystemExit(1)
PY
}

podman run -d --name "$dropped" --network=none --user=65534:65534 \
  --cap-drop=all "$image" sleep 300 >/dev/null
inspect_caps="$(podman inspect "$dropped" --format '{{json .EffectiveCaps}}')"
[[ "$inspect_caps" = null || "$inspect_caps" = '[]' ]]
empty_report="$(podman top "$dropped" capbnd capeff capinh capprm)"
validate_empty_report "$empty_report"

podman run -d --name "$control" --network=none --cap-add=net_raw \
  "$image" sleep 300 >/dev/null
control_report="$(podman top "$control" capbnd capeff capinh capprm)"
if validate_empty_report "$control_report"; then
  echo 'live-capability parser accepted a process with a capability' >&2
  exit 1
fi

printf 'real-podman-capability-contract-fixture-ok podman=%s\n' "$podman_version"
