---
paths:
  - "src/main.rs"
---

# src/main.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust MCP Tools And Search
Hybrid BM25+vector search, slim hits, RRF fusion, recency boost, session-scoped seen tracker.

- [MT-08 L1437] FTS query construction: tokens joined with implicit AND, single-char tokens dropped (so R&D doesn't degenerate to zero results), hyphenated tokens preserved as quoted phrases ('s 8-1', '355-25').
- [MT-13 L1453] types and doc_scope accept shell glob patterns: '*' is translated to SQL LIKE '%', and '\\', '%', '_' are escaped via _glob_to_like + ESCAPE clause.
- [MT-10 L1483] Defaults exclude Edited_private_advice (DEFAULT_EXCLUDED_TYPES); content dated before 2000 is also excluded unless include_old=True or types matches DEFAULT_OLD_CONTENT_EXCEPTION_TYPES (legislation).
- [MT-04 L1543] search returns slim hits only (chunk_id, doc_id, title, type, date, anchor, snippet, canonical_url, plus optional currency markers and has_in_doc_links / has_related_docs / has_history flags) — never the full chunk body; bodies materialize via get_chunks (progressive disclosure).
- [MT-02 L1733] search clamps k, inflates first-stage internal_limit to max(k*5, 50), then deduplicates candidates per document with max_per_doc before materializing hits.
- [MT-16 L1735] search() accepts similar_to_chunk_id to find chunks semantically near a known chunk without re-encoding a query. When set, the runtime loads that chunk's stored int8 embedding from chunk_embeddings, skips query encoding, forces vector-only mode (no BM25 stage), skips the reranker (no query string to anchor against), and filters the seed chunk out of results.
- [MT-06 L1821] sort_by=recency expands the frontier to max(k*5, 50), materializes/deduplicates by relevance first, then sorts returned hits by date descending and truncates to k.
- [MT-03 L1825] search JSON metadata exposes next_call when more candidates exist and k can be increased; the next call preserves query, mode, filters, sort_by, include_old, and current_only.
- [MT-09 L2015] Hybrid/vector search require an EmbeddingGemma corpus: ensure_vector_search_ready checks embedding_model_id, model/tokenizer files, and chunk_embeddings before encoding the query.
  - Keyword mode stays lexical-only and is the explicit non-semantic mode; there is no lexical-hash vector fallback in the Rust runtime.
- [MT-05 L2133] Hybrid mode fuses BM25 and vector results via Reciprocal Rank Fusion with K=60: each result contributes 1/(K+rank+1) per ranker, scores summed across rankers.
- [MT-01 L2364] MCP stdio keeps one ServerState per process; SemanticRuntime and the reranker are loaded lazily and reused for subsequent tools/calls within that server process.
- [MT-14 L2857] search_titles bm25-ranks against title_fts (title + collected headings) — independent of chunks and the SeenTracker; the default exclusions for EPA and old non-legislation match search.
- [MT-07 L12370] get_chunks fetches exact chunk ids from search results, can include before/after ordinal neighbours, deduplicates overlapping requested ranges, and emits next_call when max_chars truncates context.
- [MT-17 L12662] get_doc_anchors response carries a cited_by array of {doc_id, title, type, date} sourced from the citations table, ordered by source date DESC, capped at CITED_BY_LIMIT=100. When the cap truncates, cited_by_total reports the full distinct-source count so the agent knows the magnitude; both fields gracefully no-op (empty array, no total) on corpora that predate the citations table.

## Rust Output Formatters
JSON output for hits, document outline + section + full renderers.

- [OF-01 L1423] canonical_url is synthesised from doc_id by direct substitution into the ATO URL pattern; href is not stored separately so the link always reflects the current doc_id.
- [OF-06 L8979] JSON outputs use serde_json::to_string_pretty or to_vec_pretty before returning/writing, so CLI/MCP JSON responses and installed manifests are deterministic human-readable JSON strings/files.

## Rust Server Wiring
FastMCP tool registration, instructions builder from corpus stats, opportunistic warmup. MCP surface now includes get_definition as the single definition primitive.

- [SW-05 L8955] prefix_breakdown is corpus-derived: per-prefix doc counts plus a sample title used as the description. Replaces the hand-maintained prefix-to-doc-type map; surfaced via stats() so agents discover the canonical `doc_scope="<PREFIX>/%"` filter idiom for every prefix in the corpus.
- [SW-06 L9575] Serve startup runs a synchronous non-mutating availability probe (check_for_update_availability + http_probe_client + fetch_bytes_probe) with a tight 5s budget. It reuses the same fingerprint/compat helpers as the update fast-path skip; every error / timeout / missing installed manifest / incompatible summary collapses to None, so a slow network cannot stall the MCP stdio loop. The Option<UpdateAvailability> is stashed on ServerState and read by server_instructions to surface the update-available notice to the agent.
  - ATO_MCP_OFFLINE=1 short-circuits the probe before any I/O.
- [SW-02 L12876] Server instructions are built dynamically at start time from corpus stats (doc count, chunk count, type breakdown, meta keys), so the agent sees up-to-date corpus shape without restart-time configuration.
- [SW-03 L12877] server_instructions is built from stats(OutputFormat::Json); if stats cannot be read (corpus not yet installed) it returns a static install message telling the agent to ask the user to run ato-mcp update. When the serve-startup probe has stashed an UpdateAvailability on ServerState, both branches append a newer-index-available notice carrying the published index_version.
- [SW-01 L12905] Eight MCP tools are exposed by tool_descriptors/call_tool: search, search_titles, get_document, get_chunks, get_definition, get_asset, get_doc_anchors, and stats.
  - The surface stays small and explicit; unsupported tools fail through the normal tools/call error path.

## Rust Update Mechanism
End-user update flow: 898-byte update.json fast-path skip, otherwise full live-DB rebuild via staging file + atomic rename, with single-writer LOCK guarding mutation.

- [UM-02 L1216] The writer lock is implemented with fs2::FileExt::lock_exclusive on the app LOCK file, giving a cross-platform advisory lock around update/install mutation.
- [UM-06 L9067] Rollback path copies backups/ato.db.prev back over the live DB; replace_live_db preserves a backup before every atomic swap so doctor --rollback is the recovery path for any failed update.
- [UM-01 L9186] Single-writer guard: apply_update takes the app LOCK file before apply_update_locked and releases it afterwards; serve/search paths open read-only DB connections and do not take the writer lock.
- [UM-05 L9255] Update flow: 898-byte update.json fast-path skip when the installed corpus already matches the published summary; otherwise fetch manifest, ensure model/reranker, then rebuild the live DB wholesale via a staging file + atomic rename. There is no in-place delete+insert path.
- [UM-07 L9336,12669] rebuild_live_db_from_manifest calls derive_citations between the bulk pack insert and verify_semantic_install. Freshly-inserted chunks carry no citation rows, so every row must be derived in the staging DB before the atomic swap; skipping it ships an install with an empty citations table. Idempotent: clears + repopulates by streaming chunks.text once and regex-extracting [doc:X] markers.
- [UM-03 L9411] Fetch helpers resolve local paths, file://, manifest-relative assets, HTTP(S), and hf:// model/reranker URLs; downloaded model, reranker, and pack bytes are sha256-verified when the manifest provides a hash.
- [UM-04 L11352] fetch helpers intentionally don't read GitHub token env vars and don't shell out to gh — private release assets must be exposed through an approved mirror or installed from a local/offline bundle. This keeps the end-user runtime credential-free.

## CLI Commands
Typer command surface, end-user vs maintainer split, defaults and global excludes.

- [CC-04 L1267] Runtime compatibility is fail-fast: open_read/open_write enforce the DB schema_version, and apply_update_locked rejects manifests whose schema_version or min_client_version exceeds the binary.
  - The Rust runtime does not run Python-era in-place migrations; incompatible or incomplete installs are rejected with reinstall/upgrade guidance.
- [CC-03 L9208] ato-mcp update and the serve startup availability probe both gate on enforce_manifest_compatibility / enforce_update_summary_compatibility. Incompatible manifests surface as an upgrade-the-binary error from update; the probe silently suppresses incompatible-summary cases so the agent never points the user at an action that could not succeed.
  - ATO_MCP_OFFLINE=1 disables the startup probe entirely; the server still starts and serves whatever local corpus is present.
