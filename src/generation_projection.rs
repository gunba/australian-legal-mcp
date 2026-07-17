//! Deterministic schema-10 to schema-11 generation projection.
//!
//! This maintainer-only path recognizes exactly one legacy schema. It copies a
//! validated immutable generation, rebuilds only the chunk FTS storage from the
//! text already held by schema 10, and never enters acquisition, chunking,
//! model tokenization, model execution, embedding, or ANN construction code.
//! SQLite FTS5 necessarily tokenizes the existing text while rebuilding its
//! contentless keyword index.

use crate::config::{GENERATION_MANIFEST_FILENAME, LEGAL_DB_FILENAME};
use crate::source::{GenerationId, Manifest, ManifestDb};
use crate::SUPPORTED_SCHEMA_VERSION;
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) const LEGACY_SCHEMA_VERSION: u32 = 10;
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PROJECTION_DB_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const PROJECTION_SPACE_MARGIN_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const LEGACY_CHUNKS_FTS_SQL: &str = r#"CREATE VIRTUAL TABLE chunks_fts USING fts5(
    text,
    tokenize = "porter unicode61 remove_diacritics 2"
)"#;
const LEGACY_FTS_TEMP_NAME: &str = "chunks_fts_schema10_projection";

pub(crate) struct DeriveSchema11Args<'a> {
    pub(crate) source_generation_dir: &'a Path,
    pub(crate) expected_source_generation: &'a GenerationId,
    pub(crate) out_dir: &'a Path,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProjectionReport {
    source_generation: GenerationId,
    generation: GenerationId,
    schema_version: u32,
    output_dir: String,
    database_size: u64,
    chunks: u64,
    chunk_embeddings: u64,
    embedding_cache_rows_removed: u64,
    reflinked_files: usize,
    copied_files: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyMethod {
    Reflink,
    Copy,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowsFileIdentity {
    file_attributes: u32,
    volume_serial_number: u32,
    file_index: u64,
    number_of_links: u32,
}

#[derive(Debug)]
struct ArtifactSnapshot {
    relative_path: String,
    size: u64,
    sha256: String,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    windows_identity: WindowsFileIdentity,
}

#[derive(Debug)]
struct DirectorySnapshot {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    windows_identity: WindowsFileIdentity,
}

#[derive(Debug)]
struct ValidatedLegacyGeneration {
    root: PathBuf,
    root_snapshot: DirectorySnapshot,
    ann_snapshot: DirectorySnapshot,
    manifest: Manifest,
    manifest_snapshot: ArtifactSnapshot,
    generation: GenerationId,
    artifacts: BTreeMap<String, ArtifactSnapshot>,
    chunks: u64,
    chunk_embeddings: u64,
    embedding_cache_rows: u64,
}

#[derive(Debug)]
struct ProjectionPaths {
    source_root: PathBuf,
    output_parent: PathBuf,
    output_root: PathBuf,
}

struct FreshOutput {
    root: PathBuf,
    root_snapshot: DirectorySnapshot,
    parent_snapshot: DirectorySnapshot,
    keep: bool,
}

impl FreshOutput {
    fn claim(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("projection output has no parent"))?;
        let parent_snapshot = snapshot_directory(parent, "projection output parent")?;
        fs::create_dir(path)
            .with_context(|| format!("claiming fresh projection output {}", path.display()))?;
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("projection output must be a real directory");
        }
        Ok(Self {
            root: path.to_path_buf(),
            root_snapshot: snapshot_directory(path, "projection output")?,
            parent_snapshot,
            keep: false,
        })
    }

    fn ensure_identity(&self) -> Result<()> {
        let root = snapshot_directory(&self.root, "projection output")?;
        if !same_directory_identity(&self.root_snapshot, &root) {
            bail!("claimed projection output directory was replaced");
        }
        let parent = self
            .root
            .parent()
            .ok_or_else(|| anyhow!("projection output has no parent"))?;
        let current_parent = snapshot_directory(parent, "projection output parent")?;
        if !same_directory_identity(&self.parent_snapshot, &current_parent) {
            bail!("projection output parent directory was replaced");
        }
        Ok(())
    }

    fn preserve(&mut self) {
        self.keep = true;
    }

    fn cleanup(&mut self) -> Result<()> {
        if self.keep {
            return Ok(());
        }
        match self.ensure_identity() {
            Ok(()) => {}
            Err(_error)
                if fs::symlink_metadata(&self.root)
                    .is_err_and(|io_error| io_error.kind() == io::ErrorKind::NotFound) =>
            {
                return Ok(())
            }
            Err(error) => {
                return Err(error).context(
                    "refusing to remove an incomplete projection whose directory identity changed",
                )
            }
        }
        match fs::remove_dir_all(&self.root) {
            Ok(()) => sync_directory(
                self.root
                    .parent()
                    .ok_or_else(|| anyhow!("projection output has no parent"))?,
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("removing incomplete projection {}", self.root.display())),
        }
    }
}

impl Drop for FreshOutput {
    fn drop(&mut self) {
        if !self.keep && self.ensure_identity().is_ok() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

pub(crate) fn derive_schema11_from_schema10(
    args: DeriveSchema11Args<'_>,
) -> Result<ProjectionReport> {
    if SUPPORTED_SCHEMA_VERSION != 11 {
        bail!("schema-10 projection is available only in a schema-11 binary");
    }
    let paths = validate_projection_paths(args.source_generation_dir, args.out_dir)?;
    let legacy = validate_legacy_generation(&paths.source_root, args.expected_source_generation)?;

    let reflink_supported = probe_reflink_support(&paths.output_parent)?;
    let required_space = required_projection_space(&legacy.manifest, reflink_supported)?;
    ensure_available_space(&paths.output_parent, required_space)?;

    let mut output = FreshOutput::claim(&paths.output_root)?;
    let result = derive_into_fresh_output(&legacy, &paths, required_space, &mut output);
    match result {
        Ok(report) => {
            output.preserve();
            Ok(report)
        }
        Err(error) => {
            output.cleanup().with_context(|| {
                format!(
                    "projection failed ({error:#}) and its incomplete output could not be removed"
                )
            })?;
            Err(error)
        }
    }
}

fn derive_into_fresh_output(
    legacy: &ValidatedLegacyGeneration,
    paths: &ProjectionPaths,
    required_space: u64,
    output: &mut FreshOutput,
) -> Result<ProjectionReport> {
    let manifest_path = output.root.join(GENERATION_MANIFEST_FILENAME);
    if manifest_path.exists() {
        bail!("fresh projection unexpectedly contains generation.json");
    }
    output.ensure_identity()?;

    // Recheck immediately before the largest copy. A reflink is an
    // optimization only; the preflight remains safe when copying is required.
    ensure_available_space(&paths.output_parent, required_space)?;
    let source_db = legacy
        .artifacts
        .get(LEGAL_DB_FILENAME)
        .ok_or_else(|| anyhow!("validated legacy database snapshot is missing"))?;
    let destination_db = output.root.join(LEGAL_DB_FILENAME);
    let _database_copy_method = clone_or_copy_validated(&legacy.root, source_db, &destination_db)?;

    let projection = project_database(&destination_db, &legacy.manifest, &paths.output_parent)?;
    if projection.chunks != legacy.chunks || projection.chunk_embeddings != legacy.chunk_embeddings
    {
        bail!("chunk or embedding count changed during schema projection");
    }

    let ann_dir = output.root.join(crate::ann::ANN_DIRECTORY);
    fs::create_dir(&ann_dir)?;
    let ann_snapshot = snapshot_directory(&ann_dir, "projection ANN directory")?;
    let mut reflinked_files = 0;
    let mut copied_files = 0;
    for relative in immutable_artifact_paths(&legacy.manifest) {
        output.ensure_identity()?;
        if relative.starts_with("ann/")
            && !same_directory_identity(
                &ann_snapshot,
                &snapshot_directory(&ann_dir, "projection ANN directory")?,
            )
        {
            bail!("projection ANN directory was replaced");
        }
        let snapshot = legacy
            .artifacts
            .get(&relative)
            .ok_or_else(|| anyhow!("validated legacy artifact snapshot is missing `{relative}`"))?;
        let method = clone_or_copy_validated(&legacy.root, snapshot, &output.root.join(&relative))?;
        match method {
            CopyMethod::Reflink => reflinked_files += 1,
            CopyMethod::Copy => copied_files += 1,
        }
    }
    if !same_directory_identity(
        &ann_snapshot,
        &snapshot_directory(&ann_dir, "projection ANN directory")?,
    ) {
        bail!("projection ANN directory was replaced");
    }
    sync_directory(&ann_dir)?;
    output.ensure_identity()?;

    let db_size = fs::metadata(&destination_db)?.len();
    validate_projection_db_size(db_size, "projected")?;
    let db_sha256 = sha256_path(&destination_db)?;
    let mut manifest = legacy.manifest.clone();
    manifest.schema_version = SUPPORTED_SCHEMA_VERSION;
    manifest.min_client_version = env!("CARGO_PKG_VERSION").to_string();
    manifest.db = ManifestDb {
        path: LEGAL_DB_FILENAME.to_string(),
        sha256: db_sha256,
        size: db_size,
    };
    crate::source::validate_manifest(&manifest)?;

    // The source is re-opened and re-hashed after all copies so concurrent
    // replacement or in-place mutation cannot silently produce a candidate.
    revalidate_legacy_generation(legacy)?;

    for relative in immutable_artifact_paths(&manifest) {
        let info = legacy
            .artifacts
            .get(&relative)
            .ok_or_else(|| anyhow!("missing artifact snapshot for `{relative}`"))?;
        validate_regular_file(&output.root.join(&relative), info.size, &info.sha256, false)?;
    }
    sync_file(&destination_db)?;
    sync_directory(&output.root)?;
    output.ensure_identity()?;

    // generation.json is deliberately the final generation artifact.
    crate::config::atomic_write(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)?;
    sync_directory(&output.root)?;
    sync_directory(&paths.output_parent)?;
    output.ensure_identity()?;

    let (_, generation) = crate::source::validate_generation_dir(&output.root)
        .context("strictly validating completed projected generation")?;
    let expected_generation = crate::source::generation_key(&manifest)?;
    if generation != expected_generation {
        bail!("projected generation ID changed during final validation");
    }

    Ok(ProjectionReport {
        source_generation: legacy.generation.clone(),
        generation,
        schema_version: SUPPORTED_SCHEMA_VERSION,
        output_dir: output.root.display().to_string(),
        database_size: db_size,
        chunks: projection.chunks,
        chunk_embeddings: projection.chunk_embeddings,
        embedding_cache_rows_removed: legacy.embedding_cache_rows,
        reflinked_files,
        copied_files,
    })
}

#[derive(Debug)]
struct DatabaseProjection {
    chunks: u64,
    chunk_embeddings: u64,
}

fn project_database(
    path: &Path,
    legacy_manifest: &Manifest,
    space_path: &Path,
) -> Result<DatabaseProjection> {
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening copied legacy database {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(60))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.execute_batch("PRAGMA cell_size_check=ON; PRAGMA query_only=ON;")?;
    enforce_legacy_schema(&conn)?;
    validate_legacy_chunks_fts_schema(&conn)?;
    crate::source::verify_corpus_manifest_binding(&conn, legacy_manifest)?;
    verify_declared_counts(&conn)?;
    crate::db::verify_fts_relational_bindings(&conn)?;
    verify_foreign_keys(&conn)?;
    verify_ordinary_integrity(&conn)?;
    conn.execute_batch("PRAGMA query_only=OFF")?;
    verify_fts_integrity(&conn)?;

    let chunks = table_count(&conn, "chunks")?;
    let chunk_embeddings = table_count(&conn, "chunk_embeddings")?;
    if chunks != chunk_embeddings {
        bail!("legacy database has {chunks} chunks but {chunk_embeddings} embeddings");
    }

    let legacy_db_size = fs::metadata(path)?.len();
    ensure_available_space(
        space_path,
        legacy_db_size
            .checked_add(PROJECTION_SPACE_MARGIN_BYTES)
            .ok_or_else(|| anyhow!("FTS rebuild space requirement overflow"))?,
    )?;

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    enforce_legacy_schema(&tx)?;
    validate_legacy_chunks_fts_schema(&tx)?;
    let projection_temp_exists: i64 = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = ?1)",
        [LEGACY_FTS_TEMP_NAME],
        |row| row.get(0),
    )?;
    if projection_temp_exists != 0 {
        bail!("legacy database already contains reserved projection objects");
    }
    tx.execute_batch(&format!(
        "ALTER TABLE chunks_fts RENAME TO {LEGACY_FTS_TEMP_NAME};\n{};",
        crate::db::CHUNKS_FTS_V11_SQL
    ))?;
    tx.execute_batch(&format!(
        "INSERT INTO chunks_fts(rowid, text)\n\
         SELECT rowid, text FROM {LEGACY_FTS_TEMP_NAME} ORDER BY rowid;"
    ))?;
    let old_rows = table_count(&tx, LEGACY_FTS_TEMP_NAME)?;
    let new_rows = table_count(&tx, "chunks_fts")?;
    if old_rows != chunks || new_rows != chunks {
        bail!(
            "FTS projection row count changed: legacy={old_rows}, projected={new_rows}, chunks={chunks}"
        );
    }
    if !ordered_rowids_match(
        &tx,
        &format!("SELECT rowid FROM {LEGACY_FTS_TEMP_NAME} ORDER BY rowid"),
        "SELECT rowid FROM chunks_fts ORDER BY rowid",
    )? {
        bail!("FTS projection changed chunk rowids");
    }
    let legacy_fts_digest = crate::db::chunks_fts_logical_sha256(&tx, LEGACY_FTS_TEMP_NAME)?;
    let projected_logical_fts_digest = crate::db::chunks_fts_logical_sha256(&tx, "chunks_fts")?;
    if projected_logical_fts_digest != legacy_fts_digest {
        bail!(
            "schema-11 chunks_fts postings or BM25 metadata differ from the validated schema-10 index"
        );
    }
    tx.execute_batch(&format!("DROP TABLE {LEGACY_FTS_TEMP_NAME};"))?;
    tx.execute("DELETE FROM embedding_cache", [])?;
    tx.commit()?;

    // FTS5 may finalize segment structure at transaction commit. Compute the
    // efficient runtime storage digest only after that boundary, then bind it
    // before the schema version is changed as the final logical write.
    let projected_fts_digest = crate::db::chunks_fts_index_sha256(&conn, "chunks_fts")?;
    let binding_tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    enforce_legacy_schema(&binding_tx)?;
    crate::db::validate_chunks_fts_schema(&binding_tx)?;
    crate::db::set_corpus_meta(
        &binding_tx,
        "chunks_fts_index_sha256",
        &projected_fts_digest,
    )?;
    let updated = binding_tx.execute(
        "UPDATE corpus_meta SET value = ?1 WHERE key = 'schema_version' AND value = ?2",
        [
            SUPPORTED_SCHEMA_VERSION.to_string(),
            LEGACY_SCHEMA_VERSION.to_string(),
        ],
    )?;
    if updated != 1 {
        bail!("legacy schema_version changed during projection");
    }
    binding_tx.commit()?;

    let page_size: u64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let page_count: u64 = conn.pragma_query_value(None, "page_count", |row| row.get(0))?;
    let freelist_count: u64 = conn.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    let live_pages = page_count
        .checked_sub(freelist_count)
        .ok_or_else(|| anyhow!("SQLite freelist exceeds page count"))?;
    let compact_upper_bound = live_pages
        .checked_mul(page_size)
        .ok_or_else(|| anyhow!("compacted database size estimate overflow"))?;
    let compact_margin = PROJECTION_SPACE_MARGIN_BYTES.max(compact_upper_bound / 10);
    let checked_compact_bound = compact_upper_bound
        .checked_add(compact_margin)
        .ok_or_else(|| anyhow!("database compaction space requirement overflow"))?;
    ensure_available_space(space_path, checked_compact_bound)?;
    let compact_path = path.with_file_name(".legal.db.compacting");
    if fs::symlink_metadata(&compact_path).is_ok() {
        bail!("fresh projection contains a conflicting database compaction file");
    }
    let compact_utf8 = compact_path
        .to_str()
        .ok_or_else(|| anyhow!("projection database path is not UTF-8"))?;
    conn.execute("VACUUM INTO ?1", [compact_utf8])?;
    drop(conn);
    let compact_metadata = fs::symlink_metadata(&compact_path)?;
    if compact_metadata.file_type().is_symlink()
        || !compact_metadata.is_file()
        || compact_metadata.len() == 0
        || compact_metadata.len() > MAX_PROJECTION_DB_BYTES
        || compact_metadata.len() > checked_compact_bound
    {
        bail!("compacted projection database is malformed or exceeds the size cap");
    }
    reject_hard_links(&compact_metadata, &compact_path)?;
    sync_file(&compact_path)?;
    replace_projected_database(&compact_path, path)?;
    sync_directory(
        path.parent()
            .ok_or_else(|| anyhow!("projected database has no parent"))?,
    )?;

    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch("PRAGMA cell_size_check=ON")?;
    crate::db::enforce_db_schema_version(&conn)?;
    crate::db::validate_chunks_fts_schema(&conn)?;
    let mut projected_manifest = legacy_manifest.clone();
    projected_manifest.schema_version = SUPPORTED_SCHEMA_VERSION;
    crate::source::verify_corpus_manifest_binding(&conn, &projected_manifest)?;
    verify_declared_counts(&conn)?;
    if crate::db::get_corpus_meta(&conn, "chunks_fts_index_sha256")?.as_deref()
        != Some(projected_fts_digest.as_str())
    {
        bail!("projected FTS digest metadata changed during database compaction");
    }
    if table_count(&conn, "embedding_cache")? != 0 {
        bail!("projected database retained disposable embedding_cache rows");
    }
    if table_count(&conn, "chunks")? != chunks
        || table_count(&conn, "chunk_embeddings")? != chunk_embeddings
    {
        bail!("VACUUM changed chunk or embedding rows");
    }
    crate::db::verify_fts_relational_bindings(&conn)?;
    verify_foreign_keys(&conn)?;
    verify_ordinary_integrity(&conn)?;
    verify_fts_integrity(&conn)?;
    drop(conn);
    remove_empty_sqlite_sidecars(path)?;
    sync_file(path)?;
    Ok(DatabaseProjection {
        chunks,
        chunk_embeddings,
    })
}

fn validate_projection_paths(source: &Path, output: &Path) -> Result<ProjectionPaths> {
    let source_metadata = fs::symlink_metadata(source)
        .with_context(|| format!("reading legacy generation path {}", source.display()))?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
        bail!("legacy generation path must be a real non-symlink directory");
    }
    let source_root = source
        .canonicalize()
        .with_context(|| format!("canonicalizing legacy generation {}", source.display()))?;

    match fs::symlink_metadata(output) {
        Ok(_) => bail!(
            "projection output must not already exist: {}",
            output.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let absolute_output = if output.is_absolute() {
        output.to_path_buf()
    } else {
        std::env::current_dir()?.join(output)
    };
    let output_name = absolute_output
        .file_name()
        .ok_or_else(|| anyhow!("projection output must name a new directory"))?
        .to_owned();
    let supplied_parent = absolute_output
        .parent()
        .ok_or_else(|| anyhow!("projection output has no parent directory"))?;
    let parent_metadata = fs::symlink_metadata(supplied_parent).with_context(|| {
        format!(
            "reading projection output parent {}",
            supplied_parent.display()
        )
    })?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        bail!("projection output parent must be a real existing directory");
    }
    let output_parent = supplied_parent.canonicalize()?;
    let output_root = output_parent.join(output_name);
    if output_root.starts_with(&source_root) || source_root.starts_with(&output_root) {
        bail!("legacy source and projection output must not contain one another");
    }
    if !same_filesystem(&source_root, &output_parent)? {
        bail!("legacy source and projection output must be on the same filesystem");
    }
    Ok(ProjectionPaths {
        source_root,
        output_parent,
        output_root,
    })
}

fn validate_legacy_generation(
    root: &Path,
    expected_generation: &GenerationId,
) -> Result<ValidatedLegacyGeneration> {
    let root_snapshot = snapshot_directory(root, "legacy generation")?;
    require_read_only(
        root,
        &fs::symlink_metadata(root)?,
        "legacy generation directory",
    )?;
    let expected_top = BTreeSet::from([
        crate::ann::ANN_DIRECTORY.to_string(),
        GENERATION_MANIFEST_FILENAME.to_string(),
        LEGAL_DB_FILENAME.to_string(),
        crate::semantic::EMBEDDING_MODEL_FILES[0]
            .output_name
            .to_string(),
        crate::semantic::EMBEDDING_MODEL_FILES[1]
            .output_name
            .to_string(),
    ]);
    let actual_top = directory_names(root)?;
    if actual_top != expected_top {
        bail!("legacy generation contents differ: expected {expected_top:?}, found {actual_top:?}");
    }

    let manifest_path = root.join(GENERATION_MANIFEST_FILENAME);
    let manifest_metadata = fs::symlink_metadata(&manifest_path)?;
    if manifest_metadata.len() > MAX_MANIFEST_BYTES {
        bail!("legacy generation manifest exceeds {MAX_MANIFEST_BYTES} bytes");
    }
    let manifest_bytes = read_validated_file(&manifest_path, &manifest_metadata)?;
    let manifest = decode_legacy_manifest(&manifest_bytes)?;
    crate::source::validate_manifest_contents(&manifest)?;
    let generation = crate::source::generation_key(&manifest)?;
    if &generation != expected_generation {
        bail!(
            "legacy manifest derives generation {generation}, not required generation {expected_generation}"
        );
    }
    if root.file_name().and_then(|name| name.to_str()) != Some(generation.as_str()) {
        bail!("legacy generation directory name must equal its typed generation ID");
    }

    validate_projection_db_size(manifest.db.size, "legacy")?;
    checked_generation_artifact_bytes(&manifest)?;
    let manifest_snapshot = snapshot_artifact(
        root,
        GENERATION_MANIFEST_FILENAME,
        manifest_metadata.len(),
        &sha256_bytes(&manifest_bytes),
    )?;
    let mut artifacts = BTreeMap::new();
    artifacts.insert(
        LEGAL_DB_FILENAME.to_string(),
        snapshot_artifact(
            root,
            &manifest.db.path,
            manifest.db.size,
            &manifest.db.sha256,
        )?,
    );
    for file in [&manifest.model.model, &manifest.model.tokenizer] {
        artifacts.insert(
            file.path.clone(),
            snapshot_artifact(root, &file.path, file.size, &file.sha256)?,
        );
    }

    let ann_dir = root.join(crate::ann::ANN_DIRECTORY);
    let ann_snapshot = snapshot_directory(&ann_dir, "legacy ANN directory")?;
    require_read_only(
        &ann_dir,
        &fs::symlink_metadata(&ann_dir)?,
        "legacy ANN directory",
    )?;
    let expected_ann = manifest
        .ann
        .values()
        .map(|info| {
            Path::new(&info.path)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("legacy ANN path is malformed"))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    if directory_names(&ann_dir)? != expected_ann {
        bail!("legacy ANN directory does not exactly match generation.json");
    }
    for (source_id, info) in &manifest.ann {
        let snapshot = snapshot_artifact(root, &info.path, info.size, &info.sha256)?;
        crate::ann::verify_sidecar(&root.join(&info.path), source_id, info)?;
        artifacts.insert(info.path.clone(), snapshot);
    }

    let db_path = root.join(&manifest.db.path);
    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(60))?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.execute_batch("PRAGMA query_only=ON; PRAGMA cell_size_check=ON;")?;
    enforce_legacy_schema(&conn)?;
    validate_legacy_chunks_fts_schema(&conn)?;
    crate::source::verify_corpus_manifest_binding(&conn, &manifest)?;
    verify_declared_counts(&conn)?;
    crate::source::verify_semantic_install(&conn, &manifest)?;
    crate::db::verify_fts_relational_bindings(&conn)?;
    verify_foreign_keys(&conn)?;
    verify_ordinary_integrity(&conn)?;
    let chunks = table_count(&conn, "chunks")?;
    let chunk_embeddings = table_count(&conn, "chunk_embeddings")?;
    let embedding_cache_rows = table_count(&conn, "embedding_cache")?;
    if chunks == 0 || chunks != chunk_embeddings {
        bail!("legacy generation does not have one embedding for every chunk");
    }
    drop(conn);

    Ok(ValidatedLegacyGeneration {
        root: root.to_path_buf(),
        root_snapshot,
        ann_snapshot,
        manifest,
        manifest_snapshot,
        generation,
        artifacts,
        chunks,
        chunk_embeddings,
        embedding_cache_rows,
    })
}

fn decode_legacy_manifest(bytes: &[u8]) -> Result<Manifest> {
    let manifest: Manifest = serde_json::from_slice(bytes).context("parsing legacy manifest")?;
    if manifest.schema_version != LEGACY_SCHEMA_VERSION {
        bail!(
            "derivation source must use exactly legacy schema {LEGACY_SCHEMA_VERSION}, got {}",
            manifest.schema_version
        );
    }
    Ok(manifest)
}

fn enforce_legacy_schema(conn: &Connection) -> Result<()> {
    let value = crate::db::get_corpus_meta(conn, "schema_version")?
        .ok_or_else(|| anyhow!("legacy database is missing corpus_meta.schema_version"))?;
    let parsed = value
        .parse::<u32>()
        .with_context(|| format!("legacy schema_version `{value}` is malformed"))?;
    if parsed != LEGACY_SCHEMA_VERSION {
        bail!(
            "derivation source database must use exactly legacy schema {LEGACY_SCHEMA_VERSION}, got {parsed}"
        );
    }
    Ok(())
}

fn validate_legacy_chunks_fts_schema(conn: &Connection) -> Result<()> {
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'chunks_fts'",
            [],
            |row| row.get(0),
        )
        .context("reading legacy chunks_fts schema")?;
    if crate::db::normalized_sql(&sql) != crate::db::normalized_sql(LEGACY_CHUNKS_FTS_SQL) {
        bail!("legacy chunks_fts does not match the exact schema-10 FTS contract");
    }
    Ok(())
}

fn verify_declared_counts(conn: &Connection) -> Result<()> {
    for (key, table) in [
        ("documents_count", "documents"),
        ("chunks_count", "chunks"),
        ("chunk_embeddings_count", "chunk_embeddings"),
        ("definitions_count", "definitions"),
    ] {
        let declared = crate::db::get_corpus_meta(conn, key)?
            .ok_or_else(|| anyhow!("database is missing corpus_meta.{key}"))?
            .parse::<u64>()
            .with_context(|| format!("corpus_meta.{key} is malformed"))?;
        let actual = table_count(conn, table)?;
        if declared != actual {
            bail!("corpus_meta.{key} declares {declared}, but {table} contains {actual} rows");
        }
    }
    Ok(())
}

fn verify_foreign_keys(conn: &Connection) -> Result<()> {
    let violations: i64 =
        conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations != 0 {
        bail!("generation database has {violations} foreign-key violations");
    }
    Ok(())
}

fn verify_ordinary_integrity(conn: &Connection) -> Result<()> {
    let tables = {
        let mut statement = conn.prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table'
               AND name NOT LIKE 'chunks_fts%'
               AND name NOT LIKE 'title_fts%'
             ORDER BY name",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if tables.is_empty() {
        bail!("generation database has no ordinary tables to validate");
    }
    for table in tables {
        let escaped = table.replace('\'', "''");
        let sql = format!("PRAGMA integrity_check('{escaped}')");
        let values = {
            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if values.as_slice() != ["ok"] {
            bail!("database table `{table}` failed SQLite integrity_check: {values:?}");
        }
    }
    Ok(())
}

fn verify_fts_integrity(conn: &Connection) -> Result<()> {
    let transaction = conn.unchecked_transaction()?;
    transaction.execute(
        "INSERT INTO title_fts(title_fts) VALUES('integrity-check')",
        [],
    )?;
    transaction.execute(
        "INSERT INTO chunks_fts(chunks_fts) VALUES('integrity-check')",
        [],
    )?;
    transaction.rollback()?;
    Ok(())
}

fn table_count(conn: &Connection, table: &str) -> Result<u64> {
    let count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })?;
    u64::try_from(count).with_context(|| format!("{table} row count is negative"))
}

fn ordered_rowids_match(conn: &Connection, left: &str, right: &str) -> Result<bool> {
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

fn immutable_artifact_paths(manifest: &Manifest) -> Vec<String> {
    let mut paths = vec![
        manifest.model.model.path.clone(),
        manifest.model.tokenizer.path.clone(),
    ];
    paths.extend(manifest.ann.values().map(|info| info.path.clone()));
    paths
}

fn snapshot_artifact(
    root: &Path,
    relative: &str,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<ArtifactSnapshot> {
    let path = root.join(relative);
    let metadata = validate_regular_file(&path, expected_size, expected_sha256, true)?;
    #[cfg(windows)]
    let _ = &metadata;
    let snapshot = ArtifactSnapshot {
        relative_path: relative.to_string(),
        size: expected_size,
        sha256: expected_sha256.to_string(),
        #[cfg(unix)]
        device: unix_device(&metadata),
        #[cfg(unix)]
        inode: unix_inode(&metadata),
        #[cfg(windows)]
        windows_identity: windows_identity_from_path(&path, false)?,
    };
    #[cfg(not(any(unix, windows)))]
    drop(metadata);
    Ok(snapshot)
}

fn validate_regular_file(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
    require_immutable: bool,
) -> Result<fs::Metadata> {
    let mut file = open_regular_no_follow(path)?;
    let metadata = file.metadata()?;
    reject_hard_links(&metadata, path)?;
    if require_immutable {
        require_read_only(path, &metadata, "legacy generation file")?;
    }
    if metadata.len() != expected_size {
        bail!("generation file size mismatch for {}", path.display());
    }
    let actual = sha256_file(&mut file)?;
    if actual != expected_sha256 {
        bail!("generation file SHA-256 mismatch for {}", path.display());
    }
    Ok(metadata)
}

fn read_validated_file(path: &Path, path_metadata: &fs::Metadata) -> Result<Vec<u8>> {
    #[cfg(windows)]
    let (path_identity, _) = (windows_identity_from_path(path, false)?, path_metadata);
    let mut file = open_regular_no_follow(path)?;
    let metadata = file.metadata()?;
    reject_hard_links(&metadata, path)?;
    require_read_only(path, &metadata, "legacy generation file")?;
    #[cfg(not(windows))]
    let identity_matches = same_file_identity(path_metadata, &metadata);
    #[cfg(windows)]
    let identity_matches = windows_identity_from_file(&file)? == path_identity;
    if !identity_matches {
        bail!(
            "generation file changed while being opened: {}",
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() {
        bail!(
            "generation file changed while being read: {}",
            path.display()
        );
    }
    Ok(bytes)
}

fn snapshot_directory(path: &Path, label: &str) -> Result<DirectorySnapshot> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{label} must be a real non-symlink directory");
    }
    Ok(DirectorySnapshot {
        #[cfg(unix)]
        device: unix_device(&metadata),
        #[cfg(unix)]
        inode: unix_inode(&metadata),
        #[cfg(windows)]
        windows_identity: windows_identity_from_path(path, true)?,
    })
}

fn revalidate_legacy_generation(legacy: &ValidatedLegacyGeneration) -> Result<()> {
    let current_root = snapshot_directory(&legacy.root, "legacy generation")?;
    if !same_directory_identity(&legacy.root_snapshot, &current_root) {
        bail!("legacy generation directory changed during projection");
    }
    let root_metadata = fs::symlink_metadata(&legacy.root)?;
    require_read_only(&legacy.root, &root_metadata, "legacy generation directory")?;
    let expected_top = BTreeSet::from([
        crate::ann::ANN_DIRECTORY.to_string(),
        GENERATION_MANIFEST_FILENAME.to_string(),
        LEGAL_DB_FILENAME.to_string(),
        crate::semantic::EMBEDDING_MODEL_FILES[0]
            .output_name
            .to_string(),
        crate::semantic::EMBEDDING_MODEL_FILES[1]
            .output_name
            .to_string(),
    ]);
    if directory_names(&legacy.root)? != expected_top {
        bail!("legacy generation entries changed during projection");
    }
    let ann_dir = legacy.root.join(crate::ann::ANN_DIRECTORY);
    let current_ann = snapshot_directory(&ann_dir, "legacy ANN directory")?;
    if !same_directory_identity(&legacy.ann_snapshot, &current_ann) {
        bail!("legacy ANN directory changed during projection");
    }
    let ann_metadata = fs::symlink_metadata(&ann_dir)?;
    require_read_only(&ann_dir, &ann_metadata, "legacy ANN directory")?;
    let expected_ann = legacy
        .manifest
        .ann
        .values()
        .map(|info| {
            Path::new(&info.path)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("legacy ANN path is malformed"))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    if directory_names(&ann_dir)? != expected_ann {
        bail!("legacy ANN entries changed during projection");
    }
    revalidate_snapshot(&legacy.root, &legacy.manifest_snapshot)?;
    for snapshot in legacy.artifacts.values() {
        revalidate_snapshot(&legacy.root, snapshot)?;
    }
    Ok(())
}

fn revalidate_snapshot(root: &Path, snapshot: &ArtifactSnapshot) -> Result<()> {
    let path = root.join(&snapshot.relative_path);
    let metadata = validate_regular_file(&path, snapshot.size, &snapshot.sha256, true)?;
    #[cfg(windows)]
    let _ = &metadata;
    #[cfg(unix)]
    if unix_device(&metadata) != snapshot.device || unix_inode(&metadata) != snapshot.inode {
        bail!(
            "legacy artifact was replaced during projection: {}",
            path.display()
        );
    }
    #[cfg(windows)]
    if windows_identity_from_path(&path, false)? != snapshot.windows_identity {
        bail!(
            "legacy artifact was replaced during projection: {}",
            path.display()
        );
    }
    #[cfg(not(any(unix, windows)))]
    drop(metadata);
    Ok(())
}

fn clone_or_copy_validated(
    source_root: &Path,
    snapshot: &ArtifactSnapshot,
    destination: &Path,
) -> Result<CopyMethod> {
    let source_path = source_root.join(&snapshot.relative_path);
    let mut source = open_regular_no_follow(&source_path)?;
    let source_metadata = source.metadata()?;
    reject_hard_links(&source_metadata, &source_path)?;
    if source_metadata.len() != snapshot.size {
        bail!(
            "legacy artifact changed before copy: {}",
            source_path.display()
        );
    }
    #[cfg(unix)]
    if unix_device(&source_metadata) != snapshot.device
        || unix_inode(&source_metadata) != snapshot.inode
    {
        bail!(
            "legacy artifact was replaced before copy: {}",
            source_path.display()
        );
    }
    #[cfg(windows)]
    if windows_identity_from_file(&source)? != snapshot.windows_identity {
        bail!(
            "legacy artifact was replaced before copy: {}",
            source_path.display()
        );
    }

    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("artifact destination has no parent"))?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let mut output = options
        .open(destination)
        .with_context(|| format!("creating projected artifact {}", destination.display()))?;
    let method = match try_reflink(&source, &output) {
        Ok(()) => CopyMethod::Reflink,
        Err(error) if reflink_is_unsupported(&error) => {
            ensure_available_space(
                parent,
                snapshot
                    .size
                    .checked_add(PROJECTION_SPACE_MARGIN_BYTES)
                    .ok_or_else(|| anyhow!("artifact copy space requirement overflow"))?,
            )?;
            output.set_len(0)?;
            output.seek(SeekFrom::Start(0))?;
            source.seek(SeekFrom::Start(0))?;
            let copied = io::copy(&mut source, &mut output)?;
            if copied != snapshot.size {
                bail!(
                    "artifact copy wrote {copied} bytes; expected {} for {}",
                    snapshot.size,
                    destination.display()
                );
            }
            CopyMethod::Copy
        }
        Err(error) => return Err(error).context("copy-on-write clone failed"),
    };
    output.sync_all()?;
    drop(output);

    let destination_metadata =
        validate_regular_file(destination, snapshot.size, &snapshot.sha256, false)?;
    #[cfg(windows)]
    let _ = &destination_metadata;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if destination_metadata.nlink() != 1
            || (destination_metadata.dev() == source_metadata.dev()
                && destination_metadata.ino() == source_metadata.ino())
        {
            bail!("projected artifacts must be distinct single-link inodes");
        }
    }
    #[cfg(windows)]
    {
        let destination_identity = windows_identity_from_path(destination, false)?;
        if destination_identity.number_of_links != 1
            || (destination_identity.volume_serial_number
                == snapshot.windows_identity.volume_serial_number
                && destination_identity.file_index == snapshot.windows_identity.file_index)
        {
            bail!("projected artifacts must be distinct single-link files");
        }
    }
    #[cfg(not(any(unix, windows)))]
    drop(destination_metadata);
    sync_directory(parent)?;
    Ok(method)
}

fn open_regular_no_follow(path: &Path) -> Result<File> {
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading generation file {}", path.display()))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        bail!(
            "generation artifact must be a regular non-symlink file: {}",
            path.display()
        );
    }
    #[cfg(windows)]
    let path_identity = windows_identity_from_path(path, false)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    #[cfg(not(windows))]
    let identity_matches = same_file_identity(&path_metadata, &file.metadata()?);
    #[cfg(windows)]
    let identity_matches = windows_identity_from_file(&file)? == path_identity;
    if !identity_matches {
        bail!(
            "generation artifact changed while being opened: {}",
            path.display()
        );
    }
    Ok(file)
}

fn directory_names(path: &Path) -> Result<BTreeSet<String>> {
    fs::read_dir(path)?
        .map(|entry| {
            entry?
                .file_name()
                .into_string()
                .map_err(|_| anyhow!("generation contains a non-Unicode path"))
        })
        .collect()
}

fn required_projection_space(manifest: &Manifest, reflink_supported: bool) -> Result<u64> {
    let database_copy_bytes = if reflink_supported {
        0
    } else {
        manifest.db.size
    };
    manifest
        .db
        .size
        // The FTS rebuild is bounded by the complete legacy DB size. Immutable
        // files are copied later with a fresh per-file space check.
        .checked_add(database_copy_bytes)
        .and_then(|value| value.checked_add(PROJECTION_SPACE_MARGIN_BYTES))
        .ok_or_else(|| anyhow!("projection space requirement overflow"))
}

fn validate_projection_db_size(size: u64, label: &str) -> Result<()> {
    if size == 0 || size > MAX_PROJECTION_DB_BYTES {
        bail!(
            "{label} database size {size} is outside the projection range 1..={MAX_PROJECTION_DB_BYTES}"
        );
    }
    Ok(())
}

fn checked_generation_artifact_bytes(manifest: &Manifest) -> Result<u64> {
    manifest
        .db
        .size
        .checked_add(manifest.model.model.size)
        .and_then(|value| value.checked_add(manifest.model.tokenizer.size))
        .and_then(|value| {
            manifest
                .ann
                .values()
                .try_fold(value, |total, info| total.checked_add(info.size))
        })
        .ok_or_else(|| anyhow!("generation artifact sizes overflow u64"))
}

fn probe_reflink_support(parent: &Path) -> Result<bool> {
    let mut source = tempfile::Builder::new()
        .prefix(".legal-mcp-reflink-source-")
        .tempfile_in(parent)?;
    source.write_all(b"australian-legal-mcp-reflink-probe")?;
    source.as_file().sync_all()?;
    let destination = tempfile::Builder::new()
        .prefix(".legal-mcp-reflink-destination-")
        .tempfile_in(parent)?;
    match try_reflink(source.as_file(), destination.as_file()) {
        Ok(()) => Ok(true),
        Err(error) if reflink_is_unsupported(&error) => Ok(false),
        Err(error) => Err(error).context("probing projection filesystem reflink support"),
    }
}

fn ensure_available_space(path: &Path, required: u64) -> Result<()> {
    let available = fs2::available_space(path)?;
    if available < required {
        bail!(
            "projection requires at least {required} free bytes on {}, but only {available} are available",
            path.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn try_reflink(source: &File, destination: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let result = unsafe { libc::ioctl(destination.as_raw_fd(), FICLONE, source.as_raw_fd()) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn try_reflink(_source: &File, _destination: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "reflink is not implemented on this platform",
    ))
}

fn reflink_is_unsupported(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    return error.raw_os_error().is_some_and(|code| {
        matches!(
            code,
            libc::EOPNOTSUPP | libc::ENOTTY | libc::EINVAL | libc::EXDEV | libc::ENOSYS
        )
    });
    #[cfg(not(unix))]
    false
}

#[cfg(unix)]
fn same_filesystem(left: &Path, right: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(fs::metadata(left)?.dev() == fs::metadata(right)?.dev())
}

#[cfg(windows)]
fn same_filesystem(left: &Path, right: &Path) -> Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    extern "system" {
        fn GetVolumePathNameW(
            file_name: *const u16,
            volume_path_name: *mut u16,
            buffer_length: u32,
        ) -> i32;
    }
    fn volume(path: &Path) -> Result<String> {
        let path = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut output = vec![0_u16; 32_768];
        let success =
            unsafe { GetVolumePathNameW(path.as_ptr(), output.as_mut_ptr(), output.len() as u32) };
        if success == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let length = output
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(output.len());
        Ok(String::from_utf16(&output[..length])?.to_ascii_lowercase())
    }
    Ok(volume(left)? == volume(right)?)
}

#[cfg(unix)]
fn reject_hard_links(metadata: &fs::Metadata, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() != 1 {
        bail!(
            "generation artifacts must have exactly one link: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
#[cfg(not(windows))]
fn reject_hard_links(_metadata: &fs::Metadata, _path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn reject_hard_links(_metadata: &fs::Metadata, path: &Path) -> Result<()> {
    if windows_identity_from_path(path, false)?.number_of_links != 1 {
        bail!(
            "generation artifacts must have exactly one link: {}",
            path.display()
        );
    }
    Ok(())
}

fn require_read_only(path: &Path, metadata: &fs::Metadata, label: &str) -> Result<()> {
    #[cfg(unix)]
    if !metadata.permissions().readonly() {
        bail!("{label} must be immutable/read-only: {}", path.display());
    }
    #[cfg(not(unix))]
    let _ = (path, metadata, label);
    Ok(())
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
#[cfg(not(windows))]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len()
}

#[cfg(unix)]
fn unix_device(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

#[cfg(unix)]
fn unix_inode(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ino()
}

#[cfg(unix)]
fn same_directory_identity(left: &DirectorySnapshot, right: &DirectorySnapshot) -> bool {
    left.device == right.device && left.inode == right.inode
}

#[cfg(not(unix))]
#[cfg(not(windows))]
fn same_directory_identity(_left: &DirectorySnapshot, _right: &DirectorySnapshot) -> bool {
    true
}

#[cfg(windows)]
fn same_directory_identity(left: &DirectorySnapshot, right: &DirectorySnapshot) -> bool {
    left.windows_identity == right.windows_identity
}

#[cfg(windows)]
fn windows_identity_from_path(path: &Path, directory: bool) -> Result<WindowsFileIdentity> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    let mut options = OpenOptions::new();
    options.read(true).custom_flags(
        FILE_FLAG_OPEN_REPARSE_POINT
            | if directory {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                0
            },
    );
    let file = options
        .open(path)
        .with_context(|| format!("opening Windows identity handle for {}", path.display()))?;
    let identity = windows_identity_from_file(&file)?;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    if identity.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (identity.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
    {
        bail!(
            "Windows generation path changed type while being opened: {}",
            path.display()
        );
    }
    Ok(identity)
}

#[cfg(windows)]
fn windows_identity_from_file(file: &File) -> Result<WindowsFileIdentity> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let success =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if success == 0 {
        return Err(io::Error::last_os_error()).context("reading Windows file identity");
    }
    let information = unsafe { information.assume_init() };
    Ok(WindowsFileIdentity {
        file_attributes: information.dwFileAttributes,
        volume_serial_number: information.dwVolumeSerialNumber,
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
        number_of_links: information.nNumberOfLinks,
    })
}

fn sha256_file(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256_path(path: &Path) -> Result<String> {
    let mut file = open_regular_no_follow(path)?;
    sha256_file(&mut file)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sync_file(path: &Path) -> Result<()> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?
        .sync_all()?;
    Ok(())
}

#[cfg(not(windows))]
fn replace_projected_database(compact: &Path, destination: &Path) -> Result<()> {
    fs::rename(compact, destination)?;
    Ok(())
}

#[cfg(windows)]
fn replace_projected_database(compact: &Path, destination: &Path) -> Result<()> {
    // The directory has no manifest yet, so interruption cannot make either
    // file activatable. Windows rename does not replace an existing file.
    fs::remove_file(destination)?;
    fs::rename(compact, destination)?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn remove_empty_sqlite_sidecars(database: &Path) -> Result<()> {
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut name = database.as_os_str().to_os_string();
        name.push(suffix);
        let path = PathBuf::from(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() != 0 {
                    bail!(
                        "projected database retained SQLite sidecar {}",
                        path.display()
                    );
                }
                fs::remove_file(&path)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    sync_directory(
        database
            .parent()
            .ok_or_else(|| anyhow!("database has no parent directory"))?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{ManifestFile, ModelInfo};
    use rusqlite::params;

    fn fixture_manifest(schema_version: u32) -> Manifest {
        Manifest {
            schema_version,
            index_version: "fixture-v19".to_string(),
            created_at: "2026-07-14T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: crate::EMBEDDING_MODEL_ID.to_string(),
                fingerprint: crate::EMBEDDING_MODEL_FINGERPRINT.to_string(),
                model: ManifestFile {
                    path: "model.onnx".to_string(),
                    sha256: "1".repeat(64),
                    size: 3,
                },
                tokenizer: ManifestFile {
                    path: "tokenizer.json".to_string(),
                    sha256: "2".repeat(64),
                    size: 5,
                },
            },
            db: ManifestDb {
                path: LEGAL_DB_FILENAME.to_string(),
                sha256: "3".repeat(64),
                size: 8,
            },
            ann: BTreeMap::new(),
        }
    }

    fn create_legacy_database(path: &Path) -> Result<Manifest> {
        let manifest = fixture_manifest(LEGACY_SCHEMA_VERSION);
        let conn = Connection::open(path)?;
        conn.execute_batch(&format!(
            "CREATE TABLE corpus_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE sources(source_id TEXT PRIMARY KEY, display_name TEXT NOT NULL);
             CREATE TABLE documents(
                 source_id TEXT NOT NULL,
                 native_id TEXT NOT NULL,
                 title TEXT NOT NULL,
                 PRIMARY KEY(source_id, native_id)
             );
             CREATE TABLE chunks(
                 chunk_id INTEGER PRIMARY KEY,
                 source_id TEXT NOT NULL,
                 native_id TEXT NOT NULL,
                 ord INTEGER NOT NULL,
                 text BLOB NOT NULL
             );
             CREATE TABLE definitions(definition_id TEXT PRIMARY KEY);
             CREATE TABLE embedding_cache(
                 model_id TEXT NOT NULL,
                 text_sha256 TEXT NOT NULL,
                 embedding BLOB NOT NULL,
                 PRIMARY KEY(model_id, text_sha256)
             ) WITHOUT ROWID;
             CREATE TABLE chunk_embeddings(
                 chunk_id INTEGER PRIMARY KEY,
                 embedding BLOB NOT NULL CHECK(length(embedding) = 256)
             );
             CREATE VIRTUAL TABLE title_fts USING fts5(
                 source_id UNINDEXED,
                 native_id UNINDEXED,
                 title,
                 headings,
                 tokenize = \"porter unicode61 remove_diacritics 2\"
             );
             {LEGACY_CHUNKS_FTS_SQL};"
        ))?;
        for (key, value) in [
            ("schema_version", LEGACY_SCHEMA_VERSION.to_string()),
            ("index_version", manifest.index_version.clone()),
            ("embedding_model_id", manifest.model.id.clone()),
            ("last_update_at", manifest.created_at.clone()),
            ("documents_count", "3".to_string()),
            ("chunks_count", "4".to_string()),
            ("chunk_embeddings_count", "4".to_string()),
            ("definitions_count", "1".to_string()),
        ] {
            conn.execute(
                "INSERT INTO corpus_meta(key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
        }
        conn.execute_batch(
            "INSERT INTO sources(source_id, display_name) VALUES ('ato', 'ATO');
             INSERT INTO documents(source_id, native_id, title) VALUES
                 ('ato', 'best', 'Best result'),
                 ('ato', 'common-a', 'Common A'),
                 ('ato', 'common-b', 'Common B');
             INSERT INTO chunks(chunk_id, source_id, native_id, ord, text) VALUES
                 (7, 'ato', 'best', 0, X'0102'),
                 (41, 'ato', 'best', 1, X'0304'),
                 (90, 'ato', 'common-a', 0, X'0506'),
                 (105, 'ato', 'common-b', 0, X'0708');
             INSERT INTO definitions(definition_id) VALUES ('definition');
             INSERT INTO title_fts(rowid, source_id, native_id, title, headings) VALUES
                 (1, 'ato', 'best', 'Best result', ''),
                 (2, 'ato', 'common-a', 'Common A', ''),
                 (3, 'ato', 'common-b', 'Common B', '');
             INSERT INTO chunks_fts(rowid, text) VALUES
                 (7, 'common distinctive distinctive evidence'),
                 (41, 'taxation research development incentive'),
                 (90, 'common evidence'),
                 (105, 'common');",
        )?;
        for chunk_id in [7_i64, 41, 90, 105] {
            let vector = (0..crate::EMBEDDING_DIM)
                .map(|dimension| (chunk_id as usize + dimension) as u8)
                .collect::<Vec<_>>();
            conn.execute(
                "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (?1, ?2)",
                params![chunk_id, vector],
            )?;
        }
        conn.execute(
            "INSERT INTO embedding_cache(model_id, text_sha256, embedding)
             VALUES (?1, ?2, ?3)",
            params![crate::EMBEDDING_MODEL_ID, "a".repeat(64), vec![9_u8; 256]],
        )?;
        drop(conn);
        Ok(manifest)
    }

    fn bm25_results(conn: &Connection, query: &str) -> Result<Vec<(i64, f64)>> {
        let mut statement = conn.prepare(
            "SELECT rowid, bm25(chunks_fts)
             FROM chunks_fts
             WHERE chunks_fts MATCH ?1
             ORDER BY bm25(chunks_fts), rowid",
        )?;
        let rows = statement.query_map([query], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn embedding_rows(conn: &Connection) -> Result<Vec<(i64, Vec<u8>)>> {
        let mut statement =
            conn.prepare("SELECT chunk_id, embedding FROM chunk_embeddings ORDER BY chunk_id")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn preserved_logical_rows(conn: &Connection) -> Result<Vec<String>> {
        let mut rows = Vec::new();
        for sql in [
            "SELECT 'source|' || source_id || '|' || display_name FROM sources ORDER BY source_id",
            "SELECT 'document|' || source_id || '|' || native_id || '|' || title
             FROM documents ORDER BY source_id, native_id",
            "SELECT 'chunk|' || chunk_id || '|' || source_id || '|' || native_id || '|' || ord || '|' || hex(text)
             FROM chunks ORDER BY chunk_id",
            "SELECT 'definition|' || definition_id FROM definitions ORDER BY definition_id",
            "SELECT 'title|' || rowid || '|' || source_id || '|' || native_id || '|' || title || '|' || headings
             FROM title_fts ORDER BY rowid",
        ] {
            let mut statement = conn.prepare(sql)?;
            let values = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.extend(values.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(rows)
    }

    #[test]
    fn database_projection_preserves_keyword_bm25_ids_and_embedding_bytes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("legacy.db");
        let projected = root.path().join("projected.db");
        let projected_again = root.path().join("projected-again.db");
        let manifest = create_legacy_database(&source)?;
        fs::copy(&source, &projected)?;
        fs::copy(&source, &projected_again)?;
        let source_hash = sha256_path(&source)?;
        let source_conn = Connection::open(&source)?;
        let queries = ["common distinctive", "research development", "evidence"];
        let before = queries
            .iter()
            .map(|query| bm25_results(&source_conn, query))
            .collect::<Result<Vec<_>>>()?;
        let embeddings_before = embedding_rows(&source_conn)?;
        let logical_rows_before = preserved_logical_rows(&source_conn)?;
        drop(source_conn);

        let report = project_database(&projected, &manifest, root.path())?;
        assert_eq!(report.chunks, 4);
        assert_eq!(report.chunk_embeddings, 4);
        project_database(&projected_again, &manifest, root.path())?;
        assert_eq!(
            sha256_path(&projected)?,
            sha256_path(&projected_again)?,
            "the same schema-10 bytes must project deterministically"
        );

        let projected_conn = Connection::open(&projected)?;
        for (query, expected) in queries.iter().zip(&before) {
            let actual = bm25_results(&projected_conn, query)?;
            assert_eq!(actual.len(), expected.len(), "query `{query}`");
            for ((actual_id, actual_score), (expected_id, expected_score)) in
                actual.iter().zip(expected)
            {
                assert_eq!(actual_id, expected_id, "query `{query}`");
                assert!(
                    (actual_score - expected_score).abs() <= f64::EPSILON,
                    "BM25 changed for query `{query}`: {actual_score} != {expected_score}"
                );
            }
        }
        assert_eq!(embedding_rows(&projected_conn)?, embeddings_before);
        assert_eq!(
            preserved_logical_rows(&projected_conn)?,
            logical_rows_before
        );
        assert_eq!(table_count(&projected_conn, "embedding_cache")?, 0);
        assert_eq!(
            crate::db::get_corpus_meta(&projected_conn, "schema_version")?.as_deref(),
            Some("11")
        );
        crate::db::validate_chunks_fts_schema(&projected_conn)?;
        crate::db::verify_chunks_fts_index_digest(&projected_conn)?;
        let content: Option<String> =
            projected_conn.query_row("SELECT text FROM chunks_fts WHERE rowid = 7", [], |row| {
                row.get(0)
            })?;
        assert_eq!(content, None);
        projected_conn.execute("DELETE FROM chunks_fts WHERE rowid = 105", [])?;
        assert_eq!(table_count(&projected_conn, "chunks_fts")?, 3);
        drop(projected_conn);

        assert_eq!(sha256_path(&source)?, source_hash);
        let source_conn = Connection::open(&source)?;
        enforce_legacy_schema(&source_conn)?;
        validate_legacy_chunks_fts_schema(&source_conn)?;
        assert_eq!(table_count(&source_conn, "embedding_cache")?, 1);
        assert_eq!(embedding_rows(&source_conn)?, embeddings_before);
        let source_text: String =
            source_conn.query_row("SELECT text FROM chunks_fts WHERE rowid = 7", [], |row| {
                row.get(0)
            })?;
        assert_eq!(source_text, "common distinctive distinctive evidence");
        Ok(())
    }

    #[test]
    fn legacy_reader_recognizes_exactly_schema10_and_denies_unknown_fields() -> Result<()> {
        let manifest = fixture_manifest(LEGACY_SCHEMA_VERSION);
        assert_eq!(
            decode_legacy_manifest(&serde_json::to_vec(&manifest)?)?.schema_version,
            LEGACY_SCHEMA_VERSION
        );
        for schema in [9, SUPPORTED_SCHEMA_VERSION] {
            let mut other = manifest.clone();
            other.schema_version = schema;
            assert!(decode_legacy_manifest(&serde_json::to_vec(&other)?).is_err());
        }
        let mut value = serde_json::to_value(&manifest)?;
        value["unexpected"] = serde_json::Value::Bool(true);
        assert!(decode_legacy_manifest(&serde_json::to_vec(&value)?).is_err());
        value = serde_json::to_value(&manifest)?;
        value["db"]["unexpected"] = serde_json::Value::Bool(true);
        assert!(decode_legacy_manifest(&serde_json::to_vec(&value)?).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn fixture_ann_copies_are_distinct_single_link_inodes_without_source_mutation() -> Result<()> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let output = root.path().join("output");
        fs::create_dir_all(source.join("ann"))?;
        fs::create_dir(&output)?;
        fs::create_dir(output.join("ann"))?;
        let mut snapshots = Vec::new();
        for index in 0..10 {
            let relative = format!("ann/source-{index}.ann");
            let path = source.join(&relative);
            let bytes = format!("ann fixture {index}\0immutable bytes");
            fs::write(&path, bytes.as_bytes())?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
            snapshots.push(snapshot_artifact(
                &source,
                &relative,
                bytes.len() as u64,
                &sha256_bytes(bytes.as_bytes()),
            )?);
        }
        let source_before = snapshots
            .iter()
            .map(|snapshot| {
                let metadata = fs::metadata(source.join(&snapshot.relative_path))?;
                Ok((
                    snapshot.relative_path.clone(),
                    metadata.dev(),
                    metadata.ino(),
                    sha256_path(&source.join(&snapshot.relative_path))?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;

        for snapshot in &snapshots {
            clone_or_copy_validated(&source, snapshot, &output.join(&snapshot.relative_path))?;
        }
        for (relative, source_device, source_inode, source_hash) in source_before {
            let source_metadata = fs::metadata(source.join(&relative))?;
            let output_metadata = fs::metadata(output.join(&relative))?;
            assert_eq!(source_metadata.dev(), source_device);
            assert_eq!(source_metadata.ino(), source_inode);
            assert_eq!(source_metadata.nlink(), 1);
            assert_eq!(output_metadata.nlink(), 1);
            assert_ne!(
                (output_metadata.dev(), output_metadata.ino()),
                (source_device, source_inode)
            );
            assert_eq!(sha256_path(&source.join(&relative))?, source_hash);
            assert_eq!(sha256_path(&output.join(&relative))?, source_hash);
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn legacy_artifact_snapshots_reject_hard_links_and_symlinks() -> Result<()> {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = tempfile::tempdir()?;
        let artifact = root.path().join("artifact");
        fs::write(&artifact, b"immutable")?;
        fs::set_permissions(&artifact, fs::Permissions::from_mode(0o444))?;
        fs::hard_link(&artifact, root.path().join("hard-link"))?;
        assert!(
            snapshot_artifact(root.path(), "artifact", 9, &sha256_bytes(b"immutable")).is_err()
        );

        let target = root.path().join("target");
        fs::write(&target, b"target")?;
        symlink(&target, root.path().join("symlink"))?;
        assert!(snapshot_artifact(root.path(), "symlink", 6, &sha256_bytes(b"target")).is_err());
        Ok(())
    }

    #[test]
    fn projection_paths_require_fresh_non_nested_output() -> Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let sibling = root.path().join("candidate");
        fs::create_dir(&source)?;
        let paths = validate_projection_paths(&source, &sibling)?;
        assert_eq!(
            paths.output_root,
            root.path().canonicalize()?.join("candidate")
        );

        fs::create_dir(&sibling)?;
        assert!(validate_projection_paths(&source, &sibling).is_err());
        fs::remove_dir(&sibling)?;
        fs::create_dir(source.join("builds"))?;
        assert!(validate_projection_paths(&source, &source.join("builds/candidate")).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn projection_paths_reject_symlink_roots_and_parents() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let real_parent = root.path().join("real-parent");
        fs::create_dir(&source)?;
        fs::create_dir(&real_parent)?;
        let source_link = root.path().join("source-link");
        symlink(&source, &source_link)?;
        assert!(validate_projection_paths(&source_link, &root.path().join("candidate")).is_err());
        let parent_link = root.path().join("parent-link");
        symlink(&real_parent, &parent_link)?;
        assert!(validate_projection_paths(&source, &parent_link.join("candidate")).is_err());
        Ok(())
    }

    #[test]
    fn incomplete_output_is_removed_without_a_manifest() -> Result<()> {
        let root = tempfile::tempdir()?;
        let output = root.path().join("candidate");
        {
            let _guard = FreshOutput::claim(&output)?;
            fs::write(output.join("legal.db"), b"incomplete")?;
            assert!(!output.join(GENERATION_MANIFEST_FILENAME).exists());
        }
        assert!(!output.exists());
        Ok(())
    }

    #[test]
    fn cleanup_refuses_a_replaced_output_directory() -> Result<()> {
        let root = tempfile::tempdir()?;
        let output = root.path().join("candidate");
        let moved = root.path().join("moved-original");
        let mut guard = FreshOutput::claim(&output)?;
        fs::rename(&output, &moved)?;
        fs::create_dir(&output)?;
        fs::write(output.join("must-survive"), b"substituted")?;
        let error = guard.cleanup().unwrap_err();
        assert!(error.to_string().contains("identity changed"));
        assert_eq!(fs::read(output.join("must-survive"))?, b"substituted");
        guard.preserve();
        fs::remove_dir_all(&output)?;
        fs::remove_dir_all(&moved)?;
        Ok(())
    }

    #[test]
    fn space_budget_accounts_for_reflink_and_copy_phases() -> Result<()> {
        let mut manifest = fixture_manifest(LEGACY_SCHEMA_VERSION);
        manifest.db.size = 40;
        manifest.model.model.size = 10;
        manifest.model.tokenizer.size = 5;
        let source: legal_model::SourceId = "ato".parse()?;
        let ann = crate::ann::ManifestAnn {
            source_id: source.clone(),
            format: crate::ann::ANN_FORMAT.to_string(),
            format_version: crate::ann::ANN_FORMAT_VERSION,
            library: crate::ann::ANN_LIBRARY.to_string(),
            library_version: crate::ann::ANN_LIBRARY_VERSION.to_string(),
            path: crate::ann::sidecar_manifest_path(&source),
            sha256: "4".repeat(64),
            size: 20,
            corpus_id: format!("sha256:{}", "5".repeat(64)),
            embedding_model_id: crate::EMBEDDING_MODEL_ID.to_string(),
            embedding_dimension: crate::EMBEDDING_DIM as u32,
            embedding_set_sha256: "6".repeat(64),
            vector_count: 1,
            seed: crate::ann::ANN_SEED,
            rng: crate::ann::ANN_RNG.to_string(),
            trees: crate::ann::ANN_TREES as u32,
            split_after: crate::ann::ANN_SPLIT_AFTER as u32,
            id_encoding: crate::ann::ANN_ID_ENCODING.to_string(),
            metric: crate::ann::ANN_METRIC.to_string(),
        };
        manifest.ann.insert(source, ann.clone());
        assert_eq!(
            required_projection_space(&manifest, true)?,
            PROJECTION_SPACE_MARGIN_BYTES + 40
        );
        assert_eq!(
            required_projection_space(&manifest, false)?,
            PROJECTION_SPACE_MARGIN_BYTES + 2 * 40
        );
        let source = manifest.ann.keys().next().cloned().expect("fixture ANN");
        manifest.ann.get_mut(&source).expect("fixture ANN").size = u64::MAX;
        assert!(checked_generation_artifact_bytes(&manifest).is_err());
        manifest.db.size = u64::MAX;
        assert!(required_projection_space(&manifest, false).is_err());
        Ok(())
    }

    #[test]
    fn database_size_cap_is_fail_closed() {
        assert!(validate_projection_db_size(0, "legacy").is_err());
        assert!(validate_projection_db_size(MAX_PROJECTION_DB_BYTES, "legacy").is_ok());
        assert!(validate_projection_db_size(MAX_PROJECTION_DB_BYTES + 1, "legacy").is_err());
    }
}
