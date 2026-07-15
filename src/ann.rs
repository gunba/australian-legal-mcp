//! Versioned, mmap-backed ANN sidecar construction, validation, and querying.
//!
//! SQLite int8 embeddings remain authoritative. The sidecar only discovers a
//! candidate set; search always reranks candidates with the stored int8 vectors.

use crate::{EMBEDDING_DIM, EMBEDDING_MODEL_ID};
use anyhow::{anyhow, bail, Context, Result};
use arroy::distances::Cosine;
use arroy::{Database as ArroyDatabase, Reader as ArroyReader, Writer as ArroyWriter};
use heed::types::{Bytes, Str};
use heed::{Database, Env, EnvFlags, EnvOpenOptions};
use legal_model::SourceId;
use rand::SeedableRng;
use rand_chacha::ChaCha12Rng;
use roaring::RoaringBitmap;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

pub(crate) const ANN_DIRECTORY: &str = "ann";
pub(crate) const ANN_FORMAT: &str = "arroy-cosine-f32";
pub(crate) const ANN_FORMAT_VERSION: u32 = 3;
pub(crate) const ANN_LIBRARY: &str = "arroy";
pub(crate) const ANN_LIBRARY_VERSION: &str = "0.6.4";
pub(crate) const ANN_SEED: u64 = 0x4155_534c_414e_4e31;
pub(crate) const ANN_RNG: &str = "chacha12-rand_chacha-0.3.1";
pub(crate) const ANN_TREES: usize = 16;
pub(crate) const ANN_SPLIT_AFTER: usize = 64;
pub(crate) const ANN_INDEX: u16 = 0;
pub(crate) const ANN_ID_ENCODING: &str = "sqlite-chunk-id-u32";
pub(crate) const ANN_METRIC: &str = "cosine-f32-candidates+dot-i8-rerank";
const ANN_DB_NAME: &str = "vectors";
const META_DB_NAME: &str = "metadata";
const META_KEY: &str = "sidecar";
const MIN_MAP_SIZE: u64 = 4 * 1024 * 1024 * 1024;
const MAP_BYTES_PER_VECTOR: u64 = 8 * 1024;
const SOURCE_VECTORS_SQL: &str = r#"
    SELECT e.chunk_id, e.embedding
    FROM chunk_embeddings AS e
    INNER JOIN chunks AS c ON c.chunk_id = e.chunk_id
    WHERE c.source_id = ?1
    ORDER BY e.chunk_id ASC
"#;

pub(crate) fn sidecar_relative_path(source_id: &SourceId) -> PathBuf {
    PathBuf::from(ANN_DIRECTORY).join(format!("{source_id}.ann"))
}

pub(crate) fn sidecar_manifest_path(source_id: &SourceId) -> String {
    format!("{ANN_DIRECTORY}/{source_id}.ann")
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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestAnn {
    pub(crate) source_id: SourceId,
    pub(crate) format: String,
    pub(crate) format_version: u32,
    pub(crate) library: String,
    pub(crate) library_version: String,
    pub(crate) path: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
    pub(crate) corpus_id: String,
    pub(crate) embedding_model_id: String,
    pub(crate) embedding_dimension: u32,
    pub(crate) embedding_set_sha256: String,
    pub(crate) vector_count: u64,
    pub(crate) seed: u64,
    pub(crate) rng: String,
    pub(crate) trees: u32,
    pub(crate) split_after: u32,
    pub(crate) id_encoding: String,
    pub(crate) metric: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SidecarMetadata {
    source_id: SourceId,
    format: String,
    format_version: u32,
    library: String,
    library_version: String,
    corpus_id: String,
    embedding_model_id: String,
    embedding_dimension: u32,
    embedding_set_sha256: String,
    vector_count: u64,
    seed: u64,
    rng: String,
    trees: u32,
    split_after: u32,
    id_encoding: String,
    metric: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnnIdentity {
    pub(crate) source_id: SourceId,
    pub(crate) corpus_id: String,
    pub(crate) embedding_set_sha256: String,
    pub(crate) vector_count: u64,
}

impl ManifestAnn {
    fn embedded_metadata(&self) -> SidecarMetadata {
        SidecarMetadata {
            source_id: self.source_id.clone(),
            format: self.format.clone(),
            format_version: self.format_version,
            library: self.library.clone(),
            library_version: self.library_version.clone(),
            corpus_id: self.corpus_id.clone(),
            embedding_model_id: self.embedding_model_id.clone(),
            embedding_dimension: self.embedding_dimension,
            embedding_set_sha256: self.embedding_set_sha256.clone(),
            vector_count: self.vector_count,
            seed: self.seed,
            rng: self.rng.clone(),
            trees: self.trees,
            split_after: self.split_after,
            id_encoding: self.id_encoding.clone(),
            metric: self.metric.clone(),
        }
    }
}

fn metadata_for(identity: &AnnIdentity) -> SidecarMetadata {
    SidecarMetadata {
        source_id: identity.source_id.clone(),
        format: ANN_FORMAT.to_string(),
        format_version: ANN_FORMAT_VERSION,
        library: ANN_LIBRARY.to_string(),
        library_version: ANN_LIBRARY_VERSION.to_string(),
        corpus_id: identity.corpus_id.clone(),
        embedding_model_id: EMBEDDING_MODEL_ID.to_string(),
        embedding_dimension: EMBEDDING_DIM as u32,
        embedding_set_sha256: identity.embedding_set_sha256.clone(),
        vector_count: identity.vector_count,
        seed: ANN_SEED,
        rng: ANN_RNG.to_string(),
        trees: ANN_TREES as u32,
        split_after: ANN_SPLIT_AFTER as u32,
        id_encoding: ANN_ID_ENCODING.to_string(),
        metric: ANN_METRIC.to_string(),
    }
}

fn enumerate_source_vectors(
    conn: &Connection,
    source_id: &SourceId,
    mut visit: impl FnMut(i64, u32, &[u8]) -> Result<()>,
) -> Result<u64> {
    let mut stmt = conn.prepare(SOURCE_VECTORS_SQL)?;
    let mut rows = stmt.query([source_id.as_str()])?;
    let mut previous = None;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        let chunk_id = row.get::<_, i64>(0)?;
        let embedding = row.get::<_, Vec<u8>>(1)?;
        let item_id = validate_vector_record(chunk_id, &embedding, previous)?;
        visit(chunk_id, item_id, &embedding)?;
        previous = Some(chunk_id);
        count += 1;
    }
    if count == 0 {
        bail!("cannot build ANN sidecar for source `{source_id}` without chunk embeddings");
    }
    Ok(count)
}

pub(crate) fn compute_identity(
    conn: &Connection,
    source_id: &SourceId,
    source_index_sha256: &str,
) -> Result<AnnIdentity> {
    if !is_sha256(source_index_sha256) {
        bail!("source index SHA-256 is malformed");
    }
    let mut embedding_hasher = Sha256::new();
    embedding_hasher.update(b"australian-legal-mcp-embedding-set-v1\0");
    let count = enumerate_source_vectors(conn, source_id, |chunk_id, _, embedding| {
        embedding_hasher.update(chunk_id.to_le_bytes());
        embedding_hasher.update(embedding);
        Ok(())
    })?;
    let embedding_set_sha256 = format!("{:x}", embedding_hasher.finalize());
    let mut corpus_hasher = Sha256::new();
    corpus_hasher.update(b"australian-legal-mcp-ann-corpus-v1\0");
    corpus_hasher.update(source_id.as_str().as_bytes());
    corpus_hasher.update([0]);
    corpus_hasher.update(source_index_sha256.as_bytes());
    corpus_hasher.update([0]);
    corpus_hasher.update(EMBEDDING_MODEL_ID.as_bytes());
    corpus_hasher.update([0]);
    corpus_hasher.update((EMBEDDING_DIM as u64).to_le_bytes());
    corpus_hasher.update(count.to_le_bytes());
    corpus_hasher.update(embedding_set_sha256.as_bytes());
    Ok(AnnIdentity {
        source_id: source_id.clone(),
        corpus_id: format!("sha256:{:x}", corpus_hasher.finalize()),
        embedding_set_sha256,
        vector_count: count,
    })
}

fn validate_vector_record(chunk_id: i64, embedding: &[u8], previous: Option<i64>) -> Result<u32> {
    let item_id = u32::try_from(chunk_id).map_err(|_| {
        anyhow!("chunk_id {chunk_id} cannot be represented by ANN u32 item identifiers")
    })?;
    if embedding.len() != EMBEDDING_DIM {
        bail!(
            "chunk_id {chunk_id} has {} embedding bytes; expected {EMBEDDING_DIM}",
            embedding.len()
        );
    }
    if previous.is_some_and(|value| value >= chunk_id) {
        bail!("ANN input chunk IDs are not strictly increasing at {chunk_id}");
    }
    Ok(item_id)
}

fn map_size(vector_count: u64) -> Result<usize> {
    if usize::BITS < 64 {
        bail!("ANN sidecars require a 64-bit target");
    }
    let requested = vector_count
        .saturating_mul(MAP_BYTES_PER_VECTOR)
        .max(MIN_MAP_SIZE);
    usize::try_from(requested).context("ANN LMDB map size exceeds platform address space")
}

fn open_build_env(path: &Path, vector_count: u64) -> Result<Env> {
    let mut options = EnvOpenOptions::new();
    options.map_size(map_size(vector_count)?).max_dbs(2);
    unsafe {
        options.flags(EnvFlags::NO_SUB_DIR);
        options.open(path)
    }
    .with_context(|| format!("opening ANN build environment {}", path.display()))
}

fn open_read_env(path: &Path) -> Result<Env> {
    let mut options = EnvOpenOptions::new();
    options.max_dbs(2);
    unsafe {
        options.flags(EnvFlags::NO_SUB_DIR | EnvFlags::READ_ONLY | EnvFlags::NO_LOCK);
        options.open(path)
    }
    .with_context(|| format!("opening ANN sidecar {}", path.display()))
}

pub(crate) fn build_sidecar(
    conn: &Connection,
    source_id: &SourceId,
    output_root: &Path,
    source_index_sha256: &str,
    expected_identity: &AnnIdentity,
) -> Result<ManifestAnn> {
    if &expected_identity.source_id != source_id {
        bail!(
            "ANN identity source mismatch: expected `{source_id}`, got `{}`",
            expected_identity.source_id
        );
    }
    let actual_identity = compute_identity(conn, source_id, source_index_sha256)?;
    if &actual_identity != expected_identity {
        bail!("embedding set for source `{source_id}` changed while the ANN sidecar was being prepared");
    }
    let output = output_root.join(sidecar_relative_path(source_id));
    let parent = output
        .parent()
        .ok_or_else(|| anyhow!("ANN output has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let temp_prefix = format!("ann-{source_id}-build-");
    let temp_dir = tempfile::Builder::new()
        .prefix(&temp_prefix)
        .tempdir_in(parent)?;
    let build_path = temp_dir.path().join(format!("{source_id}.ann.part"));
    let env = open_build_env(&build_path, actual_identity.vector_count)?;
    let mut wtxn = env.write_txn()?;
    let vectors: ArroyDatabase<Cosine> = env.create_database(&mut wtxn, Some(ANN_DB_NAME))?;
    let metadata_db: Database<Str, Bytes> = env.create_database(&mut wtxn, Some(META_DB_NAME))?;
    let writer = ArroyWriter::<Cosine>::new(vectors, ANN_INDEX, EMBEDDING_DIM);

    let inserted = enumerate_source_vectors(conn, source_id, |_, item_id, embedding| {
        let vector = embedding
            .iter()
            .map(|byte| (*byte as i8) as f32 / 127.0)
            .collect::<Vec<_>>();
        writer.add_item(&mut wtxn, item_id, &vector)?;
        Ok(())
    })?;
    if inserted != actual_identity.vector_count {
        bail!(
            "ANN vector count changed during build: expected {}, read {inserted}",
            actual_identity.vector_count
        );
    }

    let mut rng = ChaCha12Rng::seed_from_u64(ANN_SEED);
    let mut builder = writer.builder(&mut rng);
    builder.n_trees(ANN_TREES).split_after(ANN_SPLIT_AFTER);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .thread_name(|index| format!("ann-build-{index}"))
        .build()?;
    pool.install(|| builder.build(&mut wtxn))?;
    let metadata = metadata_for(&actual_identity);
    metadata_db.put(&mut wtxn, META_KEY, &serde_json::to_vec(&metadata)?)?;
    wtxn.commit()?;
    env.force_sync()?;
    drop(env);

    validate_sidecar_metadata(&build_path, &metadata)?;
    let size = fs::metadata(&build_path)?.len();
    if size == 0 {
        bail!("built ANN sidecar is empty");
    }
    let sha256 = sha256_path(&build_path)?;
    replace_file(&build_path, &output)?;
    let manifest = ManifestAnn {
        source_id: metadata.source_id.clone(),
        format: metadata.format.clone(),
        format_version: metadata.format_version,
        library: metadata.library.clone(),
        library_version: metadata.library_version.clone(),
        path: sidecar_manifest_path(source_id),
        sha256,
        size,
        corpus_id: metadata.corpus_id.clone(),
        embedding_model_id: metadata.embedding_model_id.clone(),
        embedding_dimension: metadata.embedding_dimension,
        embedding_set_sha256: metadata.embedding_set_sha256.clone(),
        vector_count: metadata.vector_count,
        seed: metadata.seed,
        rng: metadata.rng.clone(),
        trees: metadata.trees,
        split_after: metadata.split_after,
        id_encoding: metadata.id_encoding.clone(),
        metric: metadata.metric.clone(),
    };
    validate_manifest_ann(source_id, &manifest)?;
    Ok(manifest)
}

fn sync_file(path: &Path) -> Result<()> {
    OpenOptions::new().write(true).open(path)?.sync_all()?;
    Ok(())
}

fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    let backup = destination.with_extension("ann.backup");
    if backup.exists() {
        fs::remove_file(&backup)?;
    }
    let had_destination = destination.exists();
    if had_destination {
        fs::copy(destination, &backup)?;
        sync_file(&backup)?;
        fs::remove_file(destination)?;
    }
    let replace = (|| -> Result<()> {
        fs::rename(source, destination)?;
        sync_file(destination)?;
        sync_parent(destination)
    })();
    if let Err(error) = replace {
        let _ = fs::remove_file(destination);
        if had_destination {
            fs::rename(&backup, destination)
                .context("restoring previous ANN sidecar after replacement failure")?;
            sync_file(destination)?;
            sync_parent(destination)?;
        }
        return Err(error)
            .with_context(|| format!("replacing ANN sidecar {}", destination.display()));
    }
    if had_destination {
        if let Err(error) = fs::remove_file(&backup) {
            eprintln!(
                "legal-mcp build: warning: could not remove ANN backup {}: {error}",
                backup.display()
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}

pub(crate) fn validate_manifest_ann(source_id: &SourceId, info: &ManifestAnn) -> Result<()> {
    if &info.source_id != source_id {
        bail!(
            "ANN sidecar source mismatch: expected `{source_id}`, manifest declares `{}`",
            info.source_id
        );
    }
    if info.path != sidecar_manifest_path(source_id) {
        bail!("ANN sidecar path for source `{source_id}` is malformed");
    }
    if info.format != ANN_FORMAT
        || info.format_version != ANN_FORMAT_VERSION
        || info.library != ANN_LIBRARY
        || info.library_version != ANN_LIBRARY_VERSION
    {
        bail!(
            "unsupported ANN sidecar {} v{} ({} {})",
            info.format,
            info.format_version,
            info.library,
            info.library_version
        );
    }
    if info.embedding_model_id != EMBEDDING_MODEL_ID
        || info.embedding_dimension != EMBEDDING_DIM as u32
        || info.seed != ANN_SEED
        || info.rng != ANN_RNG
        || info.trees != ANN_TREES as u32
        || info.split_after != ANN_SPLIT_AFTER as u32
        || info.id_encoding != ANN_ID_ENCODING
        || info.metric != ANN_METRIC
    {
        bail!("ANN sidecar search contract is incompatible with this binary");
    }
    if info.size == 0
        || !is_sha256(&info.sha256)
        || !is_corpus_id(&info.corpus_id)
        || !is_sha256(&info.embedding_set_sha256)
        || info.vector_count == 0
    {
        bail!("ANN sidecar integrity metadata is malformed");
    }
    Ok(())
}

fn read_sidecar_metadata(env: &Env) -> Result<SidecarMetadata> {
    let rtxn = env.read_txn()?;
    let db: Database<Str, Bytes> = env
        .open_database(&rtxn, Some(META_DB_NAME))?
        .ok_or_else(|| anyhow!("ANN sidecar metadata database is missing"))?;
    let bytes = db
        .get(&rtxn, META_KEY)?
        .ok_or_else(|| anyhow!("ANN sidecar metadata record is missing"))?;
    serde_json::from_slice(bytes).context("decoding ANN sidecar metadata")
}

fn validate_sidecar_metadata(path: &Path, expected: &SidecarMetadata) -> Result<()> {
    let env = open_read_env(path)?;
    let actual = read_sidecar_metadata(&env)?;
    if &actual != expected {
        bail!("ANN sidecar embedded metadata does not match its manifest contract");
    }
    let rtxn = env.read_txn()?;
    let database: ArroyDatabase<Cosine> = env
        .open_database(&rtxn, Some(ANN_DB_NAME))?
        .ok_or_else(|| anyhow!("ANN vectors database is missing"))?;
    let reader = ArroyReader::<Cosine>::open(&rtxn, ANN_INDEX, database)?;
    if reader.dimensions() != EMBEDDING_DIM || reader.n_items() != expected.vector_count {
        bail!("ANN sidecar vector shape does not match embedded metadata");
    }
    Ok(())
}

pub(crate) fn verify_sidecar(path: &Path, source_id: &SourceId, info: &ManifestAnn) -> Result<()> {
    validate_manifest_ann(source_id, info)?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading ANN sidecar metadata at {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("ANN sidecar must be a regular non-symlink file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            bail!("ANN sidecar must not be hard-linked");
        }
    }
    if metadata.len() != info.size {
        bail!(
            "ANN sidecar size mismatch: manifest {}, installed {}",
            info.size,
            metadata.len()
        );
    }
    let digest = sha256_path(path)?;
    if digest != info.sha256 {
        bail!("ANN sidecar SHA-256 mismatch");
    }
    validate_sidecar_metadata(path, &info.embedded_metadata())
}

pub(crate) fn search_sidecar(
    path: &Path,
    source_id: &SourceId,
    info: &ManifestAnn,
    query: &[i8; EMBEDDING_DIM],
    candidates: &RoaringBitmap,
    count: usize,
    search_k: usize,
) -> Result<Vec<u32>> {
    validate_manifest_ann(source_id, info)?;
    if count == 0 || candidates.is_empty() {
        return Ok(Vec::new());
    }
    let env = open_read_env(path)?;
    let embedded = read_sidecar_metadata(&env)?;
    if embedded != info.embedded_metadata() {
        bail!("ANN sidecar generation does not match installed manifest");
    }
    let rtxn = env.read_txn()?;
    let database: ArroyDatabase<Cosine> = env
        .open_database(&rtxn, Some(ANN_DB_NAME))?
        .ok_or_else(|| anyhow!("ANN vectors database is missing"))?;
    let reader = ArroyReader::<Cosine>::open(&rtxn, ANN_INDEX, database)?;
    let query = query
        .iter()
        .map(|value| *value as f32 / 127.0)
        .collect::<Vec<_>>();
    let wanted = count.min(candidates.len() as usize);
    let search_k = NonZeroUsize::new(search_k.max(wanted))
        .ok_or_else(|| anyhow!("ANN search budget cannot be zero"))?;
    let mut builder = reader.nns(wanted);
    builder.candidates(candidates).search_k(search_k);
    let results = builder.by_vector(&rtxn, &query)?;
    Ok(results.into_iter().map(|(item_id, _)| item_id).collect())
}

pub(crate) fn sha256_path(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    const PRIMARY_SOURCE: &str = "source-a";
    const SECONDARY_SOURCE: &str = "source-b";
    const PRIMARY_FIRST_CHUNK: u32 = 1;
    const SECONDARY_FIRST_CHUNK: u32 = 10_001;

    fn source(value: &str) -> Result<SourceId> {
        value.parse().map_err(anyhow::Error::new)
    }

    fn vector(source_seed: u32, chunk_id: u32) -> Vec<u8> {
        (0..EMBEDDING_DIM)
            .map(|dimension| {
                let value = ((source_seed as usize * 43 + chunk_id as usize * 31 + dimension * 17)
                    % 255) as i16
                    - 127;
                value as i8 as u8
            })
            .collect()
    }

    fn test_connection(count_per_source: u32) -> Result<Connection> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            "CREATE TABLE documents(
                source_id TEXT NOT NULL,
                native_id TEXT NOT NULL,
                PRIMARY KEY(source_id, native_id)
            );
            CREATE TABLE chunks(
                chunk_id INTEGER PRIMARY KEY,
                source_id TEXT NOT NULL,
                native_id TEXT NOT NULL,
                ord INTEGER NOT NULL,
                FOREIGN KEY(source_id, native_id)
                    REFERENCES documents(source_id, native_id) ON DELETE CASCADE
            );
            CREATE INDEX idx_chunks_source_chunk ON chunks(source_id, chunk_id);
            CREATE TABLE chunk_embeddings(
                chunk_id INTEGER PRIMARY KEY REFERENCES chunks(chunk_id) ON DELETE CASCADE,
                embedding BLOB NOT NULL
            );",
        )?;
        for source_id in [PRIMARY_SOURCE, SECONDARY_SOURCE] {
            conn.execute(
                "INSERT INTO documents(source_id, native_id) VALUES (?1, 'shared-native-id')",
                [source_id],
            )?;
        }
        {
            let mut insert_chunk = conn.prepare(
                "INSERT INTO chunks(chunk_id, source_id, native_id, ord)
                 VALUES (?1, ?2, 'shared-native-id', ?3)",
            )?;
            let mut insert_embedding =
                conn.prepare("INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (?1, ?2)")?;
            for (source_id, first_chunk, source_seed) in [
                (PRIMARY_SOURCE, PRIMARY_FIRST_CHUNK, 3),
                (SECONDARY_SOURCE, SECONDARY_FIRST_CHUNK, 11),
            ] {
                for offset in 0..count_per_source {
                    let chunk_id = first_chunk + offset;
                    insert_chunk.execute(params![chunk_id, source_id, offset])?;
                    insert_embedding.execute(params![chunk_id, vector(source_seed, chunk_id)])?;
                }
            }
        }
        Ok(conn)
    }

    #[test]
    fn per_source_sidecars_are_isolated_and_deterministic() -> Result<()> {
        let count = 160;
        let conn = test_connection(count)?;
        let primary = source(PRIMARY_SOURCE)?;
        let secondary = source(SECONDARY_SOURCE)?;
        let source_sha = "a".repeat(64);
        let primary_identity = compute_identity(&conn, &primary, &source_sha)?;
        let secondary_identity = compute_identity(&conn, &secondary, &source_sha)?;
        assert_eq!(primary_identity.source_id, primary);
        assert_eq!(secondary_identity.source_id, secondary);
        assert_eq!(primary_identity.vector_count, u64::from(count));
        assert_eq!(secondary_identity.vector_count, u64::from(count));
        assert_ne!(primary_identity.corpus_id, secondary_identity.corpus_id);
        assert_ne!(
            primary_identity.embedding_set_sha256,
            secondary_identity.embedding_set_sha256
        );

        let first_root = tempfile::tempdir()?;
        let second_root = tempfile::tempdir()?;
        let first_primary = build_sidecar(
            &conn,
            &primary,
            first_root.path(),
            &source_sha,
            &primary_identity,
        )?;
        let second_primary = build_sidecar(
            &conn,
            &primary,
            second_root.path(),
            &source_sha,
            &primary_identity,
        )?;
        let first_secondary = build_sidecar(
            &conn,
            &secondary,
            first_root.path(),
            &source_sha,
            &secondary_identity,
        )?;
        let first_primary_path = first_root.path().join(sidecar_relative_path(&primary));
        let second_primary_path = second_root.path().join(sidecar_relative_path(&primary));
        let first_secondary_path = first_root.path().join(sidecar_relative_path(&secondary));

        assert_eq!(first_primary, second_primary);
        assert_eq!(
            fs::read(&first_primary_path)?,
            fs::read(&second_primary_path)?
        );
        assert_eq!(first_primary.path, "ann/source-a.ann");
        assert_eq!(first_secondary.path, "ann/source-b.ann");
        assert_eq!(first_primary.source_id, primary);
        assert_eq!(first_secondary.source_id, secondary);
        verify_sidecar(&first_primary_path, &primary, &first_primary)?;
        verify_sidecar(&first_secondary_path, &secondary, &first_secondary)?;

        let candidates = RoaringBitmap::from_iter([3, 9, 10_003, 10_009]);
        let query = [1i8; EMBEDDING_DIM];
        let first_found = search_sidecar(
            &first_primary_path,
            &primary,
            &first_primary,
            &query,
            &candidates,
            4,
            10_000,
        )?;
        let second_found = search_sidecar(
            &second_primary_path,
            &primary,
            &second_primary,
            &query,
            &candidates,
            4,
            10_000,
        )?;
        let secondary_found = search_sidecar(
            &first_secondary_path,
            &secondary,
            &first_secondary,
            &query,
            &candidates,
            4,
            10_000,
        )?;
        assert_eq!(first_found, second_found);
        assert_eq!(first_found.len(), 2);
        assert!(first_found.iter().all(|item| *item < SECONDARY_FIRST_CHUNK));
        assert_eq!(secondary_found.len(), 2);
        assert!(secondary_found
            .iter()
            .all(|item| *item >= SECONDARY_FIRST_CHUNK));
        Ok(())
    }

    #[test]
    fn sidecar_rejects_wrong_source_corruption_and_contract_mismatches() -> Result<()> {
        let conn = test_connection(80)?;
        let primary = source(PRIMARY_SOURCE)?;
        let secondary = source(SECONDARY_SOURCE)?;
        let source_sha = "b".repeat(64);
        let identity = compute_identity(&conn, &primary, &source_sha)?;
        let secondary_identity = compute_identity(&conn, &secondary, &source_sha)?;
        let dir = tempfile::tempdir()?;
        let info = build_sidecar(&conn, &primary, dir.path(), &source_sha, &identity)?;
        let path = dir.path().join(sidecar_relative_path(&primary));

        let error = verify_sidecar(&path, &secondary, &info).unwrap_err();
        assert!(error.to_string().contains("source mismatch"));
        let candidates = RoaringBitmap::from_iter([1, 2]);
        let error = search_sidecar(
            &path,
            &secondary,
            &info,
            &[0i8; EMBEDDING_DIM],
            &candidates,
            2,
            100,
        )
        .unwrap_err();
        assert!(error.to_string().contains("source mismatch"));
        let error = search_sidecar(
            &path,
            &secondary,
            &info,
            &[0i8; EMBEDDING_DIM],
            &RoaringBitmap::new(),
            0,
            0,
        )
        .unwrap_err();
        assert!(error.to_string().contains("source mismatch"));
        let error = build_sidecar(
            &conn,
            &primary,
            dir.path(),
            &source_sha,
            &secondary_identity,
        )
        .unwrap_err();
        assert!(error.to_string().contains("identity source mismatch"));

        let mut rebound = info.clone();
        rebound.source_id = secondary.clone();
        rebound.path = sidecar_manifest_path(&secondary);
        let error = verify_sidecar(&path, &secondary, &rebound).unwrap_err();
        assert!(error
            .to_string()
            .contains("embedded metadata does not match"));

        let missing = dir.path().join("missing.ann");
        assert!(verify_sidecar(&missing, &primary, &info).is_err());

        let corrupt = dir.path().join("corrupt.ann");
        fs::copy(&path, &corrupt)?;
        let mut bytes = fs::read(&corrupt)?;
        let last = bytes.len() - 1;
        bytes[last] ^= 0x5a;
        fs::write(&corrupt, bytes)?;
        assert!(verify_sidecar(&corrupt, &primary, &info).is_err());

        let mut mismatched = info.clone();
        mismatched.corpus_id = format!("sha256:{}", "0".repeat(64));
        assert!(verify_sidecar(&path, &primary, &mismatched).is_err());
        let mut unsupported = info.clone();
        unsupported.format_version += 1;
        assert!(validate_manifest_ann(&primary, &unsupported).is_err());
        let mut remote_path = info.clone();
        remote_path.path = "https://example.test/source-a.ann".to_string();
        assert!(validate_manifest_ann(&primary, &remote_path).is_err());
        let mut malformed_path = info.clone();
        malformed_path.path = " source-a.ann".to_string();
        assert!(validate_manifest_ann(&primary, &malformed_path).is_err());
        let mut uppercase_digest = info;
        uppercase_digest.sha256.make_ascii_uppercase();
        assert!(validate_manifest_ann(&primary, &uppercase_digest).is_err());
        Ok(())
    }

    #[test]
    fn identity_binds_source_for_the_same_chunk_set() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE chunks(
                chunk_id INTEGER PRIMARY KEY,
                source_id TEXT NOT NULL
            );
            CREATE TABLE chunk_embeddings(
                chunk_id INTEGER PRIMARY KEY,
                embedding BLOB NOT NULL
            );",
        )?;
        conn.execute(
            "INSERT INTO chunks(chunk_id, source_id) VALUES (1, ?1)",
            [PRIMARY_SOURCE],
        )?;
        conn.execute(
            "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (1, ?1)",
            params![vec![7u8; EMBEDDING_DIM]],
        )?;
        let source_sha = "c".repeat(64);
        let primary = source(PRIMARY_SOURCE)?;
        let secondary = source(SECONDARY_SOURCE)?;
        let first = compute_identity(&conn, &primary, &source_sha)?;
        conn.execute(
            "UPDATE chunks SET source_id = ?1 WHERE chunk_id = 1",
            [SECONDARY_SOURCE],
        )?;
        let second = compute_identity(&conn, &secondary, &source_sha)?;
        assert_eq!(first.embedding_set_sha256, second.embedding_set_sha256);
        assert_ne!(first.corpus_id, second.corpus_id);
        Ok(())
    }

    #[test]
    fn identity_rejects_invalid_vector_shape_id_range_and_empty_source() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE chunks(
                chunk_id INTEGER PRIMARY KEY,
                source_id TEXT NOT NULL
            );
            CREATE TABLE chunk_embeddings(
                chunk_id INTEGER PRIMARY KEY,
                embedding BLOB NOT NULL
            );",
        )?;
        let primary = source(PRIMARY_SOURCE)?;
        let secondary = source(SECONDARY_SOURCE)?;
        let invalid_id = i64::from(u32::MAX) + 1;
        conn.execute(
            "INSERT INTO chunks(chunk_id, source_id) VALUES (?1, ?2)",
            params![invalid_id, primary.as_str()],
        )?;
        conn.execute(
            "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (?1, ?2)",
            params![invalid_id, vec![0u8; EMBEDDING_DIM]],
        )?;
        let error = compute_identity(&conn, &primary, &"d".repeat(64)).unwrap_err();
        assert!(error.to_string().contains("cannot be represented"));
        conn.execute("DELETE FROM chunk_embeddings", [])?;
        conn.execute("DELETE FROM chunks", [])?;
        conn.execute(
            "INSERT INTO chunks(chunk_id, source_id) VALUES (1, ?1)",
            [primary.as_str()],
        )?;
        conn.execute(
            "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (1, ?1)",
            params![vec![0u8; EMBEDDING_DIM - 1]],
        )?;
        let error = compute_identity(&conn, &primary, &"d".repeat(64)).unwrap_err();
        assert!(error.to_string().contains("expected 256"));
        let error = compute_identity(&conn, &secondary, &"d".repeat(64)).unwrap_err();
        assert!(error.to_string().contains("without chunk embeddings"));
        assert!(compute_identity(&conn, &primary, &"D".repeat(64)).is_err());
        Ok(())
    }

    fn exact_rank(
        conn: &Connection,
        source_id: &SourceId,
        query: &[i8; EMBEDDING_DIM],
        limit: usize,
    ) -> Result<Vec<u32>> {
        let mut exact = Vec::new();
        enumerate_source_vectors(conn, source_id, |_, item_id, embedding| {
            exact.push((item_id, crate::semantic::dot_i8(query, embedding)?));
            Ok(())
        })?;
        exact.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        exact.truncate(limit);
        Ok(exact.into_iter().map(|(item_id, _)| item_id).collect())
    }

    fn source_embedding_at_offset(
        conn: &Connection,
        source_id: &SourceId,
        offset: usize,
    ) -> Result<Vec<u8>> {
        conn.query_row(
            "SELECT e.embedding
             FROM chunk_embeddings AS e
             INNER JOIN chunks AS c ON c.chunk_id = e.chunk_id
             WHERE c.source_id = ?1
             ORDER BY e.chunk_id ASC
             LIMIT 1 OFFSET ?2",
            params![source_id.as_str(), offset as i64],
            |row| row.get(0),
        )
        .with_context(|| format!("reading benchmark vector {offset} for source `{source_id}`"))
    }

    #[test]
    #[ignore = "run explicitly with LEGAL_MCP_BENCH_DB, LEGAL_MCP_BENCH_OUTPUT_ROOT, LEGAL_MCP_BENCH_SOURCE, and LEGAL_MCP_BENCH_SOURCE_INDEX_SHA256"]
    fn benchmark_installed_corpus_sidecar() -> Result<()> {
        let db = std::env::var_os("LEGAL_MCP_BENCH_DB")
            .ok_or_else(|| anyhow!("LEGAL_MCP_BENCH_DB is required"))?;
        let output_root = std::env::var_os("LEGAL_MCP_BENCH_OUTPUT_ROOT")
            .ok_or_else(|| anyhow!("LEGAL_MCP_BENCH_OUTPUT_ROOT is required"))?;
        let source_id = std::env::var("LEGAL_MCP_BENCH_SOURCE")
            .context("LEGAL_MCP_BENCH_SOURCE is required")?
            .parse::<SourceId>()
            .context("LEGAL_MCP_BENCH_SOURCE must be a valid source id")?;
        let source_sha = std::env::var("LEGAL_MCP_BENCH_SOURCE_INDEX_SHA256")
            .context("LEGAL_MCP_BENCH_SOURCE_INDEX_SHA256 is required")?;
        let requested_candidate_count = std::env::var("LEGAL_MCP_BENCH_CANDIDATES")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .context("LEGAL_MCP_BENCH_CANDIDATES must be an integer")?
            .unwrap_or(1_000);
        let requested_search_k = std::env::var("LEGAL_MCP_BENCH_SEARCH_K")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .context("LEGAL_MCP_BENCH_SEARCH_K must be an integer")?;
        let minimum_recall = std::env::var("LEGAL_MCP_BENCH_MIN_RECALL")
            .ok()
            .map(|value| value.parse::<f64>())
            .transpose()
            .context("LEGAL_MCP_BENCH_MIN_RECALL must be a number")?
            .unwrap_or(0.99);
        let conn = Connection::open(db)?;
        let identity = compute_identity(&conn, &source_id, &source_sha)?;
        let candidate_count = requested_candidate_count.min(identity.vector_count as usize);
        let search_k = requested_search_k.unwrap_or_else(|| {
            crate::search::initial_ann_search_k(
                identity.vector_count as usize,
                candidate_count,
                ANN_TREES,
            )
        });
        let started = std::time::Instant::now();
        let output_root = Path::new(&output_root);
        let info = build_sidecar(&conn, &source_id, output_root, &source_sha, &identity)?;
        let output_path = output_root.join(sidecar_relative_path(&source_id));
        let build_elapsed = started.elapsed();

        let mut ids = RoaringBitmap::new();
        let mut first = None;
        enumerate_source_vectors(&conn, &source_id, |_, item_id, embedding| {
            ids.insert(item_id);
            if first.is_none() {
                first = Some(embedding.to_vec());
            }
            Ok(())
        })?;
        let query: [i8; EMBEDDING_DIM] = first
            .ok_or_else(|| anyhow!("benchmark source has no embeddings"))?
            .into_iter()
            .map(|byte| byte as i8)
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|_| anyhow!("benchmark query has wrong dimensions"))?;
        let exact_started = std::time::Instant::now();
        for _ in 0..5 {
            let exact = exact_rank(&conn, &source_id, &query, 50)?;
            if exact.len() != 50 {
                bail!("benchmark exact query underfilled: {}", exact.len());
            }
        }
        let exact_elapsed = exact_started.elapsed();
        let query_started = std::time::Instant::now();
        for _ in 0..20 {
            let found = search_sidecar(
                &output_path,
                &source_id,
                &info,
                &query,
                &ids,
                candidate_count,
                search_k,
            )?;
            if found.len() != candidate_count {
                bail!(
                    "benchmark ANN query underfilled: got {}, expected {candidate_count}",
                    found.len()
                );
            }
        }
        let query_elapsed = query_started.elapsed();
        let mut recall_total = 0.0;
        let recall_queries = 5usize;
        for query_index in 0..recall_queries {
            let offset = (identity.vector_count as usize / recall_queries) * query_index;
            let query_bytes = source_embedding_at_offset(&conn, &source_id, offset)?;
            let recall_query: [i8; EMBEDDING_DIM] = query_bytes
                .into_iter()
                .map(|byte| byte as i8)
                .collect::<Vec<_>>()
                .try_into()
                .map_err(|_| anyhow!("benchmark recall query has wrong dimensions"))?;
            let candidates = search_sidecar(
                &output_path,
                &source_id,
                &info,
                &recall_query,
                &ids,
                candidate_count,
                search_k,
            )?;
            let candidate_set = candidates
                .into_iter()
                .collect::<std::collections::HashSet<_>>();
            let exact = exact_rank(&conn, &source_id, &recall_query, 50)?;
            let recalled = exact
                .iter()
                .filter(|chunk_id| candidate_set.contains(chunk_id))
                .count();
            recall_total += recalled as f64 / 50.0;
        }
        let recall_at_50 = recall_total / recall_queries as f64;
        if recall_at_50 < minimum_recall {
            bail!(
                "ANN benchmark recall@50 {recall_at_50:.3} is below required {minimum_recall:.3}"
            );
        }
        eprintln!(
            "ANN_BENCH source={} vectors={} size={} build_ms={} candidates={} search_k={} ann_query_avg_ms={:.3} exact_query_avg_ms={:.3} recall_at_50={:.3}",
            source_id,
            identity.vector_count,
            info.size,
            build_elapsed.as_millis(),
            candidate_count,
            search_k,
            query_elapsed.as_secs_f64() * 1000.0 / 20.0,
            exact_elapsed.as_secs_f64() * 1000.0 / 5.0,
            recall_at_50,
        );
        Ok(())
    }
}
