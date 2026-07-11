# Maintainer Runbook

End users install a released Rust binary and use `ato-mcp update`. Corpus
publication runs only from the maintainer checkout and requires GPU-backed
embeddings.

## Release contracts

Binary tags are immutable `vX.Y.Z` tags. The manual binary workflow checks out
the supplied tag, requires an exact tagged checkout, and requires `X.Y.Z` to
match the Cargo package version. It publishes these verified archives plus
`SHA256SUMS`:

- `ato-mcp-x86_64-unknown-linux-gnu.tar.gz` (glibc 2.27 or newer)
- `ato-mcp-aarch64-apple-darwin.tar.gz`
- `ato-mcp-x86_64-pc-windows-msvc.zip`

The Linux executable targets glibc 2.17 and enables the release-only
`vendored-openssl` feature so it does not depend on the runner's OpenSSL ABI. The
archive bundles ONNX Runtime, whose official build sets the complete archive
baseline to glibc 2.27. Its Zig build requires `make`, Perl, and the `Time::Piece`
module; the workflow installs those prerequisites.

Corpus discovery is asset-based and paginated. The updater reads GitHub releases
in API order, 100 at a time, ignores drafts and prereleases, and selects the
first release containing an asset named exactly `manifest.json`. Manifest
format 2 requires `model`,
`db`, and `ann`. The database descriptor identifies `ato.db.zst` by
URL, SHA-256, and size. The ANN descriptor binds `ato.ann` to the exact model,
dimension, corpus identity, ordered embedding-set digest, construction
parameters, sidecar format, size, and SHA-256.

The publication invariant is database first, ANN sidecar second, manifest
last. Upload and verify `ato.db.zst` and then `ato.ann` before making the
manifest discoverable. Upload
`manifest.json.minisig`, when used, before `manifest.json`. The self-hosted GPU
workflow resolves the current latest binary release and adds the corpus assets
to that release.

## Weekly corpus release

Build the maintainer binary with CUDA and run the steady-state script:

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

The script refreshes the source index, skips an unchanged incremental run,
builds a fresh `ato.db` and `ato.ann`, packages `ato.db.zst`, and delegates publication to
`scripts/publish-release.sh`. `ATO_MCP_MODE` accepts `incremental`, `catch_up`,
or `full`; a full run always rebuilds. Set `ATO_MCP_FORCE_REBUILD=1` for a
schema-only or model-only publication.

The build requires `tokenizer.json`, `onnx/model_fp16.onnx`, and
`onnx/model_fp16.onnx_data` under `ATO_MCP_MODEL_DIR`, plus an ONNX Runtime 1.20
or newer shared library. CUDA builds require a CUDA-enabled ONNX Runtime; set
`ORT_DYLIB_PATH` to its `libonnxruntime.so` when it is not the system default.
Fix CUDA or `CUDAExecutionProvider` failures rather than publishing a degraded
corpus. Set `ATO_MCP_CUDA_LIB_PATH` when the CUDA runtime libraries are outside
the normal loader path.

For an approved model mirror, set `ATO_MCP_MODEL_URL`. A Hugging Face value must
use `hf://repo@revision`. Other URLs also require
`ATO_MCP_MODEL_SHA256` and positive `ATO_MCP_MODEL_SIZE`. Manifest signing uses
`ATO_MCP_SIGN_KEY` and requires `minisign` on `PATH`.

## Manual corpus publication

After a successful local `ato-mcp build`:

```bash
scripts/publish-release.sh vX.Y.Z gunba/ato-mcp
```

The script packages the canonical database without mutating it and invokes the
Rust publisher. The publisher verifies and uploads `ato.db.zst`, `ato.ann`, the
optional signature, and finally `manifest.json`, stopping before discovery if
any earlier artifact fails. The script then promotes the release to latest.

## Offline bundle

Create an air-gapped data bundle only from the current database artifact
format:

```bash
ATO_MCP_RELEASE_DIR="$PWD/release" \
ATO_MCP_MODEL_DIR="$PWD/models/granite-embedding-small-r2" \
scripts/make-offline-bundle.sh release/ato-mcp-offline-bundle.tar.zst
```

The release directory must contain `manifest.json` and the matching
`ato.db.zst` and `ato.ann`. The script localizes the database, ANN, and model
URLs, verifies their digests and sizes, installs through `ato-mcp update
--manifest-url`, and archives the resulting data directory. Extract the archive
into the platform default data directory, or into another stable directory that
will always be supplied as `ATO_MCP_DATA_DIR`.

## Systemd operations

The end-user update service runs `update`, tries to restart the optional backend
service, and runs `stats` against the refreshed data. Install the update and
serve units together when automatic refresh is desired. A custom data directory
must be set identically in both units with a systemd environment override.

The maintainer GPU workflow is manual and targets a self-hosted Linux x64 runner.
It checks `nvidia-smi` before building with the `cuda` feature. The local
maintainer timer provides the same release path without hosted GPU spend.

## Verification

```bash
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
bash -n scripts/*.sh
ATO_MCP_DATA_DIR=/stable/test/data scripts/smoke.sh
```

Before announcing a corpus release, verify:

```bash
gh release view vX.Y.Z --repo gunba/ato-mcp \
  --json assets --jq '.assets[].name'
ato-mcp stats
ato-mcp search "research and development tax incentive" --k 1
```

The release must contain matching `ato.db.zst`, `ato.ann`, and `manifest.json` assets, and
`manifest.json` must be the last publication step.

## ANN implementation contract

The sidecar uses pinned `arroy 0.6.4` with its full-precision cosine forest on
LMDB. Arroy is maintained by Meilisearch, supports query-time RoaringBitmap
candidate filters, accepts a seeded build, and reads through LMDB's memory map
on Linux, macOS, and Windows. It avoids USearch's C++ runtime/ABI surface and
the native-endian, pointer-width persistence in `hnsw_rs`. The build fixes the
ChaCha12 algorithm and crate version, seed, tree/split parameters, insertion
order, and Rayon thread count; CI builds and verifies the embedded contract,
candidate ordering, and filtered-result behavior on all three release platforms.

SQLite signed-int8 embeddings remain authoritative. ANN discovers candidates;
runtime search exact-reranks them with `dot_i8`, uses stable chunk-ID ties, and
falls back to an exact eligible scan whenever deterministic widening cannot
fill the requested candidate pool. The installed-corpus benchmark defaults to
1,000 candidates and validates at least 0.99 recall@50 before publication.

Format-2 installs are assembled under `generations/<install-id>/` with the
database placed first, ANN sidecar second, model files next, and installed
manifest last. A durable atomic replacement of `active-generation` is the only
activation step. A crash before it leaves the previous generation active; a
crash after it leaves the new complete generation active. Incomplete staging
and inactive generations are cleaned on later successful updates.
