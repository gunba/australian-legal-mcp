//! Source-agnostic ingestion of normalized legal documents into the final corpus schema.
//!
//! Acquisition and source-specific cleaning stop at [`NormalizedDocument`]. This module owns the
//! final, source-qualified SQLite reconciliation, shared chunking, embedding reuse, citation
//! derivation, metadata refresh, and per-source ANN finalisation.

use crate::ann::{self, ManifestAnn};
use crate::chunker::{chunk_html_with_token_count, Chunk, EMBED_MAX_TOKENS};
use crate::db::{
    compress_text, decompress_text, get_corpus_meta, get_source_meta, set_corpus_meta,
    set_source_meta,
};
use crate::{ServerState, EMBEDDING_DIM, EMBEDDING_MODEL_ID};
use anyhow::{anyhow, bail, Context, Result};
use legal_model::{DocumentId, SourceDescriptor, SourceId};
use legal_source_sdk::{sha256_bytes, NormalizedDocument};
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

const EMBEDDING_BATCH_SIZE: usize = 32;

/// The only embedding capability the final-schema pipeline needs.
///
/// Implementations encode unprefixed chunk text. The production semantic runtime applies its
/// configured document prefix itself, exactly as it does for the existing corpus builder.
pub(crate) trait EmbeddingProvider {
    fn model_id(&self) -> &str;

    fn count_tokens(&self, text: &str) -> Result<usize>;

    fn encode(&self, texts: &[String]) -> Result<Vec<[i8; EMBEDDING_DIM]>>;
}

impl EmbeddingProvider for ServerState {
    fn model_id(&self) -> &str {
        EMBEDDING_MODEL_ID
    }

    fn count_tokens(&self, text: &str) -> Result<usize> {
        self.count_embedding_tokens(text)
    }

    fn encode(&self, texts: &[String]) -> Result<Vec<[i8; EMBEDDING_DIM]>> {
        self.encode_query_embeddings(texts)
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

struct ValidatedDocument {
    document: NormalizedDocument,
    content_hash: String,
}

struct PreparedDocument {
    document: NormalizedDocument,
    content_hash: String,
    headings: String,
    chunks: Vec<Chunk>,
}

struct PendingEmbedding {
    chunk_id: i64,
    text_sha256: String,
}

/// Reconcile one complete, authoritative source snapshot into an existing final-schema database.
///
/// `documents` is the complete current source inventory, not merely a delta. Documents absent from
/// it are deleted. The supplied source and descriptor must identify the same single source, and
/// every normalized document must carry that source. All SQLite changes occur in one immediate
/// transaction; source-specific normalization and ANN publication deliberately remain outside this
/// function.
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
    validate_source_input(source_id, descriptor, embeddings.model_id())?;
    let documents = collect_source_documents(source_id, documents)?;
    let source_index_sha256 = source_snapshot_sha256(source_id, &documents);

    // Cascades are part of the final schema contract. Enabling this before beginning the
    // transaction also avoids SQLite's no-op behaviour for PRAGMA foreign_keys inside a txn.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let enabled: i64 = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if enabled != 1 {
        bail!("final-schema ingestion requires SQLite foreign-key enforcement");
    }

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let report = ingest_source_transaction(
        &tx,
        source_id,
        descriptor,
        documents,
        embeddings,
        source_index_sha256,
    )?;
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

fn collect_source_documents<I>(
    source_id: &SourceId,
    documents: I,
) -> Result<BTreeMap<String, ValidatedDocument>>
where
    I: IntoIterator<Item = NormalizedDocument>,
{
    let mut collected = BTreeMap::new();
    for document in documents {
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
        let validated = ValidatedDocument {
            document,
            content_hash,
        };
        if collected.insert(native_id.clone(), validated).is_some() {
            bail!("duplicate normalized document `{source_id}/{native_id}`");
        }
    }
    Ok(collected)
}

fn source_snapshot_sha256(
    source_id: &SourceId,
    documents: &BTreeMap<String, ValidatedDocument>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"australian-legal-mcp-source-snapshot-v1\0");
    hash_field(&mut hasher, source_id.as_str().as_bytes());
    for (native_id, document) in documents {
        hash_field(&mut hasher, native_id.as_bytes());
        hash_field(&mut hasher, document.content_hash.as_bytes());
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
    documents: BTreeMap<String, ValidatedDocument>,
    embeddings: &P,
    source_index_sha256: String,
) -> Result<SourceIngestReport>
where
    P: EmbeddingProvider + ?Sized,
{
    validate_embedding_model(tx, embeddings.model_id())?;

    let existing = existing_source_hashes(tx, source_id)?;
    let incoming_ids = documents.keys().cloned().collect::<BTreeSet<_>>();
    let absent_ids = existing
        .keys()
        .filter(|native_id| !incoming_ids.contains(*native_id))
        .cloned()
        .collect::<Vec<_>>();

    let mut unchanged_documents = 0usize;
    let mut inserted_documents = 0usize;
    let mut changed_documents = 0usize;
    let mut prepared = Vec::new();
    for (_, validated) in documents {
        let ValidatedDocument {
            document,
            content_hash,
        } = validated;
        let inventory = &document.inventory;
        let native_id = &inventory.document.native_id;
        match existing.get(native_id) {
            Some(stored_hash) if stored_hash == &content_hash => {
                unchanged_documents += 1;
            }
            stored_hash => {
                if stored_hash.is_some() {
                    changed_documents += 1;
                } else {
                    inserted_documents += 1;
                }
                let headings = headings_text(&document.html)?;
                let chunks = chunk_html_with_token_count(
                    &document.html,
                    Some(inventory.title.as_str()),
                    EMBED_MAX_TOKENS,
                    |text| embeddings.count_tokens(text),
                )?;
                prepared.push(PreparedDocument {
                    document,
                    content_hash,
                    headings,
                    chunks,
                });
            }
        }
    }

    tx.execute(
        "INSERT INTO sources(source_id, display_name) VALUES (?1, ?2)
         ON CONFLICT(source_id) DO UPDATE SET display_name = excluded.display_name",
        params![source_id.as_str(), descriptor.display_name],
    )?;

    for native_id in absent_ids.iter().chain(
        prepared
            .iter()
            .filter(|document| {
                existing.contains_key(&document.document.inventory.document.native_id)
            })
            .map(|document| &document.document.inventory.document.native_id),
    ) {
        delete_source_document(tx, source_id, native_id)?;
    }

    let downloaded_at = chrono::Utc::now().to_rfc3339();
    let mut pending_embeddings = Vec::new();
    let mut text_by_sha256 = BTreeMap::new();
    let mut inserted_chunks = 0usize;
    let mut seen_asset_ids = source_asset_ids(tx, source_id)?;

    for prepared_document in &prepared {
        insert_document(
            tx,
            source_id,
            prepared_document,
            &downloaded_at,
            &mut seen_asset_ids,
            &mut pending_embeddings,
            &mut text_by_sha256,
        )?;
        inserted_chunks += prepared_document.chunks.len();
    }

    let (encoded_texts, reused_embeddings) =
        install_embeddings(tx, embeddings, &pending_embeddings, &text_by_sha256)?;
    derive_source_citations(tx, source_id)?;
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

fn source_asset_ids(conn: &Connection, source_id: &SourceId) -> Result<HashSet<String>> {
    let mut statement = conn
        .prepare("SELECT asset_id FROM document_assets WHERE source_id = ?1 ORDER BY asset_id")?;
    let rows = statement.query_map([source_id.as_str()], |row| row.get::<_, String>(0))?;
    let mut ids = HashSet::new();
    for row in rows {
        ids.insert(row?);
    }
    Ok(ids)
}

fn delete_source_document(conn: &Connection, source_id: &SourceId, native_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM chunks_fts WHERE rowid IN (
             SELECT chunk_id FROM chunks WHERE source_id = ?1 AND native_id = ?2
         )",
        params![source_id.as_str(), native_id],
    )?;
    conn.execute(
        "DELETE FROM title_fts WHERE source_id = ?1 AND native_id = ?2",
        params![source_id.as_str(), native_id],
    )?;
    conn.execute(
        "DELETE FROM documents WHERE source_id = ?1 AND native_id = ?2",
        params![source_id.as_str(), native_id],
    )?;
    Ok(())
}

fn headings_text(html: &str) -> Result<String> {
    let fragment = scraper::Html::parse_fragment(html);
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
    seen_asset_ids: &mut HashSet<String>,
    pending_embeddings: &mut Vec<PendingEmbedding>,
    text_by_sha256: &mut BTreeMap<String, String>,
) -> Result<()> {
    let document = &prepared.document;
    let inventory = &document.inventory;
    let native_id = &inventory.document.native_id;
    let has_in_doc_links = prepared.chunks.iter().any(|chunk| {
        chunk
            .anchor
            .as_deref()
            .is_some_and(|anchor| !anchor.is_empty())
    });
    conn.execute(
        "INSERT INTO documents
            (source_id, native_id, type, title, date, canonical_url, upstream_version,
             downloaded_at, content_hash, html, withdrawn_date, superseded_by, replaces,
             has_in_doc_links, has_related_docs, has_history, headings)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                 NULL, NULL, NULL, ?11, 0, 0, ?12)",
        params![
            source_id.as_str(),
            native_id,
            inventory.document_type,
            inventory.title,
            inventory.date,
            inventory.canonical_url,
            inventory.upstream_version,
            downloaded_at,
            prepared.content_hash,
            compress_text(&document.html)?,
            i64::from(has_in_doc_links),
            prepared.headings,
        ],
    )
    .with_context(|| format!("inserting document `{source_id}/{native_id}`"))?;

    let mut anchor_ord = 0_i64;
    for chunk in &prepared.chunks {
        let chunk_id: i64 = conn
            .query_row(
                "INSERT INTO chunks(source_id, native_id, ord, anchor, text)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 RETURNING chunk_id",
                params![
                    source_id.as_str(),
                    native_id,
                    chunk.ord,
                    chunk.anchor,
                    compress_text(&chunk.text)?,
                ],
                |row| row.get(0),
            )
            .with_context(|| {
                format!(
                    "inserting chunk {} for `{source_id}/{native_id}`",
                    chunk.ord
                )
            })?;
        conn.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)",
            params![chunk_id, chunk.text],
        )?;
        if let Some(anchor) = chunk.anchor.as_deref().filter(|anchor| !anchor.is_empty()) {
            conn.execute(
                "INSERT INTO doc_anchors(
                    source_id, native_id, ord, kind, label,
                    target_source_id, target_native_id, target_chunk_id
                 ) VALUES (?1, ?2, ?3, 'in_doc', ?4, ?1, ?2, ?5)",
                params![source_id.as_str(), native_id, anchor_ord, anchor, chunk_id],
            )?;
            anchor_ord += 1;
        }

        let text_sha256 = sha256_bytes(chunk.text.as_bytes());
        match text_by_sha256.entry(text_sha256.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(chunk.text.clone());
            }
            std::collections::btree_map::Entry::Occupied(entry) if entry.get() != &chunk.text => {
                bail!("distinct chunk texts produced the same SHA-256 digest");
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
        pending_embeddings.push(PendingEmbedding {
            chunk_id,
            text_sha256,
        });
    }

    conn.execute(
        "INSERT INTO title_fts(source_id, native_id, title, headings)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            source_id.as_str(),
            native_id,
            inventory.title,
            prepared.headings
        ],
    )?;

    for asset in &document.assets {
        let asset_id = &asset.asset.asset_id;
        if !seen_asset_ids.insert(asset_id.clone()) {
            bail!("duplicate asset id `{asset_id}` while ingesting `{source_id}/{native_id}`");
        }
        conn.execute(
            "INSERT INTO document_assets
                (source_id, asset_id, native_id, media_type, alt, title, sha256, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                source_id.as_str(),
                asset_id,
                native_id,
                asset.media_type,
                asset.alt,
                asset.title,
                asset.sha256,
                asset.data,
            ],
        )
        .with_context(|| format!("inserting asset `{asset_id}` for `{source_id}/{native_id}`"))?;
    }
    Ok(())
}

fn install_embeddings<P>(
    conn: &Connection,
    provider: &P,
    pending: &[PendingEmbedding],
    text_by_sha256: &BTreeMap<String, String>,
) -> Result<(usize, usize)>
where
    P: EmbeddingProvider + ?Sized,
{
    if pending.is_empty() {
        return Ok((0, 0));
    }

    let mut vectors = HashMap::<String, Vec<u8>>::new();
    let mut missing_hashes = Vec::new();
    let mut missing_texts = Vec::new();
    let mut select = conn.prepare(
        "SELECT embedding FROM embedding_cache WHERE model_id = ?1 AND text_sha256 = ?2",
    )?;
    for (text_sha256, text) in text_by_sha256 {
        let cached = select
            .query_row(params![provider.model_id(), text_sha256], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .optional()?;
        if let Some(cached) = cached {
            validate_embedding_bytes(&cached, provider.model_id(), text_sha256)?;
            vectors.insert(text_sha256.clone(), cached);
        } else {
            missing_hashes.push(text_sha256.clone());
            missing_texts.push(text.clone());
        }
    }
    drop(select);

    for (hash_batch, text_batch) in missing_hashes
        .chunks(EMBEDDING_BATCH_SIZE)
        .zip(missing_texts.chunks(EMBEDDING_BATCH_SIZE))
    {
        let encoded = provider
            .encode(text_batch)
            .with_context(|| format!("encoding {} unique chunk texts", text_batch.len()))?;
        if encoded.len() != text_batch.len() {
            bail!(
                "embedding provider returned {} vectors for {} texts",
                encoded.len(),
                text_batch.len()
            );
        }
        for (text_sha256, embedding) in hash_batch.iter().zip(encoded) {
            let bytes = embedding
                .iter()
                .map(|value| *value as u8)
                .collect::<Vec<_>>();
            conn.execute(
                "INSERT INTO embedding_cache(model_id, text_sha256, embedding)
                 VALUES (?1, ?2, ?3)",
                params![provider.model_id(), text_sha256, bytes],
            )?;
            vectors.insert(text_sha256.clone(), bytes);
        }
    }

    for item in pending {
        let embedding = vectors.get(&item.text_sha256).ok_or_else(|| {
            anyhow!(
                "embedding resolution omitted chunk text {}",
                item.text_sha256
            )
        })?;
        conn.execute(
            "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (?1, ?2)",
            params![item.chunk_id, embedding],
        )?;
    }

    let encoded_texts = missing_hashes.len();
    Ok((encoded_texts, pending.len().saturating_sub(encoded_texts)))
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

fn citation_marker_regex() -> Result<&'static Regex> {
    static REGEX: OnceLock<Result<Regex, String>> = OnceLock::new();
    match REGEX.get_or_init(|| {
        Regex::new(r"\[doc:([^\s\]@]+)(?:[^\]]*)\]").map_err(|error| error.to_string())
    }) {
        Ok(regex) => Ok(regex),
        Err(error) => Err(anyhow!("compiling document citation marker regex: {error}")),
    }
}

fn derive_source_citations(conn: &Connection, source_id: &SourceId) -> Result<()> {
    conn.execute(
        "DELETE FROM citations WHERE source_id = ?1",
        [source_id.as_str()],
    )?;
    let mut select = conn.prepare(
        "SELECT chunk_id, native_id, text FROM chunks
         WHERE source_id = ?1 ORDER BY chunk_id",
    )?;
    let rows = select.query_map([source_id.as_str()], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    let mut source_chunks = Vec::new();
    for row in rows {
        source_chunks.push(row?);
    }
    drop(select);

    let mut insert = conn.prepare(
        "INSERT OR IGNORE INTO citations(
             source_chunk_id, source_id, source_native_id,
             target_source_id, target_native_id
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (chunk_id, native_id, compressed) in source_chunks {
        let source_document = DocumentId::new(source_id.clone(), native_id.clone())?;
        let text = decompress_text(compressed)?;
        let mut seen = HashSet::new();
        for captures in citation_marker_regex()?.captures_iter(&text) {
            let target: DocumentId = captures[1].parse().with_context(|| {
                format!(
                    "invalid source-qualified document marker `[doc:{}]` in chunk {chunk_id}",
                    &captures[1]
                )
            })?;
            if target == source_document || !seen.insert(target.clone()) {
                continue;
            }
            insert.execute(params![
                chunk_id,
                source_id.as_str(),
                native_id,
                target.source.as_str(),
                target.native_id,
            ])?;
        }
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
    set_source_meta(
        conn,
        source_id.as_str(),
        "prefix_breakdown_json",
        &serde_json::to_string(&source_prefix_breakdown(conn, source_id)?)?,
    )?;

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

fn source_prefix_breakdown(
    conn: &Connection,
    source_id: &SourceId,
) -> Result<Vec<serde_json::Value>> {
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
    use anyhow::anyhow;
    use legal_source_sdk::SourceInventoryRecord;
    use std::cell::{Cell, RefCell};
    use tempfile::tempdir;

    struct FakeEmbeddings {
        fail: Cell<bool>,
        batches: Cell<usize>,
        texts: RefCell<Vec<String>>,
    }

    impl FakeEmbeddings {
        fn new() -> Self {
            Self {
                fail: Cell::new(false),
                batches: Cell::new(0),
                texts: RefCell::new(Vec::new()),
            }
        }

        fn fail(&self) {
            self.fail.set(true);
        }

        fn encoded_texts(&self) -> usize {
            self.texts.borrow().len()
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
            self.batches.set(self.batches.get() + 1);
            self.texts.borrow_mut().extend(texts.iter().cloned());
            if self.fail.get() {
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
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM title_fts
                 WHERE source_id = ?1 AND native_id = 'remove'",
                [source_id.as_str()],
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
}
