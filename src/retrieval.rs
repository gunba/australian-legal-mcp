//! MCP retrieval tools: `fetch` (URI-scheme dispatcher for live document
//! fetching), `get_chunks` (progressive disclosure), `get_asset`,
//! `get_doc_anchors`, `get_definition` (+ ordinary-meaning dictionary), plus
//! the `derive_citations` build helper and `load_cited_by` reader.

use crate::chunker::{chunk_html, EMBED_MAX_TOKENS};
use crate::config::{active_generation_key, data_dir};
use crate::db::{decompress_text, open_read, table_exists};
use crate::extract::{extract_anchors, normalize_definition_term};
use crate::html::clean_ato_html;
use crate::legal_source::source_registry;
use crate::uri::{parse_doc_uri, DocUri};
use crate::{
    ATO_FETCH_TIMEOUT, ATO_USER_AGENT, OEWN_2024_SOURCE, OEWN_2024_URL,
    ORDINARY_DICTIONARY_PATH_ENV, STATUTORY_DEFINITION_TYPE_PREFIXES,
};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use chrono::{Datelike, NaiveDate};
use legal_model::{AssetRef, ChunkRef, DocumentId, SourceId};
use regex::Regex;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use url::Url;
use zip::ZipArchive;

pub(crate) const CITED_BY_LIMIT: usize = 100;

/// Per-source native document-id caches used while annotating canonical
/// source-qualified document markers. A cache fill always has a validated
/// `SourceId` predicate; retrieval never scans identities across sources.
type CorpusDocumentIdCache = HashMap<(String, SourceId), Arc<HashSet<String>>>;
static CORPUS_DOC_IDS: OnceLock<Mutex<CorpusDocumentIdCache>> = OnceLock::new();
static ORDINARY_DICTIONARY_INSTALL: Mutex<()> = Mutex::new(());
static ORDINARY_DICTIONARY_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<OrdinaryDictionaryIndex>>>> =
    OnceLock::new();

/// `[doc:X]` marker regex. Captures the doc_id (up to whitespace, `]`, or
/// `@` for the PiT/view qualifier separator) and the trailing qualifier
/// segment up to the closing `]`. Shared by `derive_citations` (build-time
/// citation extraction) and `annotate_doc_refs` (read-time annotation).
fn doc_marker_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[doc:([^\s\]@]+)([^\]]*)\]").expect("valid regex"))
}

/// Lazily load one source's native document ids. The caller must provide a
/// typed identity and the registry check prevents syntactically valid but
/// unregistered sources from reaching SQL.
pub(crate) fn corpus_doc_id_set(source_id: &SourceId) -> Result<Arc<HashSet<String>>> {
    validate_source(source_id)?;
    let generation = active_generation()?;
    let key = (generation, source_id.clone());
    let cache = CORPUS_DOC_IDS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow!("corpus document-id cache lock poisoned"))?;
    if let Some(set) = cache.get(&key) {
        return Ok(Arc::clone(set));
    }
    let conn = open_read()?;
    let mut stmt = conn
        .prepare("SELECT native_id FROM documents WHERE source_id = ?1 ORDER BY native_id ASC")?;
    let mut set = HashSet::new();
    let rows = stmt.query_map([source_id.as_str()], |row| row.get::<_, String>(0))?;
    for row in rows {
        set.insert(row?);
    }
    let set = Arc::new(set);
    cache.insert(key, Arc::clone(&set));
    Ok(set)
}

fn validate_source(source_id: &SourceId) -> Result<()> {
    source_registry().source(source_id)?;
    Ok(())
}

fn active_generation() -> Result<String> {
    active_generation_key()?.ok_or_else(|| {
        anyhow!("no active corpus generation; install a corpus generation before retrieval")
    })
}

fn validate_chunk_ref(reference: &ChunkRef, generation: &str) -> Result<i64> {
    validate_source(&reference.source)?;
    if reference.generation != generation {
        bail!(
            "chunk reference belongs to generation `{}`; active generation is `{generation}`",
            reference.generation
        );
    }
    i64::try_from(reference.chunk_id)
        .map_err(|_| anyhow!("chunk reference exceeds SQLite integer range"))
}

fn chunk_ref(generation: &str, source: &SourceId, chunk_id: i64) -> Result<ChunkRef> {
    let chunk_id = u64::try_from(chunk_id)
        .map_err(|_| anyhow!("stored chunk_id {chunk_id} cannot be represented publicly"))?;
    Ok(ChunkRef::new(generation, source.clone(), chunk_id)?)
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

/// Rewrite canonical `[doc:SOURCE:NATIVE_ID<tail>]` markers whose target is
/// not in the local corpus into `[fetch:SOURCE:NATIVE_ID<query>]`. The marker
/// carries the one exact source identity used by both retrieval paths:
/// path:
///   - `[doc:ato:X]`        → in corpus; use `get_chunks` / `get_doc_anchors`.
///   - `[fetch:ato:X]`      → external; use the `fetch` tool.
///
/// Self-references (target == current document) and markers whose target is
/// in the corpus are left as-is. The regex only matches the `[doc:...]`
/// shape, so already-rewritten `[fetch:...]` markers are inherently
/// idempotent — they don't match and pass through untouched.
pub(crate) fn annotate_doc_refs(text: &str, self_document: &DocumentId) -> Result<String> {
    validate_source(&self_document.source)?;
    let mut sources = BTreeSet::from([self_document.source.clone()]);
    for captures in doc_marker_regex().captures_iter(text) {
        let target: DocumentId = captures[1].parse().with_context(|| {
            format!(
                "invalid source-qualified document marker `[doc:{}]`",
                &captures[1]
            )
        })?;
        validate_source(&target.source)?;
        sources.insert(target.source);
    }
    let mut corpus = HashMap::new();
    for source in sources {
        corpus.insert(source.clone(), corpus_doc_id_set(&source)?);
    }
    annotate_doc_refs_with(text, self_document, &corpus)
}

/// Pure helper: same as `annotate_doc_refs` but takes the doc-id set
/// explicitly so unit tests don't need a live DB.
pub(crate) fn annotate_doc_refs_with(
    text: &str,
    self_document: &DocumentId,
    corpus: &HashMap<SourceId, Arc<HashSet<String>>>,
) -> Result<String> {
    let re = doc_marker_regex();
    let mut annotated = String::with_capacity(text.len());
    let mut previous_end = 0;
    for captures in re.captures_iter(text) {
        let whole = captures
            .get(0)
            .ok_or_else(|| anyhow!("document marker regex omitted its full match"))?;
        let target: DocumentId = captures[1].parse().with_context(|| {
            format!(
                "invalid source-qualified document marker `[doc:{}]`",
                &captures[1]
            )
        })?;
        validate_source(&target.source)?;
        annotated.push_str(&text[previous_end..whole.start()]);
        let tail = &captures[2];
        let is_local = corpus
            .get(&target.source)
            .is_some_and(|ids| ids.contains(&target.native_id));
        if &target == self_document || is_local {
            annotated.push_str(&format!("[doc:{target}{tail}]"));
        } else {
            let query = ato_marker_tail_to_query_suffix(tail);
            let base = DocUri::new(target, None, None)?.to_uri_string();
            let candidate = format!("{base}{query}");
            let canonical = parse_doc_uri(&candidate)
                .map(|uri| uri.to_uri_string())
                .unwrap_or(base);
            annotated.push_str(&format!("[fetch:{canonical}]"));
        }
        previous_end = whole.end();
    }
    annotated.push_str(&text[previous_end..]);
    Ok(annotated)
}

/// Public entry point for the `fetch` MCP tool. Parses the URI scheme and
/// dispatches to the per-source live-fetch implementation. Returns a JSON
/// string with the shape `{uri, canonical_url, title, source, chunks}`.
pub(crate) fn fetch(uri_string: &str) -> Result<String> {
    let uri = parse_doc_uri(uri_string)?;
    let (document, pit, view) = uri.into_parts();
    validate_source(&document.source)?;
    match document.source.as_str() {
        "ato" => fetch_ato_doc(&document.native_id, pit.as_deref(), view.as_deref()),
        source => bail!("live fetch is not available for legal source `{source}`"),
    }
}

/// Live-fetch an ATO document outside the local corpus. Returns the same
/// `{uri, canonical_url, title, source, chunks}` shape as other retrieval
/// responses.
pub(crate) fn fetch_ato_doc(doc_id: &str, pit: Option<&str>, view: Option<&str>) -> Result<String> {
    let mut url = Url::parse("https://www.ato.gov.au/law/view/document")?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("docid", doc_id);
        if let Some(p) = pit.filter(|s| !s.is_empty()) {
            query.append_pair("PiT", p);
        }
        if let Some(v) = view.filter(|s| !s.is_empty()) {
            query.append_pair("db", v);
        }
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
        .get(url.clone())
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
    let source = SourceId::new("ato")?;
    validate_source(&source)?;
    let document = DocumentId::new(source, doc_id)?;
    let chunk_json: Vec<JsonValue> = chunks
        .iter()
        .map(|c| -> Result<JsonValue> {
            Ok(json!({
                "ord": c.ord,
                "anchor": c.anchor,
                "text": annotate_doc_refs(&c.text, &document)?,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    let canonical_uri = DocUri::new(
        document.clone(),
        pit.map(str::to_string),
        view.map(str::to_string),
    )?
    .to_uri_string();
    Ok(serde_json::to_string_pretty(&json!({
        "uri": canonical_uri,
        "canonical_url": url.as_str(),
        "document": document,
        "title": cleaned.title,
        "source": document.source,
        "chunks": chunk_json,
    }))?)
}

pub(crate) struct GetDefinitionOptions {
    pub(crate) source: SourceId,
    pub(crate) context_document: Option<DocumentId>,
    pub(crate) max_defs: usize,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct DefinitionSource {
    pub(crate) document: DocumentId,
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

#[derive(Debug)]
struct OrdinaryDictionaryIndex {
    source: String,
    definitions: HashMap<String, Vec<OrdinaryDefinition>>,
}

impl OrdinaryDictionaryIndex {
    fn parse(raw: &str, fallback_dictionary_source: &str) -> Result<Self> {
        if let Ok(entries) = serde_json::from_str::<Vec<DictionaryEntry>>(raw) {
            return Ok(Self::from_entries(entries, fallback_dictionary_source));
        }
        let jsonl = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<DictionaryEntry>(line.trim()).ok())
            .collect::<Vec<_>>();
        if !jsonl.is_empty() {
            return Ok(Self::from_entries(jsonl, fallback_dictionary_source));
        }

        let mut definitions: HashMap<String, Vec<OrdinaryDefinition>> = HashMap::new();
        for line in raw.lines() {
            let parts = line.splitn(4, '\t').collect::<Vec<_>>();
            let (term, definition) = match parts.as_slice() {
                [norm, _display, part_of_speech, definition] => (
                    (*norm).to_string(),
                    OrdinaryDefinition {
                        part_of_speech: Some((*part_of_speech).to_string()),
                        definition: (*definition).to_string(),
                    },
                ),
                [term, definition, ..] => (
                    normalize_definition_term(term),
                    OrdinaryDefinition {
                        part_of_speech: None,
                        definition: (*definition).to_string(),
                    },
                ),
                _ => continue,
            };
            if !term.is_empty() {
                let values = definitions.entry(term).or_default();
                if values.len() < 5 {
                    values.push(definition);
                }
            }
        }
        Ok(Self {
            source: fallback_dictionary_source.to_string(),
            definitions,
        })
    }

    fn from_entries(entries: Vec<DictionaryEntry>, fallback_dictionary_source: &str) -> Self {
        let mut source = fallback_dictionary_source.to_string();
        let mut definitions: HashMap<String, Vec<OrdinaryDefinition>> = HashMap::new();
        for entry in entries {
            if let Some(entry_source) = entry.source {
                source = entry_source;
            }
            let values = definitions
                .entry(normalize_definition_term(&entry.term))
                .or_default();
            if values.len() < 5 {
                values.push(OrdinaryDefinition {
                    part_of_speech: entry.part_of_speech,
                    definition: entry.definition,
                });
            }
        }
        Self {
            source,
            definitions,
        }
    }

    fn lookup(&self, wanted: &str) -> Option<OrdinaryMeaningHit> {
        self.definitions
            .get(wanted)
            .filter(|definitions| !definitions.is_empty())
            .map(|definitions| OrdinaryMeaningHit {
                term: wanted.to_string(),
                kind: "ordinary".to_string(),
                source: self.source.clone(),
                definitions: definitions.clone(),
            })
    }
}

pub(crate) fn context_prefix(context_document: Option<&DocumentId>) -> Option<String> {
    let document = context_document?;
    let mut parts = document.native_id.split('/');
    let first = parts.next()?;
    let second = parts.next()?;
    if first == "PAC" {
        Some(format!("{first}/{second}"))
    } else {
        None
    }
}

pub(crate) fn definition_rank(hit: &DefinitionHit, opts: &GetDefinitionOptions) -> usize {
    if opts
        .context_document
        .as_ref()
        .is_some_and(|document| hit.source.document == *document)
    {
        return 0;
    }
    if let Some(prefix) = context_prefix(opts.context_document.as_ref()) {
        if hit.source.document.native_id.starts_with(&(prefix + "/")) {
            return 1;
        }
    }
    2
}

pub(crate) fn get_definition(term: &str, opts: GetDefinitionOptions) -> Result<String> {
    validate_source(&opts.source)?;
    if let Some(document) = &opts.context_document {
        validate_source(&document.source)?;
        if opts.source != document.source {
            bail!(
                "definition source `{}` does not match context document source `{}`",
                opts.source,
                document.source
            );
        }
    }
    let source_id = opts.source.clone();
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
        SELECT x.definition_id, x.term, x.native_id, x.source_title,
               x.source_type, x.scope, x.anchor, x.body, d.canonical_url
        FROM definitions AS x
        JOIN documents AS d
          ON d.source_id = x.source_id AND d.native_id = x.native_id
        WHERE x.source_id = ? AND x.norm_term = ?
          AND x.source_type IN ({source_placeholders})
        ORDER BY x.native_id, x.ord, x.term
        LIMIT 100
        "#
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut query_params = Vec::with_capacity(2 + STATUTORY_DEFINITION_TYPE_PREFIXES.len());
    query_params.push(Value::Text(source_id.as_str().to_string()));
    query_params.push(Value::Text(norm));
    for source_type in STATUTORY_DEFINITION_TYPE_PREFIXES {
        query_params.push(Value::Text((*source_type).to_string()));
    }
    let mut hits = stmt
        .query_map(rusqlite::params_from_iter(query_params), |row| {
            let native_id: String = row.get("native_id")?;
            Ok(DefinitionHit {
                definition_id: row.get("definition_id")?,
                term: row.get("term")?,
                kind: "statutory".to_string(),
                body: row.get("body")?,
                source: DefinitionSource {
                    document: DocumentId {
                        source: source_id.clone(),
                        native_id,
                    },
                    title: row.get("source_title")?,
                    source_type: row.get("source_type")?,
                    scope: row.get("scope")?,
                    anchor: row.get("anchor")?,
                    canonical_url: row.get("canonical_url")?,
                },
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut seen = HashSet::new();
    hits.retain(|hit| seen.insert((hit.source.document.clone(), hit.body.clone())));
    hits.sort_by_key(|hit| definition_rank(hit, &opts));
    hits.truncate(max_defs);
    let (ordinary, ordinary_error) = if hits.is_empty() {
        ordinary_meaning_or_error(term)
    } else {
        (None, None)
    };
    format_definition_response(term, &hits, ordinary, ordinary_error, true)
}

pub(crate) fn ordinary_meaning_or_error(
    term: &str,
) -> (Option<OrdinaryMeaningHit>, Option<String>) {
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
    let _guard = ORDINARY_DICTIONARY_INSTALL
        .lock()
        .map_err(|_| anyhow!("ordinary-meaning dictionary install lock poisoned"))?;
    let lock_path = index_path.with_extension("install.lock");
    let install_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening ordinary-meaning lock {}", lock_path.display()))?;
    install_file
        .lock()
        .with_context(|| format!("locking ordinary-meaning index {}", lock_path.display()))?;
    if index_path.exists() {
        return Ok(index_path);
    }
    let response = reqwest::blocking::Client::builder()
        .user_agent(ATO_USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()?
        .get(OEWN_2024_URL)
        .send()?
        .error_for_status()
        .with_context(|| format!("fetching ordinary-meaning source from {OEWN_2024_URL}"))?;
    const MAX_DICTIONARY_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
    let mut zip_bytes = Vec::new();
    response
        .take(MAX_DICTIONARY_ARCHIVE_BYTES + 1)
        .read_to_end(&mut zip_bytes)?;
    if zip_bytes.len() as u64 > MAX_DICTIONARY_ARCHIVE_BYTES {
        bail!("ordinary-meaning archive exceeded the 512 MiB limit");
    }
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
    let wanted = normalize_definition_term(term);
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let cache = ORDINARY_DICTIONARY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow!("ordinary-meaning dictionary cache lock poisoned"))?;
    let index = if let Some(index) = cache.get(&canonical) {
        Arc::clone(index)
    } else {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading ordinary-meaning dictionary {}", path.display()))?;
        let index = Arc::new(OrdinaryDictionaryIndex::parse(&raw, source)?);
        cache.insert(canonical, Arc::clone(&index));
        index
    };
    drop(cache);
    Ok(index.lookup(&wanted))
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

const MAX_GET_CHUNKS_IDS: usize = 100;
const DEFAULT_GET_CHUNKS_MAX_CHARS: usize = 50_000;
const HARD_GET_CHUNKS_MAX_CHARS: usize = 200_000;

pub(crate) struct GetChunksOptions {
    pub(crate) before: usize,
    pub(crate) after: usize,
    pub(crate) max_chars: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HydratedChunk {
    pub(crate) chunk: ChunkRef,
    pub(crate) requested: bool,
    pub(crate) document: DocumentId,
    #[serde(rename = "type")]
    pub(crate) doc_type: String,
    pub(crate) title: String,
    pub(crate) date: Option<String>,
    pub(crate) anchor: Option<String>,
    pub(crate) canonical_url: String,
    pub(crate) text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text_start: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text_complete: Option<bool>,
}

#[derive(Debug, Clone)]
pub(crate) struct ChunkPointer {
    key: ChunkKey,
    pub(crate) document: DocumentId,
    pub(crate) ord: i64,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ChunkKey {
    source: SourceId,
    chunk_id: i64,
}

#[derive(Debug, Clone)]
struct ChunkRange {
    document: DocumentId,
    from_ord: i64,
    to_ord: i64,
    request_order: usize,
}

#[derive(Debug)]
struct StoredChunk {
    source: SourceId,
    chunk_id: i64,
    native_id: String,
    doc_type: String,
    title: String,
    date: Option<String>,
    anchor: Option<String>,
    canonical_url: String,
    text_blob: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct GetChunksCursor {
    chunks: Vec<ChunkRef>,
    before: usize,
    after: usize,
    max_chars: usize,
    item: usize,
    text_offset: usize,
}

fn encode_get_chunks_cursor(cursor: &GetChunksCursor) -> Result<String> {
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor)?))
}

fn decode_get_chunks_cursor(value: &str) -> Result<GetChunksCursor> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .context("invalid get_chunks cursor encoding")?;
    serde_json::from_slice(&bytes).context("invalid get_chunks cursor payload")
}

pub(crate) fn get_chunks(
    chunks: Vec<ChunkRef>,
    opts: GetChunksOptions,
    cursor: Option<&str>,
) -> Result<String> {
    let cursor = cursor.map(decode_get_chunks_cursor).transpose()?;
    get_chunks_from_cursor(&chunks, opts, cursor)
}

fn get_chunks_from_cursor(
    chunks: &[ChunkRef],
    opts: GetChunksOptions,
    cursor: Option<GetChunksCursor>,
) -> Result<String> {
    if chunks.is_empty() {
        bail!("chunks must contain at least one reference");
    }
    if chunks.len() > MAX_GET_CHUNKS_IDS {
        bail!("chunks accepts at most {MAX_GET_CHUNKS_IDS} references per request");
    }
    let max_chars = opts.max_chars.unwrap_or(DEFAULT_GET_CHUNKS_MAX_CHARS);
    if max_chars == 0 || max_chars > HARD_GET_CHUNKS_MAX_CHARS {
        bail!("max_chars must be between 1 and {HARD_GET_CHUNKS_MAX_CHARS}");
    }
    let generation = active_generation()?;
    let requested_keys = chunks
        .iter()
        .map(|reference| {
            Ok(ChunkKey {
                source: reference.source.clone(),
                chunk_id: validate_chunk_ref(reference, &generation)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let expected_cursor = GetChunksCursor {
        chunks: chunks.to_vec(),
        before: opts.before,
        after: opts.after,
        max_chars,
        item: 0,
        text_offset: 0,
    };
    let cursor = cursor.unwrap_or_else(|| expected_cursor.clone());
    if cursor.chunks != expected_cursor.chunks
        || cursor.before != expected_cursor.before
        || cursor.after != expected_cursor.after
        || cursor.max_chars != expected_cursor.max_chars
    {
        bail!("get_chunks cursor does not match chunks or retrieval options");
    }
    let conn = open_read()?;
    let predicates = vec!["(source_id = ? AND chunk_id = ?)"; requested_keys.len()].join(" OR ");
    let sql = format!("SELECT source_id, chunk_id, native_id, ord FROM chunks WHERE {predicates}");
    let mut params_vec = Vec::with_capacity(requested_keys.len() * 2);
    for key in &requested_keys {
        params_vec.push(Value::Text(key.source.as_str().to_string()));
        params_vec.push(Value::Integer(key.chunk_id));
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params_vec), |row| {
        Ok((
            row.get::<_, String>("source_id")?,
            row.get::<_, i64>("chunk_id")?,
            row.get::<_, String>("native_id")?,
            row.get::<_, i64>("ord")?,
        ))
    })?;
    let mut pointers = HashMap::new();
    for row in rows {
        let (source, chunk_id, native_id, ord) = row?;
        let source = source
            .parse::<SourceId>()
            .with_context(|| format!("invalid stored chunk source `{source}`"))?;
        validate_source(&source)?;
        let pointer = ChunkPointer {
            key: ChunkKey {
                source: source.clone(),
                chunk_id,
            },
            document: DocumentId { source, native_id },
            ord,
        };
        pointers.insert(pointer.key.clone(), pointer);
    }
    if let Some((reference, _)) = chunks
        .iter()
        .zip(&requested_keys)
        .find(|(_, key)| !pointers.contains_key(*key))
    {
        bail!("chunk reference `{reference}` was not found in the active generation");
    }

    let requested_set = requested_keys.iter().cloned().collect::<HashSet<_>>();
    let mut requested_order = HashMap::new();
    for (index, key) in requested_keys.iter().cloned().enumerate() {
        requested_order.entry(key).or_insert(index);
    }
    let ranges = chunk_ranges_in_request_order(&requested_keys, &pointers, opts.before, opts.after);
    let mut stored = Vec::new();
    let mut seen = HashSet::new();
    for range in ranges {
        for chunk in
            load_stored_chunks_by_ord_range(&conn, &range.document, range.from_ord, range.to_ord)?
        {
            let key = ChunkKey {
                source: chunk.source.clone(),
                chunk_id: chunk.chunk_id,
            };
            if requested_order
                .get(&key)
                .is_some_and(|order| *order > range.request_order)
            {
                continue;
            }
            if seen.insert(key) {
                stored.push(chunk);
            }
        }
    }
    let stored_len = stored.len();
    if cursor.item > stored_len || (cursor.item == stored_len && cursor.text_offset != 0) {
        bail!("get_chunks cursor is beyond the hydrated result set");
    }
    let mut out = Vec::new();
    let mut returned_chars = 0usize;
    let mut next = None;
    for (index, chunk) in stored.into_iter().enumerate().skip(cursor.item) {
        let raw_text = decompress_text(chunk.text_blob)?;
        let document = DocumentId {
            source: chunk.source.clone(),
            native_id: chunk.native_id.clone(),
        };
        let text = annotate_doc_refs(&raw_text, &document)?;
        let start = if index == cursor.item {
            cursor.text_offset
        } else {
            0
        };
        let total_chars = text.chars().count();
        if start > total_chars {
            bail!("get_chunks cursor text offset is beyond its chunk");
        }
        if start == total_chars {
            continue;
        }
        let remaining = max_chars - returned_chars;
        let available = total_chars - start;
        let take = available.min(remaining);
        let body = text.chars().skip(start).take(take).collect::<String>();
        let complete = take == available;
        let key = ChunkKey {
            source: chunk.source.clone(),
            chunk_id: chunk.chunk_id,
        };
        out.push(HydratedChunk {
            chunk: chunk_ref(&generation, &chunk.source, chunk.chunk_id)?,
            requested: requested_set.contains(&key),
            document,
            doc_type: chunk.doc_type.clone(),
            title: chunk.title.clone(),
            date: chunk.date.clone(),
            anchor: chunk.anchor.clone(),
            canonical_url: chunk.canonical_url,
            text: body,
            text_start: (start != 0 || !complete).then_some(start),
            text_complete: (start != 0 || !complete).then_some(complete),
        });
        returned_chars += take;
        if !complete {
            next = Some(GetChunksCursor {
                item: index,
                text_offset: start + take,
                ..expected_cursor.clone()
            });
            break;
        }
        if returned_chars == max_chars && index + 1 < stored_len {
            next = Some(GetChunksCursor {
                item: index + 1,
                text_offset: 0,
                ..expected_cursor.clone()
            });
            break;
        }
    }
    let next_call = next.as_ref().map(get_chunks_next_call).transpose()?;
    let mut meta = serde_json::Map::new();
    if next.is_some() {
        meta.insert("truncated".to_string(), JsonValue::Bool(true));
        if let Some(call) = next_call.as_ref() {
            meta.insert("next_call".to_string(), JsonValue::String(call.to_string()));
        }
    }
    let mut response = serde_json::Map::new();
    response.insert("requested".to_string(), serde_json::to_value(chunks)?);
    response.insert(
        "context".to_string(),
        json!({
            "before": opts.before,
            "after": opts.after,
            "max_chars": max_chars,
        }),
    );
    response.insert("chunks".to_string(), serde_json::to_value(&out)?);
    if !meta.is_empty() {
        response.insert("meta".to_string(), JsonValue::Object(meta));
    }
    Ok(serde_json::to_string_pretty(&JsonValue::Object(response))?)
}

fn get_chunks_next_call(cursor: &GetChunksCursor) -> Result<String> {
    let chunks = serde_json::to_string(&cursor.chunks)?;
    Ok(format!(
        "get_chunks(chunks={chunks}, before={}, after={}, max_chars={}, cursor={})",
        cursor.before,
        cursor.after,
        cursor.max_chars,
        serde_json::to_string(&encode_get_chunks_cursor(cursor)?)?
    ))
}

fn chunk_ranges_in_request_order(
    requested: &[ChunkKey],
    pointers: &HashMap<ChunkKey, ChunkPointer>,
    before: usize,
    after: usize,
) -> Vec<ChunkRange> {
    let mut ranges = Vec::new();
    for (request_order, key) in requested.iter().enumerate() {
        let Some(pointer) = pointers.get(key) else {
            continue;
        };
        ranges.push(ChunkRange {
            document: pointer.document.clone(),
            from_ord: pointer.ord.saturating_sub(before as i64),
            to_ord: pointer.ord.saturating_add(after as i64),
            request_order,
        });
    }
    ranges
}

fn load_stored_chunks_by_ord_range(
    conn: &Connection,
    document: &DocumentId,
    from_ord: i64,
    to_ord: i64,
) -> Result<Vec<StoredChunk>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT c.chunk_id, c.native_id, c.anchor, c.text,
               d.type, d.title, d.date, d.canonical_url
        FROM chunks c
        JOIN documents d
          ON d.source_id = c.source_id AND d.native_id = c.native_id
        WHERE c.source_id = ? AND c.native_id = ? AND c.ord BETWEEN ? AND ?
        ORDER BY c.ord ASC
        "#,
    )?;
    let rows = stmt.query_map(
        params![
            document.source.as_str(),
            document.native_id,
            from_ord,
            to_ord
        ],
        |row| {
            Ok((
                row.get::<_, i64>("chunk_id")?,
                row.get::<_, String>("native_id")?,
                row.get::<_, String>("type")?,
                row.get::<_, String>("title")?,
                row.get::<_, Option<String>>("date")?,
                row.get::<_, Option<String>>("anchor")?,
                row.get::<_, String>("canonical_url")?,
                row.get::<_, Vec<u8>>("text")?,
            ))
        },
    )?;
    rows.map(|row| {
        let (chunk_id, native_id, doc_type, title, date, anchor, canonical_url, text_blob) = row?;
        Ok(StoredChunk {
            source: document.source.clone(),
            chunk_id,
            native_id,
            doc_type,
            title,
            date,
            anchor,
            canonical_url,
            text_blob,
        })
    })
    .collect()
}

pub(crate) fn get_asset(asset: AssetRef) -> Result<JsonValue> {
    validate_source(&asset.source)?;
    let conn = open_read()?;
    let mut stmt = conn.prepare(
        r#"
        SELECT native_id, media_type, alt, title, data
        FROM document_assets
        WHERE source_id = ? AND asset_id = ?
        "#,
    )?;
    let mut rows = stmt.query(params![asset.source.as_str(), asset.asset_id])?;
    let Some(row) = rows.next()? else {
        return Ok(json!([
            { "type": "text", "text": format!("_Asset not found: `{}`_", serde_json::to_string(&asset)?) }
        ]));
    };
    let document = DocumentId {
        source: asset.source.clone(),
        native_id: row.get("native_id")?,
    };
    let media_type: Option<String> = row.get("media_type")?;
    let alt: Option<String> = row.get("alt")?;
    let title: Option<String> = row.get("title")?;
    let data: Vec<u8> = row.get("data")?;
    let bytes = data.len();

    let mime = media_type.unwrap_or_else(|| "application/octet-stream".to_string());
    let caption = match alt.as_deref().or(title.as_deref()) {
        Some(label) if !label.is_empty() => {
            format!("Asset `{asset}` ({mime}, {bytes} bytes) from `{document}`: {label}")
        }
        _ => format!("Asset `{asset}` ({mime}, {bytes} bytes) from `{document}`"),
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
    Ok(json!([
        { "type": "text", "text": caption },
        { "type": "image", "data": b64, "mimeType": mime },
    ]))
}

/// Convert an ATO point-in-time timestamp (`YYYYMMDDHHMMSS`) to an ISO
/// `YYYY-MM-DD` date. Returns `None` when the input is shorter than 8
/// characters or its first 8 characters are not all digits.
pub(crate) fn pit_to_date(pit: &str) -> Option<String> {
    let head = pit.get(..8)?;
    if !head.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let date = NaiveDate::parse_from_str(head, "%Y%m%d").ok()?;
    Some(format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month(),
        date.day()
    ))
}

pub(crate) fn get_doc_anchors(document: DocumentId) -> Result<String> {
    validate_source(&document.source)?;
    let generation = active_generation()?;
    let conn = open_read()?;
    let mut stmt = conn.prepare(
        r#"
        SELECT a.ord, a.kind, a.label, a.target_chunk_id,
               a.target_source_id, a.target_native_id, a.target_pit,
               target.canonical_url AS target_canonical_url
        FROM doc_anchors AS a
        LEFT JOIN documents AS target
          ON target.source_id = a.target_source_id
         AND target.native_id = a.target_native_id
        WHERE a.source_id = ? AND a.native_id = ?
        ORDER BY a.ord ASC
        "#,
    )?;
    let mut in_doc = Vec::<JsonValue>::new();
    let mut related_docs = Vec::<JsonValue>::new();
    let mut historical_versions = Vec::<JsonValue>::new();
    let mut unresolved_in_doc = false;
    let rows = stmt.query_map(
        params![document.source.as_str(), document.native_id],
        |row| {
            let kind: String = row.get("kind")?;
            let label: String = row.get("label")?;
            let target_chunk_id: Option<i64> = row.get("target_chunk_id")?;
            let target_source_id: Option<String> = row.get("target_source_id")?;
            let target_native_id: Option<String> = row.get("target_native_id")?;
            let target_pit: Option<String> = row.get("target_pit")?;
            let canonical_url: Option<String> = row.get("target_canonical_url")?;
            Ok((
                kind,
                label,
                target_chunk_id,
                target_source_id,
                target_native_id,
                target_pit,
                canonical_url,
            ))
        },
    )?;
    for row in rows {
        let (
            kind,
            label,
            target_chunk_id,
            target_source_id,
            target_native_id,
            target_pit,
            canonical_url,
        ) = row?;
        match kind.as_str() {
            "in_doc" => {
                if let Some(chunk_id) = target_chunk_id {
                    let source = target_source_id.ok_or_else(|| {
                        anyhow!(
                            "stored in-doc anchor with a chunk target is missing target_source_id"
                        )
                    })?;
                    let target_source = source
                        .parse::<SourceId>()
                        .with_context(|| format!("invalid stored anchor source `{source}`"))?;
                    validate_source(&target_source)?;
                    in_doc.push(json!({
                        "label": label,
                        "chunk": chunk_ref(&generation, &target_source, chunk_id)?,
                    }));
                } else {
                    unresolved_in_doc = true;
                }
            }
            "sister" => {
                let source = target_source_id
                    .ok_or_else(|| anyhow!("stored sister anchor is missing target_source_id"))?;
                let native_id = target_native_id
                    .ok_or_else(|| anyhow!("stored sister anchor is missing target_native_id"))?;
                let source = source
                    .parse::<SourceId>()
                    .with_context(|| format!("invalid stored anchor source `{source}`"))?;
                validate_source(&source)?;
                related_docs.push(json!({
                    "label": label,
                    "document": DocumentId { source, native_id },
                    "canonical_url": canonical_url,
                }));
            }
            "history" => {
                let source = target_source_id
                    .ok_or_else(|| anyhow!("stored history anchor is missing target_source_id"))?;
                let native_id = target_native_id
                    .ok_or_else(|| anyhow!("stored history anchor is missing target_native_id"))?;
                let source = source
                    .parse::<SourceId>()
                    .with_context(|| format!("invalid stored anchor source `{source}`"))?;
                validate_source(&source)?;
                let mut entry = serde_json::Map::new();
                entry.insert("label".to_string(), JsonValue::String(label));
                entry.insert(
                    "document".to_string(),
                    serde_json::to_value(DocumentId { source, native_id })?,
                );
                if let Some(url) = canonical_url {
                    entry.insert("canonical_url".to_string(), JsonValue::String(url));
                }
                if let Some(pit) = target_pit.as_deref() {
                    entry.insert("pit".to_string(), JsonValue::String(pit.to_string()));
                    if let Some(date) = pit_to_date(pit) {
                        entry.insert("date".to_string(), JsonValue::String(date));
                    }
                }
                historical_versions.push(JsonValue::Object(entry));
            }
            other => bail!("unsupported stored anchor kind `{other}`"),
        }
    }
    if unresolved_in_doc {
        let mut seen = in_doc
            .iter()
            .filter_map(|entry| {
                Some((
                    entry.get("label")?.as_str()?.to_string(),
                    serde_json::from_value::<ChunkRef>(entry.get("chunk")?.clone()).ok()?,
                ))
            })
            .collect::<HashSet<_>>();
        for entry in resolve_in_doc_anchor_chunks(&conn, &document, &generation)? {
            let Some(label) = entry.get("label").and_then(|value| value.as_str()) else {
                continue;
            };
            let Some(chunk) = entry
                .get("chunk")
                .cloned()
                .and_then(|value| serde_json::from_value::<ChunkRef>(value).ok())
            else {
                continue;
            };
            if seen.insert((label.to_string(), chunk)) {
                in_doc.push(entry);
            }
        }
    }
    let (cited_by, cited_by_total) = load_cited_by(&conn, &document)?;
    let mut response = serde_json::Map::new();
    response.insert("document".to_string(), serde_json::to_value(document)?);
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

pub(crate) fn resolve_in_doc_anchor_chunks(
    conn: &Connection,
    document: &DocumentId,
    generation: &str,
) -> Result<Vec<JsonValue>> {
    let html_blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT html FROM documents WHERE source_id = ? AND native_id = ?",
            params![document.source.as_str(), document.native_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(html_blob) = html_blob else {
        return Ok(Vec::new());
    };
    let html = decompress_text(html_blob)?;
    let refs = extract_anchors(&html, &document.native_id);
    if refs.is_empty() {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT anchor, chunk_id FROM chunks \
         WHERE source_id = ? AND native_id = ? AND anchor IS NOT NULL",
    )?;
    let rows = stmt.query_map(
        params![document.source.as_str(), document.native_id],
        |row| {
            Ok((
                row.get::<_, String>("anchor")?,
                row.get::<_, i64>("chunk_id")?,
            ))
        },
    )?;
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
                "chunk": chunk_ref(generation, &document.source, chunk_id)?,
            }));
        }
    }
    Ok(out)
}

/// Per-doc cap on the `cited_by` array surfaced by `get_doc_anchors`. The
/// most heavily-cited docs (ITAA 1997 s 8-1, Pt IVA, ...) have thousands of
/// citers and would otherwise dominate the response. Order by source date
/// DESC so the agent sees the most recent citations first; the total count
/// lives on `cited_by_total` when truncation occurs.
///
/// Streams one source's `chunks.text`, extracts canonical source-qualified
/// document markers, and replaces only that source's derived citation rows.
/// PiT/view qualifiers collapse to the typed base `DocumentId`.
///
/// Called at the tail of `rebuild_live_db_from_manifest`. The rebuild path
/// bulk-inserts chunks into a fresh staging DB and then atomic-renames it
/// over the live file; freshly-inserted chunks carry no citation rows, so
/// every row must be derived here before the swap.
pub(crate) fn derive_citations(conn: &Connection, source_id: &SourceId) -> Result<()> {
    conn.execute(
        "DELETE FROM citations WHERE source_id = ?1",
        [source_id.as_str()],
    )?;
    let mut select = conn.prepare(
        "SELECT chunk_id, native_id, text FROM chunks \
         WHERE source_id = ?1 ORDER BY chunk_id ASC",
    )?;
    let mut insert = conn.prepare(
        "INSERT OR IGNORE INTO citations (\
             source_chunk_id, source_id, source_native_id, \
             target_source_id, target_native_id\
         ) VALUES (?, ?, ?, ?, ?)",
    )?;
    let rows = select.query_map([source_id.as_str()], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    let mut total: u64 = 0;
    for row in rows {
        let (chunk_id, native_id, blob) = row?;
        let source_document = DocumentId {
            source: source_id.clone(),
            native_id: native_id.clone(),
        };
        let text = decompress_text(blob)?;
        let mut seen = HashSet::new();
        for captures in doc_marker_regex().captures_iter(&text) {
            let target: DocumentId = captures[1].parse().with_context(|| {
                format!(
                    "invalid source-qualified document marker `[doc:{}]` in chunk {chunk_id}",
                    &captures[1]
                )
            })?;
            if target == source_document {
                continue;
            }
            if !seen.insert(target.clone()) {
                continue;
            }
            insert.execute(params![
                chunk_id,
                source_id.as_str(),
                native_id,
                target.source.as_str(),
                target.native_id
            ])?;
            total += 1;
        }
    }
    eprintln!("citations: derived {total} rows post-update");
    Ok(())
}

pub(crate) fn load_cited_by(
    conn: &Connection,
    document: &DocumentId,
) -> Result<(Vec<JsonValue>, i64)> {
    validate_source(&document.source)?;
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM (\
             SELECT DISTINCT source_id, source_native_id FROM citations \
             WHERE target_source_id = ?1 AND target_native_id = ?2\
         )",
        params![document.source.as_str(), document.native_id],
        |row| row.get(0),
    )?;
    let mut stmt = conn.prepare(
        r#"
        SELECT c.source_id, c.source_native_id, d.title, d.type, d.date,
               d.canonical_url
        FROM (
            SELECT DISTINCT source_id, source_native_id
            FROM citations
            WHERE target_source_id = ? AND target_native_id = ?
        ) c
        JOIN documents d
          ON d.source_id = c.source_id AND d.native_id = c.source_native_id
        ORDER BY d.date DESC NULLS LAST, c.source_id ASC, c.source_native_id ASC
        LIMIT ?
        "#,
    )?;
    let rows = stmt.query_map(
        params![
            document.source.as_str(),
            document.native_id,
            CITED_BY_LIMIT as i64
        ],
        |row| {
            let source: String = row.get("source_id")?;
            let native_id: String = row.get("source_native_id")?;
            let title: String = row.get("title")?;
            let dtype: String = row.get("type")?;
            let date: Option<String> = row.get("date")?;
            let canonical_url: String = row.get("canonical_url")?;
            Ok((source, native_id, title, dtype, date, canonical_url))
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        let (source, native_id, title, dtype, date, canonical_url) = row?;
        let source = source
            .parse::<SourceId>()
            .with_context(|| format!("invalid stored citation source `{source}`"))?;
        validate_source(&source)?;
        let mut entry = serde_json::Map::new();
        entry.insert(
            "document".to_string(),
            serde_json::to_value(DocumentId { source, native_id })?,
        );
        entry.insert("title".to_string(), JsonValue::String(title));
        entry.insert("type".to_string(), JsonValue::String(dtype));
        entry.insert(
            "canonical_url".to_string(),
            JsonValue::String(canonical_url),
        );
        if let Some(d) = date {
            entry.insert("date".to_string(), JsonValue::String(d));
        }
        out.push(JsonValue::Object(entry));
    }
    Ok((out, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> SourceId {
        "ato".parse().expect("valid source")
    }

    fn document(native_id: &str) -> DocumentId {
        DocumentId::new(source(), native_id).expect("valid document")
    }

    fn chunk(chunk_id: u64) -> ChunkRef {
        ChunkRef::new("generation-1", source(), chunk_id).expect("valid chunk")
    }

    #[test]
    fn chunk_cursor_round_trips_all_request_state() -> Result<()> {
        let cursor = GetChunksCursor {
            chunks: vec![chunk(7), chunk(9), chunk(7)],
            before: 2,
            after: 3,
            max_chars: 1234,
            item: 4,
            text_offset: 55,
        };
        assert_eq!(
            decode_get_chunks_cursor(&encode_get_chunks_cursor(&cursor)?)?,
            cursor
        );
        let call = get_chunks_next_call(&cursor)?;
        assert!(call.contains("chunks=[{"));
        assert!(call.contains("\"generation\":\"generation-1\""));
        assert!(call.contains("\"source\":\"ato\""));
        assert!(call.contains("\"chunk_id\":7"));
        assert!(call.contains("before=2"));
        assert!(call.contains("after=3"));
        assert!(call.contains("max_chars=1234"));
        Ok(())
    }

    #[test]
    fn chunk_ranges_preserve_mixed_source_request_order() {
        let frl: SourceId = "frl".parse().expect("valid source");
        let pointers = [
            (
                ChunkKey {
                    source: source(),
                    chunk_id: 10,
                },
                ChunkPointer {
                    key: ChunkKey {
                        source: source(),
                        chunk_id: 10,
                    },
                    document: document("A"),
                    ord: 5,
                },
            ),
            (
                ChunkKey {
                    source: source(),
                    chunk_id: 11,
                },
                ChunkPointer {
                    key: ChunkKey {
                        source: source(),
                        chunk_id: 11,
                    },
                    document: document("A"),
                    ord: 7,
                },
            ),
            (
                ChunkKey {
                    source: frl.clone(),
                    chunk_id: 20,
                },
                ChunkPointer {
                    key: ChunkKey {
                        source: frl.clone(),
                        chunk_id: 20,
                    },
                    document: DocumentId::new(frl.clone(), "B").expect("valid document"),
                    ord: 1,
                },
            ),
        ]
        .into_iter()
        .collect();
        let requested = vec![
            ChunkKey {
                source: source(),
                chunk_id: 10,
            },
            ChunkKey {
                source: frl.clone(),
                chunk_id: 20,
            },
            ChunkKey {
                source: source(),
                chunk_id: 11,
            },
        ];
        let ranges = chunk_ranges_in_request_order(&requested, &pointers, 1, 1);
        assert_eq!(ranges.len(), 3);
        assert_eq!(
            (
                ranges[0].document.native_id.as_str(),
                ranges[0].from_ord,
                ranges[0].to_ord,
                ranges[0].request_order
            ),
            ("A", 4, 6, 0)
        );
        assert_eq!(
            (
                ranges[1].document.source.as_str(),
                ranges[1].document.native_id.as_str(),
                ranges[1].request_order
            ),
            ("frl", "B", 1)
        );
        assert_eq!(
            (
                ranges[2].document.native_id.as_str(),
                ranges[2].from_ord,
                ranges[2].to_ord,
                ranges[2].request_order
            ),
            ("A", 6, 8, 2)
        );
    }

    #[test]
    fn pit_dates_are_calendar_valid_and_unicode_safe() {
        assert_eq!(pit_to_date("20240229120000").as_deref(), Some("2024-02-29"));
        assert_eq!(pit_to_date("20230229120000"), None);
        assert_eq!(pit_to_date("éééééééé"), None);
    }

    #[test]
    fn ordinary_dictionary_builds_a_bounded_lookup_index() -> Result<()> {
        let raw = "car\tcar\tnoun\ta road vehicle\ncar\tcar\tnoun\nan automobile\n";
        let index = OrdinaryDictionaryIndex::parse(raw, "test")?;
        let hit = index.lookup("car").expect("indexed term");
        assert_eq!(hit.source, "test");
        assert_eq!(hit.definitions.len(), 2);
        assert!(index.lookup("missing").is_none());
        Ok(())
    }

    #[test]
    fn document_markers_require_one_canonical_source_qualified_identity() -> Result<()> {
        let self_document = document("PAC/1997-38/1-1");
        let mut corpus = HashMap::new();
        corpus.insert(
            source(),
            Arc::new(HashSet::from(["PAC/1997-38/2-1".to_string()])),
        );
        let text = "[doc:ato:PAC/1997-38/1-1] [doc:ato:PAC/1997-38/2-1] [doc:ato:PAC/missing]";
        let annotated = annotate_doc_refs_with(text, &self_document, &corpus)?;
        assert_eq!(
            annotated,
            "[doc:ato:PAC/1997-38/1-1] [doc:ato:PAC/1997-38/2-1] [fetch:legal://ato/PAC%2Fmissing]"
        );
        let error = annotate_doc_refs_with("[doc:PAC/1997-38/1-1]", &self_document, &corpus)
            .expect_err("bare native ids must not parse");
        assert!(error.to_string().contains("source-qualified"));
        Ok(())
    }

    #[test]
    fn citation_derivation_replaces_only_the_requested_source() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            r#"
            CREATE TABLE chunks (
                chunk_id INTEGER PRIMARY KEY,
                source_id TEXT NOT NULL,
                native_id TEXT NOT NULL,
                text BLOB NOT NULL
            );
            CREATE TABLE citations (
                source_chunk_id INTEGER NOT NULL,
                source_id TEXT NOT NULL,
                source_native_id TEXT NOT NULL,
                target_source_id TEXT NOT NULL,
                target_native_id TEXT NOT NULL,
                PRIMARY KEY (source_chunk_id, target_source_id, target_native_id)
            );
            INSERT INTO citations VALUES (99, 'other', 'doc', 'ato', 'preserved');
            "#,
        )?;
        let text = crate::db::compress_text("[doc:ato:PAC/target] [doc:ato:PAC/target]")?;
        conn.execute(
            "INSERT INTO chunks VALUES (1, 'ato', 'PAC/source', ?1)",
            params![text],
        )?;
        derive_citations(&conn, &source())?;
        let rows = conn
            .prepare(
                "SELECT source_id, source_native_id, target_source_id, target_native_id \
                 FROM citations ORDER BY source_chunk_id",
            )?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(
            rows,
            vec![
                (
                    "ato".to_string(),
                    "PAC/source".to_string(),
                    "ato".to_string(),
                    "PAC/target".to_string(),
                ),
                (
                    "other".to_string(),
                    "doc".to_string(),
                    "ato".to_string(),
                    "preserved".to_string(),
                ),
            ]
        );
        Ok(())
    }

    #[test]
    fn get_chunks_rejects_empty_and_unbounded_requests_before_opening_the_db() {
        let empty = get_chunks_from_cursor(
            &[],
            GetChunksOptions {
                before: 0,
                after: 0,
                max_chars: Some(10),
            },
            None,
        )
        .expect_err("empty request must fail");
        assert!(empty.to_string().contains("at least one"));

        let excessive = get_chunks_from_cursor(
            &[chunk(1)],
            GetChunksOptions {
                before: 0,
                after: 0,
                max_chars: Some(HARD_GET_CHUNKS_MAX_CHARS + 1),
            },
            None,
        )
        .expect_err("unbounded request must fail");
        assert!(excessive.to_string().contains("max_chars"));
    }
}
