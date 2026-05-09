---
paths:
  - "src/ato_mcp/store/schema.sql"
---

# src/ato_mcp/store/schema.sql

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Storage Layer
SQLite schema, sqlite-vec virtual table, FTS5 with Porter stemmer, WAL+mmap, prepared queries, migration.

- [SL-03 L48] chunks.text is a zstd-compressed UTF-8 BLOB; heading_path joins headings with ' › ' (U+203A) and is empty-string for intro chunks (before any body heading).
- [SL-04 L102] Both title_fts and chunks_fts are FTS5 virtual tables with tokenize='porter unicode61 remove_diacritics 2' — Porter stemming with diacritic-insensitive matching tuned for English legal text.
