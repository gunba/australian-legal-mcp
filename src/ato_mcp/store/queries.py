"""Prepared SQL queries used by maintainer build paths."""
from __future__ import annotations

INSERT_DOCUMENT = """
INSERT OR REPLACE INTO documents
    (doc_id, type, title, date, downloaded_at, content_hash, pack_sha8,
     html, withdrawn_date, superseded_by, replaces)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"""

DELETE_DOCUMENT = "DELETE FROM documents WHERE doc_id = ?"

INSERT_CHUNK = """
INSERT INTO chunks (doc_id, ord, heading_path, anchor, text)
VALUES (?, ?, ?, ?, ?)
"""

INSERT_CHUNK_FTS = """
INSERT INTO chunks_fts (rowid, text, heading_path) VALUES (?, ?, ?)
"""

INSERT_DEFINITION = """
INSERT OR REPLACE INTO definitions
    (definition_id, term, norm_term, doc_id, source_title, source_type, scope,
     heading_path, anchor, ord, body)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"""

INSERT_ASSET = """
INSERT OR REPLACE INTO document_assets
    (asset_ref, doc_id, source_path, relative_path, media_type, alt, title, sha256, bytes)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
"""

INSERT_TITLE_FTS = """
INSERT INTO title_fts (doc_id, title, headings) VALUES (?, ?, ?)
"""

DELETE_TITLE_FTS_BY_DOC = """
INSERT INTO title_fts (title_fts, doc_id, title, headings)
  SELECT 'delete', doc_id, title, headings FROM title_fts WHERE doc_id = ?
"""

INSERT_VEC = "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (?, ?)"

SELECT_CHUNKS_FOR_DOC = """
SELECT chunk_id, ord, heading_path, anchor, text
FROM chunks WHERE doc_id = ? ORDER BY ord ASC
"""

SELECT_DOCUMENT = "SELECT * FROM documents WHERE doc_id = ?"

# Empty-shell tracker. UPSERT so build/retry runs can re-observe a doc
# without the PK constraint firing — the ``excluded.*`` refs pull from
# the would-be inserted row, bumping last_checked_at + the counter.
INSERT_EMPTY_SHELL = """
INSERT INTO empty_shells (doc_id, first_seen_at, last_checked_at, check_count, source)
VALUES (?, ?, ?, 1, ?)
ON CONFLICT(doc_id) DO UPDATE SET
    last_checked_at = excluded.last_checked_at,
    check_count     = empty_shells.check_count + 1,
    source          = COALESCE(excluded.source, empty_shells.source)
"""

DELETE_EMPTY_SHELL = "DELETE FROM empty_shells WHERE doc_id = ?"
