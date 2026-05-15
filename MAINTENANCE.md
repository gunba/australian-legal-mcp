# Maintainer Runbook

End users install the Rust binary and never run any of the publication
commands. This file is for corpus publication only.

## What Runs Where

- Rust binary: end-user CLI, updater, MCP server, search/fetch tools,
  **and** the entire maintainer pipeline (scrape, extract, chunk,
  embed, pack generation, GitHub release upload).
- GitHub Actions: cheap CI, cross-platform Rust binary release assets,
  and an optional self-hosted GPU corpus release workflow.

The corpus build must use GPU-backed embeddings. If there is no suitable
GitHub GPU runner, keep the build local. Do not silently fall back to a CPU
or keyword-only release build.

## Weekly Local Release

```bash
cd /path/to/ato-mcp
cargo build --release --features cuda

ATO_MCP_MODE=incremental \
ATO_MCP_REPO_DIR="$PWD" \
ATO_MCP_PAGES_DIR="/path/to/ato_pages" \
ATO_MCP_MODEL_DIR="$PWD/models/granite-embedding-small-r2" \
ATO_MCP_GH_REPO=gunba/ato-mcp \
scripts/maintainer-sync.sh
```

`scripts/maintainer-sync.sh` will:

1. Refresh `ato_pages` in the requested mode (incremental, catch_up, full),
   then always run the incremental What's New refresh as the final
   pre-build source step.
2. Build `release/<tag>/ato.db`, packs, and `manifest.json` via
   `ato-mcp build --profile`.
3. Build embeddings from `ATO_MCP_MODEL_DIR` and write the pinned Hugging
   Face Granite embedding source into the manifest, unless `ATO_MCP_MODEL_URL`
   points at an approved mirror.
4. Upload the corpus assets via `ato-mcp publish-release`.
5. Mark the release latest.

Weekly release runs should use `ATO_MCP_MODE=incremental`. The full-tree
`catch_up` mode is for proving genuinely missing canonical IDs after a
long outage; it is not an empty-shell retry queue. Empty shells are pages the
ATO served without extractable body content and should be treated as
diagnostics unless a live document URL is known to have gained content.

The script skips rebuilds when the refreshed `index.jsonl` hash is unchanged,
except in `ATO_MCP_MODE=full`. Set `ATO_MCP_FORCE_REBUILD=1` for schema or
model-only publications. Before rebuilding, the script validates that
`release/.latest` is a current-model base release the running binary can
parse. If the pointer is missing, it scans local corpus release directories
and, when possible, materializes a compatible published corpus release from
its manifest and pack assets without running the embedding model. If no base
release exists but an interrupted output directory has a matching checkpoint,
the script resumes that checkpoint, even if the date-derived release tag has
rolled over. After a successful publication, `release/.latest` points at the
whole release directory so the next incremental run passes it as
`--base-release-dir`; the builder copies the previous DB/packs, re-cleans
source docs to compare source-derived content hashes, and only re-embeds
documents whose cleaned content changed or whose doc_id is new. Build logs
include stage timing, embedding token throughput, padding efficiency, and
source-doc progress.

The script requires `nvidia-smi`/CUDA to be available so the Rust
binary's bundled `ort` runtime can use the CUDA execution provider. If
CUDA is unavailable, fix the environment instead of publishing a
degraded corpus.

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

After a local `ato-mcp build`:

```bash
jq '.packs | length' release/manifest.json
scripts/publish-release.sh v0.3.0 gunba/ato-mcp
```

Set `ATO_MCP_MODEL_URL` only when publishing against an approved model mirror.
For non-Hugging Face mirrors, also set `ATO_MCP_MODEL_SHA256` and
`ATO_MCP_MODEL_SIZE`. By default the manifest points at pinned Hugging Face
Granite embedding files.
This uploads manifest and packs to GitHub Releases; it does not upload the
model to GitHub or duplicate the corpus into an offline bundle by default.
Do not publish DB-derived repacks.

For an explicit air-gapped install package:

```bash
scripts/make-offline-bundle.sh release/ato-mcp-offline-v0.3.0.tar.zst
```

The offline bundle script runs the Rust installer against a local mirror of
the manifest, packs, and model bundle, then packages the resulting data
directory. Do not build offline bundles by copying `release/ato.db` directly.

## Health Checks

```bash
ato-mcp stats
ato-mcp doctor
cargo test --locked
```

Watch for:

- Zero new rows for several weekly incremental runs while the live What's New
  feed has changed.
- Growing failed rows in `ato_pages/index.jsonl`.
- `ato-mcp doctor` failures after update.
- Missing `CUDAExecutionProvider` before a release build.

### CLI Search

`ato-mcp search "..."` from the CLI runs the same `search()` code path the
MCP server does. CLI latency benchmarks therefore reflect production
hybrid search behaviour.

## Do Not

- Do not hand-edit `index.jsonl`.
- Do not delete published packs referenced by a manifest.
- Do not publish a corpus built without GPU-backed embeddings.
- Do not paste or print local tokens in logs or release notes.
