# Maintainer Runbook

Australian Legal MCP ships one Rust executable, `legal-mcp`. End users install
the released binary and activate verified corpus generations with
`legal-mcp update`. Source acquisition, embedding, corpus construction and
publication run from the maintainer checkout.

## Release contract

Release tags are immutable `vX.Y.Z` tags. The package version is
`australian-legal-mcp`; the tag version, Cargo package version, plugin/package
metadata and release archive version must agree. Platform archives contain the
`legal-mcp` executable and the matching ONNX Runtime library:

- `legal-mcp-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz`
- `legal-mcp-X.Y.Z-aarch64-apple-darwin.tar.gz`
- `legal-mcp-X.Y.Z-x86_64-pc-windows-msvc.zip`

The Linux executable targets glibc 2.17 and uses vendored OpenSSL. The packaged
ONNX Runtime sets the complete Linux archive baseline to glibc 2.27. Publish
`SHA256SUMS` for every platform archive.

Corpus discovery is release-asset based and paginated. The updater examines
GitHub releases in API order, ignores drafts and prereleases, and selects the
newest release containing `manifest.json`. The manifest declares:

- the embedding model artifacts, identity, digests and sizes;
- the compressed database installed as `legal.db`;
- a source-keyed ANN descriptor for every registered source, installed at
  `ann/<source>.ann`;
- the shared corpus identity and source generations;
- exact URLs, byte sizes and SHA-256 values for every artifact.

Each ANN descriptor binds its sidecar to the source, model, dimensions, ordered
embedding-set digest, construction parameters, corpus identity and artifact
digest. Semantic readiness requires a validated sidecar for the selected source.

Publish into a fresh draft release, verify data artifacts, upload
`manifest.json` last, and then publish the non-latest corpus release. The
manifest is the discoverable commit point. An install
assembles a complete immutable directory under `generations/<generation-id>/`;
durable atomic replacement of `active-generation` activates it.

## Source update contract

Source adapters run concurrently with independent pacing, retry, cursor and
failure state. A successful adapter produces a durable inventory and normalized
documents. A failed adapter retains its last publishable state while other
source
jobs complete. Cursor advancement follows durable acquisition and validation.

Every publication writes a fresh `legal.db` from validated source inputs. For
each source transaction:

1. compare the incoming authoritative inventory with the selected published
   inventory;
2. directly delete source documents absent from the incoming inventory;
3. normalize and rechunk new or changed documents;
4. reuse embeddings whose approved model and chunk-text hash match;
5. write source-qualified documents, chunks, definitions, anchors, citations and
   assets;
6. rebuild `ann/<source>.ann`;
7. run integrity, retrieval and ANN recall checks.

SQLite signed-int8 embeddings remain authoritative. ANN selects candidates;
runtime search exact-reranks with the SQLite vectors and stable chunk-reference
ties.

### ATO source

The routine ATO workspace is
`/home/jordan/Desktop/Projects/ato_pages`. Use its integrity-pinned
`index.jsonl`
and payload tree directly. Normal updates run the ATO What's New discovery and
fetch changed links under one shared 50 ms issue interval, four workers and a
30-second request timeout. Verify every selected payload's declared size and
SHA-256 before normalization.

A full ATO acquisition is an explicitly authorised repair operation. The routine
path preserves the existing source workspace and its stable native identities.

### Federal Register source

FRL acquisition uses the official API root
`https://api.prod.legislation.gov.au/v1/` and metadata contract at
`https://api.prod.legislation.gov.au/v1/$metadata`.

- Initial and periodic authoritative reconciliation pages `Titles`, ordered by
  `id`, with `$top` no greater than 100 and stable keyset boundaries.
- Incremental discovery pages `Versions` inclusively from a seven-day overlap
  boundary, ordered by `registeredAt`, `titleId`, `start` and
  `retrospectiveStart`.
- The persisted cursor contains that full ordering tuple; overlap records dedupe
  by stable identity.
- `titleId` is the native document ID, the version tuple identifies a version,
  and `registerId` records registration provenance.
- `Documents` is filtered to the selected version and ordered by the remaining
  rendition key.
- Official authorised rendition preference is EPUB, then DOCX, then official
  extracted PDF text.
- The initial rate policy is two concurrent operations, a 250 ms issue interval,
  a 30-second request timeout and bounded exponential backoff with jitter.

A completed authoritative `Titles` reconciliation directly deletes FRL titles
outside the selected current inventory.

## Routine corpus publication

Build the maintainer executable with CUDA and run the steady-state workflow from
the repository root:

```bash
cd /path/to/australian-legal-mcp
cargo build --release --features cuda

LEGAL_MCP_ATO_MODE=incremental \
LEGAL_MCP_REPO_DIR="$PWD" \
LEGAL_MCP_ATO_PAGES_DIR="/home/jordan/Desktop/Projects/ato_pages" \
LEGAL_MCP_FRL_DIR="/stable/workspaces/frl" \
LEGAL_MCP_MODEL_DIR="$PWD/models/granite-embedding-small-r2" \
LEGAL_MCP_GH_REPO=gunba/australian-legal-mcp \
scripts/maintainer-sync.sh
```

The workflow acquires `ato` and `frl` through the registered adapters, skips
publication when every source and corpus contract is unchanged, builds a fresh
`legal.db`, writes `ann/ato.ann` and `ann/frl.ann`, validates the generation,
packages it and delegates final publication to `scripts/publish-release.sh`.
FRL full acquisition persists verified per-title cache entries as it progresses,
so a repeated source update resumes completed renditions before committing the
next authoritative state.

`LEGAL_MCP_ATO_MODE=incremental` is the routine mode. Use the explicit full or
catch-up repair mode for an authorised source repair. Set
`LEGAL_MCP_FORCE_REBUILD=1` for a deliberate schema, chunker or model-only
publication.
Set `LEGAL_MCP_EMBEDDING_CACHE_DB` to the prior completed schema-v10 `legal.db`
to seed model-and-text-qualified embeddings into the fresh generation.

The model directory contains `tokenizer.json`, `onnx/model_fp16.onnx` and
`onnx/model_fp16.onnx_data`. Use ONNX Runtime 1.20 or newer. CUDA builds require
a CUDA-enabled ONNX Runtime; point `ORT_DYLIB_PATH` at its shared library and
`LEGAL_MCP_CUDA_LIB_PATH` at additional CUDA libraries when required. Treat a
missing CUDA execution provider as a failed maintainer build.

For an approved model mirror, set `LEGAL_MCP_MODEL_URL`. A Hugging Face model
uses `hf://repo@revision`; other URLs also require
`LEGAL_MCP_MODEL_SHA256` and a positive `LEGAL_MCP_MODEL_SIZE`.

## Manual publication

After a successful local build and generation validation:

```bash
scripts/publish-release.sh corpus-YYYYMMDDThhmmssZ gunba/australian-legal-mcp
```

The publisher packages `legal.db` without mutating it, verifies each database,
source ANN and model artifact, uploads `manifest.json` last, and publishes the
completed draft as a non-latest release. Corpus release tags are immutable; each
build uses a fresh `corpus-<UTC timestamp>` tag.

## Offline bundle

Create an air-gapped bundle from one fully validated generation:

```bash
LEGAL_MCP_RELEASE_DIR="$PWD/release" \
LEGAL_MCP_MODEL_DIR="$PWD/models/granite-embedding-small-r2" \
scripts/make-offline-bundle.sh \
  release/legal-mcp-offline-bundle.tar.zst
```

Extract the bundle into the platform default `australian-legal-mcp` data
directory or another stable directory supplied consistently as
`LEGAL_MCP_DATA_DIR`. Verify it through the updater's local manifest path and
then run `legal-mcp stats`.

## Verification

Run the local release gate before announcing a binary or corpus release:

```bash
cargo fmt --all -- --check
cargo test --workspace --locked --all-features
cargo clippy --workspace --locked --all-targets --all-features -- -D warnings
cargo audit
cargo deny check advisories
bash -n scripts/*.sh
git diff --check
LEGAL_MCP_DATA_DIR=/stable/test/data scripts/smoke.sh
```

Verify release assets and both sources:

```bash
gh release view vX.Y.Z --repo gunba/australian-legal-mcp \
  --json assets --jq '.assets[].name'
legal-mcp stats
legal-mcp frl-probe
legal-mcp search "research and development tax incentive" --source ato --k 5
legal-mcp search "income tax assessment act" --source frl --k 5
legal-mcp fetch 'legal://ato/PAC%2F1'
```

Exercise the real tokenizer, ONNX model, source-qualified database and both ANN
sidecars with the bounded unified-generation fixture:

```bash
ORT_DYLIB_PATH=/path/to/libonnxruntime.so \
LEGAL_MCP_TEST_MODEL_DIR=/path/to/granite-embedding-small-r2 \
cargo test --locked --bin legal-mcp \
  frl::tests::real_ato_and_frl_fixtures_build_one_verified_generation \
  -- --ignored --exact
```

The release gate checks source isolation, canonical URI parsing, direct
authoritative deletion, failure isolation, ATO content quality, FRL fixture
normalization, per-source ANN identity and recall, exact reranking, manifest
digests and atomic activation.

## ANN implementation contract

The source sidecars use pinned Arroy with a full-precision cosine forest on
LMDB.
The build fixes the crate version, ChaCha12 algorithm, seed, tree and split
parameters, insertion order and Rayon thread count. CI verifies candidate
ordering and filtered-result behaviour on Linux, macOS and Windows.

The installed-corpus benchmark uses deterministic widening and requires at least
0.99 recall@50 before publication. Exact eligible scanning fills a candidate
pool
when widening reaches its bound, preserving deterministic search results.
