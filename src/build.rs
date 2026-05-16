//! Corpus build orchestrator: walks ato_pages/index.jsonl, runs cleaning +
//! chunker + rules-engine + embedder pipeline in-process, writes documents/
//! chunks/embeddings/anchors/definitions/citations, manifest, ato.db.zst, and
//! update.json. Plus checkpoint resume and `bundle_localize_manifest` for
//! offline bundles.

use crate::chunker::{chunk_html, Chunk, CHUNKER_FORMAT_VERSION, EMBED_MAX_TOKENS};
use crate::db::{
    compress_text, init_db, open_write_at, set_meta,
};
use crate::extract::{
    anchors_node_text, extract_anchors, extract_compose_title, extract_currency,
    extract_definitions, extract_em_front_matter, extract_leading_headings, metadata_content_hash,
    metadata_doc_id_for, metadata_extract_pub_date, metadata_parse_docid, rewrite_images_html, AnchorRef, CurrencyInfo,
    DefinitionChunk, ExtractedAsset,
};
use crate::html::{clean_ato_html, normalise_named_anchors, rewrite_links_html, strip_attributes};
use crate::retrieval::derive_citations;
use crate::rules::{derive_metadata, RuleInputs};
use crate::semantic::{
    SemanticEncodeStats, SemanticModelPaths,
};
use crate::source::{
    manifest_fingerprint, verify_semantic_install, Manifest, ManifestDb, ModelInfo, UpdateSummary,
};

// BuildCheckpoint carries minimal per-doc records purely for resume tracking.
// The original pack-based fields are kept as opaque JSON to stay
// forward-compatible with any pre-v0.13 partial checkpoints sitting on disk.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocRef {
    pub(crate) doc_id: String,
    #[serde(default)]
    pub(crate) content_hash: String,
    #[serde(default)]
    pub(crate) pack_sha8: String,
    #[serde(default)]
    pub(crate) offset: u64,
    #[serde(default)]
    pub(crate) length: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackInfo {
    #[serde(default)]
    pub(crate) sha8: String,
    #[serde(default)]
    pub(crate) sha256: String,
    #[serde(default)]
    pub(crate) size: u64,
    #[serde(default)]
    pub(crate) url: String,
}
use crate::{
    enforce_db_schema_version, ServerState, EMBEDDING_DIM,
    EMBEDDING_INPUT_MAX_TOKENS, EMBEDDING_MODEL_FINGERPRINT, EMBEDDING_MODEL_HF_SIZE,
    EMBEDDING_MODEL_HF_URL, EMBEDDING_MODEL_ID, SUPPORTED_MANIFEST_VERSION,
};
use rusqlite::OpenFlags;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) const BUILD_EMBED_BATCH_SIZE: usize = 32;
pub(crate) const BUILD_EMBED_PENDING_FLUSH_CHUNKS: usize = 4096;
pub(crate) const BUILD_CHECKPOINT_SCHEMA_VERSION: u32 = 2;

// ----- Build orchestrator (port of src/ato_mcp/indexer/build.py) -----
//
// Walks pages_dir/index.jsonl, runs each doc through the cleaning + chunker
// + rules-engine metadata classifier + embedder pipeline in-process, writes
// documents + chunks + chunk_embeddings + chunks_fts + title_fts +
// doc_anchors + definitions + citations rows, then writes pack files,
// asset blobs, manifest.json, and update.json to --out-dir. Missing vs
// build.py: release seeding, checkpoint resume, parallelism.

pub(crate) struct PendingBuildEmbedding {
    pub(crate) chunk_id: i64,
    pub(crate) text: String,
}

#[derive(Default)]
pub(crate) struct BuildProfile {
    pub(crate) enabled: bool,
    pub(crate) started_at: Option<std::time::Instant>,
    pub(crate) docs: usize,
    pub(crate) chunks: usize,
    pub(crate) html_bytes: u64,
    pub(crate) read: Duration,
    pub(crate) clean: Duration,
    pub(crate) metadata: Duration,
    pub(crate) chunking: Duration,
    pub(crate) references: Duration,
    pub(crate) sqlite: Duration,
    pub(crate) assets: Duration,
    pub(crate) embedding: Duration,
    pub(crate) embedding_tokenize: Duration,
    pub(crate) embedding_prepare: Duration,
    pub(crate) embedding_run: Duration,
    pub(crate) embedding_postprocess: Duration,
    pub(crate) embedding_write: Duration,
    pub(crate) embedding_batches: usize,
    pub(crate) embedding_inputs: usize,
    pub(crate) embedding_active_tokens: usize,
    pub(crate) embedding_padded_tokens: usize,
    pub(crate) embedding_max_batch: usize,
    pub(crate) embedding_max_seq_len: usize,
    pub(crate) pack: Duration,
    pub(crate) checkpoint: Duration,
    pub(crate) finalise: Duration,
}

impl BuildProfile {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            started_at: enabled.then(std::time::Instant::now),
            ..Self::default()
        }
    }

    fn elapsed(&self) -> Duration {
        self.started_at
            .map(|started| started.elapsed())
            .unwrap_or_default()
    }

    fn print(&self) {
        if !self.enabled {
            return;
        }
        // [IB-19] --profile reports stage timing plus embedding batch,
        // token, padding, and model-throughput counters for build tuning.
        let total = self.elapsed().as_secs_f64().max(0.000_001);
        eprintln!(
            "ato-mcp build profile: docs={} chunks={} html_mb={:.1} total_s={:.2} docs_per_s={:.2}",
            self.docs,
            self.chunks,
            self.html_bytes as f64 / (1024.0 * 1024.0),
            total,
            self.docs as f64 / total
        );
        let rows = [
            ("read", self.read),
            ("clean", self.clean),
            ("metadata", self.metadata),
            ("chunking", self.chunking),
            ("references", self.references),
            ("sqlite", self.sqlite),
            ("assets", self.assets),
            ("embedding", self.embedding),
            ("pack", self.pack),
            ("checkpoint", self.checkpoint),
            ("finalise", self.finalise),
        ];
        for (name, duration) in rows {
            let secs = duration.as_secs_f64();
            eprintln!("  {name:>10}: {secs:>8.2}s {:>5.1}%", secs * 100.0 / total);
        }
        if self.embedding_batches > 0 {
            let model_secs = self.embedding_run.as_secs_f64().max(0.000_001);
            let padding_ratio = if self.embedding_padded_tokens == 0 {
                0.0
            } else {
                self.embedding_active_tokens as f64 / self.embedding_padded_tokens as f64
            };
            eprintln!(
                "  embedding batches={} inputs={} active_tokens={} padded_tokens={} padding_efficiency={:.1}% max_batch={} max_seq_len={} model_tokens_per_s={:.0}",
                self.embedding_batches,
                self.embedding_inputs,
                self.embedding_active_tokens,
                self.embedding_padded_tokens,
                padding_ratio * 100.0,
                self.embedding_max_batch,
                self.embedding_max_seq_len,
                self.embedding_padded_tokens as f64 / model_secs,
            );
            let rows = [
                ("embed_tok", self.embedding_tokenize),
                ("embed_prep", self.embedding_prepare),
                ("embed_run", self.embedding_run),
                ("embed_post", self.embedding_postprocess),
                ("embed_write", self.embedding_write),
            ];
            for (name, duration) in rows {
                let secs = duration.as_secs_f64();
                eprintln!("  {name:>10}: {secs:>8.2}s {:>5.1}%", secs * 100.0 / total);
            }
        }
    }
}

pub(crate) fn maybe_report_build_progress(
    processed: usize,
    rebuilt: usize,
    reused: usize,
    started_at: std::time::Instant,
) {
    if processed > 0 && processed.is_multiple_of(1000) {
        let elapsed = started_at.elapsed().as_secs_f64().max(0.000_001);
        eprintln!(
            "ato-mcp build: processed {processed} source docs ({:.1}/s, rebuilt {rebuilt}, reused {reused})",
            processed as f64 / elapsed
        );
    }
}

pub(crate) fn is_batch_allocation_failure(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    msg.contains("failed to allocate memory") || msg.contains("out of memory")
}

pub(crate) fn encode_build_embeddings_adaptive(
    state: &ServerState,
    inputs: &[String],
) -> Result<(Vec<[i8; EMBEDDING_DIM]>, SemanticEncodeStats)> {
    if inputs.is_empty() {
        return Ok((Vec::new(), SemanticEncodeStats::default()));
    }
    match state.encode_query_embeddings_with_stats(inputs) {
        Ok((embeddings, stats)) => Ok((embeddings, stats)),
        Err(err) if inputs.len() > 1 && is_batch_allocation_failure(&err) => {
            let mid = inputs.len() / 2;
            eprintln!(
                "ato-mcp build: embedding batch of {} exceeded GPU memory; retrying as {} + {}",
                inputs.len(),
                mid,
                inputs.len() - mid
            );
            let (mut embeddings, mut stats) =
                encode_build_embeddings_adaptive(state, &inputs[..mid])?;
            let (tail_embeddings, tail_stats) =
                encode_build_embeddings_adaptive(state, &inputs[mid..])?;
            embeddings.extend(tail_embeddings);
            stats.merge(tail_stats);
            Ok((embeddings, stats))
        }
        Err(err) => Err(err).context(format!("encoding {} chunk embeddings", inputs.len())),
    }
}

pub(crate) fn flush_pending_build_embeddings(
    state: &ServerState,
    conn: &Connection,
    pending: &mut Vec<PendingBuildEmbedding>,
    profile: &mut BuildProfile,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let mut order: Vec<usize> = (0..pending.len()).collect();
    order.sort_by_key(|&idx| pending[idx].text.len());
    for batch in order.chunks(BUILD_EMBED_BATCH_SIZE) {
        let inputs: Vec<String> = batch.iter().map(|&idx| pending[idx].text.clone()).collect();
        let (embeddings, stats) = encode_build_embeddings_adaptive(state, &inputs)?;
        profile.embedding_tokenize += stats.tokenize;
        profile.embedding_prepare += stats.prepare;
        profile.embedding_run += stats.run;
        profile.embedding_postprocess += stats.postprocess;
        profile.embedding_batches += stats.batches;
        profile.embedding_inputs += stats.inputs;
        profile.embedding_active_tokens += stats.active_tokens;
        profile.embedding_padded_tokens += stats.padded_tokens;
        profile.embedding_max_batch = profile.embedding_max_batch.max(stats.max_batch);
        profile.embedding_max_seq_len = profile.embedding_max_seq_len.max(stats.max_seq_len);
        if embeddings.len() != batch.len() {
            bail!(
                "embedding batch returned {} vectors for {} chunks",
                embeddings.len(),
                batch.len()
            );
        }
        let write_started = std::time::Instant::now();
        for (&idx, emb) in batch.iter().zip(embeddings.iter()) {
            let item = &pending[idx];
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(emb.as_ptr() as *const u8, emb.len()) };
            conn.execute(
                "INSERT INTO chunk_embeddings (chunk_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![item.chunk_id, bytes],
            )
            .context("INSERT chunk_embeddings")?;
        }
        profile.embedding_write += write_started.elapsed();
    }
    pending.clear();
    profile.embedding += started.elapsed();
    Ok(())
}



#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildCheckpoint {
    // [IB-13] Checkpoints persist source/model/chunker gates plus committed
    // doc refs, packs, and base-release verification state.
    pub(crate) schema_version: u32,
    pub(crate) source_index_sha256: String,
    pub(crate) zstd_level: i32,
    pub(crate) embedding_model_id: String,
    pub(crate) embedding_model_fingerprint: String,
    pub(crate) embedding_dim: usize,
    pub(crate) embedding_input_max_tokens: usize,
    pub(crate) chunker_format_version: u32,
    pub(crate) documents: Vec<DocRef>,
    pub(crate) packs: Vec<PackInfo>,
    #[serde(default)]
    pub(crate) base_documents: Vec<DocRef>,
    #[serde(default)]
    pub(crate) base_source_hash_by_doc_id: HashMap<String, String>,
    #[serde(default)]
    pub(crate) verified_source_doc_ids: Vec<String>,
}

pub(crate) fn build_checkpoint_path(out_dir: &Path) -> PathBuf {
    out_dir.join("build-state.json")
}

pub(crate) fn load_build_checkpoint(
    out_dir: &Path,
    source_index_sha256: &str,
    zstd_level: i32,
) -> Result<Option<BuildCheckpoint>> {
    let path = build_checkpoint_path(out_dir);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let checkpoint: BuildCheckpoint =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if checkpoint.schema_version != BUILD_CHECKPOINT_SCHEMA_VERSION {
        bail!(
            "unsupported build checkpoint schema {} in {}",
            checkpoint.schema_version,
            path.display()
        );
    }
    if checkpoint.source_index_sha256 != source_index_sha256 {
        bail!(
            "build checkpoint source index hash differs from {}; remove {} to start a fresh build",
            source_index_sha256,
            path.display()
        );
    }
    if checkpoint.zstd_level != zstd_level {
        bail!(
            "build checkpoint zstd level {} differs from requested {}; remove {} to start a fresh build",
            checkpoint.zstd_level,
            zstd_level,
            path.display()
        );
    }
    if checkpoint.embedding_model_id != EMBEDDING_MODEL_ID {
        bail!(
            "build checkpoint embedding model `{}` differs from requested `{}`; remove {} to start a fresh build",
            checkpoint.embedding_model_id,
            EMBEDDING_MODEL_ID,
            path.display()
        );
    }
    if checkpoint.embedding_model_fingerprint != EMBEDDING_MODEL_FINGERPRINT {
        bail!(
            "build checkpoint embedding model fingerprint differs from requested model; remove {} to start a fresh build",
            path.display()
        );
    }
    if checkpoint.embedding_dim != EMBEDDING_DIM {
        bail!(
            "build checkpoint embedding dim {} differs from requested {}; remove {} to start a fresh build",
            checkpoint.embedding_dim,
            EMBEDDING_DIM,
            path.display()
        );
    }
    if checkpoint.embedding_input_max_tokens != EMBEDDING_INPUT_MAX_TOKENS {
        bail!(
            "build checkpoint embedding input max {} differs from requested {}; remove {} to start a fresh build",
            checkpoint.embedding_input_max_tokens,
            EMBEDDING_INPUT_MAX_TOKENS,
            path.display()
        );
    }
    if checkpoint.chunker_format_version != CHUNKER_FORMAT_VERSION {
        bail!(
            "build checkpoint chunker format {} differs from requested {}; remove {} to start a fresh build",
            checkpoint.chunker_format_version,
            CHUNKER_FORMAT_VERSION,
            path.display()
        );
    }
    Ok(Some(checkpoint))
}

pub(crate) fn check_build_checkpoint(
    out_dir: &Path,
    source_index_sha256: &str,
    zstd_level: i32,
) -> Result<()> {
    // Maintainer scripts can resume interrupted build outputs only when the
    // checkpoint, source index, model, and DB agree exactly.
    let checkpoint =
        load_build_checkpoint(out_dir, source_index_sha256, zstd_level)?.ok_or_else(|| {
            anyhow!(
                "build checkpoint missing {}",
                build_checkpoint_path(out_dir).display()
            )
        })?;
    let db_path = out_dir.join("ato.db");
    if !db_path.exists() {
        bail!("build checkpoint missing {}", db_path.display());
    }
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", db_path.display()))?;
    enforce_db_schema_version(&conn)
        .with_context(|| format!("validating DB schema in {}", db_path.display()))?;
    let pending_docs = pending_build_doc_count(&conn)?;
    if pending_docs > 0 {
        bail!(
            "build checkpoint has {pending_docs} uncheckpointed PENDING documents at {}",
            db_path.display()
        );
    }
    let committed_docs = committed_build_doc_count(&conn)?;
    if committed_docs != checkpoint.documents.len() {
        bail!(
            "build checkpoint has {} documents but DB has {committed_docs}",
            checkpoint.documents.len()
        );
    }
    println!(
        "build checkpoint resumable: {} ({} docs, {} packs)",
        out_dir.display(),
        checkpoint.documents.len(),
        checkpoint.packs.len()
    );
    Ok(())
}

pub(crate) struct SaveBuildCheckpointArgs<'a> {
    pub(crate) out_dir: &'a Path,
    pub(crate) source_index_sha256: &'a str,
    pub(crate) zstd_level: i32,
    pub(crate) documents: &'a [DocRef],
    pub(crate) packs: &'a [PackInfo],
    pub(crate) base_documents: &'a [DocRef],
    pub(crate) base_source_hash_by_doc_id: &'a HashMap<String, String>,
    pub(crate) verified_source_doc_ids: &'a HashSet<String>,
}

pub(crate) fn save_build_checkpoint(args: SaveBuildCheckpointArgs<'_>) -> Result<()> {
    let mut verified_source_doc_ids: Vec<String> =
        args.verified_source_doc_ids.iter().cloned().collect();
    verified_source_doc_ids.sort();
    let checkpoint = BuildCheckpoint {
        schema_version: BUILD_CHECKPOINT_SCHEMA_VERSION,
        source_index_sha256: args.source_index_sha256.to_string(),
        zstd_level: args.zstd_level,
        embedding_model_id: EMBEDDING_MODEL_ID.to_string(),
        embedding_model_fingerprint: EMBEDDING_MODEL_FINGERPRINT.to_string(),
        embedding_dim: EMBEDDING_DIM,
        embedding_input_max_tokens: EMBEDDING_INPUT_MAX_TOKENS,
        chunker_format_version: CHUNKER_FORMAT_VERSION,
        documents: args.documents.to_vec(),
        packs: args.packs.to_vec(),
        base_documents: args.base_documents.to_vec(),
        base_source_hash_by_doc_id: args.base_source_hash_by_doc_id.clone(),
        verified_source_doc_ids,
    };
    let path = build_checkpoint_path(args.out_dir);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&checkpoint)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}


pub(crate) fn committed_build_doc_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM documents WHERE pack_sha8 <> 'PENDING'",
        [],
        |row| row.get(0),
    )?;
    Ok(count as usize)
}

pub(crate) fn pending_build_doc_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM documents WHERE pack_sha8 = 'PENDING'",
        [],
        |row| row.get(0),
    )?;
    Ok(count as usize)
}


pub(crate) fn remove_build_doc(conn: &Connection, doc_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM chunks_fts WHERE rowid IN (SELECT chunk_id FROM chunks WHERE doc_id = ?1)",
        [doc_id],
    )?;
    conn.execute("DELETE FROM title_fts WHERE doc_id = ?1", [doc_id])?;
    conn.execute("DELETE FROM citations WHERE target_doc_id = ?1", [doc_id])?;
    conn.execute("DELETE FROM documents WHERE doc_id = ?1", [doc_id])?;
    Ok(())
}


pub(crate) struct BuildSourceFingerprint<'a> {
    pub(crate) doc_id: &'a str,
    pub(crate) doc_type: &'a str,
    pub(crate) title: &'a str,
    pub(crate) date: &'a Option<String>,
    pub(crate) html: &'a str,
    pub(crate) currency: &'a CurrencyInfo,
    pub(crate) has_in_doc_links: bool,
    pub(crate) has_related_docs: bool,
    pub(crate) has_history: bool,
    pub(crate) anchor_refs: &'a [AnchorRef],
    pub(crate) definitions: &'a [JsonValue],
    pub(crate) chunks: &'a [Chunk],
    pub(crate) assets: &'a [ExtractedAsset],
}

pub(crate) fn source_fingerprint_hash(value: &JsonValue) -> Result<String> {
    let mut h = Sha256::new();
    h.update(serde_json::to_vec(value)?);
    let digest = h.finalize();
    let hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Ok(format!("sha256:{hex}"))
}

pub(crate) fn build_source_fingerprint_value(input: BuildSourceFingerprint<'_>) -> JsonValue {
    json!({
        "doc_id": input.doc_id,
        "type": input.doc_type,
        "title": input.title,
        "date": input.date,
        "html": input.html,
        "withdrawn_date": &input.currency.withdrawn_date,
        "superseded_by": &input.currency.superseded_by,
        "replaces": &input.currency.replaces,
        "has_in_doc_links": input.has_in_doc_links as i64,
        "has_related_docs": input.has_related_docs as i64,
        "has_history": input.has_history as i64,
        "anchors": input.anchor_refs.iter().map(|r| json!({
            "kind": &r.kind,
            "label": &r.label,
            "target_doc_id": &r.target_doc_id,
            "target_pit": &r.target_pit,
        })).collect::<Vec<_>>(),
        "definitions": input.definitions,
        "chunks": input.chunks.iter().map(|chunk| json!({
            "ord": chunk.ord,
            "anchor": &chunk.anchor,
            "text": &chunk.text,
        })).collect::<Vec<_>>(),
        "assets": input.assets.iter().map(|asset| json!({
            "asset_ref": &asset.asset_ref,
            "source_path": &asset.source_path,
            "relative_path": &asset.relative_path,
            "media_type": &asset.media_type,
            "alt": &asset.alt,
            "title": &asset.title,
            "sha256": &asset.sha256,
            "size": asset.size,
        })).collect::<Vec<_>>(),
    })
}







pub(crate) struct BuildCorpusArgs<'a> {
    pub(crate) pages_dir: &'a Path,
    pub(crate) db_path: &'a Path,
    pub(crate) model_dir: &'a Path,
    pub(crate) base_release_dir: Option<&'a Path>,
    pub(crate) out_dir: &'a Path,
    pub(crate) zstd_level: i32,
    pub(crate) limit: Option<usize>,
    pub(crate) use_gpu: bool,
    pub(crate) profile_enabled: bool,
}

pub(crate) fn build_corpus(args: BuildCorpusArgs<'_>) -> Result<()> {
    use std::io::BufRead as _;

    let BuildCorpusArgs {
        pages_dir,
        db_path,
        model_dir,
        base_release_dir,
        out_dir,
        zstd_level,
        limit,
        use_gpu,
        profile_enabled,
    } = args;

    // [IB-17] Maintainer builds require a local pinned Granite model
    // checkout; hosted model metadata is owned by publish/release.
    let semantic_model_paths = SemanticModelPaths::from_model_dir(model_dir)?;
    let index_path = pages_dir.join("index.jsonl");
    let source_index_sha256 = sha256_file(&index_path)?;
    let index_file =
        File::open(&index_path).with_context(|| format!("opening {}", index_path.display()))?;
    let reader = std::io::BufReader::new(index_file);

    fs::create_dir_all(out_dir)
        .with_context(|| format!("creating out_dir {}", out_dir.display()))?;
    fs::create_dir_all(out_dir.join("packs"))?;
    fs::create_dir_all(out_dir.join("assets"))?;

    let checkpoint = load_build_checkpoint(out_dir, &source_index_sha256, zstd_level)?;
    let checkpoint_loaded = checkpoint.is_some();
    let mut base_doc_refs: HashMap<String, DocRef> = HashMap::new();
    let mut base_documents: Vec<DocRef> = Vec::new();
    let mut base_source_hash_by_doc_id: HashMap<String, String> = HashMap::new();
    let mut source_doc_ids: HashSet<String> = HashSet::new();
    let mut base_seeded = false;
    let (mut documents, packs) = match checkpoint {
        Some(checkpoint) => {
            eprintln!(
                "ato-mcp build: resuming from checkpoint ({} docs, {} packs)",
                checkpoint.documents.len(),
                checkpoint.packs.len()
            );
            if !checkpoint.base_documents.is_empty() {
                base_documents = checkpoint.base_documents;
                base_doc_refs = base_documents
                    .iter()
                    .map(|doc| (doc.doc_id.clone(), doc.clone()))
                    .collect();
                base_source_hash_by_doc_id = checkpoint.base_source_hash_by_doc_id;
                source_doc_ids = checkpoint.verified_source_doc_ids.into_iter().collect();
                base_seeded = true;
            }
            (checkpoint.documents, checkpoint.packs)
        }
        None => {
            fs::remove_file(out_dir.join("manifest.json")).ok();
            fs::remove_file(out_dir.join("update.json")).ok();
            if base_release_dir.is_some() {
                bail!(
                    "--base-release-dir is no longer supported (the pack-based seed path was \
                    removed); run a full build instead."
                );
            }
            (Vec::new(), Vec::new())
        }
    };
    let conn = open_write_at(db_path)
        .with_context(|| format!("opening sqlite at {}", db_path.display()))?;
    init_db(&conn)?;
    let committed_docs = committed_build_doc_count(&conn)?;
    let pending_docs = pending_build_doc_count(&conn)?;
    if pending_docs > 0 {
        bail!(
            "build DB has {pending_docs} uncheckpointed PENDING documents at {}; remove the release dir to start fresh",
            db_path.display()
        );
    }
    if committed_docs != documents.len() {
        bail!(
            "build checkpoint has {} documents but DB has {committed_docs}; remove {} to start fresh",
            documents.len(),
            build_checkpoint_path(out_dir).display()
        );
    }
    // [IB-14] Resume skips only checkpoint-committed docs (or verified source
    // doc_ids for a base-seeded checkpoint); PENDING rows abort above.
    let checkpoint_doc_ids: HashSet<String> = if checkpoint_loaded && base_seeded {
        source_doc_ids.clone()
    } else if checkpoint_loaded {
        documents.iter().map(|doc| doc.doc_id.clone()).collect()
    } else {
        HashSet::new()
    };

    let mut profile = BuildProfile::new(profile_enabled);
    // [IB-16] Corpus build runs as a single Rust process with adaptive
    // embedding batches and no separate worker-pool build path.
    let state = ServerState::with_model_paths(use_gpu, semantic_model_paths);
    let mut processed: usize = if checkpoint_loaded && base_seeded {
        source_doc_ids.len()
    } else {
        checkpoint_doc_ids.len()
    };
    let mut skipped_no_payload: usize = 0;
    let mut skipped_duplicate_doc_ids: usize = 0;
    let mut reused_base_docs: usize = 0;
    let mut changed_base_docs: usize = 0;
    let mut removed_base_docs: usize = 0;
    let mut tx = conn.unchecked_transaction()?;
    let progress_started = std::time::Instant::now();

    let mut pending_embeddings: Vec<PendingBuildEmbedding> =
        Vec::with_capacity(BUILD_EMBED_BATCH_SIZE);
    let mut doc_hashes: HashMap<String, String> = HashMap::new();
    for line_res in reader.lines() {
        if let Some(n) = limit {
            if processed >= n {
                break;
            }
        }
        let line = line_res?;
        if line.trim().is_empty() {
            continue;
        }
        let record: JsonValue = serde_json::from_str(&line).context("parsing index.jsonl line")?;
        let canonical_id = record
            .get("canonical_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("index record missing canonical_id"))?;
        let Some(payload_path_raw) = record.get("payload_path").and_then(|v| v.as_str()) else {
            skipped_no_payload += 1;
            continue;
        };
        if payload_path_raw.is_empty() {
            skipped_no_payload += 1;
            continue;
        }
        let payload_path = pages_dir.join(payload_path_raw);
        let doc_id = metadata_doc_id_for(canonical_id);
        let checkpoint_verified = checkpoint_doc_ids.contains(&doc_id);
        if !source_doc_ids.insert(doc_id.clone()) {
            if checkpoint_verified {
                processed += 1;
                maybe_report_build_progress(
                    processed,
                    profile.docs,
                    reused_base_docs,
                    progress_started,
                );
                continue;
            }
            skipped_duplicate_doc_ids += 1;
            continue;
        }
        if checkpoint_verified {
            processed += 1;
            maybe_report_build_progress(
                processed,
                profile.docs,
                reused_base_docs,
                progress_started,
            );
            continue;
        }

        let started = std::time::Instant::now();
        let html = fs::read_to_string(&payload_path)
            .with_context(|| format!("reading payload {}", payload_path.display()))?;
        profile.read += started.elapsed();
        profile.html_bytes += html.len() as u64;
        let doc_type = metadata_parse_docid(canonical_id).unwrap_or_default();

        // Cleaning pipeline.
        let started = std::time::Instant::now();
        let cleaned = clean_ato_html(&html);
        let (rewritten_html, assets) =
            rewrite_images_html(&cleaned.html, Some(&doc_id), Some(payload_path.as_path()));
        let normalised = normalise_named_anchors(&rewritten_html);
        let with_links = rewrite_links_html(&normalised);
        let final_html = strip_attributes(&with_links);
        profile.clean += started.elapsed();

        // Currency / supersession from raw page HTML (alert + body scan).
        let started = std::time::Instant::now();
        let currency = extract_currency(&html);

        // Initial title from leading-headings composer (always present).
        let leading = extract_leading_headings(&cleaned.html);
        let composed_title = extract_compose_title(&leading);
        let raw_title = composed_title
            .clone()
            .or(cleaned.title.clone())
            .unwrap_or_else(|| canonical_id.to_string());

        // Headings + levels for the rule engine.
        let mut headings: Vec<String> = Vec::new();
        let mut heading_levels: Vec<u32> = Vec::new();
        {
            let frag = scraper::Html::parse_fragment(&final_html);
            let h_sel = scraper::Selector::parse("h1, h2, h3, h4, h5, h6").unwrap();
            for h in frag.select(&h_sel) {
                let text = anchors_node_text(h);
                if text.is_empty() {
                    continue;
                }
                let level: u32 = match h.value().name() {
                    "h1" => 1,
                    "h2" => 2,
                    "h3" => 3,
                    "h4" => 4,
                    "h5" => 5,
                    "h6" => 6,
                    _ => 0,
                };
                headings.push(text);
                heading_levels.push(level);
            }
        }

        // Body head (first 3000 chars of cleaned text) for date / case-name pulls.
        let body_head: String = cleaned.text.chars().take(3000).collect();
        let pub_date = metadata_extract_pub_date(&body_head);

        // EM front-matter signals (parliamentary EM / regulation ES).
        let (fm_refs, fm_phrase) = extract_em_front_matter(&cleaned.html);

        let rule_inputs = RuleInputs {
            doc_id: doc_id.clone(),
            title: Some(raw_title.clone()),
            headings,
            heading_levels,
            body_head,
            category: Some(doc_type.clone()),
            pub_date,
            front_matter_refs: fm_refs,
            front_matter_phrase: fm_phrase,
        };
        let derived = derive_metadata(&rule_inputs);
        let title = derived.title.clone().unwrap_or(raw_title);
        let derived_date = derived.date.clone();
        profile.metadata += started.elapsed();

        // Chunker.
        let started = std::time::Instant::now();
        let chunks = chunk_html(&final_html, Some(&title), EMBED_MAX_TOKENS);
        profile.chunking += started.elapsed();
        profile.chunks += chunks.len();

        // Anchor refs (used for navigation flags + doc_anchors table).
        let started = std::time::Instant::now();
        let anchor_refs = extract_anchors(&final_html, &doc_id);
        let has_in_doc_links = anchor_refs.iter().any(|r| r.kind == "in_doc");
        let has_related_docs = anchor_refs.iter().any(|r| r.kind == "sister");
        let has_history = anchor_refs.iter().any(|r| r.kind == "history");
        profile.references += started.elapsed();

        // Definitions are source-derived and needed before the base-release
        // reuse decision, but their DB rows are inserted after the document row.
        let started = std::time::Instant::now();
        let def_chunks: Vec<DefinitionChunk> = chunks
            .iter()
            .map(|c| DefinitionChunk {
                ord: c.ord,
                anchor: c.anchor.clone(),
                text: c.text.clone(),
            })
            .collect();
        let defs = extract_definitions(&doc_id, &title, &doc_type, &def_chunks);
        let definition_records: Vec<JsonValue> = defs
            .iter()
            .map(|d| {
                json!({
                    "definition_id": d.definition_id.clone(),
                    "term": d.term.clone(),
                    "norm_term": d.norm_term.clone(),
                    "doc_id": d.doc_id.clone(),
                    "source_title": d.source_title.clone(),
                    "source_type": d.source_type.clone(),
                    "scope": d.scope.clone(),
                    "anchor": d.anchor.clone(),
                    "ord": d.ord,
                    "body": d.body.clone(),
                })
            })
            .collect();
        profile.references += started.elapsed();

        let source_hash =
            source_fingerprint_hash(&build_source_fingerprint_value(BuildSourceFingerprint {
                doc_id: &doc_id,
                doc_type: &doc_type,
                title: &title,
                date: &derived_date,
                html: &final_html,
                currency: &currency,
                has_in_doc_links,
                has_related_docs,
                has_history,
                anchor_refs: &anchor_refs,
                definitions: &definition_records,
                chunks: &chunks,
                assets: &assets,
            }))?;
        let content_hash = metadata_content_hash(&cleaned.text);
        if base_doc_refs.contains_key(&doc_id) {
            let base_source_hash = base_source_hash_by_doc_id
                .get(&doc_id)
                .ok_or_else(|| anyhow!("base release missing source fingerprint for {doc_id}"))?;
            if base_source_hash == &source_hash {
                reused_base_docs += 1;
                processed += 1;
                maybe_report_build_progress(
                    processed,
                    profile.docs,
                    reused_base_docs,
                    progress_started,
                );
                continue;
            }
            remove_build_doc(&tx, &doc_id)
                .with_context(|| format!("removing changed base doc {doc_id}"))?;
            documents.retain(|doc| doc.doc_id != doc_id);
            changed_base_docs += 1;
        }

        let now = chrono::Utc::now().to_rfc3339();
        doc_hashes.insert(doc_id.clone(), content_hash.clone());

        // Pack sha8 placeholder; finalised after all docs processed.
        let pack_placeholder = "PENDING".to_string();

        // Collect headings once: stored on documents.headings for install-time
        // FTS5 rebuild, and re-used below to populate title_fts.headings.
        let headings_frag = scraper::Html::parse_fragment(&final_html);
        let h_sel = scraper::Selector::parse("h1, h2, h3, h4, h5, h6").unwrap();
        let headings_concat: Vec<String> = headings_frag
            .select(&h_sel)
            .map(anchors_node_text)
            .filter(|s| !s.is_empty())
            .collect();
        let headings_text = headings_concat.join(" ");

        let started = std::time::Instant::now();
        tx.execute(
            "INSERT INTO documents
                (doc_id, type, title, date, downloaded_at, content_hash, pack_sha8,
                 html, withdrawn_date, superseded_by, replaces,
                 has_in_doc_links, has_related_docs, has_history, headings)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            rusqlite::params![
                doc_id,
                doc_type,
                title,
                derived_date,
                now,
                content_hash,
                pack_placeholder,
                compress_text(&final_html)?,
                currency.withdrawn_date.clone(),
                currency.superseded_by.clone(),
                currency.replaces.clone(),
                has_in_doc_links as i64,
                has_related_docs as i64,
                has_history as i64,
                headings_text.clone(),
            ],
        )
        .context("INSERT documents")?;

        let mut chunk_ids: Vec<(i64, String, Option<String>)> = Vec::new();
        let mut doc_pending_embeddings: Vec<(i64, String)> = Vec::new();
        for chunk in &chunks {
            let zstd_text =
                zstd::stream::encode_all(std::io::Cursor::new(chunk.text.as_bytes()), zstd_level)
                    .context("zstd-compressing chunk text")?;
            let chunk_id: i64 = tx
                .query_row(
                    "INSERT INTO chunks (doc_id, ord, anchor, text)
                 VALUES (?1, ?2, ?3, ?4)
                 RETURNING chunk_id",
                    rusqlite::params![doc_id, chunk.ord, chunk.anchor, zstd_text],
                    |row| row.get(0),
                )
                .context("INSERT chunks")?;
            chunk_ids.push((chunk_id, chunk.text.clone(), chunk.anchor.clone()));

            tx.execute(
                "INSERT INTO chunks_fts (rowid, text) VALUES (?1, ?2)",
                rusqlite::params![chunk_id, chunk.text],
            )
            .with_context(|| {
                format!(
                    "INSERT chunks_fts doc_id={} chunk_id={} ord={}",
                    doc_id, chunk_id, chunk.ord
                )
            })?;

            doc_pending_embeddings.push((chunk_id, chunk.text.clone()));
        }

        // title_fts: re-use the headings collected before the documents INSERT.
        tx.execute(
            "INSERT INTO title_fts (doc_id, title, headings) VALUES (?1, ?2, ?3)",
            rusqlite::params![doc_id, title, headings_text],
        )
        .context("INSERT title_fts")?;

        // doc_anchors.
        let mut anchor_records: Vec<JsonValue> = Vec::new();
        for (anchor_ord, r) in (0_i64..).zip(anchor_refs.iter()) {
            let target_chunk_id: Option<i64> = if r.kind == "in_doc" {
                if let Some(name) = r.target_anchor.as_deref() {
                    let marker = format!("[anchor:{name}]");
                    chunk_ids
                        .iter()
                        .find(|(_id, text, anchor)| {
                            anchor.as_deref() == Some(name) || text.contains(&marker)
                        })
                        .map(|(id, _, _)| *id)
                } else {
                    None
                }
            } else {
                None
            };
            anchor_records.push(json!({
                "ord": anchor_ord,
                "kind": r.kind.clone(),
                "label": r.label.clone(),
                "target_chunk_id": target_chunk_id,
                "target_doc_id": r.target_doc_id.clone(),
                "target_pit": r.target_pit.clone(),
            }));
            tx.execute(
                "INSERT INTO doc_anchors
                    (doc_id, ord, kind, label, target_chunk_id, target_doc_id, target_pit)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    doc_id,
                    anchor_ord,
                    r.kind,
                    r.label,
                    target_chunk_id,
                    r.target_doc_id,
                    r.target_pit,
                ],
            )
            .context("INSERT doc_anchors")?;
        }
        profile.sqlite += started.elapsed();

        // Definitions.
        let started = std::time::Instant::now();
        for d in &defs {
            tx.execute(
                "INSERT OR REPLACE INTO definitions
                    (definition_id, term, norm_term, doc_id, source_title,
                     source_type, scope, anchor, ord, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    d.definition_id,
                    d.term,
                    d.norm_term,
                    d.doc_id,
                    d.source_title,
                    d.source_type,
                    d.scope,
                    d.anchor,
                    d.ord,
                    d.body,
                ],
            )
            .context("INSERT definitions")?;
        }
        profile.sqlite += started.elapsed();

        // Asset persistence: write each image to <out_dir>/assets/<sha[:2]>/<sha>.bin.
        let started = std::time::Instant::now();
        for asset in &assets {
            let target = out_dir.join(&asset.relative_path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            if !target.exists() || fs::metadata(&target)?.len() != asset.size {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(asset.data_b64.as_bytes())
                    .context("decoding asset b64")?;
                fs::write(&target, &bytes)?;
            }
        }
        profile.assets += started.elapsed();

        for (chunk_id, text) in doc_pending_embeddings {
            pending_embeddings.push(PendingBuildEmbedding { chunk_id, text });
        }
        if pending_embeddings.len() >= BUILD_EMBED_PENDING_FLUSH_CHUNKS {
            flush_pending_build_embeddings(
                &state,
                &tx,
                &mut pending_embeddings,
                &mut profile,
            )?;
            // Checkpoint periodically so a long build can resume from an
            // interrupted partial DB.
            let started = std::time::Instant::now();
            tx.commit()?;
            save_build_checkpoint(SaveBuildCheckpointArgs {
                out_dir,
                source_index_sha256: &source_index_sha256,
                zstd_level,
                documents: &documents,
                packs: &packs,
                base_documents: &base_documents,
                base_source_hash_by_doc_id: &base_source_hash_by_doc_id,
                verified_source_doc_ids: &source_doc_ids,
            })?;
            profile.checkpoint += started.elapsed();
            doc_hashes.clear();
            tx = conn.unchecked_transaction()?;
        }

        processed += 1;
        profile.docs += 1;
        maybe_report_build_progress(processed, profile.docs, reused_base_docs, progress_started);
    }

    if base_seeded {
        let removed_doc_ids: Vec<String> = documents
            .iter()
            .filter(|doc| {
                base_doc_refs.contains_key(&doc.doc_id) && !source_doc_ids.contains(&doc.doc_id)
            })
            .map(|doc| doc.doc_id.clone())
            .collect();
        for doc_id in &removed_doc_ids {
            remove_build_doc(&tx, doc_id)
                .with_context(|| format!("removing doc absent from source index {doc_id}"))?;
        }
        removed_base_docs = removed_doc_ids.len();
        if removed_base_docs > 0 {
            documents.retain(|doc| !removed_doc_ids.contains(&doc.doc_id));
        }
    }

    flush_pending_build_embeddings(
        &state,
        &tx,
        &mut pending_embeddings,
        &mut profile,
    )?;
    tx.commit()?;
    let started = std::time::Instant::now();
    save_build_checkpoint(SaveBuildCheckpointArgs {
        out_dir,
        source_index_sha256: &source_index_sha256,
        zstd_level,
        documents: &documents,
        packs: &packs,
        base_documents: &base_documents,
        base_source_hash_by_doc_id: &base_source_hash_by_doc_id,
        verified_source_doc_ids: &source_doc_ids,
    })?;
    profile.checkpoint += started.elapsed();
    if skipped_no_payload > 0 {
        eprintln!("ato-mcp build: skipped {skipped_no_payload} index records without payload_path");
    }
    if skipped_duplicate_doc_ids > 0 {
        eprintln!(
            "ato-mcp build: skipped {skipped_duplicate_doc_ids} duplicate doc_id index records"
        );
    }
    if reused_base_docs > 0 {
        eprintln!("ato-mcp build: reused {reused_base_docs} unchanged docs from base release");
    }
    if changed_base_docs > 0 {
        eprintln!("ato-mcp build: rebuilt {changed_base_docs} changed docs from base release");
    }
    if removed_base_docs > 0 {
        eprintln!("ato-mcp build: removed {removed_base_docs} base docs absent from source index");
    }

    let started = std::time::Instant::now();
    let created_at = chrono::Utc::now().to_rfc3339();
    let manifest = Manifest {
        schema_version: SUPPORTED_MANIFEST_VERSION as i64,
        index_version: chrono::Utc::now().format("%Y.%m.%d").to_string(),
        created_at,
        min_client_version: env!("CARGO_PKG_VERSION").to_string(),
        model: ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: EMBEDDING_MODEL_FINGERPRINT.to_string(),
            size: EMBEDDING_MODEL_HF_SIZE,
            url: EMBEDDING_MODEL_HF_URL.to_string(),
        },
        // package-corpus fills in the real db sha256/size/url after stripping
        // FTS5 and zstd-compressing the canonical ato.db.
        db: ManifestDb {
            url: "ato.db.zst".to_string(),
            sha256: String::new(),
            size: 0,
        },
    };

    let final_tx = conn.unchecked_transaction()?;
    set_meta(&final_tx, "index_version", &manifest.index_version)?;
    set_meta(&final_tx, "embedding_model_id", &manifest.model.id)?;
    set_meta(&final_tx, "last_update_at", &manifest.created_at)?;
    eprintln!("ato-mcp build: deriving citations…");
    // [IB-20] Build finalisation derives citations from stored [doc:X]
    // markers before manifest/update metadata is written.
    derive_citations(&final_tx)?;
    verify_semantic_install(&final_tx, &manifest)?;
    final_tx.commit()?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

    let manifest_path = out_dir.join("manifest.json");
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    let summary = UpdateSummary {
        schema_version: manifest.schema_version,
        index_version: manifest.index_version.clone(),
        min_client_version: manifest.min_client_version.clone(),
        model: manifest.model.clone(),
        db_sha256: manifest.db.sha256.clone(),
        db_size: manifest.db.size,
        manifest_fingerprint: Some(manifest_fingerprint(&manifest)?),
    };
    let summary_path = out_dir.join("update.json");
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)?;
    eprintln!(
        "ato-mcp build: wrote {} + {}",
        manifest_path.display(),
        summary_path.display()
    );
    profile.finalise += started.elapsed();
    profile.print();

    eprintln!(
        "ato-mcp build: done - {processed} docs written to {}",
        db_path.display()
    );
    Ok(())
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

/// Strip FTS5 indexes from a copy of the canonical ato.db, VACUUM, and
/// zstd-compress to produce a shippable artifact. The input file is never
/// mutated. Returns {path, sha256, size} for embedding into manifest.json.
pub(crate) fn package_corpus(db_path: &Path, out: &Path, level: i32) -> Result<JsonValue> {
    use std::io::{copy as io_copy, BufReader, BufWriter};

    if !db_path.is_file() {
        bail!("input DB not found: {}", db_path.display());
    }
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }

    let staging = tempfile::tempdir().context("creating staging dir")?;
    let staged_db = staging.path().join("stage.db");
    fs::copy(db_path, &staged_db).with_context(|| {
        format!("copying {} → {}", db_path.display(), staged_db.display())
    })?;

    // Drop FTS5 + VACUUM on the copy. install will rebuild both indexes from
    // chunks.text and documents.headings.
    {
        let conn = Connection::open(&staged_db)
            .with_context(|| format!("opening staged DB {}", staged_db.display()))?;
        conn.execute_batch(
            "DROP TABLE IF EXISTS chunks_fts; DROP TABLE IF EXISTS title_fts; VACUUM;",
        )
        .context("stripping FTS5 + VACUUM on staged DB")?;
    }

    // zstd compress with long-distance matching for the high-redundancy DB.
    let input = File::open(&staged_db)
        .with_context(|| format!("opening staged DB {}", staged_db.display()))?;
    let output = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut reader = BufReader::new(input);
    let writer = BufWriter::new(output);
    let mut encoder = zstd::stream::Encoder::new(writer, level)
        .context("creating zstd encoder")?;
    encoder
        .long_distance_matching(true)
        .context("enabling zstd long-distance matching")?;
    io_copy(&mut reader, &mut encoder).context("compressing staged DB")?;
    encoder.finish().context("finalising zstd stream")?;

    // sha256 + size of the compressed artifact.
    let sha256 = sha256_file(out).context("hashing compressed artifact")?;
    let size = fs::metadata(out)?.len();

    Ok(json!({
        "path": out.display().to_string(),
        "sha256": sha256,
        "size": size,
    }))
}

/// Update a manifest.json in-place so its `db` field points at the freshly
/// packaged ato.db.zst. The URL is set to the bare filename; the publish
/// pipeline rewrites it to a GitHub release URL later. `manifest.schema_version`
/// is bumped to the current supported version.
pub(crate) fn update_manifest_with_db(
    manifest_path: &Path,
    artifact_path: &Path,
    artifact_summary: &JsonValue,
) -> Result<()> {
    let raw = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let mut value: JsonValue = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("manifest is not a JSON object"))?;
    let filename = artifact_path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("artifact path has no UTF-8 filename"))?
        .to_string();
    let sha256 = artifact_summary
        .get("sha256")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("artifact summary missing sha256"))?
        .to_string();
    let size = artifact_summary
        .get("size")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("artifact summary missing size"))?;
    obj.insert(
        "db".to_string(),
        json!({
            "url": filename,
            "sha256": sha256,
            "size": size,
        }),
    );
    obj.insert(
        "schema_version".to_string(),
        json!(SUPPORTED_MANIFEST_VERSION),
    );
    let pretty = serde_json::to_vec_pretty(&value)?;
    fs::write(manifest_path, pretty)
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    Ok(())
}

pub(crate) fn bundle_localize_manifest(
    manifest_path: &Path,
    packs_dir: &Path,
    model_bundle: &Path,
) -> Result<()> {
    let mut manifest: JsonValue = serde_json::from_str(&fs::read_to_string(manifest_path)?)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;

    if let Some(packs) = manifest.get_mut("packs").and_then(|v| v.as_array_mut()) {
        for pack in packs {
            let url = pack
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let filename = std::path::Path::new(&url)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&url)
                .to_string();
            let pack_path = packs_dir.join(&filename);
            if !pack_path.exists() {
                bail!("manifest references missing pack: {}", filename);
            }
            pack["url"] = JsonValue::String(format!("packs/{filename}"));
            pack["sha256"] = JsonValue::String(sha256_file(&pack_path)?);
            pack["size"] =
                JsonValue::Number(serde_json::Number::from(fs::metadata(&pack_path)?.len()));
        }
    }

    let model_filename = model_bundle
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("model bundle has no filename"))?;
    if let Some(model) = manifest.get_mut("model") {
        model["url"] = JsonValue::String(model_filename.to_string());
        model["sha256"] = JsonValue::String(sha256_file(model_bundle)?);
        model["size"] =
            JsonValue::Number(serde_json::Number::from(fs::metadata(model_bundle)?.len()));
    }

    let manifest_typed: Manifest = serde_json::from_value(manifest.clone())
        .with_context(|| format!("validating {}", manifest_path.display()))?;
    let manifest_fingerprint = manifest_fingerprint(&manifest_typed)?;
    fs::write(manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

    let summary = json!({
        "schema_version": manifest.get("schema_version").cloned().unwrap_or(JsonValue::Null),
        "index_version": manifest.get("index_version").cloned().unwrap_or(JsonValue::Null),
        "min_client_version": manifest.get("min_client_version").cloned().unwrap_or(JsonValue::String(String::new())),
        "model": manifest.get("model").cloned().unwrap_or(JsonValue::Null),
        "document_count": manifest
            .get("documents")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        "pack_count": manifest
            .get("packs")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        "manifest_fingerprint": manifest_fingerprint,
    });
    let summary_path = manifest_path
        .parent()
        .map(|p| p.join("update.json"))
        .ok_or_else(|| anyhow!("manifest has no parent dir"))?;
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)?;

    eprintln!(
        "bundle-localize-manifest: rewrote {} + {}",
        manifest_path.display(),
        summary_path.display(),
    );
    Ok(())
}
