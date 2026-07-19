//! Corpus build orchestrator: ingests every registered source workspace, runs
//! source-specific extraction plus shared chunking and embedding, and emits a
//! complete local immutable generation.
//! Includes checkpoint resume; corpus distribution is deliberately out of
//! scope because the hosting machine activates locally transferred builds.

use crate::chunker::CHUNKER_FORMAT_VERSION;
use crate::db::{decompress_text, init_db, open_write_at, set_corpus_meta};
use crate::pipeline::finalise_source_ann;
use crate::semantic::{SemanticModelPaths, EMBEDDING_MODEL_FILES};
use crate::source::{verify_semantic_install, Manifest, ManifestDb, ManifestFile, ModelInfo};
use crate::{
    ServerState, EMBEDDING_DIM, EMBEDDING_INPUT_MAX_TOKENS, EMBEDDING_MODEL_FINGERPRINT,
    EMBEDDING_MODEL_ID, SUPPORTED_SCHEMA_VERSION,
};
use anyhow::{anyhow, bail, Context, Result};
use legal_model::SourceId;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

pub(crate) struct BuildCorpusArgs<'a> {
    pub(crate) source_workspaces: &'a BTreeMap<SourceId, PathBuf>,
    pub(crate) db_path: &'a Path,
    pub(crate) model_dir: &'a Path,
    pub(crate) embedding_cache_db: Option<&'a Path>,
    pub(crate) out_dir: &'a Path,
    pub(crate) zstd_level: i32,
    pub(crate) profile_enabled: bool,
}

const BUILD_STATE_SCHEMA_VERSION: u32 = 2;
const WORKSPACE_LOCK_FILENAME: &str = ".source-update.lock";
const WORKSPACE_STAGING_DIRECTORY: &str = "staging";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct BuildState {
    schema_version: u32,
    corpus_schema_version: u32,
    embedding_model_id: String,
    embedding_model_fingerprint: String,
    embedding_dim: usize,
    embedding_input_max_tokens: usize,
    chunker_format_version: u32,
    zstd_level: i32,
    source_workspaces: BTreeMap<String, SourceWorkspaceBinding>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceWorkspaceBinding {
    path: String,
    state: WorkspaceTreeFingerprint,
    content: WorkspaceTreeFingerprint,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceTreeFingerprint {
    files: u64,
    bytes: u64,
    sha256: String,
}

struct WorkspaceTreeHasher {
    hasher: Sha256,
    files: u64,
    bytes: u64,
}

impl WorkspaceTreeHasher {
    fn new(domain: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(domain);
        Self {
            hasher,
            files: 0,
            bytes: 0,
        }
    }

    fn hash_directory(&mut self, relative: &str) {
        self.hasher.update(b"directory\0");
        hash_fingerprint_field(&mut self.hasher, relative.as_bytes());
    }

    fn hash_file(&mut self, root: &Path, path: &Path, metadata: &fs::Metadata) -> Result<()> {
        let relative = workspace_relative_path(root, path)?;
        self.hasher.update(b"file\0");
        hash_fingerprint_field(&mut self.hasher, relative.as_bytes());
        self.hasher.update(metadata.len().to_le_bytes());

        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(path)
            .with_context(|| format!("opening source workspace file {}", path.display()))?;
        let opened = file.metadata()?;
        if !opened.is_file() || opened.len() != metadata.len() {
            bail!(
                "source workspace file changed while being opened: {}",
                path.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
                bail!(
                    "source workspace file was replaced while being opened: {}",
                    path.display()
                );
            }
        }
        let mut read_total = 0u64;
        let mut buffer = [0u8; 1024 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            self.hasher.update(&buffer[..read]);
            read_total = read_total
                .checked_add(u64::try_from(read)?)
                .ok_or_else(|| anyhow!("source workspace byte count overflow"))?;
        }
        if read_total != metadata.len() || file.metadata()?.len() != metadata.len() {
            bail!(
                "source workspace file changed while being fingerprinted: {}",
                path.display()
            );
        }
        self.files = self
            .files
            .checked_add(1)
            .ok_or_else(|| anyhow!("source workspace file count overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(read_total)
            .ok_or_else(|| anyhow!("source workspace byte count overflow"))?;
        Ok(())
    }

    fn finish(mut self) -> WorkspaceTreeFingerprint {
        self.hasher.update(b"summary\0");
        self.hasher.update(self.files.to_le_bytes());
        self.hasher.update(self.bytes.to_le_bytes());
        WorkspaceTreeFingerprint {
            files: self.files,
            bytes: self.bytes,
            sha256: format!("{:x}", self.hasher.finalize()),
        }
    }
}

fn fingerprint_source_workspaces(
    workspaces: &BTreeMap<SourceId, PathBuf>,
) -> Result<BTreeMap<String, SourceWorkspaceBinding>> {
    workspaces
        .iter()
        .map(|(source, workspace)| {
            let binding = fingerprint_source_workspace(workspace)
                .with_context(|| format!("fingerprinting source workspace `{source}`"))?;
            Ok((source.as_str().to_string(), binding))
        })
        .collect()
}

fn fingerprint_source_workspace(workspace: &Path) -> Result<SourceWorkspaceBinding> {
    let metadata = fs::symlink_metadata(workspace)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("source workspace must be a real non-symlink directory");
    }
    let canonical = workspace.canonicalize()?;
    if canonical != workspace {
        bail!(
            "source workspace path must already be canonical: {}",
            workspace.display()
        );
    }
    let path = canonical
        .to_str()
        .ok_or_else(|| anyhow!("source workspace path is not UTF-8"))?
        .to_string();
    let mut state = WorkspaceTreeHasher::new(b"australian-legal-mcp-workspace-state-v1\0");
    let mut content = WorkspaceTreeHasher::new(b"australian-legal-mcp-workspace-content-v1\0");
    for entry in sorted_directory_entries(&canonical)? {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("source workspace contains a non-UTF-8 top-level name"))?;
        let entry_path = entry.path();
        let metadata = fs::symlink_metadata(&entry_path)?;
        if metadata.file_type().is_symlink() {
            bail!(
                "source workspace contains a symlink: {}",
                entry_path.display()
            );
        }
        if name == WORKSPACE_LOCK_FILENAME {
            if !metadata.is_file() {
                bail!("source workspace lock path is not a regular file");
            }
            continue;
        }
        if name == WORKSPACE_STAGING_DIRECTORY {
            if !metadata.is_dir() {
                bail!("source workspace staging path is not a directory");
            }
            continue;
        }
        if metadata.is_file() {
            state.hash_file(&canonical, &entry_path, &metadata)?;
        } else if metadata.is_dir() {
            content.hash_directory(&workspace_relative_path(&canonical, &entry_path)?);
            fingerprint_content_directory(&canonical, &entry_path, &mut content)?;
        } else {
            bail!(
                "source workspace contains a special file: {}",
                entry_path.display()
            );
        }
    }
    Ok(SourceWorkspaceBinding {
        path,
        state: state.finish(),
        content: content.finish(),
    })
}

fn fingerprint_content_directory(
    root: &Path,
    directory: &Path,
    hasher: &mut WorkspaceTreeHasher,
) -> Result<()> {
    for entry in sorted_directory_entries(directory)? {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            bail!(
                "source workspace content contains a symlink: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            hasher.hash_directory(&workspace_relative_path(root, &path)?);
            fingerprint_content_directory(root, &path, hasher)?;
        } else if metadata.is_file() {
            hasher.hash_file(root, &path, &metadata)?;
        } else {
            bail!(
                "source workspace content contains a special file: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn sorted_directory_entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn workspace_relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(root)?;
    let components = relative
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("source workspace contains a non-UTF-8 path"))
        })
        .collect::<Result<Vec<_>>>()?;
    if components.is_empty() {
        bail!("source workspace fingerprint path is empty");
    }
    Ok(components.join("/"))
}

fn hash_fingerprint_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn validate_or_create_build_state(
    build_state_path: &Path,
    db_path: &Path,
    expected: &BuildState,
) -> Result<()> {
    match fs::symlink_metadata(build_state_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("build state must be a regular non-symlink file");
            }
            if metadata.len() == 0 || metadata.len() > 16 * 1024 * 1024 {
                bail!("build state is empty or exceeds its size cap");
            }
            let actual: BuildState = serde_json::from_slice(&fs::read(build_state_path)?)
                .with_context(|| format!("parsing {}", build_state_path.display()))?;
            if &actual != expected {
                bail!(
                    "build state {} does not match this build or its exact source workspace bytes; choose a fresh output directory",
                    build_state_path.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if db_path.exists() {
                bail!(
                    "refusing to reuse uncheckpointed corpus database {}; choose a fresh output directory",
                    db_path.display()
                );
            }
            atomic_write(build_state_path, &serde_json::to_vec_pretty(expected)?)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn seed_embedding_cache(
    target: &Connection,
    target_path: &Path,
    seed_path: &Path,
) -> Result<usize> {
    let metadata = fs::symlink_metadata(seed_path)
        .with_context(|| format!("reading embedding cache seed {}", seed_path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("embedding cache seed must be a real SQLite file");
    }
    let seed_path = seed_path.canonicalize()?;
    let target_path = target_path.canonicalize()?;
    if seed_path == target_path {
        bail!("embedding cache seed must differ from the fresh target database");
    }
    let seed = Connection::open_with_flags(
        &seed_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    crate::db::enforce_db_schema_version(&seed)?;
    let model_id = seed
        .query_row(
            "SELECT value FROM corpus_meta WHERE key = 'embedding_model_id'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("embedding cache seed has no completed model binding"))?;
    if model_id != EMBEDDING_MODEL_ID {
        bail!("embedding cache seed model `{model_id}` does not match `{EMBEDDING_MODEL_ID}`");
    }
    let invalid: i64 = seed.query_row(
        "SELECT
             (SELECT COUNT(*) FROM embedding_cache
              WHERE model_id = ?1 AND length(embedding) != ?2)
           + (SELECT COUNT(*) FROM chunk_embeddings
              WHERE length(embedding) != ?2)",
        rusqlite::params![EMBEDDING_MODEL_ID, EMBEDDING_DIM as i64],
        |row| row.get(0),
    )?;
    if invalid != 0 {
        bail!("embedding cache seed contains {invalid} invalid vectors");
    }
    drop(seed);

    let seed_utf8 = seed_path
        .to_str()
        .ok_or_else(|| anyhow!("embedding cache seed path is not UTF-8"))?;
    let before: i64 =
        target.query_row("SELECT COUNT(*) FROM embedding_cache", [], |row| row.get(0))?;
    target.execute("ATTACH DATABASE ?1 AS embedding_seed", [seed_utf8])?;
    let copy_result = target.execute(
        "INSERT OR IGNORE INTO main.embedding_cache(model_id, text_sha256, embedding)
         SELECT model_id, text_sha256, embedding
         FROM embedding_seed.embedding_cache
         WHERE model_id = ?1",
        [EMBEDDING_MODEL_ID],
    );
    let detach_result = target.execute_batch("DETACH DATABASE embedding_seed;");
    copy_result?;
    detach_result?;

    // Runtime generations intentionally clear their disposable embedding
    // cache. Reconstruct exact cache keys from their authoritative chunk text
    // and vectors so a chunker/source-only rebuild reuses every unchanged
    // embedding without model execution.
    target.execute("ATTACH DATABASE ?1 AS embedding_seed", [seed_utf8])?;
    let authoritative_copy = (|| -> Result<()> {
        let transaction = target.unchecked_transaction()?;
        {
            let mut select = transaction.prepare(
                "SELECT c.text, e.embedding
                 FROM embedding_seed.chunks AS c
                 JOIN embedding_seed.chunk_embeddings AS e
                   ON e.chunk_id = c.chunk_id
                 ORDER BY c.chunk_id",
            )?;
            let mut insert = transaction.prepare(
                "INSERT OR IGNORE INTO main.embedding_cache(model_id, text_sha256, embedding)
                 VALUES (?1, ?2, ?3)",
            )?;
            let mut rows = select.query([])?;
            while let Some(row) = rows.next()? {
                let compressed = row.get::<_, Vec<u8>>(0)?;
                let embedding = row.get::<_, Vec<u8>>(1)?;
                if embedding.len() != EMBEDDING_DIM {
                    bail!(
                        "embedding cache seed contains an authoritative vector with length {}, expected {EMBEDDING_DIM}",
                        embedding.len()
                    );
                }
                let text = decompress_text(compressed)?;
                let text_sha256 = format!("{:x}", Sha256::digest(text.as_bytes()));
                insert.execute(rusqlite::params![
                    EMBEDDING_MODEL_ID,
                    text_sha256,
                    embedding
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    })();
    let authoritative_detach = target.execute_batch("DETACH DATABASE embedding_seed;");
    authoritative_copy?;
    authoritative_detach?;
    let after: i64 =
        target.query_row("SELECT COUNT(*) FROM embedding_cache", [], |row| row.get(0))?;
    usize::try_from(after.saturating_sub(before)).context("embedding cache seed count overflow")
}

pub(crate) fn build_corpus(args: BuildCorpusArgs<'_>) -> Result<()> {
    let BuildCorpusArgs {
        source_workspaces,
        db_path,
        model_dir,
        embedding_cache_db,
        out_dir,
        zstd_level,
        profile_enabled,
    } = args;
    let build_started = std::time::Instant::now();

    let registered_sources = crate::legal_source::source_registry()
        .source_ids()
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let supplied_sources = source_workspaces
        .keys()
        .map(|source| source.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    if supplied_sources != registered_sources {
        let missing = registered_sources
            .difference(&supplied_sources)
            .cloned()
            .collect::<Vec<_>>();
        let unexpected = supplied_sources
            .difference(&registered_sources)
            .cloned()
            .collect::<Vec<_>>();
        bail!("source workspace set mismatch; missing {missing:?}, unexpected {unexpected:?}");
    }

    let mut canonical_workspaces = BTreeMap::new();
    let mut seen_paths = BTreeSet::new();
    for (source, workspace) in source_workspaces {
        let canonical = workspace.canonicalize().with_context(|| {
            format!("canonicalizing {source} workspace {}", workspace.display())
        })?;
        if !seen_paths.insert(canonical.clone()) {
            bail!("source workspaces must be distinct directories");
        }
        canonical_workspaces.insert(source.clone(), canonical);
    }
    let _workspace_locks = canonical_workspaces
        .values()
        .map(|workspace| crate::source_update::lock_workspace_shared(workspace))
        .collect::<Result<Vec<_>>>()?;
    let source_workspace_bindings = fingerprint_source_workspaces(&canonical_workspaces)?;
    let semantic_model_paths = SemanticModelPaths::from_model_dir(model_dir)?;

    fs::create_dir_all(out_dir)
        .with_context(|| format!("creating out_dir {}", out_dir.display()))?;
    let manifest_path = out_dir.join(crate::config::GENERATION_MANIFEST_FILENAME);
    let build_state_path = out_dir.join("build-state.json");
    let expected_build_state = BuildState {
        schema_version: BUILD_STATE_SCHEMA_VERSION,
        corpus_schema_version: SUPPORTED_SCHEMA_VERSION,
        embedding_model_id: EMBEDDING_MODEL_ID.to_string(),
        embedding_model_fingerprint: EMBEDDING_MODEL_FINGERPRINT.to_string(),
        embedding_dim: EMBEDDING_DIM,
        embedding_input_max_tokens: EMBEDDING_INPUT_MAX_TOKENS,
        chunker_format_version: CHUNKER_FORMAT_VERSION,
        zstd_level,
        source_workspaces: source_workspace_bindings.clone(),
    };
    if manifest_path.exists() && !build_state_path.exists() {
        bail!(
            "refusing to mutate completed corpus output {}; choose a fresh output directory",
            out_dir.display()
        );
    }
    validate_or_create_build_state(&build_state_path, db_path, &expected_build_state)?;
    if manifest_path.exists() {
        // The manifest is published before the checkpoint is removed so a
        // crash can never destroy resumability. A generation containing
        // build-state.json is not activatable; remove only that incomplete
        // publication marker after the exact source fingerprints match.
        fs::remove_file(&manifest_path).with_context(|| {
            format!("removing interrupted manifest {}", manifest_path.display())
        })?;
        sync_parent(&manifest_path)?;
    }

    let mut conn = open_write_at(db_path)
        .with_context(|| format!("opening sqlite at {}", db_path.display()))?;
    init_db(&conn)?;
    let model_tx = conn.unchecked_transaction()?;
    if let Some(stored_model) = crate::db::get_corpus_meta(&model_tx, "embedding_model_id")? {
        if stored_model != EMBEDDING_MODEL_ID {
            bail!(
                "checkpoint database uses embedding model `{stored_model}`, expected `{EMBEDDING_MODEL_ID}`"
            );
        }
    }
    set_corpus_meta(&model_tx, "embedding_model_id", EMBEDDING_MODEL_ID)?;
    model_tx.commit()?;
    if let Some(seed_path) = embedding_cache_db {
        let imported = seed_embedding_cache(&conn, db_path, seed_path)?;
        eprintln!("legal-mcp build: imported {imported} reusable embeddings");
    }

    let state = ServerState::with_model_paths(semantic_model_paths);
    let (ann_sender, ann_receiver) = std::sync::mpsc::channel::<SourceId>();
    let ann_db_path = db_path.to_path_buf();
    let ann_output = out_dir.to_path_buf();
    let ann_started = std::time::Instant::now();
    eprintln!("legal-mcp build: starting overlapped deterministic ANN worker…");
    let ann_handle = std::thread::spawn(
        move || -> Result<Vec<(SourceId, crate::ann::ManifestAnn)>> {
            let mut results = Vec::new();
            for source in ann_receiver {
                let source_started = std::time::Instant::now();
                let source_conn = Connection::open_with_flags(
                    &ann_db_path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .with_context(|| format!("opening ANN read connection for {source}"))?;
                source_conn
                    .busy_timeout(std::time::Duration::from_secs(60))
                    .with_context(|| format!("configuring ANN read timeout for {source}"))?;
                let info = finalise_source_ann(&source_conn, &source, &ann_output)?;
                eprintln!(
                    "legal-mcp build: {source} ANN completed in {:.1}s",
                    source_started.elapsed().as_secs_f64()
                );
                results.push((source, info));
            }
            Ok(results)
        },
    );
    let mut build_workspaces = canonical_workspaces
        .iter()
        .map(|(source, workspace)| {
            let documents = crate::source_catalog::normalized_document_results(source, workspace)?;
            let (minimum, maximum) = documents.size_hint();
            let count = maximum
                .filter(|maximum| *maximum == minimum)
                .ok_or_else(|| {
                    anyhow!("source `{source}` normalized inventory has no exact size hint")
                })?;
            Ok((source, workspace, count))
        })
        .collect::<Result<Vec<_>>>()?;
    // Build the largest source first. Its ANN sidecar can then consume the
    // committed rows while the remaining sources are ingested, instead of
    // becoming a long serial tail after the final large source.
    build_workspaces.sort_by(
        |(left_source, _, left_count), (right_source, _, right_count)| {
            right_count
                .cmp(left_count)
                .then_with(|| left_source.cmp(right_source))
        },
    );
    eprintln!(
        "legal-mcp build: source order by document count: {}",
        build_workspaces
            .iter()
            .map(|(source, _, count)| format!("{source}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let ingestion_result = (|| -> Result<()> {
        for (source, workspace, _) in build_workspaces {
            let source_started = std::time::Instant::now();
            eprintln!("legal-mcp build: loading normalized {source} documents…");
            let documents = crate::source_catalog::normalized_document_results(source, workspace)?;
            let descriptor = crate::legal_source::source_registry()
                .source(source)?
                .descriptor()
                .clone();
            let report = crate::pipeline::ingest_source_results(
                &mut conn,
                source,
                &descriptor,
                documents,
                &state,
            )?;
            let source_elapsed = source_started.elapsed().as_secs_f64();
            eprintln!(
                "legal-mcp build: {source} committed {} documents and {} chunks in {source_elapsed:.1}s ({} changed, {} removed, {} embeddings encoded, {} reused; total {:.1}s)",
                report.inserted_documents
                    + report.changed_documents
                    + report.unchanged_documents,
                report.inserted_chunks,
                report.changed_documents,
                report.deleted_documents,
                report.encoded_texts,
                report.reused_embeddings,
                build_started.elapsed().as_secs_f64(),
            );
            ann_sender
                .send(source.clone())
                .map_err(|_| anyhow!("ANN worker stopped before source `{source}` was queued"))?;
        }
        Ok(())
    })();
    drop(ann_sender);
    let ann_result = ann_handle
        .join()
        .map_err(|_| anyhow!("ANN worker panicked"))?;
    ingestion_result?;
    let ann_results = ann_result?;
    eprintln!(
        "legal-mcp build: all ANN sidecars ready after {:.1}s",
        ann_started.elapsed().as_secs_f64()
    );

    let created_at = chrono::Utc::now().to_rfc3339();
    let mut manifest = Manifest {
        schema_version: SUPPORTED_SCHEMA_VERSION,
        index_version: chrono::Utc::now().format("%Y.%m.%d").to_string(),
        created_at,
        min_client_version: env!("CARGO_PKG_VERSION").to_string(),
        model: ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            fingerprint: EMBEDDING_MODEL_FINGERPRINT.to_string(),
            model: ManifestFile {
                path: EMBEDDING_MODEL_FILES[0].output_name.to_string(),
                sha256: EMBEDDING_MODEL_FILES[0].sha256.to_string(),
                size: EMBEDDING_MODEL_FILES[0].size,
            },
            tokenizer: ManifestFile {
                path: EMBEDDING_MODEL_FILES[1].output_name.to_string(),
                sha256: EMBEDDING_MODEL_FILES[1].sha256.to_string(),
                size: EMBEDDING_MODEL_FILES[1].size,
            },
        },
        db: ManifestDb {
            path: crate::config::LEGAL_DB_FILENAME.to_string(),
            sha256: String::new(),
            size: 0,
        },
        ann: BTreeMap::new(),
    };

    // Bind corpus-wide immutable metadata only after every source transaction commits.
    let chunks_fts_index_sha256 = crate::db::chunks_fts_index_sha256(&conn, "chunks_fts")?;
    let binding_tx = conn.unchecked_transaction()?;
    set_corpus_meta(&binding_tx, "index_version", &manifest.index_version)?;
    set_corpus_meta(&binding_tx, "embedding_model_id", &manifest.model.id)?;
    set_corpus_meta(&binding_tx, "last_update_at", &manifest.created_at)?;
    set_corpus_meta(
        &binding_tx,
        "chunks_fts_index_sha256",
        &chunks_fts_index_sha256,
    )?;
    let documents_count: i64 =
        binding_tx.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))?;
    let chunks_count: i64 =
        binding_tx.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;
    let chunk_embeddings_count: i64 =
        binding_tx.query_row("SELECT COUNT(*) FROM chunk_embeddings", [], |row| {
            row.get(0)
        })?;
    let definitions_count: i64 =
        binding_tx.query_row("SELECT COUNT(*) FROM definitions", [], |row| row.get(0))?;
    for (key, value) in [
        ("documents_count", documents_count),
        ("chunks_count", chunks_count),
        ("chunk_embeddings_count", chunk_embeddings_count),
        ("definitions_count", definitions_count),
    ] {
        set_corpus_meta(&binding_tx, key, &value.to_string())?;
    }
    binding_tx.commit()?;

    for (source, info) in ann_results {
        manifest.ann.insert(source, info);
    }

    // The exact-text cache accelerates this build only. It duplicates stored
    // chunk vectors and must not consume host runtime storage.
    conn.execute("DELETE FROM embedding_cache", [])?;
    conn.execute_batch("VACUUM")?;
    crate::source::verify_corpus_manifest_binding(&conn, &manifest)?;
    verify_semantic_install(&conn, &manifest)?;
    verify_final_build_fts(&conn)?;
    let final_workspace_bindings = fingerprint_source_workspaces(&canonical_workspaces)?;
    if final_workspace_bindings != source_workspace_bindings {
        bail!(
            "source workspace state or content changed during the build; the checkpoint remains resumable only from its original exact bytes"
        );
    }

    for file in EMBEDDING_MODEL_FILES {
        let source = model_dir.join(file.path);
        let destination = out_dir.join(file.output_name);
        install_model_file(&source, &destination, file.size, file.sha256)?;
    }

    File::open(db_path)?.sync_all()?;
    drop(conn);
    let journal = db_path.with_extension("db-journal");
    if journal.exists() {
        if fs::metadata(&journal)?.len() != 0 {
            bail!("completed database retained a nonempty rollback journal");
        }
        fs::remove_file(&journal)?;
    }
    manifest.db.size = fs::metadata(db_path)?.len();
    manifest.db.sha256 = sha256_file(db_path)?;
    crate::source::validate_manifest(&manifest)?;
    atomic_write(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)?;
    fs::remove_file(&build_state_path)
        .with_context(|| format!("removing {}", build_state_path.display()))?;
    sync_parent(&build_state_path)?;

    let total_elapsed = build_started.elapsed().as_secs_f64();
    if profile_enabled {
        eprintln!(
            "legal-mcp build profile: documents={documents_count} chunks={chunks_count} embeddings={chunk_embeddings_count} definitions={definitions_count} total_s={total_elapsed:.2}"
        );
    }
    eprintln!("legal-mcp build: wrote {}", manifest_path.display());
    eprintln!(
        "legal-mcp build: done - {documents_count} docs written to {} in {total_elapsed:.1}s",
        db_path.display()
    );
    Ok(())
}

fn verify_final_build_fts(conn: &Connection) -> Result<()> {
    crate::db::validate_chunks_fts_schema(conn)?;
    crate::db::verify_chunks_fts_index_digest(conn)?;
    // This includes exact FTS/source row identity and non-overlapping source
    // rowid partitions for both chunk and title indexes.
    crate::db::verify_fts_relational_bindings(conn)?;
    let foreign_keys: i64 =
        conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_keys != 0 {
        bail!("completed build has {foreign_keys} foreign-key violations");
    }
    crate::db::verify_fts_integrity(conn)?;
    Ok(())
}

fn install_model_file(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<()> {
    let source_metadata = fs::symlink_metadata(source)
        .with_context(|| format!("reading model input {}", source.display()))?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
        bail!(
            "model input must be a regular non-symlink: {}",
            source.display()
        );
    }
    if source_metadata.len() != expected_size || sha256_file(source)? != expected_sha256 {
        bail!(
            "model input does not match its pinned size and SHA-256: {}",
            source.display()
        );
    }

    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() != expected_size
                || sha256_file(destination)? != expected_sha256
            {
                bail!(
                    "completed generation contains a conflicting {}",
                    destination.display()
                );
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("model destination has no Unicode file name"))?;
    let temporary = destination.with_file_name(format!(".{file_name}.copying"));
    if let Ok(metadata) = fs::symlink_metadata(&temporary) {
        if metadata.is_dir() {
            bail!(
                "model copy temporary path is a directory: {}",
                temporary.display()
            );
        }
        fs::remove_file(&temporary)?;
    }
    let copy_result = (|| -> Result<()> {
        fs::copy(source, &temporary).with_context(|| {
            format!(
                "copying model file into generation: {}",
                destination.display()
            )
        })?;
        let metadata = fs::symlink_metadata(&temporary)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() != expected_size
            || sha256_file(&temporary)? != expected_sha256
        {
            bail!("copied model file failed its pinned size or SHA-256 check");
        }
        File::open(&temporary)?.sync_all()?;
        fs::rename(&temporary, destination)?;
        sync_parent(destination)?;
        Ok(())
    })();
    if copy_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    copy_result
}

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read as _;
    let mut hasher = Sha256::new();
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)
            .with_context(|| format!("opening {} for sync", parent.display()))?
            .sync_all()
            .with_context(|| format!("syncing {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::config::atomic_write(path, bytes)
}

#[cfg(test)]
mod security_tests {
    use super::*;

    fn fixture_build_state(source: &str, binding: SourceWorkspaceBinding) -> BuildState {
        BuildState {
            schema_version: BUILD_STATE_SCHEMA_VERSION,
            corpus_schema_version: SUPPORTED_SCHEMA_VERSION,
            embedding_model_id: EMBEDDING_MODEL_ID.to_string(),
            embedding_model_fingerprint: EMBEDDING_MODEL_FINGERPRINT.to_string(),
            embedding_dim: EMBEDDING_DIM,
            embedding_input_max_tokens: EMBEDDING_INPUT_MAX_TOKENS,
            chunker_format_version: CHUNKER_FORMAT_VERSION,
            zstd_level: 3,
            source_workspaces: BTreeMap::from([(source.to_string(), binding)]),
        }
    }

    #[test]
    fn atomic_write_replaces_only_with_complete_bytes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("generation.json");
        fs::write(&path, b"old")?;
        atomic_write(&path, b"new complete value")?;
        assert_eq!(fs::read(&path)?, b"new complete value");
        Ok(())
    }

    #[test]
    fn fresh_build_can_seed_only_model_bound_embedding_cache_rows() -> Result<()> {
        let root = tempfile::tempdir()?;
        let seed_path = root.path().join("seed.db");
        let target_path = root.path().join("target.db");
        let seed = open_write_at(&seed_path)?;
        init_db(&seed)?;
        set_corpus_meta(&seed, "embedding_model_id", EMBEDDING_MODEL_ID)?;
        seed.execute(
            "INSERT INTO embedding_cache(model_id, text_sha256, embedding)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![
                EMBEDDING_MODEL_ID,
                "1".repeat(64),
                vec![1_u8; EMBEDDING_DIM]
            ],
        )?;
        drop(seed);

        let target = open_write_at(&target_path)?;
        init_db(&target)?;
        assert_eq!(seed_embedding_cache(&target, &target_path, &seed_path)?, 1);
        assert_eq!(seed_embedding_cache(&target, &target_path, &seed_path)?, 0);
        assert_eq!(
            target.query_row("SELECT COUNT(*) FROM embedding_cache", [], |row| {
                row.get::<_, i64>(0)
            })?,
            1
        );
        Ok(())
    }

    #[test]
    fn runtime_generation_vectors_seed_cache_by_exact_chunk_text() -> Result<()> {
        let root = tempfile::tempdir()?;
        let seed_path = root.path().join("seed.db");
        let target_path = root.path().join("target.db");
        let seed = open_write_at(&seed_path)?;
        init_db(&seed)?;
        set_corpus_meta(&seed, "embedding_model_id", EMBEDDING_MODEL_ID)?;
        seed.execute(
            "INSERT INTO sources(source_id, display_name) VALUES ('ato', 'ATO')",
            [],
        )?;
        seed.execute(
            "INSERT INTO documents(
                 source_id, native_id, type, title, canonical_url, downloaded_at,
                 content_hash, html
             ) VALUES ('ato', 'DOC', 'TXR', 'Document',
                 'https://example.invalid/doc', '2026-01-01T00:00:00Z', 'hash', X'00')",
            [],
        )?;
        let text = "authoritative unchanged chunk";
        seed.execute(
            "INSERT INTO chunks(chunk_id, source_id, native_id, ord, text)
             VALUES (1, 'ato', 'DOC', 0, ?1)",
            [crate::db::compress_text(text)?],
        )?;
        seed.execute(
            "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (1, ?1)",
            [vec![7_u8; EMBEDDING_DIM]],
        )?;
        drop(seed);

        let target = open_write_at(&target_path)?;
        init_db(&target)?;
        assert_eq!(seed_embedding_cache(&target, &target_path, &seed_path)?, 1);
        let expected_hash = format!("{:x}", Sha256::digest(text.as_bytes()));
        let stored: (String, Vec<u8>) = target.query_row(
            "SELECT text_sha256, embedding FROM embedding_cache
             WHERE model_id = ?1",
            [EMBEDDING_MODEL_ID],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(stored, (expected_hash, vec![7_u8; EMBEDDING_DIM]));
        Ok(())
    }

    #[test]
    fn interrupted_resume_rejects_changed_workspace_state_or_content() -> Result<()> {
        let root = tempfile::tempdir()?;
        let workspace = root.path().join("workspace");
        let documents = workspace.join("documents");
        fs::create_dir_all(&documents)?;
        fs::write(
            workspace.join(WORKSPACE_LOCK_FILENAME),
            b"lock bytes are excluded",
        )?;
        fs::write(workspace.join("state.json"), b"{\"version\":1}\n")?;
        fs::write(documents.join("one.json"), b"{\"body\":\"one\"}\n")?;

        let original_binding = fingerprint_source_workspace(&workspace.canonicalize()?)?;
        let original = fixture_build_state("ato", original_binding.clone());
        let checkpoint = root.path().join("build-state.json");
        let database = root.path().join(crate::config::LEGAL_DB_FILENAME);
        validate_or_create_build_state(&checkpoint, &database, &original)?;
        fs::write(&database, b"interrupted SQLite checkpoint")?;

        fs::write(
            workspace.join(WORKSPACE_LOCK_FILENAME),
            b"a different lock file is still excluded",
        )?;
        assert_eq!(
            fingerprint_source_workspace(&workspace.canonicalize()?)?,
            original_binding
        );
        validate_or_create_build_state(&checkpoint, &database, &original)?;

        fs::write(documents.join("one.json"), b"{\"body\":\"changed\"}\n")?;
        let changed_content = fixture_build_state(
            "ato",
            fingerprint_source_workspace(&workspace.canonicalize()?)?,
        );
        let error = validate_or_create_build_state(&checkpoint, &database, &changed_content)
            .expect_err("changed document bytes must reject resume");
        assert!(error.to_string().contains("exact source workspace bytes"));

        fs::write(documents.join("one.json"), b"{\"body\":\"one\"}\n")?;
        fs::write(workspace.join("state.json"), b"{\"version\":2}\n")?;
        let changed_state = fixture_build_state(
            "ato",
            fingerprint_source_workspace(&workspace.canonicalize()?)?,
        );
        assert!(validate_or_create_build_state(&checkpoint, &database, &changed_state).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn workspace_fingerprint_rejects_symlinked_content() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace)?;
        fs::write(workspace.join("state.json"), b"{}")?;
        let target = root.path().join("target");
        fs::write(&target, b"content")?;
        symlink(&target, workspace.join("documents"))?;
        assert!(fingerprint_source_workspace(&workspace.canonicalize()?).is_err());
        Ok(())
    }

    #[test]
    fn final_build_gate_rejects_overlapping_source_partitions() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        init_db(&conn)?;
        conn.execute_batch(
            "INSERT INTO sources(source_id, display_name) VALUES ('ato', 'ATO'), ('frl', 'FRL');
             INSERT INTO documents(
                 source_id, native_id, type, title, canonical_url, downloaded_at,
                 content_hash, html
             ) VALUES
                 ('ato', 'a', 'fixture', 'A', 'https://example.invalid/a', '2026-01-01T00:00:00Z', 'a', X'00'),
                 ('frl', 'b', 'fixture', 'B', 'https://example.invalid/b', '2026-01-01T00:00:00Z', 'b', X'00'),
                 ('ato', 'c', 'fixture', 'C', 'https://example.invalid/c', '2026-01-01T00:00:00Z', 'c', X'00');
             INSERT INTO chunks(chunk_id, source_id, native_id, ord, text) VALUES
                 (1, 'ato', 'a', 0, X'00'),
                 (2, 'frl', 'b', 0, X'00'),
                 (3, 'ato', 'c', 0, X'00');
             INSERT INTO chunks_fts(rowid, text) VALUES (1, 'a'), (2, 'b'), (3, 'c');
             INSERT INTO title_fts(rowid, source_id, native_id, title, headings) VALUES
                 (1, 'ato', 'a', 'A', ''),
                 (2, 'frl', 'b', 'B', ''),
                 (3, 'ato', 'c', 'C', '');",
        )?;
        let digest = crate::db::chunks_fts_index_sha256(&conn, "chunks_fts")?;
        set_corpus_meta(&conn, "chunks_fts_index_sha256", &digest)?;
        let error = verify_final_build_fts(&conn).unwrap_err();
        assert!(error.to_string().contains("overlap rowid ranges"));
        Ok(())
    }
}
