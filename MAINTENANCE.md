# Maintainer runbook

The RTX maintainer PC owns official-source acquisition, OCR, normalization,
embedding, ANN construction, corpus validation, and immutable activation.
Corpus and embedding-model artifacts are never packaged for or uploaded to
GitHub Releases. GitHub tags and releases contain software binaries only.

## Software release contract

Release tags are immutable `vX.Y.Z` tags on `main`; repository rules must block
tag update/deletion. Cargo, plugin/package metadata, archive names, and the tag
version must agree. The workflow binds every job to the validated commit and
uses Rust 1.95.0. Platform archives contain `legal-mcp`, ONNX Runtime 1.25.0,
its licence/notices, and run `verify-runtime` after extraction before one
non-replaceable release is created. Linux requires glibc 2.27+; Windows requires
the Microsoft Visual C++ 2015–2022 Redistributable. Publish and independently
verify `SHA256SUMS` for every archive.

The software tree is version 0.19.11. Immutable v0.19.10 release assets, OCI
digest, labels, runtime, and attestation were independently verified. V0.19.11
adds document-scoped FTS narrowing and a public-route-aware HarbourGrid probe;
it does not change the corpus. Its host-tool contract advances normally to
v0.19.11 and removes the completed one-time bridge surface; only the retained
immutable v0.19.10 bundle owns that historical recovery. The one-time
compatibility bridge kept production `/run` `noexec`, made only its two private
adapter file binds executable inside the recovery mount namespace, proved the
bounding, effective, inheritable, and permitted sets through `podman top`, and
let the unchanged v0.19.8 updater retire its own journal. Exact v0.19.10 host
tools then committed the paired flat-int8 v22/image cutover and retired their
journal. The service is public through authenticated Caddy routes, v22 is
active, Arroy v20 is the sole hosted rollback, and all four live capability sets
remain empty.

## Canonical local data

All persistent project data is beneath `data/`:

```text
sources/             current authoritative source workspaces
source-snapshots/    rollback, failed-refresh, and legacy stores
models/              pinned unpacked model inputs
builds/              resumable and inactive corpus builds
runtime/             active immutable local generations
cache/               disposable embedding and TensorRT acceleration
runs/                acquisition evidence and pending-generation journal
logs/                build/activation logs
validation/          retained validation-only layouts
archive/             non-canonical historical diagnostics
```

`LEGAL_MCP_PROJECT_DATA_DIR` may override that project root.
`LEGAL_MCP_DATA_DIR` is different: it selects only a runtime generation root,
such as `data/runtime` locally or `/var/lib/australian-legal-mcp` on the VM.

## Source acquisition contract

The production catalogue is:

```text
ato
frl
federal-court
high-court
nsw-caselaw
nsw-legislation
qld-legislation
wa-legislation
sa-legislation
tas-legislation
```

Each authoritative workspace is flat: `state.json`, `documents/`, `assets/`,
and temporary `staging/`. Acquisition takes an exclusive workspace lock; builds
hold shared locks across the exact ten-source set. Empty inventories, duplicate
identities, unsafe URLs, catastrophic shrinkage, and less than 99% usable full
text are rejected. Stable authoritative 404s may be omitted; broad failures
abort. A failed source retains its last committed state while independent
sources finish.

`source-update` runs requested adapters concurrently. Each adapter owns
incremental/full discovery, pacing, retries, identity, inventory, and omission
policy. A reused `data/runs/<run>` allows validated discovery plans and staging
to resume. Full repair uses a new complete source set under
`data/source-snapshots/full-refresh/`; the set is built and validated before one
same-filesystem atomic directory exchange. The former complete set is retained
under `data/source-snapshots/rollback/`.

### ATO

ATO state is `data/sources/ato`. Routine acquisition retains What's New
semantics, a shared 50 ms issue interval, adaptive concurrency, 30-second
requests, exact payload sizes/hashes, and the pinned normalized fixture bytes.
A failed refresh preserves the last verified payload and remains retryable.

### Federal Register of Legislation

FRL uses `https://api.prod.legislation.gov.au/v1/`. The authoritative inventory
selects in-force Acts, legislative instruments, notifiable instruments, and
administrative-arrangements orders. Title scans use bounded `id` keysets;
version scans use stable registration ordering. `titleId` is identity while the
selected version tuple and `registerId` retain provenance.

Rendition preference is EPUB, DOCX, then PDF. Genuinely textless official PDFs
may retain title metadata; parser, OCR, or network failures fail the record.

### Courts and state legislation

The other adapters use only official publisher surfaces: Federal and High Court,
NSW Caselaw, and NSW, Queensland, Western Australian, South Australian, and
Tasmanian legislation.

The shared HTTP layer enforces exact HTTPS host allowlists, allowlisted
redirects, bounded decompression and response sizes, bounded retries, adaptive
concurrency, cookie affinity where required, and structured request audit
records. Federal Court alone uses one bounded Chrome/Chromium CDP process for
protected document hosts; discovery and all other sources use ordinary HTTP.
Set `LEGAL_MCP_CHROME` only if Chrome is not on `PATH`.

Install these maintainer-only conversion programs:

```text
unrtf antiword soffice pdftotext pdftoppm tesseract
```

They cover RTF, legacy Word, image-only Word, PDF text, rendering, and bounded
OCR. The serving VM does not need them.

## Model contract

The unpacked model input is:

```text
data/models/mdbr-leaf-ir-standard/
├── tokenizer.json
└── onnx/model.onnx
```

It is exported by `scripts/export-mdbr-leaf-ir-model.py` from
`MongoDB/mdbr-leaf-ir` revision
`1bb4fc387c49dee1c10c2b22f59db758be87dcaa`.

- `model.onnx`: 91,555,023 bytes, SHA-256
  `242a1d386f2f63a7daec443399b32d35b4b155b0820ee19b7c81c50436f95e11`
- `tokenizer.json`: 711,661 bytes, SHA-256
  `da0e79933b9ed51798a3ae27893d3c5fa4a201126cef75586296df9b4d2c62a0`

No model archive or mirror is produced. Disposable TensorRT engine/timing caches
belong under `data/cache`. Production builds use ONNX Runtime 1.25, the
deterministic FP32 graph, TensorRT FP16 profiles from `1x1` through `64x512`,
and CUDA fallback. Set `ORT_DYLIB_PATH` and `LEGAL_MCP_CUDA_LIB_PATH` when the
libraries are not discoverable. Keep `MALLOC_ARENA_MAX=24`.

Documents are unprefixed. Queries use exactly
`Represent this sentence for searching relevant passages:` followed by one
ASCII space.

Inputs split losslessly at 512 tokens. Stored vectors are normalized,
int8-quantized first-256-dimension embeddings.

## Routine local build and activation

```bash
cd /path/to/australian-legal-mcp
cargo build --release --features cuda
scripts/maintainer-sync.sh
```

A complete fresh repair is:

```bash
scripts/maintainer-sync.sh --full
```

Use `LEGAL_MCP_FORCE_REBUILD=1` for a deliberate schema/chunker/model-only
rebuild. `LEGAL_MCP_EMBEDDING_CACHE_DB` may point to a completed matching-schema
DB; only exact `(model_id, chunk_text_sha256)` vectors are reused. This is
disposable acceleration, never authoritative state.

For the one schema-10 to schema-11 cutover, derive rather than rebuild:

```bash
target/release/legal-mcp derive-schema11-from-schema10 \
  --source-generation-dir "$PWD/data/runtime/generations/1a6beead567b55babebbe253b5ae13efcd9ce2e8ab55b60c2de4106e39f180f4" \
  --expected-source-generation 1a6beead567b55babebbe253b5ae13efcd9ce2e8ab55b60c2de4106e39f180f4 \
  --out-dir "$PWD/data/builds/<fresh-schema-11-candidate>"
```

The projection strictly revalidates the immutable parent, reflinks or copies
its artifacts, rebuilds only the chunk FTS5 storage as contentless-delete,
removes the disposable embedding cache, writes `generation.json` last, and
strictly validates the result. SQLite necessarily performs FTS tokenization of
the already stored chunk text. The path performs no acquisition, OCR,
rechunking, model tokenization, model execution, re-embedding, or sidecar rebuild;
model, tokenizer, and all ten flat sidecars remain byte-identical. The command
accepts only the current flat format and is not a conversion path for historical
Arroy generations.

The script durably journals pending acquisition/build/activation work in
`data/runs/pending-generation.json`, resumes the same build output, performs
strict activation/verification, and retains full-refresh rollback stores. It
never packages or publishes corpus/model bytes.

Manual lifecycle commands are:

```bash
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp activate \
  --generation-dir "$PWD/data/builds/<generation-directory>"
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp verify
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp rollback \
  --generation <generation-id>
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp prune-generations \
  --keep-inactive 1
```

Deploy the locally active generation through the restricted CoW/delta path:

```bash
scripts/deploy-generation.sh \
  --host legal-mcp-publisher@HOST
```

The first run uploads the complete generation with negotiated zstd transport
compression. Subsequent runs CoW-seed remote staging from the active XFS
generation and rsync only changed blocks; interrupted uploads resume in place.
A one-shot copy of the exact OCI image strictly validates and activates the
result. The long-running service and every ordinary lifecycle invocation drop
all capabilities. Only the exact prepared-upload `activate` invocation adds
`CAP_DAC_OVERRIDE`, while remaining networkless, read-only-root, and
`no-new-privileges`; this is required to traverse and rename from the
publisher-owned mode-`0700` upload parent. The capability profile rejects every
other command. See [DEPLOYMENT.md](DEPLOYMENT.md) for OpenTofu, volume identity,
authentication, readiness, rollback, and VPS replacement; see
[MICROSOFT_COPILOT.md](MICROSOFT_COPILOT.md) for Entra/Copilot.

Flat-int8 v22 is active on the public Linode with exact v0.19.11 host tools and
runtime image. Arroy v20 is the sole hosted rollback generation; all transaction
journals and compatibility-adapter residue are absent. All seven tools, all ten
source partitions, formula assets, exact routes, private/public HarbourGrid,
live empty capability sets, reboot recovery, and API-key revocation passed after
cutover. The sole current key ID is `enterprise-laptop`; `second-client` must
remain revoked.

## Build semantics

Every generation starts from one complete committed source set. The build:

1. reconciles each authoritative inventory and source-scoped deletion;
2. streams normalization/chunk preparation without loading whole sources;
3. splits text losslessly and reuses exact model/text vectors;
4. encodes missing vectors in bounded batches;
5. derives metadata, links, definitions, citations, and FTS rows;
6. builds deterministic source-keyed flat int8 sidecars;
7. clears the disposable embedding cache and finalizes SQLite;
8. copies and re-verifies pinned model files atomically;
9. writes `generation.json` last while retaining resumable state until durable;
10. validates exact registry, DB, model, ANN, FTS, and hash bindings before
    immutable activation.

Each sidecar has a fixed 4 KiB little-endian header, a sorted u32 chunk-ID
plane, and an aligned contiguous 256-byte int8 vector plane. One bounded global
four-thread pool scans eligible rows exactly; SQLite rereads and reranks the
selected candidates with raw integer dots and chunk-ID tie order.

## Verification gate

```bash
cargo fmt --all -- --check
cargo test --workspace --locked --all-features
cargo clippy --workspace --locked --all-targets --all-features -- -D warnings
cargo audit
cargo deny check advisories
bash -n scripts/*.sh scripts/legal-mcp-azure-deploy \
  scripts/legal-mcp-host-deploy scripts/legal-mcp-publisher-command \
  infra/hosting/*.sh infra/linode/*.sh tests/*.sh
python3 -m unittest discover -s tests -p 'test_*.py'
# Run the host deployment fixtures, pinned ShellCheck, and actionlint exactly as
# .github/workflows/ci.yml does.
az bicep build --file infra/azure/main.bicep
git diff --check
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" scripts/smoke.sh
```

The production gate also exercises `activate`, failed activation, `verify`,
`rollback`, pruning, CoW/delta hosted deployment, hardened OCI/API-key behavior,
exact-generation `/readyz`, service restart persistence, and representative
keyword/vector/hybrid retrieval across
all ten sources. Sampled per-source top-50 IDs and raw integer scores must
exactly match the SQLite reference.
