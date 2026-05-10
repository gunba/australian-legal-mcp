-- ato-mcp SQLite schema v7
-- Minimal field set: doc_id PK, type, title, date + 3 build-time columns +
-- 3 currency columns (W2.2) — withdrawn_date, superseded_by, replaces.
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
    replaces         TEXT
);
CREATE INDEX IF NOT EXISTS idx_doc_type ON documents(type);
CREATE INDEX IF NOT EXISTS idx_doc_date ON documents(date);
CREATE INDEX IF NOT EXISTS idx_doc_withdrawn ON documents(withdrawn_date);

-- Chunks: text is zstd-compressed UTF-8.
-- [SL-03] chunks.text is zstd-compressed UTF-8 BLOB; heading_path uses ' › ' (U+203A) separator; empty-string == intro chunk.
--
-- heading_path: nearest-heading trail joined with " › ". Front-matter
-- echoes (the document title and its " — "-separated components) are
-- stripped at chunk emission time by chunk.strip_title_prefix, so a TR's
-- "Ruling" section reads as "Ruling" rather than
-- "Taxation Ruling — TR 2024/3 — … › Taxation Ruling › TR 2024/3 › Ruling".
-- Empty string == intro chunk, before any body heading.
CREATE TABLE IF NOT EXISTS chunks (
    chunk_id      INTEGER PRIMARY KEY,
    doc_id        TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
    ord           INTEGER NOT NULL,
    heading_path  TEXT,
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
    heading_path  TEXT,
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

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    text,
    heading_path,
    tokenize = "porter unicode61 remove_diacritics 2"
);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
