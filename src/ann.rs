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
use rand::SeedableRng;
use rand_chacha::ChaCha12Rng;
use roaring::RoaringBitmap;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::path::Path;

pub(crate) const ANN_FILENAME: &str = "ato.ann";
pub(crate) const ANN_FORMAT: &str = "arroy-cosine-f32";
pub(crate) const ANN_FORMAT_VERSION: u32 = 2;
pub(crate) const ANN_LIBRARY: &str = "arroy";
pub(crate) const ANN_LIBRARY_VERSION: &str = "0.6.4";
pub(crate) const ANN_SEED: u64 = 0x4154_4f2d_414e_4e31;
pub(crate) const ANN_RNG: &str = "chacha12-rand_chacha-0.3.1";
pub(crate) const ANN_TREES: usize = 16;
pub(crate) const ANN_SPLIT_AFTER: usize = 64;
pub(crate) const ANN_INDEX: u16 = 0;
pub(crate) const ANN_ID_ENCODING: &str = "sqlite-chunk-id-u32";
pub(crate) const ANN_METRIC: &str = "cosine-f32-candidates+dot-i8-rerank";
const ANN_DB_NAME: &str = "vectors";
const META_DB_NAME: &str = "ato-metadata";
const META_KEY: &str = "sidecar";
const MIN_MAP_SIZE: u64 = 4 * 1024 * 1024 * 1024;
const MAP_BYTES_PER_VECTOR: u64 = 8 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestAnn {
    pub(crate) format: String,
    pub(crate) format_version: u32,
    pub(crate) library: String,
    pub(crate) library_version: String,
    pub(crate) url: String,
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
    pub(crate) corpus_id: String,
    pub(crate) embedding_set_sha256: String,
    pub(crate) vector_count: u64,
}

impl ManifestAnn {
    fn embedded_metadata(&self) -> SidecarMetadata {
        SidecarMetadata {
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

pub(crate) fn compute_identity(
    conn: &Connection,
    source_index_sha256: &str,
) -> Result<AnnIdentity> {
    if source_index_sha256.len() != 64
        || !source_index_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("source index SHA-256 is malformed");
    }
    let mut embedding_hasher = Sha256::new();
    embedding_hasher.update(b"ato-mcp-embedding-set-v1\0");
    let mut count = 0u64;
    let mut previous = None;
    let mut stmt =
        conn.prepare("SELECT chunk_id, embedding FROM chunk_embeddings ORDER BY chunk_id ASC")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    for row in rows {
        let (chunk_id, embedding) = row?;
        validate_vector_record(chunk_id, &embedding, previous)?;
        embedding_hasher.update(chunk_id.to_le_bytes());
        embedding_hasher.update(&embedding);
        previous = Some(chunk_id);
        count += 1;
    }
    if count == 0 {
        bail!("cannot build ANN sidecar without chunk embeddings");
    }
    let embedding_set_sha256 = format!("{:x}", embedding_hasher.finalize());
    let mut corpus_hasher = Sha256::new();
    corpus_hasher.update(b"ato-mcp-ann-corpus-v1\0");
    corpus_hasher.update(source_index_sha256.as_bytes());
    corpus_hasher.update([0]);
    corpus_hasher.update(EMBEDDING_MODEL_ID.as_bytes());
    corpus_hasher.update([0]);
    corpus_hasher.update((EMBEDDING_DIM as u64).to_le_bytes());
    corpus_hasher.update(count.to_le_bytes());
    corpus_hasher.update(embedding_set_sha256.as_bytes());
    Ok(AnnIdentity {
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
    output: &Path,
    source_index_sha256: &str,
    expected_identity: &AnnIdentity,
) -> Result<ManifestAnn> {
    let actual_identity = compute_identity(conn, source_index_sha256)?;
    if &actual_identity != expected_identity {
        bail!("embedding set changed while the ANN sidecar was being prepared");
    }
    let parent = output
        .parent()
        .ok_or_else(|| anyhow!("ANN output has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let temp_dir = tempfile::Builder::new()
        .prefix("ato-ann-build-")
        .tempdir_in(parent)?;
    let build_path = temp_dir.path().join("ato.ann.part");
    let env = open_build_env(&build_path, actual_identity.vector_count)?;
    let mut wtxn = env.write_txn()?;
    let vectors: ArroyDatabase<Cosine> = env.create_database(&mut wtxn, Some(ANN_DB_NAME))?;
    let metadata_db: Database<Str, Bytes> = env.create_database(&mut wtxn, Some(META_DB_NAME))?;
    let writer = ArroyWriter::<Cosine>::new(vectors, ANN_INDEX, EMBEDDING_DIM);

    let mut previous = None;
    let mut inserted = 0u64;
    let mut stmt =
        conn.prepare("SELECT chunk_id, embedding FROM chunk_embeddings ORDER BY chunk_id ASC")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    for row in rows {
        let (chunk_id, embedding) = row?;
        let item_id = validate_vector_record(chunk_id, &embedding, previous)?;
        let vector = embedding
            .iter()
            .map(|byte| (*byte as i8) as f32 / 127.0)
            .collect::<Vec<_>>();
        writer.add_item(&mut wtxn, item_id, &vector)?;
        previous = Some(chunk_id);
        inserted += 1;
    }
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
        .thread_name(|index| format!("ato-ann-build-{index}"))
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
    replace_file(&build_path, output)?;
    let mut manifest = ManifestAnn {
        format: metadata.format.clone(),
        format_version: metadata.format_version,
        library: metadata.library.clone(),
        library_version: metadata.library_version.clone(),
        url: ANN_FILENAME.to_string(),
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
    validate_manifest_ann(&manifest)?;
    // Always derive the URL from the caller's output filename for non-standard
    // maintainer output directories.
    manifest.url = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("ANN output filename is not UTF-8"))?
        .to_string();
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
                "ato-mcp build: warning: could not remove ANN backup {}: {error}",
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

pub(crate) fn validate_manifest_ann(info: &ManifestAnn) -> Result<()> {
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
        || info.sha256.len() != 64
        || !info.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !info.corpus_id.starts_with("sha256:")
        || info.corpus_id.len() != 71
        || info.embedding_set_sha256.len() != 64
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

pub(crate) fn verify_sidecar(path: &Path, info: &ManifestAnn) -> Result<()> {
    validate_manifest_ann(info)?;
    let metadata = fs::metadata(path)
        .with_context(|| format!("reading ANN sidecar metadata at {}", path.display()))?;
    if metadata.len() != info.size {
        bail!(
            "ANN sidecar size mismatch: manifest {}, installed {}",
            info.size,
            metadata.len()
        );
    }
    let digest = sha256_path(path)?;
    if !digest.eq_ignore_ascii_case(&info.sha256) {
        bail!("ANN sidecar SHA-256 mismatch");
    }
    validate_sidecar_metadata(path, &info.embedded_metadata())
}

pub(crate) fn search_sidecar(
    path: &Path,
    info: &ManifestAnn,
    query: &[i8; EMBEDDING_DIM],
    candidates: &RoaringBitmap,
    count: usize,
    search_k: usize,
) -> Result<Vec<u32>> {
    if count == 0 || candidates.is_empty() {
        return Ok(Vec::new());
    }
    validate_manifest_ann(info)?;
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
    let mut builder = reader.nns(wanted);
    builder
        .candidates(candidates)
        .search_k(NonZeroUsize::new(search_k.max(wanted)).expect("wanted is non-zero"));
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

pub(crate) fn copy_verified(
    input: &mut dyn Read,
    destination: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<u64> {
    let mut output = File::create(destination)?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| anyhow!("ANN sidecar size overflow"))?;
        if total > expected_size {
            bail!("ANN sidecar exceeds manifest size {expected_size}");
        }
        output.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
    }
    output.sync_all()?;
    if total != expected_size {
        bail!("ANN sidecar size mismatch: expected {expected_size}, received {total}");
    }
    let digest = format!("{:x}", hasher.finalize());
    if !digest.eq_ignore_ascii_case(expected_sha256) {
        bail!("ANN sidecar SHA-256 mismatch");
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn test_connection(count: u32) -> Result<Connection> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE chunk_embeddings(
                chunk_id INTEGER PRIMARY KEY,
                embedding BLOB NOT NULL
            );",
        )?;
        let mut insert =
            conn.prepare("INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (?1, ?2)")?;
        for id in 1..=count {
            let vector = (0..EMBEDDING_DIM)
                .map(|dimension| {
                    let value = ((id as usize * 31 + dimension * 17) % 255) as i16 - 127;
                    value as i8 as u8
                })
                .collect::<Vec<_>>();
            insert.execute(params![id, vector])?;
        }
        drop(insert);
        Ok(conn)
    }

    #[test]
    fn sidecar_build_contract_and_filter_fills() -> Result<()> {
        let conn = test_connection(300)?;
        let source_sha = "a".repeat(64);
        let identity = compute_identity(&conn, &source_sha)?;
        let dir = tempfile::tempdir()?;
        let first_path = dir.path().join("first.ann");
        let second_path = dir.path().join("second.ann");
        let first = build_sidecar(&conn, &first_path, &source_sha, &identity)?;
        let second = build_sidecar(&conn, &second_path, &source_sha, &identity)?;
        assert_eq!(first.embedded_metadata(), second.embedded_metadata());

        let candidates = RoaringBitmap::from_iter([3, 9, 41, 77, 199]);
        let query = [1i8; EMBEDDING_DIM];
        let first_found = search_sidecar(&first_path, &first, &query, &candidates, 5, 10_000)?;
        let second_found = search_sidecar(&second_path, &second, &query, &candidates, 5, 10_000)?;
        assert_eq!(first_found, second_found);
        assert_eq!(first_found.len(), candidates.len() as usize);
        assert!(first_found.iter().all(|item| candidates.contains(*item)));
        Ok(())
    }

    #[test]
    fn sidecar_rejects_missing_corrupt_and_mismatched_artifacts() -> Result<()> {
        let conn = test_connection(80)?;
        let source_sha = "b".repeat(64);
        let identity = compute_identity(&conn, &source_sha)?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ato.ann");
        let info = build_sidecar(&conn, &path, &source_sha, &identity)?;

        let missing = dir.path().join("missing.ann");
        assert!(verify_sidecar(&missing, &info).is_err());

        let corrupt = dir.path().join("corrupt.ann");
        fs::copy(&path, &corrupt)?;
        let mut bytes = fs::read(&corrupt)?;
        let last = bytes.len() - 1;
        bytes[last] ^= 0x5a;
        fs::write(&corrupt, bytes)?;
        assert!(verify_sidecar(&corrupt, &info).is_err());

        let mut mismatched = info.clone();
        mismatched.corpus_id = format!("sha256:{}", "0".repeat(64));
        assert!(verify_sidecar(&path, &mismatched).is_err());
        let mut unsupported = info.clone();
        unsupported.format_version += 1;
        assert!(validate_manifest_ann(&unsupported).is_err());
        Ok(())
    }

    #[test]
    fn identity_rejects_invalid_vector_shape_and_id_range() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE chunk_embeddings(chunk_id INTEGER PRIMARY KEY, embedding BLOB NOT NULL);",
        )?;
        conn.execute(
            "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (?1, ?2)",
            params![i64::from(u32::MAX) + 1, vec![0u8; EMBEDDING_DIM]],
        )?;
        let error = compute_identity(&conn, &"c".repeat(64)).unwrap_err();
        assert!(error.to_string().contains("cannot be represented"));
        conn.execute("DELETE FROM chunk_embeddings", [])?;
        conn.execute(
            "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (1, ?1)",
            params![vec![0u8; EMBEDDING_DIM - 1]],
        )?;
        let error = compute_identity(&conn, &"c".repeat(64)).unwrap_err();
        assert!(error.to_string().contains("expected 256"));
        Ok(())
    }

    #[test]
    #[ignore = "run explicitly with ATO_MCP_BENCH_DB and ATO_MCP_BENCH_OUT"]
    fn benchmark_installed_corpus_sidecar() -> Result<()> {
        let db = std::env::var_os("ATO_MCP_BENCH_DB")
            .ok_or_else(|| anyhow!("ATO_MCP_BENCH_DB is required"))?;
        let output = std::env::var_os("ATO_MCP_BENCH_OUT")
            .ok_or_else(|| anyhow!("ATO_MCP_BENCH_OUT is required"))?;
        let candidate_count = std::env::var("ATO_MCP_BENCH_CANDIDATES")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .context("ATO_MCP_BENCH_CANDIDATES must be an integer")?
            .unwrap_or(1_000);
        let requested_search_k = std::env::var("ATO_MCP_BENCH_SEARCH_K")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .context("ATO_MCP_BENCH_SEARCH_K must be an integer")?;
        let minimum_recall = std::env::var("ATO_MCP_BENCH_MIN_RECALL")
            .ok()
            .map(|value| value.parse::<f64>())
            .transpose()
            .context("ATO_MCP_BENCH_MIN_RECALL must be a number")?
            .unwrap_or(0.99);
        let conn = Connection::open(db)?;
        let source_sha =
            crate::db::get_meta(&conn, "source_index_sha256")?.unwrap_or_else(|| "0".repeat(64));
        let identity = compute_identity(&conn, &source_sha)?;
        let search_k = requested_search_k.unwrap_or_else(|| {
            crate::search::initial_ann_search_k(
                identity.vector_count as usize,
                candidate_count,
                ANN_TREES,
            )
        });
        let started = std::time::Instant::now();
        let output_path = Path::new(&output);
        let info = if output_path.is_file() {
            let env = open_read_env(output_path)?;
            let metadata = read_sidecar_metadata(&env)?;
            ManifestAnn {
                format: metadata.format,
                format_version: metadata.format_version,
                library: metadata.library,
                library_version: metadata.library_version,
                url: ANN_FILENAME.to_string(),
                sha256: sha256_path(output_path)?,
                size: fs::metadata(output_path)?.len(),
                corpus_id: metadata.corpus_id,
                embedding_model_id: metadata.embedding_model_id,
                embedding_dimension: metadata.embedding_dimension,
                embedding_set_sha256: metadata.embedding_set_sha256,
                vector_count: metadata.vector_count,
                seed: metadata.seed,
                rng: metadata.rng,
                trees: metadata.trees,
                split_after: metadata.split_after,
                id_encoding: metadata.id_encoding,
                metric: metadata.metric,
            }
        } else {
            build_sidecar(&conn, output_path, &source_sha, &identity)?
        };
        let build_elapsed = started.elapsed();

        let mut ids = RoaringBitmap::new();
        let mut stmt = conn.prepare("SELECT chunk_id FROM chunk_embeddings ORDER BY chunk_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, u32>(0))?;
        for row in rows {
            ids.insert(row?);
        }
        let first: Vec<u8> = conn.query_row(
            "SELECT embedding FROM chunk_embeddings ORDER BY chunk_id LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        let query: [i8; EMBEDDING_DIM] = first
            .into_iter()
            .map(|byte| byte as i8)
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|_| anyhow!("benchmark query has wrong dimensions"))?;
        let exact_started = std::time::Instant::now();
        for _ in 0..5 {
            let exact = crate::search::vector_search(
                &conn,
                &query,
                &crate::search::SqlFilter::default(),
                50,
            )?;
            if exact.len() != 50 {
                bail!("benchmark exact query underfilled: {}", exact.len());
            }
        }
        let exact_elapsed = exact_started.elapsed();
        let query_started = std::time::Instant::now();
        for _ in 0..20 {
            let found = search_sidecar(
                Path::new(&output),
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
            let query_bytes: Vec<u8> = conn.query_row(
                "SELECT embedding FROM chunk_embeddings ORDER BY chunk_id LIMIT 1 OFFSET ?1",
                [offset as i64],
                |row| row.get(0),
            )?;
            let recall_query: [i8; EMBEDDING_DIM] = query_bytes
                .into_iter()
                .map(|byte| byte as i8)
                .collect::<Vec<_>>()
                .try_into()
                .map_err(|_| anyhow!("benchmark recall query has wrong dimensions"))?;
            let candidates = search_sidecar(
                Path::new(&output),
                &info,
                &recall_query,
                &ids,
                candidate_count,
                search_k,
            )?;
            let candidate_set = candidates
                .into_iter()
                .collect::<std::collections::HashSet<_>>();
            let mut exact = Vec::with_capacity(identity.vector_count as usize);
            let mut stmt =
                conn.prepare("SELECT chunk_id, embedding FROM chunk_embeddings ORDER BY chunk_id")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            for row in rows {
                let (chunk_id, embedding) = row?;
                exact.push((
                    chunk_id,
                    crate::semantic::dot_i8(&recall_query, &embedding)?,
                ));
            }
            exact.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let recalled = exact
                .iter()
                .take(50)
                .filter(|(chunk_id, _)| candidate_set.contains(chunk_id))
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
            "ANN_BENCH vectors={} size={} build_ms={} candidates={} search_k={} ann_query_avg_ms={:.3} exact_query_avg_ms={:.3} recall_at_50={:.3}",
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
