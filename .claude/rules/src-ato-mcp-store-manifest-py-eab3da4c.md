---
paths:
  - "src/ato_mcp/store/manifest.py"
---

# src/ato_mcp/store/manifest.py

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Storage Layer
SQLite schema, sqlite-vec virtual table, FTS5 with Porter stemmer, WAL+mmap, prepared queries, migration.

- [SL-07 L172] Manifest signature verification calls the minisign CLI via subprocess rather than a Python library; the choice exercises the same verifier maintainers use offline so signing-key hygiene problems surface early.
- [SL-08 L195] diff_manifests compares doc refs by content_hash, pack_sha8, offset, and length to produce (added, changed, removed_doc_ids), so same-content repacks are still ingested by delta installs.
  - This keeps the Python maintainer diff aligned with the Rust updater: content-stable records whose pack byte range changes are treated as changed because pack-side fields such as definitions can change without a document content_hash change.
