//! Corpus build orchestrator: walks ato_pages/index.jsonl, runs cleaning +
//! chunker + rules-engine + embedder pipeline in-process, writes documents/
//! chunks/embeddings/anchors/definitions/citations, packs, manifest, and
//! update.json. Plus base-release seeding, checkpoint resume, and
//! `bundle_localize_manifest` for offline bundles.

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
use crate::pack::{
    read_record_from_pack_bytes, write_pack, PackRecord,
};
use crate::retrieval::derive_citations;
use crate::rules::{derive_metadata, RuleInputs};
use crate::semantic::{
    SemanticEncodeStats, SemanticModelPaths,
};
use crate::source::{
    enforce_manifest_compatibility, insert_docs_from_packs, manifest_fingerprint,
    verify_semantic_install, DocRef, Manifest, ModelInfo, PackInfo, UpdateSummary,
};
use crate::{
    enforce_db_schema_version, fetch_bytes, parse_hf_model_url,
    resolve_manifest_asset, verify_sha256_bytes, ServerState, UrlContext, EMBEDDING_DIM,
    EMBEDDING_INPUT_MAX_TOKENS, EMBEDDING_MODEL_FINGERPRINT, EMBEDDING_MODEL_HF_SIZE,
    EMBEDDING_MODEL_HF_URL, EMBEDDING_MODEL_ID, SUPPORTED_MANIFEST_VERSION,
};
use rusqlite::OpenFlags;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) const BUILD_EMBED_BATCH_SIZE: usize = 32;
pub(crate) const BUILD_EMBED_PENDING_FLUSH_CHUNKS: usize = 4096;
// [IB-10] Build pack shards are bounded by document count, not target
// bytes, so downloads stay tractable while pack offsets remain stable.
pub(crate) const BUILD_PACK_RECORDS_PER_SHARD: usize = 4096;
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
    pub(crate) doc_idx: usize,
    pub(crate) chunk_idx: usize,
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
    pack_records: &mut [(String, JsonValue)],
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
            // [IB-11] Pack records carry base64 raw int8 embeddings; install
            // decode length-checks them against EMBEDDING_DIM.
            let emb_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            let chunk_record = pack_records
                .get_mut(item.doc_idx)
                .and_then(|(_doc_id, record)| record.get_mut("chunks"))
                .and_then(|chunks| chunks.as_array_mut())
                .and_then(|chunks| chunks.get_mut(item.chunk_idx))
                .ok_or_else(|| anyhow!("missing pack chunk record for embedded chunk"))?;
            chunk_record["embedding_b64"] = JsonValue::String(emb_b64);
        }
        profile.embedding_write += write_started.elapsed();
    }
    pending.clear();
    profile.embedding += started.elapsed();
    Ok(())
}

pub(crate) struct BuildPackShardContext<'a> {
    pub(crate) out_dir: &'a Path,
    pub(crate) zstd_level: i32,
    pub(crate) doc_hashes: &'a HashMap<String, String>,
    pub(crate) documents: &'a mut Vec<DocRef>,
    pub(crate) packs: &'a mut Vec<PackInfo>,
    pub(crate) profile: &'a mut BuildProfile,
}

pub(crate) fn write_build_pack_shard(
    shard_idx: usize,
    pack_records: &mut Vec<(String, JsonValue)>,
    ctx: &mut BuildPackShardContext<'_>,
) -> Result<()> {
    if pack_records.is_empty() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    eprintln!(
        "ato-mcp build: writing pack shard {} ({} docs)",
        shard_idx + 1,
        pack_records.len()
    );
    let tmp_pack = ctx
        .out_dir
        .join("packs")
        .join(format!(".pack-{shard_idx:04}-writing.bin.zst.tmp"));
    let pack_meta = write_pack(&tmp_pack, ctx.zstd_level, pack_records.drain(..).map(Ok))?;
    let sha8 = pack_meta
        .get("sha8")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("write_pack returned no sha8"))?
        .to_string();
    let final_pack = ctx
        .out_dir
        .join("packs")
        .join(format!("pack-{sha8}.bin.zst"));
    fs::rename(&tmp_pack, &final_pack)?;

    let refs = pack_meta
        .get("refs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("write_pack returned no refs"))?;
    for r in refs {
        let doc_id = r
            .get("doc_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("pack ref missing doc_id"))?
            .to_string();
        let content_hash = ctx
            .doc_hashes
            .get(&doc_id)
            .cloned()
            .ok_or_else(|| anyhow!("missing content hash for packed doc {doc_id}"))?;
        ctx.documents.push(DocRef {
            doc_id,
            content_hash,
            pack_sha8: sha8.clone(),
            offset: r
                .get("offset")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow!("pack ref missing offset"))?,
            length: r
                .get("length")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow!("pack ref missing length"))?,
        });
    }

    let pack_size = pack_meta
        .get("size")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("write_pack returned no size"))?;
    let pack_sha256 = pack_meta
        .get("sha256")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("write_pack returned no sha256"))?
        .to_string();
    ctx.packs.push(PackInfo {
        sha8: sha8.clone(),
        sha256: pack_sha256,
        size: pack_size,
        url: format!("packs/pack-{sha8}.bin.zst"),
    });
    ctx.profile.pack += started.elapsed();
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

pub(crate) fn clean_stale_build_packs(out_dir: &Path, packs: &[PackInfo]) -> Result<()> {
    let packs_dir = out_dir.join("packs");
    if !packs_dir.exists() {
        return Ok(());
    }
    let keep: HashSet<String> = packs
        .iter()
        .map(|pack| format!("pack-{}.bin.zst", pack.sha8))
        .collect();
    for entry in fs::read_dir(&packs_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let is_pack = name.starts_with("pack-") && name.ends_with(".bin.zst");
        let is_tmp = name.starts_with(".pack-") && name.ends_with(".tmp");
        if is_tmp || (is_pack && !keep.contains(name)) {
            fs::remove_file(&path).with_context(|| format!("removing stale {}", path.display()))?;
        }
    }
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

pub(crate) fn update_pack_sha8_for_docs(conn: &Connection, docs: &[DocRef]) -> Result<()> {
    let mut update = conn.prepare("UPDATE documents SET pack_sha8 = ?1 WHERE doc_id = ?2")?;
    for doc in docs {
        update.execute(rusqlite::params![&doc.pack_sha8, &doc.doc_id])?;
    }
    Ok(())
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

pub(crate) fn copy_or_hard_link(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if dst.exists() {
        return Ok(());
    }
    match fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(src, dst)
                .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
            Ok(())
        }
    }
}

pub(crate) struct BaseReleaseSeed {
    pub(crate) documents: Vec<DocRef>,
    pub(crate) packs: Vec<PackInfo>,
    pub(crate) by_doc_id: HashMap<String, DocRef>,
    pub(crate) source_hash_by_doc_id: HashMap<String, String>,
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

pub(crate) fn pack_record_source_fingerprint_value(record: &PackRecord) -> JsonValue {
    json!({
        "doc_id": &record.doc_id,
        "type": &record.doc_type,
        "title": &record.title,
        "date": &record.date,
        "html": &record.html,
        "withdrawn_date": &record.withdrawn_date,
        "superseded_by": &record.superseded_by,
        "replaces": &record.replaces,
        "has_in_doc_links": record.has_in_doc_links,
        "has_related_docs": record.has_related_docs,
        "has_history": record.has_history,
        "anchors": record.anchors.iter().map(|anchor| json!({
            "kind": &anchor.kind,
            "label": &anchor.label,
            "target_doc_id": &anchor.target_doc_id,
            "target_pit": &anchor.target_pit,
        })).collect::<Vec<_>>(),
        "definitions": record.definitions.iter().map(|definition| json!({
            "definition_id": &definition.definition_id,
            "term": &definition.term,
            "norm_term": &definition.norm_term,
            "doc_id": &definition.doc_id,
            "source_title": &definition.source_title,
            "source_type": &definition.source_type,
            "scope": &definition.scope,
            "anchor": &definition.anchor,
            "ord": definition.ord,
            "body": &definition.body,
        })).collect::<Vec<_>>(),
        "chunks": record.chunks.iter().map(|chunk| json!({
            "ord": chunk.ord,
            "anchor": &chunk.anchor,
            "text": &chunk.text,
        })).collect::<Vec<_>>(),
        "assets": record.assets.iter().map(|asset| json!({
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

pub(crate) fn pack_filename(url: &str) -> Result<String> {
    Path::new(url)
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("pack URL has no filename: {url}"))
}

pub(crate) fn validate_base_release_manifest(manifest: &Manifest) -> Result<()> {
    // Maintainer base seeding accepts only the current manifest and embedding
    // model shape, so old corpus releases fail before build starts.
    enforce_manifest_compatibility(manifest)?;
    if manifest.model.id != EMBEDDING_MODEL_ID {
        bail!(
            "base release uses embedding model `{}`; expected `{EMBEDDING_MODEL_ID}`",
            manifest.model.id
        );
    }
    if parse_hf_model_url(&manifest.model.url).is_some()
        && manifest.model.sha256 != EMBEDDING_MODEL_FINGERPRINT
    {
        bail!("base release embedding fingerprint differs from the current pinned model");
    }
    Ok(())
}

pub(crate) fn check_base_release(base_dir: &Path) -> Result<()> {
    let manifest_path = base_dir.join("manifest.json");
    let db_path = base_dir.join("ato.db");
    let packs_dir = base_dir.join("packs");
    if !manifest_path.exists() {
        bail!("base release missing {}", manifest_path.display());
    }
    if !db_path.exists() {
        bail!("base release missing {}", db_path.display());
    }
    if !packs_dir.is_dir() {
        bail!("base release missing {}", packs_dir.display());
    }
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", db_path.display()))?;
    enforce_db_schema_version(&conn)
        .with_context(|| format!("validating DB schema in {}", db_path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    validate_base_release_manifest(&manifest)?;
    for pack in &manifest.packs {
        let filename = pack_filename(&pack.url)?;
        let path = packs_dir.join(&filename);
        if !path.exists() {
            bail!("base release missing pack {}", path.display());
        }
        let len = fs::metadata(&path)?.len();
        if pack.size != 0 && len != pack.size {
            bail!(
                "base release pack size mismatch for {}: got {}, expected {}",
                path.display(),
                len,
                pack.size
            );
        }
    }
    println!(
        "base release usable: {} ({} docs, {} packs)",
        base_dir.display(),
        manifest.documents.len(),
        manifest.packs.len()
    );
    Ok(())
}

pub(crate) fn materialize_base_release(manifest_url: &str, out_dir: &Path) -> Result<()> {
    // A published current-model corpus can be reconstructed locally from
    // manifest + packs without re-running transformer embeddings.
    if out_dir.exists() {
        bail!("--out-dir already exists: {}", out_dir.display());
    }
    let source_context = UrlContext::from_manifest_url(manifest_url);
    let manifest_bytes = fetch_bytes(manifest_url, &source_context)
        .with_context(|| format!("fetching manifest from {manifest_url}"))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("parsing manifest from {manifest_url}"))?;
    validate_base_release_manifest(&manifest)?;

    let packs_dir = out_dir.join("packs");
    fs::create_dir_all(&packs_dir).with_context(|| format!("creating {}", packs_dir.display()))?;
    let mut local_manifest = manifest;
    let mut bytes_downloaded = manifest_bytes.len() as u64;
    for pack in &mut local_manifest.packs {
        let filename = pack_filename(&pack.url)?;
        let pack_url = resolve_manifest_asset(&pack.url, &source_context);
        let pack_bytes = fetch_bytes(&pack_url, &source_context)
            .with_context(|| format!("fetching {pack_url}"))?;
        if !pack.sha256.is_empty() {
            verify_sha256_bytes(&pack_bytes, &pack.sha256)
                .with_context(|| format!("verifying {}", pack.url))?;
        }
        if pack.size != 0 && pack_bytes.len() as u64 != pack.size {
            bail!(
                "pack size mismatch for {}: got {}, expected {}",
                pack.url,
                pack_bytes.len(),
                pack.size
            );
        }
        bytes_downloaded += pack_bytes.len() as u64;
        fs::write(packs_dir.join(&filename), &pack_bytes)
            .with_context(|| format!("writing {}", packs_dir.join(&filename).display()))?;
        pack.url = format!("packs/{filename}");
    }

    let manifest_path = out_dir.join("manifest.json");
    fs::write(&manifest_path, serde_json::to_vec_pretty(&local_manifest)?)
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    let summary = UpdateSummary {
        schema_version: local_manifest.schema_version,
        index_version: local_manifest.index_version.clone(),
        min_client_version: local_manifest.min_client_version.clone(),
        model: local_manifest.model.clone(),
        document_count: local_manifest.documents.len(),
        pack_count: local_manifest.packs.len(),
        manifest_fingerprint: Some(manifest_fingerprint(&local_manifest)?),
    };
    let summary_path = out_dir.join("update.json");
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)
        .with_context(|| format!("writing {}", summary_path.display()))?;

    let db_path = out_dir.join("ato.db");
    let conn = open_write_at(&db_path)?;
    init_db(&conn)?;
    let local_context = UrlContext {
        manifest_dir: Some(out_dir.to_path_buf()),
        manifest_base_url: None,
    };
    let tx = conn.unchecked_transaction()?;
    let mut bytes_read_from_local_packs = 0_u64;
    let apply_result = (|| -> Result<()> {
        insert_docs_from_packs(
            &tx,
            &local_manifest,
            &local_context,
            &local_manifest.documents,
            &mut bytes_read_from_local_packs,
            out_dir,
        )?;
        set_meta(&tx, "index_version", &local_manifest.index_version)?;
        set_meta(&tx, "embedding_model_id", &local_manifest.model.id)?;
        set_meta(&tx, "last_update_at", &Utc::now().to_rfc3339())?;
        derive_citations(&tx)?;
        verify_semantic_install(&tx, &local_manifest)?;
        Ok(())
    })();
    if let Err(err) = apply_result {
        tx.rollback()?;
        return Err(err);
    }
    tx.commit()?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    eprintln!(
        "ato-mcp materialize-base-release: wrote {} from {} ({} docs, {} packs, {} bytes)",
        out_dir.display(),
        manifest_url,
        local_manifest.documents.len(),
        local_manifest.packs.len(),
        bytes_downloaded
    );
    Ok(())
}

pub(crate) fn seed_build_from_base_release(
    base_dir: &Path,
    out_dir: &Path,
    db_path: &Path,
) -> Result<BaseReleaseSeed> {
    if out_dir.exists()
        && base_dir.exists()
        && out_dir.canonicalize().ok() == base_dir.canonicalize().ok()
    {
        bail!("--base-release-dir must not point at --out-dir");
    }
    if db_path.exists() {
        bail!(
            "--base-release-dir requires an absent --db-path; remove {} before seeding",
            db_path.display()
        );
    }
    let manifest_path = base_dir.join("manifest.json");
    let db_src = base_dir.join("ato.db");
    if !manifest_path.exists() {
        bail!("base release missing {}", manifest_path.display());
    }
    if !db_src.exists() {
        bail!("base release missing {}", db_src.display());
    }
    let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    validate_base_release_manifest(&manifest)?;
    let base_packs_dir = base_dir.join("packs");
    let mut source_hash_by_doc_id = HashMap::new();
    let pack_index: HashMap<String, PackInfo> = manifest
        .packs
        .iter()
        .map(|pack| (pack.sha8.clone(), pack.clone()))
        .collect();
    let mut docs_by_pack: HashMap<String, Vec<&DocRef>> = HashMap::new();
    for doc in &manifest.documents {
        docs_by_pack
            .entry(doc.pack_sha8.clone())
            .or_default()
            .push(doc);
    }
    for (sha8, docs) in docs_by_pack {
        let pack = pack_index
            .get(&sha8)
            .ok_or_else(|| anyhow!("base manifest missing pack info for {sha8}"))?;
        let filename = pack_filename(&pack.url)?;
        let pack_path = base_packs_dir.join(filename);
        let pack_bytes =
            fs::read(&pack_path).with_context(|| format!("reading {}", pack_path.display()))?;
        if !pack.sha256.is_empty() {
            verify_sha256_bytes(&pack_bytes, &pack.sha256)
                .with_context(|| format!("verifying {}", pack_path.display()))?;
        }
        if pack.size != 0 && pack_bytes.len() as u64 != pack.size {
            bail!(
                "base pack size mismatch for {}: got {}, expected {}",
                pack_path.display(),
                pack_bytes.len(),
                pack.size
            );
        }
        for doc in docs {
            let record = read_record_from_pack_bytes(&pack_bytes, doc.offset, doc.length)
                .with_context(|| format!("reading base pack record {}", doc.doc_id))?;
            // [IB-12] Previous-release reuse is keyed by a full
            // source-derived fingerprint, not just cleaned body text.
            let source_hash =
                source_fingerprint_hash(&pack_record_source_fingerprint_value(&record))
                    .with_context(|| format!("hashing base source record {}", doc.doc_id))?;
            source_hash_by_doc_id.insert(doc.doc_id.clone(), source_hash);
        }
    }
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&db_src, db_path).with_context(|| {
        format!(
            "copying base DB {} to {}",
            db_src.display(),
            db_path.display()
        )
    })?;
    let out_packs_dir = out_dir.join("packs");
    fs::create_dir_all(&out_packs_dir)?;
    for pack in &manifest.packs {
        let filename = pack_filename(&pack.url)?;
        copy_or_hard_link(
            &base_packs_dir.join(&filename),
            &out_packs_dir.join(&filename),
        )?;
    }
    let by_doc_id = manifest
        .documents
        .iter()
        .map(|doc| (doc.doc_id.clone(), doc.clone()))
        .collect();
    Ok(BaseReleaseSeed {
        documents: manifest.documents,
        packs: manifest.packs,
        by_doc_id,
        source_hash_by_doc_id,
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
    let (mut documents, mut packs) = match checkpoint {
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
            if let Some(base_dir) = base_release_dir {
                let seed = seed_build_from_base_release(base_dir, out_dir, db_path)?;
                eprintln!(
                    "ato-mcp build: seeded from base release {} ({} docs, {} packs)",
                    base_dir.display(),
                    seed.documents.len(),
                    seed.packs.len()
                );
                base_doc_refs = seed.by_doc_id;
                base_documents = seed.documents.clone();
                base_source_hash_by_doc_id = seed.source_hash_by_doc_id;
                base_seeded = true;
                (seed.documents, seed.packs)
            } else {
                (Vec::new(), Vec::new())
            }
        }
    };
    let conn = open_write_at(db_path)
        .with_context(|| format!("opening sqlite at {}", db_path.display()))?;
    init_db(&conn)?;
    clean_stale_build_packs(out_dir, &packs)?;
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

    // Pack records collected for this build. Each record is the full doc
    // payload the Rust updater can ingest from `pack-<sha8>.bin.zst`.
    let mut pack_records: Vec<(String, JsonValue)> = Vec::new();
    let mut pending_embeddings: Vec<PendingBuildEmbedding> =
        Vec::with_capacity(BUILD_EMBED_BATCH_SIZE);
    let mut doc_hashes: HashMap<String, String> = HashMap::new();
    let mut pack_shard_idx: usize = packs.len();

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

        let started = std::time::Instant::now();
        tx.execute(
            "INSERT INTO documents
                (doc_id, type, title, date, downloaded_at, content_hash, pack_sha8,
                 html, withdrawn_date, superseded_by, replaces,
                 has_in_doc_links, has_related_docs, has_history)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            ],
        )
        .context("INSERT documents")?;

        // Insert chunks + embeddings + chunks_fts; also collect a record
        // entry for pack writing.
        let mut chunk_ids: Vec<(i64, String, Option<String>)> = Vec::new();
        let mut chunk_records: Vec<JsonValue> = Vec::new();
        let mut doc_pending_embeddings: Vec<(i64, usize, String)> = Vec::new();
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

            let chunk_idx = chunk_records.len();
            chunk_records.push(json!({
                "ord": chunk.ord,
                "anchor": chunk.anchor.clone(),
                "text": chunk.text.clone(),
                "embedding_b64": JsonValue::Null,
            }));
            doc_pending_embeddings.push((chunk_id, chunk_idx, chunk.text.clone()));
        }

        // title_fts: concat headings into a searchable per-doc row.
        let frag = scraper::Html::parse_fragment(&final_html);
        let h_sel = scraper::Selector::parse("h1, h2, h3, h4, h5, h6").unwrap();
        let headings_concat: Vec<String> = frag
            .select(&h_sel)
            .map(anchors_node_text)
            .filter(|s| !s.is_empty())
            .collect();
        let headings_text = headings_concat.join(" ");
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

        // Pack record (in-memory; written at end of build).
        let started = std::time::Instant::now();
        let doc_idx = pack_records.len();
        pack_records.push((
            doc_id.clone(),
            json!({
                "doc_id": doc_id,
                "type": doc_type,
                "title": title,
                "date": derived_date,
                "downloaded_at": now,
                "content_hash": content_hash,
                "html": final_html,
                "withdrawn_date": currency.withdrawn_date,
                "superseded_by": currency.superseded_by,
                "replaces": currency.replaces,
                "has_in_doc_links": has_in_doc_links as i64,
                "has_related_docs": has_related_docs as i64,
                "has_history": has_history as i64,
                "anchors": anchor_records,
                "definitions": definition_records,
                "chunks": chunk_records,
                "assets": assets.iter().map(|a| json!({
                    "asset_ref": a.asset_ref.clone(),
                    "source_path": a.source_path.clone(),
                    "relative_path": a.relative_path.clone(),
                    "media_type": a.media_type.clone(),
                    "alt": a.alt.clone(),
                    "title": a.title.clone(),
                    "sha256": a.sha256.clone(),
                    "size": a.size,
                    "data_b64": a.data_b64.clone(),
                })).collect::<Vec<_>>(),
            }),
        ));
        profile.pack += started.elapsed();
        for (chunk_id, chunk_idx, text) in doc_pending_embeddings {
            pending_embeddings.push(PendingBuildEmbedding {
                chunk_id,
                doc_idx,
                chunk_idx,
                text,
            });
        }
        if pending_embeddings.len() >= BUILD_EMBED_PENDING_FLUSH_CHUNKS {
            flush_pending_build_embeddings(
                &state,
                &tx,
                &mut pending_embeddings,
                &mut pack_records,
                &mut profile,
            )?;
        }
        if pack_records.len() >= BUILD_PACK_RECORDS_PER_SHARD {
            flush_pending_build_embeddings(
                &state,
                &tx,
                &mut pending_embeddings,
                &mut pack_records,
                &mut profile,
            )?;
            let first_new_doc = documents.len();
            write_build_pack_shard(
                pack_shard_idx,
                &mut pack_records,
                &mut BuildPackShardContext {
                    out_dir,
                    zstd_level,
                    doc_hashes: &doc_hashes,
                    documents: &mut documents,
                    packs: &mut packs,
                    profile: &mut profile,
                },
            )?;
            let started = std::time::Instant::now();
            update_pack_sha8_for_docs(&tx, &documents[first_new_doc..])?;
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
            pack_shard_idx += 1;
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
        &mut pack_records,
        &mut profile,
    )?;
    let first_new_doc = documents.len();
    write_build_pack_shard(
        pack_shard_idx,
        &mut pack_records,
        &mut BuildPackShardContext {
            out_dir,
            zstd_level,
            doc_hashes: &doc_hashes,
            documents: &mut documents,
            packs: &mut packs,
            profile: &mut profile,
        },
    )?;
    let started = std::time::Instant::now();
    update_pack_sha8_for_docs(&tx, &documents[first_new_doc..])?;
    tx.commit()?;
    let used_pack_sha8: HashSet<String> =
        documents.iter().map(|doc| doc.pack_sha8.clone()).collect();
    packs.retain(|pack| used_pack_sha8.contains(&pack.sha8));
    clean_stale_build_packs(out_dir, &packs)?;
    if documents.len() > first_new_doc || base_seeded || removed_base_docs > 0 {
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
    }
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
        documents,
        packs,
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
        document_count: manifest.documents.len(),
        pack_count: manifest.packs.len(),
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
