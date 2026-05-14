"""Prepared SQL queries used by maintainer build paths."""
from __future__ import annotations

INSERT_DOCUMENT = """
INSERT OR REPLACE INTO documents
    (doc_id, type, title, date, downloaded_at, content_hash, pack_sha8,
     html, withdrawn_date, superseded_by, replaces,
     has_in_doc_links, has_related_docs, has_history)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"""

INSERT_CHUNK = """
INSERT INTO chunks (doc_id, ord, anchor, text)
VALUES (?, ?, ?, ?)
"""

INSERT_CHUNK_FTS = """
INSERT INTO chunks_fts (rowid, text) VALUES (?, ?)
"""

INSERT_DEFINITION = """
INSERT OR REPLACE INTO definitions
    (definition_id, term, norm_term, doc_id, source_title, source_type, scope,
     anchor, ord, body)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"""

INSERT_ASSET = """
INSERT OR REPLACE INTO document_assets
    (asset_ref, doc_id, source_path, relative_path, media_type, alt, title, sha256, bytes)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
"""

INSERT_TITLE_FTS = """
INSERT INTO title_fts (doc_id, title, headings) VALUES (?, ?, ?)
"""

INSERT_VEC = "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (?, ?)"

INSERT_DOC_ANCHOR = """
INSERT INTO doc_anchors
    (doc_id, ord, kind, label, target_chunk_id, target_doc_id, target_pit)
VALUES (?, ?, ?, ?, ?, ?, ?)
"""

UPDATE_DOC_NAVIGATION_FLAGS = """
UPDATE documents
SET has_in_doc_links = ?, has_related_docs = ?, has_history = ?
WHERE doc_id = ?
"""

# Citations: derived index of inline [doc:X] markers in chunk text. Built
# at the tail of every full build / incremental update by scanning chunks
# rather than from the extractor (the [doc:X] marker IS the resolved
# cross-reference, so a chunk-text scan is exact and avoids re-walking
# HTML).
INSERT_CITATION = """
INSERT OR IGNORE INTO citations (source_chunk_id, source_doc_id, target_doc_id)
VALUES (?, ?, ?)
"""

DELETE_ALL_CITATIONS = "DELETE FROM citations"
