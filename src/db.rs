//! SQLite schema, connections, compression, and `meta` key/value access.

use crate::config::db_path;
use crate::SUPPORTED_SCHEMA_VERSION;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use std::fs;
use std::io::Cursor;
use std::path::Path;

pub(crate) fn open_read() -> Result<Connection> {
    let path = db_path()?;
    if !path.exists() {
        bail!(
            "no live DB found at {}; run `ato-mcp update` first",
            path.display()
        );
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening local corpus database")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // [SL-05] Read-only handles skip WAL/synchronous mutation pragmas but
    // still use in-memory temp storage for query work.
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    enforce_db_schema_version(&conn)?;
    Ok(conn)
}

pub(crate) fn open_write_at(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path).context("opening local corpus database for writing")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // [SL-05] Write handles enable WAL + synchronous=NORMAL and temp_store
    // MEMORY before schema initialization or mutation.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // Skip the schema check on a brand-new DB (no `meta` table yet);
    // `init_db` will populate it. For an existing DB, validate up front
    // so callers don't operate against an incompatible schema.
    if table_exists(&conn, "meta")? {
        enforce_db_schema_version(&conn)?;
    }
    Ok(conn)
}

/// Reject DBs whose stored `meta.schema_version` doesn't match what this
/// binary supports. A missing entry is treated as a corrupt/incomplete
/// install — refuse with a recovery hint rather than silently operating
/// on a DB that may be missing required tables/indexes.
pub(crate) fn enforce_db_schema_version(conn: &Connection) -> Result<()> {
    // [CC-04] DB compatibility is fail-fast; the Rust runtime does not run Python-era migrations.
    if !table_exists(conn, "meta")? {
        bail!(
            "no schema_version metadata; corpus may be corrupt or incomplete; run `ato-mcp update`"
        );
    }
    match get_meta(conn, "schema_version")? {
        None => bail!(
            "no schema_version metadata; corpus may be corrupt or incomplete; run `ato-mcp update`"
        ),
        Some(value) => {
            let parsed: u32 = value
                .parse()
                .with_context(|| format!("schema_version `{value}` is not a valid integer"))?;
            if parsed != SUPPORTED_SCHEMA_VERSION {
                bail!(
                    "DB schema version {parsed} not supported by this binary (expects {}); reinstall the corpus or upgrade ato-mcp",
                    SUPPORTED_SCHEMA_VERSION
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS documents (
            doc_id           TEXT PRIMARY KEY,
            type             TEXT NOT NULL,
            title            TEXT NOT NULL,
            date             TEXT,
            downloaded_at    TEXT NOT NULL,
            content_hash     TEXT NOT NULL,
            pack_sha8        TEXT NOT NULL,
            html             BLOB NOT NULL,
            withdrawn_date   TEXT,
            superseded_by    TEXT,
            replaces         TEXT,
            has_in_doc_links INTEGER NOT NULL DEFAULT 0,
            has_related_docs INTEGER NOT NULL DEFAULT 0,
            has_history      INTEGER NOT NULL DEFAULT 0,
            -- Newline-joined doc headings, captured at build time and used to
            -- seed title_fts.headings at install time after the FTS5 indexes
            -- are rebuilt from the shipped DB.
            headings         TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_doc_type ON documents(type);
        CREATE INDEX IF NOT EXISTS idx_doc_date ON documents(date);
        CREATE INDEX IF NOT EXISTS idx_doc_withdrawn ON documents(withdrawn_date);

        CREATE TABLE IF NOT EXISTS chunks (
            chunk_id      INTEGER PRIMARY KEY,
            doc_id        TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
            ord           INTEGER NOT NULL,
            anchor        TEXT,
            -- [SL-03] Chunk bodies are zstd-compressed UTF-8 BLOBs; heading
            -- and inline markers are part of the stored chunk text.
            text          BLOB NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_chunks_doc ON chunks(doc_id);
        CREATE INDEX IF NOT EXISTS idx_chunks_doc_ord ON chunks(doc_id, ord);

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

        -- [SL-12] document_assets stores image bytes inline so the release
        -- ships everything inside ato.db.zst; there is no on-disk live/assets/
        -- tree. get_asset SELECTs the data BLOB and returns MCP ImageContent.
        CREATE TABLE IF NOT EXISTS document_assets (
            asset_ref  TEXT PRIMARY KEY,
            doc_id     TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
            media_type TEXT,
            alt        TEXT,
            title      TEXT,
            sha256     TEXT NOT NULL,
            data       BLOB NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_assets_doc ON document_assets(doc_id);

        CREATE TABLE IF NOT EXISTS doc_anchors (
            -- [SL-10] Build-time anchors cover in-doc, sister-doc, and
            -- historical-version navigation for get_doc_anchors.
            doc_id           TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
            ord              INTEGER NOT NULL,
            kind             TEXT NOT NULL,
            label            TEXT NOT NULL,
            target_chunk_id  INTEGER,
            target_doc_id    TEXT,
            target_pit       TEXT,
            PRIMARY KEY (doc_id, ord)
        );
        CREATE INDEX IF NOT EXISTS idx_doc_anchors_doc ON doc_anchors(doc_id);

        CREATE TABLE IF NOT EXISTS citations (
            -- [SL-11] Reverse citations are derived from [doc:X] markers
            -- and keyed by source chunk + target doc.
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

        CREATE VIRTUAL TABLE IF NOT EXISTS title_fts USING fts5(
            -- [SL-04] FTS uses porter unicode61 with diacritic folding for
            -- English legal text in titles/headings and chunks.
            doc_id UNINDEXED,
            title,
            headings,
            tokenize = "porter unicode61 remove_diacritics 2"
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
            text,
            tokenize = "porter unicode61 remove_diacritics 2"
        );

        CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )?;
    set_meta(conn, "schema_version", &SUPPORTED_SCHEMA_VERSION.to_string())?;
    Ok(())
}

pub(crate) fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = ?")?;
    let mut rows = stmt.query([key])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}

pub(crate) fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub(crate) fn canonical_url(doc_id: &str) -> String {
    // [OF-01] canonical_url is synthesized directly from doc_id.
    format!("https://www.ato.gov.au/law/view/document?docid={}", doc_id)
}

pub(crate) fn decompress_text(blob: Vec<u8>) -> Result<String> {
    let bytes = zstd::stream::decode_all(Cursor::new(blob))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) fn compress_text(text: &str) -> Result<Vec<u8>> {
    Ok(zstd::stream::encode_all(Cursor::new(text.as_bytes()), 3)?)
}

pub(crate) fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type IN ('table', 'virtual table') AND name = ?1)",
        [table],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
}
