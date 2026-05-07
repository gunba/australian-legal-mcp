---
paths:
  - "src/ato_mcp/store/schema.sql"
---

# src/ato_mcp/store/schema.sql

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Storage Layer
SQLite schema, sqlite-vec virtual table, FTS5 with Porter stemmer, WAL+mmap, prepared queries, migration.

- [SL-01 L4] v5 schema is minimal: documents has only (doc_id PK, type, title, date) + 3 build-time columns (downloaded_at, content_hash, pack_sha8); pre-v5 DBs are rejected with a migration prompt rather than transparently upgraded.
- [SL-03 L48] chunks.text is a zstd-compressed UTF-8 BLOB; heading_path joins headings with ' › ' (U+203A) and is empty-string for intro chunks (before any body heading).
- [SL-04 L85] Both title_fts and chunks_fts are FTS5 virtual tables with tokenize='porter unicode61 remove_diacritics 2' — Porter stemming with diacritic-insensitive matching tuned for English legal text.
- [SL-02 L123] chunks_vec is a vec0 virtual table — int8[EMBEDDING_DIM] with distance_metric=cosine — created at runtime after sqlite-vec is loaded as an extension; the DDL is not in schema.sql because it depends on the extension being available first.
