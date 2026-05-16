---
paths:
  - "src/retrieval.rs"
---

# src/retrieval.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust MCP Search And Retrieval Tools
Hybrid BM25+vector search, title_hits, slim hits, RRF fusion, recency boost, similar-to-chunk vector lookup, and progressive get_chunks/get_doc_anchors retrieval.

- [MT-07 L541] get_chunks fetches exact chunk ids from search results, can include before/after ordinal neighbours, deduplicates overlapping requested ranges, and emits next_call when max_chars truncates context.
- [MT-17 L914] get_doc_anchors response carries a cited_by array of {doc_id, title, type, date} sourced from the citations table, ordered by source date DESC, capped at CITED_BY_LIMIT=100. When the cap truncates, cited_by_total reports the full distinct-source count so the agent knows the magnitude; both fields gracefully no-op (empty array, no total) on corpora that predate the citations table.

## Rust Update Mechanism
End-user update flow: update.json fast-path when local DB/model match, otherwise staged model/corpus rebuild and guarded promotion, with single-writer LOCK and doctor rollback backup.

- [UM-07 L920] rebuild_live_db_from_manifest calls derive_citations between the bulk pack insert and verify_semantic_install. Freshly-inserted chunks carry no citation rows, so every row must be derived in the staging DB before the atomic swap; skipping it ships an install with an empty citations table. Idempotent: clears + repopulates by streaming chunks.text once and regex-extracting [doc:X] markers.
