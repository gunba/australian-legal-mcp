# Current state

Updated 2026-07-25 for the validated v0.20.0 schema-12 lexical cutover. The
immutable v0.19.11 schema-11 v22 runtime remains hosted until the matching
v0.20.0 image and validated schema-12 generation are deployed as one pair.

## Implemented product

- One Rust `legal-mcp` binary and exactly seven MCP tools: `search`,
  `get_chunks`, `get_asset`, `get_doc_anchors`, `get_definition`, `stats`, and
  `fetch`.
- Explicit source selection across ATO, FRL, Federal Court, High Court, NSW
  Caselaw, and five state-legislation sources.
- Source-qualified schema 12 with typed document/chunk/asset references,
  deterministic ranking, lossless continuations, cleaned structural HTML,
  exact stored official URLs, definitions, links, assets, and point-in-time
  fetch URIs. `legal.db` has no runtime FTS tables.
- Strict BM25 runs in one deterministic `lexical/<source>.db` per source. Each
  sidecar contains only compact filter/mapping metadata and contentless
  chunk/title FTS5 tables. It is bound by source, corpus, legal DB hash,
  source-text digests, FTS storage digests, file hash, and generation identity.
  Search hydrates selected winners only from `legal.db`; title search remains
  strict while direct native-ID hits are preserved.
- Mdbr-leaf-ir semantic retrieval is unchanged. A source-scoped mmap flat int8
  sidecar is scanned exactly by one bounded four-thread pool, and SQLite
  normalized first-256-dimension vectors authoritatively rerank the candidates.
- Schema 12 is a clean cut with no projection or compatibility command. Missing,
  extra, corrupt, wrong-source, or hash-mismatched lexical artefacts fail closed.
- Streamable HTTP rejects batches, validates protocol/content/origin/body limits,
  acknowledges notifications and response objects with 202, uses bounded
  workers/backpressure, emits structured request logs, exposes `/livez` and
  generation-aware `/readyz`, and drains on SIGTERM.
- Local HTTP transport is loopback-only. Hosted-container scope binds only
  inside a bridge, requires hosted authentication, and is published solely on
  host loopback behind Caddy.

The final schema-12 generation's 1,193,025,536-byte Federal Court lexical
sidecar passed strict post-build verification. The final Rust gate reopens the
sidecar after Linux `POSIX_FADV_DONTNEED` for every run; 30 advised-cold runs
measured 25.676 ms median, 29.863 ms p95 and 30.802 ms maximum. No result cache
is used. Internal `search-timing` logs correlate actual queue, lexical,
embedding, vector, fusion, hydration and total durations by request ID without
exposing timings, queries, scores, model details or candidate counts in MCP
responses.

## Canonical project data

All former adjacent data roots were moved without copying into ignored
`data/`, with the migration recorded in `data/migration-20260715.json`:

```text
data/sources
data/source-snapshots
data/models
data/builds
data/runtime
data/cache
data/runs
data/logs
data/validation
data/archive
```

Current authoritative source workspaces total 409,549 documents. Legacy and
rollback stores remain under `source-snapshots`; no destructive source cleanup
has occurred.

## Acquisition and model

- All ten source adapters implement official discovery, adaptive acquisition,
  strict inventory/quality validation, normalization, source locking, and
  transactional commit.
- Shared chunker format 9 holds headings until substantive body arrives,
  repeats that structural context in exact-token continuations and embedding
  input, and keeps canonical typed document, asset, anchor, and fetch markers
  atomic through every splitting path. South Australian RTF normalization
  preserves headings, lists and resolved internal/external links before the
  shared chunker.
- Federal Court uses ordinary HTTP for discovery and Chrome CDP only for
  protected documents. Structurally valid official HTML is preferred. Degraded
  HTML falls through to structured official Word, rendering those same Word
  bytes to PDF/OCR if structural parsing fails, and official PDF/OCR last.
  Genuine DOCX numbering, numbered paragraphs and first-reference-ordered
  footnotes are preserved.
  Federal normalizer revision 2 has 73,036 committed documents and 16
  authoritative 404 omissions.
- High Court categories are discovered dynamically from the official index with
  bounded traversal and fail-closed listing validation. The official index has
  no reported collection for 1960–1997 and labels 1906–1994 unreported coverage
  incomplete; no guessed URL or third-party fallback fills that gap.
- FRL schema-2 state has 32,732 authoritative in-force records. Exact official
  public EPUB or Word renditions may recover unavailable API entities only when
  title and authoritative timestamps match; PDF still requires official
  extracted text.
- The pinned model is `MongoDB/mdbr-leaf-ir` revision
  `1bb4fc387c49dee1c10c2b22f59db758be87dcaa`.
- Deterministic model graph: 91,555,023 bytes,
  `242a1d386f2f63a7daec443399b32d35b4b155b0820ee19b7c81c50436f95e11`.
- Tokenizer: 711,661 bytes,
  `da0e79933b9ed51798a3ae27893d3c5fa4a201126cef75586296df9b4d2c62a0`.
- CPU FP32 and TensorRT FP16 minimum cosine was `0.9999952316`; 98.6023% of
  quantized components matched.

## V19 corpus

Validated v19 output:

- 409,528 documents;
- 6,968,250 chunks and embeddings;
- 20,170 definitions;
- 40,000,348,160-byte `legal.db`;
- approximately 57 GiB including ten ANN sidecars;
- index `2026.07.14`, schema 10, model
  `mdbr-leaf-ir-tensorrt-fp16-256d`.

Per-source documents/chunks:

| Source | Documents | Chunks |
|---|---:|---:|
| ATO | 158,937 | 1,123,777 |
| NSW Caselaw | 124,443 | 2,830,980 |
| Federal Court | 72,981 | 1,769,363 |
| FRL | 32,771 | 441,910 |
| High Court | 6,169 | 240,797 |
| SA legislation | 5,094 | 119,414 |
| Queensland legislation | 3,370 | 211,784 |
| NSW legislation | 2,231 | 125,585 |
| Tasmania legislation | 1,961 | 38,400 |
| WA legislation | 1,571 | 66,240 |

Full validation passed SQLite integrity and foreign keys, logical FTS integrity,
exact manifest/database/source bindings, all model/ANN sizes and hashes, vector
counts, ordinals, metadata, repaired Federal text, all-source retrieval, source
isolation, and official HTTPS URLs.

The old remote-release manifest was transformed into a complete local
`generation.json` candidate using Btrfs reflinks and separately pinned model
files. After several safely failed/interrupted validation attempts exposed FTS
and cross-directory rename edge cases, the final lifecycle activated v19 as
`1a6beead567b55babebbe253b5ae13efcd9ce2e8ab55b60c2de4106e39f180f4` under
`data/runtime`. Full staging validation took 33m12.76s and exited 0. The exact
DB/model/ANN tree is read-only, has no hard-linked files, and the source build
directory was atomically consumed. Strict CUDA/TensorRT `verify` passed in
29.72s with all counts and `semantic_search_ready=true`; the CPU runtime also
loaded/encoded successfully. A malformed activation preserved the exact v19
pointer, idempotent rollback revalidated v19, pruning removed nothing, and the
83-check installed-corpus smoke suite passed on the production CPU build (the
prior 76-check suite also passed on CUDA/TensorRT). All-source active retrieval
returned three correctly source-scoped official-HTTPS hits for every source. A
fresh post-pivot full v19 CPU verification and all 83 smoke checks also exited
zero. Canonical live ATO fetch, exact-generation readiness, CPU SIGTERM drain,
252 workspace/HTTP tests (with 11 explicitly ignored hardware/live tests),
strict Clippy, audit/deny, npm allowlisting, and workspace packaging pass.

## V20 corpus

The sole hosted rollback generation is
`a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3`:

- minimum client 0.19.0, index `2026.07.14`, schema 11, and the unchanged
  `mdbr-leaf-ir-tensorrt-fp16-256d` model binding;
- 409,528 documents, 6,968,250 chunks/embeddings, and 20,170 definitions;
- 19,746,840,576-byte `legal.db`, SHA-256
  `26143e8908fc879a7e03af158cf014101d846c93f5d48d2b1687e48b2cc5fe90`;
- approximately 37.42 GiB for the complete generation;
- zero embedding-cache rows; and
- index metadata plus model, tokenizer, and all ten historical Arroy manifest
  bindings identical to v19.

V20 was deterministically projected from the immutable v19 schema-10 parent.
Its historical Arroy sidecars are deliberately rejected by the hard-cut flat
sidecar binary and must be rebuilt into a new complete generation before that
binary is activated. The projection used SQLite FTS tokenization over existing
chunk text to replace the contentful chunk index with schema 11
contentless-delete FTS. It performed
no acquisition, OCR, rechunking, model tokenization, model execution,
re-embedding, or ANN rebuild. The database is 20,253,507,584 bytes smaller than
v19. Full activation and verification passed, followed by all 76 smoke checks
and all-ten-source hybrid retrieval. V19 remains installed as the rollback
source for a paired v0.18.1 binary/image fallback; the schema-11 binary does not
accept schema 10.

The initial post-validation cleanup removed superseded local build/cache
material without removing source truth or the v19 fallback. After the v22
validation, a second reviewed cleanup removed the superseded packaged v19 build,
disposable embedding/TensorRT/Azure caches, Cargo debug/cross/package outputs,
the downloaded OpenTofu provider cache, completed acquisition-attempt runs, and
one-off build/benchmark scratch files. Allocated project usage fell from about
300 GiB immediately before that cleanup to about 203 GiB. Btrfs reflink sharing
means the filesystem gained about 23 GiB rather than the full logical reduction.
The active v22, v21 rollback parent, hosted Arroy v20 source, v19 DR corpus,
canonical source workspaces, models, deployment state, logs, and validation
evidence were retained.

## Validated schema-12 release candidate

The final chunker-format-9 generation is
`624e214456dbb58b5ac231be5f296fa6079c49b9cb6e57fc8a22f1a5116dca33`:

- schema 12, index `2026.07.24`, and minimum client v0.20.0;
- 409,549 documents, 7,588,730 chunks/embeddings, and 20,943 definitions;
- 15,635,169,280-byte `legal.db`, SHA-256
  `5c4984ee7651dbec6f26988982f594873691ecb574cb217f407c5de6d2a9fd9e`;
- ten source-scoped flat-int8 ANN and ten source-scoped SQLite lexical
  sidecars; and
- exact-tree/hash, SQLite integrity, foreign key, source binding, asset hash,
  isolated activation, strict CUDA startup, all-seven-tool HarbourGrid, and
  cold lexical validation passed.

HarbourGrid completed with zero failures. Warm p95 was 15.316 ms for hybrid
search, 7.956 ms for keyword search, and 6.817 ms for retrieval. Production
remains on the matching v0.19.11/schema-11 pair until the v0.20.0 release gate
completes.

## V22 released corpus

The released v0.19.11 schema-11 binary narrowed document-scoped lexical work
while preserving wildcard, case-insensitive, and missing-scope behavior. Its
active local generation is
`937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939`:

- minimum client 0.19.7, index `2026.07.19`, schema 11, and chunker format 6;
- 409,528 documents, 6,986,040 chunks/embeddings, and 20,169 definitions;
- 19,758,231,552-byte `legal.db`, SHA-256
  `c8e77a7dbf61a8b185592c07bb47b0cc324bfc2cce2b9e2663f5c4716483b851`;
- ten deterministic source-scoped flat-int8 sidecars totalling 1,816,430,592
  bytes; and
- strict CPU verification with `semantic_search_ready=true`.

The rebuild seeded its exact `(model_id, chunk_text_sha256)` cache from the
active v21 SQLite `chunk_embeddings`. Unchanged text reused authoritative
vectors; only changed chunk text was encoded. Chunker format 6 preserves typed
FRL formula images through internal-link rewriting. The HarbourGrid evaluation
passed with zero failures, including formula text, typed asset discovery,
`get_asset`, all expected authorities, warm keyword p95 363.234 ms, warm hybrid
p95 516.406 ms, retrieval p95 11.636 ms, and readiness under concurrent load.

## Local lifecycle and hosted deployment

Implemented hard cut:

- removed runtime `update`, corpus packaging/publication, offline bundle, remote
  artifact discovery/download, and the GPU corpus-release workflow;
- added strict `activate`, `verify`, `rollback`, `prune-generations`, and bounded
  container `healthcheck` operations;
- local builds emit complete immutable `generation.json` directories containing
  exact database, model, tokenizer, and ten source-bound ANN sidecars;
- `scripts/maintainer-sync.sh` journals/resumes local work and atomically
  exchanges complete full source sets;
- `Containerfile` produces a corpus-free linux/amd64 image from digest-pinned
  Rust/Debian bases with bundled SQLite, mmap flat-int8 search,
  tokenizer/reranking code, ONNX Runtime 1.25.0, native runtimes, CA
  certificates, and fixed
  unprivileged UID/GID 971;
- the long-running OCI service uses a read-only root, zero capabilities,
  `no-new-privileges`, bounded resources, separate read-only
  generations/lifecycle and read-write state mounts, and bridge publication only
  at host `127.0.0.1:51235`;
- hosted-container network scope cannot start without `--require-http-auth`;
- HTTP auth supports exact `api-key`, `entra`, and `entra+api-key` modes. API
  keys have revocable IDs, 256-bit generated secrets, protected digest-only
  verifier files, constant-time comparison, ambiguity rejection, and structured
  principal logging;
- Entra retains exact issuer/audience/tenant/time/scope/caller checks and RFC
  9728 metadata, while JWKS cache fallback now has a hard 24-hour stale limit;
- `infra/linode` contains lock-pinned OpenTofu for a Sydney Ubuntu VPS,
  persistent encrypted 128-GiB reflink volume, creation-time SSH-first Cloud
  Firewall attachment with essential ICMPv6, and optional DNS. The volume has
  `prevent_destroy` and public 80/443 default off;
- the Linode installer accepts only a signature-free new block device or an
  exact UUID/marker-bound existing XFS/reflink volume. It validates before
  atomically persisting fstab and requires `noatime,nodev,noexec,nosuid`, exact
  ACLs, and file-type support. It creates fixed service, publisher, and
  break-glass administrator identities, disables root/password SSH, installs
  rootful Podman/Quadlet, and leaves the generated application unit inactive
  and release-bundled, checksum-pinned Caddy disabled/inactive;
- the forced publisher can write only upload staging. Strict local verification,
  CoW seeding, checksum/block-delta rsync, one-shot image validation, atomic
  activation, explicit activation/rollback journal phases, exact readiness,
  durable recovery, and rollback remain separate privileged operations. An
  execute-only ACL lets the publisher reach staging without exposing generation
  or lifecycle directories. The exact networkless prepared-upload `activate`
  invocation alone receives `CAP_DAC_OVERRIDE` so it can traverse and rename
  from the publisher-owned mode-`0700` parent; the service and all other
  lifecycle invocations remain capability-free;
- corpus, auth/ingress, and image-digest changes share one host transaction lock.
  Auth and image changes close UFW/Caddy during mutation, persist recoverable
  prior state, enforce the exact administrator/public UFW allowlist, and
  re-prove the exact generation, challenge/metadata contract, and positive API
  key where applicable before restoring ingress;
- release workflows either build once or recover the existing same-revision OCI
  artifact, scan that exact OCI digest, publish only immutable GHCR version/SHA
  tags, attach SBOM/max provenance plus GitHub/Sigstore attestation, and deploy
  by digest rather than tag. Release archives carry the exact `SOURCE_COMMIT`;
- Copilot Studio Swagger and Microsoft 365 plugin v2.4 templates still render
  from the exact seven read-only descriptors. Entra works unchanged on Linode
  and remains the Microsoft 365 identity path.

The host deployment contract now also provides a transactional,
version-matched `--upgrade-host-tools` operation, a publisher-accessible
explicit and idempotent `abort`, and a fail-closed
`update-image.sh --bootstrap-empty-host` image cutover. Upload or activation
failure never triggers abort automatically. Under the shared host lock it
atomically covers the publisher helper, wrapper,
sudoers, `configure-auth`, `update-image`, installed Quadlet template, and V2
marker/hashes; exact version, `SOURCE_COMMIT`, and release bytes are mandatory,
and recovery uses the same bundle. Auth cutover now treats
`legal-mcp.service` as generated and preserves exact activity without attempting
to enable or disable it.

The historical v0.18.1 image and its SBOM/provenance-bearing OCI archive were
built locally, loaded ONNX Runtime, ran as `971:971` with a read-only root and no
capabilities, and passed bridged valid/invalid/ambiguous API-key plus exact-path
HTTP probes while the host mapping remained loopback-only. Skopeo preserved the
scanned top-level digest, and Trivy 0.65.0 reported zero fixed HIGH/CRITICAL
findings across 92 Debian packages. Podman 4.9.3 generated the final Quadlet;
actionlint, ShellCheck, Caddy 2.11.4, and the Linode OpenTofu 4.1.0 provider
contract validate cleanly. Disposable Ubuntu fixtures exercise the forced
publisher/lock, packaged `rrsync -wo`, locked-parent activation with the exact
capability bit, capability-free verify/prune, SIGKILL reconciliation,
generated/inactive pending-auth recovery, exact active/inactive auth rollback,
disabled/API-key/Entra auth recovery, incomplete-transaction ingress closure,
and API-key image-recovery parsing. The auth fixture rejects application
enable/disable operations because the Quadlet service is generated.
Provider-neutral Microsoft assets render for custom DNS. On 2026-07-16 the
reviewed OpenTofu plan created one Sydney `g6-standard-4` instance, one encrypted
128-GiB Block Storage volume, and one creation-time Cloud Firewall. The host was
bootstrapped with verified v0.18.1 artifacts and subsequently cut over to the
v0.19.0 empty-host software contract. The v0.19.2 publisher-tool repair and v20
activation then succeeded.

Exact v0.19.10 host tools committed v22
`937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939`.
Immutable v0.19.11 then advanced the host tools and same-generation runtime to
commit `893b06c20e5fc2f33ca7633e636023ccb5762745` and image
`ghcr.io/gunba/australian-legal-mcp@sha256:43be03afbdd78c509053200d0f61b35a1519e9d95f303b917f8023f4ae2a7470`.
Arroy v20 is the sole hosted rollback; every image/auth/host-tool/corpus journal
is absent. The service is public through exact
Caddy routes, port 51235 is host-loopback-only, and live bounding, effective,
inheritable, and permitted capability sets are empty. Private and public
HarbourGrid, typed formulas/assets, all seven tools, all ten source partitions,
route denial, API-key revocation, and reboot recovery passed. Current key IDs
are `local-pi` and `work-laptop`. The current Pi install passed an authenticated
v22 stats call with `local-pi`; `enterprise-laptop` and `second-client` return
401 and must remain revoked.

The HarbourGrid historical agent memorandum has completed substantive technical
QA. It remains useful evaluation evidence but is not board-ready without the
corrections and citation refresh recorded in
[the technical review](docs/validation/harbourgrid-memorandum-technical-review.md).

No Azure resource or Entra tenant object exists. Azure
Bicep/Blob work remains preserved as a secondary future provider path in
[docs/AZURE_FUTURE.md](docs/AZURE_FUTURE.md).

## Remaining optional enterprise and disaster-recovery work

1. Replace the temporary Linode hostname with stable owned DNS.
2. Create the Entra resource and caller app registrations, exercise a real
   delegated token, and test Copilot Studio consent, tool invocation, and DLP.
3. Test the optional direct Microsoft 365 declarative-agent SSO registration in
   a licensed tenant.
4. During separately reviewed maintenance, prove volume detach/reattach,
   disposable VPS replacement, and an image rollback without risking the only
   live corpus host.

High Court historical coverage remains bounded by the official site's available
digitized collection: reported judgments have an official 1960–1997 gap and the
1906–1994 unreported collection is explicitly incomplete. OALCC is
reference-only clean-room research evidence.
