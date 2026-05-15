---
paths:
  - "src/main.rs"
---

# src/main.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Granite Embedding Model
Granite ONNX semantic runtime: CPU by default, optional CUDA for maintainer builds, 1024-token dynamic padding, sentence_embedding or mean-pooling, 256-d int8 vectors.

- [EM-05 L44] Stored semantic vectors use the first EMBEDDING_DIM=256 dimensions of the Granite output before normalisation and int8 quantisation.
- [EM-03 L47] The tokenizer truncates semantic inputs at EMBEDDING_INPUT_MAX_TOKENS=1024 and pads dynamically to the batch max sequence length.
- [EM-02 L50] Granite embedding inputs use source-derived text directly; EMBEDDING_TEXT_PREFIX is empty, so neither stored chunk bodies nor runtime queries get query/passage prompt prefixes.
- [EM-01 L2369] SemanticRuntime loads Granite ONNX on CPU by default; maintainer --gpu builds require the cuda feature and CUDA execution-provider registration with error_on_failure.
- [EM-04 L2478] If the ONNX graph exposes sentence_embedding it is used directly; otherwise pooled_embeddings mean-pools 3D token embeddings with the attention mask and clamps the denominator to avoid div-by-zero on all-padding rows.
- [EM-06 L2664] Quantization rejects non-finite values, L2-normalises the first 256 dimensions, clips to [-1, 1], multiplies by 127, rounds, and stores raw int8 bytes.

## Rust CLI Commands
Closed clap command surface covering end-user MCP/update/doctor/search commands plus maintainer source, build, and release commands in the Rust binary.

- [CC-01 L99] One Rust binary owns both end-user commands (serve, install-http, update, doctor, stats, search, retrieval helpers) and maintainer-only source, build, and release commands; AGENTS.md documents which maintainer commands require checkout/source/model/GPU.
- [CC-06 L102] The CLI surface is a closed clap enum: every external command is declared in Command, with no dynamic plugin subcommands or generated shell-completion surface.
- [CC-05 L308] Source acquisition and corpus building are separate commands: source commands populate ato_pages/index.jsonl, while build requires pages-dir/model-dir/db-path/out-dir and can reuse a base release; the same pages tree can feed repeated builds.
- [CC-04 L1397] Runtime compatibility is fail-fast: open_read/open_write enforce the DB schema_version, and apply_update_locked rejects manifests whose schema_version or min_client_version exceeds the binary.
  - The Rust runtime does not run Python-era in-place migrations; incompatible or incomplete installs are rejected with reinstall/upgrade guidance.
- [CC-03 L10067] ato-mcp update and the serve startup availability probe both gate on enforce_manifest_compatibility / enforce_update_summary_compatibility. Incompatible manifests surface as an upgrade-the-binary error from update; the probe silently suppresses incompatible-summary cases so the agent never points the user at an action that could not succeed.
  - ATO_MCP_OFFLINE=1 disables the startup probe entirely; the server still starts and serves whatever local corpus is present.

## Rust Extraction And Chunking
Source HTML cleaning, container selection, leading-heading title composition, doc_id extraction, and block-aware chunking all live in the Rust binary.

- [IB-06 L3108] HTML container selection is deterministic: try #LawContent, #lawContents, #LawContents, then #contents, falling back to main/body if none match.
- [IB-07 L3623] Document title composition starts from leading headings, suppresses adjacent prefix-overlap duplicates, and falls back to the cleaned source title or canonical_id in the build path.
- [IB-18 L5102] doc_id is the ATO docid query path verbatim with prefix/case/slashes preserved; missing or malformed canonical URLs fall back to canonical_id so every source record has a stable key.

## Rust Index Builder
Rust corpus build orchestration: source-derived cleaning/metadata/chunking, adaptive Granite embedding batches, base-release seeding, checkpoint resume, packs, manifest/update output, and citation derivation.

- [IB-10 L70] Build pack shards seal after BUILD_PACK_RECORDS_PER_SHARD=4096 documents rather than a byte target, keeping pack downloads tractable while preserving stable offsets within each written pack.
- [IB-21 L4347] CHUNKER_FORMAT_VERSION is part of the build checkpoint gate; output-shape changes must bump it so stale checkpoints fail instead of silently resuming incompatible chunk records.
- [IB-22 L5007] chunker_pack projects chunk size from accumulated raw word counts, not summed per-block integer token estimates, so per-block truncation drift cannot push packed chunks over the max-token budget.
- [IB-09 L5295] Pack record format is length:uint32 little-endian followed by zstd(JSON record); the trailer stores MAGIC, record count, index offset/length, and a zstd(JSON) reverse index of doc_id, offset, and length.
- [IB-19 L6904] Build --profile emits cumulative stage timings plus embedding telemetry: batch count, inputs, active/padded tokens, padding efficiency, max batch, max sequence length, model tokens/sec, and tokenize/prepare/run/postprocess/write timings.
  - BuildProfile collects these counters during adaptive Granite encoding and prints them once at finalisation.
- [IB-11 L7095] Chunk embeddings travel through pack records as base64-encoded raw int8 bytes; install-side decode checks the decoded length against EMBEDDING_DIM before writing chunk_embeddings.
- [IB-13 L7204] Build checkpoints persist source index hash, zstd level, embedding model id/fingerprint/dim/max tokens, chunker format, committed doc refs, packs, base docs, base source hashes, and verified source doc_ids.
- [IB-12 L7820] Previous-release seeding copies the base DB and packs, computes a full source-derived fingerprint from each base pack record, and reuses a document only when the current fingerprint matches.
  - The fingerprint covers html, title/date/type, currency fields, navigation flags, anchors, definitions, chunks, and assets, so non-body extraction changes rebuild the document.
- [IB-17 L7887] Maintainer build accepts only the pinned local Granite model files via --model-dir; lexical/hash-vector experiments are not exposed as release corpus builders, and keyword mode is only a query-time path.
  - SemanticModelPaths::from_model_dir validates tokenizer.json, onnx/model_fp16.onnx, and onnx/model_fp16.onnx_data against pinned size and sha256 before build_corpus starts.
- [IB-14 L7967] Resume support skips only checkpoint-committed documents, or verified source doc_ids for a base-seeded checkpoint; any PENDING document rows abort the build and require a fresh release directory.
- [IB-16 L7978] The release corpus builder is a single Rust process with adaptive embedding batches; HTML extraction/chunking and DB writes are in-process, and no separate worker-pool build path is exposed.
- [IB-20 L8580] derive_citations runs during build finalisation and live-DB rebuild, clears and repopulates citations by streaming chunks.text, zstd-decompressing, regex-extracting [doc:X] markers, collapsing qualifiers to base doc_id, and skipping self-citations.

## Rust MCP Search And Retrieval Tools
Hybrid BM25+vector search, title_hits, slim hits, RRF fusion, recency boost, similar-to-chunk vector lookup, and progressive get_chunks/get_doc_anchors retrieval.

- [MT-08 L1575] FTS query construction: tokens joined with implicit AND, single-char tokens dropped (so R&D doesn't degenerate to zero results), hyphenated tokens preserved as quoted phrases ('s 8-1', '355-25').
- [MT-13 L1591] types and doc_scope accept shell glob patterns: '*' is translated to SQL LIKE '%', and '\\', '%', '_' are escaped via _glob_to_like + ESCAPE clause.
- [MT-10 L1621] Defaults exclude Edited_private_advice (DEFAULT_EXCLUDED_TYPES); content dated before 2000 is also excluded unless include_old=True or types matches DEFAULT_OLD_CONTENT_EXCEPTION_TYPES (legislation).
- [MT-04 L1681] search returns slim hits only (chunk_id, doc_id, title, type, date, anchor, snippet, canonical_url, plus optional currency markers and has_in_doc_links / has_related_docs / has_history flags) — never the full chunk body; bodies materialize via get_chunks (progressive disclosure).
- [MT-02 L1871] search clamps k, inflates first-stage internal_limit to max(k*5, 50), then deduplicates candidates per document with max_per_doc before materializing hits.
- [MT-16 L1873] search accepts similar_to_chunk_id to find chunks semantically near a known chunk without re-encoding a query.
  - When set, the runtime loads that chunk's stored int8 embedding from chunk_embeddings, forces vector-only mode with no BM25 stage, and filters the seed chunk out of results.
- [MT-06 L1959] sort_by=recency expands the frontier to max(k*5, 50), materializes/deduplicates by relevance first, then sorts returned hits by date descending and truncates to k.
- [MT-03 L1963] search JSON metadata exposes next_call when more candidates exist and k can be increased; the next call preserves query, mode, filters, sort_by, include_old, and current_only.
- [MT-09 L2153] Hybrid/vector search require a Granite semantic corpus: ensure_vector_search_ready checks embedding_model_id, installed_manifest model metadata, model_fp16.onnx, model_fp16.onnx_data, tokenizer.json, marker agreement, and chunk_embeddings before encoding the query.
  - Keyword mode stays lexical-only and is the explicit non-semantic mode; there is no lexical-hash vector fallback in the Rust runtime.
- [MT-05 L2273] Hybrid mode fuses BM25 and vector results via Reciprocal Rank Fusion with K=60: each result contributes 1/(K+rank+1) per ranker, scores summed across rankers.
- [MT-01 L2508] ServerState owns one lazy SemanticRuntime cache; HTTP transport shares one ServerState across worker threads, and semantic runtime loading is reused across subsequent semantic tool calls.
  - Search-time inference holds the semantic_runtime mutex only while encoding query embeddings; read-only non-semantic tools run without that runtime lock.
- [MT-14 L3021] search populates title_hits from direct doc_id / ATO-link lookups and BM25 over title_fts (title + collected headings), independently of chunk ranking and SeenTracker.
  - Title hits reuse the same document filter as chunk search, so EPA/current/old/type/doc_scope exclusions stay consistent.
- [MT-07 L13638] get_chunks fetches exact chunk ids from search results, can include before/after ordinal neighbours, deduplicates overlapping requested ranges, and emits next_call when max_chars truncates context.
- [MT-17 L13930] get_doc_anchors response carries a cited_by array of {doc_id, title, type, date} sourced from the citations table, ordered by source date DESC, capped at CITED_BY_LIMIT=100. When the cap truncates, cited_by_total reports the full distinct-source count so the agent knows the magnitude; both fields gracefully no-op (empty array, no total) on corpora that predate the citations table.

## Rust Output Formatters
JSON output for hits, document outline + section + full renderers.

- [OF-01 L1561] canonical_url is synthesised from doc_id by direct substitution into the ATO URL pattern; href is not stored separately so the link always reflects the current doc_id.
- [OF-06 L9838] JSON outputs use serde_json::to_string_pretty or to_vec_pretty before returning/writing, so CLI/MCP JSON responses and installed manifests are deterministic human-readable JSON strings/files.

## Rust Server Wiring
MCP tool registration, shared ServerState, runtime statistics instructions, install/update notices, and the small explicit tool surface.

- [SW-04 L2560] ServerState lazily loads SemanticRuntime on the first semantic query and reuses that runtime for the rest of the process.
  - There is no reranker state in the MCP surface; non-semantic tools do not load the semantic runtime.
- [SW-05 L9814] prefix_breakdown is corpus-derived: per-prefix doc counts plus a sample title used as the description. Replaces the hand-maintained prefix-to-doc-type map; surfaced via stats() so agents discover the canonical `doc_scope="<PREFIX>/%"` filter idiom for every prefix in the corpus.
- [SW-06 L10704] Serve startup runs a synchronous non-mutating availability probe (check_for_update_availability + http_probe_client + fetch_bytes_probe) with a tight 5s budget. It reuses the same fingerprint/compat helpers as the update fast-path skip; every error / timeout / missing installed manifest / incompatible summary collapses to None, so a slow network cannot stall the MCP stdio loop. The Option<UpdateAvailability> is stashed on ServerState and read by server_instructions to surface the update-available notice to the agent.
  - ATO_MCP_OFFLINE=1 short-circuits the probe before any I/O.
- [SW-02 L14144] Server instructions are built dynamically at start time from corpus stats (doc count, chunk count, type breakdown, meta keys), so the agent sees up-to-date corpus shape without restart-time configuration.
- [SW-03 L14145] server_instructions is built from stats(OutputFormat::Json); if stats cannot be read (corpus not yet installed) it returns a static install message telling the agent to ask the user to run ato-mcp update. When the serve-startup probe has stashed an UpdateAvailability on ServerState, both branches append a newer-index-available notice carrying the published index_version.
- [SW-01 L14173] Seven MCP tools are exposed by tool_descriptors/call_tool: search, get_chunks, get_definition, get_asset, get_doc_anchors, fetch_external_doc, and stats.
  - The surface stays small and explicit; unsupported tools fail through the normal tools/call error path.

## Rust Source Scraper
Maintainer source acquisition commands for What's New incremental pulls, tree crawl snapshots, snapshot reduction, deduped catch-up, and paced link download.

- [SS-02 L272] fetch_nodes_blocking calls the ATO browse-content API through a reqwest blocking client and expects the response payload to be a JSON list.
- [SS-01 L361] Source acquisition has explicit maintainer modes: whats-new plus scrape-diff for incremental pulls, tree-crawl plus snapshot-reduce for full snapshots, and scrape-diff over deduped links for catch-up gaps.
- [SS-04 L415] Default maintainer source-download pacing is 0.05s, and link-download defaults to max_workers=4; workers parse/write concurrently while the shared delay lock serializes HTTP issuance.
- [SS-03 L10932] Maintainer ATO API pacing uses a mutex-protected Instant before outgoing tree-crawl/link-download requests, serializing issuance across workers for the configured interval.
- [SS-07 L11140] snapshot_reduce dedupes canonical IDs across the tree, chooses a representative_path, records redundant folders, and filters excluded titles plus descendants before writing deduped_links and skip lists.
- [SS-06 L11452] link-download builds payload paths under payloads/ from each link record representative_path, so catch-up records inherit the reducer source path without manual category assignment.
- [SS-08 L11548] link_download uses up to max_workers threads with a shared queue, reqwest client, index map/writer, progress counters, and request-delay lock.

## Rust Storage Layer
SQLite schema, compressed chunk/html storage, FTS5, WAL write handles, pack/assets install, optional minisign release signatures, doc anchors, and derived citations.

- [SL-05 L1365,1378] Write connections enable foreign_keys, WAL, synchronous=NORMAL, and temp_store=MEMORY; read-only handles enable foreign_keys and temp_store=MEMORY without mutating WAL/synchronous settings.
- [SL-03 L1454] chunks.text is a zstd-compressed UTF-8 BLOB. Heading and inline emphasis markers are rendered into the stored chunk text; there is no separate heading_path column.
- [SL-10 L1490] doc_anchors stores in-document anchors, sister-document links, and historical-version pointers extracted at build time; get_doc_anchors also includes reverse citations from the citations table.
- [SL-11 L1504] citations is a derived reverse-citation index populated from inline [doc:X] markers. It is keyed by source_chunk_id plus target_doc_id, indexed by target_doc_id, collapses qualifiers to the base doc_id, and skips self-citations.
- [SL-04 L1519] Both title_fts and chunks_fts are FTS5 virtual tables using tokenize='porter unicode61 remove_diacritics 2' for stemmed, diacritic-insensitive English legal text search.
- [SL-07 L12341] publish-release optionally signs manifest.json by shelling out to the maintainer minisign CLI, then uploads manifest.json.minisig with the release artifacts.
- [SL-08 L12451] diff_manifests compares content_hash, pack_sha8, offset, and length, but the result is cosmetic update-summary telemetry; installs rebuild the live DB wholesale rather than applying deltas.
  - The changed/added/removed counts only feed the CLI summary printed by ato-mcp update.

## Rust Update Mechanism
End-user update flow: update.json fast-path when local DB/model match, otherwise staged model/corpus rebuild and guarded promotion, with single-writer LOCK and doctor rollback backup.

- [UM-02 L1342] The writer lock is implemented with fs2::FileExt::lock_exclusive on the app LOCK file, giving a cross-platform advisory lock around update/install mutation.
- [UM-06 L9926] doctor --rollback restores backups/ato.db.prev over the live DB; successful update promotion persists the previous DB as that rollback backup only after model, DB, assets, and manifest promotion have reached the commit point.
  - Transient promotion guards restore DB/assets/manifest/model on failed promotion before commit, and failed promotions do not replace the existing doctor rollback backup.
- [UM-01 L10045] Single-writer guard: apply_update takes the app LOCK file before apply_update_locked and releases it afterwards; serve/search paths open read-only DB connections and do not take the writer lock.
- [UM-05 L10115] Update flow short-circuits via update.json only when the installed corpus and local Granite model files match the published summary; otherwise it fetches manifest.json, stages model files, rebuilds DB/assets in staging, then promotes model, DB, assets, and installed_manifest with rollback guards.
  - There is no in-place delete+insert path; full rebuild on a fresh SQLite file is faster than mutating the live multi-GB DB and avoids FK cascades wiping derived tables mid-update.
- [UM-07 L10417,13937] rebuild_live_db_from_manifest calls derive_citations between the bulk pack insert and verify_semantic_install. Freshly-inserted chunks carry no citation rows, so every row must be derived in the staging DB before the atomic swap; skipping it ships an install with an empty citations table. Idempotent: clears + repopulates by streaming chunks.text once and regex-extracting [doc:X] markers.
- [UM-03 L10540] Fetch helpers resolve local paths, file://, manifest-relative assets, HTTP(S), and hf:// Granite model file URLs; downloaded model bundle/file and pack bytes are sha256-verified when the manifest or pinned model metadata provides a hash.
  - HF model installs verify each pinned Granite file and non-HF model bundles require explicit sha256 and positive size metadata.
- [UM-04 L12549] fetch helpers intentionally don't read GitHub token env vars and don't shell out to gh — private release assets must be exposed through an approved mirror or installed from a local/offline bundle. This keeps the end-user runtime credential-free.
