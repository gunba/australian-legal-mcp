//! ATO acquisition, corpus statistics, manifest validation, updates and
//! immutable generation promotion.

use crate::adaptive_http::{AdaptiveConcurrency, RequestOutcome, SOURCE_WORKER_CEILING};
use crate::config::{
    activate_generation, active_generation_key, data_dir, generation_dir, generations_dir,
    lifecycle_lock_file, lock_file, LEGAL_DB_FILENAME,
};
use crate::db::{get_corpus_meta, get_source_meta, open_read};
use crate::extract::anchors_node_text;
use crate::legal_source::{source_registry, SourceId};
use crate::search::ensure_vector_search_ready;
use crate::semantic::EMBEDDING_MODEL_FILES;
use crate::{ATO_USER_AGENT, EMBEDDING_MODEL_ID, SUPPORTED_SCHEMA_VERSION};
use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::{Client, Response};
use reqwest::redirect::Policy;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ATO_HTML_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 198 && matches!(octets[1], 18 | 19))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0))
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
                || ip.segments()[0] == 0x2001 && ip.segments()[1] == 0x0db8)
        }
    }
}

fn validate_ato_url(url: &url::Url) -> Result<(String, Vec<SocketAddr>)> {
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        bail!("ATO acquisition URL must be uncredentialed HTTPS: {url}");
    }
    if url.port().is_some_and(|port| port != 443) {
        bail!("ATO acquisition URL must use the default HTTPS port: {url}");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("ATO acquisition URL has no hostname: {url}"))?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host.parse::<IpAddr>().is_ok() || !(host == "ato.gov.au" || host.ends_with(".ato.gov.au")) {
        bail!("unapproved ATO acquisition hostname `{host}`");
    }
    let addresses = (host.as_str(), 443)
        .to_socket_addrs()
        .with_context(|| format!("resolving {host}"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
        bail!("hostname `{host}` resolved to a non-public network address");
    }
    Ok((host, addresses))
}

fn secure_get(mut url: url::Url, timeout: Duration) -> Result<Response> {
    for redirect in 0..=MAX_REDIRECTS {
        let (host, addresses) = validate_ato_url(&url)?;
        let client = Client::builder()
            .user_agent(ATO_USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .redirect(Policy::none())
            .resolve_to_addrs(&host, &addresses)
            .build()?;
        let response = client.get(url.clone()).send()?;
        if response.status().is_redirection() {
            if redirect == MAX_REDIRECTS {
                bail!("too many redirects fetching {url}");
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| anyhow!("redirect from {url} omitted Location"))?
                .to_str()
                .context("redirect Location was not valid text")?;
            url = url.join(location).context("resolving redirect Location")?;
            continue;
        }
        return response
            .error_for_status()
            .with_context(|| format!("fetching {url}"));
    }
    unreachable!()
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::config::atomic_write(path, bytes)
}

fn sha256_path(path: &Path) -> Result<String> {
    let mut input = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// ----- ATO What's New feed ingestion -----

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WhatsNewEntry {
    pub(crate) href: String,
    pub(crate) title: String,
    pub(crate) heading: Option<String>,
}

pub(crate) fn normalize_doc_href(href: &str) -> String {
    if href.is_empty() {
        return String::new();
    }
    // Try to parse as absolute URL; if relative, treat path/query directly.
    let parsed = url::Url::parse(href)
        .ok()
        .or_else(|| url::Url::parse(&format!("https://www.ato.gov.au{href}")).ok());
    let Some(parsed) = parsed else {
        return href.to_string();
    };
    let mut path = parsed.path().to_string();
    if !path.is_empty() && !path.starts_with('/') {
        path = format!("/{path}");
    }
    let mut docid: Option<String> = None;
    for (k, v) in parsed.query_pairs() {
        if k.eq_ignore_ascii_case("docid") {
            let raw = v.into_owned();
            let trimmed = raw
                .trim_matches(|c: char| c == '\'' || c == '"' || c == ' ')
                .to_string();
            if !trimmed.is_empty() {
                docid = Some(trimmed);
                break;
            }
        }
    }
    if let Some(id) = docid {
        return format!(
            "/law/view/document?docid={}",
            crate::html::canonical_ato_native_id(&id)
        );
    }
    if let Some(q) = parsed.query() {
        if !q.is_empty() {
            return format!("{path}?{q}");
        }
    }
    path
}

pub(crate) fn parse_whats_new(html: &str, base_url: &str) -> Result<Vec<WhatsNewEntry>> {
    use scraper::{Node as SNode, Selector};
    let doc = scraper::Html::parse_document(html);
    let article_sel = Selector::parse("article").unwrap();
    let Some(article) = doc.select(&article_sel).next() else {
        bail!("whatsnew article block not found");
    };
    const HEADING_TAGS: &[&str] = &["h1", "h2", "h3", "h4", "h5"];
    let mut entries: Vec<WhatsNewEntry> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_heading: Option<String> = None;
    // Walk the article subtree in DOM order. Track the latest heading we
    // encountered; emit an entry every time we hit a usable <a href>.
    for node in article.descendants() {
        if let Some(eref) = scraper::ElementRef::wrap(node) {
            let tag = eref.value().name();
            if HEADING_TAGS.contains(&tag) {
                let t = anchors_node_text(eref);
                last_heading = if t.is_empty() { None } else { Some(t) };
                continue;
            }
            if tag == "a" {
                let raw_href = match eref.value().attr("href") {
                    Some(h) => h,
                    None => continue,
                };
                let absolute =
                    if raw_href.starts_with("http://") || raw_href.starts_with("https://") {
                        raw_href.to_string()
                    } else if raw_href.starts_with('/') {
                        format!("{}{}", base_url.trim_end_matches('/'), raw_href)
                    } else {
                        format!("{}/{}", base_url.trim_end_matches('/'), raw_href)
                    };
                let canonical = normalize_doc_href(&absolute);
                if !canonical.starts_with("/law/view/document") {
                    continue;
                }
                if seen.contains(&canonical) {
                    continue;
                }
                seen.insert(canonical.clone());
                let title = anchors_node_text(eref);
                let title = if title.is_empty() {
                    canonical.clone()
                } else {
                    title
                };
                entries.push(WhatsNewEntry {
                    href: canonical,
                    title,
                    heading: last_heading.clone(),
                });
            }
        } else if let SNode::Text(_) = node.value() {
            // Text nodes don't affect heading state.
        }
    }
    Ok(entries)
}

pub(crate) fn stats() -> Result<String> {
    let conn = open_read()?;
    let active_manifest = load_active_generation_manifest()?;
    let docs = read_corpus_count_meta(&conn, "documents_count")?
        .unwrap_or(count_table(&conn, "documents")?);
    let chunks =
        read_corpus_count_meta(&conn, "chunks_count")?.unwrap_or(count_table(&conn, "chunks")?);
    let embeddings = read_corpus_count_meta(&conn, "chunk_embeddings_count")?
        .unwrap_or(count_table(&conn, "chunk_embeddings")?);
    let definitions = read_corpus_count_meta(&conn, "definitions_count")?
        .unwrap_or(count_table(&conn, "definitions")?);

    let registry = source_registry();
    let registered_sources = registered_source_ids();
    let database_sources = database_source_ids(&conn)?;
    if database_sources != registered_sources {
        bail!(
            "installed database source set does not match this binary: registered={registered_sources:?}, database={database_sources:?}"
        );
    }
    let descriptors = registry.descriptors();
    let mut source_stats = BTreeMap::new();
    let mut semantic_search_ready = true;
    for descriptor in &descriptors {
        let source_id = descriptor.id.as_str();
        let source_docs = read_source_count_meta(&conn, source_id, "documents_count")?
            .unwrap_or(count_source_table(&conn, source_id, "documents")?);
        let source_chunks = read_source_count_meta(&conn, source_id, "chunks_count")?
            .unwrap_or(count_source_table(&conn, source_id, "chunks")?);
        let source_embeddings = read_source_count_meta(&conn, source_id, "chunk_embeddings_count")?
            .unwrap_or(count_source_table(&conn, source_id, "chunk_embeddings")?);
        let source_definitions = read_source_count_meta(&conn, source_id, "definitions_count")?
            .unwrap_or(count_source_table(&conn, source_id, "definitions")?);
        let types = match get_source_meta(&conn, source_id, "documents_by_type_json")? {
            Some(cached) => {
                serde_json::from_str::<BTreeMap<String, i64>>(&cached).with_context(|| {
                    format!("parsing cached document types for source `{source_id}`")
                })?
            }
            None => compute_documents_by_type(&conn, source_id)?,
        };
        let prefix_breakdown = match get_source_meta(&conn, source_id, "prefix_breakdown_json")? {
            Some(cached) => serde_json::from_str::<Vec<JsonValue>>(&cached).with_context(|| {
                format!("parsing cached prefix breakdown for source `{source_id}`")
            })?,
            None => collect_prefix_breakdown(&conn, source_id)?,
        };
        let source_semantic_search_ready =
            ensure_vector_search_ready(&conn, &descriptor.id).is_ok();
        semantic_search_ready &= source_semantic_search_ready;
        source_stats.insert(
            source_id.to_string(),
            json!({
                "source_id": source_id,
                "display_name": descriptor.display_name,
                "corpus_id": get_source_meta(&conn, source_id, "corpus_id")?,
                "semantic_search_ready": source_semantic_search_ready,
                "documents": source_docs,
                "chunks": source_chunks,
                "chunk_embeddings": source_embeddings,
                "definitions": source_definitions,
                "ann": active_manifest
                    .as_ref()
                    .and_then(|manifest| manifest.ann.get(&descriptor.id)),
                "types": types,
                "prefix_breakdown": prefix_breakdown,
            }),
        );
    }
    let payload = json!({
        "data_dir": data_dir()?.display().to_string(),
        "active_generation": active_generation_key()?,
        "index_version": get_corpus_meta(&conn, "index_version")?,
        "last_update_at": get_corpus_meta(&conn, "last_update_at")?,
        "sources": descriptors,
        "source_stats": source_stats,
        "embedding_model_id": get_corpus_meta(&conn, "embedding_model_id")?,
        "semantic_search_ready": semantic_search_ready,
        "search_modes": ["hybrid", "vector", "keyword"],
        "default_search_mode": "hybrid",
        "documents": docs,
        "chunks": chunks,
        "chunk_embeddings": embeddings,
        "definitions": definitions,
    });
    // JSON outputs use serde_json pretty rendering before return/write.
    Ok(serde_json::to_string_pretty(&payload)?)
}

pub(crate) fn verify_active_generation(prewarm: impl FnOnce() -> Result<()>) -> Result<String> {
    let corpus_lock = crate::config::corpus_read_lock()?;
    let result = (|| {
        let active =
            active_generation_key()?.ok_or_else(|| anyhow!("no immutable generation is active"))?;
        let target = generation_dir(&active)?;
        let (_, validated_key) = validate_installed_generation_dir(&target)
            .context("validating every immutable generation artifact")?;
        if validated_key.as_str() != active {
            bail!("active generation directory name does not match its immutable content");
        }
        ensure_generation_read_only(&target)?;
        let report = stats()?;
        let value: JsonValue = serde_json::from_str(&report)?;
        if value
            .get("semantic_search_ready")
            .and_then(JsonValue::as_bool)
            != Some(true)
        {
            let conn = open_read()?;
            for descriptor in source_registry().descriptors() {
                ensure_vector_search_ready(&conn, &descriptor.id).with_context(|| {
                    format!(
                        "active generation is not ready for semantic search in source `{}`",
                        descriptor.id
                    )
                })?;
            }
            bail!("active generation is not ready for semantic search across every source");
        }
        if value.get("active_generation").and_then(JsonValue::as_str) != Some(active.as_str()) {
            bail!("active generation changed during strict verification");
        }
        prewarm().context("loading and executing the verified semantic model")?;
        Ok(report)
    })();
    fs2::FileExt::unlock(&corpus_lock)?;
    result
}

pub(crate) fn verify_active_generation_quick() -> Result<String> {
    let corpus_lock = crate::config::corpus_read_lock()?;
    let result = (|| {
        let active =
            active_generation_key()?.ok_or_else(|| anyhow!("no immutable generation is active"))?;
        ensure_generation_read_only(&generation_dir(&active)?)?;
        let report = stats()?;
        let value: JsonValue = serde_json::from_str(&report)?;
        if value
            .get("semantic_search_ready")
            .and_then(JsonValue::as_bool)
            != Some(true)
        {
            bail!("active generation is not ready for semantic search across every source");
        }
        if value.get("active_generation").and_then(JsonValue::as_str) != Some(active.as_str()) {
            bail!("active generation changed during startup verification");
        }
        Ok(report)
    })();
    fs2::FileExt::unlock(&corpus_lock)?;
    result
}

fn parse_count_meta(value: Option<String>, label: &str) -> Result<Option<i64>> {
    value
        .map(|raw| -> Result<i64> {
            let count = raw
                .parse::<i64>()
                .with_context(|| format!("parsing {label} count `{raw}`"))?;
            if count < 0 {
                bail!("{label} count must not be negative");
            }
            Ok(count)
        })
        .transpose()
}

fn read_corpus_count_meta(conn: &Connection, key: &str) -> Result<Option<i64>> {
    parse_count_meta(get_corpus_meta(conn, key)?, key)
}

fn read_source_count_meta(conn: &Connection, source_id: &str, key: &str) -> Result<Option<i64>> {
    parse_count_meta(
        get_source_meta(conn, source_id, key)?,
        &format!("source `{source_id}` {key}"),
    )
}

fn count_table(conn: &Connection, table: &str) -> Result<i64> {
    // Caller passes a compile-time string literal; no user input reaches here.
    let sql = format!("SELECT COUNT(*) FROM {table}");
    conn.query_row(&sql, [], |r| r.get(0))
        .with_context(|| format!("counting rows in {table}"))
}

pub(crate) fn compute_documents_by_type(
    conn: &Connection,
    source_id: &str,
) -> Result<BTreeMap<String, i64>> {
    let mut types = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT type, COUNT(*) AS n
         FROM documents
         WHERE source_id = ?1
         GROUP BY type
         ORDER BY n DESC, type ASC",
    )?;
    let rows = stmt.query_map([source_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (typ, n) = row?;
        types.insert(typ, n);
    }
    Ok(types)
}

fn count_source_table(conn: &Connection, source_id: &str, table: &str) -> Result<i64> {
    if table == "chunk_embeddings" {
        return conn
            .query_row(
                "SELECT COUNT(*)
                 FROM chunk_embeddings AS embedding
                 INNER JOIN chunks AS chunk ON chunk.chunk_id = embedding.chunk_id
                 WHERE chunk.source_id = ?1",
                [source_id],
                |row| row.get(0),
            )
            .map_err(Into::into);
    }
    let sql = match table {
        "documents" => "SELECT COUNT(*) FROM documents WHERE source_id = ?1",
        "chunks" => "SELECT COUNT(*) FROM chunks WHERE source_id = ?1",
        "definitions" => "SELECT COUNT(*) FROM definitions WHERE source_id = ?1",
        _ => bail!("unsupported source-count table `{table}`"),
    };
    conn.query_row(sql, [source_id], |row| row.get(0))
        .map_err(Into::into)
}

/// Per-prefix corpus breakdown — doc_id-prefix counts plus a sample-title
/// description. Replaces the hand-maintained prefix-to-doc-type yaml: the only
/// signal we trust is the corpus itself.
///
/// The description is the leading segment of the first sample title (the part
/// before ` — ` when present, otherwise the full title), since titles for many
/// ATO doc types don't carry a doc-type label at all (cases, sections, etc.).
pub(crate) fn collect_prefix_breakdown(
    conn: &rusqlite::Connection,
    source_id: &str,
) -> Result<Vec<JsonValue>> {
    // Single-pass window function: partition by docid prefix, compute count
    // + pick one representative title per prefix. Replaces N+1 selects that
    // each ran an UPPER(title) LIKE sort over thousands of rows — that
    // pattern stalled MCP `initialize` on large corpora.
    //
    // Title preference: when a prefix has at least one title that doesn't
    // start with the docid form ("EXM ADEBB74A"), prefer the composed one
    // ("Explanatory Memorandum — …"). Title scan is case-sensitive — ATO
    // docid-form titles are always uppercase, so dropping UPPER() saves a
    // per-row case fold without changing results.
    let mut stmt = conn.prepare(
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
            COUNT(*) OVER (PARTITION BY prefix) AS doc_count,
            ROW_NUMBER() OVER (
              PARTITION BY prefix
              ORDER BY
                CASE WHEN title LIKE prefix || ' %' THEN 1 ELSE 0 END,
                native_id
            ) AS rn
          FROM ranked
        )
        SELECT prefix, doc_count, title
        FROM windowed
        WHERE rn = 1
        ORDER BY doc_count DESC, prefix ASC
        "#,
    )?;
    let rows = stmt.query_map([source_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut entries: Vec<JsonValue> = Vec::new();
    for row in rows {
        let (prefix, count, title) = row?;
        let description = title.map(|t| description_from_title(&t));
        entries.push(json!({
            "prefix": prefix,
            "doc_count": count,
            "description": description,
        }));
    }
    Ok(entries)
}

fn registered_source_ids() -> BTreeSet<SourceId> {
    source_registry()
        .descriptors()
        .into_iter()
        .map(|descriptor| descriptor.id)
        .collect()
}

fn database_source_ids(conn: &Connection) -> Result<BTreeSet<SourceId>> {
    let mut stmt = conn.prepare("SELECT source_id FROM sources ORDER BY source_id")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut source_ids = BTreeSet::new();
    for row in rows {
        let raw = row?;
        let source_id = raw
            .parse::<SourceId>()
            .with_context(|| format!("database contains invalid source id `{raw}`"))?;
        source_ids.insert(source_id);
    }
    Ok(source_ids)
}

/// Take the part before the first ` — ` em-dash separator if present, else the
/// full title. ATO ruling titles use that separator to delimit the citation;
/// for other doc types the title is already the cleanest description we have.
pub(crate) fn description_from_title(title: &str) -> String {
    const SEP: &str = " \u{2014} ";
    match title.find(SEP) {
        Some(idx) => title[..idx].trim().to_string(),
        None => title.trim().to_string(),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub(crate) schema_version: u32,
    pub(crate) index_version: String,
    pub(crate) created_at: String,
    pub(crate) min_client_version: String,
    pub(crate) model: ModelInfo,
    pub(crate) db: ManifestDb,
    pub(crate) ann: BTreeMap<SourceId, crate::ann::ManifestAnn>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestDb {
    pub(crate) path: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelInfo {
    pub(crate) id: String,
    pub(crate) fingerprint: String,
    pub(crate) model: ManifestFile,
    pub(crate) tokenizer: ManifestFile,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestFile {
    pub(crate) path: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct GenerationId(String);

impl GenerationId {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if !is_lower_sha256(&value) {
            bail!("generation ID must be exactly 64 lowercase hexadecimal characters");
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GenerationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::str::FromStr for GenerationId {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(value.to_string()).map_err(|error| error.to_string())
    }
}

pub(crate) fn validate_manifest(manifest: &Manifest) -> Result<()> {
    if manifest.schema_version != SUPPORTED_SCHEMA_VERSION {
        bail!(
            "manifest schema_version {} is not supported by this binary (expects {SUPPORTED_SCHEMA_VERSION}); install a matching australian-legal-mcp release",
            manifest.schema_version
        );
    }
    validate_manifest_contents(manifest)
}

pub(crate) fn validate_manifest_contents(manifest: &Manifest) -> Result<()> {
    if manifest.index_version.trim() != manifest.index_version
        || manifest.index_version.is_empty()
        || manifest.index_version.chars().any(char::is_control)
    {
        bail!("manifest index_version is malformed");
    }
    chrono::DateTime::parse_from_rfc3339(&manifest.created_at)
        .context("manifest created_at must be RFC 3339")?;
    let min = parse_release_version(&manifest.min_client_version, "manifest min_client_version")?;
    let current = parse_release_version(env!("CARGO_PKG_VERSION"), "binary version")?;
    if min > current {
        bail!(
            "manifest requires australian-legal-mcp >= {}, but this binary is {}; please upgrade the legal-mcp binary",
            manifest.min_client_version,
            env!("CARGO_PKG_VERSION")
        );
    }
    if manifest.model.id != EMBEDDING_MODEL_ID {
        bail!(
            "manifest model `{}` does not match required model `{EMBEDDING_MODEL_ID}`",
            manifest.model.id
        );
    }
    if manifest.model.fingerprint != crate::EMBEDDING_MODEL_FINGERPRINT {
        bail!("manifest model fingerprint does not match the pinned model");
    }
    validate_manifest_file(
        &manifest.model.model,
        &EMBEDDING_MODEL_FILES[0],
        "model graph",
    )?;
    validate_manifest_file(
        &manifest.model.tokenizer,
        &EMBEDDING_MODEL_FILES[1],
        "model tokenizer",
    )?;
    if manifest.db.path != LEGAL_DB_FILENAME {
        bail!("manifest database path must be `{LEGAL_DB_FILENAME}`");
    }
    if manifest.db.size == 0 || !is_lower_sha256(&manifest.db.sha256) {
        bail!("manifest database metadata is malformed");
    }
    let expected_sources = registered_source_ids();
    let manifest_sources = manifest.ann.keys().cloned().collect::<BTreeSet<_>>();
    if manifest_sources != expected_sources {
        bail!(
            "manifest source set does not match this binary: registered={expected_sources:?}, manifest={manifest_sources:?}"
        );
    }
    for (source_id, ann) in &manifest.ann {
        crate::ann::validate_manifest_ann(source_id, ann)?;
        if ann.embedding_model_id != manifest.model.id {
            bail!("ANN sidecar model for source `{source_id}` does not match manifest model");
        }
    }
    Ok(())
}

fn validate_manifest_file(
    info: &ManifestFile,
    expected: &crate::semantic::EmbeddingModelFile,
    label: &str,
) -> Result<()> {
    if info.path != expected.output_name
        || info.size != expected.size
        || info.sha256 != expected.sha256
    {
        bail!("manifest {label} metadata does not match the pinned model");
    }
    Ok(())
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn parse_release_version(value: &str, label: &str) -> Result<Vec<u32>> {
    if value.is_empty() || value.trim() != value {
        bail!("{label} is malformed");
    }
    let (core, suffix) = value
        .split_once('-')
        .map_or((value, None), |(core, suffix)| (core, Some(suffix)));
    if suffix.is_some_and(|suffix| {
        suffix.is_empty()
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    }) {
        bail!("{label} is malformed");
    }
    let fields = core.split('.').collect::<Vec<_>>();
    if fields.len() != 3 {
        bail!("{label} must contain major.minor.patch");
    }
    fields
        .into_iter()
        .map(|field| {
            if field.is_empty()
                || !field.bytes().all(|byte| byte.is_ascii_digit())
                || (field.len() > 1 && field.starts_with('0'))
            {
                bail!("{label} is malformed");
            }
            field
                .parse::<u32>()
                .with_context(|| format!("{label} component is too large"))
        })
        .collect()
}

/// Compare two dotted version strings (`a.b.c[-suffix]`) by their numeric
/// components only. Returns `Ordering::Less/Equal/Greater` for the first
/// arg relative to the second. Pre-release suffixes are ignored.
#[cfg(test)]
pub(crate) fn cmp_dotted_version(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(v: &str) -> Vec<u32> {
        let core = v.split('-').next().unwrap_or("");
        let mut out: Vec<u32> = core
            .split('.')
            .map(|s| s.parse::<u32>().unwrap_or(0))
            .collect();
        // Pad to length 3 so 1.2 == 1.2.0.
        while out.len() < 3 {
            out.push(0);
        }
        out
    }
    let pa = parts(a);
    let pb = parts(b);
    pa.cmp(&pb)
}

#[derive(Debug, Serialize)]
pub(crate) struct ActivationReport {
    pub(crate) active_generation: String,
    pub(crate) previous_generation: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DeactivationReport {
    pub(crate) deactivated_generation: String,
}

fn set_generation_read_only(root: &Path) -> Result<()> {
    fn visit(path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            bail!(
                "immutable generation cannot contain symlinks: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                visit(&entry?.path())?;
            }
        }
        let mut permissions = metadata.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)?;
        Ok(())
    }

    visit(root).with_context(|| {
        format!(
            "setting immutable generation permissions on {}",
            root.display()
        )
    })
}

fn set_generation_owner_writable(root: &Path) -> Result<()> {
    fn visit(path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            bail!(
                "immutable generation cannot contain symlinks: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                visit(&entry?.path())?;
            }
        }
        let mut permissions = metadata.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(permissions.mode() | 0o200);
        }
        #[cfg(not(unix))]
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
        Ok(())
    }

    visit(root).with_context(|| {
        format!(
            "temporarily enabling immutable-generation removal at {}",
            root.display()
        )
    })
}

#[cfg(unix)]
fn ensure_generation_read_only(root: &Path) -> Result<()> {
    fn visit(path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.permissions().readonly() {
            bail!(
                "installed generation path is not immutable: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                visit(&entry?.path())?;
            }
        }
        Ok(())
    }
    visit(root)
}

#[cfg(not(unix))]
fn ensure_generation_read_only(_root: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn rename_across_directories(source: &Path, destination: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    if unsafe { libc::rename(source.as_ptr(), destination.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(not(unix))]
fn rename_across_directories(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    Ok(())
}

pub(crate) fn generation_key(manifest: &Manifest) -> Result<GenerationId> {
    let mut hasher = Sha256::new();
    hasher.update(b"australian-legal-mcp-local-generation-v1\0");
    hasher.update(serde_json::to_vec(manifest)?);
    GenerationId::parse(format!("{:x}", hasher.finalize()))
}

#[cfg(unix)]
fn reject_hard_link(metadata: &fs::Metadata, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() != 1 {
        bail!(
            "generation files must not be hard-linked: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_hard_link(_metadata: &fs::Metadata, _path: &Path) -> Result<()> {
    Ok(())
}

fn verified_regular_file(
    root: &Path,
    relative: &str,
    expected_size: u64,
    expected_sha: &str,
) -> Result<PathBuf> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("reading generation file {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "generation file must be a regular non-symlink: {}",
            path.display()
        );
    }
    reject_hard_link(&metadata, &path)?;
    if metadata.len() != expected_size {
        bail!("generation file size mismatch for {}", path.display());
    }
    let actual = sha256_path(&path)?;
    if actual != expected_sha {
        bail!("generation file SHA-256 mismatch for {}", path.display());
    }
    Ok(path)
}

pub(crate) fn read_generation_manifest(root: &Path) -> Result<Manifest> {
    let path = root.join(crate::config::GENERATION_MANIFEST_FILENAME);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("reading generation manifest {}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_MANIFEST_BYTES
    {
        bail!("generation manifest must be a bounded regular non-symlink file");
    }
    reject_hard_link(&metadata, &path)?;
    let manifest: Manifest = serde_json::from_slice(&fs::read(&path)?)
        .with_context(|| format!("parsing generation manifest {}", path.display()))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

pub(crate) fn validate_generation_dir(root: &Path) -> Result<(Manifest, GenerationId)> {
    validate_generation_dir_with_mode(root, true)
}

fn validate_installed_generation_dir(root: &Path) -> Result<(Manifest, GenerationId)> {
    validate_generation_dir_with_mode(root, false)
}

fn validate_generation_dir_with_mode(
    root: &Path,
    full_sqlite_integrity: bool,
) -> Result<(Manifest, GenerationId)> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("reading generation directory {}", root.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("generation path must be a real directory");
    }
    let expected_top = BTreeSet::from([
        crate::ann::ANN_DIRECTORY.to_string(),
        crate::config::GENERATION_MANIFEST_FILENAME.to_string(),
        LEGAL_DB_FILENAME.to_string(),
        manifest_model_path().to_string(),
        manifest_tokenizer_path().to_string(),
    ]);
    let mut actual_top = BTreeSet::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("generation contains a non-Unicode path"))?;
        actual_top.insert(name);
    }
    if actual_top != expected_top {
        bail!("generation contents differ: expected {expected_top:?}, found {actual_top:?}");
    }

    let manifest = read_generation_manifest(root)?;
    let db_path = verified_regular_file(
        root,
        &manifest.db.path,
        manifest.db.size,
        &manifest.db.sha256,
    )?;
    verified_regular_file(
        root,
        &manifest.model.model.path,
        manifest.model.model.size,
        &manifest.model.model.sha256,
    )?;
    verified_regular_file(
        root,
        &manifest.model.tokenizer.path,
        manifest.model.tokenizer.size,
        &manifest.model.tokenizer.sha256,
    )?;

    let ann_dir = root.join(crate::ann::ANN_DIRECTORY);
    let ann_metadata = fs::symlink_metadata(&ann_dir)?;
    if ann_metadata.file_type().is_symlink() || !ann_metadata.is_dir() {
        bail!("generation ANN path must be a real directory");
    }
    let expected_ann = manifest
        .ann
        .values()
        .map(|info| Path::new(&info.path).file_name().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    let actual_ann = fs::read_dir(&ann_dir)?
        .map(|entry| Ok(entry?.file_name()))
        .collect::<Result<BTreeSet<_>>>()?;
    if actual_ann != expected_ann {
        bail!("generation ANN file set differs from its manifest");
    }
    for (source_id, info) in &manifest.ann {
        let path = root.join(&info.path);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("reading generation ANN file {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "generation ANN file must be a regular non-symlink: {}",
                path.display()
            );
        }
        reject_hard_link(&metadata, &path)?;
        crate::ann::verify_sidecar(&path, source_id, info)?;
    }

    // FTS5's PRAGMA-integrity callback is rejected by a read-only SQLite
    // connection even though a successful check does not alter the database.
    // Full validation applies only to a writable staging generation. Installed
    // generations are immutable and can be revalidated from their exact hashes
    // plus the read-only relational/binding checks below.
    let access = if full_sqlite_integrity {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let conn = Connection::open_with_flags(&db_path, access | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(Duration::from_secs(30))?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.execute_batch("PRAGMA query_only=ON; PRAGMA cell_size_check=ON;")?;
    crate::db::enforce_db_schema_version(&conn)?;
    crate::db::validate_chunks_fts_schema(&conn)?;
    crate::db::verify_chunks_fts_index_digest(&conn)?;
    verify_corpus_manifest_binding(&conn, &manifest)?;
    verify_semantic_install(&conn, &manifest)?;
    if full_sqlite_integrity {
        // Bundled SQLite asks FTS5 virtual tables to perform a write-shaped
        // callback during a global integrity_check. Run SQLite's native check
        // over every ordinary/shadow-free table, then use FTS5's documented
        // integrity command explicitly in a rolled-back transaction.
        let tables = {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type = 'table'
                   AND name NOT LIKE 'chunks_fts%'
                   AND name NOT LIKE 'title_fts%'
                 ORDER BY name",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        if tables.is_empty() {
            bail!("generation database has no ordinary tables to validate");
        }
        for table in tables {
            let escaped = table.replace('\'', "''");
            let sql = format!("PRAGMA integrity_check('{escaped}')");
            let integrity = {
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };
            if integrity.as_slice() != ["ok"] {
                bail!(
                    "generation database table `{table}` failed SQLite integrity_check: {integrity:?}"
                );
            }
        }
        conn.execute_batch("PRAGMA query_only=OFF")?;
        let fts_check = conn.unchecked_transaction()?;
        fts_check.execute(
            "INSERT INTO title_fts(title_fts) VALUES('integrity-check')",
            [],
        )?;
        fts_check.execute(
            "INSERT INTO chunks_fts(chunks_fts) VALUES('integrity-check')",
            [],
        )?;
        fts_check.rollback()?;
    }
    let foreign_keys: i64 =
        conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_keys != 0 {
        bail!("generation database has {foreign_keys} foreign-key violations");
    }
    crate::db::verify_fts_relational_bindings(&conn)?;
    drop(conn);
    if full_sqlite_integrity {
        verified_regular_file(
            root,
            &manifest.db.path,
            manifest.db.size,
            &manifest.db.sha256,
        )
        .context("generation database changed during integrity validation")?;
    }
    let key = generation_key(&manifest)?;
    Ok((manifest, key))
}

fn manifest_model_path() -> &'static str {
    EMBEDDING_MODEL_FILES[0].output_name
}

fn manifest_tokenizer_path() -> &'static str {
    EMBEDDING_MODEL_FILES[1].output_name
}

pub(crate) fn activate_local_generation(
    source: &Path,
    expected_generation: Option<&str>,
) -> Result<ActivationReport> {
    let lifecycle_lock = lifecycle_lock_file()?;
    let result = activate_local_generation_locked(source, expected_generation);
    fs2::FileExt::unlock(&lifecycle_lock)?;
    result
}

fn activate_local_generation_locked(
    source: &Path,
    expected_generation: Option<&str>,
) -> Result<ActivationReport> {
    let supplied_metadata = fs::symlink_metadata(source)
        .with_context(|| format!("reading generation path {}", source.display()))?;
    if supplied_metadata.file_type().is_symlink() || !supplied_metadata.is_dir() {
        bail!("generation path must be a real non-symlink directory");
    }
    let source = source
        .canonicalize()
        .with_context(|| format!("canonicalizing generation {}", source.display()))?;
    let (_, key) = validate_generation_dir(&source)?;
    if let Some(expected) = expected_generation {
        if expected != key.as_str() {
            bail!(
                "validated generation key {key} differs from required deployment generation {expected}"
            );
        }
    }
    validate_installed_generation_dir(&source)
        .and_then(|(_, sealed_key)| {
            if sealed_key != key {
                bail!("generation changed while it was being sealed");
            }
            Ok(())
        })
        .context("revalidating generation immediately before sealing")?;
    commit_validated_generation(&source, key.as_str())
}

pub(crate) fn deactivate_generation(expected_generation: &str) -> Result<DeactivationReport> {
    let lifecycle_lock = lifecycle_lock_file()?;
    let corpus_lock = lock_file()?;
    let result = (|| {
        let active = active_generation_key()?
            .ok_or_else(|| anyhow!("cannot deactivate: no generation is active"))?;
        if active != expected_generation {
            bail!("refusing to deactivate generation {active}; expected {expected_generation}");
        }
        let pointer = crate::config::lifecycle_dir()?.join("active-generation");
        let metadata = fs::symlink_metadata(&pointer)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("active generation pointer must be a regular non-symlink file");
        }
        reject_hard_link(&metadata, &pointer)?;
        fs::remove_file(&pointer)?;
        sync_directory(&crate::config::lifecycle_dir()?)?;
        Ok(DeactivationReport {
            deactivated_generation: active,
        })
    })();
    fs2::FileExt::unlock(&corpus_lock)?;
    fs2::FileExt::unlock(&lifecycle_lock)?;
    result
}

fn commit_validated_generation(source: &Path, key: &str) -> Result<ActivationReport> {
    let corpus_lock = lock_file()?;
    let result = commit_validated_generation_locked(source, key);
    fs2::FileExt::unlock(&corpus_lock)?;
    result
}

fn commit_validated_generation_locked(source: &Path, key: &str) -> Result<ActivationReport> {
    let previous = active_generation_key()?;
    let final_dir = generation_dir(key)?;
    if final_dir.exists() {
        if previous.as_deref() == Some(key) {
            return Ok(ActivationReport {
                active_generation: key.to_string(),
                previous_generation: None,
            });
        }
        bail!(
            "immutable generation collision at {}; refusing to overwrite it",
            final_dir.display()
        );
    }
    let source_parent = source
        .parent()
        .ok_or_else(|| anyhow!("generation has no parent directory"))?
        .to_path_buf();
    let generation_root = generations_dir()?;
    rename_across_directories(source, &final_dir).with_context(|| {
        format!(
            "committing immutable generation; build and runtime must share one filesystem: {} -> {}",
            source.display(),
            final_dir.display()
        )
    })?;
    let durability = sync_directory(&source_parent).and_then(|()| sync_directory(&generation_root));
    set_generation_read_only(&final_dir)?;
    sync_directory(&final_dir)?;
    durability.with_context(|| {
        format!(
            "generation was sealed safely at {} but its directory rename was not confirmed durable; recover with `legal-mcp rollback --generation {key}`",
            final_dir.display()
        )
    })?;
    if let Err(error) = activate_generation(key) {
        if active_generation_key()?.as_deref() == Some(key) {
            sync_directory(&crate::config::lifecycle_dir()?)?;
            return Ok(ActivationReport {
                active_generation: key.to_string(),
                previous_generation: previous,
            });
        }
        return Err(error).with_context(|| {
            format!(
                "generation was installed safely at {} but its active pointer was not changed; retry with `legal-mcp rollback --generation {key}`",
                final_dir.display()
            )
        });
    }
    Ok(ActivationReport {
        active_generation: key.to_string(),
        previous_generation: previous,
    })
}

pub(crate) fn rollback_generation(key: &str) -> Result<ActivationReport> {
    let lifecycle_lock = lifecycle_lock_file()?;
    let result = (|| {
        let previous = active_generation_key()?;
        let target = generation_dir(key)?;
        let (_, actual_key) = validate_installed_generation_dir(&target)?;
        if actual_key.as_str() != key {
            bail!("generation directory name does not match its immutable content");
        }
        set_generation_read_only(&target)?;
        ensure_generation_read_only(&target)?;
        let corpus_lock = lock_file()?;
        activate_generation(key)?;
        fs2::FileExt::unlock(&corpus_lock)?;
        Ok(ActivationReport {
            active_generation: key.to_string(),
            previous_generation: previous,
        })
    })();
    fs2::FileExt::unlock(&lifecycle_lock)?;
    result
}

pub(crate) fn prune_inactive_generations(keep: usize) -> Result<Vec<String>> {
    let lifecycle_lock = lifecycle_lock_file()?;
    let corpus_lock = lock_file()?;
    let result = (|| {
        let active = active_generation_key()?
            .ok_or_else(|| anyhow!("cannot prune generations without an active generation"))?;
        let active_path = generation_dir(&active)?;
        let (_, validated_key) = validate_installed_generation_dir(&active_path)
            .context("validating active generation before pruning rollback state")?;
        if validated_key.as_str() != active {
            bail!("active generation directory name does not match its immutable content");
        }
        ensure_generation_read_only(&active_path)?;
        let mut inactive = Vec::new();
        for entry in fs::read_dir(generations_dir()?)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == active || name.starts_with('.') || !entry.file_type()?.is_dir() {
                continue;
            }
            if name.len() != 64
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                bail!(
                    "unexpected directory in generations: {}",
                    entry.path().display()
                );
            }
            let modified = entry.metadata()?.modified()?;
            inactive.push((modified, name.to_string(), entry.path()));
        }
        inactive.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        let mut removed = Vec::new();
        for (_, name, path) in inactive.into_iter().skip(keep) {
            set_generation_owner_writable(&path)?;
            fs::remove_dir_all(&path)
                .with_context(|| format!("removing inactive generation {}", path.display()))?;
            removed.push(name);
        }
        sync_directory(&generations_dir()?)?;
        Ok(removed)
    })();
    fs2::FileExt::unlock(&corpus_lock)?;
    fs2::FileExt::unlock(&lifecycle_lock)?;
    result
}

fn required_corpus_meta(conn: &Connection, key: &str) -> Result<String> {
    get_corpus_meta(conn, key)?
        .ok_or_else(|| anyhow!("staged database is missing corpus_meta.{key}"))
}

fn required_source_meta(conn: &Connection, source_id: &SourceId, key: &str) -> Result<String> {
    get_source_meta(conn, source_id.as_str(), key)?
        .ok_or_else(|| anyhow!("staged database is missing source_meta[`{source_id}`].{key}"))
}

pub(crate) fn verify_corpus_manifest_binding(conn: &Connection, manifest: &Manifest) -> Result<()> {
    let expected = [
        ("schema_version", manifest.schema_version.to_string()),
        ("index_version", manifest.index_version.clone()),
        ("embedding_model_id", manifest.model.id.clone()),
        ("last_update_at", manifest.created_at.clone()),
    ];
    for (key, expected_value) in expected {
        let actual = required_corpus_meta(conn, key)?;
        if actual != expected_value {
            bail!(
                "staged database corpus_meta.{key} does not match manifest: expected `{expected_value}`, got `{actual}`"
            );
        }
    }
    Ok(())
}

pub(crate) fn verify_ann_db_binding(
    conn: &Connection,
    source_id: &SourceId,
    info: &crate::ann::ManifestAnn,
) -> Result<()> {
    let corpus_id = required_source_meta(conn, source_id, "corpus_id")?;
    let embedding_set_sha256 = required_source_meta(conn, source_id, "embedding_set_sha256")?;
    if corpus_id != info.corpus_id
        || embedding_set_sha256 != info.embedding_set_sha256
        || u64::try_from(chunk_embedding_count(conn, source_id)?).ok() != Some(info.vector_count)
    {
        bail!("ANN sidecar metadata for source `{source_id}` does not match the staged database");
    }
    Ok(())
}

fn verify_ann_db_content(
    conn: &Connection,
    source_id: &SourceId,
    info: &crate::ann::ManifestAnn,
) -> Result<()> {
    let source_index_sha256 = required_source_meta(conn, source_id, "source_index_sha256")?;
    let actual = crate::ann::compute_identity(conn, source_id, &source_index_sha256)?;
    if actual.source_id != *source_id
        || actual.corpus_id != info.corpus_id
        || actual.embedding_set_sha256 != info.embedding_set_sha256
        || actual.vector_count != info.vector_count
    {
        bail!("ANN sidecar embedding digest for source `{source_id}` does not match the staged database");
    }
    Ok(())
}

pub(crate) fn verify_semantic_install(conn: &Connection, manifest: &Manifest) -> Result<()> {
    if manifest.model.id != EMBEDDING_MODEL_ID {
        bail!("semantic corpus uses an unsupported embedding model");
    }
    let database_sources = database_source_ids(conn)?;
    let manifest_sources = manifest.ann.keys().cloned().collect::<BTreeSet<_>>();
    let registered_sources = registered_source_ids();
    if database_sources != registered_sources || manifest_sources != registered_sources {
        bail!(
            "semantic corpus source sets differ: registered={registered_sources:?}, manifest={manifest_sources:?}, database={database_sources:?}"
        );
    }
    for (source_id, ann_info) in &manifest.ann {
        let documents = count_source_table(conn, source_id.as_str(), "documents")?;
        let chunks = count_source_table(conn, source_id.as_str(), "chunks")?;
        let embeddings = chunk_embedding_count(conn, source_id)?;
        if documents == 0
            || chunks == 0
            || embeddings != chunks
            || u64::try_from(embeddings).ok() != Some(ann_info.vector_count)
        {
            bail!(
                "semantic corpus for source `{source_id}` is incomplete: documents={documents}, chunks={chunks}, chunk_embeddings={embeddings}, ann_vectors={}",
                ann_info.vector_count
            );
        }
        verify_ann_db_binding(conn, source_id, ann_info)?;
        verify_ann_db_content(conn, source_id, ann_info)?;
    }
    Ok(())
}

pub(crate) fn chunk_embedding_count(conn: &Connection, source_id: &SourceId) -> Result<i64> {
    count_source_table(conn, source_id.as_str(), "chunk_embeddings")
}

pub(crate) fn load_active_generation_manifest() -> Result<Option<Manifest>> {
    let root = crate::config::live_dir()?;
    let path = root.join(crate::config::GENERATION_MANIFEST_FILENAME);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(read_generation_manifest(&root)?))
}

// ----- ATO browse-tree crawl and source snapshot -----

pub(crate) const SCRAPER_EXCLUDED_TITLES: &[&str] = &[
    "Archived document types",
    "Amending legislation",
    "Amending regulations",
    "Archived",
    "Full document",
    "View list of provisions",
    "Draft",
    "Draft amendments",
];

pub(crate) fn scraper_normalise_title(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
        .to_lowercase()
}

pub(crate) fn scraper_is_excluded(title: &str) -> bool {
    static EXCLUDED: std::sync::OnceLock<std::collections::HashSet<String>> =
        std::sync::OnceLock::new();
    let set = EXCLUDED.get_or_init(|| {
        SCRAPER_EXCLUDED_TITLES
            .iter()
            .map(|s| scraper_normalise_title(s))
            .collect()
    });
    set.contains(&scraper_normalise_title(title))
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SnapshotNode {
    pub(crate) uid: u64,
    pub(crate) parent_uid: Option<u64>,
    pub(crate) title: String,
    pub(crate) level: u32,
    pub(crate) node_type: String,
    pub(crate) data_url: Option<String>,
    pub(crate) href: Option<String>,
    pub(crate) canonical_id: Option<String>,
    pub(crate) path: Vec<String>,
    pub(crate) payload: JsonValue,
}

pub(crate) fn canonical_id_from(data_url: Option<&str>, href: Option<&str>) -> Option<String> {
    if let Some(h) = href {
        return Some(h.to_string());
    }
    let data_url = data_url?;
    // parse_qs equivalent: find TOC=value in the query string portion.
    let qs = data_url.split_once('?').map(|x| x.1).unwrap_or(data_url);
    for pair in qs.split('&') {
        let mut it = pair.splitn(2, '=');
        if let (Some(k), Some(v)) = (it.next(), it.next()) {
            if k == "TOC" {
                // Manual percent-decode (avoids pulling percent-encoding crate).
                let mut out = String::with_capacity(v.len());
                let bytes = v.as_bytes();
                let mut i = 0;
                while i < bytes.len() {
                    if bytes[i] == b'%' && i + 2 < bytes.len() {
                        if let Ok(byte) = u8::from_str_radix(
                            std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00"),
                            16,
                        ) {
                            out.push(byte as char);
                            i += 3;
                            continue;
                        }
                    }
                    out.push(bytes[i] as char);
                    i += 1;
                }
                return Some(out);
            }
        }
    }
    Some(data_url.to_string())
}

// fetch_nodes_blocking hits the ATO browse-content JSON API through a
// reqwest blocking client; the response payload is expected to be a JSON list.
pub(crate) fn fetch_nodes_blocking(
    _client: &reqwest::blocking::Client,
    base_url: &str,
    query: &str,
) -> Result<Vec<JsonValue>> {
    let url = if query.is_empty() {
        base_url.trim_end_matches('?').to_string()
    } else {
        format!(
            "{}?{}",
            base_url.trim_end_matches('?'),
            query.trim_start_matches('?')
        )
    };
    let parsed = url::Url::parse(&url).with_context(|| format!("parsing {url}"))?;
    let mut response = secure_get(parsed, Duration::from_secs(120))?;
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("ATO API response exceeded {MAX_MANIFEST_BYTES} bytes");
    }
    let payload: JsonValue = serde_json::from_slice(&bytes).context("parsing ATO API JSON")?;
    let arr = payload
        .as_array()
        .ok_or_else(|| anyhow!("ATO response payload is not a list"))?;
    Ok(arr.clone())
}

pub(crate) fn tree_crawl(
    root_query: &str,
    out_dir: &Path,
    base_url: &str,
    timeout_seconds: f64,
    request_interval_seconds: f64,
    max_nodes: Option<usize>,
) -> Result<()> {
    use std::collections::VecDeque;

    fs::create_dir_all(out_dir)?;
    let nodes_path = out_dir.join("nodes.jsonl");
    let nodes_file = File::create(&nodes_path)?;
    let mut nodes_writer = std::io::BufWriter::new(nodes_file);

    let client = reqwest::blocking::Client::builder()
        .user_agent(ATO_USER_AGENT)
        .timeout(Duration::from_secs_f64(timeout_seconds))
        .build()?;

    // Maintainer ATO API pacing is serialized through this mutex so
    // tree-crawl/link-download do not issue simultaneous outgoing requests.
    // Tree crawler can issue thousands per run.
    let last_request = std::sync::Mutex::new(
        std::time::Instant::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(std::time::Instant::now),
    );
    let acquire = || {
        if request_interval_seconds <= 0.0 {
            return;
        }
        let mut last = last_request.lock().unwrap();
        let now = std::time::Instant::now();
        let earliest = *last + Duration::from_secs_f64(request_interval_seconds);
        if earliest > now {
            std::thread::sleep(earliest - now);
            *last = earliest;
        } else {
            *last = now;
        }
    };

    acquire();
    let initial = fetch_nodes_blocking(&client, base_url, root_query)?;

    #[derive(Debug)]
    struct QueueItem {
        parent_uid: Option<u64>,
        path: Vec<String>,
        payload: JsonValue,
        level: u32,
    }
    let mut queue: VecDeque<QueueItem> = VecDeque::new();
    queue.extend(initial.into_iter().map(|p| QueueItem {
        parent_uid: None,
        path: Vec::new(),
        payload: p,
        level: 0,
    }));
    let mut visited_data_urls: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut uid_counter: u64 = 0;
    let mut total_written: usize = 0;
    let mut folder_count: usize = 0;
    let mut link_count: usize = 0;

    while let Some(item) = queue.pop_front() {
        uid_counter += 1;
        let title = item
            .payload
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("(untitled)")
            .to_string();
        let data_url = item
            .payload
            .get("data")
            .and_then(|d| d.get("url"))
            .and_then(|u| u.as_str())
            .map(|s| s.to_string());
        let href = item
            .payload
            .get("a_attr")
            .and_then(|a| a.get("href"))
            .and_then(|h| h.as_str())
            .map(|s| s.to_string());
        let node_type = match (data_url.is_some(), href.is_some()) {
            (true, true) => "folder+link",
            (true, false) => "folder",
            (false, true) => "link",
            (false, false) => "unknown",
        }
        .to_string();
        let canonical_id = canonical_id_from(data_url.as_deref(), href.as_deref());
        let mut new_path = item.path.clone();
        new_path.push(title.clone());

        let node = SnapshotNode {
            uid: uid_counter,
            parent_uid: item.parent_uid,
            title: title.clone(),
            level: item.level,
            node_type: node_type.clone(),
            data_url: data_url.clone(),
            href: href.clone(),
            canonical_id,
            path: new_path.clone(),
            payload: item.payload.clone(),
        };

        if scraper_is_excluded(&title) {
            if let Some(url) = data_url.as_deref() {
                visited_data_urls.insert(url.to_string());
            }
            continue;
        }

        // Stream node to disk to avoid holding entire snapshot in memory.
        use std::io::Write as _;
        writeln!(nodes_writer, "{}", serde_json::to_string(&node)?)?;
        total_written += 1;
        if node_type.contains("folder") {
            folder_count += 1;
        }
        if node_type.contains("link") {
            link_count += 1;
        }

        if total_written.is_multiple_of(500) {
            eprintln!(
                "tree-crawl: nodes={total_written} folders={folder_count} links={link_count} frontier={}",
                queue.len(),
            );
        }
        if let Some(cap) = max_nodes {
            if total_written >= cap {
                eprintln!("tree-crawl: reached max_nodes={cap}");
                break;
            }
        }

        let Some(child_url) = data_url else { continue };
        if !visited_data_urls.insert(child_url.clone()) {
            continue;
        }

        acquire();
        let child_payloads = match fetch_nodes_blocking(&client, base_url, &child_url) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("tree-crawl: failed to fetch {child_url}: {e}");
                continue;
            }
        };
        queue.extend(child_payloads.into_iter().map(|p| QueueItem {
            parent_uid: Some(uid_counter),
            path: new_path.clone(),
            payload: p,
            level: item.level + 1,
        }));
    }

    nodes_writer.flush()?;

    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let meta = json!({
        "generated_at": timestamp,
        "node_count": total_written,
        "folder_count": folder_count,
        "link_count": link_count,
        "root_query": root_query,
    });
    let meta_path = out_dir.join("meta.json");
    fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?)?;

    eprintln!(
        "tree-crawl: captured {total_written} nodes (folders={folder_count}, links={link_count}) in {}",
        out_dir.display()
    );
    Ok(())
}

// ----- Deterministic ATO snapshot reduction -----

#[derive(Debug, Default)]
pub(crate) struct CanonicalEntry {
    pub(crate) canonical_id: String,
    pub(crate) title: Option<String>,
    pub(crate) href: Option<String>,
    pub(crate) representative_path: Vec<String>,
    pub(crate) occurrences: u64,
    pub(crate) folder_occurrences: std::collections::HashSet<String>,
    pub(crate) owner_folder: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct FolderRecord {
    pub(crate) data_url: String,
    pub(crate) title: Option<String>,
    pub(crate) path: Vec<String>,
    pub(crate) parent_data_url: Option<String>,
    pub(crate) canonical_ids: std::collections::HashSet<String>,
    pub(crate) owned_ids: std::collections::HashSet<String>,
    pub(crate) redundant: bool,
}

pub(crate) fn is_better_path(candidate: &[String], incumbent: &[String]) -> bool {
    if incumbent.is_empty() {
        return true;
    }
    (candidate.len(), candidate) < (incumbent.len(), incumbent)
}

pub(crate) fn snapshot_reduce(nodes_path: &Path, output_dir: Option<&Path>) -> Result<()> {
    use std::collections::{HashMap, HashSet};
    use std::io::BufRead as _;

    let out_dir = output_dir
        .map(Path::to_path_buf)
        .or_else(|| nodes_path.parent().map(Path::to_path_buf))
        .ok_or_else(|| anyhow!("could not derive output dir"))?;
    fs::create_dir_all(&out_dir)?;

    let f = File::open(nodes_path).with_context(|| format!("opening {}", nodes_path.display()))?;
    let reader = std::io::BufReader::new(f);

    // node uid → (parent_uid, data_url)
    let mut node_meta: HashMap<u64, (Option<u64>, Option<String>)> = HashMap::new();
    // Reduction dedupes canonical IDs, chooses a representative
    // source path, and carries excluded-title descendants into skip output.
    let mut folder_records: HashMap<String, FolderRecord> = HashMap::new();
    let mut folder_children: HashMap<Option<String>, HashSet<String>> = HashMap::new();
    let mut canonical_entries: HashMap<String, CanonicalEntry> = HashMap::new();
    let mut excluded_uids: HashSet<u64> = HashSet::new();
    let mut excluded_counts: HashMap<String, u64> = HashMap::new();
    let mut excluded_folder_urls: HashSet<String> = HashSet::new();

    fn find_parent_folder(
        mut uid: Option<u64>,
        meta: &HashMap<u64, (Option<u64>, Option<String>)>,
    ) -> Option<String> {
        while let Some(u) = uid {
            let m = meta.get(&u)?;
            if let Some(url) = &m.1 {
                return Some(url.clone());
            }
            uid = m.0;
        }
        None
    }

    let mut total_nodes: u64 = 0;
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record: JsonValue = serde_json::from_str(trimmed)?;
        let uid = record
            .get("uid")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow!("node missing uid"))?;
        let parent_uid = record.get("parent_uid").and_then(|v| v.as_u64());
        let data_url = record
            .get("data_url")
            .and_then(|v| v.as_str())
            .map(String::from);
        node_meta.insert(uid, (parent_uid, data_url.clone()));

        let title = record
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let title_excluded = scraper_is_excluded(&title);
        let parent_excluded = parent_uid.is_some_and(|p| excluded_uids.contains(&p));
        if title_excluded || parent_excluded {
            excluded_uids.insert(uid);
            *excluded_counts
                .entry(if title.is_empty() {
                    "(untitled)".into()
                } else {
                    title.clone()
                })
                .or_default() += 1;
            if let Some(url) = &data_url {
                excluded_folder_urls.insert(url.clone());
            }
            continue;
        }

        if let Some(url) = &data_url {
            let parent_folder = find_parent_folder(parent_uid, &node_meta);
            let entry = folder_records
                .entry(url.clone())
                .or_insert_with(|| FolderRecord {
                    data_url: url.clone(),
                    title: record
                        .get("title")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    path: record
                        .get("path")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|p| p.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    parent_data_url: parent_folder.clone(),
                    ..Default::default()
                });
            entry.parent_data_url = parent_folder.clone();
            folder_children
                .entry(parent_folder)
                .or_default()
                .insert(url.clone());
        }

        let node_type = record
            .get("node_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let canonical_id_opt = record
            .get("canonical_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        if node_type.contains("link") {
            let Some(canonical_id) = canonical_id_opt else {
                continue;
            };
            let folder_url = data_url
                .clone()
                .or_else(|| find_parent_folder(parent_uid, &node_meta));
            let Some(folder_url) = folder_url else {
                continue;
            };
            folder_records
                .entry(folder_url.clone())
                .or_insert_with(|| FolderRecord {
                    data_url: folder_url.clone(),
                    ..Default::default()
                });
            let path: Vec<String> = record
                .get("path")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|p| p.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let title_str = record
                .get("title")
                .and_then(|v| v.as_str())
                .map(String::from);
            let href_str = record
                .get("href")
                .and_then(|v| v.as_str())
                .map(String::from);

            let entry = canonical_entries
                .entry(canonical_id.clone())
                .or_insert_with(|| CanonicalEntry {
                    canonical_id: canonical_id.clone(),
                    ..Default::default()
                });
            entry.occurrences += 1;
            entry.folder_occurrences.insert(folder_url.clone());
            if entry.href.is_none() {
                entry.href = href_str;
            }
            if entry.title.is_none() {
                entry.title = title_str.clone();
            }
            if entry.representative_path.is_empty()
                || is_better_path(&path, &entry.representative_path)
            {
                entry.representative_path = path;
                entry.title = title_str;
                entry.owner_folder = Some(folder_url.clone());
            }
            folder_records.entry(folder_url.clone()).and_modify(|fr| {
                fr.canonical_ids.insert(canonical_id.clone());
            });
        }

        total_nodes += 1;
        if total_nodes.is_multiple_of(1000) {
            eprintln!("snapshot-reduce: nodes={total_nodes}");
        }
    }

    // Assign folder ownership.
    for entry in canonical_entries.values() {
        if let Some(owner) = &entry.owner_folder {
            if let Some(rec) = folder_records.get_mut(owner) {
                rec.owned_ids.insert(entry.canonical_id.clone());
            }
        }
    }

    // Mark redundant folders via DFS rooted at folders whose parent is None.
    fn dfs(
        folder_url: &str,
        folder_records: &mut HashMap<String, FolderRecord>,
        folder_children: &HashMap<Option<String>, HashSet<String>>,
    ) -> bool {
        let mut has_owned = folder_records
            .get(folder_url)
            .is_some_and(|r| !r.owned_ids.is_empty());
        let children: Vec<String> = folder_children
            .get(&Some(folder_url.to_string()))
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        for c in children {
            if dfs(&c, folder_records, folder_children) {
                has_owned = true;
            }
        }
        if let Some(rec) = folder_records.get_mut(folder_url) {
            rec.redundant = !has_owned;
        }
        has_owned
    }
    let roots: Vec<String> = folder_children
        .get(&None)
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    for root in roots {
        dfs(&root, &mut folder_records, &folder_children);
    }

    // Write deduped_links.jsonl + dedup_summary.json + redundant_paths.json + skip_data_urls.json.
    let dedup_path = out_dir.join("deduped_links.jsonl");
    let dedup_file = File::create(&dedup_path)?;
    let mut dedup_writer = std::io::BufWriter::new(dedup_file);
    let mut sorted_keys: Vec<&String> = canonical_entries.keys().collect();
    sorted_keys.sort();
    let mut total_occurrences: u64 = 0;
    for k in &sorted_keys {
        let entry = &canonical_entries[*k];
        total_occurrences += entry.occurrences;
        let row = json!({
            "canonical_id": entry.canonical_id,
            "href": entry.href,
            "title": entry.title,
            "representative_path": entry.representative_path,
            "occurrences": entry.occurrences,
            "folder_count": entry.folder_occurrences.len(),
        });
        use std::io::Write as _;
        writeln!(dedup_writer, "{}", serde_json::to_string(&row)?)?;
    }
    dedup_writer.flush()?;

    let mut excluded_urls_sorted: Vec<String> = excluded_folder_urls.iter().cloned().collect();
    excluded_urls_sorted.sort();
    let summary = json!({
        "unique_links": canonical_entries.len(),
        "total_occurrences": total_occurrences,
        "excluded_titles": excluded_counts,
        "excluded_folder_urls": excluded_urls_sorted,
    });
    fs::write(
        out_dir.join("dedup_summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;

    let mut redundant: Vec<&FolderRecord> =
        folder_records.values().filter(|r| r.redundant).collect();
    redundant.sort_by(|a, b| (a.path.len(), &a.data_url).cmp(&(b.path.len(), &b.data_url)));
    let payload: Vec<JsonValue> = redundant
        .iter()
        .map(|r| {
            json!({
                "data_url": r.data_url,
                "title": r.title,
                "path": r.path,
                "parent_data_url": r.parent_data_url,
                "canonical_id_count": r.canonical_ids.len(),
                "owned_canonical_ids": r.owned_ids.len(),
            })
        })
        .collect();
    fs::write(
        out_dir.join("redundant_paths.json"),
        serde_json::to_vec_pretty(&payload)?,
    )?;

    let mut all_skip: HashSet<String> = redundant.iter().map(|r| r.data_url.clone()).collect();
    all_skip.extend(excluded_folder_urls.iter().cloned());
    let mut skip_sorted: Vec<String> = all_skip.into_iter().collect();
    skip_sorted.sort();
    fs::write(
        out_dir.join("skip_data_urls.json"),
        serde_json::to_vec_pretty(&skip_sorted)?,
    )?;

    eprintln!(
        "snapshot-reduce: {} unique links, {} folders, {} redundant; out={}",
        canonical_entries.len(),
        folder_records.len(),
        payload.len(),
        out_dir.display(),
    );
    Ok(())
}

// ----- Rate-limited ATO source document download -----

pub(crate) struct LinkDownloadArgs {
    pub(crate) deduped_links: PathBuf,
    pub(crate) out_dir: PathBuf,
    pub(crate) base_url: String,
    pub(crate) request_delay_seconds: f64,
    pub(crate) timeout_seconds: f64,
    pub(crate) force: bool,
    pub(crate) workspace_lock_held: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LinkDownloadReport {
    pub(crate) completed: usize,
    pub(crate) errors: usize,
    pub(crate) skipped: usize,
}

struct LinkRefreshFailure {
    status: &'static str,
    error: String,
    http_status: Option<u16>,
    attempted_at: String,
    href: Option<String>,
}

fn record_link_refresh_failure(
    index: &mut std::collections::HashMap<String, JsonValue>,
    canonical_id: &str,
    out_dir: &Path,
    failure: LinkRefreshFailure,
) -> JsonValue {
    let record = if let Some(existing) = index.get(canonical_id).filter(|record| {
        record.get("status").and_then(JsonValue::as_str) == Some("success")
            && indexed_payload_path(out_dir, record)
                .is_some_and(|path| payload_matches_record(&path, record))
    }) {
        let mut record = existing.clone();
        if let Some(object) = record.as_object_mut() {
            object.insert("refresh_status".to_string(), json!(failure.status));
            object.insert("refresh_error".to_string(), json!(failure.error));
            object.insert(
                "refresh_http_status".to_string(),
                json!(failure.http_status),
            );
            object.insert(
                "refresh_attempted_at".to_string(),
                json!(failure.attempted_at),
            );
            object.insert("refresh_href".to_string(), json!(failure.href));
        }
        record
    } else {
        json!({
            "canonical_id": canonical_id,
            "href": failure.href,
            "status": failure.status,
            "payload_path": null,
            "error": failure.error,
            "http_status": failure.http_status,
            "downloaded_at": failure.attempted_at,
        })
    };
    index.insert(canonical_id.to_string(), record.clone());
    record
}

fn merge_resume_index_record(
    index: &mut std::collections::HashMap<String, JsonValue>,
    canonical_id: &str,
    record: JsonValue,
) {
    let status = record.get("status").and_then(JsonValue::as_str);
    if matches!(status, Some("failed" | "missing_content")) {
        if let Some(mut preserved) = index
            .get(canonical_id)
            .filter(|existing| {
                existing.get("status").and_then(JsonValue::as_str) == Some("success")
            })
            .cloned()
        {
            if let Some(object) = preserved.as_object_mut() {
                object.insert("refresh_status".to_string(), json!(status));
                for (source, target) in [
                    ("error", "refresh_error"),
                    ("http_status", "refresh_http_status"),
                    ("downloaded_at", "refresh_attempted_at"),
                    ("href", "refresh_href"),
                ] {
                    if let Some(value) = record.get(source) {
                        object.insert(target.to_string(), value.clone());
                    }
                }
            }
            index.insert(canonical_id.to_string(), preserved);
            return;
        }
    }
    index.insert(canonical_id.to_string(), record);
}

pub(crate) fn slug_for(text: &str, fallback: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"[^A-Za-z0-9]+").unwrap());
    let s = re.replace_all(text.trim(), "_");
    let s = s.trim_matches('_').to_string();
    let s = if s.is_empty() {
        fallback.to_string()
    } else {
        s
    };
    s.chars().take(80).collect()
}

pub(crate) fn build_payload_path(out_dir: &Path, link: &JsonValue) -> Result<PathBuf> {
    let payload_dir = out_dir.join("payloads");
    let mut dir = payload_dir.clone();
    // Catch-up/download payload paths inherit representative_path
    // from reduced source links.
    if let Some(seg) = link.get("representative_path").and_then(|v| v.as_array()) {
        for s in seg.iter().filter_map(|v| v.as_str()) {
            dir = dir.join(slug_for(s, "node"));
        }
    }
    let canonical_id = link
        .get("canonical_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let filename = format!("{}.html", slug_for(canonical_id, "link"));
    let path = dir.join(filename);
    if !path.starts_with(&payload_dir) {
        bail!("payload path escaped payload root");
    }
    Ok(path)
}

fn prepare_payload_parent(payload_root: &Path, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("payload path has no parent"))?;
    if !path.starts_with(payload_root) {
        bail!("payload path escaped {}", payload_root.display());
    }
    let mut current = payload_root.to_path_buf();
    for component in parent.strip_prefix(payload_root)?.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                bail!("unsafe payload path component {}", current.display());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
            }
            Err(error) => return Err(error.into()),
        }
        let canonical = current.canonicalize()?;
        if !canonical.starts_with(payload_root) {
            bail!("payload directory escaped {}", payload_root.display());
        }
    }
    Ok(())
}

fn payload_matches_record(path: &Path, record: &JsonValue) -> bool {
    let Some(expected_sha) = record.get("sha256").and_then(JsonValue::as_str) else {
        return false;
    };
    let Some(expected_size) = record.get("size").and_then(JsonValue::as_u64) else {
        return false;
    };
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() == expected_size)
        && sha256_path(path).is_ok_and(|actual| actual == expected_sha)
}

fn indexed_payload_path(out_dir: &Path, record: &JsonValue) -> Option<PathBuf> {
    let raw = Path::new(record.get("payload_path")?.as_str()?);
    if raw.is_absolute()
        || raw
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    let canonical_root = out_dir.canonicalize().ok()?;
    let canonical = canonical_root.join(raw).canonicalize().ok()?;
    canonical.starts_with(&canonical_root).then_some(canonical)
}

fn immutable_payload_path(base_path: &Path, sha256: &str) -> Result<PathBuf> {
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("payload SHA-256 is malformed");
    }
    let parent = base_path
        .parent()
        .ok_or_else(|| anyhow!("payload path has no parent"))?;
    Ok(parent.join(format!("{sha256}.html")))
}

fn persist_immutable_payload(payload_root: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    prepare_payload_parent(payload_root, path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("immutable payload path is unsafe: {}", path.display());
            }
            if metadata.len() != bytes.len() as u64
                || sha256_path(path)? != format!("{:x}", Sha256::digest(bytes))
            {
                bail!("immutable payload collision at {}", path.display());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            atomic_write(path, bytes)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(crate) fn extract_law_contents(html: &str) -> Option<String> {
    use scraper::{Html, Selector};
    let doc = Html::parse_document(html);
    let sel = Selector::parse("article").ok()?;
    let article = doc.select(&sel).next()?;
    // Wrap children in <div id="lawContents">.
    let mut inner = String::new();
    for child in article.children() {
        if let Some(eref) = scraper::ElementRef::wrap(child) {
            inner.push_str(&eref.html());
        } else if let Some(text) = child.value().as_text() {
            inner.push_str(text);
        }
    }
    Some(format!(r#"<div id="lawContents">{inner}</div>"#))
}

pub(crate) fn link_download(args: LinkDownloadArgs) -> Result<LinkDownloadReport> {
    use std::io::BufRead as _;
    use std::sync::{Arc, Mutex};

    let download_started_at = chrono::Utc::now().to_rfc3339();

    fs::create_dir_all(&args.out_dir)?;
    let out_dir = args.out_dir.canonicalize()?;
    let _workspace_lock = if args.workspace_lock_held {
        None
    } else {
        Some(crate::source_update::lock_workspace_exclusive(&out_dir)?)
    };
    let payload_dir = out_dir.join("payloads");
    if fs::symlink_metadata(&payload_dir).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "payload root must not be a symlink: {}",
            payload_dir.display()
        );
    }
    let index_path = out_dir.join("index.jsonl");
    fs::create_dir_all(&payload_dir)?;
    let payload_dir = payload_dir.canonicalize()?;
    if !payload_dir.starts_with(&out_dir) {
        bail!("payload root escaped output directory");
    }

    // Load links.
    let f = File::open(&args.deduped_links)
        .with_context(|| format!("opening {}", args.deduped_links.display()))?;
    let reader = std::io::BufReader::new(f);
    let mut links: Vec<JsonValue> = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        links.push(serde_json::from_str(trimmed)?);
    }
    let total = links.len();
    eprintln!("link-download: {total} links to consider");

    // Load existing index for resumability.
    let mut index: std::collections::HashMap<String, JsonValue> = std::collections::HashMap::new();
    if index_path.exists() {
        if fs::symlink_metadata(&index_path)?.file_type().is_symlink() {
            bail!(
                "resume index must not be a symlink: {}",
                index_path.display()
            );
        }
        let bytes = fs::read(&index_path)?;
        let terminated = bytes.ends_with(b"\n");
        let lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
        for (line_number, line) in lines.iter().enumerate() {
            let line = std::str::from_utf8(line).context("resume index is not UTF-8")?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let rec: JsonValue = match serde_json::from_str(trimmed) {
                Ok(record) => record,
                Err(_) if !terminated && line_number + 1 == lines.len() => break,
                Err(error) => return Err(error).context("parsing resume index"),
            };
            if let Some(cid) = rec.get("canonical_id").and_then(|v| v.as_str()) {
                let cid = cid.to_string();
                merge_resume_index_record(&mut index, &cid, rec);
            }
        }
    }
    let initial_completed = links
        .iter()
        .filter(|link| {
            let Some(canonical_id) = link.get("canonical_id").and_then(JsonValue::as_str) else {
                return false;
            };
            index.get(canonical_id).is_some_and(|record| {
                record.get("status").and_then(JsonValue::as_str) == Some("success")
                    && record.get("refresh_status").is_none()
                    && indexed_payload_path(&out_dir, record)
                        .is_some_and(|path| payload_matches_record(&path, record))
            })
        })
        .count();
    if initial_completed > 0 {
        eprintln!("link-download: resuming with {initial_completed} previously completed");
    }
    let index = Arc::new(Mutex::new(index));

    let last_request = Arc::new(Mutex::new(
        std::time::Instant::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(std::time::Instant::now),
    ));
    let request_delay = args.request_delay_seconds;
    let concurrency = Arc::new(AdaptiveConcurrency::new("ato"));

    // Link-download fans out over worker threads with a shared queue,
    // shared client, shared index writer, and shared request-delay lock.
    let work_queue: Arc<Mutex<Vec<JsonValue>>> = Arc::new(Mutex::new(links));
    let stats_completed = Arc::new(std::sync::atomic::AtomicUsize::new(initial_completed));
    let stats_errors = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stats_skipped = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let index_writer = Arc::new(Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index_path)?,
    ));

    let mut handles = Vec::with_capacity(SOURCE_WORKER_CEILING);
    for worker_id in 0..SOURCE_WORKER_CEILING {
        let work_queue = Arc::clone(&work_queue);
        let last_request = Arc::clone(&last_request);
        let index = Arc::clone(&index);
        let index_writer = Arc::clone(&index_writer);
        let stats_completed = Arc::clone(&stats_completed);
        let stats_errors = Arc::clone(&stats_errors);
        let stats_skipped = Arc::clone(&stats_skipped);
        let concurrency = Arc::clone(&concurrency);
        let base_url = args.base_url.clone();
        let out_dir = out_dir.clone();
        let payload_dir = payload_dir.clone();
        let timeout = Duration::from_secs_f64(args.timeout_seconds);
        let force = args.force;

        handles.push(std::thread::spawn(move || -> Result<()> {
            loop {
                let link = {
                    let mut q = work_queue.lock().unwrap();
                    q.pop()
                };
                let Some(link) = link else { break };
                let canonical_id = link
                    .get("canonical_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let href = link.get("href").and_then(|v| v.as_str()).map(String::from);

                let payload_path = build_payload_path(&out_dir, &link)?;

                // Skip if already done.
                if !force {
                    let already_done = {
                        let idx = index.lock().unwrap();
                        idx.get(&canonical_id).is_some_and(|record| {
                            record.get("status").and_then(JsonValue::as_str) == Some("success")
                                && record.get("refresh_status").is_none()
                                && indexed_payload_path(&out_dir, record)
                                    .is_some_and(|path| payload_matches_record(&path, record))
                        })
                    };
                    if already_done {
                        stats_skipped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        continue;
                    }
                }

                let url = match href.as_deref() {
                    Some(h) if h.starts_with('/') => {
                        format!("{}{}", base_url.trim_end_matches('/'), h)
                    }
                    Some(h) => h.to_string(),
                    None => {
                        eprintln!("link-download w{worker_id}: missing href for {canonical_id}");
                        stats_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        continue;
                    }
                };

                let request = concurrency.acquire()?;
                let pacing_started = Instant::now();
                if request_delay > 0.0 {
                    let mut last = last_request.lock().unwrap();
                    let now = std::time::Instant::now();
                    let earliest = *last + Duration::from_secs_f64(request_delay);
                    if earliest > now {
                        std::thread::sleep(earliest - now);
                        *last = earliest;
                    } else {
                        *last = now;
                    }
                }
                let pacing_wait = pacing_started.elapsed();

                let parsed_url =
                    url::Url::parse(&url).with_context(|| format!("parsing acquisition URL {url}"));
                let resp = parsed_url.and_then(|url| secure_get(url, timeout));
                let (http_status, html) = match resp {
                    Ok(mut r) => {
                        let status = r.status();
                        let mut bytes = Vec::new();
                        r.by_ref()
                            .take(MAX_ATO_HTML_BYTES + 1)
                            .read_to_end(&mut bytes)?;
                        if bytes.len() as u64 > MAX_ATO_HTML_BYTES {
                            bail!("ATO response exceeded {MAX_ATO_HTML_BYTES} bytes");
                        }
                        let html =
                            String::from_utf8(bytes).context("ATO response was not UTF-8")?;
                        request.finish(
                            &url,
                            Some(status.as_u16()),
                            html.len(),
                            1,
                            RequestOutcome::Success,
                            pacing_wait,
                            None,
                        );
                        (status.as_u16(), html)
                    }
                    Err(e) => {
                        request.finish(
                            &url,
                            None,
                            0,
                            1,
                            RequestOutcome::Transient,
                            pacing_wait,
                            None,
                        );
                        eprintln!("link-download w{worker_id}: failed {canonical_id}: {e}");
                        stats_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let now = chrono::Utc::now().to_rfc3339();
                        {
                            use std::io::Write as _;
                            let mut idx = index.lock().unwrap();
                            let rec = record_link_refresh_failure(
                                &mut idx,
                                &canonical_id,
                                &out_dir,
                                LinkRefreshFailure {
                                    status: "failed",
                                    error: e.to_string(),
                                    http_status: None,
                                    attempted_at: now,
                                    href: href.clone(),
                                },
                            );
                            let mut w = index_writer.lock().unwrap();
                            writeln!(w, "{}", serde_json::to_string(&rec)?)?;
                            w.sync_data()?;
                        }
                        continue;
                    }
                };

                let snippet = match extract_law_contents(&html) {
                    Some(s) => s,
                    None => {
                        eprintln!(
                            "link-download w{worker_id}: missing lawContents for {canonical_id}"
                        );
                        stats_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let now = chrono::Utc::now().to_rfc3339();
                        {
                            use std::io::Write as _;
                            let mut idx = index.lock().unwrap();
                            let rec = record_link_refresh_failure(
                                &mut idx,
                                &canonical_id,
                                &out_dir,
                                LinkRefreshFailure {
                                    status: "missing_content",
                                    error: "lawContents div not found".to_string(),
                                    http_status: Some(http_status),
                                    attempted_at: now,
                                    href: href.clone(),
                                },
                            );
                            let mut w = index_writer.lock().unwrap();
                            writeln!(w, "{}", serde_json::to_string(&rec)?)?;
                            w.sync_data()?;
                        }
                        continue;
                    }
                };

                let payload_sha = format!("{:x}", Sha256::digest(snippet.as_bytes()));
                let payload_path = immutable_payload_path(&payload_path, &payload_sha)?;
                persist_immutable_payload(&payload_dir, &payload_path, snippet.as_bytes())?;
                let payload_size = snippet.len() as u64;

                let rel = payload_path
                    .strip_prefix(&out_dir)
                    .unwrap_or(&payload_path)
                    .to_string_lossy()
                    .to_string();
                let now = chrono::Utc::now().to_rfc3339();
                let rec = json!({
                    "canonical_id": canonical_id,
                    "href": href,
                    "status": "success",
                    "payload_path": rel,
                    "assets": [],
                    "error": null,
                    "http_status": http_status,
                    "downloaded_at": now,
                    "sha256": payload_sha,
                    "size": payload_size,
                });
                {
                    use std::io::Write as _;
                    let mut idx = index.lock().unwrap();
                    idx.insert(canonical_id.clone(), rec.clone());
                    let mut w = index_writer.lock().unwrap();
                    writeln!(w, "{}", serde_json::to_string(&rec)?)?;
                    w.sync_data()?;
                }
                let n = stats_completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n.is_multiple_of(50) {
                    eprintln!(
                        "link-download: {n}/{total} done (errors={}, skipped={})",
                        stats_errors.load(std::sync::atomic::Ordering::Relaxed),
                        stats_skipped.load(std::sync::atomic::Ordering::Relaxed),
                    );
                }
            }
            Ok(())
        }));
    }

    let mut worker_errors = Vec::new();
    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => worker_errors.push(error.to_string()),
            Err(_) => worker_errors.push("worker panicked".to_string()),
        }
    }
    drop(index_writer);
    if !worker_errors.is_empty() {
        bail!(
            "link-download workers failed after all workers stopped: {}",
            worker_errors.join("; ")
        );
    }

    // Atomic rewrite of index.jsonl with sorted entries.
    let idx = index.lock().unwrap();
    let mut keys: Vec<&String> = idx.keys().collect();
    keys.sort();
    let mut tmp = tempfile::NamedTempFile::new_in(&out_dir)?;
    for k in keys {
        use std::io::Write as _;
        writeln!(tmp, "{}", serde_json::to_string(&idx[k])?)?;
    }
    tmp.as_file().sync_all()?;
    tmp.persist(&index_path).map_err(|error| error.error)?;
    sync_parent(&index_path)?;

    // metadata.json.
    let download_completed_at = chrono::Utc::now().to_rfc3339();
    let report = LinkDownloadReport {
        completed: stats_completed.load(std::sync::atomic::Ordering::Relaxed),
        errors: stats_errors.load(std::sync::atomic::Ordering::Relaxed),
        skipped: stats_skipped.load(std::sync::atomic::Ordering::Relaxed),
    };
    let metadata = json!({
        "links_file": args.deduped_links.to_string_lossy(),
        "download_started_at": download_started_at,
        "download_completed_at": download_completed_at,
        "total_links": total,
        "completed_links": report.completed,
    });
    atomic_write(
        &out_dir.join("metadata.json"),
        &serde_json::to_vec_pretty(&metadata)?,
    )?;

    eprintln!(
        "link-download: done — {} success, {} errors, {} skipped (out_dir={})",
        report.completed,
        report.errors,
        report.skipped,
        args.out_dir.display(),
    );
    Ok(report)
}

// ----- Incremental and catch-up ATO snapshot diff -----

pub(crate) fn representative_path_from_docid(
    canonical_id: &str,
    title: &str,
    heading: Option<&str>,
) -> Vec<String> {
    // Derive a stable representative path from the document id, heading, and
    // title; fall back to `Other` when no category can be identified.
    use scraper as _;
    if let Some(category) = doc_id_top_category(canonical_id) {
        let mut out = vec![category.to_string()];
        if let Some(h) = heading {
            if !h.is_empty() {
                out.push(h.to_string());
            }
        }
        if !title.is_empty() {
            out.push(title.to_string());
        }
        return out;
    }
    vec!["Other".to_string()]
}

pub(crate) fn doc_id_top_category(canonical_id: &str) -> Option<&'static str> {
    // Best-effort extraction of the top-level category from a canonical_id
    // such as /law/view/document?docid=CRP%2FCRP19%2FCR. The common document
    // prefixes used by the maintainer pipeline map to stable buckets; anything
    // unrecognised falls through to `Other` so downloads always have a folder.
    let lower = canonical_id.to_ascii_lowercase();
    if lower.contains("docid=cm") || lower.contains("docid=tr") || lower.contains("docid=tr%2f") {
        return Some("Public_rulings");
    }
    if lower.contains("docid=psr") || lower.contains("docid=ps%20la") || lower.contains("docid=ps")
    {
        return Some("Practice_statements");
    }
    if lower.contains("docid=pba") || lower.contains("docid=pbr") {
        return Some("Edited_private_advice");
    }
    if lower.contains("docid=cr") || lower.contains("docid=crp") {
        return Some("Cases");
    }
    if lower.contains("docid=mt") || lower.contains("docid=md") {
        return Some("Public_rulings");
    }
    if lower.contains("docid=lct") || lower.contains("docid=ind") {
        return Some("Public_rulings");
    }
    if lower.contains("docid=pak") || lower.contains("docid=pal") {
        return Some("Legislation_and_supporting_material");
    }
    if lower.contains("docid=scd") || lower.contains("docid=scr") {
        return Some("Cases");
    }
    if lower.contains("docid=otr") {
        return Some("Public_rulings");
    }
    if lower.contains("docid=ato") {
        return Some("Public_rulings");
    }
    None
}

pub(crate) fn load_canonical_ids(index_path: &Path) -> Result<std::collections::HashSet<String>> {
    use std::io::BufRead as _;
    let mut states = BTreeMap::new();
    if !index_path.exists() {
        return Ok(std::collections::HashSet::new());
    }
    let f = File::open(index_path)?;
    let reader = std::io::BufReader::new(f);
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let rec: JsonValue = serde_json::from_str(trimmed)?;
        if let Some(cid) = rec.get("canonical_id").and_then(|value| value.as_str()) {
            let normalised = normalize_doc_href(cid);
            if normalised.is_empty() {
                continue;
            }
            let complete = matches!(
                rec.get("status").and_then(JsonValue::as_str),
                Some("success" | "confirmed_404" | "confirmed_stub")
            ) && rec.get("refresh_status").is_none();
            states.insert(normalised, complete);
        }
    }
    Ok(states
        .into_iter()
        .filter_map(|(canonical_id, complete)| complete.then_some(canonical_id))
        .collect())
}

pub(crate) fn scrape_diff(
    index_path: &Path,
    deduped: Option<&Path>,
    whats_new_url: Option<&str>,
    path_prefix: Option<&str>,
    out_path: &Path,
) -> Result<()> {
    use std::io::BufRead as _;
    use std::io::Write as _;

    let existing = load_canonical_ids(index_path)?;
    eprintln!(
        "scrape-diff: {} existing canonical IDs in {}",
        existing.len(),
        index_path.display()
    );

    let prefix_parts: Vec<String> = match path_prefix {
        Some(p) => p
            .split('/')
            .map(String::from)
            .filter(|s| !s.is_empty())
            .collect(),
        None => Vec::new(),
    };

    let out_file = File::create(out_path)?;
    let mut out_writer = std::io::BufWriter::new(out_file);

    let mut total: usize = 0;
    let mut missing: usize = 0;
    let mut by_category: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    if let Some(d) = deduped {
        // Catch-up mode: diff a deduped_links.jsonl against the existing index.
        let f = File::open(d)?;
        let reader = std::io::BufReader::new(f);
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            total += 1;
            let mut rec: JsonValue = serde_json::from_str(trimmed)?;
            let cid = rec
                .get("canonical_id")
                .and_then(|v| v.as_str())
                .map(normalize_doc_href)
                .unwrap_or_default();
            if cid.is_empty() || existing.contains(&cid) {
                continue;
            }
            if !prefix_parts.is_empty() {
                let mut new_path: Vec<JsonValue> = prefix_parts
                    .iter()
                    .map(|s| JsonValue::String(s.clone()))
                    .collect();
                if let Some(rep) = rec.get("representative_path").and_then(|v| v.as_array()) {
                    new_path.extend(rep.iter().cloned());
                }
                rec["representative_path"] = JsonValue::Array(new_path);
            }
            let cat = rec
                .get("representative_path")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|s| s.as_str())
                .unwrap_or("(uncategorized)")
                .to_string();
            *by_category.entry(cat).or_insert(0) += 1;
            writeln!(out_writer, "{}", serde_json::to_string(&rec)?)?;
            missing += 1;
        }
    } else if let Some(url) = whats_new_url {
        // Incremental mode: fetch What's New live, build pending records.
        let parsed = url::Url::parse(url).with_context(|| format!("parsing {url}"))?;
        let mut response = secure_get(parsed, Duration::from_secs(30))?;
        let mut bytes = Vec::new();
        response
            .by_ref()
            .take(MAX_ATO_HTML_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_ATO_HTML_BYTES {
            bail!("What's New response exceeded {MAX_ATO_HTML_BYTES} bytes");
        }
        let html = String::from_utf8(bytes).context("What's New response was not UTF-8")?;
        let entries = parse_whats_new(&html, "https://www.ato.gov.au")?;
        for e in entries {
            total += 1;
            let cid = normalize_doc_href(&e.href);
            if cid.is_empty() || existing.contains(&cid) {
                continue;
            }
            let segments = representative_path_from_docid(&cid, &e.title, e.heading.as_deref());
            let cat = segments
                .first()
                .cloned()
                .unwrap_or_else(|| "(uncategorized)".to_string());
            *by_category.entry(cat).or_insert(0) += 1;
            let rec = json!({
                "canonical_id": cid,
                "href": cid,
                "title": e.title,
                "representative_path": segments,
                "occurrences": 1,
                "folder_count": 1,
            });
            writeln!(out_writer, "{}", serde_json::to_string(&rec)?)?;
            missing += 1;
        }
    } else {
        bail!("scrape-diff: must pass either --deduped FILE or --whats-new-url URL");
    }

    out_writer.flush()?;
    let mut sorted_cats: Vec<(String, usize)> = by_category.into_iter().collect();
    sorted_cats.sort_by_key(|b| std::cmp::Reverse(b.1));
    eprintln!(
        "scrape-diff: {missing} missing of {total} candidates -> {} ({} categories)",
        out_path.display(),
        sorted_cats.len(),
    );
    for (cat, n) in sorted_cats.iter().take(10) {
        eprintln!("  {n:>5} {cat}");
    }
    Ok(())
}

#[cfg(test)]
mod security_tests {
    use super::*;

    #[test]
    fn private_and_special_networks_are_rejected() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.169.254",
            "192.0.2.1",
            "198.18.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!public_ip(address.parse().unwrap()), "accepted {address}");
        }
        assert!(public_ip("8.8.8.8".parse().unwrap()));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    fn sample_manifest() -> Manifest {
        let ann = crate::legal_source::source_registry()
            .source_ids()
            .into_iter()
            .enumerate()
            .map(|(index, source)| {
                let source_id: SourceId = source.parse().expect("valid registered source id");
                let digit = char::from_digit((index % 10) as u32, 10)
                    .expect("single decimal digit")
                    .to_string();
                let metadata = crate::ann::ManifestAnn {
                    source_id: source_id.clone(),
                    format: crate::ann::ANN_FORMAT.to_string(),
                    format_version: crate::ann::ANN_FORMAT_VERSION,
                    library: crate::ann::ANN_LIBRARY.to_string(),
                    library_version: crate::ann::ANN_LIBRARY_VERSION.to_string(),
                    path: crate::ann::sidecar_manifest_path(&source_id),
                    sha256: digit.repeat(64),
                    size: 1,
                    corpus_id: format!("sha256:{}", digit.repeat(64)),
                    embedding_model_id: EMBEDDING_MODEL_ID.to_string(),
                    embedding_dimension: crate::EMBEDDING_DIM as u32,
                    embedding_set_sha256: digit.repeat(64),
                    vector_count: 1,
                    seed: crate::ann::ANN_SEED,
                    rng: crate::ann::ANN_RNG.to_string(),
                    trees: crate::ann::ANN_TREES as u32,
                    split_after: crate::ann::ANN_SPLIT_AFTER as u32,
                    id_encoding: crate::ann::ANN_ID_ENCODING.to_string(),
                    metric: crate::ann::ANN_METRIC.to_string(),
                };
                (source_id, metadata)
            })
            .collect();
        Manifest {
            schema_version: SUPPORTED_SCHEMA_VERSION,
            index_version: "2026.07.11".to_string(),
            created_at: "2026-07-11T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                fingerprint: crate::EMBEDDING_MODEL_FINGERPRINT.to_string(),
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
                path: LEGAL_DB_FILENAME.to_string(),
                sha256: "5".repeat(64),
                size: 1,
            },
            ann,
        }
    }

    #[test]
    fn final_manifest_requires_exact_registered_source_map() -> Result<()> {
        let manifest = sample_manifest();
        validate_manifest(&manifest)?;

        let mut missing = manifest.clone();
        missing.ann.clear();
        assert!(validate_manifest(&missing).is_err());

        let mut mismatched = manifest.clone();
        mismatched
            .ann
            .get_mut(&"ato".parse::<SourceId>().expect("valid source id"))
            .expect("ATO manifest entry")
            .source_id = "frl".parse().expect("valid source id");
        assert!(validate_manifest(&mismatched).is_err());

        let mut encoded = serde_json::to_value(&manifest)?;
        encoded
            .as_object_mut()
            .expect("manifest object")
            .insert("unexpected".to_string(), json!(2));
        assert!(serde_json::from_value::<Manifest>(encoded).is_err());
        Ok(())
    }

    #[test]
    fn payload_paths_remain_beneath_payload_root() {
        let root = tempfile::tempdir().unwrap();
        let link = json!({
            "canonical_id": "../../JUD\\evil",
            "representative_path": ["..", "C:\\windows", "/absolute"]
        });
        let path = build_payload_path(root.path(), &link).unwrap();
        assert!(path.starts_with(root.path().join("payloads")));
        assert!(!path.to_string_lossy().contains(".."));
    }

    #[cfg(unix)]
    #[test]
    fn generation_files_reject_hard_links() -> Result<()> {
        let root = tempfile::tempdir()?;
        let file = root.path().join("artifact");
        fs::write(&file, b"immutable bytes")?;
        fs::hard_link(&file, root.path().join("other-link"))?;
        let sha = sha256_path(&file)?;
        let error = verified_regular_file(root.path(), "artifact", 15, &sha).unwrap_err();
        assert!(error.to_string().contains("hard-linked"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn activation_rejects_a_symlinked_generation_argument() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let runtime = root.path().join("runtime");
        let real = root.path().join("real-generation");
        let linked = root.path().join("linked-generation");
        fs::create_dir(&real)?;
        symlink(&real, &linked)?;
        let _environment =
            crate::TestEnvironment::set(&[("LEGAL_MCP_DATA_DIR", runtime.as_os_str())]);
        let error = activate_local_generation(&linked, None).unwrap_err();
        assert!(error.to_string().contains("real non-symlink directory"));
        Ok(())
    }

    #[test]
    fn incremental_diff_retries_nonterminal_acquisition_records() -> Result<()> {
        let root = tempfile::tempdir()?;
        let index = root.path().join("index.jsonl");
        fs::write(
            &index,
            [
                json!({"canonical_id": "/law/view/document?docid=JUD/GOOD/00001", "status": "success"}),
                json!({"canonical_id": "/law/view/document?docid=JUD/GONE/00001", "status": "confirmed_404"}),
                json!({"canonical_id": "/law/view/document?docid=JUD/RETRY/00001", "status": "failed"}),
                json!({"canonical_id": "/law/view/document?docid=JUD/MISSING/00001", "status": "missing_content"}),
                json!({"canonical_id": "/law/view/document?docid=JUD/STALE/00001", "status": "success", "refresh_status": "failed"}),
                json!({"canonical_id": "/law/view/document?docid=JUD/FLAP/00001", "status": "success"}),
                json!({"canonical_id": "/law/view/document?docid=JUD/FLAP/00001", "status": "failed"}),
            ]
            .into_iter()
            .map(|record| serde_json::to_string(&record))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("\n"),
        )?;

        let complete = load_canonical_ids(&index)?;
        assert!(complete.contains("/law/view/document?docid=JUD/GOOD/00001"));
        assert!(complete.contains("/law/view/document?docid=JUD/GONE/00001"));
        assert!(!complete.contains("/law/view/document?docid=JUD/RETRY/00001"));
        assert!(!complete.contains("/law/view/document?docid=JUD/MISSING/00001"));
        assert!(!complete.contains("/law/view/document?docid=JUD/STALE/00001"));
        assert!(!complete.contains("/law/view/document?docid=JUD/FLAP/00001"));
        Ok(())
    }

    #[test]
    fn failed_refresh_preserves_last_verified_payload_for_builds() -> Result<()> {
        let root = tempfile::tempdir()?;
        let root_path = root.path().canonicalize()?;
        let payload = root_path.join("document.html");
        fs::write(&payload, b"verified source")?;
        let canonical_id = "/law/view/document?docid=JUD/KEEP/00001";
        let mut index = std::collections::HashMap::from([(
            canonical_id.to_string(),
            json!({
                "canonical_id": canonical_id,
                "status": "success",
                "payload_path": "document.html",
                "size": fs::metadata(&payload)?.len(),
                "sha256": sha256_path(&payload)?,
            }),
        )]);
        let record = record_link_refresh_failure(
            &mut index,
            canonical_id,
            &root_path,
            LinkRefreshFailure {
                status: "failed",
                error: "transient".to_string(),
                http_status: None,
                attempted_at: "2026-07-12T00:00:00Z".to_string(),
                href: Some(canonical_id.to_string()),
            },
        );
        assert_eq!(record["status"], "success");
        assert_eq!(record["payload_path"], "document.html");
        assert_eq!(record["refresh_status"], "failed");
        assert!(payload_matches_record(&payload, &record));

        let mut reloaded = std::collections::HashMap::from([(
            canonical_id.to_string(),
            index[canonical_id].clone(),
        )]);
        reloaded
            .get_mut(canonical_id)
            .and_then(JsonValue::as_object_mut)
            .expect("object")
            .remove("refresh_status");
        merge_resume_index_record(
            &mut reloaded,
            canonical_id,
            json!({
                "canonical_id": canonical_id,
                "status": "missing_content",
                "error": "transient parse failure",
            }),
        );
        assert_eq!(reloaded[canonical_id]["status"], "success");
        assert_eq!(reloaded[canonical_id]["refresh_status"], "missing_content");

        let replacement = b"new verified source";
        let replacement_sha = format!("{:x}", Sha256::digest(replacement));
        let replacement_path = immutable_payload_path(&payload, &replacement_sha)?;
        persist_immutable_payload(&root_path, &replacement_path, replacement)?;
        assert_eq!(fs::read(&payload)?, b"verified source");
        assert_eq!(fs::read(&replacement_path)?, replacement);
        Ok(())
    }

    #[test]
    fn validated_generation_commit_is_immutable_and_prune_revalidates_active() -> Result<()> {
        let root = tempfile::tempdir()?;
        let _environment =
            crate::TestEnvironment::set(&[("LEGAL_MCP_DATA_DIR", root.path().as_os_str())]);

        let first_key = "a".repeat(64);
        let first = root.path().join("first-staging");
        fs::create_dir(&first)?;
        fs::write(first.join("marker"), b"first")?;
        let first_report = commit_validated_generation(&first, &first_key)?;
        assert_eq!(first_report.active_generation, first_key);
        assert_eq!(first_report.previous_generation, None);
        assert!(!first.exists());
        assert_eq!(
            active_generation_key()?.as_deref(),
            Some(first_key.as_str())
        );

        let second_key = "b".repeat(64);
        let second = root.path().join("second-staging");
        fs::create_dir(&second)?;
        fs::write(second.join("marker"), b"second")?;
        let second_report = commit_validated_generation(&second, &second_key)?;
        assert_eq!(
            second_report.previous_generation.as_deref(),
            Some(first_key.as_str())
        );
        assert_eq!(
            active_generation_key()?.as_deref(),
            Some(second_key.as_str())
        );

        let collision = root.path().join("collision-staging");
        fs::create_dir(&collision)?;
        let repeated = commit_validated_generation(&collision, &second_key)?;
        assert_eq!(repeated.active_generation, second_key);
        assert!(
            collision.exists(),
            "an idempotent activation must not consume its duplicate input"
        );

        let error = prune_inactive_generations(0).expect_err("fake generation must not prune");
        assert!(format!("{error:#}").contains("generation contents differ"));
        assert_eq!(
            active_generation_key()?.as_deref(),
            Some(second_key.as_str())
        );
        assert!(generation_dir(&first_key)?.is_dir());
        assert!(generation_dir(&second_key)?.is_dir());
        assert!(deactivate_generation(&first_key).is_err());
        let deactivated = deactivate_generation(&second_key)?;
        assert_eq!(deactivated.deactivated_generation, second_key);
        assert_eq!(active_generation_key()?, None);
        Ok(())
    }
}
