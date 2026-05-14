#!/usr/bin/env bash
# Build a pre-populated Rust data directory as a tar.zst for air-gapped installs.
#
# This intentionally runs `ato-mcp update` against a local mirror of the release
# manifest/packs/model bundle. That keeps the offline DB shape identical to a
# normal Rust install, including chunk_embeddings.
#
# Usage:
#   scripts/make-offline-bundle.sh
#   scripts/make-offline-bundle.sh ./out.tar.zst
#
# Env overrides:
#   ATO_MCP_REPO_DIR       default: $(pwd)
#   ATO_MCP_RELEASE_DIR    default: $REPO_DIR/release
#   ATO_MCP_BIN            default: $REPO_DIR/target/release/ato-mcp
#   ATO_MCP_MODEL_BUNDLE   optional existing embeddinggemma-bundle.tar.zst
#   ATO_MCP_MODEL_DIR      used to build the bundle when ATO_MCP_MODEL_BUNDLE is unset
set -euo pipefail

REPO_DIR="${ATO_MCP_REPO_DIR:-$(pwd)}"
RELEASE_DIR="${ATO_MCP_RELEASE_DIR:-$REPO_DIR/release}"
BIN="${ATO_MCP_BIN:-$REPO_DIR/target/release/ato-mcp}"
MODEL_DIR="${ATO_MCP_MODEL_DIR:-$REPO_DIR/models/embeddinggemma}"
OUT="${1:-$RELEASE_DIR/ato-mcp-offline-bundle.tar.zst}"
mkdir -p "$(dirname "$OUT")"
rm -f "$OUT" "${OUT}.part"*.bin

for f in "$RELEASE_DIR/manifest.json" "$BIN"; do
  [ -e "$f" ] || { echo "missing: $f" >&2; exit 1; }
done
[ -d "$RELEASE_DIR/packs" ] || { echo "missing: $RELEASE_DIR/packs" >&2; exit 1; }

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT
MIRROR="$WORKDIR/mirror"
DATA_DIR="$WORKDIR/data"
mkdir -p "$MIRROR/packs" "$DATA_DIR"

cp "$RELEASE_DIR/manifest.json" "$MIRROR/manifest.json"
cp "$RELEASE_DIR"/packs/pack-*.bin.zst "$MIRROR/packs/"

MODEL_BUNDLE="${ATO_MCP_MODEL_BUNDLE:-}"
if [ -z "$MODEL_BUNDLE" ] && [ -f "$RELEASE_DIR/embeddinggemma-bundle.tar.zst" ]; then
  MODEL_BUNDLE="$RELEASE_DIR/embeddinggemma-bundle.tar.zst"
fi

if [ -n "$MODEL_BUNDLE" ]; then
  [ -f "$MODEL_BUNDLE" ] || { echo "missing: $MODEL_BUNDLE" >&2; exit 1; }
  cp "$MODEL_BUNDLE" "$MIRROR/embeddinggemma-bundle.tar.zst"
else
  MODEL_STAGE="$WORKDIR/model-stage"
  mkdir -p "$MODEL_STAGE"
  for name in model_quantized.onnx model_quantized.onnx_data tokenizer.json; do
    if [ -f "$MODEL_DIR/$name" ]; then
      cp "$MODEL_DIR/$name" "$MODEL_STAGE/$name"
    elif [ -f "$MODEL_DIR/onnx/$name" ]; then
      cp "$MODEL_DIR/onnx/$name" "$MODEL_STAGE/$name"
    else
      echo "missing model file $name under $MODEL_DIR" >&2
      exit 1
    fi
  done
  tar --sort=name --mtime='2026-01-01 UTC' -C "$MODEL_STAGE" -cf - . \
    | zstd -T0 -3 -o "$MIRROR/embeddinggemma-bundle.tar.zst"
fi

"$BIN" bundle-localize-manifest \
  --manifest     "$MIRROR/manifest.json" \
  --packs-dir    "$MIRROR/packs" \
  --model-bundle "$MIRROR/embeddinggemma-bundle.tar.zst"

ATO_MCP_DATA_DIR="$DATA_DIR" "$BIN" update --manifest-url "$MIRROR/manifest.json"
ATO_MCP_DATA_DIR="$DATA_DIR" "$BIN" doctor

if command -v sqlite3 >/dev/null 2>&1; then
  sqlite3 "$DATA_DIR/live/ato.db" 'PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;'
fi
rm -f "$DATA_DIR/LOCK" "$DATA_DIR/live/ato.db-shm" "$DATA_DIR/live/ato.db-wal"
rm -rf "$DATA_DIR/backups" "$DATA_DIR/staging"

tar --sort=name --mtime='2026-01-01 UTC' -C "$DATA_DIR" -cf - . | zstd -T0 -10 -o "$OUT"

SPLIT_THRESHOLD=$((1900 * 1024 * 1024))
SIZE=$(stat -c%s "$OUT" 2>/dev/null || stat -f%z "$OUT")
if [ "$SIZE" -gt "$SPLIT_THRESHOLD" ]; then
  echo "bundle > 1.9 GiB; splitting into parts"
  split --bytes=1900M --numeric-suffixes=1 --additional-suffix=.bin "$OUT" "${OUT}.part"
  rm -f "$OUT"
  ls -lh "${OUT}.part"*
else
  echo "bundle: $OUT"
  du -h "$OUT"
fi
