---
paths:
  - "src/ato_mcp/store/schema.sql"
---

# src/ato_mcp/store/schema.sql

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Storage Layer
SQLite schema, sqlite-vec virtual table, FTS5 with Porter stemmer, WAL+mmap, prepared queries, migration.

- [SL-03 L60] chunks.text is a zstd-compressed UTF-8 BLOB. Heading text and emphasis (h1-h6, strong, em, blockquote, pre, li, dt+dd) are rendered inline as markdown so the embedder + BM25 see them as part of the chunk body — there is no separate heading_path column.
- [SL-05 L103] Connections enable WAL+synchronous=NORMAL for write modes only; read-only handles skip those pragmas. mmap_size=256 MB and temp_store=MEMORY are set unconditionally for both modes.
- [SL-04 L136] Both title_fts and chunks_fts are FTS5 virtual tables with tokenize='porter unicode61 remove_diacritics 2' — Porter stemming with diacritic-insensitive matching tuned for English legal text.
