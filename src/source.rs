//! Source acquisition (scraper) + stats + manifest/update/release.
//!
//! TODO: this file accumulated several distinct concerns during the v0.11
//! refactor — split into scraper.rs, stats.rs, and manifest.rs in a follow-up.

use crate::config::{
    data_dir, db_path, installed_manifest_path, live_dir, lock_file,
    model_marker_path, staging_dir,
};
use crate::db::{
    get_meta, open_read, open_write_at, set_meta, table_exists,
};
use crate::extract::anchors_node_text;
use crate::search::ensure_vector_search_ready;
use crate::semantic::EMBEDDING_MODEL_HF_FILES;
use crate::{
    fetch_bytes, fetch_bytes_with, stage_model,
    validate_manifest_model_source, UrlContext,
    ATO_USER_AGENT, DEFAULT_EXCLUDED_TYPES, EDITED_PRIVATE_ADVICE_LABEL, EMBEDDING_MODEL_ID, LEGISLATION_TYPE, LEGISLATION_TYPE_PREFIXES,
    OLD_CONTENT_CUTOFF, SUPPORTED_SCHEMA_VERSION,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use reqwest::blocking::Client;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ----- What's New scraper (port of src/ato_mcp/scraper/whats_new.py) -----

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
        return format!("/law/view/document?docid={id}");
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
    let docs: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?;
    let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
    let embeddings: i64 = if table_exists(&conn, "chunk_embeddings")? {
        conn.query_row("SELECT COUNT(*) FROM chunk_embeddings", [], |r| r.get(0))?
    } else {
        0
    };
    let definitions: i64 = if table_exists(&conn, "definitions")? {
        conn.query_row("SELECT COUNT(*) FROM definitions", [], |r| r.get(0))?
    } else {
        0
    };
    let mut types = BTreeMap::new();
    let mut stmt =
        conn.prepare("SELECT type, COUNT(*) AS n FROM documents GROUP BY type ORDER BY n DESC")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (typ, n) = row?;
        types.insert(typ, n);
    }
    // [SW-05] prefix_breakdown is corpus-derived: doc_id-prefix counts plus a
    // sample title per prefix as the description. Replaces the hand-maintained
    // prefix-to-doc-type map; agents read this to discover the canonical
    // ``doc_scope="<PREFIX>/*"`` filter idiom for every prefix in the corpus.
    let prefix_breakdown = collect_prefix_breakdown(&conn)?;
    let semantic_search_ready = ensure_vector_search_ready(&conn).is_ok();
    let payload = json!({
        "data_dir": data_dir()?.display().to_string(),
        "index_version": get_meta(&conn, "index_version")?,
        "last_update_at": get_meta(&conn, "last_update_at")?,
        "embedding_model_id": get_meta(&conn, "embedding_model_id")?,
        "semantic_search_ready": semantic_search_ready,
        "search_modes": ["hybrid", "vector", "keyword"],
        "default_search_mode": "hybrid",
        "documents": docs,
        "chunks": chunks,
        "chunk_embeddings": embeddings,
        "definitions": definitions,
        "types": types,
        "prefix_breakdown": prefix_breakdown,
        "default_search_policy": {
            "excluded_types": DEFAULT_EXCLUDED_TYPES,
            "excluded_type_labels": [EDITED_PRIVATE_ADVICE_LABEL],
            "old_content_cutoff": OLD_CONTENT_CUTOFF,
            "old_content_exception_types": LEGISLATION_TYPE_PREFIXES,
            "old_content_exception_type_labels": [LEGISLATION_TYPE],
        },
        "austlii": crate::cookies::session_summary_json(),
    });
    // [OF-06] JSON outputs use serde_json pretty rendering before return/write.
    Ok(serde_json::to_string_pretty(&payload)?)
}

/// Per-prefix corpus breakdown — doc_id-prefix counts plus a sample-title
/// description. Replaces the hand-maintained prefix-to-doc-type yaml: the only
/// signal we trust is the corpus itself.
///
/// The description is the leading segment of the first sample title (the part
/// before ` — ` when present, otherwise the full title), since titles for many
/// ATO doc types don't carry a doc-type label at all (cases, sections, etc.).
pub(crate) fn collect_prefix_breakdown(conn: &rusqlite::Connection) -> Result<Vec<JsonValue>> {
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
              WHEN INSTR(doc_id, '/') > 0
                THEN UPPER(SUBSTR(doc_id, 1, INSTR(doc_id, '/') - 1))
              ELSE UPPER(doc_id)
            END AS prefix,
            title,
            doc_id
          FROM documents
        ),
        windowed AS (
          SELECT
            prefix,
            title,
            doc_id,
            COUNT(*) OVER (PARTITION BY prefix) AS doc_count,
            ROW_NUMBER() OVER (
              PARTITION BY prefix
              ORDER BY
                CASE WHEN title LIKE prefix || ' %' THEN 1 ELSE 0 END,
                doc_id
            ) AS rn
          FROM ranked
        )
        SELECT prefix, doc_count, title
        FROM windowed
        WHERE rn = 1
        ORDER BY doc_count DESC, prefix ASC
        "#,
    )?;
    let rows = stmt.query_map([], |row| {
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub(crate) schema_version: i64,
    pub(crate) index_version: String,
    pub(crate) created_at: String,
    pub(crate) min_client_version: String,
    pub(crate) model: ModelInfo,
    pub(crate) db: ManifestDb,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestDb {
    pub(crate) url: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelInfo {
    pub(crate) id: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
    pub(crate) url: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct UpdateStats {
    pub(crate) bytes_downloaded: u64,
}

pub(crate) fn apply_update(manifest_url: &str) -> Result<UpdateStats> {
    // [UM-01] apply_update holds the app LOCK around all install/update mutation.
    let lock = lock_file()?;
    let result = apply_update_locked(manifest_url);
    lock.unlock()?;
    result
}

/// Reject a manifest whose `schema_version` is not the current release
/// format, or whose `min_client_version` is newer than the currently-running
/// binary.
pub(crate) fn enforce_manifest_compatibility(manifest: &Manifest) -> Result<()> {
    let schema_version = manifest.schema_version;
    if schema_version < 0 {
        bail!("manifest schema_version is negative ({schema_version}); manifest is malformed");
    }
    let schema_version = schema_version as u32;
    if schema_version != SUPPORTED_SCHEMA_VERSION {
        bail!(
            "manifest schema_version {schema_version} is not supported by this binary (expects {SUPPORTED_SCHEMA_VERSION}); install a matching ato-mcp release"
        );
    }
    let min = manifest.min_client_version.trim();
    if !min.is_empty() {
        let current = env!("CARGO_PKG_VERSION");
        if cmp_dotted_version(min, current).is_gt() {
            bail!(
                "manifest requires ato-mcp >= {min}, but this binary is {current}; please upgrade the ato-mcp binary"
            );
        }
    }
    validate_manifest_model_source(&manifest.model)?;
    Ok(())
}

/// Compare two dotted version strings (`a.b.c[-suffix]`) by their numeric
/// components only. Returns `Ordering::Less/Equal/Greater` for the first
/// arg relative to the second. Pre-release suffixes are ignored.
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

pub(crate) fn apply_update_locked(manifest_url: &str) -> Result<UpdateStats> {
    // Full corpus replacement every time: rebuilding the live DB wholesale
    // through a staging file and atomic rename is faster than mutating the
    // multi-GB live DB and avoids FK cascades wiping the citations table
    // mid-update.
    let manifest_context = UrlContext::from_manifest_url(manifest_url);
    let staging = staging_dir()?;
    let manifest_bytes = fetch_bytes(manifest_url, &manifest_context)
        .with_context(|| format!("fetching manifest from {manifest_url}"))?;
    let new_manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
    enforce_manifest_compatibility(&new_manifest)?;

    let update_root = staging.join("update-apply");
    if update_root.exists() {
        fs::remove_dir_all(&update_root)?;
    }
    fs::create_dir_all(&update_root)?;
    let staged_model = stage_model(
        &new_manifest,
        &manifest_context,
        &update_root.join("model-stage"),
    )?;
    let staged_corpus = stage_live_db_from_db_artifact(
        &new_manifest,
        &manifest_context,
        manifest_bytes.len() as u64,
        &update_root.join("corpus-rebuild"),
    )?;
    let stats = staged_corpus.stats;
    promote_staged_update(staged_model.as_ref(), staged_corpus, &new_manifest)?;
    let _ = fs::remove_dir_all(&update_root);
    Ok(stats)
}

#[derive(Debug)]
pub(crate) struct StagedModel {
    pub(crate) dir: PathBuf,
    pub(crate) marker_value: String,
}

pub(crate) struct ModelPromotionGuard {
    pub(crate) backup_dir: PathBuf,
    pub(crate) marker_value: String,
    pub(crate) active: bool,
}

impl ModelPromotionGuard {
    fn write_marker(&self) -> Result<()> {
        fs::write(model_marker_path()?, &self.marker_value)?;
        Ok(())
    }

    fn commit(mut self) {
        self.active = false;
        let _ = fs::remove_dir_all(&self.backup_dir);
    }
}

impl Drop for ModelPromotionGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = restore_model_backup(&self.backup_dir);
        }
    }
}

pub(crate) struct PathPromotionGuard {
    pub(crate) live_path: PathBuf,
    pub(crate) backup_path: PathBuf,
    pub(crate) had_live: bool,
    pub(crate) active: bool,
}

impl PathPromotionGuard {
    fn backup(live_path: PathBuf, backup_path: PathBuf) -> Result<Self> {
        if let Some(parent) = backup_path.parent() {
            fs::create_dir_all(parent)?;
        }
        remove_path_if_exists(&backup_path)?;
        let had_live = live_path.exists();
        if had_live {
            fs::rename(&live_path, &backup_path).with_context(|| {
                format!(
                    "backing up {} to {}",
                    live_path.display(),
                    backup_path.display()
                )
            })?;
        }
        Ok(Self {
            live_path,
            backup_path,
            had_live,
            active: true,
        })
    }

    fn commit(mut self) {
        self.active = false;
        let _ = remove_path_if_exists(&self.backup_path);
    }
}

impl Drop for PathPromotionGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _ = remove_path_if_exists(&self.live_path);
        if self.had_live && self.backup_path.exists() {
            let _ = fs::rename(&self.backup_path, &self.live_path);
        }
    }
}

pub(crate) fn remove_path_if_exists(path: &Path) -> Result<()> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if meta.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub(crate) struct StagedCorpusUpdate {
    pub(crate) staging_root: PathBuf,
    pub(crate) staged_db: PathBuf,
    pub(crate) stats: UpdateStats,
}

pub(crate) fn promote_staged_update(
    staged_model: Option<&StagedModel>,
    staged_corpus: StagedCorpusUpdate,
    manifest: &Manifest,
) -> Result<()> {
    let model_guard = match staged_model {
        Some(model) => Some(promote_staged_model_files(
            model,
            &staged_corpus.staging_root.join("model-backup"),
        )?),
        None => None,
    };
    let db_guard = promote_live_db(
        &staged_corpus.staged_db,
        &staged_corpus.staging_root.join("ato.db.backup"),
    )?;
    let manifest_guard = promote_installed_manifest(
        manifest,
        &staged_corpus
            .staging_root
            .join("installed_manifest.json.backup"),
    )?;
    if let Some(guard) = model_guard.as_ref() {
        guard.write_marker()?;
    }
    if let Some(guard) = model_guard {
        guard.commit();
    }
    manifest_guard.commit();
    db_guard.commit();
    let _ = fs::remove_dir_all(&staged_corpus.staging_root);
    Ok(())
}

pub(crate) fn live_model_file_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = EMBEDDING_MODEL_HF_FILES
        .iter()
        .map(|file| file.output_name)
        .collect();
    names.push(".model.sha256");
    names
}

pub(crate) fn backup_live_model_files(backup_dir: &Path) -> Result<()> {
    if backup_dir.exists() {
        fs::remove_dir_all(backup_dir)?;
    }
    fs::create_dir_all(backup_dir)?;
    let live = live_dir()?;
    for name in live_model_file_names() {
        let src = live.join(name);
        if src.exists() {
            fs::copy(&src, backup_dir.join(name))
                .with_context(|| format!("backing up {}", src.display()))?;
        }
    }
    Ok(())
}

pub(crate) fn restore_model_backup(backup_dir: &Path) -> Result<()> {
    let live = live_dir()?;
    for name in live_model_file_names() {
        let dest = live.join(name);
        if dest.exists() {
            fs::remove_file(&dest).with_context(|| format!("removing {}", dest.display()))?;
        }
        let backup = backup_dir.join(name);
        if backup.exists() {
            fs::copy(&backup, &dest).with_context(|| format!("restoring {}", dest.display()))?;
        }
    }
    Ok(())
}

pub(crate) fn promote_staged_model_files(
    staged: &StagedModel,
    backup_dir: &Path,
) -> Result<ModelPromotionGuard> {
    backup_live_model_files(backup_dir)?;
    let live = live_dir()?;
    for file in EMBEDDING_MODEL_HF_FILES {
        let src = staged.dir.join(file.output_name);
        if !src.is_file() {
            bail!("staged model missing {}", src.display());
        }
        let dest = live.join(file.output_name);
        if dest.exists() {
            fs::remove_file(&dest)?;
        }
        fs::copy(&src, &dest)
            .with_context(|| format!("promoting model file {}", dest.display()))?;
    }
    Ok(ModelPromotionGuard {
        backup_dir: backup_dir.to_path_buf(),
        marker_value: staged.marker_value.clone(),
        active: true,
    })
}


/// Install path for manifest schema 5+: download the single zstd-compressed
/// SQLite artifact, decompress to staging, rebuild FTS5 indexes locally from
/// the chunks/documents already in the DB, and emit a StagedCorpusUpdate
/// the caller's promotion guards can swap into live.
pub(crate) fn stage_live_db_from_db_artifact(
    manifest: &Manifest,
    context: &UrlContext,
    manifest_bytes: u64,
    staging_root: &Path,
) -> Result<StagedCorpusUpdate> {
    let db_info = &manifest.db;

    if staging_root.exists() {
        fs::remove_dir_all(staging_root)?;
    }
    fs::create_dir_all(staging_root)?;
    let staged_db = staging_root.join("ato.db");

    // Download ato.db.zst and verify size + sha256.
    let compressed = fetch_bytes(&db_info.url, context)
        .with_context(|| format!("downloading {}", db_info.url))?;
    if compressed.len() as u64 != db_info.size {
        bail!(
            "size mismatch for {}: expected {}, got {}",
            db_info.url,
            db_info.size,
            compressed.len()
        );
    }
    let actual_sha = format!("{:x}", Sha256::digest(&compressed));
    if actual_sha != db_info.sha256 {
        bail!(
            "sha256 mismatch for {}: expected {}, got {}",
            db_info.url,
            db_info.sha256,
            actual_sha
        );
    }
    let bytes_downloaded = manifest_bytes + compressed.len() as u64;

    // Decompress to staged DB.
    {
        let mut input = Cursor::new(compressed);
        let mut output = File::create(&staged_db)
            .with_context(|| format!("creating {}", staged_db.display()))?;
        zstd::stream::copy_decode(&mut input, &mut output)
            .with_context(|| format!("decompressing into {}", staged_db.display()))?;
    }

    // Open writable and rebuild FTS5 indexes. We register a `zstd_decompress`
    // scalar UDF so the chunks_fts repopulation can run as a single SQL
    // INSERT … SELECT rather than 467 k Rust↔SQLite round-trips.
    let conn = open_write_at(&staged_db)?;
    conn.create_scalar_function(
        "zstd_decompress",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| -> rusqlite::Result<String> {
            let blob: Vec<u8> = ctx.get(0)?;
            let bytes = zstd::stream::decode_all(Cursor::new(blob))
                .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
            String::from_utf8(bytes)
                .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))
        },
    )
    .context("registering zstd_decompress scalar function")?;

    conn.execute_batch(
        r#"
        CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
            text,
            tokenize = 'porter unicode61 remove_diacritics 2'
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS title_fts USING fts5(
            doc_id UNINDEXED, title, headings,
            tokenize = 'porter unicode61 remove_diacritics 2'
        );
        DELETE FROM chunks_fts;
        DELETE FROM title_fts;
        INSERT INTO chunks_fts(rowid, text)
            SELECT chunk_id, zstd_decompress(text) FROM chunks;
        INSERT INTO title_fts(doc_id, title, headings)
            SELECT doc_id, title, headings FROM documents;
        "#,
    )
    .context("rebuilding FTS5 indexes on staged DB")?;

    set_meta(&conn, "index_version", &manifest.index_version)?;
    set_meta(&conn, "embedding_model_id", &manifest.model.id)?;
    set_meta(&conn, "last_update_at", &Utc::now().to_rfc3339())?;

    verify_semantic_install(&conn, manifest)?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    drop(conn);

    Ok(StagedCorpusUpdate {
        staging_root: staging_root.to_path_buf(),
        staged_db,
        stats: UpdateStats { bytes_downloaded },
    })
}

pub(crate) fn promote_live_db(staged_db: &Path, backup: &Path) -> Result<PathPromotionGuard> {
    let live = live_dir()?;
    let db = db_path()?;
    for suffix in ["-wal", "-shm"] {
        let path = live.join(format!("ato.db{suffix}"));
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    let guard = PathPromotionGuard::backup(db.clone(), backup.to_path_buf())?;
    fs::rename(staged_db, &db)
        .with_context(|| format!("promoting staged DB to {}", db.display()))?;
    Ok(guard)
}

pub(crate) fn promote_installed_manifest(manifest: &Manifest, backup: &Path) -> Result<PathPromotionGuard> {
    let path = installed_manifest_path()?;
    let guard = PathPromotionGuard::backup(path.clone(), backup.to_path_buf())?;
    let tmp = path.with_extension("json.tmp");
    remove_path_if_exists(&tmp)?;
    fs::write(&tmp, serde_json::to_vec_pretty(manifest)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(guard)
}


pub(crate) fn verify_semantic_install(conn: &Connection, manifest: &Manifest) -> Result<()> {
    if manifest.model.id != EMBEDDING_MODEL_ID {
        return Ok(());
    }
    let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;
    let embeddings = chunk_embedding_count(conn)?;
    if embeddings != chunks {
        bail!(
            "semantic corpus install incomplete: chunk_embeddings={embeddings}, chunks={chunks}; rebuild the release packs with embedding_b64"
        );
    }
    Ok(())
}

pub(crate) fn chunk_embedding_count(conn: &Connection) -> Result<i64> {
    if table_exists(conn, "chunk_embeddings")? {
        conn.query_row("SELECT COUNT(*) FROM chunk_embeddings", [], |row| {
            row.get(0)
        })
        .map_err(Into::into)
    } else {
        Ok(0)
    }
}

pub(crate) fn load_installed_manifest() -> Result<Option<Manifest>> {
    let path = installed_manifest_path()?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

/// Notice surfaced to the agent via `server_instructions` when the
/// published corpus is newer than the installed one. Carries the published
/// `index_version` so the agent can mention it to the user when suggesting
/// `ato-mcp update`.
pub(crate) struct UpdateAvailability {
    pub(crate) available_index_version: String,
}

pub(crate) fn http_probe_client() -> Result<Client> {
    // Tight budget: this client runs synchronously inside `serve` startup.
    // A slow network must not stall the startup banner — `serve` falls
    // through to no-notice if the probe doesn't complete in time.
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(5))
        .build()?)
}

pub(crate) fn fetch_bytes_probe(url_or_path: &str, context: &UrlContext) -> Result<Vec<u8>> {
    fetch_bytes_with(url_or_path, context, &http_probe_client()?)
}

/// Non-mutating availability probe. Returns `Some(UpdateAvailability)` only
/// when (a) an installed manifest is present, (b) the published `manifest.json`
/// is reachable inside the probe timeout, (c) it parses, (d) this binary can
/// still ingest it, and (e) its `index_version` differs from the installed
/// corpus. Every other case collapses to `Ok(None)` — no error path that
/// could stall serve startup. A published index that requires a newer binary
/// also returns `None` rather than emitting an "update available" notice the
/// user could not act on; the next manual `ato-mcp update` will surface the
/// real upgrade-the-binary error.
pub(crate) fn check_for_update_availability(manifest_url: &str) -> Result<Option<UpdateAvailability>> {
    let Some(installed) = load_installed_manifest()? else {
        return Ok(None);
    };
    let context = UrlContext::from_manifest_url(manifest_url);
    let manifest_bytes = match fetch_bytes_probe(manifest_url, &context) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let manifest: Manifest = match serde_json::from_slice(&manifest_bytes) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if enforce_manifest_compatibility(&manifest).is_err() {
        return Ok(None);
    }
    if installed.index_version == manifest.index_version {
        return Ok(None);
    }
    Ok(Some(UpdateAvailability {
        available_index_version: manifest.index_version,
    }))
}

// ----- tree-crawl (port of src/ato_mcp/scraper/tree_crawler.py + snapshot.py) -----

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

pub(crate) fn fetch_nodes_blocking(
    client: &reqwest::blocking::Client,
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
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("fetching {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("ATO API returned HTTP {status} for {url}");
    }
    let body = resp.text()?;
    let payload: JsonValue = serde_json::from_str(&body).context("parsing ATO API JSON")?;
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

    // [SS-03] Maintainer ATO API pacing is serialized through this mutex so
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

// ----- snapshot-reduce (port of src/ato_mcp/scraper/reducer.py) -----

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
    // [SS-07] Reduction dedupes canonical IDs, chooses a representative
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

// ----- link-download (port of src/ato_mcp/scraper/downloader.py) -----

pub(crate) struct LinkDownloadArgs {
    pub(crate) deduped_links: PathBuf,
    pub(crate) out_dir: PathBuf,
    pub(crate) base_url: String,
    pub(crate) request_delay_seconds: f64,
    pub(crate) max_workers: usize,
    pub(crate) timeout_seconds: f64,
    pub(crate) force: bool,
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

pub(crate) fn build_payload_path(out_dir: &Path, link: &JsonValue) -> PathBuf {
    let payload_dir = out_dir.join("payloads");
    let mut dir = payload_dir;
    // [SS-06] Catch-up/download payload paths inherit representative_path
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
    dir.join(filename)
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

pub(crate) fn link_download(args: LinkDownloadArgs) -> Result<()> {
    use std::io::BufRead as _;
    use std::sync::{Arc, Mutex};

    let payload_dir = args.out_dir.join("payloads");
    let index_path = args.out_dir.join("index.jsonl");
    fs::create_dir_all(&payload_dir)?;

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
        let f = File::open(&index_path)?;
        let reader = std::io::BufReader::new(f);
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let rec: JsonValue = serde_json::from_str(trimmed)?;
            if let Some(cid) = rec.get("canonical_id").and_then(|v| v.as_str()) {
                index.insert(cid.to_string(), rec);
            }
        }
    }
    let initial_completed = index
        .values()
        .filter(|r| r.get("status").and_then(|v| v.as_str()) == Some("success"))
        .count();
    if initial_completed > 0 {
        eprintln!("link-download: resuming with {initial_completed} previously completed");
    }
    let index = Arc::new(Mutex::new(index));

    let client = Arc::new(
        reqwest::blocking::Client::builder()
            .user_agent(ATO_USER_AGENT)
            .timeout(Duration::from_secs_f64(args.timeout_seconds))
            .build()?,
    );

    let last_request = Arc::new(Mutex::new(
        std::time::Instant::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(std::time::Instant::now),
    ));
    let request_delay = args.request_delay_seconds;

    // [SS-08] Link-download fans out over worker threads with a shared queue,
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

    let mut handles = Vec::with_capacity(args.max_workers);
    for worker_id in 0..args.max_workers {
        let work_queue = Arc::clone(&work_queue);
        let client = Arc::clone(&client);
        let last_request = Arc::clone(&last_request);
        let index = Arc::clone(&index);
        let index_writer = Arc::clone(&index_writer);
        let stats_completed = Arc::clone(&stats_completed);
        let stats_errors = Arc::clone(&stats_errors);
        let stats_skipped = Arc::clone(&stats_skipped);
        let base_url = args.base_url.clone();
        let out_dir = args.out_dir.clone();
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

                let payload_path = build_payload_path(&out_dir, &link);

                // Skip if already done.
                if !force {
                    let already_done = {
                        let idx = index.lock().unwrap();
                        idx.get(&canonical_id)
                            .and_then(|r| r.get("status").and_then(|v| v.as_str()))
                            == Some("success")
                    };
                    if already_done {
                        stats_skipped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        continue;
                    }
                    if payload_path.exists() {
                        // Orphan payload — emit synthetic success.
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
                            "http_status": null,
                            "downloaded_at": now,
                        });
                        {
                            use std::io::Write as _;
                            let mut idx = index.lock().unwrap();
                            idx.insert(canonical_id.clone(), rec.clone());
                            let mut w = index_writer.lock().unwrap();
                            writeln!(w, "{}", serde_json::to_string(&rec)?)?;
                        }
                        stats_completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        continue;
                    }
                }

                // Rate limit.
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

                let resp = client.get(&url).send();
                let (http_status, html) = match resp {
                    Ok(r) => {
                        let status = r.status();
                        if !status.is_success() {
                            eprintln!(
                                "link-download w{worker_id}: HTTP {status} for {canonical_id}"
                            );
                            stats_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let now = chrono::Utc::now().to_rfc3339();
                            let rec = json!({
                                "canonical_id": canonical_id,
                                "href": href,
                                "status": "failed",
                                "payload_path": null,
                                "error": format!("HTTP {status}"),
                                "http_status": status.as_u16(),
                                "downloaded_at": now,
                            });
                            {
                                use std::io::Write as _;
                                let mut idx = index.lock().unwrap();
                                idx.insert(canonical_id.clone(), rec.clone());
                                let mut w = index_writer.lock().unwrap();
                                writeln!(w, "{}", serde_json::to_string(&rec)?)?;
                            }
                            continue;
                        }
                        (status.as_u16(), r.text().unwrap_or_default())
                    }
                    Err(e) => {
                        eprintln!("link-download w{worker_id}: failed {canonical_id}: {e}");
                        stats_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let now = chrono::Utc::now().to_rfc3339();
                        let rec = json!({
                            "canonical_id": canonical_id,
                            "href": href,
                            "status": "failed",
                            "payload_path": null,
                            "error": e.to_string(),
                            "http_status": null,
                            "downloaded_at": now,
                        });
                        {
                            use std::io::Write as _;
                            let mut idx = index.lock().unwrap();
                            idx.insert(canonical_id.clone(), rec.clone());
                            let mut w = index_writer.lock().unwrap();
                            writeln!(w, "{}", serde_json::to_string(&rec)?)?;
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
                        let rec = json!({
                            "canonical_id": canonical_id,
                            "href": href,
                            "status": "missing_content",
                            "payload_path": null,
                            "error": "lawContents div not found",
                            "http_status": http_status,
                            "downloaded_at": now,
                        });
                        {
                            use std::io::Write as _;
                            let mut idx = index.lock().unwrap();
                            idx.insert(canonical_id.clone(), rec.clone());
                            let mut w = index_writer.lock().unwrap();
                            writeln!(w, "{}", serde_json::to_string(&rec)?)?;
                        }
                        continue;
                    }
                };

                if let Some(parent) = payload_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&payload_path, &snippet)?;

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
                });
                {
                    use std::io::Write as _;
                    let mut idx = index.lock().unwrap();
                    idx.insert(canonical_id.clone(), rec.clone());
                    let mut w = index_writer.lock().unwrap();
                    writeln!(w, "{}", serde_json::to_string(&rec)?)?;
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

    for h in handles {
        h.join().expect("worker panic")?;
    }

    // Atomic rewrite of index.jsonl with sorted entries.
    let idx = index.lock().unwrap();
    let mut keys: Vec<&String> = idx.keys().collect();
    keys.sort();
    let tmp_path = index_path.with_extension("jsonl.tmp");
    let mut tmp = File::create(&tmp_path)?;
    for k in keys {
        use std::io::Write as _;
        writeln!(tmp, "{}", serde_json::to_string(&idx[k])?)?;
    }
    fs::rename(&tmp_path, &index_path)?;

    // metadata.json.
    let now = chrono::Utc::now().to_rfc3339();
    let metadata = json!({
        "links_file": args.deduped_links.to_string_lossy(),
        "download_started_at": now,
        "download_completed_at": now,
        "total_links": total,
        "completed_links": stats_completed.load(std::sync::atomic::Ordering::Relaxed),
    });
    fs::write(
        args.out_dir.join("metadata.json"),
        serde_json::to_vec_pretty(&metadata)?,
    )?;

    eprintln!(
        "link-download: done — {} success, {} errors, {} skipped (out_dir={})",
        stats_completed.load(std::sync::atomic::Ordering::Relaxed),
        stats_errors.load(std::sync::atomic::Ordering::Relaxed),
        stats_skipped.load(std::sync::atomic::Ordering::Relaxed),
        args.out_dir.display(),
    );
    Ok(())
}

// ----- scrape-diff (port of pipeline.py incremental + catch_up diff steps) -----

pub(crate) fn representative_path_from_docid(
    canonical_id: &str,
    title: &str,
    heading: Option<&str>,
) -> Vec<String> {
    // Mirrors src/ato_mcp/indexer/metadata.py:representative_path_from_docid.
    // Falls back to ['Other'] when nothing better can be determined.
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
    // like /law/view/document?docid=CRP%2FCRP19%2FCR. The full Python
    // version walks docid prefixes against a 200-row mapping table; this
    // covers the dozen most common buckets the maintainer pipeline cares
    // about. Anything unrecognised falls through to "Other" so the
    // downloader still has a folder to write to.
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
    let mut out = std::collections::HashSet::new();
    if !index_path.exists() {
        return Ok(out);
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
        if let Some(cid) = rec.get("canonical_id").and_then(|v| v.as_str()) {
            let normalised = normalize_doc_href(cid);
            if !normalised.is_empty() {
                out.insert(normalised);
            }
        }
    }
    Ok(out)
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
        let client = reqwest::blocking::Client::builder()
            .user_agent(ATO_USER_AGENT)
            .timeout(Duration::from_secs(30))
            .build()?;
        let resp = client
            .get(url)
            .send()
            .with_context(|| format!("fetching {url}"))?;
        if !resp.status().is_success() {
            bail!("HTTP {} fetching {}", resp.status(), url);
        }
        let html = resp.text()?;
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
