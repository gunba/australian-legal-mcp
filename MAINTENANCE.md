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

The deployed software release is version 0.19.11. Immutable v0.19.11 release
assets, OCI digest, labels, runtime, and attestation were independently verified
and are live. V0.19.11 adds document-scoped FTS narrowing and a
public-route-aware HarbourGrid probe; it does not change the corpus. Its
host-tool contract
advanced normally to v0.19.11. The service is public through authenticated
Caddy routes, v22 is active, Arroy v20 is the sole hosted rollback, and all four
live capability sets remain empty.

Unreleased version 0.20.0 accepts schema 12 only. It requires ten
manifest-bound lexical SQLite sidecars and a payload-only `legal.db`. Do not
deploy it through an ordinary image-only or generation-only operation against
the live schema-11 pair. Build and validate a fresh complete generation, then
use the explicit pair transition below.

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

Schema 12 is a hard cut. There is no schema projection command. Build a fresh
complete generation from the committed source workspaces before activation.

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

## Schema-12 image and generation pair transition

An incompatible runtime and generation cannot move independently. The generic
pair coordinator binds two exact image/generation pairs and validates each
generation only with its matching digest-pinned image. It does not inspect or
translate a schema or index format.

First make schema 12 the locally active, strictly verified generation. Upload
it through the ordinary restricted publisher path without asking the old image
to activate it:

```bash
scripts/deploy-generation.sh \
  --host legal-mcp-publisher@HOST \
  --prepare-only
```

Record the 64-character generation ID. Upgrade the host tools from the exact
v0.20 release bundle. A public host requires explicit authority to close
ingress; the upgrade remains configured-dark:

```bash
sudo /var/lib/legal-mcp-release/v0.20.0/infra/linode/install-host.sh \
  --upgrade-host-tools --version 0.20.0 --from-public
```

Run the installed privileged coordinator while the prepared upload and its
ordinary deployment journal are intact:

```bash
sudo /usr/local/sbin/legal-mcp-update-image --pair-cutover \
  --generation SCHEMA12_GENERATION \
  --expected-current-generation 937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939 \
  --image ghcr.io/gunba/australian-legal-mcp@sha256:V020_DIGEST \
  --version 0.20.0 \
  --template /var/lib/legal-mcp-release/v0.20.0/infra/hosting/legal-mcp.container.template \
  < /path/to/current-probe-key
```

Omit the input redirection for Entra-only mode. If v0.20 host tools are already
installed while the service is public, add `--from-public`; without it the
coordinator refuses to darken a public host. The flag authorises closure only.
The command never republishes Caddy, UFW, or `auth-ready`.

The coordinator seals the upload, validates it through a read-only temporary
lifecycle with the target image, switches the pinned image and active pointer,
proves exact loopback readiness, authentication, mounts, UID/GID, and empty
capability sets, and commits only after the target pair passes. It leaves the
v0.19.11 image and schema-11 generation in place as the paired rollback. Hosted
activation does not prune generations automatically. Keep the immutable
v0.19.11 release bundle and do not manually prune that generation.

After success, explicitly restore the unchanged configured authentication:

```bash
sudo /usr/local/sbin/legal-mcp-configure-auth --recover \
  < /path/to/current-probe-key
```

Recover a killed or power-interrupted pair operation through the installed
launcher. Recovery rolls back both members before the durable target decision,
or finishes both members after it. It always returns configured-dark:

```bash
sudo /usr/local/sbin/legal-mcp-update-image --recover --pair-cutover \
  < /path/to/current-probe-key
```

To return from schema 12 to the retained v0.19.11/schema-11 pair, use the same
generic machinery with the immutable v0.19.11 bundle and digest. Add
`--from-public` when starting from the public state:

```bash
sudo /usr/local/sbin/legal-mcp-update-image --pair-rollback --from-public \
  --generation 937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939 \
  --expected-current-generation SCHEMA12_GENERATION \
  --image ghcr.io/gunba/australian-legal-mcp@sha256:43be03afbdd78c509053200d0f61b35a1519e9d95f303b917f8023f4ae2a7470 \
  --version 0.19.11 \
  --template /var/lib/legal-mcp-release/v0.19.11/infra/hosting/legal-mcp.container.template \
  < /path/to/current-probe-key
```

Republish authentication explicitly after the rollback. Ordinary same-schema
prepare, activate, abort, rollback, image, authentication, and bootstrap routes
remain unchanged outside an active pair transaction.

Flat-int8 v22 is active on the public Linode with exact v0.19.11 host tools and
runtime image. Arroy v20 is the sole hosted rollback generation; all transaction
journals are absent. All seven tools, all ten
source partitions, formula assets, exact routes, private/public HarbourGrid,
live empty capability sets, reboot recovery, and API-key revocation passed after
cutover. Current key IDs are `local-pi` and `work-laptop`; `enterprise-laptop`
and `second-client` must remain revoked.

## Build semantics

Every generation starts from one complete committed source set. The build:

1. reconciles each authoritative inventory and source-scoped deletion;
2. streams normalization/chunk preparation without loading whole sources;
3. splits text losslessly and reuses exact model/text vectors;
4. encodes missing vectors in bounded batches;
5. derives metadata, links, definitions, and citations in `legal.db`;
6. builds deterministic source-keyed flat int8 sidecars;
7. clears the disposable embedding cache and finalizes payload-only SQLite;
8. builds one deterministic source-only lexical SQLite sidecar per source;
9. copies and re-verifies pinned model files atomically;
10. writes `generation.json` last while retaining resumable state until durable;
11. validates exact registry, DB, model, ANN, lexical, filter, FTS, source-text,
    and hash bindings before
    immutable activation.

Each sidecar has a fixed 4 KiB little-endian header, a sorted u32 chunk-ID
plane, and an aligned contiguous 256-byte int8 vector plane. One bounded global
four-thread pool scans eligible rows exactly; SQLite rereads and reranks the
selected candidates with raw integer dots and chunk-ID tie order.

Each `lexical/<source>.db` contains only document filters, chunk mappings, and
contentless chunk/title FTS5 tables. Search is strict-only, runs entirely in
that source sidecar, orders by score descending then chunk ID ascending, and
hydrates selected winners from `legal.db`.

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
