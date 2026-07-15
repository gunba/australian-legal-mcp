# Delivery plan

## Mission

Operate one source-grounded Australian Legal MCP over ten official sources with
exactly seven tools, explicit source selection, deterministic citations and
ranking, locally built immutable generations, and secure low-cost private
hosting.

## Fixed architecture

- Current source truth: `data/sources/<source>`.
- Fresh/rollback/legacy stores: `data/source-snapshots`.
- Pinned unpacked model: `data/models/mdbr-leaf-ir-standard`.
- Resumable builds: `data/builds`; disposable acceleration: `data/cache`.
- Local activation: `data/runtime`; evidence: `data/runs`, `logs`, and
  `validation`.
- Acquisition, OCR, embedding, and ANN construction run only on the RTX PC.
- Builds consume committed source stores and never rescrape.
- Complete generations are validated, sealed read-only, atomically activated,
  and directly transferred over SSH.
- The CPU serving VM never downloads or builds corpus/model artifacts.
- GitHub Releases remain binary-only.

Every search requires one registered source. Omission is an error. Public JSON
uses typed source-qualified document/chunk/asset identities. `fetch` accepts only
canonical `legal://SOURCE/PERCENT_ENCODED_NATIVE_ID`. The MCP surface remains:
`search`, `get_chunks`, `get_asset`, `get_doc_anchors`, `get_definition`,
`stats`, and `fetch`.

## Completed

- Ten official adapters, concurrent source coordinator, adaptive ordinary HTTP,
  bounded Federal Court Chrome CDP, transactional acquisition, and strict
  source-quality gates.
- Source-qualified schema 10, cleaned HTML, links/assets/definitions, FTS,
  deterministic ranking/continuations, exact reranking, and ANN recall ≥ 0.99
  at 50.
- Pinned mdbr-leaf-ir FP32 ONNX graph, exact tokenizer, lossless 512-token
  splitting, TensorRT FP16/CUDA build path, and CPU serving path.
- V19: 409,528 documents, 6,968,250 chunks/embeddings, 20,170 definitions,
  validated DB/FTS/model/ANN/source bindings and all-source retrieval.
- Consolidated adjacent roots into the canonical ignored project `data/` tree
  without deleting rollback/legacy source truth.
- Removed remote updater/downloader/corpus publication/offline-bundle/GPU release
  paths.
- Added local activation, strict verification, rollback, pruning, lifecycle
  locks, durable build state, read-only generation sealing, generation-aware
  readiness, graceful bounded HTTP serving, and direct resumable SSH deployment.
- Added hardened systemd and a sub-USD-100 private VPS/Tailscale design.

## Phase 1 — prove local lifecycle

1. Build the final release binary and complete v19 activation from
   `data/builds/v19-local-generation` into `data/runtime`.
2. Verify exact generation ID, read-only permissions, hashes, SQLite/FTS,
   ten-source ANN readiness, and all-source representative retrieval.
3. Attempt activation of malformed/incomplete/symlink/hardlink generations and
   prove the pointer and v19 remain unchanged.
4. Prove rollback and pruning using storage-efficient Btrfs generation fixtures
   while preserving one known-good copy.
5. Prove interrupted activation/build resumption and pending source-set recovery.

Exit criterion: local build → validate → seal → activate → verify → rollback →
prune is deterministic, crash-safe, and contains no remote artifact assumption.

## Phase 2 — complete repository gates

Run and repair until all pass:

```bash
cargo fmt --all -- --check
cargo test --workspace --locked --all-features
cargo clippy --workspace --locked --all-targets --all-features -- -D warnings
cargo audit
cargo deny check advisories
bash -n scripts/*.sh
git diff --check
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" scripts/smoke.sh
cargo package --workspace --locked --allow-dirty
```

Also validate Linux binary packaging, CPU ONNX Runtime discovery, exactly seven
MCP descriptors, Streamable HTTP header/status behavior, SIGTERM drain,
backpressure, origin rejection, and exact-generation readiness.

## Phase 3 — private low-cost deployment

1. Provision a Sydney Linux VPS with at least 4 vCPU, 8 GiB RAM, and 160 GiB
   local SSD/NVMe; prefer 200–256 GB when budget permits.
2. Create the non-login `legal-mcp` account and install the release binary, CPU
   ONNX Runtime, environment file, and hardened `legal-mcp.service`.
3. Join the private Tailscale network. Keep port 51235 loopback-only and publish
   private HTTPS with Tailscale Serve.
4. Run `scripts/deploy-generation.sh deploy@host` from the RTX PC.
5. Verify direct transfer resume, disk margin, strict pre-prune checks, immutable
   activation, exact `/readyz` generation, representative searches, restart
   persistence, rollback, and retention.
6. Capture monthly cost, p50/p95 latency, request queue rejections, RSS, page
   cache, disk growth, and service logs.

Exit criterion: the hosted machine can be rebuilt from binary + direct local
transfer, serves no public unauthenticated port, and recovers to the previous
generation after a failed deployment.

## Phase 4 — scale only from evidence

- Increase RAM/workers vertically when p95 latency or queue rejection warrants.
- Add local-SSD read-only replicas behind an OAuth-capable gateway when one VM
  is insufficient; never put SQLite/Arroy on a network filesystem.
- Move to an Azure VM plus managed disk when Entra governance, procurement,
  private networking, or Microsoft 365/Copilot integration justifies the extra
  cost.
- Before public/cross-tenant exposure, implement MCP-compatible OAuth protected
  resource metadata, audience/expiry validation, authorization challenges, and
  audited tenant policy. Never use a shared static bearer token as a substitute.

## Cleanup gate

Delete v18, manual validation runtimes, old model archives, repair workspaces,
legacy diagnostics, and obsolete adjacent snapshots only after:

- v19 local activation and rollback pass;
- hosted v19 retrieval and restart persistence pass;
- direct deployment rollback passes;
- retained source snapshots are explicitly inventoried;
- no surviving file is the sole copy of source truth or validation evidence.
