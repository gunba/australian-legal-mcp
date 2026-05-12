---
paths:
  - "src/ato_mcp/store/schema.sql"
---

# src/ato_mcp/store/schema.sql

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Storage Layer
SQLite schema, sqlite-vec virtual table, FTS5 with Porter stemmer, WAL+mmap, prepared queries, migration.

- [SL-03 L60] chunks.text is a zstd-compressed UTF-8 BLOB. Heading text and emphasis (h1-h6, strong, em, blockquote, pre, li, dt+dd) are rendered inline as markdown so the embedder + BM25 see them as part of the chunk body — there is no separate heading_path column.
- [SL-10 L103] doc_anchors stores in-doc paragraph anchors, sister-doc references, and historical-version pointers extracted from <a href> markup at build time; doc_anchors.kind is one of 'in_doc' | 'sister' | 'history'. Surfaced through the get_doc_anchors MCP tool, which also returns reverse citations from the citations table.
- [SL-11 L118] citations is a derived reverse-citation index, populated from inline [doc:X] markers in chunks.text at the tail of every build / apply_update. PRIMARY KEY (source_chunk_id, target_doc_id) so each chunk-target pair appears once; indexed on target_doc_id for reverse lookup. PiT and view qualifiers collapse to the base doc_id; self-citations are skipped.
- [SL-04 L136] Both title_fts and chunks_fts are FTS5 virtual tables with tokenize='porter unicode61 remove_diacritics 2' — Porter stemming with diacritic-insensitive matching tuned for English legal text.
