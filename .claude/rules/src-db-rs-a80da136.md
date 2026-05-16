---
paths:
  - "src/db.rs"
---

# src/db.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust CLI Commands
Closed clap command surface covering end-user MCP/update/doctor/search commands plus maintainer source, build, and release commands in the Rust binary.

- [CC-04 L54] Runtime compatibility is fail-fast: open_read/open_write enforce the DB schema_version, and apply_update_locked rejects manifests whose schema_version or min_client_version exceeds the binary.
  - The Rust runtime does not run Python-era in-place migrations; incompatible or incomplete installs are rejected with reinstall/upgrade guidance.

## Rust Output Formatters
JSON output for hits, document outline + section + full renderers.

- [OF-01 L222] canonical_url is synthesised from doc_id by direct substitution into the ATO URL pattern; href is not stored separately so the link always reflects the current doc_id.

## Rust Storage Layer
SQLite schema, compressed chunk/html storage, FTS5, WAL write handles, pack/assets install, optional minisign release signatures, doc anchors, and derived citations.

- [SL-05 L22,35] Write connections enable foreign_keys, WAL, synchronous=NORMAL, and temp_store=MEMORY; read-only handles enable foreign_keys and temp_store=MEMORY without mutating WAL/synchronous settings.
- [SL-03 L115] chunks.text is a zstd-compressed UTF-8 BLOB. Heading and inline emphasis markers are rendered into the stored chunk text; there is no separate heading_path column.
- [SL-10 L151] doc_anchors stores in-document anchors, sister-document links, and historical-version pointers extracted at build time; get_doc_anchors also includes reverse citations from the citations table.
- [SL-11 L165] citations is a derived reverse-citation index populated from inline [doc:X] markers. It is keyed by source_chunk_id plus target_doc_id, indexed by target_doc_id, collapses qualifiers to the base doc_id, and skips self-citations.
- [SL-04 L180] Both title_fts and chunks_fts are FTS5 virtual tables using tokenize='porter unicode61 remove_diacritics 2' for stemmed, diacritic-insensitive English legal text search.
