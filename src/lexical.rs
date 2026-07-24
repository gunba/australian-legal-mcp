//! Deterministic immutable source-only SQLite FTS5 sidecars.
//!
//! Sidecars contain only compact document filters, chunk-to-document mappings,
//! and contentless chunk/title FTS indexes. `legal.db` remains authoritative for
//! every payload and hydrates only selected search winners.

use crate::db::{decompress_text, fts_index_sha256, get_source_meta, normalized_sql};
use anyhow::{anyhow, bail, Context, Result};
use legal_model::SourceId;
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

pub(crate) const LEXICAL_DIRECTORY: &str = "lexical";
pub(crate) const LEXICAL_FORMAT: &str = "sqlite-fts5-source-lexical";
pub(crate) const LEXICAL_FORMAT_VERSION: u32 = 1;
pub(crate) const LEXICAL_TOKENIZER: &str = "porter unicode61 remove_diacritics 2";

const SQLITE_APPLICATION_ID: i64 = 0x4c45_5831; // `LEX1`
const SQLITE_PAGE_SIZE: i64 = 4 * 1024;
const MAX_SIDECAR_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_VERIFIED_SIDECARS: usize = 20;
static VERIFIED_SIDECARS: OnceLock<Mutex<HashMap<RuntimeVerificationKey, Arc<RuntimeFileSeal>>>> =
    OnceLock::new();

const LEXICAL_META_SQL: &str = r#"CREATE TABLE lexical_meta(
    key TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID"#;

const DOCUMENT_FILTER_SQL: &str = r#"CREATE TABLE document_filter(
    doc_key INTEGER PRIMARY KEY CHECK(doc_key > 0),
    native_id TEXT NOT NULL UNIQUE COLLATE BINARY,
    type TEXT NOT NULL,
    date TEXT,
    withdrawn_date TEXT,
    is_superseded INTEGER NOT NULL CHECK(is_superseded IN (0, 1)),
    first_chunk_id INTEGER,
    last_chunk_id INTEGER,
    CHECK(
        (first_chunk_id IS NULL AND last_chunk_id IS NULL)
        OR (first_chunk_id > 0 AND last_chunk_id >= first_chunk_id)
    )
)"#;

const DOCUMENT_FILTER_NOCASE_INDEX_SQL: &str =
    "CREATE INDEX document_filter_native_nocase ON document_filter(native_id COLLATE NOCASE)";

const CHUNK_FILTER_SQL: &str = r#"CREATE TABLE chunk_filter(
    chunk_id INTEGER PRIMARY KEY CHECK(chunk_id > 0),
    doc_key INTEGER NOT NULL REFERENCES document_filter(doc_key),
    is_intro INTEGER NOT NULL CHECK(is_intro IN (0, 1))
)"#;

const CHUNK_FTS_SQL: &str = r#"CREATE VIRTUAL TABLE chunk_fts USING fts5(
    text,
    content = '',
    contentless_delete = 1,
    tokenize = "porter unicode61 remove_diacritics 2"
)"#;

const TITLE_FTS_SQL: &str = r#"CREATE VIRTUAL TABLE title_fts USING fts5(
    title,
    headings,
    content = '',
    contentless_delete = 1,
    tokenize = "porter unicode61 remove_diacritics 2"
)"#;

const EXPECTED_META_KEYS: [&str; 15] = [
    "chunk_count",
    "chunk_fts_index_sha256",
    "chunk_text_sha256",
    "document_count",
    "first_chunk_id",
    "format",
    "format_version",
    "last_chunk_id",
    "main_db_sha256",
    "source_corpus_id",
    "source_id",
    "source_index_sha256",
    "title_fts_index_sha256",
    "title_text_sha256",
    "tokenizer",
];

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestLexical {
    pub(crate) source_id: SourceId,
    pub(crate) format: String,
    pub(crate) format_version: u32,
    pub(crate) path: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
    pub(crate) corpus_id: String,
    pub(crate) source_index_sha256: String,
    pub(crate) tokenizer: String,
    pub(crate) document_count: u64,
    pub(crate) chunk_count: u64,
    pub(crate) first_chunk_id: u32,
    pub(crate) last_chunk_id: u32,
    pub(crate) chunk_text_sha256: String,
    pub(crate) title_text_sha256: String,
    pub(crate) chunk_fts_index_sha256: String,
    pub(crate) title_fts_index_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceIdentity {
    corpus_id: String,
    source_index_sha256: String,
    document_count: u64,
    chunk_count: u64,
    first_chunk_id: u32,
    last_chunk_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceTextDigests {
    chunk: String,
    title: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct RuntimeVerificationKey {
    path: PathBuf,
    source_id: SourceId,
    sidecar_sha256: String,
    main_db_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimeFileStamp {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change_seconds: i64,
    #[cfg(unix)]
    change_nanoseconds: i64,
    #[cfg(windows)]
    volume_serial_number: Option<u32>,
    #[cfg(windows)]
    file_index: Option<u64>,
    #[cfg(windows)]
    change_time: Option<u64>,
    #[cfg(windows)]
    number_of_links: Option<u32>,
}

#[derive(Debug)]
struct RuntimeFileSeal {
    stamp: RuntimeFileStamp,
    file: File,
}

impl RuntimeFileSeal {
    fn capture(path: &Path) -> Result<Self> {
        let metadata = regular_file_metadata(path)?;
        let file = open_immutable_sidecar_handle(path)?;
        let stamp = runtime_file_stamp_from_handle(&file.metadata()?, &file)?;
        if runtime_file_stamp(path, &metadata)? != stamp {
            bail!("lexical sidecar changed while its immutable handle was captured");
        }
        Ok(Self { stamp, file })
    }

    fn verify(&self, path: &Path) -> Result<()> {
        let current = open_immutable_sidecar_handle(path)?;
        if runtime_file_stamp_from_handle(&current.metadata()?, &current)? != self.stamp
            || runtime_file_stamp_from_handle(&self.file.metadata()?, &self.file)? != self.stamp
        {
            bail!("lexical sidecar changed after runtime verification");
        }
        Ok(())
    }
}

pub(crate) fn sidecar_relative_path(source_id: &SourceId) -> PathBuf {
    PathBuf::from(LEXICAL_DIRECTORY).join(format!("{source_id}.db"))
}

pub(crate) fn sidecar_manifest_path(source_id: &SourceId) -> String {
    format!("{LEXICAL_DIRECTORY}/{source_id}.db")
}

pub(crate) fn validate_manifest_lexical(
    source_id: &SourceId,
    info: &ManifestLexical,
) -> Result<()> {
    if &info.source_id != source_id {
        bail!(
            "lexical sidecar source mismatch: map key `{source_id}`, entry `{}`",
            info.source_id
        );
    }
    if info.format != LEXICAL_FORMAT || info.format_version != LEXICAL_FORMAT_VERSION {
        bail!("lexical sidecar format for source `{source_id}` is unsupported");
    }
    if info.path != sidecar_manifest_path(source_id)
        || Path::new(&info.path).components().count() != 2
    {
        bail!("lexical sidecar path for source `{source_id}` is noncanonical");
    }
    if info.size == 0 || info.size > MAX_SIDECAR_BYTES || !is_sha256(&info.sha256) {
        bail!("lexical sidecar file metadata for source `{source_id}` is malformed");
    }
    if !is_corpus_id(&info.corpus_id)
        || !is_sha256(&info.source_index_sha256)
        || info.tokenizer != LEXICAL_TOKENIZER
        || info.document_count == 0
        || info.chunk_count == 0
        || info.first_chunk_id == 0
        || info.last_chunk_id < info.first_chunk_id
    {
        bail!("lexical sidecar identity for source `{source_id}` is malformed");
    }
    if u64::from(info.last_chunk_id - info.first_chunk_id) + 1 != info.chunk_count {
        bail!("lexical sidecar chunk range is not contiguous for source `{source_id}`");
    }
    for (label, digest) in [
        ("chunk text", &info.chunk_text_sha256),
        ("title text", &info.title_text_sha256),
        ("chunk FTS", &info.chunk_fts_index_sha256),
        ("title FTS", &info.title_fts_index_sha256),
    ] {
        if !is_sha256(digest) {
            bail!("lexical sidecar {label} digest for source `{source_id}` is malformed");
        }
    }
    Ok(())
}

pub(crate) fn build_sidecars(
    legal: &Connection,
    output_root: &Path,
    main_db_sha256: &str,
) -> Result<BTreeMap<SourceId, ManifestLexical>> {
    if !is_sha256(main_db_sha256) {
        bail!("main database SHA-256 is malformed");
    }
    let directory = output_root.join(LEXICAL_DIRECTORY);
    fs::create_dir_all(&directory)?;
    remove_abandoned_build_directories(&directory)?;
    let mut output = BTreeMap::new();
    for descriptor in crate::legal_source::source_registry().descriptors() {
        let source_id = descriptor.id;
        let info = build_sidecar(legal, &source_id, output_root, main_db_sha256)
            .with_context(|| format!("building lexical sidecar for source `{source_id}`"))?;
        if output.insert(source_id.clone(), info).is_some() {
            bail!("duplicate registered lexical source `{source_id}`");
        }
    }
    verify_sidecar_file_set(&directory, &output)?;
    Ok(output)
}

fn remove_abandoned_build_directories(directory: &Path) -> Result<()> {
    let prefixes = crate::legal_source::source_registry()
        .descriptors()
        .into_iter()
        .map(|descriptor| format!("lexical-{}-build-", descriptor.id))
        .collect::<Vec<_>>();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("lexical build directory contains a non-Unicode path"))?;
        if entry.file_type()?.is_dir() && prefixes.iter().any(|prefix| name.starts_with(prefix)) {
            fs::remove_dir_all(entry.path())
                .with_context(|| format!("removing abandoned lexical build directory `{name}`"))?;
        }
    }
    Ok(())
}

pub(crate) fn build_sidecar(
    legal: &Connection,
    source_id: &SourceId,
    output_root: &Path,
    main_db_sha256: &str,
) -> Result<ManifestLexical> {
    if !is_sha256(main_db_sha256) {
        bail!("main database SHA-256 is malformed");
    }
    let identity = source_identity(legal, source_id).context("reading lexical source identity")?;
    let output = output_root.join(sidecar_relative_path(source_id));
    let parent = output
        .parent()
        .ok_or_else(|| anyhow!("lexical sidecar output has no parent"))?;
    fs::create_dir_all(parent).context("creating lexical output directory")?;
    let temporary = tempfile::Builder::new()
        .prefix(&format!("lexical-{source_id}-build-"))
        .tempdir_in(parent)
        .context("creating lexical build directory")?;
    let build_path = temporary.path().join(format!("{source_id}.db.part"));

    let mut sidecar = Connection::open(&build_path).context("opening lexical build database")?;
    configure_build_connection(&sidecar).context("configuring lexical build database")?;
    create_schema(&sidecar).context("creating lexical sidecar schema")?;
    let digests = populate_sidecar(&mut sidecar, legal, source_id, &identity)
        .context("populating lexical sidecar")?;

    sidecar.execute("INSERT INTO chunk_fts(chunk_fts) VALUES('optimize')", [])?;
    sidecar.execute("INSERT INTO title_fts(title_fts) VALUES('optimize')", [])?;
    let chunk_fts_index_sha256 = fts_index_sha256(&sidecar, "chunk_fts")?;
    let title_fts_index_sha256 = fts_index_sha256(&sidecar, "title_fts")?;

    let metadata = expected_metadata(
        source_id,
        &identity,
        main_db_sha256,
        &digests,
        &chunk_fts_index_sha256,
        &title_fts_index_sha256,
    );
    let transaction = sidecar.unchecked_transaction()?;
    {
        let mut insert =
            transaction.prepare("INSERT INTO lexical_meta(key, value) VALUES (?1, ?2)")?;
        for (key, value) in &metadata {
            insert.execute(params![key, value])?;
        }
    }
    transaction.commit()?;
    sidecar.execute_batch("ANALYZE; VACUUM;")?;
    if fts_index_sha256(&sidecar, "chunk_fts")? != chunk_fts_index_sha256
        || fts_index_sha256(&sidecar, "title_fts")? != title_fts_index_sha256
    {
        bail!("lexical FTS storage digest changed during deterministic finalization");
    }
    validate_schema(&sidecar)?;
    validate_metadata(&sidecar, &metadata)?;
    verify_fts_rowids(&sidecar, &identity)?;
    let foreign_keys: i64 =
        sidecar.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_keys != 0 {
        bail!("lexical sidecar has {foreign_keys} foreign-key violations");
    }
    sidecar
        .close()
        .map_err(|(_, error)| error)
        .context("closing completed lexical sidecar")?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&build_path)?
        .sync_all()?;

    let info = ManifestLexical {
        source_id: source_id.clone(),
        format: LEXICAL_FORMAT.to_string(),
        format_version: LEXICAL_FORMAT_VERSION,
        path: sidecar_manifest_path(source_id),
        sha256: sha256_path(&build_path)?,
        size: fs::metadata(&build_path)?.len(),
        corpus_id: identity.corpus_id,
        source_index_sha256: identity.source_index_sha256,
        tokenizer: LEXICAL_TOKENIZER.to_string(),
        document_count: identity.document_count,
        chunk_count: identity.chunk_count,
        first_chunk_id: identity.first_chunk_id,
        last_chunk_id: identity.last_chunk_id,
        chunk_text_sha256: digests.chunk,
        title_text_sha256: digests.title,
        chunk_fts_index_sha256,
        title_fts_index_sha256,
    };
    validate_manifest_lexical(source_id, &info)?;
    verify_sidecar(&build_path, source_id, &info, legal, main_db_sha256, true)
        .context("verifying completed lexical build")?;
    replace_file(&build_path, &output).context("publishing completed lexical build")?;
    Ok(info)
}

pub(crate) fn open_runtime_sidecar(
    legal: &Connection,
    source_id: &SourceId,
    info: &ManifestLexical,
    main_db_sha256: &str,
) -> Result<Connection> {
    validate_manifest_lexical(source_id, info)?;
    let path = crate::config::live_dir()?.join(&info.path);
    let seal = Arc::new(RuntimeFileSeal::capture(&path)?);
    if seal.stamp.len != info.size {
        bail!("lexical sidecar size mismatch for source `{source_id}`");
    }
    let key = RuntimeVerificationKey {
        path: path.clone(),
        source_id: source_id.clone(),
        sidecar_sha256: info.sha256.clone(),
        main_db_sha256: main_db_sha256.to_string(),
    };
    let cache = VERIFIED_SIDECARS.get_or_init(|| Mutex::new(HashMap::new()));
    let cached = cache
        .lock()
        .map_err(|_| anyhow!("lexical verification cache lock poisoned"))?
        .get(&key)
        .cloned();
    let requires_full_verification = if let Some(verified) = cached {
        if verified.stamp != seal.stamp {
            bail!("lexical sidecar changed after runtime verification");
        }
        verified.verify(&path)?;
        false
    } else {
        if sha256_path(&path)? != info.sha256 {
            bail!("lexical sidecar SHA-256 mismatch for source `{source_id}`");
        }
        seal.verify(&path)?;
        true
    };
    let sidecar = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening lexical sidecar for source `{source_id}`"))?;
    configure_read_connection(&sidecar)?;
    validate_schema(&sidecar)?;
    let identity = runtime_source_identity(legal, source_id, info)?;
    let expected = expected_metadata(
        source_id,
        &identity,
        main_db_sha256,
        &SourceTextDigests {
            chunk: info.chunk_text_sha256.clone(),
            title: info.title_text_sha256.clone(),
        },
        &info.chunk_fts_index_sha256,
        &info.title_fts_index_sha256,
    );
    validate_info_matches_identity(info, &identity)?;
    validate_metadata(&sidecar, &expected)?;
    if requires_full_verification {
        verify_fts_rowids(&sidecar, &identity)?;
        seal.verify(&path)?;
        let mut cache = cache
            .lock()
            .map_err(|_| anyhow!("lexical verification cache lock poisoned"))?;
        if cache.len() >= MAX_VERIFIED_SIDECARS {
            cache.clear();
        }
        cache.insert(key, seal);
    }
    Ok(sidecar)
}

pub(crate) fn verify_sidecar(
    path: &Path,
    source_id: &SourceId,
    info: &ManifestLexical,
    legal: &Connection,
    main_db_sha256: &str,
    full_sqlite_integrity: bool,
) -> Result<()> {
    verify_sidecar_with_seal_retention(
        path,
        source_id,
        info,
        legal,
        main_db_sha256,
        full_sqlite_integrity,
        false,
    )
}

pub(crate) fn verify_installed_sidecar(
    path: &Path,
    source_id: &SourceId,
    info: &ManifestLexical,
    legal: &Connection,
    main_db_sha256: &str,
) -> Result<()> {
    verify_sidecar_with_seal_retention(path, source_id, info, legal, main_db_sha256, false, true)
}

fn verify_sidecar_with_seal_retention(
    path: &Path,
    source_id: &SourceId,
    info: &ManifestLexical,
    legal: &Connection,
    main_db_sha256: &str,
    full_sqlite_integrity: bool,
    retain_runtime_seal: bool,
) -> Result<()> {
    validate_manifest_lexical(source_id, info)?;
    let before = regular_file_metadata(path)?;
    if before.len() != info.size || sha256_path(path)? != info.sha256 {
        bail!("lexical sidecar hash or size mismatch for source `{source_id}`");
    }
    let flags = if full_sqlite_integrity {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let sidecar = Connection::open_with_flags(path, flags)?;
    sidecar.busy_timeout(Duration::from_secs(30))?;
    configure_read_connection(&sidecar)?;
    validate_schema(&sidecar)?;
    let identity = source_identity(legal, source_id)?;
    validate_info_matches_identity(info, &identity)?;
    let expected = expected_metadata(
        source_id,
        &identity,
        main_db_sha256,
        &SourceTextDigests {
            chunk: info.chunk_text_sha256.clone(),
            title: info.title_text_sha256.clone(),
        },
        &info.chunk_fts_index_sha256,
        &info.title_fts_index_sha256,
    );
    validate_metadata(&sidecar, &expected)?;
    if fts_index_sha256(&sidecar, "chunk_fts")? != info.chunk_fts_index_sha256
        || fts_index_sha256(&sidecar, "title_fts")? != info.title_fts_index_sha256
    {
        bail!("lexical FTS postings or BM25 metadata differ from generation.json");
    }
    let actual_digests = verify_relational_bindings(&sidecar, legal, source_id, &identity)?;
    if actual_digests.chunk != info.chunk_text_sha256
        || actual_digests.title != info.title_text_sha256
    {
        bail!("lexical sidecar source-text binding differs from legal.db");
    }
    verify_fts_rowids(&sidecar, &identity)?;
    if full_sqlite_integrity {
        verify_sqlite_integrity(&sidecar)?;
    }
    sidecar
        .close()
        .map_err(|(_, error)| error)
        .context("closing verified lexical sidecar")?;
    let seal = Arc::new(RuntimeFileSeal::capture(path)?);
    if runtime_file_stamp(path, &before)? != seal.stamp {
        bail!("lexical sidecar changed during strict verification");
    }
    if full_sqlite_integrity && sha256_path(path)? != info.sha256 {
        bail!("lexical sidecar changed during strict verification");
    }
    if retain_runtime_seal {
        let key = RuntimeVerificationKey {
            path: path.to_path_buf(),
            source_id: source_id.clone(),
            sidecar_sha256: info.sha256.clone(),
            main_db_sha256: main_db_sha256.to_string(),
        };
        let cache = VERIFIED_SIDECARS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut cache = cache
            .lock()
            .map_err(|_| anyhow!("lexical verification cache lock poisoned"))?;
        if cache.len() >= MAX_VERIFIED_SIDECARS {
            cache.clear();
        }
        cache.insert(key, seal);
    } else {
        drop(seal);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn clear_runtime_verification_cache_for_tests() {
    if let Some(cache) = VERIFIED_SIDECARS.get() {
        cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }
}

pub(crate) fn verify_sidecar_file_set(
    directory: &Path,
    manifest: &BTreeMap<SourceId, ManifestLexical>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(directory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("generation lexical path must be a real directory");
    }
    let expected = manifest
        .values()
        .map(|info| {
            Path::new(&info.path)
                .file_name()
                .expect("validated lexical path has a file name")
                .to_owned()
        })
        .collect::<BTreeSet<_>>();
    let actual = fs::read_dir(directory)?
        .map(|entry| Ok(entry?.file_name()))
        .collect::<Result<BTreeSet<_>>>()?;
    if actual != expected {
        bail!("generation lexical file set differs from generation.json");
    }
    Ok(())
}

fn configure_build_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "page_size", SQLITE_PAGE_SIZE)?;
    conn.pragma_update(None, "auto_vacuum", "NONE")?;
    conn.pragma_update(None, "journal_mode", "OFF")?;
    conn.pragma_update(None, "synchronous", "OFF")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    conn.pragma_update(None, "application_id", SQLITE_APPLICATION_ID)?;
    conn.pragma_update(None, "user_version", LEXICAL_FORMAT_VERSION)?;
    conn.pragma_update(None, "case_sensitive_like", "OFF")?;
    Ok(())
}

fn configure_read_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "case_sensitive_like", "OFF")?;
    conn.execute_batch("PRAGMA query_only=ON; PRAGMA cell_size_check=ON;")?;
    Ok(())
}

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "{LEXICAL_META_SQL};\n{DOCUMENT_FILTER_SQL};\n{DOCUMENT_FILTER_NOCASE_INDEX_SQL};\n{CHUNK_FILTER_SQL};\n{CHUNK_FTS_SQL};\n{TITLE_FTS_SQL};"
    ))?;
    Ok(())
}

fn populate_sidecar(
    sidecar: &mut Connection,
    legal: &Connection,
    source_id: &SourceId,
    identity: &SourceIdentity,
) -> Result<SourceTextDigests> {
    let transaction = sidecar.unchecked_transaction()?;
    let mut doc_keys = HashMap::with_capacity(usize::try_from(identity.document_count)?);
    let mut title_hasher = Sha256::new();
    title_hasher.update(b"australian-legal-mcp-lexical-title-text-v1\0");
    {
        let mut documents = legal.prepare(
            "SELECT d.native_id, d.type, d.date, d.withdrawn_date,\n                    d.superseded_by IS NOT NULL, d.title, d.headings,\n                    MIN(c.chunk_id), MAX(c.chunk_id), COUNT(c.chunk_id)\n             FROM documents AS d\n             LEFT JOIN chunks AS c\n               ON c.source_id = d.source_id AND c.native_id = d.native_id\n             WHERE d.source_id = ?1\n             GROUP BY d.native_id COLLATE BINARY\n             ORDER BY d.native_id COLLATE BINARY",
        )?;
        let mut rows = documents.query([source_id.as_str()])?;
        let mut insert_filter = transaction.prepare(
            "INSERT INTO document_filter(\n                 doc_key, native_id, type, date, withdrawn_date, is_superseded,\n                 first_chunk_id, last_chunk_id\n             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        let mut insert_title = transaction
            .prepare("INSERT INTO title_fts(rowid, title, headings) VALUES (?1, ?2, ?3)")?;
        let mut count = 0u64;
        while let Some(row) = rows.next()? {
            count = count
                .checked_add(1)
                .ok_or_else(|| anyhow!("lexical document count overflow"))?;
            let doc_key = i64::try_from(count)?;
            let native_id = row.get::<_, String>(0)?;
            let document_type = row.get::<_, String>(1)?;
            let date = row.get::<_, Option<String>>(2)?;
            let withdrawn_date = row.get::<_, Option<String>>(3)?;
            let is_superseded = row.get::<_, i64>(4)?;
            let title = row.get::<_, String>(5)?;
            let headings = row.get::<_, String>(6)?;
            let first = row.get::<_, Option<i64>>(7)?;
            let last = row.get::<_, Option<i64>>(8)?;
            let chunks = row.get::<_, i64>(9)?;
            validate_document_chunk_range(source_id, &native_id, first, last, chunks)?;
            insert_filter.execute(params![
                doc_key,
                native_id,
                document_type,
                date,
                withdrawn_date,
                is_superseded,
                first,
                last
            ])?;
            insert_title.execute(params![doc_key, title, headings])?;
            hash_field(&mut title_hasher, &doc_key.to_le_bytes());
            hash_field(&mut title_hasher, native_id.as_bytes());
            hash_field(&mut title_hasher, title.as_bytes());
            hash_field(&mut title_hasher, headings.as_bytes());
            if doc_keys.insert(native_id, doc_key).is_some() {
                bail!("duplicate source document while building lexical sidecar");
            }
        }
        if count != identity.document_count {
            bail!("source document count changed during lexical sidecar construction");
        }
        title_hasher.update(count.to_le_bytes());
    }

    let mut chunk_hasher = Sha256::new();
    chunk_hasher.update(b"australian-legal-mcp-lexical-chunk-text-v1\0");
    {
        let mut chunks = legal.prepare(
            "SELECT chunk_id, native_id, ord, text\n             FROM chunks WHERE source_id = ?1 ORDER BY chunk_id",
        )?;
        let mut rows = chunks.query([source_id.as_str()])?;
        let mut insert_filter = transaction
            .prepare("INSERT INTO chunk_filter(chunk_id, doc_key, is_intro) VALUES (?1, ?2, ?3)")?;
        let mut insert_fts =
            transaction.prepare("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)")?;
        let mut count = 0u64;
        let mut previous = None;
        while let Some(row) = rows.next()? {
            let chunk_id = row.get::<_, i64>(0)?;
            if chunk_id <= 0 || previous.is_some_and(|value| value >= chunk_id) {
                bail!("source chunk IDs are not strictly increasing positive integers");
            }
            u32::try_from(chunk_id).context("chunk ID exceeds lexical u32 manifest bounds")?;
            let native_id = row.get::<_, String>(1)?;
            let doc_key = doc_keys
                .get(&native_id)
                .copied()
                .ok_or_else(|| anyhow!("chunk {chunk_id} has no lexical document mapping"))?;
            let ord = row.get::<_, i64>(2)?;
            let text = decompress_text(row.get::<_, Vec<u8>>(3)?)?;
            let is_intro = i64::from(ord == 0 && text.len() < 100);
            insert_filter.execute(params![chunk_id, doc_key, is_intro])?;
            insert_fts.execute(params![chunk_id, text])?;
            hash_field(&mut chunk_hasher, &chunk_id.to_le_bytes());
            hash_field(&mut chunk_hasher, text.as_bytes());
            count = count
                .checked_add(1)
                .ok_or_else(|| anyhow!("lexical chunk count overflow"))?;
            previous = Some(chunk_id);
        }
        if count != identity.chunk_count {
            bail!("source chunk count changed during lexical sidecar construction");
        }
        chunk_hasher.update(count.to_le_bytes());
    }
    transaction.commit()?;
    Ok(SourceTextDigests {
        chunk: format!("{:x}", chunk_hasher.finalize()),
        title: format!("{:x}", title_hasher.finalize()),
    })
}

fn validate_document_chunk_range(
    source_id: &SourceId,
    native_id: &str,
    first: Option<i64>,
    last: Option<i64>,
    count: i64,
) -> Result<()> {
    match (first, last, count) {
        (None, None, 0) => Ok(()),
        (Some(first), Some(last), count)
            if first > 0 && last >= first && last - first + 1 == count =>
        {
            Ok(())
        }
        _ => bail!(
            "source `{source_id}` document `{native_id}` does not occupy one exact chunk-ID range"
        ),
    }
}

fn source_identity(legal: &Connection, source_id: &SourceId) -> Result<SourceIdentity> {
    let (document_count, chunk_count, first, last) = legal.query_row(
        "SELECT\n             (SELECT COUNT(*) FROM documents WHERE source_id = ?1),\n             (SELECT COUNT(*) FROM chunks WHERE source_id = ?1),\n             (SELECT MIN(chunk_id) FROM chunks WHERE source_id = ?1),\n             (SELECT MAX(chunk_id) FROM chunks WHERE source_id = ?1)",
        [source_id.as_str()],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        },
    )?;
    let (corpus_id, source_index_sha256, metadata_documents, metadata_chunks) =
        source_identity_metadata(legal, source_id)?;
    let document_count =
        u64::try_from(document_count).context("source document count is negative")?;
    let chunk_count = u64::try_from(chunk_count).context("source chunk count is negative")?;
    let first_chunk_id = first
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| anyhow!("source `{source_id}` has no valid first chunk ID"))?;
    let last_chunk_id = last
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| anyhow!("source `{source_id}` has no valid last chunk ID"))?;
    if document_count != metadata_documents || chunk_count != metadata_chunks {
        bail!("source `{source_id}` lexical count metadata differs from legal.db");
    }
    Ok(SourceIdentity {
        corpus_id,
        source_index_sha256,
        document_count,
        chunk_count,
        first_chunk_id,
        last_chunk_id,
    })
}

fn source_identity_metadata(
    legal: &Connection,
    source_id: &SourceId,
) -> Result<(String, String, u64, u64)> {
    let required = |key: &str| -> Result<String> {
        get_source_meta(legal, source_id.as_str(), key)?.ok_or_else(|| {
            anyhow!("source `{source_id}` is missing lexical binding metadata `{key}`")
        })
    };
    let corpus_id = required("corpus_id")?;
    let source_index_sha256 = required("source_index_sha256")?;
    let document_count = parse_u64(&required("documents_count")?, "documents_count")?;
    let chunk_count = parse_u64(&required("chunks_count")?, "chunks_count")?;
    if !is_corpus_id(&corpus_id)
        || !is_sha256(&source_index_sha256)
        || document_count == 0
        || chunk_count == 0
    {
        bail!("source `{source_id}` lexical binding metadata is malformed");
    }
    Ok((corpus_id, source_index_sha256, document_count, chunk_count))
}

fn runtime_source_identity(
    legal: &Connection,
    source_id: &SourceId,
    info: &ManifestLexical,
) -> Result<SourceIdentity> {
    let (corpus_id, source_index_sha256, document_count, chunk_count) =
        source_identity_metadata(legal, source_id)?;
    let identity = SourceIdentity {
        corpus_id,
        source_index_sha256,
        document_count,
        chunk_count,
        first_chunk_id: info.first_chunk_id,
        last_chunk_id: info.last_chunk_id,
    };
    validate_info_matches_identity(info, &identity)?;
    Ok(identity)
}

fn validate_info_matches_identity(info: &ManifestLexical, identity: &SourceIdentity) -> Result<()> {
    if info.corpus_id != identity.corpus_id
        || info.source_index_sha256 != identity.source_index_sha256
        || info.document_count != identity.document_count
        || info.chunk_count != identity.chunk_count
        || info.first_chunk_id != identity.first_chunk_id
        || info.last_chunk_id != identity.last_chunk_id
    {
        bail!("lexical sidecar metadata does not match its legal.db source partition");
    }
    Ok(())
}

fn expected_metadata(
    source_id: &SourceId,
    identity: &SourceIdentity,
    main_db_sha256: &str,
    text: &SourceTextDigests,
    chunk_fts_index_sha256: &str,
    title_fts_index_sha256: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("chunk_count".to_string(), identity.chunk_count.to_string()),
        (
            "chunk_fts_index_sha256".to_string(),
            chunk_fts_index_sha256.to_string(),
        ),
        ("chunk_text_sha256".to_string(), text.chunk.clone()),
        (
            "document_count".to_string(),
            identity.document_count.to_string(),
        ),
        (
            "first_chunk_id".to_string(),
            identity.first_chunk_id.to_string(),
        ),
        ("format".to_string(), LEXICAL_FORMAT.to_string()),
        (
            "format_version".to_string(),
            LEXICAL_FORMAT_VERSION.to_string(),
        ),
        (
            "last_chunk_id".to_string(),
            identity.last_chunk_id.to_string(),
        ),
        ("main_db_sha256".to_string(), main_db_sha256.to_string()),
        ("source_corpus_id".to_string(), identity.corpus_id.clone()),
        ("source_id".to_string(), source_id.as_str().to_string()),
        (
            "source_index_sha256".to_string(),
            identity.source_index_sha256.clone(),
        ),
        (
            "title_fts_index_sha256".to_string(),
            title_fts_index_sha256.to_string(),
        ),
        ("title_text_sha256".to_string(), text.title.clone()),
        ("tokenizer".to_string(), LEXICAL_TOKENIZER.to_string()),
    ])
}

fn validate_metadata(conn: &Connection, expected: &BTreeMap<String, String>) -> Result<()> {
    let expected_keys = expected.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let contract_keys = EXPECTED_META_KEYS.into_iter().collect::<BTreeSet<_>>();
    if expected_keys != contract_keys {
        bail!("internal lexical metadata contract is inconsistent");
    }
    let mut statement = conn.prepare("SELECT key, value FROM lexical_meta ORDER BY key")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let actual = rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    if actual != *expected {
        bail!("lexical sidecar metadata does not match its immutable binding");
    }
    Ok(())
}

fn validate_schema(conn: &Connection) -> Result<()> {
    let application_id: i64 = conn.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let user_version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let page_size: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
    if application_id != SQLITE_APPLICATION_ID
        || user_version != LEXICAL_FORMAT_VERSION
        || page_size != SQLITE_PAGE_SIZE
    {
        bail!("lexical sidecar SQLite header contract is incompatible");
    }

    for (name, expected) in [
        ("lexical_meta", LEXICAL_META_SQL),
        ("document_filter", DOCUMENT_FILTER_SQL),
        ("chunk_filter", CHUNK_FILTER_SQL),
        ("chunk_fts", CHUNK_FTS_SQL),
        ("title_fts", TITLE_FTS_SQL),
    ] {
        let actual = conn
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [name],
                |row| row.get::<_, String>(0),
            )
            .with_context(|| format!("reading lexical schema object `{name}`"))?;
        if normalized_sql(&actual) != normalized_sql(expected) {
            bail!("lexical schema object `{name}` is not canonical");
        }
    }
    let index_sql: String = conn.query_row(
        "SELECT sql FROM sqlite_schema WHERE type = 'index' AND name = 'document_filter_native_nocase'",
        [],
        |row| row.get(0),
    )?;
    if normalized_sql(&index_sql) != normalized_sql(DOCUMENT_FILTER_NOCASE_INDEX_SQL) {
        bail!("lexical native-ID case-insensitive lookup index is not canonical");
    }

    for table in ["chunk_fts", "title_fts"] {
        for (suffix, definition) in [
            (
                "config",
                format!("CREATE TABLE '{table}_config'(k PRIMARY KEY, v) WITHOUT ROWID"),
            ),
            (
                "data",
                format!("CREATE TABLE '{table}_data'(id INTEGER PRIMARY KEY, block BLOB)"),
            ),
            (
                "docsize",
                format!(
                    "CREATE TABLE '{table}_docsize'(id INTEGER PRIMARY KEY, sz BLOB, origin INTEGER)"
                ),
            ),
            (
                "idx",
                format!(
                    "CREATE TABLE '{table}_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID"
                ),
            ),
        ] {
            let name = format!("{table}_{suffix}");
            let actual: String = conn.query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
                [&name],
                |row| row.get(0),
            )?;
            if normalized_sql(&actual) != normalized_sql(&definition) {
                bail!("lexical FTS shadow table `{name}` is not canonical");
            }
        }
    }
    for (name, definition) in [
        ("sqlite_stat1", "CREATE TABLE sqlite_stat1(tbl,idx,stat)"),
        (
            "sqlite_stat4",
            "CREATE TABLE sqlite_stat4(tbl,idx,neq,nlt,ndlt,sample)",
        ),
    ] {
        let actual: String = conn.query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
            [name],
            |row| row.get(0),
        )?;
        if normalized_sql(&actual) != normalized_sql(definition) {
            bail!("lexical statistics table `{name}` is not canonical");
        }
    }

    let expected_tables = BTreeSet::from([
        "chunk_filter".to_string(),
        "chunk_fts".to_string(),
        "chunk_fts_config".to_string(),
        "chunk_fts_data".to_string(),
        "chunk_fts_docsize".to_string(),
        "chunk_fts_idx".to_string(),
        "document_filter".to_string(),
        "lexical_meta".to_string(),
        "sqlite_stat1".to_string(),
        "sqlite_stat4".to_string(),
        "title_fts".to_string(),
        "title_fts_config".to_string(),
        "title_fts_data".to_string(),
        "title_fts_docsize".to_string(),
        "title_fts_idx".to_string(),
    ]);
    let mut statement =
        conn.prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    if tables != expected_tables {
        bail!("lexical sidecar table set is not exact");
    }
    let expected_indexes = BTreeSet::from([
        "document_filter_native_nocase".to_string(),
        "sqlite_autoindex_document_filter_1".to_string(),
    ]);
    let mut statement = conn.prepare("SELECT name FROM sqlite_schema WHERE type='index'")?;
    let indexes = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    if indexes != expected_indexes {
        bail!("lexical sidecar index set is not exact");
    }
    let unexpected_objects: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE type IN ('view', 'trigger')",
        [],
        |row| row.get(0),
    )?;
    if unexpected_objects != 0 {
        bail!("lexical sidecar contains unexpected schema objects");
    }

    for (table, expected_columns) in [
        ("lexical_meta", vec!["key", "value"]),
        (
            "document_filter",
            vec![
                "doc_key",
                "native_id",
                "type",
                "date",
                "withdrawn_date",
                "is_superseded",
                "first_chunk_id",
                "last_chunk_id",
            ],
        ),
        ("chunk_filter", vec!["chunk_id", "doc_key", "is_intro"]),
        ("chunk_fts", vec!["text"]),
        ("chunk_fts_config", vec!["k", "v"]),
        ("chunk_fts_data", vec!["id", "block"]),
        ("chunk_fts_docsize", vec!["id", "sz", "origin"]),
        ("chunk_fts_idx", vec!["segid", "term", "pgno"]),
        ("title_fts", vec!["title", "headings"]),
        ("title_fts_config", vec!["k", "v"]),
        ("title_fts_data", vec!["id", "block"]),
        ("title_fts_docsize", vec!["id", "sz", "origin"]),
        ("title_fts_idx", vec!["segid", "term", "pgno"]),
    ] {
        let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if columns != expected_columns {
            bail!("lexical sidecar `{table}` columns are not exact");
        }
    }
    Ok(())
}

fn verify_fts_rowids(conn: &Connection, identity: &SourceIdentity) -> Result<()> {
    let chunk_fts_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM chunk_fts", [], |row| row.get(0))?;
    let title_fts_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM title_fts", [], |row| row.get(0))?;
    if u64::try_from(chunk_fts_count).ok() != Some(identity.chunk_count)
        || u64::try_from(title_fts_count).ok() != Some(identity.document_count)
    {
        bail!("lexical FTS row counts do not match filter mappings");
    }
    if !ordered_i64_queries_match(
        conn,
        "SELECT chunk_id FROM chunk_filter ORDER BY chunk_id",
        "SELECT rowid FROM chunk_fts ORDER BY rowid",
    )? {
        bail!("chunk_fts rowids do not exactly match chunk_filter.chunk_id");
    }
    if !ordered_i64_queries_match(
        conn,
        "SELECT doc_key FROM document_filter ORDER BY doc_key",
        "SELECT rowid FROM title_fts ORDER BY rowid",
    )? {
        bail!("title_fts rowids do not exactly match document_filter.doc_key");
    }
    Ok(())
}

fn verify_relational_bindings(
    sidecar: &Connection,
    legal: &Connection,
    source_id: &SourceId,
    identity: &SourceIdentity,
) -> Result<SourceTextDigests> {
    let mut title_hasher = Sha256::new();
    title_hasher.update(b"australian-legal-mcp-lexical-title-text-v1\0");
    let mut legal_documents = legal.prepare(
        "SELECT d.native_id, d.type, d.date, d.withdrawn_date,\n                d.superseded_by IS NOT NULL, d.title, d.headings,\n                MIN(c.chunk_id), MAX(c.chunk_id), COUNT(c.chunk_id)\n         FROM documents AS d\n         LEFT JOIN chunks AS c\n           ON c.source_id = d.source_id AND c.native_id = d.native_id\n         WHERE d.source_id = ?1\n         GROUP BY d.native_id COLLATE BINARY\n         ORDER BY d.native_id COLLATE BINARY",
    )?;
    let mut legal_rows = legal_documents.query([source_id.as_str()])?;
    let mut side_documents = sidecar.prepare(
        "SELECT doc_key, native_id, type, date, withdrawn_date, is_superseded,\n                first_chunk_id, last_chunk_id\n         FROM document_filter ORDER BY doc_key",
    )?;
    let mut side_rows = side_documents.query([])?;
    let mut document_count = 0u64;
    loop {
        match (legal_rows.next()?, side_rows.next()?) {
            (Some(legal_row), Some(side_row)) => {
                document_count += 1;
                let doc_key = i64::try_from(document_count)?;
                let native_id = legal_row.get::<_, String>(0)?;
                let document_type = legal_row.get::<_, String>(1)?;
                let date = legal_row.get::<_, Option<String>>(2)?;
                let withdrawn = legal_row.get::<_, Option<String>>(3)?;
                let is_superseded = legal_row.get::<_, i64>(4)?;
                let title = legal_row.get::<_, String>(5)?;
                let headings = legal_row.get::<_, String>(6)?;
                let first = legal_row.get::<_, Option<i64>>(7)?;
                let last = legal_row.get::<_, Option<i64>>(8)?;
                let chunks = legal_row.get::<_, i64>(9)?;
                validate_document_chunk_range(source_id, &native_id, first, last, chunks)?;
                let actual = (
                    side_row.get::<_, i64>(0)?,
                    side_row.get::<_, String>(1)?,
                    side_row.get::<_, String>(2)?,
                    side_row.get::<_, Option<String>>(3)?,
                    side_row.get::<_, Option<String>>(4)?,
                    side_row.get::<_, i64>(5)?,
                    side_row.get::<_, Option<i64>>(6)?,
                    side_row.get::<_, Option<i64>>(7)?,
                );
                let expected = (
                    doc_key,
                    native_id.clone(),
                    document_type,
                    date,
                    withdrawn,
                    is_superseded,
                    first,
                    last,
                );
                if actual != expected {
                    bail!("lexical document filters differ from legal.db");
                }
                hash_field(&mut title_hasher, &doc_key.to_le_bytes());
                hash_field(&mut title_hasher, native_id.as_bytes());
                hash_field(&mut title_hasher, title.as_bytes());
                hash_field(&mut title_hasher, headings.as_bytes());
            }
            (None, None) => break,
            _ => bail!("lexical document mapping cardinality differs from legal.db"),
        }
    }
    if document_count != identity.document_count {
        bail!("lexical document mapping count differs from its manifest");
    }
    title_hasher.update(document_count.to_le_bytes());

    let mut chunk_hasher = Sha256::new();
    chunk_hasher.update(b"australian-legal-mcp-lexical-chunk-text-v1\0");
    let mut legal_chunks = legal.prepare(
        "SELECT chunk_id, native_id, ord, text FROM chunks\n         WHERE source_id = ?1 ORDER BY chunk_id",
    )?;
    let mut legal_rows = legal_chunks.query([source_id.as_str()])?;
    let mut side_chunks = sidecar.prepare(
        "SELECT c.chunk_id, d.native_id, c.is_intro\n         FROM chunk_filter AS c\n         JOIN document_filter AS d ON d.doc_key = c.doc_key\n         ORDER BY c.chunk_id",
    )?;
    let mut side_rows = side_chunks.query([])?;
    let mut chunk_count = 0u64;
    loop {
        match (legal_rows.next()?, side_rows.next()?) {
            (Some(legal_row), Some(side_row)) => {
                let chunk_id = legal_row.get::<_, i64>(0)?;
                let native_id = legal_row.get::<_, String>(1)?;
                if side_row.get::<_, i64>(0)? != chunk_id
                    || side_row.get::<_, String>(1)? != native_id
                {
                    bail!("lexical chunk mapping differs from legal.db");
                }
                let ord = legal_row.get::<_, i64>(2)?;
                let text = decompress_text(legal_row.get::<_, Vec<u8>>(3)?)?;
                if side_row.get::<_, i64>(2)? != i64::from(ord == 0 && text.len() < 100) {
                    bail!("lexical chunk ranking metadata differs from legal.db");
                }
                hash_field(&mut chunk_hasher, &chunk_id.to_le_bytes());
                hash_field(&mut chunk_hasher, text.as_bytes());
                chunk_count += 1;
            }
            (None, None) => break,
            _ => bail!("lexical chunk mapping cardinality differs from legal.db"),
        }
    }
    if chunk_count != identity.chunk_count {
        bail!("lexical chunk mapping count differs from its manifest");
    }
    chunk_hasher.update(chunk_count.to_le_bytes());
    Ok(SourceTextDigests {
        chunk: format!("{:x}", chunk_hasher.finalize()),
        title: format!("{:x}", title_hasher.finalize()),
    })
}

fn verify_sqlite_integrity(conn: &Connection) -> Result<()> {
    for table in ["lexical_meta", "document_filter", "chunk_filter"] {
        let escaped = table.replace('\'', "''");
        let mut statement = conn.prepare(&format!("PRAGMA integrity_check('{escaped}')"))?;
        let values = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if values.as_slice() != ["ok"] {
            bail!("lexical table `{table}` failed SQLite integrity_check: {values:?}");
        }
    }
    conn.execute_batch("PRAGMA query_only=OFF")?;
    let transaction = conn.unchecked_transaction()?;
    transaction.execute(
        "INSERT INTO chunk_fts(chunk_fts) VALUES('integrity-check')",
        [],
    )?;
    transaction.execute(
        "INSERT INTO title_fts(title_fts) VALUES('integrity-check')",
        [],
    )?;
    transaction.rollback()?;
    conn.execute_batch("PRAGMA query_only=ON")?;
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

fn regular_file_metadata(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading lexical sidecar {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "lexical sidecar must be a regular non-symlink file: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            bail!(
                "lexical sidecar must not be hard-linked: {}",
                path.display()
            );
        }
    }
    #[cfg(windows)]
    {
        let file = open_immutable_sidecar_handle(path)?;
        let identity = crate::ann::windows_file_identity(&file)?;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if identity.number_of_links != 1
            || identity.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            bail!(
                "lexical sidecar must not be a reparse point or hard link: {}",
                path.display()
            );
        }
    }
    Ok(metadata)
}

fn runtime_file_stamp(path: &Path, metadata: &fs::Metadata) -> Result<RuntimeFileStamp> {
    let file = open_immutable_sidecar_handle(path)?;
    runtime_file_stamp_from_handle(metadata, &file)
}

fn runtime_file_stamp_from_handle(
    metadata: &fs::Metadata,
    file: &File,
) -> Result<RuntimeFileStamp> {
    #[cfg(not(windows))]
    let _ = file;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    #[cfg(windows)]
    use std::os::windows::fs::MetadataExt;
    #[cfg(windows)]
    let windows_identity = crate::ann::windows_file_identity(file)?;

    Ok(RuntimeFileStamp {
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
        volume_serial_number: Some(windows_identity.volume_serial_number),
        #[cfg(windows)]
        file_index: Some(windows_identity.file_index),
        #[cfg(windows)]
        change_time: Some(metadata.last_write_time()),
        #[cfg(windows)]
        number_of_links: Some(windows_identity.number_of_links),
    })
}

#[cfg(unix)]
fn open_immutable_sidecar_handle(path: &Path) -> Result<File> {
    Ok(File::open(path)?)
}

#[cfg(windows)]
fn open_immutable_sidecar_handle(path: &Path) -> Result<File> {
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
    if identity.number_of_links != 1 || identity.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        bail!(
            "lexical sidecar must not be a reparse point or hard link: {}",
            path.display()
        );
    }
    Ok(file)
}

#[cfg(all(not(unix), not(windows)))]
fn open_immutable_sidecar_handle(path: &Path) -> Result<File> {
    Ok(File::open(path)?)
}

fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(destination) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("existing lexical sidecar destination is not a regular file");
        }
    }
    replace_file_platform(source, destination)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(destination)?
        .sync_all()?;
    #[cfg(not(windows))]
    if let Some(parent) = destination.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file_platform(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file_platform(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn sha256_path(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn parse_u64(value: &str, label: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .with_context(|| format!("lexical {label} `{value}` is malformed"))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_corpus_id(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(is_sha256)
}

#[cfg(test)]
pub(crate) fn test_sidecar_connection(
    legal: &Connection,
    source_id: &SourceId,
) -> Result<Connection> {
    let sidecar = Connection::open_in_memory()?;
    create_schema(&sidecar)?;
    let mut keys = HashMap::new();
    let mut documents = legal.prepare(
        "SELECT d.native_id, d.type, d.date, d.withdrawn_date,
                d.superseded_by IS NOT NULL, d.title, d.headings,
                MIN(c.chunk_id), MAX(c.chunk_id)
         FROM documents AS d
         LEFT JOIN chunks AS c
           ON c.source_id=d.source_id AND c.native_id=d.native_id
         WHERE d.source_id=?1
         GROUP BY d.native_id COLLATE BINARY
         ORDER BY d.native_id COLLATE BINARY",
    )?;
    let mut rows = documents.query([source_id.as_str()])?;
    let mut doc_key = 0i64;
    while let Some(row) = rows.next()? {
        doc_key += 1;
        let native_id = row.get::<_, String>(0)?;
        sidecar.execute(
            "INSERT INTO document_filter(
                 doc_key, native_id, type, date, withdrawn_date, is_superseded,
                 first_chunk_id, last_chunk_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                doc_key,
                native_id,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<i64>>(8)?,
            ],
        )?;
        sidecar.execute(
            "INSERT INTO title_fts(rowid, title, headings) VALUES (?1, ?2, ?3)",
            params![doc_key, row.get::<_, String>(5)?, row.get::<_, String>(6)?],
        )?;
        keys.insert(native_id, doc_key);
    }
    let mut chunks = legal.prepare(
        "SELECT chunk_id, native_id, ord, text FROM chunks
         WHERE source_id=?1 ORDER BY chunk_id",
    )?;
    let mut rows = chunks.query([source_id.as_str()])?;
    while let Some(row) = rows.next()? {
        let chunk_id = row.get::<_, i64>(0)?;
        let native_id = row.get::<_, String>(1)?;
        let doc_key = keys
            .get(&native_id)
            .copied()
            .ok_or_else(|| anyhow!("test chunk has no document mapping"))?;
        let ord = row.get::<_, i64>(2)?;
        let text = decompress_text(row.get::<_, Vec<u8>>(3)?)?;
        sidecar.execute(
            "INSERT INTO chunk_filter(chunk_id, doc_key, is_intro) VALUES (?1, ?2, ?3)",
            params![chunk_id, doc_key, i64::from(ord == 0 && text.len() < 100)],
        )?;
        sidecar.execute(
            "INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)",
            params![chunk_id, text],
        )?;
    }
    Ok(sidecar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{compress_text, init_db, set_source_meta};
    use rusqlite::params;
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom, Write};

    fn source() -> SourceId {
        "ato".parse().expect("valid source")
    }

    fn fixture_legal() -> Result<Connection> {
        let conn = Connection::open_in_memory()?;
        init_db(&conn)?;
        conn.execute(
            "INSERT INTO sources(source_id, display_name) VALUES ('ato', 'ATO')",
            [],
        )?;
        let documents = [
            ("PAC/OLD", "PAC", Some("1990-01-01"), None, "Rare Alpha"),
            ("EV/NEW", "EV", Some("2025-01-01"), None, "Rare Alpha"),
            ("TXR/OLD", "TXR", Some("1990-01-01"), None, "Common Beta"),
            (
                "TXR/WITHDRAWN",
                "TXR",
                Some("2025-01-01"),
                Some("2026-01-01"),
                "Common Gamma",
            ),
            (
                "TXR/CURRENT",
                "TXR",
                Some("2025-02-01"),
                None,
                "Common Delta",
            ),
            ("META/ONLY", "TXR", None, None, "Metadata Only"),
        ];
        for (native_id, document_type, date, withdrawn, title) in documents {
            conn.execute(
                "INSERT INTO documents(
                     source_id, native_id, type, title, date, canonical_url,
                     downloaded_at, content_hash, html, withdrawn_date, headings
                 ) VALUES ('ato', ?1, ?2, ?3, ?4, ?5,
                           '2026-01-01T00:00:00Z', ?1, X'00', ?6, 'Fixture heading')",
                params![
                    native_id,
                    document_type,
                    title,
                    date,
                    format!("https://example.invalid/{native_id}"),
                    withdrawn,
                ],
            )?;
        }
        for (chunk_id, native_id, text) in [
            (1_i64, "PAC/OLD", "rare alpha"),
            (2, "EV/NEW", "rare alpha"),
            (3, "TXR/OLD", "common beta"),
            (4, "TXR/WITHDRAWN", "common gamma"),
            (5, "TXR/CURRENT", "common delta"),
        ] {
            conn.execute(
                "INSERT INTO chunks(chunk_id, source_id, native_id, ord, text)
                 VALUES (?1, 'ato', ?2, 0, ?3)",
                params![chunk_id, native_id, compress_text(text)?],
            )?;
        }
        let digest = "1".repeat(64);
        for (key, value) in [
            ("source_index_sha256", digest.clone()),
            ("corpus_id", format!("sha256:{digest}")),
            ("documents_count", "6".to_string()),
            ("chunks_count", "5".to_string()),
        ] {
            set_source_meta(&conn, "ato", key, &value)?;
        }
        Ok(conn)
    }

    #[test]
    fn deterministic_build_is_byte_identical_and_payload_free() -> Result<()> {
        let legal = fixture_legal()?;
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        let database_sha = "2".repeat(64);
        let first_info = build_sidecar(&legal, &source(), first.path(), &database_sha)?;
        let second_info = build_sidecar(&legal, &source(), second.path(), &database_sha)?;
        assert_eq!(first_info, second_info);
        let first_bytes = fs::read(first.path().join(&first_info.path))?;
        let second_bytes = fs::read(second.path().join(&second_info.path))?;
        assert_eq!(
            first_bytes, second_bytes,
            "repeated builds must be byte-identical"
        );

        let sidecar = Connection::open(first.path().join(&first_info.path))?;
        let chunk_payload: Option<String> =
            sidecar.query_row("SELECT text FROM chunk_fts WHERE rowid=1", [], |row| {
                row.get(0)
            })?;
        let title_payload: (Option<String>, Option<String>) = sidecar.query_row(
            "SELECT title, headings FROM title_fts WHERE rowid=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(chunk_payload, None);
        assert_eq!(title_payload, (None, None));
        for forbidden in [
            "canonical_url",
            "html",
            "asset",
            "embedding",
            "snippet",
            "downloaded_at",
        ] {
            let count: i64 = sidecar.query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                 WHERE lower(coalesce(sql, '')) LIKE '%' || ?1 || '%'",
                [forbidden],
                |row| row.get(0),
            )?;
            assert_eq!(
                count, 0,
                "sidecar retained forbidden payload column {forbidden}"
            );
        }
        Ok(())
    }

    #[test]
    fn case_distinct_native_ids_remain_distinct_with_nocase_scope_lookup() -> Result<()> {
        let legal = Connection::open_in_memory()?;
        init_db(&legal)?;
        legal.execute(
            "INSERT INTO sources(source_id, display_name) VALUES ('ato', 'ATO')",
            [],
        )?;
        for (chunk_id, native_id) in [(1_i64, "Case/ID"), (2, "case/id")] {
            legal.execute(
                "INSERT INTO documents(
                     source_id, native_id, type, title, canonical_url,
                     downloaded_at, content_hash, html
                 ) VALUES ('ato', ?1, 'TXR', ?1, ?2,
                           '2026-01-01T00:00:00Z', ?1, X'00')",
                params![native_id, format!("https://example.invalid/{chunk_id}")],
            )?;
            legal.execute(
                "INSERT INTO chunks(chunk_id, source_id, native_id, ord, text)
                 VALUES (?1, 'ato', ?2, 0, ?3)",
                params![chunk_id, native_id, compress_text("distinct mapping")?],
            )?;
        }
        let sidecar = test_sidecar_connection(&legal, &source())?;
        let exact: i64 = sidecar.query_row(
            "SELECT COUNT(*) FROM document_filter WHERE native_id IN ('Case/ID', 'case/id')",
            [],
            |row| row.get(0),
        )?;
        let scoped: i64 = sidecar.query_row(
            "SELECT COUNT(*) FROM document_filter WHERE native_id LIKE 'CASE/ID'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(exact, 2);
        assert_eq!(scoped, 2);
        Ok(())
    }

    #[test]
    fn abandoned_owned_build_directory_is_removed_but_unknown_entries_fail_closed() -> Result<()> {
        let root = tempfile::tempdir()?;
        let owned = root.path().join("lexical-ato-build-interrupted");
        let unknown = root.path().join("unexpected-directory");
        fs::create_dir(&owned)?;
        fs::write(owned.join("ato.db.part"), b"partial")?;
        fs::create_dir(&unknown)?;
        remove_abandoned_build_directories(root.path())?;
        assert!(!owned.exists());
        assert!(unknown.is_dir());
        let empty = BTreeMap::new();
        assert!(verify_sidecar_file_set(root.path(), &empty).is_err());
        Ok(())
    }

    #[test]
    fn independent_bm25_reference_and_chunk_id_tie_order_are_exact() -> Result<()> {
        let legal = fixture_legal()?;
        let root = tempfile::tempdir()?;
        let info = build_sidecar(&legal, &source(), root.path(), &"2".repeat(64))
            .context("building BM25 reference sidecar")?;
        let sidecar = Connection::open(root.path().join(info.path))
            .context("opening BM25 reference sidecar")?;
        let mut statement = sidecar.prepare(
            "SELECT rowid, -bm25(chunk_fts) AS score
             FROM chunk_fts WHERE chunk_fts MATCH 'rare'
             ORDER BY score DESC, rowid ASC",
        )?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(rows.iter().map(|row| row.0).collect::<Vec<_>>(), vec![1, 2]);

        // Published FTS5 BM25 constants: k1=1.2, b=0.75. The fixture has
        // N=5, df(rare)=2, average length=2, and tf=1 in both matching rows.
        let idf = ((5.0_f64 - 2.0 + 0.5) / (2.0 + 0.5)).ln();
        let expected = idf * (1.0 * (1.2 + 1.0)) / (1.0 + 1.2 * (1.0 - 0.75 + 0.75 * 2.0 / 2.0));
        for (_, actual) in rows {
            assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
        }

        let mut statement = sidecar.prepare(
            "SELECT d.native_id, -bm25(title_fts) AS score
             FROM title_fts AS t
             JOIN document_filter AS d ON d.doc_key=t.rowid
             WHERE title_fts MATCH 'rare'
             ORDER BY score DESC, d.native_id COLLATE BINARY ASC",
        )?;
        let title_rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(
            title_rows
                .iter()
                .map(|row| row.0.as_str())
                .collect::<Vec<_>>(),
            vec!["EV/NEW", "PAC/OLD"]
        );
        let title_idf = ((6.0_f64 - 2.0 + 0.5) / (2.0 + 0.5)).ln();
        for (_, actual) in title_rows {
            assert!(
                (actual - title_idf).abs() < 1e-12,
                "{actual} != {title_idf}"
            );
        }
        Ok(())
    }

    #[test]
    fn sidecar_verification_fails_closed_for_missing_extra_wrong_source_hash_and_corruption(
    ) -> Result<()> {
        let legal = fixture_legal()?;
        let root = tempfile::tempdir()?;
        let database_sha = "2".repeat(64);
        let info = build_sidecar(&legal, &source(), root.path(), &database_sha)?;
        let path = root.path().join(&info.path);

        let missing = root.path().join("lexical/missing.db");
        assert!(verify_sidecar(&missing, &source(), &info, &legal, &database_sha, false).is_err());

        let mut wrong_hash = info.clone();
        wrong_hash.sha256 = "3".repeat(64);
        let hash_error =
            verify_sidecar(&path, &source(), &wrong_hash, &legal, &database_sha, false)
                .unwrap_err();
        assert!(hash_error.to_string().contains("hash or size"));

        let frl: SourceId = "frl".parse()?;
        assert!(validate_manifest_lexical(&frl, &info).is_err());

        let map = BTreeMap::from([(source(), info.clone())]);
        let directory = root.path().join(LEXICAL_DIRECTORY);
        verify_sidecar_file_set(&directory, &map)?;
        fs::write(directory.join("extra.db"), b"extra")?;
        assert!(verify_sidecar_file_set(&directory, &map).is_err());
        fs::remove_file(directory.join("extra.db"))?;

        let corrupt_path = root.path().join("corrupt.db");
        fs::copy(&path, &corrupt_path)?;
        let mut file = OpenOptions::new().write(true).open(&corrupt_path)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(b"NOTSQLITE")?;
        file.sync_all()?;
        drop(file);
        let mut corrupt_info = info.clone();
        corrupt_info.size = fs::metadata(&corrupt_path)?.len();
        corrupt_info.sha256 = sha256_path(&corrupt_path)?;
        assert!(verify_sidecar(
            &corrupt_path,
            &source(),
            &corrupt_info,
            &legal,
            &database_sha,
            false,
        )
        .is_err());

        fs::remove_file(&path)?;
        assert!(verify_sidecar_file_set(&directory, &map).is_err());
        Ok(())
    }

    fn rebind_file(path: &Path, template: &ManifestLexical) -> Result<ManifestLexical> {
        let mut rebound = template.clone();
        rebound.size = fs::metadata(path)?.len();
        rebound.sha256 = sha256_path(path)?;
        Ok(rebound)
    }

    #[test]
    fn strict_verification_rejects_rebound_posting_mapping_and_database_hash_corruption(
    ) -> Result<()> {
        let legal = fixture_legal()?;
        let root = tempfile::tempdir()?;
        let database_sha = "2".repeat(64);
        let info = build_sidecar(&legal, &source(), root.path(), &database_sha)?;
        let original = root.path().join(&info.path);

        let chunk_postings = root.path().join("chunk-postings.db");
        fs::copy(&original, &chunk_postings)?;
        let connection = Connection::open(&chunk_postings)?;
        connection.execute("DELETE FROM chunk_fts WHERE rowid=1", [])?;
        connection.execute(
            "INSERT INTO chunk_fts(rowid, text) VALUES (1, 'replacement tokens')",
            [],
        )?;
        drop(connection);
        let rebound = rebind_file(&chunk_postings, &info)?;
        let error = verify_sidecar(
            &chunk_postings,
            &source(),
            &rebound,
            &legal,
            &database_sha,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("FTS postings"));

        let title_postings = root.path().join("title-postings.db");
        fs::copy(&original, &title_postings)?;
        let connection = Connection::open(&title_postings)?;
        connection.execute("DELETE FROM title_fts WHERE rowid=1", [])?;
        connection.execute(
            "INSERT INTO title_fts(rowid, title, headings) VALUES (1, 'wrong title', '')",
            [],
        )?;
        drop(connection);
        let rebound = rebind_file(&title_postings, &info)?;
        let error = verify_sidecar(
            &title_postings,
            &source(),
            &rebound,
            &legal,
            &database_sha,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("FTS postings"));

        let mapping = root.path().join("mapping.db");
        fs::copy(&original, &mapping)?;
        let connection = Connection::open(&mapping)?;
        connection.execute("UPDATE chunk_filter SET doc_key=2 WHERE chunk_id=1", [])?;
        drop(connection);
        let rebound = rebind_file(&mapping, &info)?;
        let error = verify_sidecar(&mapping, &source(), &rebound, &legal, &database_sha, false)
            .unwrap_err();
        assert!(error.to_string().contains("chunk mapping"));

        let error = verify_sidecar(&original, &source(), &info, &legal, &"4".repeat(64), false)
            .unwrap_err();
        assert!(error.to_string().contains("immutable binding"));
        Ok(())
    }

    #[test]
    fn strict_verification_recomputes_authoritative_source_text_binding() -> Result<()> {
        let legal = fixture_legal()?;
        let root = tempfile::tempdir()?;
        let database_sha = "2".repeat(64);
        let info = build_sidecar(&legal, &source(), root.path(), &database_sha)?;
        legal.execute(
            "UPDATE chunks SET text=?1 WHERE chunk_id=1",
            [compress_text("changed authoritative text")?],
        )?;
        let error = verify_sidecar(
            &root.path().join(&info.path),
            &source(),
            &info,
            &legal,
            &database_sha,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("source-text binding"));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires LEGAL_MCP_BENCH_DB, LEGAL_MCP_BENCH_OUTPUT_ROOT, and LEGAL_MCP_BENCH_SOURCE"]
    fn benchmark_installed_source_sidecar() -> Result<()> {
        let required = |name: &str| {
            std::env::var(name).with_context(|| format!("{name} is required for this benchmark"))
        };
        let db = PathBuf::from(required("LEGAL_MCP_BENCH_DB")?);
        let output_root = PathBuf::from(required("LEGAL_MCP_BENCH_OUTPUT_ROOT")?);
        let source_id: SourceId = required("LEGAL_MCP_BENCH_SOURCE")?
            .parse()
            .context("LEGAL_MCP_BENCH_SOURCE is invalid")?;
        let db_sha256 = sha256_path(&db)?;
        let query = std::env::var("LEGAL_MCP_BENCH_QUERY").unwrap_or_else(|_| {
            "moreton resources innovation science australia activities".to_string()
        });
        let expected_ids = match std::env::var("LEGAL_MCP_BENCH_EXPECTED_IDS") {
            Ok(value) => value
                .split(',')
                .map(|item| item.trim().parse::<i64>().map_err(Into::into))
                .collect::<Result<Vec<_>>>()?,
            Err(_) if source_id.as_str() == "federal-court" => vec![
                5_433_720, 5_578_447, 5_433_795, 5_494_336, 5_433_665, 5_668_951, 5_326_814,
                4_135_052,
            ],
            Err(_) => bail!(
                "LEGAL_MCP_BENCH_EXPECTED_IDS is required outside the Federal Court QA benchmark"
            ),
        };
        let runs = std::env::var("LEGAL_MCP_BENCH_RUNS")
            .unwrap_or_else(|_| "50".to_string())
            .parse::<usize>()?;
        if runs == 0 {
            bail!("LEGAL_MCP_BENCH_RUNS must be positive");
        }
        let legal = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let build_started = std::time::Instant::now();
        let info = build_sidecar(&legal, &source_id, &output_root, &db_sha256)?;
        let build_seconds = build_started.elapsed().as_secs_f64();
        let sidecar = Connection::open_with_flags(
            output_root.join(&info.path),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let filter = crate::search::build_lexical_doc_filter(
            "d",
            crate::search::DocumentFilterSpec {
                source_id: &source_id,
                types: None,
                date_from: None,
                date_to: None,
                doc_scope: None,
                include_old: false,
                current_only: true,
            },
        );
        for _ in 0..5 {
            crate::search::lexical_search(&sidecar, &source_id, &query, &filter, 50)?;
        }
        let mut times_ms = Vec::with_capacity(runs);
        for _ in 0..runs {
            let started = std::time::Instant::now();
            let hits = crate::search::lexical_search(&sidecar, &source_id, &query, &filter, 50)?;
            times_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
            let ids = hits.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>();
            if ids != expected_ids {
                bail!("lexical benchmark results differ from the exact expected chunk IDs");
            }
        }
        let mut ordered = times_ms.clone();
        ordered.sort_by(f64::total_cmp);
        let percentile = |fraction: f64| {
            let index = ((fraction * ordered.len() as f64).ceil() as usize)
                .saturating_sub(1)
                .min(ordered.len() - 1);
            ordered[index]
        };
        let warm_p95_ms = percentile(0.95);
        let p95_limit_ms = std::env::var("LEGAL_MCP_BENCH_P95_LIMIT_MS")
            .unwrap_or_else(|_| "100".to_string())
            .parse::<f64>()?;
        if !p95_limit_ms.is_finite() || p95_limit_ms <= 0.0 || warm_p95_ms >= p95_limit_ms {
            bail!(
                "lexical benchmark warm p95 {warm_p95_ms:.3} ms does not satisfy the {p95_limit_ms:.3} ms limit"
            );
        }
        drop(sidecar);

        let (cold_times_ms, cold_median_ms, cold_p95_ms, cold_max_ms, cold_p95_limit_ms) = {
            use std::os::fd::AsRawFd;

            let cold_runs = std::env::var("LEGAL_MCP_BENCH_COLD_RUNS")
                .unwrap_or_else(|_| "30".to_string())
                .parse::<usize>()?;
            if cold_runs == 0 {
                bail!("LEGAL_MCP_BENCH_COLD_RUNS must be positive");
            }
            let cold_p95_limit_ms = std::env::var("LEGAL_MCP_BENCH_COLD_P95_LIMIT_MS")
                .unwrap_or_else(|_| "100".to_string())
                .parse::<f64>()?;
            if !cold_p95_limit_ms.is_finite() || cold_p95_limit_ms <= 0.0 {
                bail!("LEGAL_MCP_BENCH_COLD_P95_LIMIT_MS must be finite and positive");
            }
            let sidecar_path = output_root.join(&info.path);
            let mut cold_times_ms = Vec::with_capacity(cold_runs);
            for _ in 0..cold_runs {
                let file = std::fs::File::open(&sidecar_path)?;
                // SAFETY: `file` owns a live read-only descriptor, offset and length are
                // non-negative, and POSIX defines a zero length as extending to EOF.
                let status = unsafe {
                    libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED)
                };
                if status != 0 {
                    bail!("POSIX_FADV_DONTNEED failed with status {status}");
                }
                drop(file);

                let cold_sidecar = Connection::open_with_flags(
                    &sidecar_path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                configure_read_connection(&cold_sidecar)?;
                let started = std::time::Instant::now();
                let hits =
                    crate::search::lexical_search(&cold_sidecar, &source_id, &query, &filter, 50)?;
                cold_times_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
                let ids = hits.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>();
                if ids != expected_ids {
                    bail!(
                        "cold lexical benchmark results differ from the exact expected chunk IDs"
                    );
                }
                cold_sidecar
                    .close()
                    .map_err(|(_, error)| error)
                    .context("closing cold lexical benchmark sidecar")?;
            }
            let mut ordered = cold_times_ms.clone();
            ordered.sort_by(f64::total_cmp);
            let percentile = |fraction: f64| {
                let index = ((fraction * ordered.len() as f64).ceil() as usize)
                    .saturating_sub(1)
                    .min(ordered.len() - 1);
                ordered[index]
            };
            let cold_p95_ms = percentile(0.95);
            if cold_p95_ms >= cold_p95_limit_ms {
                bail!(
                    "lexical benchmark cold p95 {cold_p95_ms:.3} ms does not satisfy the {cold_p95_limit_ms:.3} ms limit"
                );
            }
            (
                cold_times_ms,
                percentile(0.5),
                cold_p95_ms,
                ordered[ordered.len() - 1],
                cold_p95_limit_ms,
            )
        };
        let report = serde_json::json!({
            "source": source_id,
            "query": query,
            "build_seconds": build_seconds,
            "sidecar": info,
            "runs": runs,
            "warm_median_ms": percentile(0.5),
            "warm_p95_ms": warm_p95_ms,
            "warm_p95_limit_ms": p95_limit_ms,
            "warm_max_ms": ordered[ordered.len() - 1],
            "cold_runs": cold_times_ms.len(),
            "cold_median_ms": cold_median_ms,
            "cold_p95_ms": cold_p95_ms,
            "cold_p95_limit_ms": cold_p95_limit_ms,
            "cold_max_ms": cold_max_ms,
            "result_ids": expected_ids,
            "times_ms": times_ms,
            "cold_times_ms": cold_times_ms,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        Ok(())
    }
}
