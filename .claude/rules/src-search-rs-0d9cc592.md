---
paths:
  - "src/search.rs"
---

# src/search.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust MCP Search And Retrieval Tools
Hybrid BM25+vector search, title_hits, slim hits, RRF fusion, recency boost, similar-to-chunk vector lookup, and progressive get_chunks/get_doc_anchors retrieval.

- [MT-08 L21] FTS query construction: tokens joined with implicit AND, single-char tokens dropped (so R&D doesn't degenerate to zero results), hyphenated tokens preserved as quoted phrases ('s 8-1', '355-25').
- [MT-13 L37] types and doc_scope accept shell glob patterns: '*' is translated to SQL LIKE '%', and '\\', '%', '_' are escaped via _glob_to_like + ESCAPE clause.
- [MT-10 L70] Defaults exclude Edited_private_advice (DEFAULT_EXCLUDED_TYPES); content dated before 2000 is also excluded unless include_old=True or types matches DEFAULT_OLD_CONTENT_EXCEPTION_TYPES (legislation).
- [MT-04 L133] search returns slim hits only (chunk_id, doc_id, title, type, date, anchor, snippet, canonical_url, plus optional currency markers and has_in_doc_links / has_related_docs / has_history flags) — never the full chunk body; bodies materialize via get_chunks (progressive disclosure).
- [MT-02 L323] search clamps k, inflates first-stage internal_limit to max(k*5, 50), then deduplicates candidates per document with max_per_doc before materializing hits.
- [MT-16 L325] search accepts similar_to_chunk_id to find chunks semantically near a known chunk without re-encoding a query.
  - When set, the runtime loads that chunk's stored int8 embedding from chunk_embeddings, forces vector-only mode with no BM25 stage, and filters the seed chunk out of results.
- [MT-06 L411] sort_by=recency expands the frontier to max(k*5, 50), materializes/deduplicates by relevance first, then sorts returned hits by date descending and truncates to k.
- [MT-03 L415] search JSON metadata exposes next_call when more candidates exist and k can be increased; the next call preserves query, mode, filters, sort_by, include_old, and current_only.
- [MT-09 L596] Hybrid/vector search require a Granite semantic corpus: ensure_vector_search_ready checks embedding_model_id, installed_manifest model metadata, model_fp16.onnx, model_fp16.onnx_data, tokenizer.json, marker agreement, and chunk_embeddings before encoding the query.
  - Keyword mode stays lexical-only and is the explicit non-semantic mode; there is no lexical-hash vector fallback in the Rust runtime.
- [MT-05 L716] Hybrid mode fuses BM25 and vector results via Reciprocal Rank Fusion with K=60: each result contributes 1/(K+rank+1) per ranker, scores summed across rankers.
- [MT-01 L743] ServerState owns one lazy SemanticRuntime cache; HTTP transport shares one ServerState across worker threads, and semantic runtime loading is reused across subsequent semantic tool calls.
  - Search-time inference holds the semantic_runtime mutex only while encoding query embeddings; read-only non-semantic tools run without that runtime lock.
- [MT-14 L1097] search populates title_hits from direct doc_id / ATO-link lookups and BM25 over title_fts (title + collected headings), independently of chunk ranking and SeenTracker.
  - Title hits reuse the same document filter as chunk search, so EPA/current/old/type/doc_scope exclusions stay consistent.
