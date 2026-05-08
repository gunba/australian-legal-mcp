---
paths:
  - "src/main.rs"
---

# src/main.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust MCP Tools And Search
Hybrid BM25+vector search, slim hits, RRF fusion, recency boost, session-scoped seen tracker.

- [MT-08 L710] FTS query construction: tokens joined with implicit AND, single-char tokens dropped (so R&D doesn't degenerate to zero results), hyphenated tokens preserved as quoted phrases ('s 8-1', '355-25').
- [MT-13 L726] types and doc_scope accept shell glob patterns: '*' is translated to SQL LIKE '%', and '\\', '%', '_' are escaped via _glob_to_like + ESCAPE clause.
- [MT-10 L756] Defaults exclude Edited_private_advice (DEFAULT_EXCLUDED_TYPES); content dated before 2000 is also excluded unless include_old=True or types matches DEFAULT_OLD_CONTENT_EXCEPTION_TYPES (legislation).
- [MT-04 L816] search returns slim hits only (chunk_id, doc_id, title, type, date, heading_path, anchor, snippet, canonical_url, score) — never the full chunk body; bodies materialize via get_chunks (progressive disclosure).
- [MT-02 L1001] search clamps k, inflates first-stage internal_limit to max(k*5, 50), then deduplicates candidates per document with max_per_doc before materializing hits.
- [MT-06 L1111] sort_by=recency expands the frontier to max(k*5, 50), materializes/deduplicates by relevance first, then sorts returned hits by date descending and truncates to k.
- [MT-03 L1115] search JSON metadata exposes next_call when more candidates exist and k can be increased; the next call preserves query, mode, filters, sort_by, include_old, and current_only.
- [MT-09 L1333] Hybrid/vector search require an EmbeddingGemma corpus: ensure_vector_search_ready checks embedding_model_id, model/tokenizer files, and chunk_embeddings before encoding the query.
  - Keyword mode stays lexical-only and is the explicit non-semantic mode; there is no lexical-hash vector fallback in the Rust runtime.
- [MT-05 L1451] Hybrid mode fuses BM25 and vector results via Reciprocal Rank Fusion with K=60: each result contributes 1/(K+rank+1) per ranker, scores summed across rankers.
- [MT-01 L1793] MCP stdio keeps one ServerState per process; SemanticRuntime and the reranker are loaded lazily and reused for subsequent tools/calls within that server process.
- [MT-14 L2354] search_titles bm25-ranks against title_fts (title + collected headings) — independent of chunks and the SeenTracker; the default exclusions for EPA and old non-legislation match search.
- [MT-11 L2475] get_document supports three retrieval modes through one tool: format='outline' returns the TOC; anchor/heading_path returns a section (include_children rolls up the subtree); from_ord paginates with count or max_chars and emits continuation_ord.
- [MT-12 L2476] get_document outline/card/markdown/json all resolve through the same document row and chunk selectors; outline/card use outline_for_doc, while markdown/json materialize selected chunks with continuation metadata.
- [MT-15 L3095] whats_new sorts by COALESCE(date, downloaded_at) DESC; the synthesised snippet says 'published <date>' when date is present, otherwise 'ingested <downloaded_at>'.
- [MT-07 L5012] get_chunks fetches exact chunk ids from search results, can include before/after ordinal neighbours, deduplicates overlapping requested ranges, and emits next_call when max_chars truncates context.

## Rust Output Formatters
Markdown table for hits, previously_seen tail, document outline + section + full renderers, JSON output.

- [OF-01 L696] canonical_url is synthesised from doc_id by direct substitution into the ATO URL pattern; href is not stored separately so the link always reflects the current doc_id.
- [OF-02 L2134] format_hits_markdown returns '_No hits._' for an empty hit list and otherwise renders a compact result table for search, search_titles, and whats_new.
- [OF-03 L2152] Markdown hit rows show compact doc_id references for follow-up retrieval; JSON hit rows retain canonical_url for callers that need the full ATO link.
  - Search, search_titles, and whats_new markdown tables avoid repeating full ATO URLs in every row.
- [OF-04 L2170] Markdown table cells escape '|' to '\\|' and replace newlines with spaces so snippets and heading_paths can't break out of the table grid.
- [OF-05 L2782] Outline rows indent by heading depth using doubled non-breaking spaces ('&nbsp;&nbsp;' per level), so deeper headings sit visually nested under shallower ones.
- [OF-06 L3270] JSON outputs use serde_json::to_string_pretty or to_vec_pretty before returning/writing, so CLI/MCP JSON responses and installed manifests are deterministic human-readable JSON strings/files.

## Rust Server Wiring
FastMCP tool registration, instructions builder from corpus stats, opportunistic warmup. MCP surface now includes get_definition as the single definition primitive.

- [SW-04 L1794] ServerState lazily loads SemanticRuntime on first semantic query and resolves the reranker state once; reranker load failures disable reranking for the rest of that server process.
- [SW-02 L5167] Server instructions are built dynamically at start time from corpus stats (doc count, chunk count, type breakdown, meta keys), so the agent sees up-to-date corpus shape without restart-time configuration.
- [SW-03 L5168] server_instructions is built from stats(OutputFormat::Json); if stats cannot be read, the server returns a static instruction telling the user to run ato-mcp init instead of crashing.
- [SW-01 L5519] Seven MCP tools are exposed by tool_descriptors/call_tool: search, search_titles, get_document, get_chunks, get_definition, verify_quote, and whats_new.
  - The surface stays small and explicit; unsupported tools fail through the normal tools/call error path.

## Rust Update Mechanism
End-user update flow: manifest diff, pack fetch, in-place SQLite patch application, lock.

- [UM-02 L509] The writer lock is implemented with fs2::FileExt::lock_exclusive on the app LOCK file, giving a cross-platform advisory lock around update/install mutation.
- [UM-06 L3305] Rollback path copies backups/ato.db.prev back over the live DB; failed delta updates also restore the backup before returning the error.
- [UM-01 L3433] Single-writer guard: apply_update takes the app LOCK file before apply_update_locked and releases it afterwards; serve/search paths open read-only DB connections and do not take the writer lock.
- [UM-05 L3537] Delta update flow: diff manifests, optionally promote to whole-corpus rebuild/backfill, fetch changed/added pack records, mutate SQLite in one transaction, verify semantic install, then write installed_manifest.json last.
- [UM-03 L3788] Fetch helpers resolve local paths, file://, manifest-relative assets, HTTP(S), and hf:// model/reranker URLs; downloaded model, reranker, and pack bytes are sha256-verified when the manifest provides a hash.
- [UM-04 L4124] fetch helpers intentionally don't read GitHub token env vars and don't shell out to gh — private release assets must be exposed through an approved mirror or installed from a local/offline bundle. This keeps the end-user runtime credential-free.

## CLI Commands
Typer command surface, end-user vs maintainer split, defaults and global excludes.

- [CC-04 L565] Runtime compatibility is fail-fast: open_read/open_write enforce the DB schema_version, and apply_update_locked rejects manifests whose schema_version or min_client_version exceeds the binary.
  - The Rust runtime does not run Python-era in-place migrations; incompatible or incomplete installs are rejected with reinstall/upgrade guidance.
- [CC-02 L3441] serve starts from the installed corpus by default; update_before_serve only runs when --check-update or ATO_MCP_AUTO_UPDATE is set, and opted-in startup update failures fall back to the installed DB when one exists.
  - ATO_MCP_OFFLINE forces the no-network startup path.
- [CC-03 L3492] init/update and opted-in serve startup checks call apply_update against a manifest URL, so manifest compatibility, model/reranker resolution, and pack ingestion are shared when network update logic is used.
  - Plain serve without --check-update only verifies that a local DB exists before entering the MCP stdio loop.
