# Current Implementation State

Updated: 2026-07-12

## Repository and product

- Local checkout: `/home/jordan/Desktop/Projects/australian-legal-mcp`
- Repository: `https://github.com/gunba/australian-legal-mcp`
- Package: `australian-legal-mcp`
- Version: `0.17.0`
- Executable: `legal-mcp`
- MCP key: `australian-legal`
- Configuration prefix: `LEGAL_MCP_*`
- Data directory name: `australian-legal-mcp`

The implementation is one Rust binary. `legal-mcp mcp` provides stdio MCP
through a shared local loopback backend; the same executable owns end-user
update and retrieval commands plus maintainer source, build and publication
commands. Remote public serving and deployment-role separation are roadmap work.

## Public retrieval contract

The MCP server exposes exactly seven tools:

1. `search`
2. `get_chunks`
3. `get_asset`
4. `get_doc_anchors`
5. `get_definition`
6. `stats`
7. `fetch`

Search selects exactly one source and defaults to `ato`. The production source
registry contains:

- `ato` — Australian Taxation Office legal and guidance material;
- `frl` — official Commonwealth legislation from the Federal Register of
  Legislation.

Public document, chunk and asset identities are source-qualified. Chunk
references also identify the corpus generation. Live fetch uses strict canonical
`legal://<source>/<percent-encoded-native-id>` parsing; ATO `pit` and `view`
queries round-trip through the typed URI.

## Storage and retrieval

The corpus schema is source-qualified throughout documents, chunks, definitions,
anchors, citations, assets, metadata and FTS. A published generation contains:

```text
legal.db
ann/ato.ann
ann/frl.ann
```

SQLite signed-int8 embeddings are authoritative. The selected source ANN finds
semantic candidates, and the runtime exact-reranks them with deterministic
chunk-reference ties. Hybrid search combines those candidates with source-scoped
BM25 results.

Document bodies are cleaned structural HTML. Search text is derived directly
from that HTML; heading paths remain metadata, and links/assets use deterministic
source-qualified references.

## Source foundation

Shared legal model and source SDK components provide:

- validated `SourceId`, document, chunk and asset identities;
- source descriptors and exact source resolution;
- normalized source inventory records and documents;
- acquisition rate policies, cursors and deterministic hashes;
- fixtureable source adapters and source-scoped failure results.

The production registry resolves only `ato` and `frl`, with `ato` as the default.
Search schema, CLI selection, continuations, statistics and retrieval preserve
the selected source.

## Acquisition state

Source updates run concurrently under independent source policies. Outcomes
distinguish current, updated, partial and failed source runs. Workspaces and run
directories are stable, distinct paths protected by per-workspace locks. Cursor
and inventory state advance after durable acquisition.

A source failure retains its last publishable state while unrelated sources
continue. Invalid rate policies fail before acquisition begins, and failed or
incomplete records remain eligible for the next overlap run.

### ATO

The ATO adapter directly reuses
`/home/jordan/Desktop/Projects/ato_pages/index.jsonl` and its integrity-pinned
payload tree. Routine acquisition runs What's New discovery and fetches changed
links with a shared 50 ms issue interval, four workers and a 30-second timeout.
Inventory fingerprints record the JSONL SHA-256 and record count, and selected
payloads are verified against declared size and SHA-256. Successful refreshes
write immutable SHA-256-named payloads before committing their inventory record;
failed refreshes retain the last verified payload and remain retryable.

### Federal Register of Legislation

The FRL adapter is registered as `frl` and uses the official
`https://api.prod.legislation.gov.au/v1/` API. Implemented contracts cover:

- deterministic authoritative `Titles` paging by `id`, with pages of at most 100;
- inclusive overlapping `Versions` discovery ordered by `registeredAt`,
  `titleId`, `start` and `retrospectiveStart`;
- a seven-day cursor overlap and stable deduplication;
- per-version `Documents` rendition enumeration;
- stable `titleId`, version tuple and `registerId` provenance;
- authorised EPUB, DOCX and official extracted PDF normalization;
- two concurrent operations, a 250 ms issue interval, a 30-second timeout and
  bounded retry;
- atomic state commits after rendition acquisition;
- direct deletion from periodic authoritative title reconciliation.

Fixture coverage includes OData paging, enum representations, rendition
selection, cursor overlap, unchanged and changed inventories, removed titles,
fetch failures, EPUB/DOCX normalization and PDF text requirements.
A bounded `legal-mcp frl-probe` validates one current official title, one
rendition page and one normalized document and returns the content projection.

## Corpus pipeline

The source-agnostic pipeline validates normalized documents, derives headings and
chunks, and reconciles each source in an immediate SQLite transaction. Every
publication targets a fresh `legal.db`. The pipeline:

- directly deletes source documents absent from an authoritative inventory;
- replaces changed documents and their dependent rows transactionally;
- reuses embeddings by approved model and chunk-text SHA-256;
- batches new embeddings;
- derives definitions, anchors and citations from normalized content;
- refreshes source and corpus metadata;
- finalizes one `ann/<source>.ann` per indexed source;
- validates shared corpus and vector identities before activation.

A complete generation is immutable and `active-generation` is its atomic commit
point.

Fresh builds can seed `embedding_cache` from a completed schema-v10 `legal.db`;
the imported rows remain keyed by the exact approved model and chunk-text hash.
Builds hold shared locks on every source workspace, while acquisition holds the
corresponding exclusive lock.

## Validation focus

The repository test suite covers typed source identities, source registry
resolution, strict `legal://` parsing, tool schemas, source-scoped continuations,
HTTP MCP transport, source update concurrency, failure isolation, ATO acquisition
reports, FRL official API fixtures, authoritative deletion, embedding reuse,
generation identity and ANN behaviour.

The release gate is:

```bash
cargo fmt --all -- --check
cargo test --workspace --locked --all-features
cargo clippy --workspace --locked --all-targets --all-features -- -D warnings
cargo audit
cargo deny check advisories
bash -n scripts/*.sh
git diff --check
scripts/smoke.sh
```

Installed-corpus validation exercises `stats`, source-specific keyword/vector/
hybrid searches, chunk and asset retrieval, anchors, definitions and strict ATO
live fetch. Publication also requires per-source ANN recall of at least 0.99 at
50 results and exact reranking checks.

## Release-readiness work

The next local release gate will:

1. run the ATO What's New update against the existing `ato_pages` workspace;
2. complete an authoritative FRL inventory and rendition fetch;
3. build a fresh combined `legal.db`;
4. build and validate `ann/ato.ann` and `ann/frl.ann`;
5. verify direct deletion, failure isolation and embedding reuse;
6. install the generated manifest into a clean
   `australian-legal-mcp` data directory;
7. run both source search suites and strict `legal://` fetch checks;
8. verify package archives contain `legal-mcp` and the matching ONNX Runtime.

After the combined local generation passes, planned work proceeds to High Court
and NSW Caselaw adapters, local container and OAuth validation, the remote
Streamable HTTP phase and the measured Azure pilot in [PLAN.md](PLAN.md).

## Operational facts

- Routine ATO work uses the existing `ato_pages` workspace and changed-link
  acquisition.
- Authoritative inventories directly remove absent source records.
- Source acquisition failures remain isolated by source.
- Corpus publications always construct a fresh `legal.db`.
- Matching embeddings are reused by model and chunk-text hash.
- End-user install, update, search and serving use the CPU-safe runtime.
- Maintainer embedding builds use `cargo build --release --features cuda`.
- Generation directories are immutable and updates activate atomically.
- The official FRL live probe normalized the current Acts Interpretation Act
  1901 EPUB into 254,530 bytes of HTML with one retained asset.
