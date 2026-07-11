//! Corpus build orchestrator: ingests the ATO source inventory and normalized
//! Federal Register documents, runs source-specific extraction plus shared
//! chunking and embedding, and emits a source-qualified corpus manifest.
//! Includes checkpoint resume and offline-bundle manifest localization.

use crate::chunker::{
    chunk_html_with_token_count, Chunk, CHUNKER_FORMAT_VERSION, EMBED_MAX_TOKENS,
};
use crate::db::{compress_text, init_db, open_write_at, set_corpus_meta, set_source_meta};
use crate::extract::{
    anchors_node_text, extract_anchors, extract_compose_title, extract_currency,
    extract_definitions, extract_em_front_matter, extract_leading_headings, metadata_doc_id_for,
    metadata_extract_pub_date, metadata_parse_docid, rewrite_images_html, AnchorRef, CurrencyInfo,
    DefinitionChunk, ExtractedAsset,
};
use crate::frl::load_normalized_documents;
use crate::html::{
    canonical_ato_native_id, clean_ato_html, normalise_named_anchors, rewrite_links_html,
    strip_attributes,
};
use crate::pipeline::{finalise_source_ann, ingest_source};
use crate::retrieval::derive_citations;
use crate::rules::{derive_metadata, RuleInputs};
use crate::semantic::{SemanticEncodeStats, SemanticModelPaths};
use crate::source::{verify_semantic_install, Manifest, ManifestDb, ModelInfo};
use legal_model::{DocumentId, SourceId};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocRef {
    pub(crate) document_id: DocumentId,
    pub(crate) content_hash: String,
}
use crate::{
    ServerState, EMBEDDING_DIM, EMBEDDING_INPUT_MAX_TOKENS, EMBEDDING_MODEL_FINGERPRINT,
    EMBEDDING_MODEL_HF_SIZE, EMBEDDING_MODEL_HF_URL, EMBEDDING_MODEL_ID, SUPPORTED_SCHEMA_VERSION,
};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) const BUILD_EMBED_BATCH_SIZE: usize = 32;
pub(crate) const BUILD_EMBED_PENDING_FLUSH_CHUNKS: usize = 4096;
pub(crate) const BUILD_CHECKPOINT_SCHEMA_VERSION: u32 = 3;
pub(crate) const ATO_SOURCE_ID: &str = "ato";

// ----- Deterministic corpus build orchestrator -----
//
// Walks pages_dir/index.jsonl, runs each doc through the cleaning + chunker
// + rules-engine metadata classifier + embedder pipeline in-process, writes
// documents + chunks + chunk_embeddings + chunks_fts + title_fts +
// doc_anchors + definitions + citations rows, then writes the manifest.json
// to --out-dir.

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
        // --profile reports stage timing plus embedding batch,
        // token, padding, and model-throughput counters for build tuning.
        let total = self.elapsed().as_secs_f64().max(0.000_001);
        eprintln!(
            "legal-mcp build profile: docs={} chunks={} html_mb={:.1} total_s={:.2} docs_per_s={:.2}",
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
            "legal-mcp build: processed {processed} source docs ({:.1}/s, rebuilt {rebuilt}, reused {reused})",
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
                "legal-mcp build: embedding batch of {} exceeded GPU memory; retrying as {} + {}",
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
    let mut text_by_sha256 = BTreeMap::<String, String>::new();
    let mut pending_hashes = Vec::with_capacity(pending.len());
    for item in pending.iter() {
        let text_sha256 = format!("{:x}", Sha256::digest(item.text.as_bytes()));
        match text_by_sha256.entry(text_sha256.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(item.text.clone());
            }
            std::collections::btree_map::Entry::Occupied(entry) if entry.get() != &item.text => {
                bail!("distinct chunk texts produced the same SHA-256 digest");
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
        pending_hashes.push((item.chunk_id, text_sha256));
    }

    let mut vectors = HashMap::<String, Vec<u8>>::new();
    let mut missing = Vec::<(String, String)>::new();
    let mut select = conn.prepare(
        "SELECT embedding FROM embedding_cache WHERE model_id = ?1 AND text_sha256 = ?2",
    )?;
    for (text_sha256, text) in text_by_sha256 {
        let cached = select
            .query_row(rusqlite::params![EMBEDDING_MODEL_ID, &text_sha256], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .optional()?;
        if let Some(cached) = cached {
            if cached.len() != EMBEDDING_DIM {
                bail!("cached embedding has invalid dimensions");
            }
            vectors.insert(text_sha256, cached);
        } else {
            missing.push((text_sha256, text));
        }
    }
    drop(select);
    missing.sort_by_key(|(_, text)| text.len());

    for batch in missing.chunks(BUILD_EMBED_BATCH_SIZE) {
        let inputs = batch
            .iter()
            .map(|(_, text)| text.clone())
            .collect::<Vec<_>>();
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
        for ((text_sha256, _), emb) in batch.iter().zip(embeddings.iter()) {
            let bytes = emb.iter().map(|value| *value as u8).collect::<Vec<_>>();
            conn.execute(
                "INSERT INTO embedding_cache(model_id, text_sha256, embedding)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![EMBEDDING_MODEL_ID, text_sha256, &bytes],
            )
            .context("INSERT embedding_cache")?;
            vectors.insert(text_sha256.clone(), bytes);
        }
        profile.embedding_write += write_started.elapsed();
    }

    let write_started = std::time::Instant::now();
    for (chunk_id, text_sha256) in pending_hashes {
        let embedding = vectors
            .get(&text_sha256)
            .ok_or_else(|| anyhow!("missing cached embedding for chunk text {text_sha256}"))?;
        conn.execute(
            "INSERT INTO chunk_embeddings (chunk_id, embedding) VALUES (?1, ?2)",
            rusqlite::params![chunk_id, embedding],
        )
        .context("INSERT chunk_embeddings")?;
    }
    profile.embedding_write += write_started.elapsed();
    pending.clear();
    profile.embedding += started.elapsed();
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildCheckpoint {
    pub(crate) schema_version: u32,
    pub(crate) source_id: SourceId,
    pub(crate) source_index_sha256: String,
    pub(crate) zstd_level: i32,
    pub(crate) embedding_model_id: String,
    pub(crate) embedding_model_fingerprint: String,
    pub(crate) embedding_dim: usize,
    pub(crate) embedding_input_max_tokens: usize,
    pub(crate) chunker_format_version: u32,
    pub(crate) documents: Vec<DocRef>,
}

pub(crate) fn build_checkpoint_path(out_dir: &Path) -> PathBuf {
    out_dir.join("build-state.json")
}

pub(crate) fn load_build_checkpoint(
    out_dir: &Path,
    source_id: &SourceId,
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
    if &checkpoint.source_id != source_id {
        bail!(
            "build checkpoint source `{}` differs from requested `{source_id}`; remove {} to start a fresh build",
            checkpoint.source_id,
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

pub(crate) struct SaveBuildCheckpointArgs<'a> {
    pub(crate) out_dir: &'a Path,
    pub(crate) source_id: &'a SourceId,
    pub(crate) source_index_sha256: &'a str,
    pub(crate) zstd_level: i32,
    pub(crate) documents: &'a [DocRef],
}

pub(crate) fn save_build_checkpoint(args: SaveBuildCheckpointArgs<'_>) -> Result<()> {
    let checkpoint = BuildCheckpoint {
        schema_version: BUILD_CHECKPOINT_SCHEMA_VERSION,
        source_id: args.source_id.clone(),
        source_index_sha256: args.source_index_sha256.to_string(),
        zstd_level: args.zstd_level,
        embedding_model_id: EMBEDDING_MODEL_ID.to_string(),
        embedding_model_fingerprint: EMBEDDING_MODEL_FINGERPRINT.to_string(),
        embedding_dim: EMBEDDING_DIM,
        embedding_input_max_tokens: EMBEDDING_INPUT_MAX_TOKENS,
        chunker_format_version: CHUNKER_FORMAT_VERSION,
        documents: args.documents.to_vec(),
    };
    let path = build_checkpoint_path(args.out_dir);
    atomic_write(&path, &serde_json::to_vec_pretty(&checkpoint)?)
        .with_context(|| format!("writing {}", path.display()))
}

pub(crate) fn committed_build_doc_count(conn: &Connection, documents: &[DocRef]) -> Result<usize> {
    let mut stored =
        conn.prepare("SELECT content_hash FROM documents WHERE source_id = ?1 AND native_id = ?2")?;
    let mut count = 0;
    for document in documents {
        let content_hash = stored
            .query_row(
                rusqlite::params![
                    document.document_id.source.as_str(),
                    document.document_id.native_id
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        count += usize::from(content_hash.as_deref() == Some(document.content_hash.as_str()));
    }
    Ok(count)
}

pub(crate) fn remove_build_doc(conn: &Connection, source_id: &str, native_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM chunks_fts WHERE rowid IN (
            SELECT chunk_id FROM chunks WHERE source_id = ?1 AND native_id = ?2
        )",
        rusqlite::params![source_id, native_id],
    )?;
    conn.execute(
        "DELETE FROM title_fts WHERE source_id = ?1 AND native_id = ?2",
        rusqlite::params![source_id, native_id],
    )?;
    conn.execute(
        "DELETE FROM citations WHERE target_source_id = ?1 AND target_native_id = ?2",
        rusqlite::params![source_id, native_id],
    )?;
    conn.execute(
        "DELETE FROM documents WHERE source_id = ?1 AND native_id = ?2",
        rusqlite::params![source_id, native_id],
    )?;
    Ok(())
}

pub(crate) fn remove_absent_build_docs(
    conn: &Connection,
    source_id: &str,
    inventory: &HashSet<DocumentId>,
) -> Result<usize> {
    let mut select =
        conn.prepare("SELECT native_id FROM documents WHERE source_id = ?1 ORDER BY native_id")?;
    let existing = select
        .query_map([source_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let inventory_native_ids = inventory
        .iter()
        .filter(|document_id| document_id.source.as_str() == source_id)
        .map(|document_id| document_id.native_id.as_str())
        .collect::<HashSet<_>>();
    let absent = existing
        .into_iter()
        .filter(|native_id| !inventory_native_ids.contains(native_id.as_str()))
        .collect::<Vec<_>>();
    for native_id in &absent {
        remove_build_doc(conn, source_id, native_id)?;
    }
    Ok(absent.len())
}

fn compute_source_documents_by_type(
    conn: &Connection,
    source_id: &str,
) -> Result<BTreeMap<String, i64>> {
    let mut types = BTreeMap::new();
    let mut statement = conn.prepare(
        "SELECT type, COUNT(*) AS count
         FROM documents
         WHERE source_id = ?1
         GROUP BY type
         ORDER BY count DESC, type ASC",
    )?;
    let rows = statement.query_map([source_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (document_type, count) = row?;
        types.insert(document_type, count);
    }
    Ok(types)
}

fn source_description_from_title(title: &str) -> String {
    title
        .split_once(" \u{2014} ")
        .map_or(title, |(description, _)| description)
        .trim()
        .to_string()
}

fn collect_source_prefix_breakdown(conn: &Connection, source_id: &str) -> Result<Vec<JsonValue>> {
    let mut statement = conn.prepare(
        r#"
        WITH ranked AS (
          SELECT
            CASE
              WHEN INSTR(native_id, '/') > 0
                THEN UPPER(SUBSTR(native_id, 1, INSTR(native_id, '/') - 1))
              ELSE UPPER(native_id)
            END AS prefix,
            title,
            native_id
          FROM documents
          WHERE source_id = ?1
        ),
        windowed AS (
          SELECT
            prefix,
            title,
            native_id,
            COUNT(*) OVER (PARTITION BY prefix) AS document_count,
            ROW_NUMBER() OVER (
              PARTITION BY prefix
              ORDER BY
                CASE WHEN title LIKE prefix || ' %' THEN 1 ELSE 0 END,
                native_id
            ) AS row_number
          FROM ranked
        )
        SELECT prefix, document_count, title
        FROM windowed
        WHERE row_number = 1
        ORDER BY document_count DESC, prefix ASC
        "#,
    )?;
    let rows = statement.query_map([source_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (prefix, document_count, title) = row?;
        entries.push(json!({
            "prefix": prefix,
            "doc_count": document_count,
            "description": title.map(|value| source_description_from_title(&value)),
        }));
    }
    Ok(entries)
}

fn ato_document_id(canonical_id: &str) -> Result<DocumentId> {
    DocumentId::new(
        ATO_SOURCE_ID.parse()?,
        canonical_ato_native_id(&metadata_doc_id_for(canonical_id)),
    )
    .map_err(Into::into)
}

fn ato_canonical_url(native_id: &str) -> String {
    format!("https://www.ato.gov.au/law/view/document?docid={native_id}")
}

fn query_parameter(value: &str, name: &str) -> Option<String> {
    let parsed = url::Url::parse(value).ok().or_else(|| {
        let path = if value.starts_with('/') {
            value.to_string()
        } else {
            format!("/{value}")
        };
        url::Url::parse(&format!("https://www.ato.gov.au{path}")).ok()
    })?;
    parsed
        .query_pairs()
        .find(|(key, value)| key.eq_ignore_ascii_case(name) && !value.trim().is_empty())
        .map(|(_, value)| value.into_owned())
}

fn ato_upstream_version(record: &JsonValue, canonical_id: &str) -> Option<String> {
    for field in ["upstream_version", "version", "pit"] {
        if let Some(value) = record
            .get(field)
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_string());
        }
    }
    record
        .get("href")
        .and_then(JsonValue::as_str)
        .and_then(|href| query_parameter(href, "pit"))
        .or_else(|| query_parameter(canonical_id, "pit"))
}

pub(crate) struct BuildSourceFingerprint<'a> {
    pub(crate) document_id: &'a DocumentId,
    pub(crate) canonical_url: &'a str,
    pub(crate) upstream_version: &'a Option<String>,
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
        "source_id": input.document_id.source.as_str(),
        "native_id": &input.document_id.native_id,
        "canonical_url": input.canonical_url,
        "upstream_version": input.upstream_version,
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
            "target_source_id": (r.kind == "in_doc" || r.target_doc_id.is_some())
                .then(|| input.document_id.source.as_str()),
            "target_native_id": if r.kind == "in_doc" {
                Some(input.document_id.native_id.as_str())
            } else {
                r.target_doc_id.as_deref()
            },
            "target_pit": &r.target_pit,
        })).collect::<Vec<_>>(),
        "definitions": input.definitions,
        "chunks": input.chunks.iter().map(|chunk| json!({
            "ord": chunk.ord,
            "anchor": &chunk.anchor,
            "text": &chunk.text,
        })).collect::<Vec<_>>(),
        "assets": input.assets.iter().map(|asset| json!({
            "asset_id": &asset.asset_id,
            "media_type": &asset.media_type,
            "alt": &asset.alt,
            "title": &asset.title,
            "sha256": &asset.sha256,
            "size": asset.data.len(),
        })).collect::<Vec<_>>(),
    })
}

pub(crate) struct BuildCorpusArgs<'a> {
    pub(crate) pages_dir: &'a Path,
    pub(crate) frl_workspace: &'a Path,
    pub(crate) db_path: &'a Path,
    pub(crate) model_dir: &'a Path,
    pub(crate) embedding_cache_db: Option<&'a Path>,
    pub(crate) out_dir: &'a Path,
    pub(crate) zstd_level: i32,
    pub(crate) profile_enabled: bool,
}

fn load_authoritative_ato_records(index_path: &Path) -> Result<Vec<JsonValue>> {
    use std::io::BufRead as _;

    let file = File::open(index_path)
        .with_context(|| format!("opening source index {}", index_path.display()))?;
    let mut latest = BTreeMap::<String, Option<JsonValue>>::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: JsonValue = serde_json::from_str(&line).context("parsing index.jsonl line")?;
        let canonical_id = record
            .get("canonical_id")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| anyhow!("index record missing canonical_id"))?
            .to_owned();
        let inventory_key = ato_document_id(&canonical_id)?.native_id;
        let has_payload = record
            .get("payload_path")
            .and_then(JsonValue::as_str)
            .is_some_and(|path| !path.is_empty())
            && record.get("size").and_then(JsonValue::as_u64).is_some()
            && record
                .get("sha256")
                .and_then(JsonValue::as_str)
                .is_some_and(|digest| digest.len() == 64);
        let status = record.get("status").and_then(JsonValue::as_str);
        if has_payload && status == Some("success") {
            latest.insert(inventory_key, Some(record));
        } else if matches!(status, Some("confirmed_404" | "confirmed_stub")) {
            latest.insert(inventory_key, None);
        } else {
            latest.entry(inventory_key).or_insert(None);
        }
    }
    Ok(latest.into_values().flatten().collect())
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
        pages_dir,
        frl_workspace,
        db_path,
        model_dir,
        embedding_cache_db,
        out_dir,
        zstd_level,
        profile_enabled,
    } = args;

    let pages_root = pages_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing pages_dir {}", pages_dir.display()))?;
    let frl_root = frl_workspace
        .canonicalize()
        .with_context(|| format!("canonicalizing FRL workspace {}", frl_workspace.display()))?;
    if pages_root == frl_root {
        bail!("ATO and FRL source workspaces must be distinct directories");
    }
    let _workspace_locks = [
        crate::source_update::lock_workspace_shared(&pages_root)?,
        crate::source_update::lock_workspace_shared(&frl_root)?,
    ];

    // Maintainer builds require a local pinned Granite model
    // checkout; hosted model metadata is owned by publish/release.
    let semantic_model_paths = SemanticModelPaths::from_model_dir(model_dir)?;
    let index_path = pages_root.join("index.jsonl");
    if fs::symlink_metadata(&index_path)?.file_type().is_symlink() {
        bail!(
            "source index must not be a symlink: {}",
            index_path.display()
        );
    }
    let source_index_sha256 = sha256_file(&index_path)?;
    let source_records = load_authoritative_ato_records(&index_path)?;

    fs::create_dir_all(out_dir)
        .with_context(|| format!("creating out_dir {}", out_dir.display()))?;
    let manifest_path = out_dir.join("manifest.json");
    if manifest_path.exists() {
        bail!(
            "refusing to mutate completed corpus output {}; choose a fresh output directory",
            out_dir.display()
        );
    }

    let source_id: SourceId = ATO_SOURCE_ID.parse()?;
    let checkpoint = load_build_checkpoint(out_dir, &source_id, &source_index_sha256, zstd_level)?;
    if checkpoint.is_none() && db_path.exists() {
        bail!(
            "refusing to reuse uncheckpointed corpus database {}; choose a fresh output directory",
            db_path.display()
        );
    }
    let mut documents = match checkpoint {
        Some(checkpoint) => {
            eprintln!(
                "legal-mcp build: resuming source {} from checkpoint ({} docs)",
                checkpoint.source_id,
                checkpoint.documents.len()
            );
            checkpoint.documents
        }
        None => {
            fs::remove_file(out_dir.join("update.json")).ok();
            Vec::new()
        }
    };
    let mut conn = open_write_at(db_path)
        .with_context(|| format!("opening sqlite at {}", db_path.display()))?;
    init_db(&conn)?;
    if let Some(seed_path) = embedding_cache_db {
        let imported = seed_embedding_cache(&conn, db_path, seed_path)?;
        eprintln!("legal-mcp build: imported {imported} reusable embeddings");
    }
    let source_display_name = &crate::legal_source::source_registry()
        .source(&source_id)?
        .descriptor()
        .display_name;
    conn.execute(
        "INSERT INTO sources(source_id, display_name) VALUES (?1, ?2)
         ON CONFLICT(source_id) DO UPDATE SET display_name = excluded.display_name",
        rusqlite::params![source_id.as_str(), source_display_name],
    )?;
    let committed_docs = committed_build_doc_count(&conn, &documents)?;
    if committed_docs != documents.len() {
        bail!(
            "build checkpoint has {} documents but only {committed_docs} are committed in the DB; remove {} to start fresh",
            documents.len(),
            build_checkpoint_path(out_dir).display()
        );
    }
    let checkpoint_document_ids: HashSet<DocumentId> = documents
        .iter()
        .map(|doc| doc.document_id.clone())
        .collect();
    if checkpoint_document_ids.len() != documents.len()
        || checkpoint_document_ids
            .iter()
            .any(|document_id| document_id.source != source_id)
    {
        bail!(
            "build checkpoint contains duplicate or foreign-source document identities; remove {} to start fresh",
            build_checkpoint_path(out_dir).display()
        );
    }

    let mut profile = BuildProfile::new(profile_enabled);
    // Corpus build runs as a single Rust process with adaptive
    // embedding batches and no separate worker-pool build path.
    let state = ServerState::with_model_paths(semantic_model_paths);
    let mut processed = checkpoint_document_ids.len();
    let mut skipped_no_payload: usize = 0;
    let mut skipped_duplicate_documents: usize = 0;
    let mut source_inventory: HashSet<DocumentId> = HashSet::new();
    let mut tx = conn.unchecked_transaction()?;
    let progress_started = std::time::Instant::now();

    let mut pending_embeddings: Vec<PendingBuildEmbedding> =
        Vec::with_capacity(BUILD_EMBED_BATCH_SIZE);
    for record in source_records {
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
        let document_id = ato_document_id(canonical_id)?;
        if !source_inventory.insert(document_id.clone()) {
            skipped_duplicate_documents += 1;
            continue;
        }
        if checkpoint_document_ids.contains(&document_id) {
            processed += 1;
            maybe_report_build_progress(processed, profile.docs, 0, progress_started);
            continue;
        }
        let payload_path = confined_payload_path(&pages_root, payload_path_raw)?;
        let native_id = document_id.native_id.clone();
        let canonical_url = ato_canonical_url(&native_id);
        let upstream_version = ato_upstream_version(&record, canonical_id);

        let started = std::time::Instant::now();
        let expected_size = record
            .get("size")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| anyhow!("index record missing payload size for {canonical_id}"))?;
        let expected_sha256 = record
            .get("sha256")
            .and_then(JsonValue::as_str)
            .filter(|sha| sha.len() == 64)
            .ok_or_else(|| anyhow!("index record missing payload sha256 for {canonical_id}"))?;
        let actual_size = fs::metadata(&payload_path)?.len();
        let actual_sha256 = sha256_file(&payload_path)?;
        if actual_size != expected_size || actual_sha256 != expected_sha256 {
            bail!("payload integrity mismatch for {canonical_id}");
        }
        let html = fs::read_to_string(&payload_path)
            .with_context(|| format!("reading payload {}", payload_path.display()))?;
        profile.read += started.elapsed();
        profile.html_bytes += html.len() as u64;
        let doc_type = metadata_parse_docid(canonical_id).unwrap_or_default();

        // Cleaning pipeline.
        let started = std::time::Instant::now();
        let cleaned = clean_ato_html(&html);
        let (rewritten_html, assets) = rewrite_images_html(
            &cleaned.html,
            Some(&native_id),
            Some(payload_path.as_path()),
        );
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
            let h_sel = scraper::Selector::parse("h1, h2, h3, h4, h5, h6")
                .map_err(|error| anyhow!("parsing heading selector: {error:?}"))?;
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
            doc_id: native_id.clone(),
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
        let chunks =
            chunk_html_with_token_count(&final_html, Some(&title), EMBED_MAX_TOKENS, |text| {
                state.count_embedding_tokens(text)
            })?;
        profile.chunking += started.elapsed();
        profile.chunks += chunks.len();

        // Anchor refs (used for navigation flags + doc_anchors table).
        let started = std::time::Instant::now();
        let anchor_refs = extract_anchors(&final_html, &native_id);
        let has_in_doc_links = anchor_refs.iter().any(|r| r.kind == "in_doc");
        let has_related_docs = anchor_refs.iter().any(|r| r.kind == "sister");
        let has_history = anchor_refs.iter().any(|r| r.kind == "history");
        profile.references += started.elapsed();

        // Definitions participate in the source fingerprint and are inserted
        // after their source-qualified document row.
        let started = std::time::Instant::now();
        let def_chunks: Vec<DefinitionChunk> = chunks
            .iter()
            .map(|c| DefinitionChunk {
                ord: c.ord,
                anchor: c.anchor.clone(),
                text: c.text.clone(),
            })
            .collect();
        let defs = extract_definitions(&native_id, &title, &doc_type, &def_chunks);
        let definition_records: Vec<JsonValue> = defs
            .iter()
            .map(|d| {
                json!({
                    "source_id": source_id.as_str(),
                    "definition_id": d.definition_id.clone(),
                    "term": d.term.clone(),
                    "norm_term": d.norm_term.clone(),
                    "native_id": native_id.clone(),
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
                document_id: &document_id,
                canonical_url: &canonical_url,
                upstream_version: &upstream_version,
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
        remove_build_doc(&tx, source_id.as_str(), &native_id)
            .with_context(|| format!("replacing source document {document_id}"))?;

        let downloaded_at = record
            .get("downloaded_at")
            .and_then(JsonValue::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

        // Collect headings once: stored on documents.headings for install-time
        // FTS5 rebuild, and re-used below to populate title_fts.headings.
        let headings_frag = scraper::Html::parse_fragment(&final_html);
        let h_sel = scraper::Selector::parse("h1, h2, h3, h4, h5, h6")
            .map_err(|error| anyhow!("parsing heading selector: {error:?}"))?;
        let headings_concat: Vec<String> = headings_frag
            .select(&h_sel)
            .map(anchors_node_text)
            .filter(|s| !s.is_empty())
            .collect();
        let headings_text = headings_concat.join(" ");

        let started = std::time::Instant::now();
        tx.execute(
            "INSERT INTO documents
                (source_id, native_id, type, title, date, canonical_url, upstream_version,
                 downloaded_at, content_hash, html, withdrawn_date, superseded_by, replaces,
                 has_in_doc_links, has_related_docs, has_history, headings)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            rusqlite::params![
                source_id.as_str(),
                &native_id,
                &doc_type,
                &title,
                &derived_date,
                &canonical_url,
                &upstream_version,
                downloaded_at,
                &source_hash,
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
                    "INSERT INTO chunks (source_id, native_id, ord, anchor, text)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 RETURNING chunk_id",
                    rusqlite::params![
                        source_id.as_str(),
                        &native_id,
                        chunk.ord,
                        chunk.anchor,
                        zstd_text
                    ],
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
                    "INSERT chunks_fts document={} chunk_id={} ord={}",
                    document_id, chunk_id, chunk.ord
                )
            })?;

            doc_pending_embeddings.push((chunk_id, chunk.text.clone()));
        }

        // title_fts: re-use the headings collected before the documents INSERT.
        tx.execute(
            "INSERT INTO title_fts (source_id, native_id, title, headings)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![source_id.as_str(), &native_id, &title, headings_text],
        )
        .context("INSERT title_fts")?;

        // doc_anchors.
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
            let target_native_id = if r.kind == "in_doc" {
                Some(native_id.as_str())
            } else {
                r.target_doc_id.as_deref()
            };
            let target_source_id = target_native_id.map(|_| source_id.as_str());
            tx.execute(
                "INSERT INTO doc_anchors
                    (source_id, native_id, ord, kind, label, target_chunk_id,
                     target_source_id, target_native_id, target_pit)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    source_id.as_str(),
                    &native_id,
                    anchor_ord,
                    r.kind,
                    r.label,
                    target_chunk_id,
                    target_source_id,
                    target_native_id,
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
                "INSERT INTO definitions
                    (source_id, definition_id, term, norm_term, native_id, source_title,
                     source_type, scope, anchor, ord, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    source_id.as_str(),
                    d.definition_id,
                    d.term,
                    d.norm_term,
                    &native_id,
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

        // Asset persistence: store each image inline in document_assets.data.
        let started = std::time::Instant::now();
        for asset in &assets {
            tx.execute(
                "INSERT INTO document_assets
                    (source_id, asset_id, native_id, media_type, alt, title, sha256, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    source_id.as_str(),
                    asset.asset_id,
                    &native_id,
                    asset.media_type,
                    asset.alt,
                    asset.title,
                    asset.sha256,
                    asset.data,
                ],
            )
            .context("INSERT document_assets")?;
        }
        profile.assets += started.elapsed();

        documents.push(DocRef {
            document_id: document_id.clone(),
            content_hash: source_hash,
        });

        for (chunk_id, text) in doc_pending_embeddings {
            pending_embeddings.push(PendingBuildEmbedding { chunk_id, text });
        }
        if pending_embeddings.len() >= BUILD_EMBED_PENDING_FLUSH_CHUNKS {
            flush_pending_build_embeddings(&state, &tx, &mut pending_embeddings, &mut profile)?;
            // Checkpoint periodically so a long build can resume from an
            // interrupted partial DB.
            let started = std::time::Instant::now();
            tx.commit()?;
            save_build_checkpoint(SaveBuildCheckpointArgs {
                out_dir,
                source_id: &source_id,
                source_index_sha256: &source_index_sha256,
                zstd_level,
                documents: &documents,
            })?;
            profile.checkpoint += started.elapsed();
            tx = conn.unchecked_transaction()?;
        }

        processed += 1;
        profile.docs += 1;
        maybe_report_build_progress(processed, profile.docs, 0, progress_started);
    }

    let removed_documents = remove_absent_build_docs(&tx, source_id.as_str(), &source_inventory)?;
    documents.retain(|doc| source_inventory.contains(&doc.document_id));

    flush_pending_build_embeddings(&state, &tx, &mut pending_embeddings, &mut profile)?;
    tx.commit()?;
    let started = std::time::Instant::now();
    save_build_checkpoint(SaveBuildCheckpointArgs {
        out_dir,
        source_id: &source_id,
        source_index_sha256: &source_index_sha256,
        zstd_level,
        documents: &documents,
    })?;
    profile.checkpoint += started.elapsed();
    if skipped_no_payload > 0 {
        eprintln!(
            "legal-mcp build: skipped {skipped_no_payload} index records without payload_path"
        );
    }
    if skipped_duplicate_documents > 0 {
        eprintln!(
            "legal-mcp build: skipped {skipped_duplicate_documents} duplicate source document records"
        );
    }
    if removed_documents > 0 {
        eprintln!(
            "legal-mcp build: removed {removed_documents} ATO documents absent from the source inventory"
        );
    }

    let started = std::time::Instant::now();
    let created_at = chrono::Utc::now().to_rfc3339();
    let mut manifest = Manifest {
        schema_version: SUPPORTED_SCHEMA_VERSION,
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
        // FTS5 and zstd-compressing the canonical legal.db.
        db: ManifestDb {
            url: "legal.db.zst".to_string(),
            sha256: String::new(),
            size: 0,
        },
        ann: BTreeMap::new(),
    };

    let final_tx = conn.unchecked_transaction()?;
    set_corpus_meta(&final_tx, "index_version", &manifest.index_version)?;
    set_corpus_meta(&final_tx, "embedding_model_id", &manifest.model.id)?;
    set_corpus_meta(&final_tx, "last_update_at", &manifest.created_at)?;
    set_source_meta(
        &final_tx,
        source_id.as_str(),
        "source_index_sha256",
        &source_index_sha256,
    )?;
    eprintln!("legal-mcp build: deriving citations…");
    derive_citations(&final_tx, &source_id)?;
    // Precompute the corpus-shape values runtime stats() returns so MCP
    // `initialize` becomes a meta key/value read (sub-ms) instead of a
    // multi-table COUNT(*) + GROUP BY scan (5-10s cold on a multi-GB DB).
    // The corpus is read-only for the server lifetime — `legal-mcp update`
    // requires a restart — so caching at build time is safe.
    let documents_count: i64 =
        final_tx.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?;
    let chunks_count: i64 = final_tx.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
    let chunk_embeddings_count: i64 =
        final_tx.query_row("SELECT COUNT(*) FROM chunk_embeddings", [], |r| r.get(0))?;
    let definitions_count: i64 =
        final_tx.query_row("SELECT COUNT(*) FROM definitions", [], |r| r.get(0))?;
    set_corpus_meta(&final_tx, "documents_count", &documents_count.to_string())?;
    set_corpus_meta(&final_tx, "chunks_count", &chunks_count.to_string())?;
    set_corpus_meta(
        &final_tx,
        "chunk_embeddings_count",
        &chunk_embeddings_count.to_string(),
    )?;
    set_corpus_meta(
        &final_tx,
        "definitions_count",
        &definitions_count.to_string(),
    )?;

    let source_documents_count: i64 = final_tx.query_row(
        "SELECT COUNT(*) FROM documents WHERE source_id = ?1",
        [source_id.as_str()],
        |row| row.get(0),
    )?;
    let source_chunks_count: i64 = final_tx.query_row(
        "SELECT COUNT(*) FROM chunks WHERE source_id = ?1",
        [source_id.as_str()],
        |row| row.get(0),
    )?;
    let source_chunk_embeddings_count: i64 = final_tx.query_row(
        "SELECT COUNT(*)
         FROM chunk_embeddings AS embedding
         JOIN chunks AS chunk ON chunk.chunk_id = embedding.chunk_id
         WHERE chunk.source_id = ?1",
        [source_id.as_str()],
        |row| row.get(0),
    )?;
    let source_definitions_count: i64 = final_tx.query_row(
        "SELECT COUNT(*) FROM definitions WHERE source_id = ?1",
        [source_id.as_str()],
        |row| row.get(0),
    )?;
    for (key, value) in [
        ("documents_count", source_documents_count),
        ("chunks_count", source_chunks_count),
        ("chunk_embeddings_count", source_chunk_embeddings_count),
        ("definitions_count", source_definitions_count),
    ] {
        set_source_meta(&final_tx, source_id.as_str(), key, &value.to_string())?;
    }
    let documents_by_type = compute_source_documents_by_type(&final_tx, source_id.as_str())?;
    set_source_meta(
        &final_tx,
        source_id.as_str(),
        "documents_by_type_json",
        &serde_json::to_string(&documents_by_type)?,
    )?;
    let prefix_breakdown = collect_source_prefix_breakdown(&final_tx, source_id.as_str())?;
    set_source_meta(
        &final_tx,
        source_id.as_str(),
        "prefix_breakdown_json",
        &serde_json::to_string(&prefix_breakdown)?,
    )?;
    let ann_identity = crate::ann::compute_identity(&final_tx, &source_id, &source_index_sha256)?;
    set_source_meta(
        &final_tx,
        source_id.as_str(),
        "corpus_id",
        &ann_identity.corpus_id,
    )?;
    set_source_meta(
        &final_tx,
        source_id.as_str(),
        "embedding_set_sha256",
        &ann_identity.embedding_set_sha256,
    )?;
    final_tx.commit()?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

    eprintln!("legal-mcp build: ingesting normalized Federal Register documents…");
    let frl_source: SourceId = crate::frl::FRL_SOURCE_ID.parse()?;
    let frl_descriptor = crate::legal_source::source_registry()
        .source(&frl_source)?
        .descriptor()
        .clone();
    let frl_documents = load_normalized_documents(&frl_root)?;
    let frl_report = ingest_source(
        &mut conn,
        &frl_source,
        &frl_descriptor,
        frl_documents,
        &state,
    )?;
    eprintln!(
        "legal-mcp build: FRL ingested {} documents ({} changed, {} removed, {} embeddings encoded, {} reused)",
        frl_report.inserted_documents + frl_report.unchanged_documents,
        frl_report.changed_documents,
        frl_report.deleted_documents,
        frl_report.encoded_texts,
        frl_report.reused_embeddings,
    );
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

    // Source ingestion owns source-local state. The completed generation owns
    // the one corpus-wide timestamp and version recorded in its manifest.
    let binding_tx = conn.unchecked_transaction()?;
    set_corpus_meta(&binding_tx, "index_version", &manifest.index_version)?;
    set_corpus_meta(&binding_tx, "embedding_model_id", &manifest.model.id)?;
    set_corpus_meta(&binding_tx, "last_update_at", &manifest.created_at)?;
    binding_tx.commit()?;

    eprintln!("legal-mcp build: constructing deterministic ANN sidecar…");
    let ann = crate::ann::build_sidecar(
        &conn,
        &source_id,
        out_dir,
        &source_index_sha256,
        &ann_identity,
    )?;
    manifest.ann.insert(source_id, ann);
    let frl_ann = finalise_source_ann(&conn, &frl_source, out_dir)?;
    manifest.ann.insert(frl_source, frl_ann);
    crate::source::verify_corpus_manifest_binding(&conn, &manifest)?;
    verify_semantic_install(&conn, &manifest)?;

    atomic_write(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)?;
    eprintln!("legal-mcp build: wrote {}", manifest_path.display());
    profile.finalise += started.elapsed();
    profile.print();

    eprintln!(
        "legal-mcp build: done - {processed} docs written to {}",
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

fn confined_payload_path(pages_root: &Path, raw: &str) -> Result<PathBuf> {
    use std::path::Component;

    if raw.is_empty()
        || raw.contains(['\\', ':'])
        || raw.starts_with(['/', '\\'])
        || raw.contains(['?', '#'])
    {
        bail!("unsafe payload path `{raw}`");
    }
    let relative = Path::new(raw);
    let mut components = relative.components();
    if components.next() != Some(Component::Normal("payloads".as_ref()))
        || components.any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("payload path must be a relative path beneath payloads/: `{raw}`");
    }
    let mut current = pages_root.to_path_buf();
    for component in relative.components() {
        if matches!(component, Component::CurDir) {
            continue;
        }
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("reading payload path component {}", current.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("payload path contains symlink {}", current.display());
        }
    }
    let canonical = current.canonicalize()?;
    if !canonical.starts_with(pages_root) || !canonical.is_file() {
        bail!("payload path escaped {}", pages_root.display());
    }
    Ok(canonical)
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

/// Strip FTS5 indexes from a copy of the canonical legal.db, VACUUM, and
/// zstd-compress to produce a shippable artifact. The input file is never
/// mutated. Returns {path, sha256, size} for embedding into manifest.json.
pub(crate) fn package_corpus(db_path: &Path, out: &Path, level: i32) -> Result<JsonValue> {
    use std::io::{copy as io_copy, BufReader, BufWriter, Write as _};

    if !db_path.is_file() {
        bail!("input DB not found: {}", db_path.display());
    }
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }

    let staging = tempfile::tempdir().context("creating staging dir")?;
    let staged_db = staging.path().join("stage.db");
    fs::copy(db_path, &staged_db)
        .with_context(|| format!("copying {} → {}", db_path.display(), staged_db.display()))?;

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
    let parent = out.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary artifact in {}", parent.display()))?;
    let mut reader = BufReader::new(input);
    let writer = BufWriter::new(temp.as_file_mut());
    let mut encoder = zstd::stream::Encoder::new(writer, level).context("creating zstd encoder")?;
    encoder
        .long_distance_matching(true)
        .context("enabling zstd long-distance matching")?;
    io_copy(&mut reader, &mut encoder).context("compressing staged DB")?;
    let mut writer = encoder.finish().context("finalising zstd stream")?;
    writer.flush()?;
    drop(writer);
    temp.as_file().sync_all()?;

    // Decode the completed temporary stream before promotion so a truncated
    // or otherwise invalid zstd artifact can never replace the prior output.
    {
        use std::io::Seek as _;
        temp.as_file_mut().rewind()?;
        let mut decoder = zstd::stream::read::Decoder::new(temp.as_file_mut())
            .context("validating compressed corpus artifact")?;
        std::io::copy(&mut decoder, &mut std::io::sink())
            .context("validating compressed corpus artifact")?;
    }

    // sha256 + size of the compressed artifact.
    let sha256 = sha256_file(temp.path()).context("hashing compressed artifact")?;
    let size = temp.as_file().metadata()?.len();
    if size == 0 {
        bail!("compressed corpus artifact is empty");
    }
    temp.persist(out)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replacing {}", out.display()))?;
    sync_parent(out)?;
    if sha256_file(out)? != sha256 || fs::metadata(out)?.len() != size {
        bail!("persisted corpus artifact failed integrity verification");
    }

    Ok(json!({
        "path": out.display().to_string(),
        "sha256": sha256,
        "size": size,
    }))
}

/// Update a manifest.json in-place so its `db` field points at the freshly
/// packaged legal.db.zst. The URL is set to the bare filename; the publish
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
    let actual_size = fs::metadata(artifact_path)
        .with_context(|| format!("reading {} metadata", artifact_path.display()))?
        .len();
    let actual_sha256 = sha256_file(artifact_path)?;
    if actual_size != size || actual_sha256 != sha256 {
        bail!(
            "artifact summary does not match {}",
            artifact_path.display()
        );
    }
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
        json!(SUPPORTED_SCHEMA_VERSION),
    );
    let pretty = serde_json::to_vec_pretty(&value)?;
    atomic_write(manifest_path, &pretty)
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod security_tests {
    use super::*;

    #[test]
    fn build_payloads_are_canonically_confined() -> Result<()> {
        let root = tempfile::tempdir()?;
        let payloads = root.path().join("payloads");
        fs::create_dir(&payloads)?;
        let payload = payloads.join("doc.html");
        fs::write(&payload, "doc")?;
        let canonical_root = root.path().canonicalize()?;
        assert_eq!(
            confined_payload_path(&canonical_root, "payloads/doc.html")?,
            payload.canonicalize()?
        );
        for raw in [
            "../secret",
            "/etc/passwd",
            r"payloads\..\secret",
            r"C:\secret",
            "doc.html",
            "payloads/doc.html?ignored",
        ] {
            assert!(
                confined_payload_path(&canonical_root, raw).is_err(),
                "accepted {raw}"
            );
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&payload, payloads.join("linked.html"))?;
            assert!(confined_payload_path(&canonical_root, "payloads/linked.html").is_err());
        }
        Ok(())
    }

    #[test]
    fn atomic_write_replaces_only_with_complete_bytes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("manifest.json");
        fs::write(&path, b"old")?;
        atomic_write(&path, b"new complete value")?;
        assert_eq!(fs::read(&path)?, b"new complete value");
        Ok(())
    }

    #[test]
    fn checkpoint_document_refs_are_structurally_source_qualified() -> Result<()> {
        let document_id = DocumentId::new("ato".parse()?, "TR/2026/1")?;
        let encoded = serde_json::to_value(DocRef {
            document_id: document_id.clone(),
            content_hash: "sha256:abc".to_string(),
        })?;
        assert_eq!(encoded["document_id"]["source"], "ato");
        assert_eq!(encoded["document_id"]["native_id"], "TR/2026/1");
        assert_eq!(
            serde_json::from_value::<DocRef>(encoded)?.document_id,
            document_id
        );
        assert!(serde_json::from_value::<DocRef>(serde_json::json!({
            "doc_id": "TR/2026/1",
            "content_hash": "sha256:abc"
        }))
        .is_err());
        Ok(())
    }

    #[test]
    fn ato_inventory_uses_latest_usable_payload_and_authoritative_removals() -> Result<()> {
        let root = tempfile::tempdir()?;
        let index = root.path().join("index.jsonl");
        let records = [
            json!({"canonical_id":"/law/view/document?docid=a", "status":"success", "payload_path":"one.html", "size":1, "sha256":"1".repeat(64)}),
            json!({"canonical_id":"/law/view/document?docid=A", "status":"failed", "payload_path":null}),
            json!({"canonical_id":"B", "status":"success", "payload_path":"gone.html", "size":1, "sha256":"2".repeat(64)}),
            json!({"canonical_id":"B", "status":"confirmed_404", "payload_path":null}),
            json!({"canonical_id":"C", "status":"failed", "payload_path":null}),
            json!({"canonical_id":"C", "status":"success", "payload_path":"current.html", "size":1, "sha256":"3".repeat(64)}),
            json!({"canonical_id":"/law/view/document?docid=A#A", "status":"success", "payload_path":"two.html", "size":1, "sha256":"4".repeat(64)}),
        ];
        fs::write(
            &index,
            records
                .iter()
                .map(serde_json::to_string)
                .collect::<std::result::Result<Vec<_>, _>>()?
                .join("\n"),
        )?;
        let authoritative = load_authoritative_ato_records(&index)?;
        assert_eq!(authoritative.len(), 2);
        assert_eq!(
            authoritative[0]["canonical_id"],
            "/law/view/document?docid=A#A"
        );
        assert_eq!(authoritative[0]["payload_path"], "two.html");
        assert_eq!(authoritative[1]["canonical_id"], "C");
        assert_eq!(authoritative[1]["payload_path"], "current.html");
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
    fn ato_provenance_is_authoritative_and_versioned_when_available() {
        let native_id = "TXR/TR2026-001/NAT/ATO/00001";
        assert_eq!(
            ato_canonical_url(native_id),
            "https://www.ato.gov.au/law/view/document?docid=TXR/TR2026-001/NAT/ATO/00001"
        );
        let record = serde_json::json!({
            "href": "/law/view/document?docid=TXR/TR2026-001/NAT/ATO/00001&PiT=20260701000001"
        });
        assert_eq!(
            ato_upstream_version(&record, native_id).as_deref(),
            Some("20260701000001")
        );
        assert_eq!(
            ato_upstream_version(
                &serde_json::json!({"upstream_version": "edition-4"}),
                native_id
            )
            .as_deref(),
            Some("edition-4")
        );
    }

    fn source_scoped_deletion_connection() -> Result<Connection> {
        let conn = Connection::open_in_memory()?;
        init_db(&conn)?;
        conn.execute_batch(
            r#"
            INSERT INTO sources(source_id, display_name) VALUES
                ('ato', 'Australian Taxation Office'),
                ('frl', 'Federal Register of Legislation');
            INSERT INTO documents(
                source_id, native_id, type, title, canonical_url, downloaded_at,
                content_hash, html
            ) VALUES
                ('ato', 'shared', 'ruling', 'ATO shared', 'https://ato.example/shared',
                 '2026-07-11T00:00:00Z', 'ato-hash', X'00'),
                ('frl', 'shared', 'act', 'FRL shared', 'https://frl.example/shared',
                 '2026-07-11T00:00:00Z', 'frl-hash', X'00');
            INSERT INTO chunks(chunk_id, source_id, native_id, ord, text) VALUES
                (1, 'ato', 'shared', 0, X'00'),
                (2, 'frl', 'shared', 0, X'00');
            INSERT INTO chunks_fts(rowid, text) VALUES
                (1, 'ATO chunk'),
                (2, 'FRL chunk');
            INSERT INTO title_fts(source_id, native_id, title, headings) VALUES
                ('ato', 'shared', 'ATO shared', ''),
                ('frl', 'shared', 'FRL shared', '');
            INSERT INTO citations(
                source_chunk_id, source_id, source_native_id,
                target_source_id, target_native_id
            ) VALUES
                (1, 'ato', 'shared', 'frl', 'shared'),
                (2, 'frl', 'shared', 'ato', 'shared');
            "#,
        )?;
        Ok(conn)
    }

    #[test]
    fn remove_build_doc_cleans_fts_and_cascades_only_one_source() -> Result<()> {
        let conn = source_scoped_deletion_connection()?;
        remove_build_doc(&conn, "ato", "shared")?;

        let ato_documents: i64 = conn.query_row(
            "SELECT COUNT(*) FROM documents WHERE source_id = 'ato'",
            [],
            |row| row.get(0),
        )?;
        let frl_documents: i64 = conn.query_row(
            "SELECT COUNT(*) FROM documents WHERE source_id = 'frl'",
            [],
            |row| row.get(0),
        )?;
        let mut chunks_fts_statement =
            conn.prepare("SELECT rowid FROM chunks_fts ORDER BY rowid")?;
        let remaining_chunk_fts: Vec<i64> = chunks_fts_statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut title_statement =
            conn.prepare("SELECT source_id FROM title_fts ORDER BY source_id")?;
        let title_sources: Vec<String> = title_statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let citations: i64 =
            conn.query_row("SELECT COUNT(*) FROM citations", [], |row| row.get(0))?;

        assert_eq!(ato_documents, 0);
        assert_eq!(frl_documents, 1);
        assert_eq!(remaining_chunk_fts, vec![2]);
        assert_eq!(title_sources, vec!["frl"]);
        assert_eq!(citations, 0);
        Ok(())
    }

    #[test]
    fn source_inventory_difference_deletes_absent_rows_without_cross_source_effects() -> Result<()>
    {
        let conn = source_scoped_deletion_connection()?;
        conn.execute(
            "INSERT INTO documents(
                source_id, native_id, type, title, canonical_url, downloaded_at,
                content_hash, html
             ) VALUES ('ato', 'keep', 'ruling', 'Keep', 'https://ato.example/keep',
                       '2026-07-11T00:00:00Z', 'keep-hash', X'00')",
            [],
        )?;
        let inventory = HashSet::from([DocumentId::new("ato".parse()?, "keep")?]);

        assert_eq!(remove_absent_build_docs(&conn, "ato", &inventory)?, 1);
        let mut remaining_statement = conn
            .prepare("SELECT source_id, native_id FROM documents ORDER BY source_id, native_id")?;
        let remaining: Vec<(String, String)> = remaining_statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(
            remaining,
            vec![
                ("ato".to_string(), "keep".to_string()),
                ("frl".to_string(), "shared".to_string()),
            ]
        );
        Ok(())
    }
}
