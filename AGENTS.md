# AGENTS.md

Instructions for agents developing, installing, or operating Australian Legal
MCP. Read [README.md](README.md), [MAINTENANCE.md](MAINTENANCE.md),
[CURRENT_STATE.md](CURRENT_STATE.md), and [DEPLOYMENT.md](DEPLOYMENT.md).

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
RTX maintainer host. A complete generation is validated and atomically
activated, then transferred directly over SSH to a CPU serving host. The serving
runtime never downloads, scrapes, embeds, packages, or publishes corpus/model
artifacts. GitHub Releases are binary-only.

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
Keep deterministic Arroy construction and source-qualified schema 10.

Maintainer dependencies are `unrtf`, `antiword`, `soffice`, `pdftotext`,
`pdftoppm`, `tesseract`, and Chrome/Chromium for Federal Court. Serving is
CPU-safe.

## Operate

Local maintainer lifecycle:

```bash
cargo build --release --features cuda
scripts/maintainer-sync.sh             # or --full
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp verify
scripts/deploy-generation.sh deploy@example-vps
```

Manual recovery uses `activate`, `verify`, `rollback`, and
`prune-generations`. There is no runtime `update`, corpus download, corpus/model
package, publication, or offline-bundle command.

The hosted service runs `legal-mcp serve` on loopback behind private Tailscale
HTTPS. Local stdio development may use `legal-mcp mcp`. Never expose port 51235
publicly or substitute a shared static token for proper private identity/OAuth.

## Validation

Before activation or direct SSH deployment:

```bash
cargo fmt --all -- --check
cargo test --workspace --locked --all-features
cargo clippy --workspace --locked --all-targets --all-features -- -D warnings
cargo audit
cargo deny check advisories
bash -n scripts/*.sh
git diff --check
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" scripts/smoke.sh
```

Also prove failed activation preserves the prior pointer, rollback, pruning,
exact-generation `/readyz`, service restart persistence, all-source retrieval,
and per-source ANN recall ≥ 0.99 at 50.

## Troubleshooting

- Missing active corpus: do not suggest a download. Locate/build a validated
  generation, activate it, or roll back.
- Hosted endpoint failure: check Tailscale, `/livez`, `/readyz`,
  `systemctl status legal-mcp.service`, and `journalctl -u legal-mcp.service`.
- Local stdio failure: confirm `legal-mcp mcp` and the intended
  `LEGAL_MCP_DATA_DIR`.
- Empty search: require the explicit source, inspect `stats`, and check type/date
  policy.
