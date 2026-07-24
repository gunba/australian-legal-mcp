//! Source-agnostic ingestion of normalized legal documents into the final corpus schema.
//!
//! Acquisition and source-specific cleaning stop at [`NormalizedDocument`]. This module owns the
//! final, source-qualified SQLite reconciliation, shared chunking, embedding reuse, citation
//! derivation, metadata refresh, and per-source ANN finalisation.

use crate::ann::{self, ManifestAnn};
use crate::chunker::{chunk_fragment_with_prepared_tokens, Chunk, EMBED_MAX_TOKENS};
use crate::db::{get_corpus_meta, get_source_meta, set_corpus_meta, set_source_meta};
use crate::extract::{
    extract_anchors_from_document, extract_currency_from_document, extract_definitions, AnchorRef,
    CurrencyInfo, Definition, DefinitionChunk,
};
use crate::semantic::EMBEDDING_BATCH_SIZE;
use crate::source_catalog::ATO_SOURCE_ID;
use crate::{ServerState, EMBEDDING_DIM, EMBEDDING_MODEL_ID};
use anyhow::{anyhow, bail, Context, Result};
#[cfg(test)]
use legal_model::DocumentId;
use legal_model::{encode_public_component, SourceDescriptor, SourceId};
use legal_source_sdk::{sha256_bytes, NormalizedDocument};
use rayon::prelude::*;
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;

const EMBEDDING_CACHE_LOOKUP_BATCH_SIZE: usize = 500;
const PRODUCTION_EMBEDDING_FLUSH_SIZE: usize = 8_192;
const EMBEDDING_FLUSH_SIZE: usize = PRODUCTION_EMBEDDING_FLUSH_SIZE;
const DOCUMENT_PREPARATION_BATCH_SIZE: usize = 256;

pub(crate) struct PreparedEmbeddingText {
    text: String,
    token_count: usize,
    token_ids: Option<Vec<i64>>,
}

/// The only embedding capability the final-schema pipeline needs.
///
/// Implementations encode unprefixed chunk text. The production semantic runtime applies its
/// configured document prefix itself, exactly as it does for the existing corpus builder.
pub(crate) trait EmbeddingProvider: Sync {
    fn model_id(&self) -> &str;

    fn count_tokens(&self, text: &str) -> Result<usize>;

    fn encode(&self, texts: &[String]) -> Result<Vec<[i8; EMBEDDING_DIM]>>;

    fn prepare_document_tokens(&self, _text: &str) -> Result<Option<Vec<i64>>> {
        Ok(None)
    }

    fn prepare_embedding_tokens_exact(&self, _text: &str) -> Result<Option<Vec<i64>>> {
        Ok(None)
    }

    fn encode_prepared(
        &self,
        inputs: &[&PreparedEmbeddingText],
    ) -> Result<Vec<[i8; EMBEDDING_DIM]>> {
        self.encode(
            &inputs
                .iter()
                .map(|input| input.text.clone())
                .collect::<Vec<_>>(),
        )
    }
}

impl EmbeddingProvider for ServerState {
    fn model_id(&self) -> &str {
        EMBEDDING_MODEL_ID
    }

    fn count_tokens(&self, text: &str) -> Result<usize> {
        self.count_embedding_tokens(text)
    }

    fn encode(&self, texts: &[String]) -> Result<Vec<[i8; EMBEDDING_DIM]>> {
        self.encode_document_embeddings(texts)
    }

    fn prepare_document_tokens(&self, text: &str) -> Result<Option<Vec<i64>>> {
        self.prepare_document_embedding_tokens(text).map(Some)
    }

    fn prepare_embedding_tokens_exact(&self, text: &str) -> Result<Option<Vec<i64>>> {
        self.prepare_embedding_tokens_exact(text).map(Some)
    }

    fn encode_prepared(
        &self,
        inputs: &[&PreparedEmbeddingText],
    ) -> Result<Vec<[i8; EMBEDDING_DIM]>> {
        let token_ids = inputs
            .iter()
            .map(|input| {
                input
                    .token_ids
                    .as_deref()
                    .ok_or_else(|| anyhow!("production embedding input is missing prepared tokens"))
            })
            .collect::<Result<Vec<_>>>()?;
        self.encode_document_token_embeddings(&token_ids)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceIngestReport {
    pub(crate) source_id: SourceId,
    pub(crate) inserted_documents: usize,
    pub(crate) changed_documents: usize,
    pub(crate) unchanged_documents: usize,
    pub(crate) deleted_documents: usize,
    pub(crate) inserted_chunks: usize,
    /// Unique chunk texts sent to the model.
    pub(crate) encoded_texts: usize,
    /// New chunk rows whose vectors did not require another model input.
    pub(crate) reused_embeddings: usize,
    pub(crate) source_index_sha256: String,
}

struct PreparedDocument {
    document: NormalizedDocument,
    content_hash: String,
    headings: String,
    chunks: Vec<Chunk>,
    currency: CurrencyInfo,
    anchor_refs: Vec<AnchorRef>,
    definitions: Vec<Definition>,
}

struct PendingEmbedding {
    chunk_id: i64,
    text_sha256: String,
}

#[derive(Default)]
struct IngestBuffers {
    seen_asset_ids: HashSet<String>,
    pending_embeddings: Vec<PendingEmbedding>,
    text_by_sha256: BTreeMap<String, PreparedEmbeddingText>,
}

struct RawEmbeddingJob {
    pending: Vec<PendingEmbedding>,
    text_by_sha256: BTreeMap<String, PreparedEmbeddingText>,
}

struct ResolvedEmbeddingJob {
    pending: Vec<PendingEmbedding>,
    vectors: HashMap<String, Vec<u8>>,
    missing: Vec<(String, PreparedEmbeddingText)>,
}

struct EncodedEmbeddingJob {
    pending: Vec<PendingEmbedding>,
    vectors: HashMap<String, Vec<u8>>,
    new_hashes: Vec<String>,
}

/// Reconcile one complete, authoritative source snapshot into an existing final-schema database.
///
/// `documents` is the complete current source inventory, not merely a delta. Documents absent from
/// it are deleted. The supplied source and descriptor must identify the same single source, and
/// every normalized document must carry that source. All SQLite changes occur in one immediate
/// transaction; source-specific normalization and ANN publication deliberately remain outside this
/// function.
#[cfg(test)]
pub(crate) fn ingest_source<P, I>(
    conn: &mut Connection,
    source_id: &SourceId,
    descriptor: &SourceDescriptor,
    documents: I,
    embeddings: &P,
) -> Result<SourceIngestReport>
where
    P: EmbeddingProvider + ?Sized,
    I: IntoIterator<Item = NormalizedDocument>,
{
    ingest_source_results(
        conn,
        source_id,
        descriptor,
        documents.into_iter().map(Ok),
        embeddings,
    )
}

/// Stream a source workspace without hydrating its document bodies in memory.
pub(crate) fn ingest_source_results<P, I>(
    conn: &mut Connection,
    source_id: &SourceId,
    descriptor: &SourceDescriptor,
    documents: I,
    embeddings: &P,
) -> Result<SourceIngestReport>
where
    P: EmbeddingProvider + ?Sized,
    I: IntoIterator<Item = Result<NormalizedDocument>>,
{
    validate_source_input(source_id, descriptor, embeddings.model_id())?;

    // Cascades are part of the final schema contract. Enabling this before beginning the
    // transaction also avoids SQLite's no-op behaviour for PRAGMA foreign_keys inside a txn.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let enabled: i64 = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if enabled != 1 {
        bail!("final-schema ingestion requires SQLite foreign-key enforcement");
    }

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let report = ingest_source_transaction(&tx, source_id, descriptor, documents, embeddings)?;
    tx.commit()?;
    Ok(report)
}

fn validate_source_input(
    source_id: &SourceId,
    descriptor: &SourceDescriptor,
    model_id: &str,
) -> Result<()> {
    if &descriptor.id != source_id {
        bail!(
            "source descriptor `{}` cannot ingest source `{source_id}`",
            descriptor.id
        );
    }
    if descriptor.display_name.trim().is_empty() {
        bail!("source `{source_id}` has an empty display name");
    }
    if model_id.trim().is_empty() {
        bail!("embedding provider model id must be nonempty");
    }
    if model_id != EMBEDDING_MODEL_ID {
        bail!("final corpus requires embedding model `{EMBEDDING_MODEL_ID}`, got `{model_id}`");
    }
    Ok(())
}

fn source_snapshot_sha256(source_id: &SourceId, documents: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"australian-legal-mcp-source-snapshot-v1\0");
    hash_field(&mut hasher, source_id.as_str().as_bytes());
    for (native_id, content_hash) in documents {
        hash_field(&mut hasher, native_id.as_bytes());
        hash_field(&mut hasher, content_hash.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn ingest_source_transaction<P>(
    tx: &Transaction<'_>,
    source_id: &SourceId,
    descriptor: &SourceDescriptor,
    documents: impl IntoIterator<Item = Result<NormalizedDocument>>,
    embeddings: &P,
) -> Result<SourceIngestReport>
where
    P: EmbeddingProvider + ?Sized,
{
    validate_embedding_model(tx, embeddings.model_id())?;

    let existing = existing_source_hashes(tx, source_id)?;
    let mut unchanged_documents = 0usize;
    let mut inserted_documents = 0usize;
    let mut changed_documents = 0usize;
    tx.execute(
        "INSERT INTO sources(source_id, display_name) VALUES (?1, ?2)
         ON CONFLICT(source_id) DO UPDATE SET display_name = excluded.display_name",
        params![source_id.as_str(), descriptor.display_name],
    )?;

    let downloaded_at = chrono::Utc::now().to_rfc3339();
    let mut inserted_chunks = 0usize;
    let mut encoded_texts = 0usize;
    let mut reused_embeddings = 0usize;
    let mut buffers = IngestBuffers::default();
    let mut incoming_hashes = BTreeMap::new();
    let mut text_compressor =
        zstd::bulk::Compressor::new(3).context("creating reusable corpus text compressor")?;

    std::thread::scope(|scope| -> Result<()> {
        let mut documents = documents.into_iter();
        let size_hint = documents.size_hint();
        let expected_documents = (size_hint.1 == Some(size_hint.0)).then_some(size_hint.0);
        let progress_started = std::time::Instant::now();
        let mut processed_documents = 0usize;
        let mut queued_jobs = VecDeque::<RawEmbeddingJob>::new();
        let mut active_job: Option<std::thread::ScopedJoinHandle<'_, Result<EncodedEmbeddingJob>>> =
            None;

        loop {
            let batch = documents
                .by_ref()
                .take(DOCUMENT_PREPARATION_BATCH_SIZE)
                .collect::<Vec<_>>();
            if batch.is_empty() {
                break;
            }
            let prepared = batch
                .into_par_iter()
                .map(|document| prepare_source_document(document, source_id, &existing, embeddings))
                .collect::<Vec<_>>();
            for prepared in prepared {
                let (native_id, content_hash, prepared_document) = prepared?;
                if incoming_hashes
                    .insert(native_id.clone(), content_hash)
                    .is_some()
                {
                    bail!("duplicate normalized document `{source_id}/{native_id}`");
                }
                processed_documents += 1;
                if processed_documents.is_multiple_of(1_000) {
                    let elapsed = progress_started.elapsed().as_secs_f64().max(0.001);
                    let rate = processed_documents as f64 / elapsed;
                    let eta = expected_documents
                        .map(|total| total.saturating_sub(processed_documents) as f64 / rate);
                    eprintln!(
                        "legal-mcp build: {source_id} prepared {processed_documents}/{} documents ({rate:.1}/s, {} chunks, {} embeddings encoded, eta {})",
                        expected_documents
                            .map(|total| total.to_string())
                            .unwrap_or_else(|| "?".to_string()),
                        inserted_chunks,
                        encoded_texts,
                        eta.map(|seconds| format!("{seconds:.0}s"))
                            .unwrap_or_else(|| "unknown".to_string()),
                    );
                }
                let Some(prepared_document) = prepared_document else {
                    unchanged_documents += 1;
                    continue;
                };
                if existing.contains_key(&native_id) {
                    changed_documents += 1;
                    delete_source_document(tx, source_id, &native_id)?;
                } else {
                    inserted_documents += 1;
                }
                insert_document(
                    tx,
                    source_id,
                    &prepared_document,
                    &downloaded_at,
                    &mut buffers,
                    &mut text_compressor,
                )?;
                inserted_chunks += prepared_document.chunks.len();
                if buffers.pending_embeddings.len() >= EMBEDDING_FLUSH_SIZE {
                    queued_jobs.push_back(RawEmbeddingJob {
                        pending: std::mem::take(&mut buffers.pending_embeddings),
                        text_by_sha256: std::mem::take(&mut buffers.text_by_sha256),
                    });
                }
            }

            if active_job
                .as_ref()
                .is_some_and(std::thread::ScopedJoinHandle::is_finished)
            {
                let job = active_job
                    .take()
                    .expect("finished embedding job is present")
                    .join()
                    .map_err(|_| anyhow!("embedding worker panicked"))??;
                if let Some(raw) = queued_jobs.pop_front() {
                    let resolved = resolve_embedding_job(tx, embeddings, raw, Some(&job.vectors))?;
                    active_job =
                        Some(scope.spawn(move || encode_embedding_job(embeddings, resolved)));
                }
                let (encoded, reused) = persist_embedding_job(tx, embeddings.model_id(), job)?;
                encoded_texts += encoded;
                reused_embeddings += reused;
            }
            if active_job.is_none() {
                if let Some(raw) = queued_jobs.pop_front() {
                    let resolved = resolve_embedding_job(tx, embeddings, raw, None)?;
                    active_job =
                        Some(scope.spawn(move || encode_embedding_job(embeddings, resolved)));
                }
            }

            // Bound prepared-but-not-embedded text while leaving enough work
            // queued to keep CUDA busy across document preparation and SQLite writes.
            if queued_jobs.len() >= 2 {
                let raw = queued_jobs
                    .pop_front()
                    .expect("embedding queue backpressure requires queued work");
                let job = active_job
                    .take()
                    .expect("embedding queue backpressure requires an active job")
                    .join()
                    .map_err(|_| anyhow!("embedding worker panicked"))??;
                let resolved = resolve_embedding_job(tx, embeddings, raw, Some(&job.vectors))?;
                active_job = Some(scope.spawn(move || encode_embedding_job(embeddings, resolved)));
                let (encoded, reused) = persist_embedding_job(tx, embeddings.model_id(), job)?;
                encoded_texts += encoded;
                reused_embeddings += reused;
            }
        }

        if !buffers.pending_embeddings.is_empty() {
            queued_jobs.push_back(RawEmbeddingJob {
                pending: std::mem::take(&mut buffers.pending_embeddings),
                text_by_sha256: std::mem::take(&mut buffers.text_by_sha256),
            });
        }
        while active_job.is_some() || !queued_jobs.is_empty() {
            if let Some(handle) = active_job.take() {
                let next_raw = queued_jobs.pop_front();
                let job = handle
                    .join()
                    .map_err(|_| anyhow!("embedding worker panicked"))??;
                if let Some(raw) = next_raw {
                    let resolved = resolve_embedding_job(tx, embeddings, raw, Some(&job.vectors))?;
                    active_job =
                        Some(scope.spawn(move || encode_embedding_job(embeddings, resolved)));
                }
                let (encoded, reused) = persist_embedding_job(tx, embeddings.model_id(), job)?;
                encoded_texts += encoded;
                reused_embeddings += reused;
            } else if let Some(raw) = queued_jobs.pop_front() {
                let resolved = resolve_embedding_job(tx, embeddings, raw, None)?;
                active_job = Some(scope.spawn(move || encode_embedding_job(embeddings, resolved)));
            }
        }
        Ok(())
    })?;

    let absent_ids = existing
        .keys()
        .filter(|native_id| !incoming_hashes.contains_key(*native_id))
        .cloned()
        .collect::<Vec<_>>();
    for native_id in &absent_ids {
        delete_source_document(tx, source_id, native_id)?;
    }
    let source_index_sha256 = source_snapshot_sha256(source_id, &incoming_hashes);
    crate::retrieval::derive_citations(tx, source_id)?;
    refresh_metadata(
        tx,
        source_id,
        embeddings.model_id(),
        &source_index_sha256,
        &downloaded_at,
    )?;

    Ok(SourceIngestReport {
        source_id: source_id.clone(),
        inserted_documents,
        changed_documents,
        unchanged_documents,
        deleted_documents: absent_ids.len(),
        inserted_chunks,
        encoded_texts,
        reused_embeddings,
        source_index_sha256,
    })
}

fn prepare_source_document<P>(
    document: Result<NormalizedDocument>,
    source_id: &SourceId,
    existing: &BTreeMap<String, String>,
    embeddings: &P,
) -> Result<(String, String, Option<PreparedDocument>)>
where
    P: EmbeddingProvider + ?Sized,
{
    let document = document?;
    document
        .validate()
        .with_context(|| format!("validating normalized document for source `{source_id}`"))?;
    let identity = &document.inventory.document;
    if &identity.source != source_id {
        bail!(
            "normalized document `{identity}` belongs to source `{}`, expected `{source_id}`",
            identity.source
        );
    }
    let native_id = identity.native_id.clone();
    let content_hash = document
        .normalized_sha256()
        .with_context(|| format!("hashing normalized document `{identity}`"))?;
    if existing.get(&native_id) == Some(&content_hash) {
        return Ok((native_id, content_hash, None));
    }
    let fragment = scraper::Html::parse_fragment(&document.html);
    let headings = headings_text(&fragment)?;
    let mut chunks = chunk_fragment_with_prepared_tokens(
        &fragment,
        Some(document.inventory.title.as_str()),
        EMBED_MAX_TOKENS,
        |text| {
            if let Some(token_ids) = embeddings.prepare_embedding_tokens_exact(text)? {
                Ok((token_ids.len(), Some(token_ids)))
            } else {
                Ok((embeddings.count_tokens(text)?, None))
            }
        },
    )
    .with_context(|| format!("chunking normalized document `{identity}`"))?;
    for chunk in &mut chunks {
        if chunk.embedding_token_ids.is_none() {
            chunk.embedding_token_ids = embeddings.prepare_document_tokens(&chunk.text)?;
        }
        if let Some(token_ids) = &chunk.embedding_token_ids {
            if token_ids.len() != chunk.token_count {
                bail!(
                    "prepared token count changed for `{identity}` chunk {}: counted {}, prepared {}",
                    chunk.ord,
                    chunk.token_count,
                    token_ids.len()
                );
            }
        }
    }
    let page = scraper::Html::parse_document(&document.html);
    let currency = extract_currency_from_document(&page);
    let anchor_refs = extract_anchors_from_document(&page, &native_id);
    let definition_chunks = chunks
        .iter()
        .map(|chunk| DefinitionChunk {
            ord: chunk.ord,
            anchor: chunk.anchor.clone(),
            text: chunk.text.clone(),
        })
        .collect::<Vec<_>>();
    let definitions = extract_definitions(
        &native_id,
        &document.inventory.title,
        &document.inventory.document_type,
        &definition_chunks,
    );
    Ok((
        native_id,
        content_hash.clone(),
        Some(PreparedDocument {
            document,
            content_hash,
            headings,
            chunks,
            currency,
            anchor_refs,
            definitions,
        }),
    ))
}

fn validate_embedding_model(conn: &Connection, model_id: &str) -> Result<()> {
    if let Some(stored) = get_corpus_meta(conn, "embedding_model_id")? {
        if stored != model_id {
            bail!(
                "corpus embedding model is `{stored}`, but source ingestion requested `{model_id}`"
            );
        }
    }
    Ok(())
}

fn existing_source_hashes(
    conn: &Connection,
    source_id: &SourceId,
) -> Result<BTreeMap<String, String>> {
    let mut statement = conn.prepare(
        "SELECT native_id, content_hash FROM documents
         WHERE source_id = ?1 ORDER BY native_id",
    )?;
    let rows = statement.query_map([source_id.as_str()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut hashes = BTreeMap::new();
    for row in rows {
        let (native_id, content_hash) = row?;
        hashes.insert(native_id, content_hash);
    }
    Ok(hashes)
}

fn delete_source_document(conn: &Connection, source_id: &SourceId, native_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM documents WHERE source_id = ?1 AND native_id = ?2",
        params![source_id.as_str(), native_id],
    )?;
    Ok(())
}

fn headings_text(fragment: &scraper::Html) -> Result<String> {
    let selector = scraper::Selector::parse("h1, h2, h3, h4, h5, h6")
        .map_err(|error| anyhow!("parsing heading selector: {error:?}"))?;
    Ok(fragment
        .select(&selector)
        .map(|heading| {
            heading
                .text()
                .flat_map(str::split_whitespace)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|heading| !heading.is_empty())
        .collect::<Vec<_>>()
        .join(" "))
}

fn insert_document(
    conn: &Connection,
    source_id: &SourceId,
    prepared: &PreparedDocument,
    downloaded_at: &str,
    buffers: &mut IngestBuffers,
    text_compressor: &mut zstd::bulk::Compressor<'_>,
) -> Result<()> {
    let document = &prepared.document;
    let inventory = &document.inventory;
    let native_id = &inventory.document.native_id;
    let has_in_doc_links = prepared
        .anchor_refs
        .iter()
        .any(|reference| reference.kind == "in_doc");
    let has_related_docs = prepared
        .anchor_refs
        .iter()
        .any(|reference| reference.kind == "sister");
    let has_history = prepared
        .anchor_refs
        .iter()
        .any(|reference| reference.kind == "history");
    conn.prepare_cached(
        "INSERT INTO documents
            (source_id, native_id, type, title, date, canonical_url, upstream_version,
             downloaded_at, content_hash, html, withdrawn_date, superseded_by, replaces,
             has_in_doc_links, has_related_docs, has_history, headings)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                 ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
    )?
    .execute(params![
        source_id.as_str(),
        native_id,
        inventory.document_type,
        inventory.title,
        inventory.date,
        inventory.canonical_url,
        inventory.upstream_version,
        downloaded_at,
        prepared.content_hash,
        text_compressor.compress(document.html.as_bytes())?,
        prepared.currency.withdrawn_date,
        prepared.currency.superseded_by,
        prepared.currency.replaces,
        i64::from(has_in_doc_links),
        i64::from(has_related_docs),
        i64::from(has_history),
        prepared.headings,
    ])
    .with_context(|| format!("inserting document `{source_id}/{native_id}`"))?;

    let mut chunk_ids = Vec::with_capacity(prepared.chunks.len());
    let mut insert_chunk = conn.prepare_cached(
        "INSERT INTO chunks(source_id, native_id, ord, anchor, text)
         VALUES (?1, ?2, ?3, ?4, ?5)
         RETURNING chunk_id",
    )?;
    for chunk in &prepared.chunks {
        let chunk_id: i64 = insert_chunk
            .query_row(
                params![
                    source_id.as_str(),
                    native_id,
                    chunk.ord,
                    chunk.anchor,
                    text_compressor.compress(chunk.text.as_bytes())?,
                ],
                |row| row.get(0),
            )
            .with_context(|| {
                format!(
                    "inserting chunk {} for `{source_id}/{native_id}`",
                    chunk.ord
                )
            })?;
        chunk_ids.push((chunk_id, chunk.text.clone(), chunk.anchor.clone()));

        let text_sha256 = sha256_bytes(chunk.text.as_bytes());
        match buffers.text_by_sha256.entry(text_sha256.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(PreparedEmbeddingText {
                    text: chunk.text.clone(),
                    token_count: chunk.token_count,
                    token_ids: chunk.embedding_token_ids.clone(),
                });
            }
            std::collections::btree_map::Entry::Occupied(entry)
                if entry.get().text != chunk.text
                    || entry.get().token_count != chunk.token_count
                    || entry.get().token_ids != chunk.embedding_token_ids =>
            {
                bail!("distinct chunk texts or token counts produced the same SHA-256 digest");
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
        buffers.pending_embeddings.push(PendingEmbedding {
            chunk_id,
            text_sha256,
        });
    }

    drop(insert_chunk);
    let mut insert_anchor = conn.prepare_cached(
        "INSERT INTO doc_anchors(
            source_id, native_id, ord, kind, label, target_chunk_id,
            target_source_id, target_native_id, target_pit
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for (anchor_ord, reference) in (0_i64..).zip(prepared.anchor_refs.iter()) {
        let target_chunk_id = if reference.kind == "in_doc" {
            reference.target_anchor.as_deref().and_then(|name| {
                let marker = format!("[anchor:{}]", encode_public_component(name));
                chunk_ids
                    .iter()
                    .find(|(_, text, anchor)| {
                        anchor.as_deref() == Some(name) || text.contains(&marker)
                    })
                    .map(|(chunk_id, _, _)| *chunk_id)
            })
        } else {
            None
        };
        let target_native_id = if reference.kind == "in_doc" {
            Some(native_id.as_str())
        } else {
            reference.target_doc_id.as_deref()
        };
        let target_source_id = target_native_id.map(|_| source_id.as_str());
        insert_anchor.execute(params![
            source_id.as_str(),
            native_id,
            anchor_ord,
            reference.kind,
            reference.label,
            target_chunk_id,
            target_source_id,
            target_native_id,
            reference.target_pit,
        ])?;
    }

    let mut insert_definition = conn.prepare_cached(
        "INSERT INTO definitions(
            source_id, definition_id, term, norm_term, native_id, source_title,
            source_type, scope, anchor, ord, body
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    for definition in &prepared.definitions {
        insert_definition.execute(params![
            source_id.as_str(),
            definition.definition_id,
            definition.term,
            definition.norm_term,
            native_id,
            definition.source_title,
            definition.source_type,
            definition.scope,
            definition.anchor,
            definition.ord,
            definition.body,
        ])?;
    }

    let mut insert_asset = conn.prepare_cached(
        "INSERT INTO document_assets
            (source_id, asset_id, native_id, media_type, alt, title, sha256, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for asset in &document.assets {
        let asset_id = &asset.asset.asset_id;
        if !buffers.seen_asset_ids.insert(asset_id.clone()) {
            bail!("duplicate asset id `{asset_id}` while ingesting `{source_id}/{native_id}`");
        }
        insert_asset
            .execute(params![
                source_id.as_str(),
                asset_id,
                native_id,
                asset.media_type,
                asset.alt,
                asset.title,
                asset.sha256,
                asset.data,
            ])
            .with_context(|| {
                format!("inserting asset `{asset_id}` for `{source_id}/{native_id}`")
            })?;
    }
    Ok(())
}

fn resolve_embedding_job<P>(
    conn: &Connection,
    provider: &P,
    raw: RawEmbeddingJob,
    available_vectors: Option<&HashMap<String, Vec<u8>>>,
) -> Result<ResolvedEmbeddingJob>
where
    P: EmbeddingProvider + ?Sized,
{
    if raw.pending.is_empty() {
        return Ok(ResolvedEmbeddingJob {
            pending: Vec::new(),
            vectors: HashMap::new(),
            missing: Vec::new(),
        });
    }

    let mut vectors = HashMap::<String, Vec<u8>>::new();
    if let Some(available) = available_vectors {
        for text_sha256 in raw.text_by_sha256.keys() {
            if let Some(vector) = available.get(text_sha256) {
                vectors.insert(text_sha256.clone(), vector.clone());
            }
        }
    }
    let hashes = raw
        .text_by_sha256
        .keys()
        .filter(|text_sha256| !vectors.contains_key(*text_sha256))
        .collect::<Vec<_>>();
    for batch in hashes.chunks(EMBEDDING_CACHE_LOOKUP_BATCH_SIZE) {
        let placeholders = (2..batch.len() + 2)
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT text_sha256, embedding FROM embedding_cache
             WHERE model_id = ?1 AND text_sha256 IN ({placeholders})"
        );
        let values = std::iter::once(provider.model_id())
            .chain(batch.iter().map(|value| value.as_str()))
            .collect::<Vec<_>>();
        let mut select = conn.prepare_cached(&sql)?;
        let rows = select.query_map(rusqlite::params_from_iter(values), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (text_sha256, cached) = row?;
            validate_embedding_bytes(&cached, provider.model_id(), &text_sha256)?;
            vectors.insert(text_sha256, cached);
        }
    }
    let mut missing = raw
        .text_by_sha256
        .into_iter()
        .filter(|(text_sha256, _)| !vectors.contains_key(text_sha256))
        .collect::<Vec<_>>();
    missing.sort_by(|left, right| {
        left.1
            .token_count
            .cmp(&right.1.token_count)
            .then_with(|| left.0.cmp(&right.0))
    });

    Ok(ResolvedEmbeddingJob {
        pending: raw.pending,
        vectors,
        missing,
    })
}

fn encode_embedding_job<P>(
    provider: &P,
    mut job: ResolvedEmbeddingJob,
) -> Result<EncodedEmbeddingJob>
where
    P: EmbeddingProvider + ?Sized,
{
    let mut new_hashes = Vec::with_capacity(job.missing.len());
    for batch in job.missing.chunks(EMBEDDING_BATCH_SIZE) {
        let prepared_batch = batch.iter().map(|(_, input)| input).collect::<Vec<_>>();
        let encoded = provider
            .encode_prepared(&prepared_batch)
            .with_context(|| format!("encoding {} unique chunk texts", prepared_batch.len()))?;
        if encoded.len() != prepared_batch.len() {
            bail!(
                "embedding provider returned {} vectors for {} texts",
                encoded.len(),
                prepared_batch.len()
            );
        }
        for ((text_sha256, _), embedding) in batch.iter().zip(encoded) {
            let bytes = embedding
                .iter()
                .map(|value| *value as u8)
                .collect::<Vec<_>>();
            new_hashes.push(text_sha256.clone());
            job.vectors.insert(text_sha256.clone(), bytes);
        }
    }

    Ok(EncodedEmbeddingJob {
        pending: job.pending,
        vectors: job.vectors,
        new_hashes,
    })
}

fn persist_embedding_job(
    conn: &Connection,
    model_id: &str,
    job: EncodedEmbeddingJob,
) -> Result<(usize, usize)> {
    let encoded_texts = job.new_hashes.len();
    let mut insert_cache = conn.prepare_cached(
        "INSERT INTO embedding_cache(model_id, text_sha256, embedding)
         VALUES (?1, ?2, ?3)",
    )?;
    for text_sha256 in &job.new_hashes {
        let bytes = job
            .vectors
            .get(text_sha256)
            .ok_or_else(|| anyhow!("encoded embedding job omitted chunk text {text_sha256}"))?;
        insert_cache.execute(params![model_id, text_sha256, bytes])?;
    }
    drop(insert_cache);

    let mut insert_chunk =
        conn.prepare_cached("INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (?1, ?2)")?;
    for item in &job.pending {
        let embedding = job.vectors.get(&item.text_sha256).ok_or_else(|| {
            anyhow!(
                "embedding resolution omitted chunk text {}",
                item.text_sha256
            )
        })?;
        insert_chunk.execute(params![item.chunk_id, embedding])?;
    }

    Ok((
        encoded_texts,
        job.pending.len().saturating_sub(encoded_texts),
    ))
}

fn validate_embedding_bytes(bytes: &[u8], model_id: &str, text_sha256: &str) -> Result<()> {
    if bytes.len() != EMBEDDING_DIM {
        bail!(
            "cached embedding for model `{model_id}` and text `{text_sha256}` has {} bytes; expected {EMBEDDING_DIM}",
            bytes.len()
        );
    }
    Ok(())
}

fn refresh_metadata(
    conn: &Connection,
    source_id: &SourceId,
    model_id: &str,
    source_index_sha256: &str,
    updated_at: &str,
) -> Result<()> {
    set_corpus_meta(conn, "embedding_model_id", model_id)?;
    set_source_meta(
        conn,
        source_id.as_str(),
        "source_index_sha256",
        source_index_sha256,
    )?;
    set_source_meta(conn, source_id.as_str(), "last_update_at", updated_at)?;

    for (key, table) in [
        ("documents_count", "documents"),
        ("chunks_count", "chunks"),
        ("definitions_count", "definitions"),
    ] {
        let source_count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE source_id = ?1"),
            [source_id.as_str()],
            |row| row.get(0),
        )?;
        let corpus_count: i64 =
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?;
        set_source_meta(conn, source_id.as_str(), key, &source_count.to_string())?;
        set_corpus_meta(conn, key, &corpus_count.to_string())?;
    }

    let source_embeddings: i64 = conn.query_row(
        "SELECT COUNT(*) FROM chunk_embeddings AS embedding
         JOIN chunks AS chunk ON chunk.chunk_id = embedding.chunk_id
         WHERE chunk.source_id = ?1",
        [source_id.as_str()],
        |row| row.get(0),
    )?;
    let source_chunks: i64 = conn.query_row(
        "SELECT COUNT(*) FROM chunks WHERE source_id = ?1",
        [source_id.as_str()],
        |row| row.get(0),
    )?;
    if source_embeddings != source_chunks {
        bail!("source `{source_id}` has {source_chunks} chunks but {source_embeddings} embeddings");
    }
    let corpus_embeddings: i64 =
        conn.query_row("SELECT COUNT(*) FROM chunk_embeddings", [], |row| {
            row.get(0)
        })?;
    set_source_meta(
        conn,
        source_id.as_str(),
        "chunk_embeddings_count",
        &source_embeddings.to_string(),
    )?;
    set_corpus_meta(
        conn,
        "chunk_embeddings_count",
        &corpus_embeddings.to_string(),
    )?;

    let documents_by_type = source_documents_by_type(conn, source_id)?;
    set_source_meta(
        conn,
        source_id.as_str(),
        "documents_by_type_json",
        &serde_json::to_string(&documents_by_type)?,
    )?;
    if source_id.as_str() == ATO_SOURCE_ID {
        set_source_meta(
            conn,
            source_id.as_str(),
            "prefix_breakdown_json",
            &serde_json::to_string(&ato_prefix_breakdown(conn, source_id)?)?,
        )?;
    } else {
        conn.execute(
            "DELETE FROM source_meta WHERE source_id = ?1 AND key = 'prefix_breakdown_json'",
            [source_id.as_str()],
        )?;
    }

    if source_embeddings == 0 {
        conn.execute(
            "DELETE FROM source_meta
             WHERE source_id = ?1 AND key IN ('corpus_id', 'embedding_set_sha256')",
            [source_id.as_str()],
        )?;
    } else {
        let identity = ann::compute_identity(conn, source_id, source_index_sha256)?;
        set_source_meta(conn, source_id.as_str(), "corpus_id", &identity.corpus_id)?;
        set_source_meta(
            conn,
            source_id.as_str(),
            "embedding_set_sha256",
            &identity.embedding_set_sha256,
        )?;
    }
    Ok(())
}

fn source_documents_by_type(
    conn: &Connection,
    source_id: &SourceId,
) -> Result<BTreeMap<String, i64>> {
    let mut statement = conn.prepare(
        "SELECT type, COUNT(*) FROM documents
         WHERE source_id = ?1 GROUP BY type ORDER BY type",
    )?;
    let rows = statement.query_map([source_id.as_str()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut counts = BTreeMap::new();
    for row in rows {
        let (document_type, count) = row?;
        counts.insert(document_type, count);
    }
    Ok(counts)
}

fn ato_prefix_breakdown(conn: &Connection, source_id: &SourceId) -> Result<Vec<serde_json::Value>> {
    if source_id.as_str() != ATO_SOURCE_ID {
        bail!("prefix breakdown is only defined for hierarchical ATO native ids");
    }
    let mut statement = conn.prepare(
        "SELECT native_id, title FROM documents
         WHERE source_id = ?1 ORDER BY native_id",
    )?;
    let rows = statement.query_map([source_id.as_str()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut prefixes = BTreeMap::<String, (i64, String)>::new();
    for row in rows {
        let (native_id, title) = row?;
        let prefix = native_id
            .split_once('/')
            .map_or(native_id.as_str(), |(prefix, _)| prefix)
            .to_uppercase();
        let entry = prefixes.entry(prefix).or_insert((0, title));
        entry.0 += 1;
    }
    Ok(prefixes
        .into_iter()
        .map(|(prefix, (doc_count, description))| {
            json!({
                "prefix": prefix,
                "doc_count": doc_count,
                "description": description,
            })
        })
        .collect())
}

/// Build and atomically replace only `source_id`'s ANN sidecar from committed SQLite vectors.
pub(crate) fn finalise_source_ann(
    conn: &Connection,
    source_id: &SourceId,
    output_root: &Path,
) -> Result<ManifestAnn> {
    let source_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sources WHERE source_id = ?1)",
        [source_id.as_str()],
        |row| row.get(0),
    )?;
    if !source_exists {
        bail!("cannot finalise ANN for unknown source `{source_id}`");
    }
    let model_id = get_corpus_meta(conn, "embedding_model_id")?
        .ok_or_else(|| anyhow!("corpus embedding_model_id metadata is missing"))?;
    if model_id != EMBEDDING_MODEL_ID {
        bail!(
            "ANN implementation requires embedding model `{EMBEDDING_MODEL_ID}`, got `{model_id}`"
        );
    }
    let source_index_sha256 = get_source_meta(conn, source_id.as_str(), "source_index_sha256")?
        .ok_or_else(|| anyhow!("source `{source_id}` has no source_index_sha256 metadata"))?;
    let identity = ann::compute_identity(conn, source_id, &source_index_sha256)?;
    for (key, actual) in [
        ("corpus_id", identity.corpus_id.as_str()),
        (
            "embedding_set_sha256",
            identity.embedding_set_sha256.as_str(),
        ),
    ] {
        let expected = get_source_meta(conn, source_id.as_str(), key)?
            .ok_or_else(|| anyhow!("source `{source_id}` has no {key} metadata"))?;
        if expected != actual {
            bail!("source `{source_id}` {key} metadata does not match its committed embeddings");
        }
    }
    ann::build_sidecar(
        conn,
        source_id,
        output_root,
        &source_index_sha256,
        &identity,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_db;
    use crate::semantic::SemanticEncodeStats;
    use anyhow::anyhow;
    use legal_source_sdk::SourceInventoryRecord;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    };
    use tempfile::tempdir;

    struct FakeEmbeddings {
        fail: AtomicBool,
        batches: AtomicUsize,
        texts: Mutex<Vec<String>>,
    }

    impl FakeEmbeddings {
        fn new() -> Self {
            Self {
                fail: AtomicBool::new(false),
                batches: AtomicUsize::new(0),
                texts: Mutex::new(Vec::new()),
            }
        }

        fn fail(&self) {
            self.fail.store(true, Ordering::Relaxed);
        }

        fn encoded_texts(&self) -> usize {
            self.texts.lock().expect("fake embedding texts").len()
        }
    }

    impl EmbeddingProvider for FakeEmbeddings {
        fn model_id(&self) -> &str {
            EMBEDDING_MODEL_ID
        }

        fn count_tokens(&self, text: &str) -> Result<usize> {
            Ok(text.split_whitespace().count().max(1))
        }

        fn encode(&self, texts: &[String]) -> Result<Vec<[i8; EMBEDDING_DIM]>> {
            if texts.is_empty() {
                return Ok(Vec::new());
            }
            self.batches.fetch_add(1, Ordering::Relaxed);
            self.texts
                .lock()
                .expect("fake embedding texts")
                .extend(texts.iter().cloned());
            if self.fail.load(Ordering::Relaxed) {
                return Err(anyhow!("deterministic fake embedding failure"));
            }
            Ok(texts
                .iter()
                .map(|text| {
                    let digest = Sha256::digest(text.as_bytes());
                    std::array::from_fn(|index| {
                        let value = digest[index % digest.len()] as i8;
                        if value == 0 {
                            1
                        } else {
                            value
                        }
                    })
                })
                .collect())
        }
    }

    fn source(value: &str) -> SourceId {
        value.parse().expect("valid test source")
    }

    fn descriptor(source_id: &SourceId) -> SourceDescriptor {
        SourceDescriptor::new(source_id.clone(), format!("{source_id} test source"))
            .expect("valid descriptor")
    }

    fn document(source_id: &SourceId, native_id: &str, html: &str) -> NormalizedDocument {
        let inventory = SourceInventoryRecord::new(
            DocumentId::new(source_id.clone(), native_id).expect("valid document id"),
            Some("version-1".to_string()),
            format!(
                "https://example.test/{}/{}",
                source_id,
                native_id.replace('/', "-")
            ),
            "TEST",
            format!("Document {native_id}"),
            None,
            format!("payloads/{source_id}-{native_id}.html"),
            sha256_bytes(html.as_bytes()),
            html.len() as u64,
            "text/html; charset=utf-8",
        )
        .expect("valid inventory");
        NormalizedDocument::new(inventory, html, Vec::new()).expect("valid normalized document")
    }

    fn connection() -> Result<Connection> {
        let conn = Connection::open_in_memory()?;
        init_db(&conn)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS embedding_cache (
                 model_id TEXT NOT NULL,
                 text_sha256 TEXT NOT NULL,
                 embedding BLOB NOT NULL,
                 PRIMARY KEY(model_id, text_sha256)
             );",
        )?;
        Ok(conn)
    }

    fn scalar(conn: &Connection, sql: &str, source_id: &SourceId) -> Result<i64> {
        Ok(conn.query_row(sql, [source_id.as_str()], |row| row.get(0))?)
    }

    fn source_signature(
        conn: &Connection,
        source_id: &SourceId,
    ) -> Result<Vec<(String, String, i64)>> {
        let mut statement = conn.prepare(
            "SELECT document.native_id, document.content_hash, COUNT(chunk.chunk_id)
             FROM documents AS document
             LEFT JOIN chunks AS chunk
               ON chunk.source_id = document.source_id
              AND chunk.native_id = document.native_id
             WHERE document.source_id = ?1
             GROUP BY document.source_id, document.native_id, document.content_hash
             ORDER BY document.native_id",
        )?;
        let rows = statement.query_map([source_id.as_str()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    #[test]
    fn source_ingestion_never_changes_another_source() -> Result<()> {
        let mut conn = connection()?;
        let alpha = source("alpha");
        let beta = source("beta");
        let provider = FakeEmbeddings::new();
        ingest_source(
            &mut conn,
            &alpha,
            &descriptor(&alpha),
            [document(&alpha, "one", "<h1>Alpha</h1><p>old</p>")],
            &provider,
        )?;
        ingest_source(
            &mut conn,
            &beta,
            &descriptor(&beta),
            [document(
                &beta,
                "one",
                "<h1>Beta</h1><p>See [doc:alpha:one]</p>",
            )],
            &provider,
        )?;
        let before = source_signature(&conn, &beta)?;
        let beta_citations: i64 = scalar(
            &conn,
            "SELECT COUNT(*) FROM citations WHERE source_id = ?1",
            &beta,
        )?;
        assert_eq!(beta_citations, 1);

        ingest_source(
            &mut conn,
            &alpha,
            &descriptor(&alpha),
            [document(&alpha, "two", "<h1>Alpha replacement</h1>")],
            &provider,
        )?;

        assert_eq!(source_signature(&conn, &beta)?, before);
        assert_eq!(
            scalar(
                &conn,
                "SELECT COUNT(*) FROM citations WHERE source_id = ?1",
                &beta,
            )?,
            beta_citations
        );
        Ok(())
    }

    #[test]
    fn authoritative_snapshot_deletes_absent_documents_and_cascades() -> Result<()> {
        let mut conn = connection()?;
        let source_id = source("delete-test");
        let provider = FakeEmbeddings::new();
        ingest_source(
            &mut conn,
            &source_id,
            &descriptor(&source_id),
            [
                document(&source_id, "keep", "<p>keep</p>"),
                document(&source_id, "remove", "<p>remove</p>"),
            ],
            &provider,
        )?;
        let removed_chunk_id: i64 = conn.query_row(
            "SELECT chunk_id FROM chunks
             WHERE source_id = ?1 AND native_id = 'remove'",
            [source_id.as_str()],
            |row| row.get(0),
        )?;
        let report = ingest_source(
            &mut conn,
            &source_id,
            &descriptor(&source_id),
            [document(&source_id, "keep", "<p>keep</p>")],
            &provider,
        )?;

        assert_eq!(report.deleted_documents, 1);
        assert_eq!(
            scalar(
                &conn,
                "SELECT COUNT(*) FROM documents WHERE source_id = ?1",
                &source_id,
            )?,
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM chunks
                 WHERE source_id = ?1 AND native_id = 'remove'",
                [source_id.as_str()],
                |row| row.get::<_, i64>(0),
            )?,
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM chunk_embeddings WHERE chunk_id = ?1",
                [removed_chunk_id],
                |row| row.get::<_, i64>(0),
            )?,
            0
        );
        Ok(())
    }

    #[test]
    fn unchanged_content_hash_reuses_the_committed_document_without_encoding() -> Result<()> {
        let mut conn = connection()?;
        let source_id = source("unchanged-test");
        let initial = FakeEmbeddings::new();
        let doc = document(&source_id, "one", "<h1>Stable</h1><p>body</p>");
        ingest_source(
            &mut conn,
            &source_id,
            &descriptor(&source_id),
            [doc],
            &initial,
        )?;
        let chunk_id: i64 = conn.query_row(
            "SELECT chunk_id FROM chunks WHERE source_id = ?1",
            [source_id.as_str()],
            |row| row.get(0),
        )?;
        let rejecting = FakeEmbeddings::new();
        rejecting.fail();
        let report = ingest_source(
            &mut conn,
            &source_id,
            &descriptor(&source_id),
            [document(&source_id, "one", "<h1>Stable</h1><p>body</p>")],
            &rejecting,
        )?;

        assert_eq!(report.unchanged_documents, 1);
        assert_eq!(report.inserted_chunks, 0);
        assert_eq!(rejecting.encoded_texts(), 0);
        assert_eq!(
            conn.query_row(
                "SELECT chunk_id FROM chunks WHERE source_id = ?1",
                [source_id.as_str()],
                |row| row.get::<_, i64>(0),
            )?,
            chunk_id
        );
        Ok(())
    }

    #[test]
    fn identical_chunks_across_documents_reuse_the_persistent_cache() -> Result<()> {
        let mut conn = connection()?;
        let source_id = source("cache-test");
        let initial = FakeEmbeddings::new();
        ingest_source(
            &mut conn,
            &source_id,
            &descriptor(&source_id),
            [document(&source_id, "one", "<p>the same legal text</p>")],
            &initial,
        )?;
        assert_eq!(initial.encoded_texts(), 1);

        let cached_only = FakeEmbeddings::new();
        cached_only.fail();
        let report = ingest_source(
            &mut conn,
            &source_id,
            &descriptor(&source_id),
            [
                document(&source_id, "one", "<p>the same legal text</p>"),
                document(&source_id, "two", "<p>the same legal text</p>"),
            ],
            &cached_only,
        )?;

        assert_eq!(report.inserted_chunks, 1);
        assert_eq!(report.encoded_texts, 0);
        assert_eq!(report.reused_embeddings, 1);
        assert_eq!(cached_only.encoded_texts(), 0);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM embedding_cache", [], |row| {
                row.get::<_, i64>(0)
            })?,
            1
        );
        Ok(())
    }

    #[test]
    fn embedding_failure_rolls_back_deletes_rows_and_metadata() -> Result<()> {
        let mut conn = connection()?;
        let source_id = source("rollback-test");
        let initial = FakeEmbeddings::new();
        ingest_source(
            &mut conn,
            &source_id,
            &descriptor(&source_id),
            [document(&source_id, "one", "<p>committed body</p>")],
            &initial,
        )?;
        let before = source_signature(&conn, &source_id)?;
        let before_hash = get_source_meta(&conn, source_id.as_str(), "source_index_sha256")?;

        let failing = FakeEmbeddings::new();
        failing.fail();
        let error = ingest_source(
            &mut conn,
            &source_id,
            &descriptor(&source_id),
            [document(
                &source_id,
                "one",
                "<p>uncommitted replacement</p>",
            )],
            &failing,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("deterministic fake embedding failure"));
        assert_eq!(source_signature(&conn, &source_id)?, before);
        assert_eq!(
            get_source_meta(&conn, source_id.as_str(), "source_index_sha256")?,
            before_hash
        );
        Ok(())
    }

    #[test]
    fn streaming_workspace_failure_rolls_back_the_complete_source() -> Result<()> {
        let mut conn = connection()?;
        let source_id = source("stream");
        let provider = FakeEmbeddings::new();
        let result = ingest_source_results(
            &mut conn,
            &source_id,
            &descriptor(&source_id),
            vec![
                Ok(document(&source_id, "one", "<p>complete</p>")),
                Err(anyhow!("corrupt normalized cache entry")),
            ],
            &provider,
        );
        assert!(result.is_err());
        assert_eq!(
            scalar(
                &conn,
                "SELECT COUNT(*) FROM documents WHERE source_id = ?1",
                &source_id,
            )?,
            0
        );
        assert_eq!(
            scalar(
                &conn,
                "SELECT COUNT(*) FROM chunks WHERE source_id = ?1",
                &source_id,
            )?,
            0
        );
        assert!(get_source_meta(&conn, source_id.as_str(), "source_index_sha256")?.is_none());
        Ok(())
    }

    #[test]
    fn ann_finalisation_binds_each_sidecar_to_its_source() -> Result<()> {
        let mut conn = connection()?;
        let alpha = source("ann-alpha");
        let beta = source("ann-beta");
        let provider = FakeEmbeddings::new();
        for source_id in [&alpha, &beta] {
            ingest_source(
                &mut conn,
                source_id,
                &descriptor(source_id),
                [document(source_id, "same", "<p>identical vector text</p>")],
                &provider,
            )?;
        }
        let output = tempdir()?;
        let alpha_ann = finalise_source_ann(&conn, &alpha, output.path())?;
        let beta_ann = finalise_source_ann(&conn, &beta, output.path())?;

        assert_eq!(alpha_ann.source_id, alpha);
        assert_eq!(beta_ann.source_id, beta);
        assert_ne!(alpha_ann.corpus_id, beta_ann.corpus_id);
        assert!(output.path().join("ann/ann-alpha.ann").is_file());
        assert!(output.path().join("ann/ann-beta.ann").is_file());
        Ok(())
    }

    #[test]
    #[ignore = "requires LEGAL_MCP_BENCH_WORKSPACE and LEGAL_MCP_TEST_MODEL_DIR"]
    fn benchmark_source_document_preparation() -> Result<()> {
        let workspace = std::env::var("LEGAL_MCP_BENCH_WORKSPACE")
            .or_else(|_| std::env::var("LEGAL_MCP_ATO_PAGES_DIR"))?;
        let model = std::env::var("LEGAL_MCP_TEST_MODEL_DIR")?;
        let requested = std::env::var("LEGAL_MCP_BENCH_SAMPLES")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()?
            .unwrap_or(10_000);
        let source_id = SourceId::new(
            std::env::var("LEGAL_MCP_BENCH_SOURCE").unwrap_or_else(|_| "ato".to_string()),
        )?;
        let provider = ServerState::with_model_paths(
            crate::semantic::SemanticModelPaths::from_model_dir(std::path::Path::new(&model))?,
        );
        let mut documents = crate::source_catalog::normalized_document_results(
            &source_id,
            std::path::Path::new(&workspace),
        )?
        .take(requested);
        let existing = BTreeMap::new();
        let started = std::time::Instant::now();
        let mut prepared_documents = 0usize;
        let mut chunks = 0usize;
        let mut token_counts = Vec::new();
        loop {
            let batch = documents
                .by_ref()
                .take(DOCUMENT_PREPARATION_BATCH_SIZE)
                .collect::<Vec<_>>();
            if batch.is_empty() {
                break;
            }
            let prepared = batch
                .into_par_iter()
                .map(|document| prepare_source_document(document, &source_id, &existing, &provider))
                .collect::<Result<Vec<_>>>()?;
            for (_, _, document) in prepared {
                if let Some(document) = document {
                    prepared_documents += 1;
                    chunks += document.chunks.len();
                    token_counts.extend(document.chunks.iter().map(|chunk| chunk.token_count));
                }
            }
        }
        let elapsed = started.elapsed().as_secs_f64();
        let mut padded_tokens = 0usize;
        for job in token_counts.chunks_mut(PRODUCTION_EMBEDDING_FLUSH_SIZE) {
            job.sort_unstable();
            padded_tokens += job
                .chunks(EMBEDDING_BATCH_SIZE)
                .map(|batch| batch.last().copied().unwrap_or(0) * batch.len())
                .sum::<usize>();
        }
        let active_tokens = token_counts.iter().sum::<usize>();
        eprintln!(
            "SOURCE_PREPARE_BENCH source={source_id} documents={prepared_documents} chunks={chunks} active_tokens={active_tokens} elapsed_s={elapsed:.3} documents_per_s={:.1} chunks_per_s={:.1} predicted_padding_efficiency={:.3}",
            prepared_documents as f64 / elapsed,
            chunks as f64 / elapsed,
            active_tokens as f64 / padded_tokens as f64,
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires LEGAL_MCP_BENCH_WORKSPACE, LEGAL_MCP_BENCH_SOURCE, and LEGAL_MCP_TEST_MODEL_DIR"]
    fn validate_specialized_tokenizer_on_source() -> Result<()> {
        let workspace = std::env::var("LEGAL_MCP_BENCH_WORKSPACE")?;
        let source_id = SourceId::new(std::env::var("LEGAL_MCP_BENCH_SOURCE")?)?;
        let model = std::env::var("LEGAL_MCP_TEST_MODEL_DIR")?;
        let requested = std::env::var("LEGAL_MCP_BENCH_SAMPLES")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()?
            .unwrap_or(1_000);
        let offset = std::env::var("LEGAL_MCP_BENCH_OFFSET")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()?
            .unwrap_or(0);
        let provider = ServerState::with_model_paths(
            crate::semantic::SemanticModelPaths::from_model_dir(std::path::Path::new(&model))?,
        );
        let mut reference =
            tokenizers::Tokenizer::from_file(std::path::Path::new(&model).join("tokenizer.json"))
                .map_err(|error| anyhow!("loading reference tokenizer: {error}"))?;
        reference
            .with_truncation(None)
            .map_err(|error| anyhow!("disabling reference truncation: {error}"))?;
        reference.with_padding(None);
        let existing = std::collections::BTreeMap::new();
        let mut chunks = 0usize;
        for document in crate::source_catalog::normalized_document_results(
            &source_id,
            std::path::Path::new(&workspace),
        )?
        .skip(offset)
        .take(requested)
        {
            let (_, _, prepared) =
                prepare_source_document(document, &source_id, &existing, &provider)?;
            let Some(prepared) = prepared else {
                continue;
            };
            for chunk in prepared.chunks {
                let expected = reference
                    .encode(chunk.text.as_str(), true)
                    .map_err(|error| anyhow!("reference tokenization failed: {error}"))?;
                let actual = chunk
                    .embedding_token_ids
                    .as_deref()
                    .ok_or_else(|| anyhow!("prepared chunk has no token IDs"))?;
                let expected = expected
                    .get_ids()
                    .iter()
                    .map(|id| i64::from(*id))
                    .collect::<Vec<_>>();
                if actual != expected {
                    bail!(
                        "specialized tokenizer mismatch for source {source_id}, native ID {}, chunk {}: actual {:?}, expected {:?}, text {:?}",
                        prepared.document.inventory.document.native_id,
                        chunk.ord,
                        actual,
                        expected,
                        chunk.text
                    );
                }
                chunks += 1;
            }
        }
        eprintln!(
            "TOKENIZER_EQUIVALENCE source={source_id} offset={offset} documents={requested} chunks={chunks}"
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires LEGAL_MCP_BENCH_WORKSPACE and LEGAL_MCP_TEST_MODEL_DIR"]
    fn benchmark_source_ingest_without_model_inference() -> Result<()> {
        struct TokenizerOnlyEmbeddings {
            tokenizer: crate::bert_tokenizer::BertWordPieceTokenizer,
        }

        impl EmbeddingProvider for TokenizerOnlyEmbeddings {
            fn model_id(&self) -> &str {
                EMBEDDING_MODEL_ID
            }

            fn count_tokens(&self, text: &str) -> Result<usize> {
                Ok(self.tokenizer.encode(text)?.len())
            }

            fn encode(&self, texts: &[String]) -> Result<Vec<[i8; EMBEDDING_DIM]>> {
                Ok(vec![[0_i8; EMBEDDING_DIM]; texts.len()])
            }

            fn prepare_document_tokens(&self, text: &str) -> Result<Option<Vec<i64>>> {
                Ok(Some(self.tokenizer.encode(text)?))
            }

            fn prepare_embedding_tokens_exact(&self, text: &str) -> Result<Option<Vec<i64>>> {
                self.prepare_document_tokens(text)
            }
        }

        let workspace = std::env::var("LEGAL_MCP_BENCH_WORKSPACE")
            .or_else(|_| std::env::var("LEGAL_MCP_ATO_PAGES_DIR"))?;
        let model = std::env::var("LEGAL_MCP_TEST_MODEL_DIR")?;
        let requested = std::env::var("LEGAL_MCP_BENCH_SAMPLES")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()?
            .unwrap_or(10_000);
        let source_id = SourceId::new(
            std::env::var("LEGAL_MCP_BENCH_SOURCE").unwrap_or_else(|_| "ato".to_string()),
        )?;
        let tokenizer = crate::bert_tokenizer::BertWordPieceTokenizer::from_file(
            &std::path::Path::new(&model).join("tokenizer.json"),
        )?;
        let provider = TokenizerOnlyEmbeddings { tokenizer };
        let root = if let Ok(parent) = std::env::var("LEGAL_MCP_BENCH_TEMP_ROOT") {
            tempfile::Builder::new()
                .prefix("ato-ingest-")
                .tempdir_in(parent)?
        } else {
            tempdir()?
        };
        let mut connection = crate::db::open_write_at(&root.path().join("legal.db"))?;
        init_db(&connection)?;
        let documents = crate::source_catalog::normalized_document_results(
            &source_id,
            std::path::Path::new(&workspace),
        )?
        .take(requested);
        let started = std::time::Instant::now();
        let report = ingest_source_results(
            &mut connection,
            &source_id,
            &descriptor(&source_id),
            documents,
            &provider,
        )?;
        let elapsed = started.elapsed().as_secs_f64();
        eprintln!(
            "SOURCE_INGEST_BENCH source={source_id} documents={} chunks={} elapsed_s={elapsed:.3} documents_per_s={:.1} chunks_per_s={:.1}",
            report.inserted_documents,
            report.inserted_chunks,
            report.inserted_documents as f64 / elapsed,
            report.inserted_chunks as f64 / elapsed,
        );
        if let Ok(output) = std::env::var("LEGAL_MCP_BENCH_OUTPUT_DB") {
            connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            std::fs::copy(root.path().join("legal.db"), output)?;
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires LEGAL_MCP_BENCH_WORKSPACE, LEGAL_MCP_BENCH_SOURCE, LEGAL_MCP_TEST_MODEL_DIR, and CUDA"]
    fn benchmark_source_ingest_with_model_inference() -> Result<()> {
        let workspace = std::env::var("LEGAL_MCP_BENCH_WORKSPACE")?;
        let source_id = SourceId::new(std::env::var("LEGAL_MCP_BENCH_SOURCE")?)?;
        let model = std::env::var("LEGAL_MCP_TEST_MODEL_DIR")?;
        let requested = std::env::var("LEGAL_MCP_BENCH_SAMPLES")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()?
            .unwrap_or(5_000);
        let provider = ServerState::with_model_paths(
            crate::semantic::SemanticModelPaths::from_model_dir(std::path::Path::new(&model))?,
        );
        let root = tempdir()?;
        let mut connection = crate::db::open_write_at(&root.path().join("legal.db"))?;
        init_db(&connection)?;
        let descriptor = crate::legal_source::source_registry()
            .source(&source_id)?
            .descriptor()
            .clone();
        let started = std::time::Instant::now();
        let documents = crate::source_catalog::normalized_document_results(
            &source_id,
            std::path::Path::new(&workspace),
        )?
        .take(requested);
        let report = ingest_source_results(
            &mut connection,
            &source_id,
            &descriptor,
            documents,
            &provider,
        )?;
        let elapsed = started.elapsed().as_secs_f64();
        eprintln!(
            "SOURCE_INGEST_MODEL_BENCH source={source_id} documents={} chunks={} embeddings={} elapsed_s={elapsed:.3} documents_per_s={:.1} chunks_per_s={:.1}",
            report.inserted_documents,
            report.inserted_chunks,
            report.encoded_texts,
            report.inserted_documents as f64 / elapsed,
            report.inserted_chunks as f64 / elapsed,
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires LEGAL_MCP_BENCH_WORKSPACE, LEGAL_MCP_TEST_MODEL_DIR, and CUDA"]
    fn benchmark_source_embedding_throughput() -> Result<()> {
        let workspace = std::env::var("LEGAL_MCP_BENCH_WORKSPACE")
            .or_else(|_| std::env::var("LEGAL_MCP_ATO_PAGES_DIR"))?;
        let model = std::env::var("LEGAL_MCP_TEST_MODEL_DIR")?;
        let requested = std::env::var("LEGAL_MCP_BENCH_SAMPLES")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()?
            .unwrap_or(10_000);
        let source_id = SourceId::new(
            std::env::var("LEGAL_MCP_BENCH_SOURCE").unwrap_or_else(|_| "ato".to_string()),
        )?;
        let provider = ServerState::with_model_paths(
            crate::semantic::SemanticModelPaths::from_model_dir(std::path::Path::new(&model))?,
        );
        let mut documents = crate::source_catalog::normalized_document_results(
            &source_id,
            std::path::Path::new(&workspace),
        )?
        .take(requested);
        let existing = BTreeMap::new();
        let mut texts = Vec::new();
        loop {
            let batch = documents
                .by_ref()
                .take(DOCUMENT_PREPARATION_BATCH_SIZE)
                .collect::<Vec<_>>();
            if batch.is_empty() {
                break;
            }
            let prepared = batch
                .into_par_iter()
                .map(|document| prepare_source_document(document, &source_id, &existing, &provider))
                .collect::<Result<Vec<_>>>()?;
            for (_, _, document) in prepared {
                if let Some(document) = document {
                    texts.extend(document.chunks.into_iter().map(|chunk| {
                        (
                            chunk.token_count,
                            chunk.text,
                            chunk
                                .embedding_token_ids
                                .expect("production provider prepares token ids"),
                        )
                    }));
                }
            }
        }
        for job in texts.chunks_mut(PRODUCTION_EMBEDDING_FLUSH_SIZE) {
            job.sort_by(|left, right| {
                left.0
                    .cmp(&right.0)
                    .then_with(|| left.1.as_bytes().cmp(right.1.as_bytes()))
            });
        }
        let sessions = std::env::var("LEGAL_MCP_BENCH_SESSIONS")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()?
            .unwrap_or(1);
        if !(1..=2).contains(&sessions) {
            bail!("LEGAL_MCP_BENCH_SESSIONS must be 1 or 2");
        }
        let providers = (0..sessions)
            .map(|_| {
                crate::semantic::SemanticModelPaths::from_model_dir(std::path::Path::new(&model))
                    .map(ServerState::with_model_paths)
            })
            .collect::<Result<Vec<_>>>()?;
        let started = std::time::Instant::now();
        let results = std::thread::scope(|scope| {
            providers
                .iter()
                .enumerate()
                .map(|(worker, provider)| {
                    let texts = &texts;
                    scope.spawn(move || -> Result<(usize, SemanticEncodeStats)> {
                        let mut encoded = 0usize;
                        let mut combined = SemanticEncodeStats::default();
                        for (job_index, job) in texts
                            .chunks(PRODUCTION_EMBEDDING_FLUSH_SIZE)
                            .enumerate()
                            .filter(|(job_index, _)| job_index % sessions == worker)
                        {
                            let _ = job_index;
                            for batch in job.chunks(EMBEDDING_BATCH_SIZE) {
                                let token_ids = batch
                                    .iter()
                                    .map(|(_, _, token_ids)| token_ids.as_slice())
                                    .collect::<Vec<_>>();
                                let (embeddings, stats) = provider
                                    .encode_document_token_embeddings_with_stats(&token_ids)?;
                                encoded += embeddings.len();
                                combined.tokenize += stats.tokenize;
                                combined.prepare += stats.prepare;
                                combined.run += stats.run;
                                combined.postprocess += stats.postprocess;
                                combined.batches += stats.batches;
                                combined.inputs += stats.inputs;
                                combined.active_tokens += stats.active_tokens;
                                combined.padded_tokens += stats.padded_tokens;
                                combined.max_batch = combined.max_batch.max(stats.max_batch);
                                combined.max_seq_len = combined.max_seq_len.max(stats.max_seq_len);
                            }
                        }
                        Ok((encoded, combined))
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| anyhow!("embedding benchmark worker panicked"))?
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let elapsed = started.elapsed().as_secs_f64();
        let encoded = results.iter().map(|(encoded, _)| *encoded).sum::<usize>();
        let active_tokens = results
            .iter()
            .map(|(_, stats)| stats.active_tokens)
            .sum::<usize>();
        let padded_tokens = results
            .iter()
            .map(|(_, stats)| stats.padded_tokens)
            .sum::<usize>();
        let tokenize = results
            .iter()
            .map(|(_, stats)| stats.tokenize)
            .sum::<std::time::Duration>();
        let prepare = results
            .iter()
            .map(|(_, stats)| stats.prepare)
            .sum::<std::time::Duration>();
        let run = results
            .iter()
            .map(|(_, stats)| stats.run)
            .sum::<std::time::Duration>();
        let postprocess = results
            .iter()
            .map(|(_, stats)| stats.postprocess)
            .sum::<std::time::Duration>();
        eprintln!(
            "SOURCE_EMBED_BENCH source={source_id} sessions={sessions} texts={encoded} active_tokens={active_tokens} elapsed_s={elapsed:.3} tokenize_s={:.3} prepare_s={:.3} run_s={:.3} postprocess_s={:.3} texts_per_s={:.1} active_tokens_per_s={:.0} padding_efficiency={:.3}",
            tokenize.as_secs_f64(),
            prepare.as_secs_f64(),
            run.as_secs_f64(),
            postprocess.as_secs_f64(),
            encoded as f64 / elapsed,
            active_tokens as f64 / elapsed,
            active_tokens as f64 / padded_tokens as f64,
        );
        Ok(())
    }
}
