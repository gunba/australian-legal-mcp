//! Pack file format: many JSON records compressed with zstd and concatenated,
//! followed by a JSON reverse-index and a fixed trailer. Used for both the
//! writer (build pipeline) and the reader (install / live-DB rebuild).
//!
//! [IB-09] A pack is a single .bin.zst blob: many records back-to-back, each
//!   length:uint32 (LE) | zstd(orjson(record))
//! Trailer: index_blob (zstd(json([{doc_id, offset, length}, ...]))) followed by
//!   MAGIC(6) | count:u32 | index_offset:u64 | index_len:u32

use crate::db::compress_text;
use crate::source::DocRef;
use crate::EMBEDDING_DIM;
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::path::Path;

// ----- Pack file writer (port of src/ato_mcp/indexer/pack.py) -----
//
// [IB-09] A pack is a single .bin.zst blob: many records back-to-back, each
//   length:uint32 (LE) | zstd(orjson(record))
// Trailer: index_blob (zstd(json([{doc_id, offset, length}, ...]))) followed by
//   MAGIC(6) | count:u32 | index_offset:u64 | index_len:u32
// Mirrors pack.py:PackWriter.

pub(crate) const PACK_TRAILER_MAGIC: &[u8; 6] = b"ATOPK\x01";
pub(crate) const PACK_RECORD_HEADER_LEN: usize = 4;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PackedDocRef {
    pub(crate) doc_id: String,
    pub(crate) offset: u64,
    pub(crate) length: u64,
}

/// Write a pack file from a stream of (doc_id, record_json) pairs read from
/// stdin as JSONL. Each line: {"doc_id": str, "record": {...}}. Outputs JSON
/// {pack_path, sha8, sha256, size, refs: [PackedDocRef, ...]}.
pub(crate) fn write_pack(
    out_path: &Path,
    level: i32,
    records: impl Iterator<Item = Result<(String, serde_json::Value)>>,
) -> Result<JsonValue> {
    use std::io::Write as _;
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file =
        File::create(out_path).with_context(|| format!("creating {}", out_path.display()))?;
    let mut hasher = Sha256::new();
    let mut offset: u64 = 0;
    let mut refs: Vec<PackedDocRef> = Vec::new();

    for r in records {
        let (doc_id, record) = r?;
        let payload =
            zstd::stream::encode_all(std::io::Cursor::new(serde_json::to_vec(&record)?), level)?;
        let header = (payload.len() as u32).to_le_bytes();
        file.write_all(&header)?;
        file.write_all(&payload)?;
        hasher.update(header);
        hasher.update(&payload);
        let length = (PACK_RECORD_HEADER_LEN + payload.len()) as u64;
        let start = offset;
        offset += length;
        refs.push(PackedDocRef {
            doc_id,
            offset: start,
            length,
        });
    }

    // Trailer.
    let index_offset = offset;
    let index_json = serde_json::to_vec(&refs)?;
    let index_blob = zstd::stream::encode_all(std::io::Cursor::new(index_json), level)?;
    file.write_all(&index_blob)?;
    let mut trailer = Vec::with_capacity(6 + 4 + 8 + 4);
    trailer.extend_from_slice(PACK_TRAILER_MAGIC);
    trailer.extend_from_slice(&(refs.len() as u32).to_le_bytes());
    trailer.extend_from_slice(&index_offset.to_le_bytes());
    trailer.extend_from_slice(&(index_blob.len() as u32).to_le_bytes());
    file.write_all(&trailer)?;
    hasher.update(&index_blob);
    hasher.update(&trailer);
    file.flush()?;

    let digest = hasher.finalize();
    let sha256_hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let sha8 = sha256_hex[..8].to_string();
    let size = fs::metadata(out_path)?.len();
    Ok(json!({
        "pack_path": out_path.display().to_string(),
        "sha8": sha8,
        "sha256": sha256_hex,
        "size": size,
        "refs": refs,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackRecord {
    pub(crate) doc_id: String,
    #[serde(default, rename = "type")]
    pub(crate) doc_type: String,
    pub(crate) title: String,
    pub(crate) date: Option<String>,
    pub(crate) downloaded_at: String,
    pub(crate) content_hash: String,
    pub(crate) html: String,
    /// W2.2 currency markers. The insert_record regression test proves these
    /// pack fields survive ingestion into the searchable SQLite corpus.
    #[serde(default)]
    pub(crate) withdrawn_date: Option<String>,
    #[serde(default)]
    pub(crate) superseded_by: Option<String>,
    #[serde(default)]
    pub(crate) replaces: Option<String>,
    /// Navigation hint flags. Set at build time by the maintainer pipeline
    /// from the doc_anchors table; ingestion writes them straight through.
    #[serde(default)]
    pub(crate) has_in_doc_links: i64,
    #[serde(default)]
    pub(crate) has_related_docs: i64,
    #[serde(default)]
    pub(crate) has_history: i64,
    /// Per-doc navigation anchors emitted by the build pipeline; ingested
    /// straight into the doc_anchors table.
    #[serde(default)]
    pub(crate) anchors: Vec<PackDocAnchor>,
    #[serde(default)]
    pub(crate) definitions: Vec<PackDefinition>,
    pub(crate) assets: Vec<PackAsset>,
    #[serde(default)]
    pub(crate) chunks: Vec<PackChunk>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackAsset {
    pub(crate) asset_ref: String,
    pub(crate) source_path: String,
    pub(crate) relative_path: String,
    pub(crate) media_type: Option<String>,
    pub(crate) alt: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) sha256: String,
    pub(crate) size: i64,
    pub(crate) data_b64: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackDefinition {
    pub(crate) definition_id: String,
    pub(crate) term: String,
    pub(crate) norm_term: String,
    pub(crate) doc_id: String,
    pub(crate) source_title: String,
    pub(crate) source_type: String,
    #[serde(default)]
    pub(crate) scope: Option<String>,
    #[serde(default)]
    pub(crate) anchor: Option<String>,
    pub(crate) ord: i64,
    pub(crate) body: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackChunk {
    pub(crate) ord: i64,
    #[serde(default)]
    pub(crate) anchor: Option<String>,
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) embedding_b64: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackDocAnchor {
    pub(crate) ord: i64,
    pub(crate) kind: String,
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) target_chunk_id: Option<i64>,
    #[serde(default)]
    pub(crate) target_doc_id: Option<String>,
    #[serde(default)]
    pub(crate) target_pit: Option<String>,
}

pub(crate) fn read_record_from_pack_bytes(pack: &[u8], offset: u64, length: u64) -> Result<PackRecord> {
    let start = offset as usize;
    let end = start + length as usize;
    if end > pack.len() || length < 4 {
        bail!(
            "pack range out of bounds: offset={offset}, length={length}, pack_len={}",
            pack.len()
        );
    }
    let blob = &pack[start..end];
    let payload_len = u32::from_le_bytes(blob[0..4].try_into().unwrap()) as usize;
    if payload_len + 4 != blob.len() {
        bail!(
            "pack record length mismatch: header says {}, range says {}",
            payload_len + 4,
            blob.len()
        );
    }
    let decoded = zstd::stream::decode_all(Cursor::new(&blob[4..]))?;
    Ok(serde_json::from_slice(&decoded)?)
}

pub(crate) fn insert_record(
    conn: &Connection,
    record: &PackRecord,
    doc_ref: &DocRef,
    asset_root: &Path,
) -> Result<()> {
    let doc_type = if record.doc_type.is_empty() {
        "Unknown"
    } else {
        &record.doc_type
    };
    conn.execute(
        r#"
        INSERT OR REPLACE INTO documents
            (doc_id, type, title, date, downloaded_at, content_hash, pack_sha8,
             html, withdrawn_date, superseded_by, replaces,
             has_in_doc_links, has_related_docs, has_history)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        params![
            record.doc_id,
            doc_type,
            record.title,
            record.date,
            record.downloaded_at,
            record.content_hash,
            doc_ref.pack_sha8,
            compress_text(&record.html)?,
            record.withdrawn_date,
            record.superseded_by,
            record.replaces,
            record.has_in_doc_links,
            record.has_related_docs,
            record.has_history,
        ],
    )?;
    write_record_assets(conn, record, asset_root)?;
    // Heading text now lives inside chunk.text (rendered inline by the
    // chunker). title_fts headings column carries an empty string — the
    // title alone is the BM25 signal.
    conn.execute(
        "INSERT INTO title_fts (doc_id, title, headings) VALUES (?, ?, ?)",
        params![record.doc_id, record.title, ""],
    )?;
    for chunk in &record.chunks {
        let blob = compress_text(&chunk.text)?;
        let rowid: i64 = conn.query_row(
            "INSERT INTO chunks (doc_id, ord, anchor, text)
             VALUES (?, ?, ?, ?)
             RETURNING chunk_id",
            params![record.doc_id, chunk.ord, chunk.anchor, blob],
            |row| row.get(0),
        )?;
        if let Some(embedding_b64) = &chunk.embedding_b64 {
            let embedding = decode_embedding_b64(embedding_b64)?;
            conn.execute(
                "INSERT INTO chunk_embeddings (chunk_id, embedding) VALUES (?, ?)",
                params![rowid, embedding],
            )?;
        }
        conn.execute(
            "INSERT INTO chunks_fts (rowid, text) VALUES (?, ?)",
            params![rowid, chunk.text],
        )
        .with_context(|| {
            format!(
                "INSERT chunks_fts doc_id={} chunk_id={} ord={}",
                record.doc_id, rowid, chunk.ord
            )
        })?;
    }
    for anchor in &record.anchors {
        conn.execute(
            r#"
            INSERT INTO doc_anchors
                (doc_id, ord, kind, label, target_chunk_id, target_doc_id, target_pit)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                record.doc_id,
                anchor.ord,
                anchor.kind,
                anchor.label,
                anchor.target_chunk_id,
                anchor.target_doc_id,
                anchor.target_pit,
            ],
        )?;
    }
    for definition in &record.definitions {
        conn.execute(
            r#"
            INSERT OR REPLACE INTO definitions
                (definition_id, term, norm_term, doc_id, source_title, source_type,
                 scope, anchor, ord, body)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                definition.definition_id,
                definition.term,
                definition.norm_term,
                definition.doc_id,
                definition.source_title,
                definition.source_type,
                definition.scope,
                definition.anchor,
                definition.ord,
                definition.body,
            ],
        )?;
    }
    Ok(())
}

pub(crate) fn write_record_assets(conn: &Connection, record: &PackRecord, asset_root: &Path) -> Result<()> {
    for asset in &record.assets {
        let data = base64::engine::general_purpose::STANDARD
            .decode(&asset.data_b64)
            .with_context(|| format!("decoding asset {}", asset.asset_ref))?;
        if data.len() as i64 != asset.size {
            bail!(
                "asset {} size mismatch: got {}, expected {}",
                asset.asset_ref,
                data.len(),
                asset.size
            );
        }
        let actual_sha = format!("{:x}", Sha256::digest(&data));
        if actual_sha != asset.sha256 {
            bail!("asset {} sha256 mismatch", asset.asset_ref);
        }
        let target = asset_root.join(&asset.relative_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let needs_write = if target.exists() {
            let existing = fs::read(&target)?;
            format!("{:x}", Sha256::digest(&existing)) != asset.sha256
        } else {
            true
        };
        if needs_write {
            fs::write(&target, &data)?;
        }
        conn.execute(
            r#"
            INSERT OR REPLACE INTO document_assets
                (asset_ref, doc_id, source_path, relative_path, media_type, alt, title, sha256, bytes)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                asset.asset_ref,
                record.doc_id,
                asset.source_path,
                asset.relative_path,
                asset.media_type,
                asset.alt,
                asset.title,
                asset.sha256,
                asset.size,
            ],
        )?;
    }
    Ok(())
}

pub(crate) fn decode_embedding_b64(value: &str) -> Result<Vec<u8>> {
    let embedding = base64::engine::general_purpose::STANDARD
        .decode(value)
        .context("decoding chunk embedding")?;
    if embedding.len() != EMBEDDING_DIM {
        bail!(
            "invalid chunk embedding length: got {}, expected {}",
            embedding.len(),
            EMBEDDING_DIM
        );
    }
    Ok(embedding)
}
