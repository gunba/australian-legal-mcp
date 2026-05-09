---
paths:
  - "src/ato_mcp/store/db.py"
---

# src/ato_mcp/store/db.py

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Storage Layer
SQLite schema, sqlite-vec virtual table, FTS5 with Porter stemmer, WAL+mmap, prepared queries, migration.

- [SL-05 L39] Connections enable WAL+synchronous=NORMAL for write modes only; read-only handles skip those pragmas. mmap_size=256 MB and temp_store=MEMORY are set unconditionally for both modes.
