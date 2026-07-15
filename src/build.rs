//! Corpus build orchestrator: ingests every registered source workspace, runs
//! source-specific extraction plus shared chunking and embedding, and emits a
//! complete local immutable generation.
//! Includes checkpoint resume; corpus distribution is deliberately out of
//! scope because the hosting machine activates locally transferred builds.

use crate::chunker::CHUNKER_FORMAT_VERSION;
use crate::db::{init_db, open_write_at, set_corpus_meta};
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
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
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
        "SELECT COUNT(*) FROM embedding_cache
         WHERE model_id = ?1 AND length(embedding) != ?2",
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
    let semantic_model_paths = SemanticModelPaths::from_model_dir(model_dir)?;

    fs::create_dir_all(out_dir)
        .with_context(|| format!("creating out_dir {}", out_dir.display()))?;
    let manifest_path = out_dir.join(crate::config::GENERATION_MANIFEST_FILENAME);
    let build_state_path = out_dir.join("build-state.json");
    if manifest_path.exists() {
        if build_state_path.exists() {
            // The manifest is published before the checkpoint is removed so a
            // crash can never destroy resumability. A generation containing
            // build-state.json is not activatable; remove only that incomplete
            // publication marker and resume from the durable checkpoint.
            fs::remove_file(&manifest_path).with_context(|| {
                format!("removing interrupted manifest {}", manifest_path.display())
            })?;
            sync_parent(&manifest_path)?;
        } else {
            bail!(
                "refusing to mutate completed corpus output {}; choose a fresh output directory",
                out_dir.display()
            );
        }
    }

    // The database is the durable source-level checkpoint. A source transaction either commits
    // completely or rolls back, and every resume revalidates committed normalized documents from
    // the local source workspaces before reusing their rows.
    let expected_build_state = json!({
        "schema_version": 1,
        "corpus_schema_version": SUPPORTED_SCHEMA_VERSION,
        "embedding_model_id": EMBEDDING_MODEL_ID,
        "embedding_model_fingerprint": EMBEDDING_MODEL_FINGERPRINT,
        "embedding_dim": EMBEDDING_DIM,
        "embedding_input_max_tokens": EMBEDDING_INPUT_MAX_TOKENS,
        "chunker_format_version": CHUNKER_FORMAT_VERSION,
        "zstd_level": zstd_level,
        "source_workspaces": canonical_workspaces.iter().map(|(source, workspace)| {
            (source.as_str().to_string(), workspace.display().to_string())
        }).collect::<BTreeMap<_, _>>(),
    });
    if build_state_path.exists() {
        let actual: JsonValue = serde_json::from_slice(&fs::read(&build_state_path)?)
            .with_context(|| format!("parsing {}", build_state_path.display()))?;
        if actual != expected_build_state {
            bail!(
                "build state {} does not match this build; choose a fresh output directory",
                build_state_path.display()
            );
        }
    } else {
        if db_path.exists() {
            bail!(
                "refusing to reuse uncheckpointed corpus database {}; choose a fresh output directory",
                db_path.display()
            );
        }
        atomic_write(
            &build_state_path,
            &serde_json::to_vec_pretty(&expected_build_state)?,
        )?;
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
    let binding_tx = conn.unchecked_transaction()?;
    set_corpus_meta(&binding_tx, "index_version", &manifest.index_version)?;
    set_corpus_meta(&binding_tx, "embedding_model_id", &manifest.model.id)?;
    set_corpus_meta(&binding_tx, "last_update_at", &manifest.created_at)?;
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
}
