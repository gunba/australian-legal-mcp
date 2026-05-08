# Maintainer Runbook

End users install the Rust binary and never run the Python maintainer
commands. This file is for corpus publication only.

## What Runs Where

- Rust binary: end-user CLI, updater, MCP server, search/fetch tools.
- Python maintainer package: scrape, extract, chunk, GPU-backed embedding
  build, pack generation, GitHub release upload.
- GitHub Actions: cheap CI, cross-platform Rust binary release assets, and
  an optional self-hosted GPU release workflow.

The corpus build must use GPU-backed embeddings. If there is no suitable
GitHub GPU runner, keep the build local. Do not silently fall back to a CPU
or keyword-only release build.

## Weekly Local Release

```bash
cd /path/to/ato-mcp

python -m venv .venv
.venv/bin/pip install -e '.[dev]'
# Swap CPU onnxruntime for the GPU build (end-users never load this Python
# package — they ship with the Rust binary — so the heavier wheel is
# isolated to the maintainer venv). Both packages provide the same
# ``onnxruntime`` module name, so the CPU one must come off first or it
# clobbers the GPU build.
.venv/bin/pip uninstall -y onnxruntime
.venv/bin/pip install -e '.[gpu]'

ATO_MCP_MODE=incremental \
ATO_MCP_REPO_DIR="$PWD" \
ATO_MCP_PAGES_DIR="/path/to/ato_pages" \
ATO_MCP_MODEL_DIR="$PWD/models/embeddinggemma" \
ATO_MCP_RERANKER_BUNDLE="$PWD/models/reranker" \
ATO_MCP_RERANKER_URL="hf://Alibaba-NLP/gte-reranker-modernbert-base@<revision-sha>" \
ATO_MCP_GH_REPO=gunba/ato-mcp \
scripts/maintainer-sync.sh
```

`scripts/maintainer-sync.sh` will:

1. Refresh `ato_pages` in the requested mode, then always run the
   incremental What's New refresh as the final pre-build source step.
2. Build `release/<tag>/ato.db`, packs, and `manifest.json`.
3. Write the pinned Hugging Face EmbeddingGemma source into the manifest,
   unless `ATO_MCP_MODEL_URL` points at an approved mirror.
4. Add the optional reranker manifest entry when `ATO_MCP_RERANKER_BUNDLE`
   or the explicit `ATO_MCP_RERANKER_*` env vars are set.
5. Upload the corpus assets with `.venv/bin/ato-mcp release`.
6. Mark the release latest.

Weekly release runs should use `ATO_MCP_MODE=incremental`. The full-tree
`catch_up` mode is for proving genuinely missing canonical IDs after a
long outage; it is not an empty-shell retry queue. Empty shells are pages the
ATO served without extractable body content and should be treated as
diagnostics unless a live document URL is known to have gained content.

The script skips rebuilds when the refreshed `index.jsonl` hash is unchanged,
except in `ATO_MCP_MODE=full`. Set `ATO_MCP_FORCE_REBUILD=1` for schema,
model, or reranker-only publications. After a successful publication,
`release/.latest` points at the whole release directory so the next
incremental run can reuse both `manifest.json` and prior `packs/`.

The script requires `nvidia-smi`/CUDA to be available through the local
Python ONNX Runtime install. If CUDA is unavailable, fix the environment
instead of publishing a degraded corpus.

Manifest signing with `--sign-key` requires the `minisign` CLI on `PATH`.

## Optional GPU Workflow

`.github/workflows/corpus-release-gpu.yml` targets:

```yaml
runs-on: [self-hosted, linux, x64, gpu]
```

It fails before scraping if `nvidia-smi` or ONNX Runtime's
`CUDAExecutionProvider` is missing. It is manual-only by default to avoid
hosted GPU spend.

## Binary Release Assets

`.github/workflows/release-binaries.yml` builds and uploads:

- `ato-mcp-x86_64-unknown-linux-gnu.tar.gz`
- `ato-mcp-aarch64-apple-darwin.tar.gz`
- `ato-mcp-x86_64-pc-windows-msvc.zip`

Run it by pushing a `v*` tag or via `workflow_dispatch`.

## Manual Corpus Publication

After a local `build-index`:

```bash
jq '.packs | length' release/manifest.json
scripts/publish-release.sh v0.3.0 gunba/ato-mcp
```

Set `ATO_MCP_MODEL_URL` only when publishing against an approved model mirror.
By default the manifest points at pinned Hugging Face EmbeddingGemma files.
This uploads manifest and packs to GitHub Releases; it does not upload the
model to GitHub, build Python wheels, or duplicate the corpus into an offline
bundle by default. Do not publish DB-derived repacks.

For an explicit air-gapped install package:

```bash
scripts/make-offline-bundle.sh release/ato-mcp-offline-v0.3.0.tar.zst
```

The offline bundle script runs the Rust installer against a local mirror of
the manifest, packs, and model bundle, then packages the resulting data
directory. Do not build offline bundles by copying `release/ato.db` directly.

## Reranker model preparation

Wave 3 (0.6.0+) introduces an optional cross-encoder reranker that the Rust
runtime applies to the top-N hybrid candidates. The preferred model is
`Alibaba-NLP/gte-reranker-modernbert-base`, using the quantized ONNX export
(~151 MB on disk).

The reranker is **optional**. A release built without `--reranker-bundle`
leaves the manifest's `reranker` field `null` and the runtime falls back to
the un-reranked hybrid score. End-user binaries continue to work; only the
result quality drops.

### One-off: build the bundle

```bash
mkdir -p ./reranker_bundle/onnx
curl -fL -o ./reranker_bundle/onnx/model_quantized.onnx \
    https://huggingface.co/Alibaba-NLP/gte-reranker-modernbert-base/resolve/<revision-sha>/onnx/model_quantized.onnx
curl -fL -o ./reranker_bundle/tokenizer.json \
    https://huggingface.co/Alibaba-NLP/gte-reranker-modernbert-base/resolve/<revision-sha>/tokenizer.json
```

The output directory contains:

- `onnx/model_quantized.onnx` (~151 MB) — the quantized weights
- `tokenizer.json` (~3.5 MB) — the tokenizer
- `config.json` — model architecture metadata

### Hosting

Mirror the EmbeddingGemma pattern: pin to a specific Hugging Face revision
URL so the Rust client always fetches the same artifact bytes. The manifest
records this URL via `--reranker-url`. Format:

```
hf://Alibaba-NLP/gte-reranker-modernbert-base@<revision-sha>
```

The Rust client tries the following filenames in order under that revision
(first one whose download matches the manifest's `reranker.sha256` wins):

1. `onnx/model_quantized.onnx` (preferred quantized output path)
2. `model_quantized.onnx` (root-level quantized alias)
3. `onnx/model.onnx` (canonical optimum-cli output path)
4. `model.onnx` (root-level alias)

This means you can host the bundle's `model_quantized.onnx` under any of
those names — the Python `_resolve_reranker_info` helper hashes whichever
one exists in your local bundle, and the Rust runtime walks the same list
on download. **Critical:** if you re-quantize with a different optimum-cli
version that produces a different filename, just upload it under one of
the recognised names and re-run `ato-mcp release`. Do **not** rename the
file — the sha will diverge from the manifest.

The Rust client also downloads `tokenizer.json` from the same revision and
renames the pair to `live/reranker.onnx` and `live/reranker_tokenizer.json`
on disk. When `--reranker-bundle` includes a `tokenizer.json`, its sha256
is auto-derived into `reranker.tokenizer_sha256` so the runtime can
verify it byte-for-byte. Manifests built before this field landed (or
publishers who omit `tokenizer.json` from the bundle) skip tokenizer
verification with a one-line warning.

When publishing a new revision, update the URL revision sha and re-run
`ato-mcp release` with the new bundle so the manifest's sha256 advances.

### Release CLI invocation

```bash
ato-mcp release \
    --out-dir ./release \
    --tag index-2026.05.06 \
    --repo gunba/ato-mcp \
    --model-dir ./models/embeddinggemma \
    --reranker-bundle ./reranker_bundle \
    --reranker-url 'hf://Alibaba-NLP/gte-reranker-modernbert-base@<revision-sha>' \
    --overwrite
```

The `--reranker-bundle` flag computes sha256 + size from
`reranker_bundle/model_quantized.onnx` (or any of the recognised filenames
listed above) automatically; pass `--reranker-sha256` / `--reranker-size`
only to override the auto-computed values (rare — used when re-packaging
the bundle).

If your bundle includes `tokenizer.json`, its sha256 is also auto-derived
and embedded into `reranker.tokenizer_sha256`. Pass
`--reranker-tokenizer-sha256` to override or to set it explicitly when the
manifest points at an HF revision whose tokenizer you've vetted out of
band.

The bundle itself is **not** uploaded to GitHub Releases; only its
fingerprint goes into `manifest.json`. The Rust runtime fetches the actual
ONNX from the Hugging Face URL on first use.

## Health Checks

```bash
ato-mcp stats
ato-mcp doctor
cargo test --locked
.venv/bin/pytest -q
```

Watch for:

- Zero new rows for several weekly incremental runs while the live What's New
  feed has changed.
- Growing failed rows in `ato_pages/index.jsonl`.
- `ato-mcp doctor` failures after update.
- Missing `CUDAExecutionProvider` before a release build.

### CLI search mirrors the MCP server pipeline

`ato-mcp search "..."` from the CLI runs the same `search()` code path the
MCP server does, including the cross-encoder reranker when its model files
are installed under `live/`. JSON output reports `ranking.reranker_used:
true` in that case (and `false` otherwise — typically when `--release` was
shipped without `--reranker-bundle`, or when `ATO_MCP_DISABLE_RERANKER=1`
is set in the environment).

This means CLI latency benchmarks reflect production behaviour. To
A/B compare RRF-only vs. reranker-on, run the same query twice with and
without `ATO_MCP_DISABLE_RERANKER=1` in the environment.

## Do Not

- Do not hand-edit `index.jsonl`.
- Do not delete published packs referenced by a manifest.
- Do not publish a corpus built without GPU-backed embeddings.
- Do not paste or print local tokens in logs or release notes.
