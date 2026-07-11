#!/usr/bin/env bash
# Build a complete ato-mcp data directory for a stable, air-gapped install.
#
# Usage:
#   scripts/make-offline-bundle.sh [output.tar.zst]
#
# The release directory must contain the current schema manifest.json and its
# single manifest.db artifact, ato.db.zst. The resulting archive is extracted
# into a stable directory and that directory is supplied as ATO_MCP_DATA_DIR to
# every future ato-mcp command.
set -euo pipefail

REPO_DIR="${ATO_MCP_REPO_DIR:-$(pwd)}"
RELEASE_DIR="${ATO_MCP_RELEASE_DIR:-$REPO_DIR/release}"
BIN="${ATO_MCP_BIN:-$REPO_DIR/target/release/ato-mcp}"
MODEL_DIR="${ATO_MCP_MODEL_DIR:-$REPO_DIR/models/granite-embedding-small-r2}"
OUT="${1:-$RELEASE_DIR/ato-mcp-offline-bundle.tar.zst}"

for command in python3 tar zstd; do
	command -v "$command" >/dev/null 2>&1 || {
		echo "missing command: $command" >&2
		exit 1
	}
done
for file in "$RELEASE_DIR/manifest.json" "$RELEASE_DIR/ato.db.zst" "$BIN"; do
	[ -e "$file" ] || {
		echo "missing: $file" >&2
		exit 1
	}
done
[ -x "$BIN" ] || {
	echo "not executable: $BIN" >&2
	exit 1
}

mkdir -p "$(dirname "$OUT")"
rm -f "$OUT" "${OUT}.part"*.bin
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT
MIRROR="$WORKDIR/release"
DATA_DIR="$WORKDIR/data"
mkdir -p "$MIRROR" "$DATA_DIR"
cp "$RELEASE_DIR/manifest.json" "$RELEASE_DIR/ato.db.zst" "$MIRROR/"
if python3 -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("ann") else 1)' \
	"$RELEASE_DIR/manifest.json"; then
	[ -f "$RELEASE_DIR/ato.ann" ] || {
		echo "manifest requires missing $RELEASE_DIR/ato.ann" >&2
		exit 1
	}
	cp "$RELEASE_DIR/ato.ann" "$MIRROR/"
fi

MODEL_BUNDLE="${ATO_MCP_MODEL_BUNDLE:-}"
if [ -z "$MODEL_BUNDLE" ] && [ -f "$RELEASE_DIR/semantic-model-bundle.tar.zst" ]; then
	MODEL_BUNDLE="$RELEASE_DIR/semantic-model-bundle.tar.zst"
fi
if [ -n "$MODEL_BUNDLE" ]; then
	[ -f "$MODEL_BUNDLE" ] || {
		echo "missing: $MODEL_BUNDLE" >&2
		exit 1
	}
	cp "$MODEL_BUNDLE" "$MIRROR/semantic-model-bundle.tar.zst"
else
	MODEL_STAGE="$WORKDIR/model-stage"
	mkdir -p "$MODEL_STAGE"
	for name in model_fp16.onnx model_fp16.onnx_data tokenizer.json; do
		if [ -f "$MODEL_DIR/$name" ]; then
			cp "$MODEL_DIR/$name" "$MODEL_STAGE/$name"
		elif [ -f "$MODEL_DIR/onnx/$name" ]; then
			cp "$MODEL_DIR/onnx/$name" "$MODEL_STAGE/$name"
		else
			echo "missing model file $name under $MODEL_DIR" >&2
			exit 1
		fi
	done
	tar --sort=name --mtime='2026-01-01 UTC' --owner=0 --group=0 --numeric-owner \
		-C "$MODEL_STAGE" -cf - . | zstd -T0 -3 -o "$MIRROR/semantic-model-bundle.tar.zst"
fi

python3 - "$MIRROR/manifest.json" "$MIRROR/ato.db.zst" "$MIRROR/semantic-model-bundle.tar.zst" "$MIRROR/ato.ann" <<'PY'
import hashlib, json, pathlib, sys
manifest_path, db_path, model_path, ann_path = map(pathlib.Path, sys.argv[1:])
manifest = json.loads(manifest_path.read_text())
required = {"schema_version", "index_version", "created_at", "min_client_version", "model", "db"}
if not required.issubset(manifest) or set(manifest) - required - {"ann"}:
    raise SystemExit("manifest has an unsupported contract")
def describe(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest(), path.stat().st_size
db_sha, db_size = describe(db_path)
if manifest["db"].get("sha256") != db_sha or manifest["db"].get("size") != db_size:
    raise SystemExit("manifest.db does not match ato.db.zst")
manifest["db"]["url"] = db_path.name
if ann := manifest.get("ann"):
    if not ann_path.is_file():
        raise SystemExit("manifest requires a missing ANN sidecar")
    ann_sha, ann_size = describe(ann_path)
    if ann.get("sha256") != ann_sha or ann.get("size") != ann_size:
        raise SystemExit("manifest.ann does not match ato.ann")
    ann["url"] = ann_path.name
model_sha, model_size = describe(model_path)
manifest["model"].update(url=model_path.name, sha256=model_sha, size=model_size)
manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
PY

ATO_MCP_DATA_DIR="$DATA_DIR" "$BIN" update --manifest-url "$MIRROR/manifest.json"
ATO_MCP_DATA_DIR="$DATA_DIR" "$BIN" stats >/dev/null
rm -f "$DATA_DIR/LOCK"
find "$DATA_DIR" -type f \( -name 'ato.db-shm' -o -name 'ato.db-wal' \) -delete
rm -rf "$DATA_DIR/backups" "$DATA_DIR/staging"

tar --sort=name --mtime='2026-01-01 UTC' --owner=0 --group=0 --numeric-owner \
	-C "$DATA_DIR" -cf - . | zstd -T0 -10 -o "$OUT"

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
