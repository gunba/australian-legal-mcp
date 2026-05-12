-- ato-mcp SQLite schema v8
-- Minimal field set: doc_id PK, type, title, date + 3 build-time columns +
-- 3 currency columns (W2.2) — withdrawn_date, superseded_by, replaces +
-- 3 navigation flags. Historical (point-in-time) versions are NOT stored as
-- separate document rows; the existence of earlier versions is surfaced
-- through the doc_anchors table only.
--
-- Design notes:
--   doc_id   The full ATO docid path, slashes included. The canonical URL
--            is synthesised at query time as
--              https://www.ato.gov.au/law/view/document?docid={doc_id}
--            so we don't store ``href`` separately.
--   type     Top-level bucket ("Public_rulings", "Cases", ...). Finer
--            doc_type is implicit in the first segment of doc_id.
--   title    Human-readable label with citation inlined
--            ("TR 2024/3 — R&D tax incentive eligibility"). The rule
--            engine produces this; title_fts searches it directly.
--   date     Best-guess publication date (ISO yyyy-mm-dd). Used only for
--            filters and recency sort — not presented as authoritative.
--   withdrawn_date   ISO yyyy-mm-dd when the doc indicates withdrawal /
--                    supersession. NULL means the doc is currently in
--                    force. Default search excludes non-NULL rows; pass
--                    `current_only=false` to include them.
--   superseded_by    Short citation of the replacing doc (e.g. "TR 2022/1").
--   replaces         Short citation of the doc this one replaces.
--   has_in_doc_links / has_related_docs / has_history
--                    Navigation hints surfaced on slim search hits. Set at
--                    build time iff the doc emitted at least one anchor of
--                    that kind. Tells the agent it's worth calling
--                    get_doc_anchors(doc_id) to navigate.

PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS documents (
    doc_id           TEXT PRIMARY KEY,
    type             TEXT NOT NULL,
    title            TEXT NOT NULL,
    date             TEXT,
    -- build-time internals, never exposed via tools:
    downloaded_at    TEXT NOT NULL,
    content_hash     TEXT NOT NULL,
    pack_sha8        TEXT NOT NULL,
    html             BLOB NOT NULL,
    -- currency markers (W2.2):
    withdrawn_date   TEXT,
    superseded_by    TEXT,
    replaces         TEXT,
    -- navigation hints (set at build time from doc_anchors):
    has_in_doc_links INTEGER NOT NULL DEFAULT 0,
    has_related_docs INTEGER NOT NULL DEFAULT 0,
    has_history      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_doc_type ON documents(type);
CREATE INDEX IF NOT EXISTS idx_doc_date ON documents(date);
CREATE INDEX IF NOT EXISTS idx_doc_withdrawn ON documents(withdrawn_date);

-- Chunks: text is zstd-compressed UTF-8.
-- [SL-03] chunks.text is zstd-compressed UTF-8 BLOB. Heading text and
-- emphasis (h1-h6, strong, em, blockquote, pre, li, dt+dd) are rendered
-- inline using markdown markers so the embedder + BM25 see them as part
-- of the chunk body. There is no separate heading_path column.
CREATE TABLE IF NOT EXISTS chunks (
    chunk_id      INTEGER PRIMARY KEY,
    doc_id        TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
    ord           INTEGER NOT NULL,
    anchor        TEXT,
    text          BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_chunks_doc ON chunks(doc_id);

-- Definition index. The runtime reports empty definition coverage explicitly
-- instead of weakening normal search/document retrieval.
CREATE TABLE IF NOT EXISTS definitions (
    definition_id TEXT PRIMARY KEY,
    term          TEXT NOT NULL,
    norm_term     TEXT NOT NULL,
    doc_id        TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
    source_title  TEXT NOT NULL,
    source_type   TEXT NOT NULL,
    scope         TEXT,
    anchor        TEXT,
    ord           INTEGER NOT NULL,
    body          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_definitions_norm_term ON definitions(norm_term);
CREATE INDEX IF NOT EXISTS idx_definitions_doc ON definitions(doc_id);

CREATE TABLE IF NOT EXISTS document_assets (
    asset_ref     TEXT PRIMARY KEY,
    doc_id        TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
    source_path   TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    media_type    TEXT,
    alt           TEXT,
    title         TEXT,
    sha256        TEXT NOT NULL,
    bytes         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_assets_doc ON document_assets(doc_id);

-- [SL-05] doc_anchors stores in-doc navigation, sister-doc references,
-- and historical-version pointers extracted from <a href> markup at
-- build time. Surfaced through the get_doc_anchors MCP tool.
CREATE TABLE IF NOT EXISTS doc_anchors (
    doc_id           TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
    ord              INTEGER NOT NULL,
    kind             TEXT NOT NULL,  -- 'in_doc' | 'sister' | 'history'
    label            TEXT NOT NULL,
    target_chunk_id  INTEGER,         -- set for kind='in_doc'
    target_doc_id    TEXT,             -- set for kind='sister' | 'history'
    target_pit       TEXT,             -- set for kind='history'
    PRIMARY KEY (doc_id, ord)
);
CREATE INDEX IF NOT EXISTS idx_doc_anchors_doc ON doc_anchors(doc_id);

-- Reverse-citation index, derived from chunk text. Every `[doc:X]` marker
-- in a chunk becomes one (source_chunk_id, source_doc_id, target_doc_id)
-- row, deduplicated per (chunk, target). Surfaced via get_doc_anchors as
-- a `cited_by` array. Built at the tail of every build/update so it stays
-- in sync with chunks.
CREATE TABLE IF NOT EXISTS citations (
    source_chunk_id  INTEGER NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
    source_doc_id    TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
    target_doc_id    TEXT NOT NULL,
    PRIMARY KEY (source_chunk_id, target_doc_id)
);
CREATE INDEX IF NOT EXISTS idx_citations_target ON citations(target_doc_id);

CREATE TABLE IF NOT EXISTS chunk_embeddings (
    chunk_id   INTEGER PRIMARY KEY REFERENCES chunks(chunk_id) ON DELETE CASCADE,
    embedding  BLOB NOT NULL CHECK(length(embedding) = 256)
);

-- [SL-04] FTS5 with Porter stemming + unicode61 + diacritic-insensitive — tuned for English legal text in both title_fts and chunks_fts.
-- Title-level FTS — just the title plus per-doc heading text. Citations
-- like "TR 2024/3" live inside ``title`` so title_fts finds them.
CREATE VIRTUAL TABLE IF NOT EXISTS title_fts USING fts5(
    doc_id UNINDEXED,
    title,
    headings,
    tokenize = "porter unicode61 remove_diacritics 2"
);

-- chunks_fts indexes only the chunk body. Heading and emphasis terms
-- reach BM25 via inline markdown rendering in chunk.text — no separate
-- heading column means no per-column weighting tricks needed.
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    text,
    tokenize = "porter unicode61 remove_diacritics 2"
);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
