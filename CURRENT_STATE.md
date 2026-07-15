# Current state

Updated 2026-07-15 on branch `codex/unified-australian-legal-mcp`.

## Implemented product

- One Rust `legal-mcp` binary and exactly seven MCP tools: `search`,
  `get_chunks`, `get_asset`, `get_doc_anchors`, `get_definition`, `stats`, and
  `fetch`.
- Explicit source selection across ATO, FRL, Federal Court, High Court, NSW
  Caselaw, and five state-legislation sources.
- Source-qualified schema 10 with typed document/chunk/asset references,
  deterministic ranking, lossless continuations, cleaned structural HTML,
  exact stored official URLs, definitions, links, assets, and point-in-time
  fetch URIs.
- BM25 plus mdbr-leaf-ir semantic retrieval. ANN proposes candidates and SQLite
  normalized int8 first-256-dimension vectors exact-rerank them.
- Streamable HTTP rejects batches, validates protocol/content/origin/body limits,
  acknowledges notifications and response objects with 202, uses bounded
  workers/backpressure, emits structured request logs, exposes `/livez` and
  generation-aware `/readyz`, and drains on SIGTERM.
- HTTP transport is loopback-only. The production design is private Tailscale
  HTTPS in front of a hardened systemd service.

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
76-check installed-corpus smoke suite passed on both CUDA/TensorRT and the
production CPU build. All-source active retrieval returned three correctly
source-scoped official-HTTPS hits for every source; canonical live ATO fetch,
exact-generation readiness, CPU SIGTERM drain, 215 non-ignored Rust unit tests,
seven HTTP integration tests, strict Clippy, audit/deny, npm allowlisting, and
workspace packaging also pass.

## Local lifecycle and hosting cutover

Implemented hard cut:

- removed runtime `update`, corpus packaging/publication, offline bundle, remote
  artifact discovery/download, and the GPU corpus-release workflow;
- added `activate`, `verify`, `rollback`, and `prune-generations`;
- local builds emit complete `generation.json` directories with pinned model
  files and no remote URLs;
- exact generation directories are non-symlink, immutable, same-filesystem,
  atomically activated, and hash/source/model/ANN bound;
- `scripts/maintainer-sync.sh` journals pending work, resumes failed builds,
  builds locally, verifies activation, and atomically exchanges complete full
  source sets;
- `scripts/deploy-generation.sh` holds a remote deployment lock, resumes direct
  rsync, checks disk, strictly verifies before pruning, activates, restarts,
  checks the exact `/readyz` generation, and rolls back on failure;
- hardened `systemd/legal-mcp.service` and low-cost private hosting guidance are
  in [DEPLOYMENT.md](DEPLOYMENT.md).

Initial hosting recommendation: one Sydney 4-vCPU/8-GiB/160-GiB SSD VPS at about
USD 48/month, two HTTP workers, loopback service, and Tailscale Serve. Azure VM
plus managed disk remains a future Entra/Microsoft 365 option but is unlikely to
fit below USD 100/month.

## Remaining proof before completion

1. Run dependency audit/deny and remaining package/cross-platform CI gates.
2. Optionally prove a pointer switch between two full valid generations; unit
   tests already cover two-key activation/pruning and the live v19 CLI covers
   strict idempotent rollback without duplicating 57 GiB.
3. Provision the VPS, install binary/CPU ONNX Runtime/systemd/Tailscale, transfer
   v19, and prove exact readiness, persistence, rollback, and retention.
4. Commit and push the still-uncommitted acquisition/model/performance/lifecycle/
   hosting tree.

High Court historical coverage remains bounded by the official site's available
digitized collection. OALCC is reference-only clean-room research evidence.
