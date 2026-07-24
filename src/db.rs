//! SQLite schema, connections, compression, and scoped metadata access.

use crate::config::db_path;
use crate::SUPPORTED_SCHEMA_VERSION;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use sha2::{Digest, Sha256};
#[cfg(windows)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::Cursor;
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
struct LiveDbStamp {
    len: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change_seconds: i64,
    #[cfg(unix)]
    change_nanoseconds: i64,
    #[cfg(windows)]
    volume_serial_number: u32,
    #[cfg(windows)]
    file_index: u64,
    #[cfg(windows)]
    last_write_time: u64,
}

pub(crate) struct LiveDbSeal {
    path: std::path::PathBuf,
    file: File,
    stamp: LiveDbStamp,
}

impl LiveDbSeal {
    pub(crate) fn capture() -> Result<Self> {
        Self::capture_path(db_path()?)
    }

    fn capture_path(path: std::path::PathBuf) -> Result<Self> {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "live DB must be a regular non-symlink file: {}",
                path.display()
            );
        }
        let file = open_immutable_db_handle(&path)?;
        let stamp = live_db_file_stamp(&file)?;
        Ok(Self { path, file, stamp })
    }

    pub(crate) fn verify(&self) -> Result<()> {
        let metadata = fs::symlink_metadata(&self.path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("live DB changed after runtime validation");
        }
        let current = open_immutable_db_handle(&self.path)?;
        if live_db_file_stamp(&current)? != self.stamp
            || live_db_file_stamp(&self.file)? != self.stamp
        {
            bail!("live DB changed after runtime validation");
        }
        Ok(())
    }
}

fn live_db_file_stamp(file: &File) -> Result<LiveDbStamp> {
    #[allow(unused_mut)]
    let mut stamp = live_db_stamp(&file.metadata()?)?;
    #[cfg(windows)]
    {
        let identity = crate::ann::windows_file_identity(file)?;
        stamp.volume_serial_number = identity.volume_serial_number;
        stamp.file_index = identity.file_index;
    }
    Ok(stamp)
}

fn live_db_stamp(metadata: &fs::Metadata) -> Result<LiveDbStamp> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    #[cfg(windows)]
    use std::os::windows::fs::MetadataExt;
    Ok(LiveDbStamp {
        len: metadata.len(),
        modified: metadata.modified()?,
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        change_seconds: metadata.ctime(),
        #[cfg(unix)]
        change_nanoseconds: metadata.ctime_nsec(),
        #[cfg(windows)]
        volume_serial_number: 0,
        #[cfg(windows)]
        file_index: 0,
        #[cfg(windows)]
        last_write_time: metadata.last_write_time(),
    })
}

#[cfg(unix)]
fn open_immutable_db_handle(path: &Path) -> Result<File> {
    Ok(File::open(path)?)
}

#[cfg(windows)]
fn open_immutable_db_handle(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let identity = crate::ann::windows_file_identity(&file)?;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    if identity.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || identity.number_of_links != 1
    {
        bail!(
            "live DB must not be a reparse point or hard link: {}",
            path.display()
        );
    }
    Ok(file)
}

#[cfg(all(not(unix), not(windows)))]
fn open_immutable_db_handle(path: &Path) -> Result<File> {
    Ok(File::open(path)?)
}

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
    #[cfg(windows)]
    {
        let file = fs::File::open(&path)?;
        let identity = crate::ann::windows_file_identity(&file)?;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if identity.number_of_links != 1
            || identity.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            bail!(
                "live DB must not be a reparse point or hard link: {}",
                path.display()
            );
        }
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening local corpus database")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Read-only handles skip WAL/synchronous mutation pragmas but
    // still use in-memory temp storage for query work.
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    enforce_db_schema_version(&conn)?;
    validate_canonical_schema(&conn)?;
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

fn schema_objects(conn: &Connection) -> Result<Vec<(String, String, String, String)>> {
    let mut statement = conn.prepare(
        "SELECT type, name, tbl_name, COALESCE(sql, '')
         FROM sqlite_schema
         WHERE name NOT LIKE 'sqlite_%'
         ORDER BY type, name, tbl_name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn validate_canonical_schema(conn: &Connection) -> Result<()> {
    let expected = Connection::open_in_memory()?;
    init_db(&expected)?;
    if schema_objects(conn)? != schema_objects(&expected)? {
        bail!("schema-{SUPPORTED_SCHEMA_VERSION} legal.db does not match the canonical schema");
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

        "#,
    )?;
    set_corpus_meta(&tx, "schema_version", &SUPPORTED_SCHEMA_VERSION.to_string())?;
    tx.commit()?;
    Ok(())
}

/// Schema 12 keeps every lexical index outside the authoritative payload DB.
pub(crate) fn validate_no_embedded_lexical_index(conn: &Connection) -> Result<()> {
    let lexical_objects: i64 = conn.query_row(
        "SELECT
             (SELECT COUNT(*) FROM sqlite_schema
              WHERE name LIKE 'chunks_fts%' OR name LIKE 'title_fts%')
             +
             (SELECT COUNT(*) FROM pragma_table_list
              WHERE schema = 'main' AND type IN ('virtual', 'shadow'))",
        [],
        |row| row.get(0),
    )?;
    if lexical_objects != 0 {
        bail!(
            "schema-{SUPPORTED_SCHEMA_VERSION} legal.db must not contain runtime lexical indexes"
        );
    }
    Ok(())
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

pub(crate) fn fts_index_sha256(conn: &Connection, table: &str) -> Result<String> {
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

fn hash_fts_docsize(hasher: &mut Sha256, conn: &Connection, table: &str) -> Result<()> {
    hasher.update(b"docsize\0");
    let mut count = 0_u64;
    let mut statement = conn.prepare(&format!(
        "SELECT id, sz, typeof(origin), quote(origin) FROM {table}_docsize ORDER BY id"
    ))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        hasher.update(row.get::<_, i64>(0)?.to_le_bytes());
        hash_digest_field(hasher, row.get_ref(1)?.as_blob()?);
        hash_digest_field(hasher, row.get::<_, String>(2)?.as_bytes());
        hash_digest_field(hasher, row.get::<_, String>(3)?.as_bytes());
        count = count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("FTS docsize row count overflow"))?;
    }
    hasher.update(count.to_le_bytes());
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
        assert!(!table_exists(&conn, "chunks_fts")?);
        assert!(!table_exists(&conn, "title_fts")?);

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

        Ok(())
    }

    #[test]
    fn canonical_schema_rejects_missing_changed_and_extra_objects() -> Result<()> {
        let canonical = Connection::open_in_memory()?;
        init_db(&canonical)?;
        validate_canonical_schema(&canonical)?;

        let missing = Connection::open_in_memory()?;
        init_db(&missing)?;
        missing.execute_batch("DROP TABLE definitions")?;
        assert!(validate_canonical_schema(&missing).is_err());

        let changed = Connection::open_in_memory()?;
        init_db(&changed)?;
        changed.execute_batch(
            "DROP INDEX idx_documents_source_type;
             CREATE INDEX idx_documents_source_type ON documents(source_id, title)",
        )?;
        assert!(validate_canonical_schema(&changed).is_err());

        let extra = Connection::open_in_memory()?;
        init_db(&extra)?;
        extra.execute_batch("CREATE TABLE unexpected(value TEXT)")?;
        assert!(validate_canonical_schema(&extra).is_err());
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
    fn live_database_seal_detects_same_length_payload_mutation() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("legal.db");
        fs::write(&path, b"authoritative")?;
        let seal = LiveDbSeal::capture_path(path.clone())?;
        let mutation = fs::write(&path, b"counterfeited!");
        #[cfg(windows)]
        {
            assert!(mutation.is_err());
            seal.verify()?;
        }
        #[cfg(not(windows))]
        {
            mutation?;
            assert!(seal.verify().is_err());
        }
        Ok(())
    }

    #[test]
    fn decompress_text_rejects_invalid_utf8() -> Result<()> {
        let compressed = zstd::stream::encode_all(Cursor::new([0xff, 0xfe]), 3)?;
        assert!(decompress_text(compressed).is_err());
        Ok(())
    }
}
