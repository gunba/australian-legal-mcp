#!/usr/bin/env bash
# End-to-end install smoke test for a published Rust release/corpus manifest.
#
# Usage:
#   ATO_MCP_MANIFEST_URL=https://.../manifest.json scripts/smoke-rust-install.sh
#
# Optional:
#   ATO_MCP_BIN=/path/to/ato-mcp
#   ATO_MCP_SMOKE_QUERY="research and development tax incentive eligibility"
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ATO_MCP_BIN:-$ROOT/target/release/ato-mcp}"
MANIFEST_URL="${ATO_MCP_MANIFEST_URL:-}"
QUERY="${ATO_MCP_SMOKE_QUERY:-research and development tax incentive eligibility}"

if [ ! -x "$BIN" ]; then
  cargo build --release --locked --manifest-path "$ROOT/Cargo.toml"
fi

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT
export ATO_MCP_DATA_DIR="$WORKDIR/data"

if [ -n "$MANIFEST_URL" ]; then
  "$BIN" update --manifest-url "$MANIFEST_URL"
else
  "$BIN" update
fi

"$BIN" doctor
"$BIN" stats --format json > "$WORKDIR/stats.json"
"$BIN" search "$QUERY" --k 3 --format json > "$WORKDIR/hybrid-search.json"
"$BIN" search "section 8-1 repairs" --mode keyword --k 3 --format json > "$WORKDIR/keyword-search.json"

echo "smoke ok: $ATO_MCP_DATA_DIR"
