//! Deterministic, source-scoped, mmap-backed exact int8 vector sidecars.
//!
//! A sidecar is a search accelerator, never an authority. It contains one
//! source's sorted SQLite chunk IDs and byte-identical int8 embeddings. Search
//! scans the eligible rows exactly, then rereads the selected embeddings from
//! SQLite for authoritative reranking.

use crate::{EMBEDDING_DIM, EMBEDDING_MODEL_FINGERPRINT, EMBEDDING_MODEL_ID};
use anyhow::{anyhow, bail, Context, Result};
use legal_model::SourceId;
use memmap2::{Mmap, MmapOptions};
use rayon::prelude::*;
use roaring::RoaringBitmap;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub(crate) const ANN_DIRECTORY: &str = "ann";
pub(crate) const ANN_FORMAT: &str = "flat-int8";
pub(crate) const ANN_FORMAT_VERSION: u32 = 1;
pub(crate) const ANN_ID_ENCODING: &str = "sqlite-chunk-id-u32";
pub(crate) const ANN_METRIC: &str = "signed-int8-dot-exact";

const HEADER_LEN: usize = 4 * 1024;
const HEADER_LEN_U64: u64 = HEADER_LEN as u64;
const PLANE_ALIGNMENT: u64 = 4 * 1024;
const ID_BYTES: u32 = 4;
const VECTOR_BYTES: u32 = EMBEDDING_DIM as u32;
const MAGIC: &[u8; 16] = b"AUSLEGAL-FLAT-I8";
const ENDIAN_MARKER: u32 = 0x0102_0304;
const SCAN_THREADS: usize = 4;
const SCAN_BLOCK_ROWS: usize = 16 * 1024;

const SOURCE_FIELD: std::ops::Range<usize> = 128..192;
const MODEL_FIELD: std::ops::Range<usize> = 192..320;
const METRIC_FIELD: std::ops::Range<usize> = 320..384;
const ID_ENCODING_FIELD: std::ops::Range<usize> = 384..448;
const CORPUS_FIELD: std::ops::Range<usize> = 448..576;
const MODEL_FINGERPRINT_FIELD: std::ops::Range<usize> = 576..608;
const EMBEDDING_SHA_FIELD: std::ops::Range<usize> = 608..640;
const HEADER_RESERVED_START: usize = 640;

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
    pub(crate) path: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
    pub(crate) corpus_id: String,
    pub(crate) embedding_model_id: String,
    pub(crate) embedding_model_fingerprint: String,
    pub(crate) embedding_dimension: u32,
    pub(crate) embedding_set_sha256: String,
    pub(crate) vector_count: u64,
    pub(crate) first_chunk_id: u32,
    pub(crate) last_chunk_id: u32,
    pub(crate) id_encoding: String,
    pub(crate) metric: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnnIdentity {
    pub(crate) source_id: SourceId,
    pub(crate) corpus_id: String,
    pub(crate) embedding_set_sha256: String,
    pub(crate) vector_count: u64,
    pub(crate) first_chunk_id: u32,
    pub(crate) last_chunk_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SidecarLayout {
    ids_offset: u64,
    ids_len: u64,
    vectors_offset: u64,
    vectors_len: u64,
    file_size: u64,
}

fn checked_align_up(value: u64, alignment: u64) -> Result<u64> {
    let remainder = value % alignment;
    if remainder == 0 {
        return Ok(value);
    }
    value
        .checked_add(alignment - remainder)
        .ok_or_else(|| anyhow!("ANN sidecar alignment overflow"))
}

fn sidecar_layout(vector_count: u64) -> Result<SidecarLayout> {
    if vector_count == 0 {
        bail!("ANN sidecar vector count cannot be zero");
    }
    let ids_len = vector_count
        .checked_mul(u64::from(ID_BYTES))
        .ok_or_else(|| anyhow!("ANN sidecar ID plane length overflow"))?;
    let ids_end = HEADER_LEN_U64
        .checked_add(ids_len)
        .ok_or_else(|| anyhow!("ANN sidecar ID plane end overflow"))?;
    let vectors_offset = checked_align_up(ids_end, PLANE_ALIGNMENT)?;
    let vectors_len = vector_count
        .checked_mul(u64::from(VECTOR_BYTES))
        .ok_or_else(|| anyhow!("ANN sidecar vector plane length overflow"))?;
    let file_size = vectors_offset
        .checked_add(vectors_len)
        .ok_or_else(|| anyhow!("ANN sidecar file size overflow"))?;
    usize::try_from(file_size).context("ANN sidecar exceeds platform address space")?;
    Ok(SidecarLayout {
        ids_offset: HEADER_LEN_U64,
        ids_len,
        vectors_offset,
        vectors_len,
        file_size,
    })
}

#[cfg(test)]
pub(crate) fn expected_sidecar_size(vector_count: u64) -> Result<u64> {
    Ok(sidecar_layout(vector_count)?.file_size)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidecarHeader {
    source_id: SourceId,
    embedding_model_id: String,
    embedding_model_fingerprint: String,
    corpus_id: String,
    embedding_set_sha256: String,
    vector_count: u64,
    first_chunk_id: u32,
    last_chunk_id: u32,
    layout: SidecarLayout,
}

impl SidecarHeader {
    fn from_manifest(info: &ManifestAnn) -> Result<Self> {
        Ok(Self {
            source_id: info.source_id.clone(),
            embedding_model_id: info.embedding_model_id.clone(),
            embedding_model_fingerprint: info.embedding_model_fingerprint.clone(),
            corpus_id: info.corpus_id.clone(),
            embedding_set_sha256: info.embedding_set_sha256.clone(),
            vector_count: info.vector_count,
            first_chunk_id: info.first_chunk_id,
            last_chunk_id: info.last_chunk_id,
            layout: sidecar_layout(info.vector_count)?,
        })
    }

    fn from_identity(identity: &AnnIdentity) -> Result<Self> {
        Ok(Self {
            source_id: identity.source_id.clone(),
            embedding_model_id: EMBEDDING_MODEL_ID.to_string(),
            embedding_model_fingerprint: EMBEDDING_MODEL_FINGERPRINT.to_string(),
            corpus_id: identity.corpus_id.clone(),
            embedding_set_sha256: identity.embedding_set_sha256.clone(),
            vector_count: identity.vector_count,
            first_chunk_id: identity.first_chunk_id,
            last_chunk_id: identity.last_chunk_id,
            layout: sidecar_layout(identity.vector_count)?,
        })
    }
}

fn put_u16(header: &mut [u8; HEADER_LEN], offset: usize, value: u16) {
    header[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(header: &mut [u8; HEADER_LEN], offset: usize, value: u32) {
    header[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(header: &mut [u8; HEADER_LEN], offset: usize, value: u64) {
    header[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u16(header: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        header[offset..offset + 2]
            .try_into()
            .expect("header bounds"),
    )
}

fn read_u32(header: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        header[offset..offset + 4]
            .try_into()
            .expect("header bounds"),
    )
}

fn read_u64(header: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        header[offset..offset + 8]
            .try_into()
            .expect("header bounds"),
    )
}

fn decode_sha256(value: &str, label: &str) -> Result<[u8; 32]> {
    if !is_sha256(value) {
        bail!("{label} is not a lowercase SHA-256 digest");
    }
    let mut out = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).expect("hex digest is ASCII");
        out[index] = u8::from_str_radix(text, 16).expect("validated hexadecimal digest");
    }
    Ok(out)
}

fn encode_sha256(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_field(
    header: &mut [u8; HEADER_LEN],
    range: std::ops::Range<usize>,
    value: &str,
    label: &str,
) -> Result<u16> {
    if value.is_empty() || value.as_bytes().contains(&0) || value.len() > range.len() {
        bail!("ANN sidecar {label} does not fit its fixed header field");
    }
    header[range.start..range.start + value.len()].copy_from_slice(value.as_bytes());
    u16::try_from(value.len()).context("ANN sidecar header field length exceeds u16")
}

fn read_field(
    header: &[u8],
    range: std::ops::Range<usize>,
    length: u16,
    label: &str,
) -> Result<String> {
    let length = usize::from(length);
    if length == 0 || length > range.len() {
        bail!("ANN sidecar {label} length is invalid");
    }
    let field = &header[range.clone()];
    if field[length..].iter().any(|byte| *byte != 0) {
        bail!("ANN sidecar {label} padding is nonzero");
    }
    let value = std::str::from_utf8(&field[..length])
        .with_context(|| format!("ANN sidecar {label} is not UTF-8"))?;
    if value.as_bytes().contains(&0) {
        bail!("ANN sidecar {label} contains a NUL byte");
    }
    Ok(value.to_string())
}

fn encode_header(value: &SidecarHeader) -> Result<[u8; HEADER_LEN]> {
    let mut header = [0u8; HEADER_LEN];
    header[..MAGIC.len()].copy_from_slice(MAGIC);
    put_u32(&mut header, 16, ANN_FORMAT_VERSION);
    put_u32(&mut header, 20, HEADER_LEN as u32);
    put_u32(&mut header, 24, ENDIAN_MARKER);
    put_u32(&mut header, 28, EMBEDDING_DIM as u32);
    put_u32(&mut header, 32, ID_BYTES);
    put_u32(&mut header, 36, VECTOR_BYTES);
    put_u64(&mut header, 40, value.vector_count);
    put_u32(&mut header, 48, value.first_chunk_id);
    put_u32(&mut header, 52, value.last_chunk_id);
    put_u64(&mut header, 56, value.layout.ids_offset);
    put_u64(&mut header, 64, value.layout.ids_len);
    put_u64(&mut header, 72, value.layout.vectors_offset);
    put_u64(&mut header, 80, value.layout.vectors_len);
    put_u64(&mut header, 88, value.layout.file_size);

    let source_len = write_field(
        &mut header,
        SOURCE_FIELD,
        value.source_id.as_str(),
        "source ID",
    )?;
    let model_len = write_field(
        &mut header,
        MODEL_FIELD,
        &value.embedding_model_id,
        "model ID",
    )?;
    let metric_len = write_field(&mut header, METRIC_FIELD, ANN_METRIC, "metric")?;
    let encoding_len = write_field(
        &mut header,
        ID_ENCODING_FIELD,
        ANN_ID_ENCODING,
        "ID encoding",
    )?;
    let corpus_len = write_field(&mut header, CORPUS_FIELD, &value.corpus_id, "corpus ID")?;
    put_u16(&mut header, 96, source_len);
    put_u16(&mut header, 98, model_len);
    put_u16(&mut header, 100, metric_len);
    put_u16(&mut header, 102, encoding_len);
    put_u16(&mut header, 104, corpus_len);
    // 106..128 and 640..4096 remain reserved zero bytes.

    header[MODEL_FINGERPRINT_FIELD].copy_from_slice(&decode_sha256(
        &value.embedding_model_fingerprint,
        "model fingerprint",
    )?);
    header[EMBEDDING_SHA_FIELD].copy_from_slice(&decode_sha256(
        &value.embedding_set_sha256,
        "embedding-set SHA-256",
    )?);
    Ok(header)
}

fn decode_header(bytes: &[u8]) -> Result<SidecarHeader> {
    if bytes.len() < HEADER_LEN {
        bail!("ANN sidecar is shorter than its 4 KiB header");
    }
    let header = &bytes[..HEADER_LEN];
    if &header[..MAGIC.len()] != MAGIC {
        bail!("ANN sidecar magic is invalid");
    }
    if read_u32(header, 16) != ANN_FORMAT_VERSION
        || read_u32(header, 20) != HEADER_LEN as u32
        || read_u32(header, 24) != ENDIAN_MARKER
    {
        bail!("ANN sidecar version, header length, or endian marker is unsupported");
    }
    if read_u32(header, 28) != EMBEDDING_DIM as u32
        || read_u32(header, 32) != ID_BYTES
        || read_u32(header, 36) != VECTOR_BYTES
    {
        bail!("ANN sidecar vector or ID shape is incompatible with this binary");
    }
    if header[106..128].iter().any(|byte| *byte != 0)
        || header[HEADER_RESERVED_START..]
            .iter()
            .any(|byte| *byte != 0)
    {
        bail!("ANN sidecar reserved header bytes are nonzero");
    }

    let vector_count = read_u64(header, 40);
    let layout = sidecar_layout(vector_count)?;
    let encoded_layout = SidecarLayout {
        ids_offset: read_u64(header, 56),
        ids_len: read_u64(header, 64),
        vectors_offset: read_u64(header, 72),
        vectors_len: read_u64(header, 80),
        file_size: read_u64(header, 88),
    };
    if encoded_layout != layout {
        bail!("ANN sidecar plane offsets, lengths, or file size are noncanonical");
    }

    let source_text = read_field(header, SOURCE_FIELD, read_u16(header, 96), "source ID")?;
    let source_id = source_text
        .parse::<SourceId>()
        .with_context(|| format!("ANN sidecar source ID `{source_text}` is invalid"))?;
    let embedding_model_id = read_field(header, MODEL_FIELD, read_u16(header, 98), "model ID")?;
    let metric = read_field(header, METRIC_FIELD, read_u16(header, 100), "metric")?;
    let id_encoding = read_field(
        header,
        ID_ENCODING_FIELD,
        read_u16(header, 102),
        "ID encoding",
    )?;
    let corpus_id = read_field(header, CORPUS_FIELD, read_u16(header, 104), "corpus ID")?;
    if metric != ANN_METRIC || id_encoding != ANN_ID_ENCODING {
        bail!("ANN sidecar metric or ID encoding is incompatible with this binary");
    }

    let value = SidecarHeader {
        source_id,
        embedding_model_id,
        embedding_model_fingerprint: encode_sha256(&header[MODEL_FINGERPRINT_FIELD]),
        corpus_id,
        embedding_set_sha256: encode_sha256(&header[EMBEDDING_SHA_FIELD]),
        vector_count,
        first_chunk_id: read_u32(header, 48),
        last_chunk_id: read_u32(header, 52),
        layout,
    };
    if encode_header(&value)? != header {
        bail!("ANN sidecar header is not in canonical fixed-width form");
    }
    Ok(value)
}

fn enumerate_source_vectors(
    conn: &Connection,
    source_id: &SourceId,
    mut visit: impl FnMut(i64, u32, &[u8]) -> Result<()>,
) -> Result<(u64, u32, u32)> {
    let mut stmt = conn.prepare(SOURCE_VECTORS_SQL)?;
    let mut rows = stmt.query([source_id.as_str()])?;
    let mut previous = None;
    let mut first = None;
    let mut last = None;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        let chunk_id = row.get::<_, i64>(0)?;
        let embedding = row.get::<_, Vec<u8>>(1)?;
        let item_id = validate_vector_record(chunk_id, &embedding, previous)?;
        visit(chunk_id, item_id, &embedding)?;
        first.get_or_insert(item_id);
        last = Some(item_id);
        previous = Some(chunk_id);
        count = count
            .checked_add(1)
            .ok_or_else(|| anyhow!("ANN sidecar vector count overflow"))?;
    }
    let (Some(first), Some(last)) = (first, last) else {
        bail!("cannot build ANN sidecar for source `{source_id}` without chunk embeddings");
    };
    Ok((count, first, last))
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
    let (count, first_chunk_id, last_chunk_id) =
        enumerate_source_vectors(conn, source_id, |chunk_id, _, embedding| {
            embedding_hasher.update(chunk_id.to_le_bytes());
            embedding_hasher.update(embedding);
            Ok(())
        })?;
    let embedding_set_sha256 = format!("{:x}", embedding_hasher.finalize());

    let mut corpus_hasher = Sha256::new();
    corpus_hasher.update(b"australian-legal-mcp-flat-ann-corpus-v1\0");
    corpus_hasher.update(source_id.as_str().as_bytes());
    corpus_hasher.update([0]);
    corpus_hasher.update(source_index_sha256.as_bytes());
    corpus_hasher.update([0]);
    corpus_hasher.update(EMBEDDING_MODEL_ID.as_bytes());
    corpus_hasher.update([0]);
    corpus_hasher.update(EMBEDDING_MODEL_FINGERPRINT.as_bytes());
    corpus_hasher.update([0]);
    corpus_hasher.update((EMBEDDING_DIM as u64).to_le_bytes());
    corpus_hasher.update(count.to_le_bytes());
    corpus_hasher.update(first_chunk_id.to_le_bytes());
    corpus_hasher.update(last_chunk_id.to_le_bytes());
    corpus_hasher.update(embedding_set_sha256.as_bytes());
    Ok(AnnIdentity {
        source_id: source_id.clone(),
        corpus_id: format!("sha256:{:x}", corpus_hasher.finalize()),
        embedding_set_sha256,
        vector_count: count,
        first_chunk_id,
        last_chunk_id,
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
    let header_value = SidecarHeader::from_identity(&actual_identity)?;
    let layout = header_value.layout;
    let output = output_root.join(sidecar_relative_path(source_id));
    let parent = output
        .parent()
        .ok_or_else(|| anyhow!("ANN output has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let temp_dir = tempfile::Builder::new()
        .prefix(&format!("ann-{source_id}-build-"))
        .tempdir_in(parent)?;
    let build_path = temp_dir.path().join(format!("{source_id}.ann.part"));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .read(true)
        .open(&build_path)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, file);
    writer.write_all(&[0u8; HEADER_LEN])?;

    let (ids_written, first, last) = enumerate_source_vectors(conn, source_id, |_, item_id, _| {
        writer.write_all(&item_id.to_le_bytes())?;
        Ok(())
    })?;
    if ids_written != actual_identity.vector_count
        || first != actual_identity.first_chunk_id
        || last != actual_identity.last_chunk_id
    {
        bail!("ANN source ID set changed during sidecar construction");
    }
    let ids_end = layout
        .ids_offset
        .checked_add(layout.ids_len)
        .ok_or_else(|| anyhow!("ANN sidecar ID end overflow"))?;
    let padding = layout
        .vectors_offset
        .checked_sub(ids_end)
        .ok_or_else(|| anyhow!("ANN sidecar vector plane overlaps its ID plane"))?;
    let padding = usize::try_from(padding).context("ANN sidecar padding exceeds usize")?;
    writer.write_all(&vec![0u8; padding])?;

    let mut embedding_hasher = Sha256::new();
    embedding_hasher.update(b"australian-legal-mcp-embedding-set-v1\0");
    let (vectors_written, vector_first, vector_last) =
        enumerate_source_vectors(conn, source_id, |chunk_id, _, embedding| {
            embedding_hasher.update(chunk_id.to_le_bytes());
            embedding_hasher.update(embedding);
            writer.write_all(embedding)?;
            Ok(())
        })?;
    let written_embedding_sha = format!("{:x}", embedding_hasher.finalize());
    if vectors_written != actual_identity.vector_count
        || vector_first != actual_identity.first_chunk_id
        || vector_last != actual_identity.last_chunk_id
        || written_embedding_sha != actual_identity.embedding_set_sha256
    {
        bail!("ANN embedding set changed during sidecar construction");
    }
    writer.flush()?;
    let mut file = writer
        .into_inner()
        .map_err(|error| anyhow!("flushing ANN sidecar: {}", error.error()))?;
    if file.metadata()?.len() != layout.file_size {
        bail!("built ANN sidecar length does not match its canonical layout");
    }
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&encode_header(&header_value)?)?;
    file.sync_all()?;
    drop(file);

    let size = fs::metadata(&build_path)?.len();
    let sha256 = sha256_path(&build_path)?;
    let manifest = ManifestAnn {
        source_id: source_id.clone(),
        format: ANN_FORMAT.to_string(),
        format_version: ANN_FORMAT_VERSION,
        path: sidecar_manifest_path(source_id),
        sha256,
        size,
        corpus_id: actual_identity.corpus_id.clone(),
        embedding_model_id: EMBEDDING_MODEL_ID.to_string(),
        embedding_model_fingerprint: EMBEDDING_MODEL_FINGERPRINT.to_string(),
        embedding_dimension: EMBEDDING_DIM as u32,
        embedding_set_sha256: actual_identity.embedding_set_sha256.clone(),
        vector_count: actual_identity.vector_count,
        first_chunk_id: actual_identity.first_chunk_id,
        last_chunk_id: actual_identity.last_chunk_id,
        id_encoding: ANN_ID_ENCODING.to_string(),
        metric: ANN_METRIC.to_string(),
    };
    validate_manifest_ann(source_id, &manifest)?;
    drop(open_verified_sidecar(&build_path, source_id, &manifest)?);
    replace_file(&build_path, &output)?;
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
    if info.format != ANN_FORMAT || info.format_version != ANN_FORMAT_VERSION {
        bail!(
            "unsupported ANN sidecar {} v{}",
            info.format,
            info.format_version
        );
    }
    if info.embedding_model_id != EMBEDDING_MODEL_ID
        || info.embedding_model_fingerprint != EMBEDDING_MODEL_FINGERPRINT
        || info.embedding_dimension != EMBEDDING_DIM as u32
        || info.id_encoding != ANN_ID_ENCODING
        || info.metric != ANN_METRIC
    {
        bail!("ANN sidecar search contract is incompatible with this binary");
    }
    if !is_sha256(&info.sha256)
        || !is_corpus_id(&info.corpus_id)
        || !is_sha256(&info.embedding_set_sha256)
        || info.vector_count == 0
        || info.first_chunk_id > info.last_chunk_id
    {
        bail!("ANN sidecar integrity metadata is malformed");
    }
    if u64::from(info.last_chunk_id - info.first_chunk_id) + 1 != info.vector_count {
        bail!("ANN sidecar chunk range is not contiguous for source `{source_id}`");
    }
    let layout = sidecar_layout(info.vector_count)?;
    if info.size != layout.file_size {
        bail!("ANN sidecar manifest size does not match its canonical plane layout");
    }
    Ok(())
}

fn open_regular_file(path: &Path, expected_size: u64) -> Result<File> {
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading ANN sidecar metadata at {}", path.display()))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        bail!("ANN sidecar must be a regular non-symlink file");
    }
    #[cfg(not(windows))]
    let mut options = OpenOptions::new();
    #[cfg(not(windows))]
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(not(windows))]
    let file = options
        .open(path)
        .with_context(|| format!("opening ANN sidecar {}", path.display()))?;
    #[cfg(windows)]
    let file = open_windows_sidecar_handle(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("ANN sidecar opened object is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            bail!("ANN sidecar must not be hard-linked");
        }
        if metadata.dev() != path_metadata.dev() || metadata.ino() != path_metadata.ino() {
            bail!("ANN sidecar changed while it was being opened");
        }
    }
    #[cfg(windows)]
    {
        let identity = windows_file_identity(&file)?;
        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if identity.file_attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        {
            bail!("ANN sidecar handle resolves to a directory or reparse point");
        }
        if identity.number_of_links != 1 {
            bail!("ANN sidecar must not be hard-linked");
        }
        let verification = open_windows_sidecar_handle(path)?;
        if windows_file_identity(&verification)? != identity {
            bail!("ANN sidecar path identity changed while it was being opened");
        }
    }
    if metadata.len() != expected_size {
        bail!(
            "ANN sidecar size mismatch: manifest {expected_size}, installed {}",
            metadata.len()
        );
    }
    Ok(file)
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WindowsFileIdentity {
    pub(crate) file_attributes: u32,
    pub(crate) volume_serial_number: u32,
    pub(crate) file_index: u64,
    pub(crate) number_of_links: u32,
}

#[cfg(windows)]
fn open_windows_sidecar_handle(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut options = OpenOptions::new();
    options
        .read(true)
        // Keep readers possible, but deny every writer and delete/rename
        // request for as long as FlatAnn retains this handle.
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options
        .open(path)
        .with_context(|| format!("securely opening Windows ANN sidecar {}", path.display()))
}

#[cfg(windows)]
pub(crate) fn windows_file_identity(file: &File) -> Result<WindowsFileIdentity> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let success =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if success == 0 {
        return Err(std::io::Error::last_os_error()).context("reading Windows ANN file identity");
    }
    let information = unsafe { information.assume_init() };
    Ok(WindowsFileIdentity {
        file_attributes: information.dwFileAttributes,
        volume_serial_number: information.dwVolumeSerialNumber,
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
        number_of_links: information.nNumberOfLinks,
    })
}

pub(crate) struct FlatAnn {
    map: Mmap,
    // The Windows opener denies write/delete sharing. Retaining this handle for
    // the complete mmap lifetime makes mutation and replacement impossible.
    _backing_file: File,
    header: SidecarHeader,
    ids: std::ops::Range<usize>,
    vectors: std::ops::Range<usize>,
    count: usize,
}

impl std::fmt::Debug for FlatAnn {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FlatAnn")
            .field("source_id", &self.header.source_id)
            .field("vector_count", &self.header.vector_count)
            .field("file_size", &self.header.layout.file_size)
            .finish()
    }
}

impl FlatAnn {
    #[cfg(test)]
    pub(crate) fn vector_count(&self) -> usize {
        self.count
    }

    fn chunk_id(&self, row: usize) -> u32 {
        let offset = self.ids.start + row * usize::try_from(ID_BYTES).expect("ID width");
        u32::from_le_bytes(
            self.map[offset..offset + 4]
                .try_into()
                .expect("validated ID plane bounds"),
        )
    }

    fn vector(&self, row: usize) -> &[u8] {
        let offset = self.vectors.start + row * EMBEDDING_DIM;
        &self.map[offset..offset + EMBEDDING_DIM]
    }

    pub(crate) fn eligible_rows(&self, chunk_ids: &RoaringBitmap) -> Result<EligibleRows> {
        if chunk_ids.is_empty() {
            return Ok(EligibleRows {
                words: Box::new([]),
                row_count: self.count,
                eligible_count: 0,
            });
        }
        let word_count = self
            .count
            .checked_add(63)
            .ok_or_else(|| anyhow!("eligibility bitmap length overflow"))?
            / 64;
        let mut words = vec![0u64; word_count];
        let mut matched = 0u64;
        for row in 0..self.count {
            if chunk_ids.contains(self.chunk_id(row)) {
                words[row / 64] |= 1u64 << (row % 64);
                matched += 1;
            }
        }
        if matched != chunk_ids.len() {
            bail!(
                "eligible filter returned {} chunk IDs absent from source `{}` sidecar",
                chunk_ids.len() - matched,
                self.header.source_id
            );
        }
        Ok(EligibleRows {
            words: words.into_boxed_slice(),
            row_count: self.count,
            eligible_count: matched,
        })
    }
}

#[derive(Debug)]
pub(crate) struct EligibleRows {
    words: Box<[u64]>,
    row_count: usize,
    eligible_count: u64,
}

impl EligibleRows {
    #[cfg(test)]
    pub(crate) fn empty_for_test(row_count: usize) -> Self {
        Self {
            words: Box::new([]),
            row_count,
            eligible_count: 0,
        }
    }

    pub(crate) fn len(&self) -> u64 {
        self.eligible_count
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.eligible_count == 0
    }

    #[inline]
    fn contains_row(&self, row: usize) -> bool {
        !self.words.is_empty() && (self.words[row / 64] & (1u64 << (row % 64))) != 0
    }
}

pub(crate) fn open_verified_sidecar(
    path: &Path,
    source_id: &SourceId,
    info: &ManifestAnn,
) -> Result<FlatAnn> {
    validate_manifest_ann(source_id, info)?;
    let file = open_regular_file(path, info.size)?;
    let map_len = usize::try_from(info.size).context("ANN sidecar size exceeds usize")?;
    // SAFETY: the file is opened read-only, has the exact immutable generation
    // size, and the returned mapping is never exposed mutably by this module.
    let map = unsafe { MmapOptions::new().len(map_len).map(&file) }
        .with_context(|| format!("memory-mapping ANN sidecar {}", path.display()))?;
    if map.len() != map_len {
        bail!("ANN sidecar mmap length differs from its manifest");
    }
    let whole_file_sha256 = format!("{:x}", Sha256::digest(&map[..]));
    if whole_file_sha256 != info.sha256 {
        bail!("ANN sidecar SHA-256 mismatch");
    }
    let header = decode_header(&map)?;
    let expected_header = SidecarHeader::from_manifest(info)?;
    if header != expected_header {
        bail!("ANN sidecar header identity does not match its manifest");
    }
    if header.layout.file_size != info.size {
        bail!("ANN sidecar header file size does not match its manifest");
    }

    let ids = usize::try_from(header.layout.ids_offset)?
        ..usize::try_from(
            header
                .layout
                .ids_offset
                .checked_add(header.layout.ids_len)
                .ok_or_else(|| anyhow!("ANN ID range overflow"))?,
        )?;
    let vectors = usize::try_from(header.layout.vectors_offset)?
        ..usize::try_from(
            header
                .layout
                .vectors_offset
                .checked_add(header.layout.vectors_len)
                .ok_or_else(|| anyhow!("ANN vector range overflow"))?,
        )?;
    if ids.start != HEADER_LEN
        || ids.end > vectors.start
        || vectors.end != map.len()
        || map[ids.end..vectors.start].iter().any(|byte| *byte != 0)
    {
        bail!("ANN sidecar planes overlap, are out of bounds, or have nonzero padding");
    }
    let count = usize::try_from(header.vector_count)
        .context("ANN vector count exceeds platform address space")?;
    let sidecar = FlatAnn {
        map,
        _backing_file: file,
        header,
        ids,
        vectors,
        count,
    };

    let mut previous = None;
    let mut embedding_hasher = Sha256::new();
    embedding_hasher.update(b"australian-legal-mcp-embedding-set-v1\0");
    for row in 0..sidecar.count {
        let chunk_id = sidecar.chunk_id(row);
        if previous.is_some_and(|value| value >= chunk_id) {
            bail!("ANN sidecar chunk IDs are not strictly increasing at {chunk_id}");
        }
        embedding_hasher.update(i64::from(chunk_id).to_le_bytes());
        embedding_hasher.update(sidecar.vector(row));
        previous = Some(chunk_id);
    }
    if sidecar.chunk_id(0) != info.first_chunk_id
        || sidecar.chunk_id(sidecar.count - 1) != info.last_chunk_id
    {
        bail!("ANN sidecar first or last chunk ID does not match its manifest");
    }
    let actual_embedding_sha256 = format!("{:x}", embedding_hasher.finalize());
    if actual_embedding_sha256 != info.embedding_set_sha256 {
        bail!("ANN sidecar embedding-set SHA-256 mismatch");
    }
    Ok(sidecar)
}

pub(crate) fn verify_sidecar(path: &Path, source_id: &SourceId, info: &ManifestAnn) -> Result<()> {
    drop(open_verified_sidecar(path, source_id, info)?);
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AnnSearchHit {
    pub(crate) chunk_id: u32,
    pub(crate) score: i32,
}

impl AnnSearchHit {
    fn better_than(self, other: Self) -> bool {
        self.score > other.score || (self.score == other.score && self.chunk_id < other.chunk_id)
    }
}

impl Ord for AnnSearchHit {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap's root is the worst retained hit: lower score first, then
        // larger chunk ID for an exact score tie.
        other
            .score
            .cmp(&self.score)
            .then_with(|| self.chunk_id.cmp(&other.chunk_id))
    }
}

impl PartialOrd for AnnSearchHit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct TopK {
    limit: usize,
    heap: BinaryHeap<AnnSearchHit>,
}

impl TopK {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            heap: BinaryHeap::with_capacity(limit.saturating_add(1)),
        }
    }

    fn push(&mut self, hit: AnnSearchHit) {
        if self.heap.len() < self.limit {
            self.heap.push(hit);
        } else if self
            .heap
            .peek()
            .is_some_and(|worst| hit.better_than(*worst))
        {
            self.heap.pop();
            self.heap.push(hit);
        }
    }

    fn merge(mut self, other: Self) -> Self {
        for hit in other.heap {
            self.push(hit);
        }
        self
    }

    fn into_sorted(self) -> Vec<AnnSearchHit> {
        let mut hits = self.heap.into_vec();
        hits.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.chunk_id.cmp(&right.chunk_id))
        });
        hits
    }
}

fn scan_pool() -> Result<&'static rayon::ThreadPool> {
    static POOL: OnceLock<std::result::Result<rayon::ThreadPool, String>> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(SCAN_THREADS)
            .clamp(1, SCAN_THREADS);
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("flat-ann-scan-{index}"))
            .build()
            .map_err(|error| error.to_string())
    })
    .as_ref()
    .map_err(|message| anyhow!("creating bounded ANN scan pool: {message}"))
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use std::arch::x86_64::{
    __m128i, __m256i, _mm256_add_epi32, _mm256_castsi256_si128, _mm256_cvtepi8_epi16,
    _mm256_dpbusd_avx_epi32, _mm256_extracti128_si256, _mm256_loadu_si256, _mm256_madd_epi16,
    _mm256_set1_epi8, _mm256_setzero_si256, _mm256_xor_si256, _mm_add_epi32, _mm_cvtsi128_si32,
    _mm_hadd_epi32, _mm_loadu_si128,
};

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
struct VnniQuery {
    blocks: [__m256i; 8],
    correction: i32,
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
enum PreparedDot {
    Vnni(Box<VnniQuery>),
    Avx2(Box<[__m256i; 16]>),
    Scalar(Box<[i8; EMBEDDING_DIM]>),
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
enum PreparedDot {
    Scalar(Box<[i8; EMBEDDING_DIM]>),
}

impl PreparedDot {
    fn new(query: &[i8; EMBEDDING_DIM]) -> Self {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            if std::arch::is_x86_feature_detected!("avxvnni")
                && std::arch::is_x86_feature_detected!("avx2")
            {
                // SAFETY: runtime feature detection above proves both target
                // features required by query preparation.
                return Self::Vnni(Box::new(unsafe { prepare_vnni_query(query) }));
            }
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: runtime feature detection above proves AVX2 support.
                return Self::Avx2(Box::new(unsafe { prepare_avx2_query(query) }));
            }
        }
        Self::Scalar(Box::new(*query))
    }

    #[cfg(test)]
    #[inline]
    fn score(&self, document: &[u8]) -> i32 {
        debug_assert_eq!(document.len(), EMBEDDING_DIM);
        match self {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Self::Vnni(query) => {
                // SAFETY: this variant is created only after AVX-VNNI and AVX2
                // runtime detection; every vector is exactly 256 bytes.
                unsafe { dot_vnni(document.as_ptr().cast(), query) }
            }
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Self::Avx2(query) => {
                // SAFETY: this variant is created only after AVX2 runtime
                // detection; every vector is exactly 256 bytes.
                unsafe { dot_avx2(document.as_ptr().cast(), query) }
            }
            Self::Scalar(query) => dot_scalar(query, document),
        }
    }

    fn scan_range(
        &self,
        sidecar: &FlatAnn,
        eligible: &EligibleRows,
        begin: usize,
        end: usize,
        top: &mut TopK,
    ) {
        match self {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Self::Vnni(query) => {
                // SAFETY: this variant is created only after AVX-VNNI and
                // AVX2 runtime detection; validated rows have fixed vectors.
                unsafe { scan_range_vnni(sidecar, eligible, begin, end, query, top) }
            }
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Self::Avx2(query) => {
                // SAFETY: this variant is created only after AVX2 runtime
                // detection; validated rows have fixed vectors.
                unsafe { scan_range_avx2(sidecar, eligible, begin, end, query, top) }
            }
            Self::Scalar(query) => {
                for row in begin..end {
                    if eligible.contains_row(row) {
                        top.push(AnnSearchHit {
                            chunk_id: sidecar.chunk_id(row),
                            score: dot_scalar(query, sidecar.vector(row)),
                        });
                    }
                }
            }
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2,avxvnni")]
unsafe fn prepare_vnni_query(query: &[i8; EMBEDDING_DIM]) -> VnniQuery {
    let blocks = std::array::from_fn(|index| {
        // SAFETY: every 32-byte load lies within the fixed 256-byte query.
        unsafe { _mm256_loadu_si256(query.as_ptr().add(index * 32).cast()) }
    });
    let correction = 128 * query.iter().map(|value| i32::from(*value)).sum::<i32>();
    VnniQuery { blocks, correction }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn prepare_avx2_query(query: &[i8; EMBEDDING_DIM]) -> [__m256i; 16] {
    std::array::from_fn(|index| {
        // SAFETY: every 16-byte load lies within the fixed 256-byte query.
        let bytes = unsafe { _mm_loadu_si128(query.as_ptr().add(index * 16).cast::<__m128i>()) };
        _mm256_cvtepi8_epi16(bytes)
    })
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn horizontal_sum(value: __m256i) -> i32 {
    let low = _mm256_castsi256_si128(value);
    let high = _mm256_extracti128_si256::<1>(value);
    let sum = _mm_add_epi32(low, high);
    let sum = _mm_hadd_epi32(sum, sum);
    let sum = _mm_hadd_epi32(sum, sum);
    _mm_cvtsi128_si32(sum)
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2,avxvnni")]
unsafe fn dot_vnni(document: *const i8, query: &VnniQuery) -> i32 {
    // AVX-VNNI multiplies unsigned document bytes by signed query bytes. XOR
    // maps each signed document component d to unsigned d+128; subtracting
    // 128*sum(query) restores the exact signed-by-signed dot product.
    let flip = _mm256_set1_epi8(0x80u8 as i8);
    let mut a0 = _mm256_setzero_si256();
    let mut a1 = _mm256_setzero_si256();
    let mut a2 = _mm256_setzero_si256();
    let mut a3 = _mm256_setzero_si256();
    macro_rules! block {
        ($acc:ident, $index:expr) => {{
            // SAFETY: callers provide a 256-byte vector and every unaligned
            // 32-byte load remains within that vector.
            let document_block =
                unsafe { _mm256_loadu_si256(document.add($index * 32).cast::<__m256i>()) };
            let unsigned_document = _mm256_xor_si256(document_block, flip);
            $acc = _mm256_dpbusd_avx_epi32($acc, unsigned_document, query.blocks[$index]);
        }};
    }
    block!(a0, 0);
    block!(a1, 1);
    block!(a2, 2);
    block!(a3, 3);
    block!(a0, 4);
    block!(a1, 5);
    block!(a2, 6);
    block!(a3, 7);
    // SAFETY: this function's target features satisfy horizontal_sum.
    unsafe {
        horizontal_sum(_mm256_add_epi32(
            _mm256_add_epi32(a0, a1),
            _mm256_add_epi32(a2, a3),
        )) - query.correction
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn dot_avx2(document: *const i8, query: &[__m256i; 16]) -> i32 {
    let mut a0 = _mm256_setzero_si256();
    let mut a1 = _mm256_setzero_si256();
    let mut a2 = _mm256_setzero_si256();
    let mut a3 = _mm256_setzero_si256();
    macro_rules! block {
        ($acc:ident, $index:expr) => {{
            // SAFETY: callers provide a 256-byte vector and every unaligned
            // 16-byte load remains within that vector.
            let bytes = unsafe { _mm_loadu_si128(document.add($index * 16).cast::<__m128i>()) };
            let wide = _mm256_cvtepi8_epi16(bytes);
            $acc = _mm256_add_epi32($acc, _mm256_madd_epi16(wide, query[$index]));
        }};
    }
    block!(a0, 0);
    block!(a1, 1);
    block!(a2, 2);
    block!(a3, 3);
    block!(a0, 4);
    block!(a1, 5);
    block!(a2, 6);
    block!(a3, 7);
    block!(a0, 8);
    block!(a1, 9);
    block!(a2, 10);
    block!(a3, 11);
    block!(a0, 12);
    block!(a1, 13);
    block!(a2, 14);
    block!(a3, 15);
    // SAFETY: this function's target features satisfy horizontal_sum.
    unsafe {
        horizontal_sum(_mm256_add_epi32(
            _mm256_add_epi32(a0, a1),
            _mm256_add_epi32(a2, a3),
        ))
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2,avxvnni")]
unsafe fn scan_range_vnni(
    sidecar: &FlatAnn,
    eligible: &EligibleRows,
    begin: usize,
    end: usize,
    query: &VnniQuery,
    top: &mut TopK,
) {
    for row in begin..end {
        if !eligible.contains_row(row) {
            continue;
        }
        top.push(AnnSearchHit {
            chunk_id: sidecar.chunk_id(row),
            // SAFETY: FlatAnn returns a validated 256-byte vector.
            score: unsafe { dot_vnni(sidecar.vector(row).as_ptr().cast(), query) },
        });
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn scan_range_avx2(
    sidecar: &FlatAnn,
    eligible: &EligibleRows,
    begin: usize,
    end: usize,
    query: &[__m256i; 16],
    top: &mut TopK,
) {
    for row in begin..end {
        if !eligible.contains_row(row) {
            continue;
        }
        top.push(AnnSearchHit {
            chunk_id: sidecar.chunk_id(row),
            // SAFETY: FlatAnn returns a validated 256-byte vector.
            score: unsafe { dot_avx2(sidecar.vector(row).as_ptr().cast(), query) },
        });
    }
}

#[inline]
fn dot_scalar(query: &[i8; EMBEDDING_DIM], document: &[u8]) -> i32 {
    query
        .iter()
        .zip(document)
        .map(|(&query_component, &document_component)| {
            i32::from(query_component) * i32::from(document_component as i8)
        })
        .sum()
}

pub(crate) fn scan_sidecar(
    sidecar: &FlatAnn,
    query: &[i8; EMBEDDING_DIM],
    eligible: &EligibleRows,
    limit: usize,
) -> Result<Vec<AnnSearchHit>> {
    if eligible.row_count != sidecar.count {
        bail!("eligible bitmap row count does not match ANN sidecar");
    }
    if limit == 0 || eligible.is_empty() {
        return Ok(Vec::new());
    }
    let wanted = limit.min(usize::try_from(eligible.len()).unwrap_or(usize::MAX));
    let prepared = PreparedDot::new(query);
    let blocks = sidecar.count.div_ceil(SCAN_BLOCK_ROWS);
    let top = scan_pool()?.install(|| {
        (0..blocks)
            .into_par_iter()
            .fold(
                || TopK::new(wanted),
                |mut top, block| {
                    let begin = block * SCAN_BLOCK_ROWS;
                    let end = sidecar.count.min(begin + SCAN_BLOCK_ROWS);
                    prepared.scan_range(sidecar, eligible, begin, end, &mut top);
                    top
                },
            )
            .reduce(|| TopK::new(wanted), TopK::merge)
    });
    let hits = top.into_sorted();
    if hits.len() != wanted {
        bail!(
            "exact ANN scan underfilled: expected {wanted} eligible hits, found {}",
            hits.len()
        );
    }
    Ok(hits)
}

fn sha256_path(path: &Path) -> Result<String> {
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
    use std::sync::Arc;

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
                    % 256) as i16
                    - 128;
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
        drop(insert_embedding);
        drop(insert_chunk);
        Ok(conn)
    }

    fn exact_rank(
        conn: &Connection,
        source_id: &SourceId,
        query: &[i8; EMBEDDING_DIM],
        eligible: &RoaringBitmap,
        limit: usize,
    ) -> Result<Vec<AnnSearchHit>> {
        let mut exact = Vec::new();
        enumerate_source_vectors(conn, source_id, |_, item_id, embedding| {
            if eligible.contains(item_id) {
                exact.push(AnnSearchHit {
                    chunk_id: item_id,
                    score: dot_scalar(query, embedding),
                });
            }
            Ok(())
        })?;
        exact.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.chunk_id.cmp(&right.chunk_id))
        });
        exact.truncate(limit);
        Ok(exact)
    }

    #[test]
    fn source_sidecars_are_isolated_deterministic_and_exact() -> Result<()> {
        let count = 513;
        let conn = test_connection(count)?;
        let primary = source(PRIMARY_SOURCE)?;
        let secondary = source(SECONDARY_SOURCE)?;
        let source_sha = "a".repeat(64);
        let primary_identity = compute_identity(&conn, &primary, &source_sha)?;
        let secondary_identity = compute_identity(&conn, &secondary, &source_sha)?;
        assert_ne!(primary_identity.corpus_id, secondary_identity.corpus_id);
        assert_ne!(
            primary_identity.embedding_set_sha256,
            secondary_identity.embedding_set_sha256
        );

        let first_root = tempfile::tempdir()?;
        let second_root = tempfile::tempdir()?;
        let first_info = build_sidecar(
            &conn,
            &primary,
            first_root.path(),
            &source_sha,
            &primary_identity,
        )?;
        let second_info = build_sidecar(
            &conn,
            &primary,
            second_root.path(),
            &source_sha,
            &primary_identity,
        )?;
        let secondary_info = build_sidecar(
            &conn,
            &secondary,
            first_root.path(),
            &source_sha,
            &secondary_identity,
        )?;
        let first_path = first_root.path().join(sidecar_relative_path(&primary));
        let second_path = second_root.path().join(sidecar_relative_path(&primary));
        let secondary_path = first_root.path().join(sidecar_relative_path(&secondary));
        assert_eq!(first_info, second_info);
        assert_eq!(fs::read(&first_path)?, fs::read(&second_path)?);
        assert_eq!(first_info.path, "ann/source-a.ann");
        assert_eq!(secondary_info.path, "ann/source-b.ann");
        assert_eq!(first_info.first_chunk_id, PRIMARY_FIRST_CHUNK);
        assert_eq!(first_info.last_chunk_id, PRIMARY_FIRST_CHUNK + count - 1);

        let sidecar = open_verified_sidecar(&first_path, &primary, &first_info)?;
        assert_eq!(sidecar._backing_file.metadata()?.len(), first_info.size);
        let secondary_sidecar =
            open_verified_sidecar(&secondary_path, &secondary, &secondary_info)?;
        let eligible = RoaringBitmap::from_iter([3, 9, 17, 101, 509]);
        let rows = sidecar.eligible_rows(&eligible)?;
        let query: [i8; EMBEDDING_DIM] = vector(19, 77)
            .into_iter()
            .map(|byte| byte as i8)
            .collect::<Vec<_>>()
            .try_into()
            .expect("fixed dimensions");
        let found = scan_sidecar(&sidecar, &query, &rows, 5)?;
        assert_eq!(found, exact_rank(&conn, &primary, &query, &eligible, 5)?);

        let foreign = RoaringBitmap::from_iter([PRIMARY_FIRST_CHUNK, SECONDARY_FIRST_CHUNK]);
        assert!(sidecar.eligible_rows(&foreign).is_err());
        assert_eq!(secondary_sidecar.vector_count(), count as usize);
        Ok(())
    }

    fn rewrite_and_rebind(
        source_path: &Path,
        output_path: &Path,
        info: &ManifestAnn,
        mutate: impl FnOnce(&mut Vec<u8>),
    ) -> Result<ManifestAnn> {
        let mut bytes = fs::read(source_path)?;
        mutate(&mut bytes);
        fs::write(output_path, &bytes)?;
        let mut rebound = info.clone();
        rebound.size = bytes.len() as u64;
        rebound.sha256 = sha256_path(output_path)?;
        Ok(rebound)
    }

    #[test]
    fn sidecar_validation_rejects_corruption_noncanonical_layout_and_old_fields() -> Result<()> {
        let conn = test_connection(80)?;
        let primary = source(PRIMARY_SOURCE)?;
        let secondary = source(SECONDARY_SOURCE)?;
        let source_sha = "b".repeat(64);
        let identity = compute_identity(&conn, &primary, &source_sha)?;
        let dir = tempfile::tempdir()?;
        let info = build_sidecar(&conn, &primary, dir.path(), &source_sha, &identity)?;
        let path = dir.path().join(sidecar_relative_path(&primary));

        assert!(verify_sidecar(&path, &secondary, &info).is_err());
        let missing = dir.path().join("missing.ann");
        assert!(verify_sidecar(&missing, &primary, &info).is_err());

        let reserved = dir.path().join("reserved.ann");
        let reserved_info = rewrite_and_rebind(&path, &reserved, &info, |bytes| bytes[700] = 1)?;
        let error = verify_sidecar(&reserved, &primary, &reserved_info).unwrap_err();
        assert!(error.to_string().contains("reserved"));

        let bad_layout = dir.path().join("bad-layout.ann");
        let bad_layout_info = rewrite_and_rebind(&path, &bad_layout, &info, |bytes| {
            bytes[72..80].copy_from_slice(&0u64.to_le_bytes());
        })?;
        let error = verify_sidecar(&bad_layout, &primary, &bad_layout_info).unwrap_err();
        assert!(error.to_string().contains("offsets"));

        let overflow = dir.path().join("overflow.ann");
        let overflow_info = rewrite_and_rebind(&path, &overflow, &info, |bytes| {
            bytes[40..48].copy_from_slice(&u64::MAX.to_le_bytes());
        })?;
        let error = verify_sidecar(&overflow, &primary, &overflow_info).unwrap_err();
        assert!(error.to_string().contains("overflow"));

        let unsorted = dir.path().join("unsorted.ann");
        let unsorted_info = rewrite_and_rebind(&path, &unsorted, &info, |bytes| {
            let first = bytes[HEADER_LEN..HEADER_LEN + 4].to_vec();
            let second = bytes[HEADER_LEN + 4..HEADER_LEN + 8].to_vec();
            bytes[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&second);
            bytes[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&first);
        })?;
        let error = verify_sidecar(&unsorted, &primary, &unsorted_info).unwrap_err();
        assert!(error.to_string().contains("strictly increasing"));

        let vector_corrupt = dir.path().join("vector-corrupt.ann");
        let vector_info = rewrite_and_rebind(&path, &vector_corrupt, &info, |bytes| {
            *bytes.last_mut().expect("nonempty sidecar") ^= 0x5a;
        })?;
        let error = verify_sidecar(&vector_corrupt, &primary, &vector_info).unwrap_err();
        assert!(error.to_string().contains("embedding-set"));

        let trailing = dir.path().join("trailing.ann");
        let trailing_info = rewrite_and_rebind(&path, &trailing, &info, |bytes| bytes.push(0))?;
        assert!(validate_manifest_ann(&primary, &trailing_info).is_err());

        let truncated = dir.path().join("truncated.ann");
        let bytes = fs::read(&path)?;
        fs::write(&truncated, &bytes[..bytes.len() - 1])?;
        assert!(verify_sidecar(&truncated, &primary, &info).is_err());

        let mut wrong_model = info.clone();
        wrong_model.embedding_model_fingerprint = "0".repeat(64);
        assert!(validate_manifest_ann(&primary, &wrong_model).is_err());
        let mut wrong_count = info.clone();
        wrong_count.vector_count = u64::MAX;
        assert!(validate_manifest_ann(&primary, &wrong_count).is_err());

        let mut old_shape = serde_json::to_value(&info)?;
        old_shape
            .as_object_mut()
            .expect("manifest object")
            .insert("trees".to_string(), serde_json::json!(16));
        assert!(serde_json::from_value::<ManifestAnn>(old_shape).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_rejects_symlinks_and_hard_links() -> Result<()> {
        use std::os::unix::fs::symlink;

        let conn = test_connection(2)?;
        let primary = source(PRIMARY_SOURCE)?;
        let source_sha = "c".repeat(64);
        let identity = compute_identity(&conn, &primary, &source_sha)?;
        let dir = tempfile::tempdir()?;
        let info = build_sidecar(&conn, &primary, dir.path(), &source_sha, &identity)?;
        let path = dir.path().join(sidecar_relative_path(&primary));
        let link = dir.path().join("link.ann");
        symlink(&path, &link)?;
        assert!(verify_sidecar(&link, &primary, &info).is_err());
        let hard = dir.path().join("hard.ann");
        fs::hard_link(&path, &hard)?;
        assert!(verify_sidecar(&path, &primary, &info).is_err());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_sidecar_handle_rejects_links_and_denies_mutation_sharing() -> Result<()> {
        let conn = test_connection(2)?;
        let primary = source(PRIMARY_SOURCE)?;
        let source_sha = "c".repeat(64);
        let identity = compute_identity(&conn, &primary, &source_sha)?;
        let dir = tempfile::tempdir()?;
        let info = build_sidecar(&conn, &primary, dir.path(), &source_sha, &identity)?;
        let path = dir.path().join(sidecar_relative_path(&primary));

        let hard_link = dir.path().join("hard-link.ann");
        fs::hard_link(&path, &hard_link)?;
        assert!(open_verified_sidecar(&path, &primary, &info).is_err());
        fs::remove_file(&hard_link)?;

        let sidecar = open_verified_sidecar(&path, &primary, &info)?;
        assert_eq!(sidecar._backing_file.metadata()?.len(), info.size);
        assert!(OpenOptions::new().write(true).open(&path).is_err());
        assert!(fs::remove_file(&path).is_err());
        drop(sidecar);
        OpenOptions::new().write(true).open(&path)?;
        fs::remove_file(&path)?;
        Ok(())
    }

    #[test]
    fn identity_rejects_invalid_shape_range_and_empty_source() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE chunks(chunk_id INTEGER PRIMARY KEY, source_id TEXT NOT NULL);
             CREATE TABLE chunk_embeddings(chunk_id INTEGER PRIMARY KEY, embedding BLOB NOT NULL);",
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
        assert!(compute_identity(&conn, &primary, &"d".repeat(64)).is_err());
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
        assert!(compute_identity(&conn, &primary, &"d".repeat(64)).is_err());
        assert!(compute_identity(&conn, &secondary, &"d".repeat(64)).is_err());
        assert!(compute_identity(&conn, &primary, &"D".repeat(64)).is_err());
        Ok(())
    }

    #[test]
    fn exact_dot_kernels_match_scalar_and_ties_use_chunk_id() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        let has_avx2 = std::arch::is_x86_feature_detected!("avx2");
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        let has_vnni = has_avx2 && std::arch::is_x86_feature_detected!("avxvnni");
        for _ in 0..90_000 {
            let query = std::array::from_fn(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8 as i8
            });
            let document: [u8; EMBEDDING_DIM] = std::array::from_fn(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            });
            let scalar = dot_scalar(&query, &document);
            assert_eq!(PreparedDot::new(&query).score(&document), scalar);
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            if has_avx2 {
                // SAFETY: the branch is guarded by runtime AVX2 detection.
                let prepared = unsafe { prepare_avx2_query(&query) };
                // SAFETY: the branch is guarded by runtime AVX2 detection and
                // the document is exactly 256 bytes.
                let actual = unsafe { dot_avx2(document.as_ptr().cast(), &prepared) };
                assert_eq!(actual, scalar);
            }
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            if has_vnni {
                // SAFETY: the branch is guarded by AVX2/AVX-VNNI detection.
                let prepared = unsafe { prepare_vnni_query(&query) };
                // SAFETY: the branch is guarded by AVX2/AVX-VNNI detection
                // and the document is exactly 256 bytes.
                let actual = unsafe { dot_vnni(document.as_ptr().cast(), &prepared) };
                assert_eq!(actual, scalar);
            }
        }
        let mut top = TopK::new(3);
        for chunk_id in [9, 3, 5, 1] {
            top.push(AnnSearchHit { chunk_id, score: 7 });
        }
        assert_eq!(
            top.into_sorted()
                .into_iter()
                .map(|hit| hit.chunk_id)
                .collect::<Vec<_>>(),
            vec![1, 3, 5]
        );
    }

    fn source_embedding_at_offset(
        conn: &Connection,
        source_id: &SourceId,
        offset: usize,
    ) -> Result<[i8; EMBEDDING_DIM]> {
        let bytes: Vec<u8> = conn.query_row(
            "SELECT e.embedding
             FROM chunk_embeddings AS e
             INNER JOIN chunks AS c ON c.chunk_id = e.chunk_id
             WHERE c.source_id = ?1
             ORDER BY e.chunk_id ASC
             LIMIT 1 OFFSET ?2",
            params![source_id.as_str(), offset as i64],
            |row| row.get(0),
        )?;
        bytes
            .into_iter()
            .map(|byte| byte as i8)
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|_| anyhow!("benchmark query has wrong dimensions"))
    }

    fn sqlite_rerank(
        conn: &Connection,
        source_id: &SourceId,
        query: &[i8; EMBEDDING_DIM],
        candidates: &[AnnSearchHit],
    ) -> Result<Vec<AnnSearchHit>> {
        let mut statement = conn.prepare_cached(
            "SELECT e.embedding FROM chunk_embeddings e
             JOIN chunks c ON c.chunk_id = e.chunk_id
             WHERE c.source_id = ?1 AND e.chunk_id = ?2",
        )?;
        let mut hits = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let embedding: Vec<u8> = statement
                .query_row(params![source_id.as_str(), candidate.chunk_id], |row| {
                    row.get(0)
                })?;
            hits.push(AnnSearchHit {
                chunk_id: candidate.chunk_id,
                score: dot_scalar(query, &embedding),
            });
        }
        hits.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.chunk_id.cmp(&right.chunk_id))
        });
        Ok(hits)
    }

    fn installed_eligible_ids(
        conn: &Connection,
        source_id: &SourceId,
        default_filter: bool,
        sidecar: &FlatAnn,
    ) -> Result<RoaringBitmap> {
        if !default_filter {
            return Ok((0..sidecar.vector_count())
                .map(|row| sidecar.chunk_id(row))
                .collect());
        }
        let filter = crate::search::build_doc_filter(
            "d",
            crate::search::DocumentFilterSpec {
                source_id,
                types: None,
                date_from: None,
                date_to: None,
                doc_scope: None,
                include_old: false,
                current_only: true,
            },
        );
        let sql = format!(
            "SELECT e.chunk_id
             FROM chunk_embeddings e
             JOIN chunks c ON c.chunk_id = e.chunk_id
             JOIN documents d
               ON d.source_id = c.source_id AND d.native_id = c.native_id
             WHERE {}",
            filter.sql
        );
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(filter.params), |row| {
            row.get::<_, i64>(0)
        })?;
        let mut ids = RoaringBitmap::new();
        for row in rows {
            let chunk_id = row?;
            let chunk_id = u32::try_from(chunk_id)
                .map_err(|_| anyhow!("benchmark filter returned out-of-range chunk ID"))?;
            if !ids.insert(chunk_id) {
                bail!("benchmark filter returned duplicate chunk ID {chunk_id}");
            }
        }
        Ok(ids)
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
        let runs = std::env::var("LEGAL_MCP_BENCH_RUNS")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()?
            .unwrap_or(20);
        let top_k = std::env::var("LEGAL_MCP_BENCH_TOP_K")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()?
            .unwrap_or(50);
        let default_filter = std::env::var("LEGAL_MCP_BENCH_DEFAULT_FILTER")
            .ok()
            .is_some_and(|value| matches!(value.as_str(), "1" | "true"));
        if runs == 0 || top_k == 0 {
            bail!("LEGAL_MCP_BENCH_RUNS and LEGAL_MCP_BENCH_TOP_K must be positive");
        }
        let conn = Connection::open_with_flags(
            db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let identity = compute_identity(&conn, &source_id, &source_sha)?;
        let build_started = std::time::Instant::now();
        let output_root = Path::new(&output_root);
        let info = build_sidecar(&conn, &source_id, output_root, &source_sha, &identity)?;
        let build_elapsed = build_started.elapsed();
        let sidecar = Arc::new(open_verified_sidecar(
            &output_root.join(sidecar_relative_path(&source_id)),
            &source_id,
            &info,
        )?);
        let eligibility_started = std::time::Instant::now();
        let ids = installed_eligible_ids(&conn, &source_id, default_filter, &sidecar)?;
        let eligible = sidecar.eligible_rows(&ids)?;
        let eligibility_elapsed = eligibility_started.elapsed();
        let query = source_embedding_at_offset(&conn, &source_id, sidecar.vector_count() / 2)?;
        let expected = exact_rank(&conn, &source_id, &query, &ids, top_k)?;
        let warm = scan_sidecar(&sidecar, &query, &eligible, top_k)?;
        assert_eq!(warm, expected);

        let benchmark_filter = crate::search::build_doc_filter(
            "d",
            crate::search::DocumentFilterSpec {
                source_id: &source_id,
                types: None,
                date_from: None,
                date_to: None,
                doc_scope: None,
                include_old: !default_filter,
                current_only: default_filter,
            },
        );
        let warm_vector_search = crate::search::benchmark_cached_vector_search(
            &conn,
            &source_id,
            &sidecar,
            &info.sha256,
            &query,
            &benchmark_filter,
            top_k,
        )?;
        assert_eq!(
            warm_vector_search
                .iter()
                .map(|hit| u32::try_from(hit.chunk_id).expect("u32 sidecar ID"))
                .collect::<Vec<_>>(),
            expected.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>()
        );

        let mut scan_times_ms = Vec::with_capacity(runs);
        for _ in 0..runs {
            let started = std::time::Instant::now();
            let found = scan_sidecar(&sidecar, &query, &eligible, top_k)?;
            scan_times_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(found, expected);
        }
        let mut end_to_end_times_ms = Vec::with_capacity(runs);
        for _ in 0..runs {
            let started = std::time::Instant::now();
            let candidates = scan_sidecar(&sidecar, &query, &eligible, top_k)?;
            let reranked = sqlite_rerank(&conn, &source_id, &query, &candidates)?;
            end_to_end_times_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(reranked, expected);
        }
        let mut cached_vector_search_times_ms = Vec::with_capacity(runs);
        for _ in 0..runs {
            let started = std::time::Instant::now();
            let hits = crate::search::benchmark_cached_vector_search(
                &conn,
                &source_id,
                &sidecar,
                &info.sha256,
                &query,
                &benchmark_filter,
                top_k,
            )?;
            cached_vector_search_times_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(
                hits.iter()
                    .map(|hit| u32::try_from(hit.chunk_id).expect("u32 sidecar ID"))
                    .collect::<Vec<_>>(),
                expected.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>()
            );
        }
        scan_times_ms.sort_by(f64::total_cmp);
        end_to_end_times_ms.sort_by(f64::total_cmp);
        cached_vector_search_times_ms.sort_by(f64::total_cmp);
        let scan_average = scan_times_ms.iter().sum::<f64>() / runs as f64;
        let end_to_end_average = end_to_end_times_ms.iter().sum::<f64>() / runs as f64;
        let scan_median = scan_times_ms[runs / 2];
        let end_to_end_median = end_to_end_times_ms[runs / 2];
        let cached_vector_search_median = cached_vector_search_times_ms[runs / 2];
        eprintln!(
            "FLAT_ANN_BENCH source={} vectors={} eligible={} filter={} top_k={} size={} build_ms={} eligibility_ms={:.3} scan_median_ms={:.3} scan_avg_ms={:.3} scan_sqlite_rerank_median_ms={:.3} scan_sqlite_rerank_avg_ms={:.3} cached_vector_search_median_ms={:.3}",
            source_id,
            identity.vector_count,
            eligible.len(),
            if default_filter { "default" } else { "all" },
            top_k,
            info.size,
            build_elapsed.as_millis(),
            eligibility_elapsed.as_secs_f64() * 1000.0,
            scan_median,
            scan_average,
            end_to_end_median,
            end_to_end_average,
            cached_vector_search_median,
        );
        Ok(())
    }
}
