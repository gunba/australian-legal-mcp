//! MCP retrieval tools: `fetch` (URI-scheme dispatcher for live document
//! fetching), `get_chunks` (progressive disclosure), `get_asset`,
//! `get_doc_anchors`, `get_definition` (+ ordinary-meaning dictionary), plus
//! the `derive_citations` build helper and `load_cited_by` reader.

use crate::chunker::{chunk_html, EMBED_MAX_TOKENS};
use crate::config::data_dir;
use crate::db::{canonical_url, decompress_text, open_read, table_exists};
use crate::extract::{extract_anchors, normalize_definition_term};
use crate::html::clean_ato_html;
use crate::uri::{parse_doc_uri, DocUri};
use crate::{
    fetch_bytes, optional_usize, required_str,
    UrlContext, ATO_FETCH_TIMEOUT, ATO_USER_AGENT, OEWN_2024_SOURCE, OEWN_2024_URL,
    ORDINARY_DICTIONARY_PATH_ENV, STATUTORY_DEFINITION_TYPE_PREFIXES,
};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use regex::Regex;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use zip::ZipArchive;

pub(crate) const CITED_BY_LIMIT: usize = 100;

/// Cached set of every doc_id in the local corpus. Loaded once per process
/// from `documents.doc_id` so the `[doc:X]` annotation pass on chunk hydration
/// stays O(text length) instead of paying a SQL round-trip per marker.
static CORPUS_DOC_IDS: OnceLock<HashSet<String>> = OnceLock::new();

/// `[doc:X]` marker regex. Captures the doc_id (up to whitespace, `]`, or
/// `@` for the PiT/view qualifier separator) and the trailing qualifier
/// segment up to the closing `]`. Shared by `derive_citations` (build-time
/// citation extraction) and `annotate_doc_refs` (read-time annotation).
fn doc_marker_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[doc:([^\s\]@]+)([^\]]*)\]").expect("valid regex"))
}

/// Lazily load every doc_id in `documents` into a set. ~150k strings of
/// ~25 chars ≈ a few MB; amortised across the process lifetime.
pub(crate) fn corpus_doc_id_set() -> Result<&'static HashSet<String>> {
    if let Some(set) = CORPUS_DOC_IDS.get() {
        return Ok(set);
    }
    let conn = open_read()?;
    let mut stmt = conn.prepare("SELECT doc_id FROM documents")?;
    let mut set = HashSet::new();
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        set.insert(row?);
    }
    Ok(CORPUS_DOC_IDS.get_or_init(|| set))
}

/// Translate an ATO `[doc:X<tail>]` marker tail (everything captured between
/// the doc_id and the closing `]`) into the equivalent `?pit=...&view=...`
/// query-string suffix for an `ato:` URI. Empty input returns an empty
/// string. Unrecognised tail shapes return an empty suffix — the marker
/// still carries the external signal via its `fetch:` prefix even if the
/// qualifier information can't be encoded into the URI.
///
/// Recognised tail shapes (from real ATO chunk markers):
///   - `""`                                    → `""`
///   - `"@<pit>"`                              → `"?pit=<pit>"`
///   - `" view=<v>"`                           → `"?view=<v>"`
///   - `"@<pit> view=<v>"`                     → `"?pit=<pit>&view=<v>"`
pub(crate) fn ato_marker_tail_to_query_suffix(tail: &str) -> String {
    let trimmed = tail.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut pit: Option<&str> = None;
    let mut view: Option<&str> = None;
    let remainder = if let Some(after_at) = trimmed.strip_prefix('@') {
        let (pit_val, rest) = after_at.split_once(' ').unwrap_or((after_at, ""));
        pit = Some(pit_val);
        rest.trim()
    } else {
        trimmed
    };
    if !remainder.is_empty() {
        if let Some(v) = remainder.strip_prefix("view=") {
            view = Some(v.trim());
        } else {
            // Unknown qualifier shape; drop it so the rewritten marker is
            // still a syntactically valid URI form.
            return String::new();
        }
    }
    let mut parts = Vec::new();
    if let Some(p) = pit.filter(|s| !s.is_empty()) {
        parts.push(format!("pit={p}"));
    }
    if let Some(v) = view.filter(|s| !s.is_empty()) {
        parts.push(format!("view={v}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

/// Rewrite `[doc:X<tail>]` markers whose target is not in the local corpus
/// into `[fetch:ato:X<query>]`. The new marker self-describes the retrieval
/// path:
///   - `[doc:X]`        → in corpus; use `get_chunks` / `get_doc_anchors`.
///   - `[fetch:ato:X]`  → external; use the `fetch` tool.
///
/// Self-references (target == current doc_id) and markers whose target IS
/// in the corpus are left as-is. The regex only matches the `[doc:...]`
/// shape, so already-rewritten `[fetch:...]` markers are inherently
/// idempotent — they don't match and pass through untouched.
pub(crate) fn annotate_doc_refs(text: &str, self_doc_id: &str) -> Result<String> {
    Ok(annotate_doc_refs_with(text, self_doc_id, corpus_doc_id_set()?))
}

/// Pure helper: same as `annotate_doc_refs` but takes the doc-id set
/// explicitly so unit tests don't need a live DB.
pub(crate) fn annotate_doc_refs_with(
    text: &str,
    self_doc_id: &str,
    corpus: &HashSet<String>,
) -> String {
    let re = doc_marker_regex();
    re.replace_all(text, |caps: &regex::Captures<'_>| {
        let doc_id = &caps[1];
        let tail = &caps[2];
        if doc_id == self_doc_id || corpus.contains(doc_id) {
            return caps[0].to_string();
        }
        let query = ato_marker_tail_to_query_suffix(tail);
        format!("[fetch:ato:{doc_id}{query}]")
    })
    .into_owned()
}

/// Public entry point for the `fetch` MCP tool. Parses the URI scheme and
/// dispatches to the per-source live-fetch implementation. Returns a JSON
/// string with the shape `{uri, canonical_url, title, source, ocr_used,
/// chunks}` regardless of source so callers don't branch on the scheme.
pub(crate) fn fetch(uri_string: &str, allow_ocr: bool) -> Result<String> {
    let uri = parse_doc_uri(uri_string)?;
    match uri {
        DocUri::Ato { doc_id, pit, view } => fetch_ato_doc(
            &doc_id,
            pit.as_deref(),
            view.as_deref(),
            allow_ocr,
        ),
        DocUri::Austlii { path } => crate::austlii::fetch_austlii_doc(&path, allow_ocr),
    }
}

/// Live-fetch an ATO document outside the local corpus. Returns the same
/// `{uri, canonical_url, title, source, ocr_used, chunks}` shape as other
/// `fetch` paths so callers don't need to branch on the scheme.
pub(crate) fn fetch_ato_doc(
    doc_id: &str,
    pit: Option<&str>,
    view: Option<&str>,
    _allow_ocr: bool,
) -> Result<String> {
    let mut url = format!("https://www.ato.gov.au/law/view/document?docid={}", doc_id);
    if let Some(p) = pit.filter(|s| !s.is_empty()) {
        url.push_str(&format!("&PiT={}", p));
    }
    if let Some(v) = view.filter(|s| !s.is_empty()) {
        url.push_str(&format!("&db={}", v));
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent(ATO_USER_AGENT)
        .timeout(ATO_FETCH_TIMEOUT)
        // ATO's public URL bounces some doc forms (subdivisions, division
        // index pages) to an internal hostname we can't resolve. Disable
        // automatic redirect-following so we can detect this and report
        // cleanly rather than failing with a confusing DNS error.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building HTTP client")?;
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("fetching {url}"))?;
    let status = resp.status();
    if status.is_redirection() {
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<unknown>")
            .to_string();
        bail!(
            "ATO redirected `{}` to `{}` — that target is on ATO's internal \
             SPA and is not directly fetchable from outside their network. \
             Try a more specific id (e.g. an indexed section) or search the \
             local corpus for the surrounding context instead.",
            url,
            location
        );
    }
    if !status.is_success() {
        bail!("ATO returned HTTP {status} for {url}");
    }
    let html = resp.text().context("reading response body")?;
    let cleaned = clean_ato_html(&html);
    if cleaned.html.trim().is_empty() {
        bail!(
            "no content container found in ATO response for {url} — page \
             structure may have changed"
        );
    }
    // Run the cleaned HTML through the same deterministic block-aware
    // chunker the build pipeline uses, so an external doc reads like a
    // corpus doc: a list of {ord, anchor, text} chunks. Stateless — the
    // chunks aren't persisted and carry no chunk_id; all of them come
    // back inline in this one response.
    let chunks = chunk_html(&cleaned.html, cleaned.title.as_deref(), EMBED_MAX_TOKENS);
    let corpus = corpus_doc_id_set()?;
    let chunk_json: Vec<JsonValue> = chunks
        .iter()
        .map(|c| {
            json!({
                "ord": c.ord,
                "anchor": c.anchor,
                "text": annotate_doc_refs_with(&c.text, doc_id, corpus),
            })
        })
        .collect();
    let canonical_uri = DocUri::Ato {
        doc_id: doc_id.to_string(),
        pit: pit.map(str::to_string),
        view: view.map(str::to_string),
    }
    .to_uri_string();
    Ok(serde_json::to_string_pretty(&json!({
        "uri": canonical_uri,
        "canonical_url": url,
        "title": cleaned.title,
        "source": "live",
        "ocr_used": false,
        "chunks": chunk_json,
    }))?)
}

pub(crate) struct GetDefinitionOptions<'a> {
    pub(crate) context_doc_id: Option<&'a str>,
    pub(crate) max_defs: usize,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct DefinitionSource {
    pub(crate) doc_id: String,
    pub(crate) title: String,
    #[serde(rename = "type")]
    pub(crate) source_type: String,
    pub(crate) scope: Option<String>,
    pub(crate) anchor: Option<String>,
    pub(crate) canonical_url: String,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct DefinitionHit {
    pub(crate) definition_id: String,
    pub(crate) term: String,
    pub(crate) kind: String,
    pub(crate) body: String,
    pub(crate) source: DefinitionSource,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct OrdinaryDefinition {
    pub(crate) part_of_speech: Option<String>,
    pub(crate) definition: String,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct OrdinaryMeaningHit {
    pub(crate) term: String,
    pub(crate) kind: String,
    pub(crate) source: String,
    pub(crate) definitions: Vec<OrdinaryDefinition>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DictionaryEntry {
    pub(crate) term: String,
    pub(crate) definition: String,
    #[serde(default)]
    pub(crate) source: Option<String>,
    #[serde(default)]
    pub(crate) part_of_speech: Option<String>,
}

pub(crate) fn context_prefix(context_doc_id: Option<&str>) -> Option<String> {
    let doc_id = context_doc_id?;
    let mut parts = doc_id.split('/');
    let first = parts.next()?;
    let second = parts.next()?;
    if first == "PAC" {
        Some(format!("{first}/{second}"))
    } else {
        None
    }
}

pub(crate) fn definition_rank(hit: &DefinitionHit, opts: &GetDefinitionOptions<'_>) -> usize {
    if opts
        .context_doc_id
        .is_some_and(|doc_id| hit.source.doc_id == doc_id)
    {
        return 0;
    }
    if let Some(prefix) = context_prefix(opts.context_doc_id) {
        if hit.source.doc_id.starts_with(&(prefix + "/")) {
            return 1;
        }
    }
    2
}

pub(crate) fn get_definition(term: &str, opts: GetDefinitionOptions<'_>) -> Result<String> {
    let conn = open_read()?;
    if !table_exists(&conn, "definitions")? {
        let (ordinary, ordinary_error) = ordinary_meaning_or_error(term);
        return format_definition_response(term, &[], ordinary, ordinary_error, false);
    }
    let norm = normalize_definition_term(term);
    let max_defs = opts.max_defs.clamp(1, 20);
    let source_placeholders = vec!["?"; STATUTORY_DEFINITION_TYPE_PREFIXES.len()].join(",");
    let sql = format!(
        r#"
        SELECT definition_id, term, doc_id, source_title, source_type, scope,
               anchor, body
        FROM definitions
        WHERE norm_term = ? AND source_type IN ({source_placeholders})
        ORDER BY doc_id, ord, term
        LIMIT 100
        "#
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut query_params = Vec::with_capacity(1 + STATUTORY_DEFINITION_TYPE_PREFIXES.len());
    query_params.push(Value::Text(norm));
    for source_type in STATUTORY_DEFINITION_TYPE_PREFIXES {
        query_params.push(Value::Text((*source_type).to_string()));
    }
    let mut hits = stmt
        .query_map(rusqlite::params_from_iter(query_params), |row| {
            let doc_id: String = row.get("doc_id")?;
            Ok(DefinitionHit {
                definition_id: row.get("definition_id")?,
                term: row.get("term")?,
                kind: "statutory".to_string(),
                body: row.get("body")?,
                source: DefinitionSource {
                    canonical_url: canonical_url(&doc_id),
                    doc_id,
                    title: row.get("source_title")?,
                    source_type: row.get("source_type")?,
                    scope: row.get("scope")?,
                    anchor: row.get("anchor")?,
                },
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut seen = HashSet::new();
    hits.retain(|hit| seen.insert((hit.source.doc_id.clone(), hit.body.clone())));
    hits.sort_by_key(|hit| definition_rank(hit, &opts));
    hits.truncate(max_defs);
    let (ordinary, ordinary_error) = if hits.is_empty() {
        ordinary_meaning_or_error(term)
    } else {
        (None, None)
    };
    format_definition_response(term, &hits, ordinary, ordinary_error, true)
}

pub(crate) fn ordinary_meaning_or_error(term: &str) -> (Option<OrdinaryMeaningHit>, Option<String>) {
    match lookup_ordinary_meaning(term) {
        Ok(hit) => (hit, None),
        Err(err) => (None, Some(err.to_string())),
    }
}

pub(crate) fn ordinary_dictionary_dir() -> Result<PathBuf> {
    let path = data_dir()?.join("ordinary-meaning");
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub(crate) fn ordinary_dictionary_index_path() -> Result<PathBuf> {
    Ok(ordinary_dictionary_dir()?.join("open-english-wordnet-2024.tsv"))
}

pub(crate) fn lookup_ordinary_meaning(term: &str) -> Result<Option<OrdinaryMeaningHit>> {
    if let Some(path) = std::env::var_os(ORDINARY_DICTIONARY_PATH_ENV) {
        let path = PathBuf::from(path);
        let source = path.display().to_string();
        return lookup_ordinary_meaning_file(&path, &source, term);
    }
    let path = ensure_oewn_ordinary_dictionary()?;
    lookup_ordinary_meaning_file(&path, OEWN_2024_SOURCE, term)
}

pub(crate) fn ensure_oewn_ordinary_dictionary() -> Result<PathBuf> {
    let index_path = ordinary_dictionary_index_path()?;
    if index_path.exists() {
        return Ok(index_path);
    }
    let context = UrlContext {
        manifest_dir: None,
        manifest_base_url: None,
    };
    let zip_bytes = fetch_bytes(OEWN_2024_URL, &context)
        .with_context(|| format!("fetching ordinary-meaning source from {OEWN_2024_URL}"))?;
    let index = build_oewn_dictionary_index(&zip_bytes)?;
    let tmp_path = index_path.with_extension("tsv.tmp");
    fs::write(&tmp_path, index)?;
    fs::rename(&tmp_path, &index_path)?;
    Ok(index_path)
}

pub(crate) fn build_oewn_dictionary_index(zip_bytes: &[u8]) -> Result<String> {
    let mut archive = ZipArchive::new(Cursor::new(zip_bytes))?;
    let mut rows = Vec::new();
    for (suffix, part_of_speech) in [
        ("data.noun", "noun"),
        ("data.verb", "verb"),
        ("data.adj", "adjective"),
        ("data.adv", "adverb"),
    ] {
        let content = read_zip_member_by_suffix(&mut archive, suffix)?;
        parse_oewn_data_file(&content, part_of_speech, &mut rows);
    }
    rows.sort();
    rows.dedup();
    Ok(rows.join("\n") + "\n")
}

pub(crate) fn read_zip_member_by_suffix(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    suffix: &str,
) -> Result<String> {
    for idx in 0..archive.len() {
        let mut file = archive.by_index(idx)?;
        if file.name().ends_with(suffix) {
            let mut content = String::new();
            file.read_to_string(&mut content)?;
            return Ok(content);
        }
    }
    bail!("ordinary-meaning source is missing {suffix}")
}

pub(crate) fn parse_oewn_data_file(content: &str, part_of_speech: &str, rows: &mut Vec<String>) {
    let mut seen = HashSet::new();
    for line in content.lines() {
        if !line
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_digit())
        {
            continue;
        }
        let Some((record, gloss)) = line.split_once('|') else {
            continue;
        };
        let mut fields = record.split_whitespace();
        let _offset = fields.next();
        let _lex_filenum = fields.next();
        let _ss_type = fields.next();
        let Some(word_count_hex) = fields.next() else {
            continue;
        };
        let Ok(word_count) = usize::from_str_radix(word_count_hex, 16) else {
            continue;
        };
        let definition = clean_ordinary_field(gloss);
        if definition.is_empty() {
            continue;
        }
        for _ in 0..word_count {
            let Some(raw_word) = fields.next() else {
                break;
            };
            let _lex_id = fields.next();
            let term = raw_word.replace('_', " ");
            let norm = normalize_definition_term(&term);
            if norm.is_empty() || !seen.insert((norm.clone(), definition.clone())) {
                continue;
            }
            rows.push(format!(
                "{}\t{}\t{}\t{}",
                clean_ordinary_field(&norm),
                clean_ordinary_field(&term),
                part_of_speech,
                definition
            ));
        }
    }
}

pub(crate) fn clean_ordinary_field(value: &str) -> String {
    value
        .replace(['\t', '\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches('"')
        .to_string()
}

pub(crate) fn lookup_ordinary_meaning_file(
    path: &Path,
    source: &str,
    term: &str,
) -> Result<Option<OrdinaryMeaningHit>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading ordinary-meaning dictionary {}", path.display()))?;
    let wanted = normalize_definition_term(term);
    if let Ok(entries) = serde_json::from_str::<Vec<DictionaryEntry>>(&raw) {
        return Ok(ordinary_from_dictionary_entries(entries, &wanted, source));
    }
    let mut jsonl_entries = Vec::new();
    let mut saw_jsonl = false;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<DictionaryEntry>(line) {
            saw_jsonl = true;
            jsonl_entries.push(entry);
        }
    }
    if saw_jsonl {
        return Ok(ordinary_from_dictionary_entries(
            jsonl_entries,
            &wanted,
            source,
        ));
    }
    ordinary_from_tsv(&raw, &wanted, source)
}

pub(crate) fn ordinary_from_dictionary_entries(
    entries: Vec<DictionaryEntry>,
    wanted: &str,
    default_source: &str,
) -> Option<OrdinaryMeaningHit> {
    let mut definitions = Vec::new();
    let mut source = default_source.to_string();
    for entry in entries {
        if normalize_definition_term(&entry.term) != wanted {
            continue;
        }
        if let Some(entry_source) = entry.source {
            source = entry_source;
        }
        definitions.push(OrdinaryDefinition {
            part_of_speech: entry.part_of_speech,
            definition: entry.definition,
        });
        if definitions.len() >= 5 {
            break;
        }
    }
    if definitions.is_empty() {
        None
    } else {
        Some(OrdinaryMeaningHit {
            term: wanted.to_string(),
            kind: "ordinary".to_string(),
            source,
            definitions,
        })
    }
}

pub(crate) fn ordinary_from_tsv(raw: &str, wanted: &str, source: &str) -> Result<Option<OrdinaryMeaningHit>> {
    let mut definitions = Vec::new();
    for line in raw.lines() {
        let parts: Vec<&str> = line.splitn(4, '\t').collect();
        if parts.len() == 4 && parts[0] == wanted {
            definitions.push(OrdinaryDefinition {
                part_of_speech: Some(parts[2].to_string()),
                definition: parts[3].to_string(),
            });
        } else if parts.len() >= 2 && normalize_definition_term(parts[0]) == wanted {
            definitions.push(OrdinaryDefinition {
                part_of_speech: None,
                definition: parts[1].to_string(),
            });
        }
        if definitions.len() >= 5 {
            break;
        }
    }
    if definitions.is_empty() {
        Ok(None)
    } else {
        Ok(Some(OrdinaryMeaningHit {
            term: wanted.to_string(),
            kind: "ordinary".to_string(),
            source: source.to_string(),
            definitions,
        }))
    }
}

pub(crate) fn format_definition_response(
    term: &str,
    hits: &[DefinitionHit],
    ordinary: Option<OrdinaryMeaningHit>,
    ordinary_error: Option<String>,
    definition_index_available: bool,
) -> Result<String> {
    let statutory_found = !hits.is_empty();
    Ok(serde_json::to_string_pretty(&json!({
        "term": term,
        "statutory_definition_found": statutory_found,
        "definitions": hits,
        "ordinary_meaning": ordinary,
        "meta": {
            "definition_index_available": definition_index_available,
            "ordinary_meaning_error": ordinary_error,
        }
    }))?)
}


pub(crate) fn get_chunks_mcp(args: &JsonValue) -> Result<String> {
    let ids = args
        .get("chunk_ids")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("missing chunk_ids array"))?;
    let chunk_ids = ids
        .iter()
        .map(|v| {
            v.as_i64()
                .ok_or_else(|| anyhow!("chunk_ids must contain integers"))
        })
        .collect::<Result<Vec<_>>>()?;
    get_chunks(
        &chunk_ids,
        GetChunksOptions {
            before: optional_usize(args, "before").unwrap_or(0).min(20),
            after: optional_usize(args, "after").unwrap_or(0).min(20),
            max_chars: optional_usize(args, "max_chars"),
        },
    )
}

pub(crate) struct GetChunksOptions {
    pub(crate) before: usize,
    pub(crate) after: usize,
    pub(crate) max_chars: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HydratedChunk {
    pub(crate) chunk_id: i64,
    pub(crate) requested: bool,
    pub(crate) doc_id: String,
    #[serde(rename = "type")]
    pub(crate) doc_type: String,
    pub(crate) title: String,
    pub(crate) date: Option<String>,
    pub(crate) anchor: Option<String>,
    pub(crate) canonical_url: String,
    pub(crate) text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ChunkPointer {
    pub(crate) chunk_id: i64,
    pub(crate) doc_id: String,
    pub(crate) ord: i64,
}

pub(crate) fn get_chunks(chunk_ids: &[i64], opts: GetChunksOptions) -> Result<String> {
    // [MT-07] get_chunks fetches exact chunk ids, optional neighbours, and truncation next_call.
    if chunk_ids.is_empty() {
        return Ok("_No chunk ids provided._".to_string());
    }
    let conn = open_read()?;
    let placeholders = vec!["?"; chunk_ids.len()].join(",");
    let sql =
        format!("SELECT chunk_id, doc_id, ord FROM chunks WHERE chunk_id IN ({placeholders})");
    let params_vec: Vec<Value> = chunk_ids.iter().map(|id| Value::Integer(*id)).collect();
    let mut stmt = conn.prepare(&sql)?;
    let pointers = stmt
        .query_map(params_from_iter(params_vec), |row| {
            Ok(ChunkPointer {
                chunk_id: row.get("chunk_id")?,
                doc_id: row.get("doc_id")?,
                ord: row.get("ord")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|pointer| (pointer.chunk_id, pointer))
        .collect::<HashMap<_, _>>();

    let mut seen = HashSet::new();
    let requested_set: HashSet<i64> = chunk_ids.iter().copied().collect();
    let mut out = Vec::new();
    let mut returned_chars = 0usize;
    let mut truncated_at: Option<HydratedChunk> = None;
    for requested_id in chunk_ids {
        let Some(pointer) = pointers.get(requested_id) else {
            continue;
        };
        let from_ord = pointer.ord.saturating_sub(opts.before as i64);
        let to_ord = pointer.ord.saturating_add(opts.after as i64);
        for mut chunk in load_chunks_by_ord_range(&conn, &pointer.doc_id, from_ord, to_ord)? {
            chunk.requested = requested_set.contains(&chunk.chunk_id);
            if !seen.insert(chunk.chunk_id) {
                continue;
            }
            let projected_chars = returned_chars + chunk.text.len();
            if opts
                .max_chars
                .is_some_and(|max| !out.is_empty() && projected_chars > max)
            {
                truncated_at = Some(chunk);
                break;
            }
            returned_chars = projected_chars;
            out.push(chunk);
        }
        if truncated_at.is_some() {
            break;
        }
    }
    let next_call = truncated_at
        .as_ref()
        .map(|chunk| format!("get_chunks(chunk_ids=[{}])", chunk.chunk_id));
    let mut meta = serde_json::Map::new();
    if truncated_at.is_some() {
        meta.insert("truncated".to_string(), JsonValue::Bool(true));
        if let Some(call) = next_call.as_ref() {
            meta.insert("next_call".to_string(), JsonValue::String(call.to_string()));
        }
    }
    let mut response = serde_json::Map::new();
    response.insert(
        "requested_chunk_ids".to_string(),
        serde_json::to_value(chunk_ids)?,
    );
    response.insert(
        "context".to_string(),
        json!({
            "before": opts.before,
            "after": opts.after,
        }),
    );
    response.insert("chunks".to_string(), serde_json::to_value(&out)?);
    if !meta.is_empty() {
        response.insert("meta".to_string(), JsonValue::Object(meta));
    }
    Ok(serde_json::to_string_pretty(&JsonValue::Object(response))?)
}

pub(crate) fn load_chunks_by_ord_range(
    conn: &Connection,
    doc_id: &str,
    from_ord: i64,
    to_ord: i64,
) -> Result<Vec<HydratedChunk>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT c.chunk_id, c.doc_id, c.anchor, c.text,
               d.type, d.title, d.date
        FROM chunks c
        JOIN documents d ON d.doc_id = c.doc_id
        WHERE c.doc_id = ? AND c.ord BETWEEN ? AND ?
        ORDER BY c.ord ASC
        "#,
    )?;
    let rows = stmt.query_map(params![doc_id, from_ord, to_ord], |row| {
        let doc_id: String = row.get("doc_id")?;
        Ok((
            row.get::<_, i64>("chunk_id")?,
            doc_id,
            row.get::<_, String>("type")?,
            row.get::<_, String>("title")?,
            row.get::<_, Option<String>>("date")?,
            row.get::<_, Option<String>>("anchor")?,
            row.get::<_, Vec<u8>>("text")?,
        ))
    })?;
    let mut out = Vec::new();
    let corpus = corpus_doc_id_set()?;
    for row in rows {
        let (chunk_id, doc_id, doc_type, title, date, anchor, text_blob) = row?;
        let raw_text = decompress_text(text_blob)?;
        let text = annotate_doc_refs_with(&raw_text, &doc_id, corpus);
        out.push(HydratedChunk {
            chunk_id,
            requested: false,
            doc_id: doc_id.clone(),
            doc_type,
            title,
            date,
            anchor,
            canonical_url: canonical_url(&doc_id),
            text,
        });
    }
    Ok(out)
}

pub(crate) fn get_asset_mcp(args: &JsonValue) -> Result<JsonValue> {
    let asset_ref = required_str(args, "asset_ref")?;
    get_asset(asset_ref)
}

pub(crate) fn get_asset(asset_ref: &str) -> Result<JsonValue> {
    let conn = open_read()?;
    let mut stmt = conn.prepare(
        r#"
        SELECT doc_id, media_type, alt, title, data
        FROM document_assets
        WHERE asset_ref = ?
        "#,
    )?;
    let mut rows = stmt.query([asset_ref])?;
    let Some(row) = rows.next()? else {
        return Ok(json!([
            { "type": "text", "text": format!("_Asset not found: `{}`_", asset_ref) }
        ]));
    };
    let doc_id: String = row.get("doc_id")?;
    let media_type: Option<String> = row.get("media_type")?;
    let alt: Option<String> = row.get("alt")?;
    let title: Option<String> = row.get("title")?;
    let data: Vec<u8> = row.get("data")?;
    let bytes = data.len();

    let mime = media_type.unwrap_or_else(|| "application/octet-stream".to_string());
    let caption = match alt.as_deref().or(title.as_deref()) {
        Some(label) if !label.is_empty() => {
            format!("Asset `{asset_ref}` ({mime}, {bytes} bytes) from `{doc_id}`: {label}")
        }
        _ => format!("Asset `{asset_ref}` ({mime}, {bytes} bytes) from `{doc_id}`"),
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
    Ok(json!([
        { "type": "text", "text": caption },
        { "type": "image", "data": b64, "mimeType": mime },
    ]))
}

pub(crate) fn get_doc_anchors_mcp(args: &JsonValue) -> Result<String> {
    let doc_id = required_str(args, "doc_id")?;
    get_doc_anchors(doc_id)
}

/// Convert an ATO point-in-time timestamp (`YYYYMMDDHHMMSS`) to an ISO
/// `YYYY-MM-DD` date. Returns `None` when the input is shorter than 8
/// characters or its first 8 characters are not all digits.
pub(crate) fn pit_to_date(pit: &str) -> Option<String> {
    if pit.len() < 8 {
        return None;
    }
    let head = &pit[..8];
    if !head.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("{}-{}-{}", &head[..4], &head[4..6], &head[6..8]))
}

pub(crate) fn get_doc_anchors(doc_id: &str) -> Result<String> {
    let conn = open_read()?;
    let mut stmt = conn.prepare(
        r#"
        SELECT ord, kind, label, target_chunk_id, target_doc_id, target_pit
        FROM doc_anchors
        WHERE doc_id = ?
        ORDER BY ord ASC
        "#,
    )?;
    let mut in_doc = Vec::<JsonValue>::new();
    let mut related_docs = Vec::<JsonValue>::new();
    let mut historical_versions = Vec::<JsonValue>::new();
    let mut unresolved_in_doc = false;
    let rows = stmt.query_map([doc_id], |row| {
        let kind: String = row.get("kind")?;
        let label: String = row.get("label")?;
        let target_chunk_id: Option<i64> = row.get("target_chunk_id")?;
        let target_doc_id: Option<String> = row.get("target_doc_id")?;
        let target_pit: Option<String> = row.get("target_pit")?;
        Ok((kind, label, target_chunk_id, target_doc_id, target_pit))
    })?;
    for row in rows {
        let (kind, label, target_chunk_id, target_doc_id, target_pit) = row?;
        match kind.as_str() {
            "in_doc" => {
                if let Some(chunk_id) = target_chunk_id {
                    in_doc.push(json!({
                        "label": label,
                        "chunk_id": chunk_id,
                    }));
                } else {
                    unresolved_in_doc = true;
                }
            }
            "sister" => {
                if let Some(target) = target_doc_id {
                    related_docs.push(json!({
                        "label": label,
                        "doc_id": target,
                    }));
                }
            }
            "history" => {
                if let Some(target) = target_doc_id {
                    let mut entry = serde_json::Map::new();
                    entry.insert("label".to_string(), JsonValue::String(label));
                    entry.insert("doc_id".to_string(), JsonValue::String(target.clone()));
                    if let Some(pit) = target_pit.as_deref() {
                        entry.insert("pit".to_string(), JsonValue::String(pit.to_string()));
                        if let Some(date) = pit_to_date(pit) {
                            entry.insert("date".to_string(), JsonValue::String(date));
                        }
                        // Fully-qualified URL the agent can WebFetch directly.
                        // Historical content is not stored locally — agents
                        // requesting the older version follow this URL.
                        entry.insert(
                            "url".to_string(),
                            JsonValue::String(format!(
                                "https://www.ato.gov.au/law/view/document?docid={target}&PiT={pit}"
                            )),
                        );
                    }
                    historical_versions.push(JsonValue::Object(entry));
                }
            }
            _ => {}
        }
    }
    if unresolved_in_doc {
        let mut seen = in_doc
            .iter()
            .filter_map(|entry| {
                Some((
                    entry.get("label")?.as_str()?.to_string(),
                    entry.get("chunk_id")?.as_i64()?,
                ))
            })
            .collect::<HashSet<_>>();
        for entry in resolve_in_doc_anchor_chunks(&conn, doc_id)? {
            let Some(label) = entry.get("label").and_then(|value| value.as_str()) else {
                continue;
            };
            let Some(chunk_id) = entry.get("chunk_id").and_then(|value| value.as_i64()) else {
                continue;
            };
            if seen.insert((label.to_string(), chunk_id)) {
                in_doc.push(entry);
            }
        }
    }
    let (cited_by, cited_by_total) = load_cited_by(&conn, doc_id)?;
    let mut response = serde_json::Map::new();
    response.insert("doc_id".to_string(), JsonValue::String(doc_id.to_string()));
    response.insert("in_doc".to_string(), JsonValue::Array(in_doc));
    response.insert("related_docs".to_string(), JsonValue::Array(related_docs));
    response.insert(
        "historical_versions".to_string(),
        JsonValue::Array(historical_versions),
    );
    response.insert("cited_by".to_string(), JsonValue::Array(cited_by.clone()));
    // Only surface the total when truncation actually hid citers — keeps
    // the wire quiet for the common case where the agent is seeing the
    // whole list.
    if (cited_by_total as usize) > cited_by.len() {
        response.insert(
            "cited_by_total".to_string(),
            JsonValue::Number(serde_json::Number::from(cited_by_total)),
        );
    }
    Ok(serde_json::to_string_pretty(&JsonValue::Object(response))?)
}

pub(crate) fn resolve_in_doc_anchor_chunks(conn: &Connection, doc_id: &str) -> Result<Vec<JsonValue>> {
    let html_blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT html FROM documents WHERE doc_id = ?",
            [doc_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(html_blob) = html_blob else {
        return Ok(Vec::new());
    };
    let html = decompress_text(html_blob)?;
    let refs = extract_anchors(&html, doc_id);
    if refs.is_empty() {
        return Ok(Vec::new());
    }

    let mut stmt = conn
        .prepare("SELECT anchor, chunk_id FROM chunks WHERE doc_id = ? AND anchor IS NOT NULL")?;
    let rows = stmt.query_map([doc_id], |row| {
        Ok((
            row.get::<_, String>("anchor")?,
            row.get::<_, i64>("chunk_id")?,
        ))
    })?;
    let mut chunk_id_by_anchor = HashMap::new();
    for row in rows {
        let (anchor, chunk_id) = row?;
        chunk_id_by_anchor.entry(anchor).or_insert(chunk_id);
    }
    if chunk_id_by_anchor.is_empty() {
        return Ok(Vec::new());
    }

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in refs {
        if r.kind != "in_doc" {
            continue;
        }
        let Some(target_anchor) = r.target_anchor.as_deref() else {
            continue;
        };
        let Some(chunk_id) = chunk_id_by_anchor.get(target_anchor).copied() else {
            continue;
        };
        if seen.insert((r.label.clone(), chunk_id)) {
            out.push(json!({
                "label": r.label,
                "chunk_id": chunk_id,
            }));
        }
    }
    Ok(out)
}

/// [MT-17] Per-doc cap on the `cited_by` array surfaced by `get_doc_anchors`. The
/// most heavily-cited docs (ITAA 1997 s 8-1, Pt IVA, ...) have thousands of
/// citers and would otherwise dominate the response. Order by source date
/// DESC so the agent sees the most recent citations first; the total count
/// lives on `cited_by_total` when truncation occurs.
///
/// [UM-07] Streams `chunks.text` once, regex-extracts every `[doc:X]` marker
/// (PiT / view qualifiers collapse to the base doc_id), and INSERT OR
/// IGNORE-batches into `citations`. Idempotent: clears first.
///
/// Called at the tail of `rebuild_live_db_from_manifest`. The rebuild path
/// bulk-inserts chunks into a fresh staging DB and then atomic-renames it
/// over the live file; freshly-inserted chunks carry no citation rows, so
/// every row must be derived here before the swap.
pub(crate) fn derive_citations(conn: &Connection) -> Result<()> {
    static DOC_MARKER_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = DOC_MARKER_RE.get_or_init(|| Regex::new(r"\[doc:([^\s\]@]+)").expect("valid regex"));

    conn.execute("DELETE FROM citations", [])?;
    let mut select = conn.prepare("SELECT chunk_id, doc_id, text FROM chunks")?;
    let mut insert = conn.prepare(
        "INSERT OR IGNORE INTO citations (source_chunk_id, source_doc_id, target_doc_id) \
         VALUES (?, ?, ?)",
    )?;
    let rows = select.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    let mut total: u64 = 0;
    for row in rows {
        let (chunk_id, doc_id, blob) = row?;
        let text = decompress_text(blob)?;
        let mut seen = std::collections::HashSet::new();
        for cap in re.captures_iter(&text) {
            let target = &cap[1];
            if target == doc_id {
                continue;
            }
            if !seen.insert(target.to_string()) {
                continue;
            }
            insert.execute(params![chunk_id, &doc_id, target])?;
            total += 1;
        }
    }
    eprintln!("citations: derived {total} rows post-update");
    Ok(())
}

pub(crate) fn load_cited_by(conn: &Connection, doc_id: &str) -> Result<(Vec<JsonValue>, i64)> {
    let total: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT source_doc_id) FROM citations WHERE target_doc_id = ?",
        [doc_id],
        |row| row.get(0),
    )?;
    let mut stmt = conn.prepare(
        r#"
        SELECT c.source_doc_id, d.title, d.type, d.date
        FROM (
            SELECT DISTINCT source_doc_id FROM citations WHERE target_doc_id = ?
        ) c
        JOIN documents d ON d.doc_id = c.source_doc_id
        ORDER BY d.date DESC NULLS LAST, c.source_doc_id ASC
        LIMIT ?
        "#,
    )?;
    let rows = stmt.query_map(params![doc_id, CITED_BY_LIMIT as i64], |row| {
        let id: String = row.get("source_doc_id")?;
        let title: String = row.get("title")?;
        let dtype: String = row.get("type")?;
        let date: Option<String> = row.get("date")?;
        Ok((id, title, dtype, date))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, title, dtype, date) = row?;
        let mut entry = serde_json::Map::new();
        entry.insert("doc_id".to_string(), JsonValue::String(id));
        entry.insert("title".to_string(), JsonValue::String(title));
        entry.insert("type".to_string(), JsonValue::String(dtype));
        if let Some(d) = date {
            entry.insert("date".to_string(), JsonValue::String(d));
        }
        out.push(JsonValue::Object(entry));
    }
    Ok((out, total))
}
