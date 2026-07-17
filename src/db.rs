//! SQLite schema, connections, compression, and scoped metadata access.

use crate::config::db_path;
use crate::SUPPORTED_SCHEMA_VERSION;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Cursor;
use std::path::Path;

pub(crate) const CHUNKS_FTS_V11_SQL: &str = r#"CREATE VIRTUAL TABLE chunks_fts USING fts5(
    text,
    content = '',
    contentless_delete = 1,
    tokenize = "porter unicode61 remove_diacritics 2"
)"#;

pub(crate) fn open_read() -> Result<Connection> {
    let path = db_path()?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "no live DB found at {}; activate a corpus generation first",
                path.display()
            )
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "live DB must be a regular non-symlink file at {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            bail!("live DB must not be hard-linked: {}", path.display());
        }
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening local corpus database")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Read-only handles skip WAL/synchronous mutation pragmas but
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
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    validate_existing_database(&conn)?;
    // Corpus builds use a rollback journal so millions of updates to the same FTS and
    // index pages remain in the page cache instead of becoming repeated WAL frames.
    // Each source transaction is still atomic and resumable after interruption.
    conn.pragma_update(None, "journal_mode", "TRUNCATE")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "cache_size", -1_048_576_i64)?;
    conn.pragma_update(None, "mmap_size", 8_589_934_592_i64)?;
    conn.pragma_update(None, "journal_size_limit", 0_i64)?;
    Ok(conn)
}

fn validate_existing_database(conn: &Connection) -> Result<()> {
    if table_exists(conn, "corpus_meta")? {
        enforce_db_schema_version(conn)?;
    } else if database_has_user_schema(conn)? {
        bail!(
            "no corpus_meta.schema_version metadata; corpus may be corrupt or incomplete; install a complete corpus generation"
        );
    }
    Ok(())
}

fn database_has_user_schema(conn: &Connection) -> Result<bool> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name NOT LIKE 'sqlite_%')",
        [],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
}

/// Reject databases whose stored schema version does not match this binary.
pub(crate) fn enforce_db_schema_version(conn: &Connection) -> Result<()> {
    if !table_exists(conn, "corpus_meta")? {
        bail!(
            "no corpus_meta.schema_version metadata; corpus may be corrupt or incomplete; install a complete corpus generation"
        );
    }
    match get_corpus_meta(conn, "schema_version")? {
        None => bail!(
            "no corpus_meta.schema_version metadata; corpus may be corrupt or incomplete; install a complete corpus generation"
        ),
        Some(value) => {
            let parsed: u32 = value
                .parse()
                .with_context(|| format!("schema_version `{value}` is not a valid integer"))?;
            if parsed != SUPPORTED_SCHEMA_VERSION {
                bail!(
                    "DB schema version {parsed} not supported by this binary (expects {}); reinstall the corpus or upgrade this binary",
                    SUPPORTED_SCHEMA_VERSION
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn init_db(conn: &Connection) -> Result<()> {
    validate_existing_database(conn)?;
    conn.pragma_update(None, "journal_mode", "TRUNCATE")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sources (
            source_id    TEXT NOT NULL PRIMARY KEY,
            display_name TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS corpus_meta (
            key   TEXT NOT NULL PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS source_meta (
            source_id TEXT NOT NULL,
            key       TEXT NOT NULL,
            value     TEXT NOT NULL,
            PRIMARY KEY (source_id, key),
            FOREIGN KEY (source_id) REFERENCES sources(source_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS documents (
            source_id         TEXT NOT NULL,
            native_id         TEXT NOT NULL,
            type              TEXT NOT NULL,
            title             TEXT NOT NULL,
            date              TEXT,
            canonical_url     TEXT NOT NULL,
            upstream_version  TEXT,
            downloaded_at     TEXT NOT NULL,
            content_hash      TEXT NOT NULL,
            html              BLOB NOT NULL,
            withdrawn_date    TEXT,
            superseded_by     TEXT,
            replaces          TEXT,
            has_in_doc_links  INTEGER NOT NULL DEFAULT 0,
            has_related_docs  INTEGER NOT NULL DEFAULT 0,
            has_history       INTEGER NOT NULL DEFAULT 0,
            headings          TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (source_id, native_id),
            FOREIGN KEY (source_id) REFERENCES sources(source_id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_documents_source_type
            ON documents(source_id, type);
        CREATE INDEX IF NOT EXISTS idx_documents_source_date
            ON documents(source_id, date);
        CREATE INDEX IF NOT EXISTS idx_documents_source_withdrawn
            ON documents(source_id, withdrawn_date);

        CREATE TABLE IF NOT EXISTS chunks (
            chunk_id  INTEGER PRIMARY KEY,
            source_id TEXT NOT NULL,
            native_id TEXT NOT NULL,
            ord       INTEGER NOT NULL,
            anchor    TEXT,
            -- Chunk bodies are zstd-compressed UTF-8 BLOBs; heading
            -- and inline markers are part of the stored chunk text.
            text      BLOB NOT NULL,
            FOREIGN KEY (source_id, native_id)
                REFERENCES documents(source_id, native_id) ON DELETE CASCADE,
            UNIQUE (source_id, native_id, ord),
            UNIQUE (source_id, chunk_id)
        );

        CREATE TABLE IF NOT EXISTS definitions (
            source_id     TEXT NOT NULL,
            definition_id TEXT NOT NULL,
            term          TEXT NOT NULL,
            norm_term     TEXT NOT NULL,
            native_id     TEXT NOT NULL,
            source_title  TEXT NOT NULL,
            source_type   TEXT NOT NULL,
            scope         TEXT,
            anchor        TEXT,
            ord           INTEGER NOT NULL,
            body          TEXT NOT NULL,
            PRIMARY KEY (source_id, definition_id),
            FOREIGN KEY (source_id, native_id)
                REFERENCES documents(source_id, native_id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_definitions_source_norm_term
            ON definitions(source_id, norm_term);
        CREATE INDEX IF NOT EXISTS idx_definitions_source_native
            ON definitions(source_id, native_id);

        -- Assets remain inline so an immutable database snapshot is
        -- sufficient to resolve every retained image.
        CREATE TABLE IF NOT EXISTS document_assets (
            source_id  TEXT NOT NULL,
            asset_id   TEXT NOT NULL,
            native_id  TEXT NOT NULL,
            media_type TEXT,
            alt        TEXT,
            title      TEXT,
            sha256     TEXT NOT NULL,
            data       BLOB NOT NULL,
            PRIMARY KEY (source_id, asset_id),
            FOREIGN KEY (source_id, native_id)
                REFERENCES documents(source_id, native_id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_document_assets_source_native
            ON document_assets(source_id, native_id);

        CREATE TABLE IF NOT EXISTS doc_anchors (
            source_id        TEXT NOT NULL,
            native_id        TEXT NOT NULL,
            ord              INTEGER NOT NULL,
            kind             TEXT NOT NULL,
            label            TEXT NOT NULL,
            target_source_id TEXT,
            target_native_id TEXT,
            target_chunk_id  INTEGER,
            target_pit       TEXT,
            PRIMARY KEY (source_id, native_id, ord),
            FOREIGN KEY (source_id, native_id)
                REFERENCES documents(source_id, native_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS citations (
            source_chunk_id  INTEGER NOT NULL,
            source_id        TEXT NOT NULL,
            source_native_id TEXT NOT NULL,
            target_source_id TEXT NOT NULL,
            target_native_id TEXT NOT NULL,
            PRIMARY KEY (source_chunk_id, target_source_id, target_native_id),
            FOREIGN KEY (source_id, source_chunk_id)
                REFERENCES chunks(source_id, chunk_id) ON DELETE CASCADE,
            FOREIGN KEY (source_id, source_native_id)
                REFERENCES documents(source_id, native_id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_citations_source_chunk
            ON citations(source_id, source_chunk_id);
        CREATE INDEX IF NOT EXISTS idx_citations_source_native
            ON citations(source_id, source_native_id);
        CREATE INDEX IF NOT EXISTS idx_citations_target_source
            ON citations(
                target_source_id, target_native_id, source_id, source_native_id
            );

        CREATE TABLE IF NOT EXISTS embedding_cache (
            model_id    TEXT NOT NULL,
            text_sha256 TEXT NOT NULL CHECK(
                length(text_sha256) = 64
                AND text_sha256 = lower(text_sha256)
            ),
            embedding   BLOB NOT NULL CHECK(length(embedding) = 256),
            PRIMARY KEY (model_id, text_sha256)
        ) WITHOUT ROWID;

        CREATE TABLE IF NOT EXISTS chunk_embeddings (
            chunk_id  INTEGER PRIMARY KEY REFERENCES chunks(chunk_id) ON DELETE CASCADE,
            embedding BLOB NOT NULL CHECK(length(embedding) = 256)
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS title_fts USING fts5(
            source_id UNINDEXED,
            native_id UNINDEXED,
            title,
            headings,
            tokenize = "porter unicode61 remove_diacritics 2"
        );

        "#,
    )?;
    if !table_exists(&tx, "chunks_fts")? {
        tx.execute_batch(CHUNKS_FTS_V11_SQL)?;
    }
    set_corpus_meta(&tx, "schema_version", &SUPPORTED_SCHEMA_VERSION.to_string())?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn validate_chunks_fts_schema(conn: &Connection) -> Result<()> {
    let actual = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'chunks_fts'",
            [],
            |row| row.get::<_, String>(0),
        )
        .context("reading chunks_fts schema")?;
    if normalized_sql(&actual) != normalized_sql(CHUNKS_FTS_V11_SQL) {
        bail!(
            "chunks_fts does not match the schema-{SUPPORTED_SCHEMA_VERSION} contentless-delete contract"
        );
    }
    Ok(())
}

pub(crate) fn verify_fts_relational_bindings(conn: &Connection) -> Result<()> {
    for (table, expected) in [("chunks_fts", "chunks"), ("title_fts", "documents")] {
        let indexed: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
        let source: i64 =
            conn.query_row(&format!("SELECT COUNT(*) FROM {expected}"), [], |row| {
                row.get(0)
            })?;
        if indexed != source {
            bail!("generation FTS table `{table}` has {indexed} rows; expected {source}");
        }
    }

    if !ordered_i64_queries_match(
        conn,
        "SELECT chunk_id FROM chunks ORDER BY chunk_id",
        "SELECT rowid FROM chunks_fts ORDER BY rowid",
    )? {
        bail!("chunks_fts rowids do not exactly match chunks.chunk_id");
    }

    if !ordered_text_pair_queries_match(
        conn,
        "SELECT source_id, native_id FROM documents ORDER BY source_id, native_id",
        "SELECT source_id, native_id FROM title_fts ORDER BY source_id, native_id",
    )? {
        bail!("title_fts identities do not exactly match documents");
    }
    Ok(())
}

fn ordered_i64_queries_match(conn: &Connection, left: &str, right: &str) -> Result<bool> {
    let mut left_statement = conn.prepare(left)?;
    let mut right_statement = conn.prepare(right)?;
    let mut left_rows = left_statement.query([])?;
    let mut right_rows = right_statement.query([])?;
    loop {
        match (left_rows.next()?, right_rows.next()?) {
            (Some(left), Some(right)) if left.get::<_, i64>(0)? == right.get::<_, i64>(0)? => {}
            (None, None) => return Ok(true),
            _ => return Ok(false),
        }
    }
}

fn ordered_text_pair_queries_match(conn: &Connection, left: &str, right: &str) -> Result<bool> {
    let mut left_statement = conn.prepare(left)?;
    let mut right_statement = conn.prepare(right)?;
    let mut left_rows = left_statement.query([])?;
    let mut right_rows = right_statement.query([])?;
    loop {
        match (left_rows.next()?, right_rows.next()?) {
            (Some(left), Some(right))
                if left.get::<_, String>(0)? == right.get::<_, String>(0)?
                    && left.get::<_, String>(1)? == right.get::<_, String>(1)? => {}
            (None, None) => return Ok(true),
            _ => return Ok(false),
        }
    }
}

fn validate_fts_digest_table(conn: &Connection, table: &str) -> Result<()> {
    if table.is_empty()
        || !table
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        bail!("FTS digest table name is malformed");
    }
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_schema
             WHERE type = 'table' AND name = ?1
         )",
        [table],
        |row| row.get(0),
    )?;
    if exists != 1 {
        bail!("FTS digest table `{table}` does not exist");
    }
    Ok(())
}

pub(crate) fn chunks_fts_logical_sha256(conn: &Connection, table: &str) -> Result<String> {
    validate_fts_digest_table(conn, table)?;

    let original_query_only: i64 = conn.pragma_query_value(None, "query_only", |row| row.get(0))?;
    conn.pragma_update(None, "query_only", "OFF")?;
    let result = (|| -> Result<String> {
        conn.execute_batch("DROP TABLE IF EXISTS temp.chunks_fts_digest_vocab;")?;
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE temp.chunks_fts_digest_vocab
             USING fts5vocab(main, {table}, instance);"
        ))?;
        let digest = (|| -> Result<String> {
            // FTS5 vocab consumes ORDER BY term and emits each term's
            // instances in doc/column/offset order. The monotonic assertion
            // makes that streaming contract fail closed without a temp sort.
            let mut statement = conn.prepare(
                "SELECT term, doc, col, offset
                 FROM temp.chunks_fts_digest_vocab
                 ORDER BY term",
            )?;
            let mut rows = statement.query([])?;
            let mut previous_term = String::new();
            let mut previous_doc = 0_i64;
            let mut previous_column = String::new();
            let mut previous_offset = 0_i64;
            let mut have_previous = false;
            let mut count = 0_u64;
            let mut hasher = Sha256::new();
            hasher.update(b"australian-legal-mcp-chunks-fts-instance-v1\0");
            while let Some(row) = rows.next()? {
                let term = row.get_ref(0)?.as_str()?;
                let doc = row.get::<_, i64>(1)?;
                let column = row.get_ref(2)?.as_str()?;
                let offset = row.get::<_, i64>(3)?;
                if have_previous
                    && (previous_term.as_str() > term
                        || (previous_term == term
                            && (previous_doc, previous_column.as_str(), previous_offset)
                                >= (doc, column, offset)))
                {
                    bail!("FTS vocabulary instances are not strictly ordered");
                }
                hash_digest_field(&mut hasher, term.as_bytes());
                hasher.update(doc.to_le_bytes());
                hash_digest_field(&mut hasher, column.as_bytes());
                hasher.update(offset.to_le_bytes());
                if previous_term != term {
                    previous_term.clear();
                    previous_term.push_str(term);
                }
                previous_doc = doc;
                previous_column.clear();
                previous_column.push_str(column);
                previous_offset = offset;
                have_previous = true;
                count = count
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("FTS vocabulary count overflow"))?;
            }
            hasher.update(count.to_le_bytes());
            hash_fts_bm25_metadata(&mut hasher, conn, table)?;
            Ok(format!("{:x}", hasher.finalize()))
        })();
        let cleanup = conn.execute_batch("DROP TABLE temp.chunks_fts_digest_vocab;");
        let digest = digest?;
        cleanup?;
        Ok(digest)
    })();
    let restore = conn.pragma_update(None, "query_only", original_query_only);
    match (result, restore) {
        (Ok(digest), Ok(())) => Ok(digest),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(error), Err(restore_error)) => Err(error).context(format!(
            "restoring SQLite query_only after FTS digest also failed: {restore_error}"
        )),
    }
}

pub(crate) fn chunks_fts_index_sha256(conn: &Connection, table: &str) -> Result<String> {
    validate_fts_digest_table(conn, table)?;
    let mut hasher = Sha256::new();
    hasher.update(b"australian-legal-mcp-chunks-fts-storage-v1\0");

    hasher.update(b"data\0");
    let mut data_count = 0_u64;
    let mut statement = conn.prepare(&format!("SELECT id, block FROM {table}_data ORDER BY id"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        hasher.update(row.get::<_, i64>(0)?.to_le_bytes());
        hash_digest_field(&mut hasher, row.get_ref(1)?.as_blob()?);
        data_count = data_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("FTS data row count overflow"))?;
    }
    hasher.update(data_count.to_le_bytes());
    drop(rows);
    drop(statement);

    hasher.update(b"index\0");
    let mut index_count = 0_u64;
    let mut statement = conn.prepare(&format!(
        "SELECT segid, term, pgno FROM {table}_idx ORDER BY segid, term"
    ))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        hasher.update(row.get::<_, i64>(0)?.to_le_bytes());
        hash_digest_field(&mut hasher, row.get_ref(1)?.as_blob()?);
        hasher.update(row.get::<_, i64>(2)?.to_le_bytes());
        index_count = index_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("FTS index row count overflow"))?;
    }
    hasher.update(index_count.to_le_bytes());
    drop(rows);
    drop(statement);

    hash_fts_docsize(&mut hasher, conn, table)?;

    hasher.update(b"config\0");
    let mut config_count = 0_u64;
    let mut statement = conn.prepare(&format!(
        "SELECT k, typeof(v), quote(v) FROM {table}_config ORDER BY k"
    ))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        hash_digest_field(&mut hasher, row.get::<_, String>(0)?.as_bytes());
        hash_digest_field(&mut hasher, row.get::<_, String>(1)?.as_bytes());
        hash_digest_field(&mut hasher, row.get::<_, String>(2)?.as_bytes());
        config_count = config_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("FTS config row count overflow"))?;
    }
    hasher.update(config_count.to_le_bytes());
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_fts_bm25_metadata(hasher: &mut Sha256, conn: &Connection, table: &str) -> Result<()> {
    hash_fts_docsize(hasher, conn, table)?;
    let averages = conn
        .query_row(
            &format!("SELECT block FROM {table}_data WHERE id = 1"),
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .context("reading FTS averages record")?;
    hasher.update(b"averages\0");
    hash_digest_field(hasher, &averages);
    Ok(())
}

fn hash_fts_docsize(hasher: &mut Sha256, conn: &Connection, table: &str) -> Result<()> {
    hasher.update(b"docsize\0");
    let mut count = 0_u64;
    let mut statement = conn.prepare(&format!("SELECT id, sz FROM {table}_docsize ORDER BY id"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        hasher.update(row.get::<_, i64>(0)?.to_le_bytes());
        hash_digest_field(hasher, row.get_ref(1)?.as_blob()?);
        count = count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("FTS docsize row count overflow"))?;
    }
    hasher.update(count.to_le_bytes());
    Ok(())
}

pub(crate) fn verify_chunks_fts_index_digest(conn: &Connection) -> Result<()> {
    let expected = get_corpus_meta(conn, "chunks_fts_index_sha256")?.ok_or_else(|| {
        anyhow::anyhow!("database is missing corpus_meta.chunks_fts_index_sha256")
    })?;
    if expected.len() != 64
        || !expected
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("corpus_meta.chunks_fts_index_sha256 is malformed");
    }
    let actual = chunks_fts_index_sha256(conn, "chunks_fts")?;
    if actual != expected {
        bail!("chunks_fts index storage or BM25 metadata does not match its schema-11 digest");
    }
    Ok(())
}

fn hash_digest_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

pub(crate) fn normalized_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn get_corpus_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM corpus_meta WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}

pub(crate) fn set_corpus_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO corpus_meta(key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub(crate) fn get_source_meta(
    conn: &Connection,
    source_id: &str,
    key: &str,
) -> Result<Option<String>> {
    let mut stmt =
        conn.prepare("SELECT value FROM source_meta WHERE source_id = ?1 AND key = ?2")?;
    let mut rows = stmt.query(params![source_id, key])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}

pub(crate) fn set_source_meta(
    conn: &Connection,
    source_id: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO source_meta(source_id, key, value) VALUES (?1, ?2, ?3) \
         ON CONFLICT(source_id, key) DO UPDATE SET value = excluded.value",
        params![source_id, key, value],
    )?;
    Ok(())
}

pub(crate) fn decompress_text(blob: Vec<u8>) -> Result<String> {
    let bytes = zstd::stream::decode_all(Cursor::new(blob))?;
    Ok(String::from_utf8(bytes)?)
}

#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    type ForeignKey = (String, Vec<String>, Vec<String>, String);
    type ForeignKeyGroup = (String, String, Vec<(i64, String, String)>);

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn table_info(conn: &Connection, table: &str) -> Result<Vec<(String, i64)>> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |row| Ok((row.get(1)?, row.get(5)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
        Ok(table_info(conn, table)?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }

    fn primary_key_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
        let mut columns: Vec<(i64, String)> = table_info(conn, table)?
            .into_iter()
            .filter_map(|(name, position)| (position != 0).then_some((position, name)))
            .collect();
        columns.sort_by_key(|(position, _)| *position);
        Ok(columns.into_iter().map(|(_, name)| name).collect())
    }

    fn unique_keys(conn: &Connection, table: &str) -> Result<BTreeSet<Vec<String>>> {
        let mut stmt = conn.prepare(&format!("PRAGMA index_list({table})"))?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
        })?;
        let indexes = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut keys = BTreeSet::new();
        for (index, unique) in indexes {
            if unique == 0 {
                continue;
            }
            let escaped = index.replace('\'', "''");
            let mut columns = conn.prepare(&format!("PRAGMA index_info('{escaped}')"))?;
            let rows = columns.query_map([], |row| row.get::<_, String>(2))?;
            keys.insert(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(keys)
    }

    fn foreign_keys(conn: &Connection, table: &str) -> Result<BTreeSet<ForeignKey>> {
        let mut stmt = conn.prepare(&format!("PRAGMA foreign_key_list({table})"))?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        let mut grouped: BTreeMap<i64, ForeignKeyGroup> = BTreeMap::new();
        for row in rows {
            let (id, sequence, target, from, to, on_delete) = row?;
            grouped
                .entry(id)
                .or_insert_with(|| (target, on_delete, Vec::new()))
                .2
                .push((sequence, from, to));
        }
        Ok(grouped
            .into_values()
            .map(|(target, on_delete, mut columns)| {
                columns.sort_by_key(|(sequence, _, _)| *sequence);
                let from = columns.iter().map(|(_, from, _)| from.clone()).collect();
                let to = columns.into_iter().map(|(_, _, to)| to).collect();
                (target, from, to, on_delete)
            })
            .collect())
    }

    fn query_plan(conn: &Connection, sql: &str) -> Result<String> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(3))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.join(" | "))
    }

    #[test]
    fn final_schema_has_source_qualified_keys_and_metadata() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        init_db(&conn)?;

        assert!(table_exists(&conn, "sources")?);
        assert!(table_exists(&conn, "corpus_meta")?);
        assert!(table_exists(&conn, "source_meta")?);
        assert_eq!(
            get_corpus_meta(&conn, "schema_version")?,
            Some(SUPPORTED_SCHEMA_VERSION.to_string())
        );

        assert_eq!(
            primary_key_columns(&conn, "sources")?,
            names(&["source_id"])
        );
        assert_eq!(primary_key_columns(&conn, "corpus_meta")?, names(&["key"]));
        assert_eq!(
            primary_key_columns(&conn, "source_meta")?,
            names(&["source_id", "key"])
        );
        assert_eq!(
            primary_key_columns(&conn, "documents")?,
            names(&["source_id", "native_id"])
        );
        assert_eq!(primary_key_columns(&conn, "chunks")?, names(&["chunk_id"]));
        assert_eq!(
            primary_key_columns(&conn, "definitions")?,
            names(&["source_id", "definition_id"])
        );
        assert_eq!(
            primary_key_columns(&conn, "document_assets")?,
            names(&["source_id", "asset_id"])
        );
        assert_eq!(
            primary_key_columns(&conn, "doc_anchors")?,
            names(&["source_id", "native_id", "ord"])
        );
        assert_eq!(
            primary_key_columns(&conn, "citations")?,
            names(&["source_chunk_id", "target_source_id", "target_native_id"])
        );
        assert_eq!(
            primary_key_columns(&conn, "chunk_embeddings")?,
            names(&["chunk_id"])
        );

        let chunk_keys = unique_keys(&conn, "chunks")?;
        assert!(chunk_keys.contains(&names(&["source_id", "native_id", "ord"])));
        assert!(chunk_keys.contains(&names(&["source_id", "chunk_id"])));

        assert_eq!(
            table_columns(&conn, "documents")?,
            names(&[
                "source_id",
                "native_id",
                "type",
                "title",
                "date",
                "canonical_url",
                "upstream_version",
                "downloaded_at",
                "content_hash",
                "html",
                "withdrawn_date",
                "superseded_by",
                "replaces",
                "has_in_doc_links",
                "has_related_docs",
                "has_history",
                "headings",
            ])
        );
        assert_eq!(
            table_columns(&conn, "title_fts")?,
            names(&["source_id", "native_id", "title", "headings"])
        );
        let title_fts_sql: String = conn.query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'title_fts'",
            [],
            |row| row.get(0),
        )?;
        assert!(title_fts_sql.contains("source_id UNINDEXED"));
        assert!(title_fts_sql.contains("native_id UNINDEXED"));
        validate_chunks_fts_schema(&conn)?;

        conn.execute(
            "INSERT INTO sources(source_id, display_name) VALUES ('ato', 'Australian Taxation Office')",
            [],
        )?;
        set_corpus_meta(&conn, "generation", "2026-07-11")?;
        set_source_meta(&conn, "ato", "cursor", "42")?;
        assert_eq!(
            get_corpus_meta(&conn, "generation")?,
            Some("2026-07-11".to_string())
        );
        assert_eq!(
            get_source_meta(&conn, "ato", "cursor")?,
            Some("42".to_string())
        );

        conn.execute(
            "INSERT INTO title_fts(source_id, native_id, title, headings) \
             VALUES ('ato', 'TR/1', 'Deductions', 'Business deductions')",
            [],
        )?;
        conn.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (41, 'business deductions')",
            [],
        )?;
        let fts_identity: (String, String) = conn.query_row(
            "SELECT source_id, native_id FROM title_fts WHERE title_fts MATCH 'deductions'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(fts_identity, ("ato".to_string(), "TR/1".to_string()));
        let chunk_rowid: i64 = conn.query_row(
            "SELECT rowid FROM chunks_fts WHERE chunks_fts MATCH 'business'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(chunk_rowid, 41);
        let stored_text: Option<String> =
            conn.query_row("SELECT text FROM chunks_fts WHERE rowid = 41", [], |row| {
                row.get(0)
            })?;
        assert_eq!(stored_text, None, "chunks_fts must be contentless");
        conn.execute("DELETE FROM chunks_fts WHERE rowid = 41", [])?;
        let remaining: i64 =
            conn.query_row("SELECT COUNT(*) FROM chunks_fts", [], |row| row.get(0))?;
        assert_eq!(
            remaining, 0,
            "contentless-delete must support normal DELETE"
        );
        Ok(())
    }

    #[test]
    fn chunks_fts_schema_rejects_contentful_or_incompatible_tables() -> Result<()> {
        for ddl in [
            r#"CREATE VIRTUAL TABLE chunks_fts USING fts5(
                text,
                tokenize = "porter unicode61 remove_diacritics 2"
            )"#,
            r#"CREATE VIRTUAL TABLE chunks_fts USING fts5(
                text,
                content = '',
                tokenize = "porter unicode61 remove_diacritics 2"
            )"#,
            r#"CREATE VIRTUAL TABLE chunks_fts USING fts5(
                text,
                content = '',
                contentless_delete = 1,
                tokenize = "unicode61"
            )"#,
        ] {
            let conn = Connection::open_in_memory()?;
            conn.execute_batch(ddl)?;
            assert!(validate_chunks_fts_schema(&conn).is_err(), "accepted {ddl}");
        }
        Ok(())
    }

    #[test]
    fn fts_relational_bindings_require_exact_chunk_rowids() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        init_db(&conn)?;
        conn.execute_batch(
            "INSERT INTO sources(source_id, display_name) VALUES ('ato', 'ATO');
             INSERT INTO documents(
                 source_id, native_id, type, title, canonical_url, downloaded_at,
                 content_hash, html
             ) VALUES (
                 'ato', 'doc', 'ruling', 'Document', 'https://example.invalid/doc',
                 '2026-01-01T00:00:00Z', 'hash', X'00'
             );
             INSERT INTO chunks(chunk_id, source_id, native_id, ord, text)
             VALUES (1, 'ato', 'doc', 0, X'00');
             INSERT INTO title_fts(rowid, source_id, native_id, title, headings)
             VALUES (1, 'ato', 'doc', 'Document', '');
             INSERT INTO chunks_fts(rowid, text) VALUES (2, 'wrong identity');",
        )?;
        let error = verify_fts_relational_bindings(&conn).unwrap_err();
        assert!(error.to_string().contains("rowids"));
        Ok(())
    }

    #[test]
    fn chunks_fts_integrity_digest_binds_postings_and_bm25_metadata() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        init_db(&conn)?;
        conn.execute_batch(
            "INSERT INTO chunks_fts(rowid, text) VALUES
                 (1, 'research development incentive'),
                 (2, 'documentary evidence');",
        )?;
        let digest = chunks_fts_index_sha256(&conn, "chunks_fts")?;
        set_corpus_meta(&conn, "chunks_fts_index_sha256", &digest)?;
        verify_chunks_fts_index_digest(&conn)?;

        conn.execute("DELETE FROM chunks_fts WHERE rowid = 2", [])?;
        conn.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (2, 'unrelated garbage')",
            [],
        )?;
        let error = verify_chunks_fts_index_digest(&conn).unwrap_err();
        assert!(error.to_string().contains("index storage"));

        conn.execute("DELETE FROM chunks_fts WHERE rowid = 2", [])?;
        conn.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (2, 'documentary evidence')",
            [],
        )?;
        let repaired_digest = chunks_fts_index_sha256(&conn, "chunks_fts")?;
        set_corpus_meta(&conn, "chunks_fts_index_sha256", &repaired_digest)?;
        verify_chunks_fts_index_digest(&conn)?;
        let logical_digest = chunks_fts_logical_sha256(&conn, "chunks_fts")?;
        let baseline_score: f64 = conn.query_row(
            "SELECT bm25(chunks_fts) FROM chunks_fts
             WHERE rowid = 1 AND chunks_fts MATCH 'research'",
            [],
            |row| row.get(0),
        )?;
        let original_docsize: Vec<u8> = conn.query_row(
            "SELECT sz FROM chunks_fts_docsize WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        conn.execute("UPDATE chunks_fts_docsize SET sz = X'64' WHERE id = 1", [])?;
        let changed_docsize_score: f64 = conn.query_row(
            "SELECT bm25(chunks_fts) FROM chunks_fts
             WHERE rowid = 1 AND chunks_fts MATCH 'research'",
            [],
            |row| row.get(0),
        )?;
        assert_ne!(changed_docsize_score, baseline_score);
        assert_ne!(
            chunks_fts_logical_sha256(&conn, "chunks_fts")?,
            logical_digest
        );
        assert!(verify_chunks_fts_index_digest(&conn).is_err());
        conn.execute(
            "UPDATE chunks_fts_docsize SET sz = ?1 WHERE id = 1",
            [original_docsize],
        )?;
        assert_eq!(
            chunks_fts_logical_sha256(&conn, "chunks_fts")?,
            logical_digest
        );
        verify_chunks_fts_index_digest(&conn)?;

        let original_averages: Vec<u8> = conn.query_row(
            "SELECT block FROM chunks_fts_data WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        conn.execute(
            "UPDATE chunks_fts_data SET block = X'0264' WHERE id = 1",
            [],
        )?;
        let changed_average_score: f64 = conn.query_row(
            "SELECT bm25(chunks_fts) FROM chunks_fts
             WHERE rowid = 1 AND chunks_fts MATCH 'research'",
            [],
            |row| row.get(0),
        )?;
        assert_ne!(changed_average_score, baseline_score);
        assert_ne!(
            chunks_fts_logical_sha256(&conn, "chunks_fts")?,
            logical_digest
        );
        conn.execute(
            "INSERT INTO chunks_fts(chunks_fts) VALUES('integrity-check')",
            [],
        )?;
        assert!(verify_chunks_fts_index_digest(&conn).is_err());
        conn.execute(
            "UPDATE chunks_fts_data SET block = ?1 WHERE id = 1",
            [original_averages],
        )?;
        assert_eq!(
            chunks_fts_logical_sha256(&conn, "chunks_fts")?,
            logical_digest
        );
        verify_chunks_fts_index_digest(&conn)?;
        let storage_before_vacuum = chunks_fts_index_sha256(&conn, "chunks_fts")?;
        conn.execute_batch("VACUUM")?;
        assert_eq!(
            chunks_fts_index_sha256(&conn, "chunks_fts")?,
            storage_before_vacuum
        );
        verify_chunks_fts_index_digest(&conn)?;
        Ok(())
    }

    #[test]
    fn source_qualified_foreign_keys_reject_mismatches_and_cascade() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        init_db(&conn)?;

        assert_eq!(
            foreign_keys(&conn, "source_meta")?,
            BTreeSet::from([(
                "sources".to_string(),
                names(&["source_id"]),
                names(&["source_id"]),
                "CASCADE".to_string(),
            )])
        );
        assert_eq!(
            foreign_keys(&conn, "documents")?,
            BTreeSet::from([(
                "sources".to_string(),
                names(&["source_id"]),
                names(&["source_id"]),
                "CASCADE".to_string(),
            )])
        );
        assert_eq!(
            foreign_keys(&conn, "chunks")?,
            BTreeSet::from([(
                "documents".to_string(),
                names(&["source_id", "native_id"]),
                names(&["source_id", "native_id"]),
                "CASCADE".to_string(),
            )])
        );
        assert_eq!(
            foreign_keys(&conn, "definitions")?,
            BTreeSet::from([(
                "documents".to_string(),
                names(&["source_id", "native_id"]),
                names(&["source_id", "native_id"]),
                "CASCADE".to_string(),
            )])
        );
        assert_eq!(
            foreign_keys(&conn, "document_assets")?,
            BTreeSet::from([(
                "documents".to_string(),
                names(&["source_id", "native_id"]),
                names(&["source_id", "native_id"]),
                "CASCADE".to_string(),
            )])
        );
        assert_eq!(
            foreign_keys(&conn, "doc_anchors")?,
            BTreeSet::from([(
                "documents".to_string(),
                names(&["source_id", "native_id"]),
                names(&["source_id", "native_id"]),
                "CASCADE".to_string(),
            )])
        );
        assert_eq!(
            foreign_keys(&conn, "citations")?,
            BTreeSet::from([
                (
                    "chunks".to_string(),
                    names(&["source_id", "source_chunk_id"]),
                    names(&["source_id", "chunk_id"]),
                    "CASCADE".to_string(),
                ),
                (
                    "documents".to_string(),
                    names(&["source_id", "source_native_id"]),
                    names(&["source_id", "native_id"]),
                    "CASCADE".to_string(),
                ),
            ])
        );
        assert_eq!(
            foreign_keys(&conn, "chunk_embeddings")?,
            BTreeSet::from([(
                "chunks".to_string(),
                names(&["chunk_id"]),
                names(&["chunk_id"]),
                "CASCADE".to_string(),
            )])
        );

        conn.execute_batch(
            r#"
            INSERT INTO sources(source_id, display_name) VALUES
                ('ato', 'Australian Taxation Office'),
                ('frl', 'Federal Register of Legislation');
            INSERT INTO documents(
                source_id, native_id, type, title, canonical_url, downloaded_at,
                content_hash, html
            ) VALUES
                ('ato', 'doc', 'ruling', 'ATO document', 'https://ato.example/doc',
                 '2026-07-11T00:00:00Z', 'ato-hash', X'00'),
                ('frl', 'target', 'act', 'FRL document', 'https://frl.example/target',
                 '2026-07-11T00:00:00Z', 'frl-hash', X'00');
            "#,
        )?;
        set_source_meta(&conn, "ato", "cursor", "42")?;
        set_source_meta(&conn, "frl", "cursor", "84")?;
        assert_eq!(
            get_source_meta(&conn, "ato", "cursor")?,
            Some("42".to_string())
        );
        assert_eq!(
            get_source_meta(&conn, "frl", "cursor")?,
            Some("84".to_string())
        );

        assert!(conn
            .execute(
                "INSERT INTO chunks(chunk_id, source_id, native_id, ord, text) \
                 VALUES (99, 'ato', 'target', 0, X'00')",
                [],
            )
            .is_err());

        conn.execute(
            "INSERT INTO chunks(chunk_id, source_id, native_id, ord, text) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1_i64, "ato", "doc", 0_i64, compress_text("chunk")?],
        )?;
        assert!(conn
            .execute(
                "INSERT INTO chunks(chunk_id, source_id, native_id, ord, text) \
                 VALUES (2, 'ato', 'doc', 0, X'00')",
                [],
            )
            .is_err());
        assert!(conn
            .execute(
                "INSERT INTO citations(\
                    source_chunk_id, source_id, source_native_id, \
                    target_source_id, target_native_id\
                 ) VALUES (1, 'frl', 'target', 'ato', 'doc')",
                [],
            )
            .is_err());

        conn.execute_batch(
            r#"
            INSERT INTO definitions(
                source_id, definition_id, term, norm_term, native_id,
                source_title, source_type, ord, body
            ) VALUES ('ato', 'def', 'Deduction', 'deduction', 'doc',
                      'ATO document', 'ruling', 0, 'Definition body');
            INSERT INTO document_assets(
                source_id, asset_id, native_id, sha256, data
            ) VALUES ('ato', 'asset', 'doc', 'asset-hash', X'01');
            INSERT INTO doc_anchors(
                source_id, native_id, ord, kind, label,
                target_source_id, target_native_id
            ) VALUES ('ato', 'doc', 0, 'sister', 'Related', 'frl', 'target');
            INSERT INTO citations(
                source_chunk_id, source_id, source_native_id,
                target_source_id, target_native_id
            ) VALUES (1, 'ato', 'doc', 'frl', 'target');
            INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (1, zeroblob(256));
            "#,
        )?;

        conn.execute(
            "DELETE FROM documents WHERE source_id = 'ato' AND native_id = 'doc'",
            [],
        )?;
        for table in [
            "chunks",
            "definitions",
            "document_assets",
            "doc_anchors",
            "citations",
            "chunk_embeddings",
        ] {
            let count: i64 =
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
            assert_eq!(count, 0, "rows remained in {table}");
        }
        conn.execute("DELETE FROM sources WHERE source_id = 'ato'", [])?;
        assert_eq!(get_source_meta(&conn, "ato", "cursor")?, None);
        assert_eq!(
            get_source_meta(&conn, "frl", "cursor")?,
            Some("84".to_string())
        );
        Ok(())
    }

    #[test]
    fn source_scoped_lookups_use_source_prefixed_indexes() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        init_db(&conn)?;

        let citation_plan = query_plan(
            &conn,
            "EXPLAIN QUERY PLAN \
             SELECT DISTINCT source_id, source_native_id FROM citations \
             WHERE target_source_id = 'frl' AND target_native_id = 'target'",
        )?;
        assert!(
            citation_plan.contains("COVERING INDEX idx_citations_target_source"),
            "unexpected citation query plan: {citation_plan}"
        );

        let document_plan = query_plan(
            &conn,
            "EXPLAIN QUERY PLAN \
             SELECT native_id FROM documents \
             WHERE source_id = 'ato' AND type = 'ruling'",
        )?;
        assert!(
            document_plan.contains("INDEX idx_documents_source_type"),
            "unexpected document query plan: {document_plan}"
        );

        let definition_plan = query_plan(
            &conn,
            "EXPLAIN QUERY PLAN \
             SELECT definition_id FROM definitions \
             WHERE source_id = 'ato' AND norm_term = 'deduction'",
        )?;
        assert!(
            definition_plan.contains("INDEX idx_definitions_source_norm_term"),
            "unexpected definition query plan: {definition_plan}"
        );
        Ok(())
    }

    #[test]
    fn initialization_rejects_unversioned_nonempty_database() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute("CREATE TABLE unexpected(value TEXT)", [])?;

        let error = init_db(&conn).unwrap_err().to_string();
        assert!(error.contains("corpus_meta.schema_version"));
        assert!(!table_exists(&conn, "sources")?);
        Ok(())
    }

    #[test]
    fn decompress_text_reuses_valid_utf8_buffer() -> Result<()> {
        let text = "Tax guidance — valid UTF-8";
        assert_eq!(decompress_text(compress_text(text)?)?, text);
        Ok(())
    }

    #[test]
    fn decompress_text_rejects_invalid_utf8() -> Result<()> {
        let compressed = zstd::stream::encode_all(Cursor::new([0xff, 0xfe]), 3)?;
        assert!(decompress_text(compressed).is_err());
        Ok(())
    }
}
