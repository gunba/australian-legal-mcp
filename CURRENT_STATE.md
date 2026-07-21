# Current state

Updated 2026-07-19 after the chunker-format-6 flat-int8 rebuild, local
verification, HarbourGrid evaluation, and hosted-cutover recovery tests.

## Implemented product

- One Rust `legal-mcp` binary and exactly seven MCP tools: `search`,
  `get_chunks`, `get_asset`, `get_doc_anchors`, `get_definition`, `stats`, and
  `fetch`.
- Explicit source selection across ATO, FRL, Federal Court, High Court, NSW
  Caselaw, and five state-legislation sources.
- Source-qualified schema 11 with typed document/chunk/asset references,
  deterministic ranking, lossless continuations, cleaned structural HTML,
  exact stored official URLs, definitions, links, assets, and point-in-time
  fetch URIs. Chunk FTS is contentless-delete and its postings/BM25 metadata
  are digest-bound; authoritative text remains in `chunks`.
- BM25 plus mdbr-leaf-ir semantic retrieval. A source-scoped mmap flat int8
  sidecar is scanned exactly by one bounded four-thread pool, and SQLite
  normalized first-256-dimension vectors authoritatively rerank the candidates.
- Streamable HTTP rejects batches, validates protocol/content/origin/body limits,
  acknowledges notifications and response objects with 202, uses bounded
  workers/backpressure, emits structured request logs, exposes `/livez` and
  generation-aware `/readyz`, and drains on SIGTERM.
- Local HTTP transport is loopback-only. Hosted-container scope binds only
  inside a bridge, requires hosted authentication, and is published solely on
  host loopback behind Caddy.

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

Current authoritative source workspaces total 409,528 documents. Legacy and
rollback stores remain under `source-snapshots`; no destructive source cleanup
has occurred.

## Acquisition and model

- All ten source adapters implement official discovery, adaptive acquisition,
  strict inventory/quality validation, normalization, source locking, and
  transactional commit.
- Federal Court uses ordinary HTTP for discovery and Chrome CDP only for
  protected documents. UTF-8-first decoding plus official Word fallback repaired
  three damaged judgments. Federal v5 has 72,981 committed documents and 16
  authoritative 404 omissions.
- FRL has 32,771 records; five genuinely textless records retain metadata.
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

The deployed Linode generation is
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
prepared flat-v20 cutover copy, canonical source workspaces, models, deployment
state, logs, and validation evidence were retained.

## V22 local corpus

The software tree is 0.19.10. The active local generation is
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

## Local lifecycle and hosted cutover

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

Schema-11 support includes maintainer-only
`derive-schema11-from-schema10` and
`derive-flat-int8-from-schema11-arroy-v20` commands. The first requires the exact
typed immutable schema-10 parent and a fresh same-filesystem output, validates
before and after projection, rebuilds only chunk FTS storage, clears the
disposable cache, and writes the new manifest last. The one-shot v20 converter
strictly validates the typed immutable Arroy generation and derives flat
sidecars solely from authoritative SQLite vectors without model execution.

The host deployment contract now also provides a transactional,
version-matched `--upgrade-host-tools` operation, a publisher-accessible
explicit and idempotent `abort`, and a fail-closed
`update-image.sh --bootstrap-empty-host` image cutover. Upload or activation
failure never triggers abort automatically. V0.19.8 upgraded the historical
v0.19.5 launcher transactionally, detects rules through verbose UFW status,
closes the exact commented web rules, and accepts prepared-bootstrap,
configured-dark, or activated-dark state as explicitly defined by the operation.
V0.19.9 first added the live proof and bridge but failed closed before
transaction mutation because production `/run` is `noexec`. V0.19.10 makes only
the two private adapter binds executable inside the recovery mount namespace,
retains the exact live four-set process proof, and provides the release-bound
bridge for the one pending
v0.19.8 cutover. The launcher and updater bytes stay immutable; that unchanged
updater advances and retires its own journal under the shared host lock.
Under the shared host lock it atomically covers the publisher helper, wrapper,
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
v0.19.0 empty-host software contract. The v0.19.2 publisher-tool repair and
activation then succeeded, and v20
`a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3` remains
the active Linode generation after the v0.19.8 cutover rejected Podman 4.9's
`EffectiveCaps=null` observation and rolled back. The host now has exact
v0.19.8 V2 tools and the immutable v0.19.0 runtime image. Service, Caddy, web
UFW ingress, and `auth-ready` are off. The v22 target remains sealed with the
pending v0.19.8 cutover journal in configured-dark state; no target decision was
committed. API-key authentication, exact Caddy routes, all seven tools, all ten
source partitions, reboot recovery, and API-key overlap/revocation had passed
before maintenance. The sole current key ID is `second-client`.

Immutable v0.19.8 release assets, OCI digest, runtime, and attestations were
independently verified. V0.19.10 recovery must retire that exact pending journal
before normal host-tool upgrade and cutover retry. No Azure resource or Entra
tenant object exists. Azure
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
5. Align host-tool/image version labels only from an exact activated-dark state;
   runtime source correctness does not depend on this cosmetic alignment.

High Court historical coverage remains bounded by the official site's available
digitized collection. OALCC is reference-only clean-room research evidence.
