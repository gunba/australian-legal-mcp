# AGENTS.md

Instructions for agents developing, installing, or operating Australian Legal
MCP. Read [README.md](README.md), [MAINTENANCE.md](MAINTENANCE.md),
[CURRENT_STATE.md](CURRENT_STATE.md), [DEPLOYMENT.md](DEPLOYMENT.md), and
[MICROSOFT_COPILOT.md](MICROSOFT_COPILOT.md).

## Canonical contract

| Concern | Value |
|---|---|
| Product/package | `australian-legal-mcp` |
| Executable | `legal-mcp` |
| MCP key | `australian-legal` |
| Environment prefix | `LEGAL_MCP_*` |
| Repository | `gunba/australian-legal-mcp` |
| Project data root | `data/` |
| Runtime selector | `LEGAL_MCP_DATA_DIR` |
| Corpus DB | `legal.db` |
| Generation contract | `generation.json` |
| ANN sidecars | `ann/<source>.ann` |
| Sources | `ato`, `frl`, `federal-court`, `high-court`, `nsw-caselaw`, `nsw-legislation`, `qld-legislation`, `wa-legislation`, `sa-legislation`, `tas-legislation` |
| Live URI | `legal://<source>/<encoded-native-id>` |

Source acquisition, OCR, embeddings, and corpus construction run on the local
RTX maintainer host. A complete generation is validated, atomically activated,
and can be delta-copied into staging on an external XFS/reflink volume attached
to the Akamai/Linode VPS. A one-shot copy of the exact serving image validates
and activates it. The serving container never scrapes, embeds, builds, or
publishes corpus/model artifacts, and the image contains no corpus. GitHub
Releases are binary-only; GHCR images are digest-pinned and attested. The
current host has no active remote generation, authentication, application
service, active Caddy service, or ingress; do not describe it as live.

## Design principles

Expose compact source-grounded retrieval primitives and let agents reason.
Prefer deterministic structure from official sources, typed identities, few
parameters, and minimal response context. Every concern has one path: one
source-qualified identity model, schema, generation layout, URI syntax, and MCP
surface. Do not add guessed aliases, hand-maintained Act maps, brittle prose
interpretation, compatibility shims, or parallel lifecycle paths.

Preserve cleaned structural HTML. Internal links become deterministic typed
document references; retained images become typed asset references. Search text
is source-derived plain text. Builds consume committed workspaces only and
perform no scraping.

## Exactly seven MCP tools

- `search`
- `get_chunks`
- `get_asset`
- `get_doc_anchors`
- `get_definition`
- `stats`
- `fetch`

Every source-scoped request requires exactly one registered source. Public
`DocumentId`, `ChunkRef`, and `AssetRef` values remain typed; chunk references
also carry the generation. Continuations preserve all explicit filters.

`fetch` accepts only canonical `legal://SOURCE/PERCENT_ENCODED_NATIVE_ID` with
allowlisted typed query keys. Reject whitespace, fragments, credentials, ports,
extra path segments, noncanonical escapes, and unregistered sources. Canonical
URLs are stored official upstream values and are never reconstructed.

Keep ranking internals, model IDs, candidate counts, chunk ordinals, and debug
counters out of public responses. Omit empty optional fields rather than using
`null`.

## Source, build, and activation invariants

Authoritative current workspaces are `data/sources/<source>` and remain flat:
`state.json`, `documents/`, `assets/`, and temporary `staging/`. Fresh/failed/
rollback stores belong under `data/source-snapshots`; run state under
`data/runs`; disposable caches under `data/cache`; evidence under `data/logs`
and `data/validation`.

Each adapter owns official upstreams, incremental/full discovery, inventory,
rate/retry policy, normalization, provenance, and fixtures. Concurrent failures
remain source-isolated, but broad failure aborts. Reject empty/duplicate/unsafe/
catastrophically shrunken inventories and less than 99% usable full text.
Authoritative stable 404s and genuinely unavailable renditions may be omitted.
Do not convert parser, OCR, or network failure into metadata-only content.

Full repair uses a fresh complete source set and one atomic same-filesystem set
exchange after build/activation validation. Never overwrite committed stores in
place. Embedding reuse is disposable acceleration keyed only by exact
`(model_id, chunk_text_sha256)`.

Every generation contains exact DB, model, tokenizer, and ten ANN bindings.
Validation checks hashes, schema, source set, relational integrity, FTS, model,
and ANN metadata before making the directory read-only. `active-generation` is
the sole atomic switch. Installed generations are immutable and rollback-capable.
Never delete the active or last known-good generation before strict verification.

### ATO

Use `data/sources/ato`. Preserve What's New behavior, the shared 50 ms issue
interval, adaptive concurrency under the ten-request ceiling, 30-second timeout,
exact payload size/hash validation, and pinned normalization fixtures.

Search remains current-guidance-first: `EV` only by explicit type; old
non-legislation requires `include_old=true`; legislation is exempt; and
`current_only=true` filters withdrawn/superseded rulings.

### FRL and other official sources

FRL uses `https://api.prod.legislation.gov.au/v1/`, stable title/version
ordering, and EPUB → DOCX → PDF preference. The court and state adapters use
only official publisher surfaces. Shared acquisition enforces HTTPS host and
redirect allowlists, bounded response/decompression sizes, retries, adaptive
concurrency, structured audit records, and official provenance.

Federal Court protected document hosts alone use bounded Chrome/Chromium CDP;
discovery and all other sources use ordinary HTTP. Full text comes from official
HTML, DOCX, RTF/Word, PDF/OCR, or official extracted text.

## Build/model contract

Maintainer builds use the pinned unpacked model at
`data/models/mdbr-leaf-ir-standard`, deterministic FP32 ONNX with TensorRT FP16
and CUDA fallback, profiles `1x1` through `64x512`, and
`MALLOC_ARENA_MAX=24`. Documents are unprefixed; queries use exactly
`Represent this sentence for searching relevant passages: `. Split losslessly
at 512 tokens; store normalized int8 first-256-dimension vectors.

ANN proposes candidates; SQLite vectors are authoritative for exact reranking.
Keep deterministic Arroy construction and source-qualified schema 11. Schema
11 makes `chunks_fts` a contentless-delete FTS5 table while preserving the
authoritative chunk text in `chunks` and binding FTS postings/BM25 metadata by
digest.

`derive-schema11-from-schema10` is the sole schema-10 projection path. It
strictly validates one immutable schema-10 generation, uses SQLite FTS
tokenization to rebuild only `chunks_fts`, removes the disposable embedding
cache, and creates a fresh complete generation. It performs no acquisition,
OCR, rechunking, model tokenization, model execution, re-embedding, or ANN
rebuild; model, tokenizer, and ANN artifacts remain byte-identical.

Maintainer dependencies are `unrtf`, `antiword`, `soffice`, `pdftotext`,
`pdftoppm`, `tesseract`, and Chrome/Chromium for Federal Court. Serving is
CPU-safe.

## Operate

Local maintainer lifecycle:

```bash
cargo build --release --features cuda
scripts/maintainer-sync.sh             # or --full
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp verify
scripts/deploy-generation.sh \
  --host legal-mcp-publisher@HOST
```

Software is 0.19.2. Active local v20 is
`a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3`;
retain its v19 parent with the matching v0.18.1 binary/image as the schema-10
fallback. The schema-11 binary must not attempt to roll back to schema 10.

The current remote v20 transaction is fully staged but not active. V0.19.0
created the legitimate empty root-owned `LIFECYCLE_LOCK`, then safely failed
because its capability-free one-shot container could not traverse the
publisher-owned mode-`0700` upload parent. A v0.19.1 host-tool upgrade made no
mutations but rejected that extra lifecycle entry. Preserve the prepared
transaction. After publishing and verifying v0.19.2, upgrade only the host
tools with `--upgrade-host-tools --version 0.19.2`, then retry the exact
publisher `activate` command. Do not abort or rerun rsync; configure
authentication only after activation succeeds.

Manual recovery uses `activate`, `verify`, `rollback`, and
`prune-generations`. There is no runtime `update`, corpus download, corpus/model
package, publication, or offline-bundle command.

Hosted service runs in the digest-pinned OCI image as UID/GID 971 with a
read-only root, all capabilities dropped, and separate read-only corpus/lifecycle
plus read-write state bind mounts. Podman publishes the bridge port only at
`127.0.0.1:51235`; native Caddy owns public TLS. Hosted startup requires
`api-key`, `entra`, or `entra+api-key` authentication. API keys are individually
identified 256-bit credentials stored server-side only as protected digests;
Copilot always uses delegated Entra identity. Caddy remains disabled until auth
and exact readiness pass. Local stdio may use `legal-mcp mcp`. Never expose
51235, put corpus bytes in the image, or use FUSE/network storage for live
SQLite, ANN, pointers, or locks.

## Validation

Before activation or direct SSH deployment:

```bash
cargo fmt --all -- --check
cargo test --workspace --locked --all-features
cargo clippy --workspace --locked --all-targets --all-features -- -D warnings
cargo audit
cargo deny check advisories
bash -n scripts/*.sh
python3 -m unittest \
  tests/test_azure_generation_transport.py \
  tests/test_manage_api_keys.py \
  tests/test_remote_mcp.py \
  tests/test_render_microsoft_integrations.py
tofu -chdir=infra/linode init -backend=false -lockfile=readonly
tofu -chdir=infra/linode validate
git diff --check
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" scripts/smoke.sh
```

Also prove failed activation preserves the prior pointer, rollback, pruning,
exact-generation `/readyz`, service restart persistence, all-source retrieval,
and per-source ANN recall ≥ 0.99 at 50.

## Troubleshooting

- Missing active corpus: do not suggest a download. Locate/build a validated
  generation, activate it, or roll back.
- Hosted endpoint failure: check the Akamai Cloud Firewall and UFW, API-key or
  Entra challenges, Caddy, loopback `/livez` and `/readyz`,
  `systemctl status legal-mcp.service`, `podman inspect australian-legal-mcp`,
  and `journalctl -u legal-mcp.service`.
- Local stdio failure: confirm `legal-mcp mcp` and the intended
  `LEGAL_MCP_DATA_DIR`.
- Empty search: require the explicit source, inspect `stats`, and check type/date
  policy.
