#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
image='caddy@sha256:98eb57d882ccd5213d1688764db10c1ca2c58a1ca3a6717a3411ad798f7a423a'
canary='non-secret-caddy-redaction-canary'
tmp="$(mktemp -d)"
container=''
cleanup() {
  if [[ -n "$container" ]]; then
    docker rm --force "$container" >/dev/null 2>&1 || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

sed 's/__PUBLIC_HOST__/localhost/g' "$repo_dir/infra/hosting/Caddyfile" > "$tmp/Caddyfile"
container="$(docker run --detach --publish 127.0.0.1::443 \
  --volume "$tmp/Caddyfile:/etc/caddy/Caddyfile:ro" "$image")"
port="$(docker port "$container" 443/tcp | awk -F: 'NR == 1 {print $NF}')"
[[ "$port" =~ ^[0-9]+$ ]]

status=''
for _ in {1..40}; do
  status="$(curl --insecure --silent --output /dev/null --write-out '%{http_code}' \
    --resolve "localhost:$port:127.0.0.1" \
    --header "X-API-Key: $canary" \
    "https://localhost:$port/mcp" || true)"
  [[ "$status" = 502 ]] && break
  sleep 0.25
done
[[ "$status" = 502 ]] || {
  echo 'could not induce the expected Caddy reverse-proxy failure' >&2
  exit 1
}

docker logs "$container" > "$tmp/caddy.log" 2>&1
if grep -Fq "$canary" "$tmp/caddy.log"; then
  echo 'Caddy error logging exposed the request-header canary' >&2
  exit 1
fi
python3 - "$tmp/caddy.log" <<'PY'
import json
import pathlib
import sys

entries = []
for line in pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines():
    try:
        entry = json.loads(line)
    except json.JSONDecodeError:
        continue
    if entry.get("logger") == "http.log.error":
        entries.append(entry)
if not entries:
    raise SystemExit("Caddy emitted no reverse-proxy error record")
if any("request" in entry for entry in entries):
    raise SystemExit("Caddy reverse-proxy error retained its request object")
PY

echo 'Caddy runtime error logs omit request objects and credentials'
