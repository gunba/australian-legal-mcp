//! Hybrid BM25 + vector search with candidate deduplication, RRF fusion,
//! bounded snippet rendering, and direct title hits.

use crate::config::{active_generation_key, ann_path};
use crate::db::{decompress_text, get_corpus_meta, get_source_meta, open_read, table_exists};
use crate::legal_source::source_registry;
use crate::semantic::{dot_i8, encode_query_embedding};
use crate::source::load_active_generation_manifest;
use crate::{
    embedding_model_installed_matches, SearchMode, ServerState, SortBy, DEFAULT_EXCLUDED_TYPES,
    EMBEDDING_DIM, EMBEDDING_MODEL_ID, HARD_MAX_PER_DOC, LEGISLATION_TYPE_PREFIXES, MAX_K,
    OLD_CONTENT_CUTOFF, SNIPPET_CHARS, TITLE_HITS_K,
};
use anyhow::{anyhow, bail, Context, Result};
use legal_model::{ChunkRef, DocumentId, SourceId};
use regex::Regex;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use rust_stemmers::{Algorithm, Stemmer};
use serde::Serialize;
use serde_json::{json, Value as JsonValue};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(test)]
pub(crate) fn fts_query(query: &str) -> String {
    fts_terms(query).join(" OR ")
}

const MAX_FTS_INPUT_CHARS: usize = 4_096;
const MAX_FTS_TERMS: usize = 16;
const FTS_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "was", "were", "with",
];

fn fts_terms(query: &str) -> Vec<String> {
    let re = Regex::new(r"[A-Za-z0-9']+(?:-[A-Za-z0-9']+)*").expect("valid regex");
    let stemmer = Stemmer::create(Algorithm::English);
    let bounded = query.chars().take(MAX_FTS_INPUT_CHARS).collect::<String>();
    let mut seen = HashSet::new();
    re.find_iter(&bounded)
        .map(|m| m.as_str().to_string())
        .filter(|term| {
            let normalized = term.to_ascii_lowercase();
            term.len() >= 2
                && !FTS_STOPWORDS.contains(&normalized.as_str())
                // chunks_fts/title_fts use SQLite's Porter tokenizer. Collapse
                // equivalent English forms before constructing boolean
                // clauses so one indexed stem cannot masquerade as multiple
                // corroborating query terms.
                && seen.insert(stemmer.stem(&normalized).into_owned())
        })
        .take(MAX_FTS_TERMS)
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect()
}

fn fts_broad_query(query: &str) -> Option<String> {
    let terms = fts_terms(query);
    (terms.len() > 1).then(|| terms.join(" OR "))
}

fn fts_corroborated_query(query: &str) -> Option<String> {
    let terms = fts_terms(query);
    match terms.len() {
        // There is no safe intermediate relaxation for two effective terms:
        // OR would permit a single common posting list to dominate the query.
        0..=2 => None,
        // For longer prose, a one-term match is usually the pathological
        // case: a common token can expand to most of a source. Require two
        // independently matching terms while still accepting every pair.
        _ => Some(fts_at_least_two_query(&terms)),
    }
}

/// Build an FTS5 expression that matches iff at least two distinct terms
/// match. The divide-and-conquer form is logically complete (unlike selecting
/// only adjacent pairs) and grows O(n log n), bounded by [`MAX_FTS_TERMS`].
fn fts_at_least_two_query(terms: &[String]) -> String {
    debug_assert!(terms.len() >= 2);
    if terms.len() == 2 {
        return format!("({} AND {})", terms[0], terms[1]);
    }

    let middle = terms.len() / 2;
    let (left, right) = terms.split_at(middle);
    let mut alternatives = Vec::with_capacity(3);
    if left.len() >= 2 {
        alternatives.push(format!("({})", fts_at_least_two_query(left)));
    }
    if right.len() >= 2 {
        alternatives.push(format!("({})", fts_at_least_two_query(right)));
    }
    alternatives.push(format!(
        "(({}) AND ({}))",
        left.join(" OR "),
        right.join(" OR ")
    ));
    alternatives.join(" OR ")
}

fn fts_strict_query(query: &str) -> String {
    fts_terms(query).join(" ")
}

pub(crate) fn glob_to_like(pattern: &str) -> String {
    // Accept both '*' and '%' as wildcards (the prefix idiom the
    // MCP tool descriptor advertises is `PAC/%`); escape '_' and '\' so they
    // match literally. ATO doc_ids never contain '%', so collapsing both
    // wildcard glyphs onto SQL LIKE '%' is safe.
    let mut out = String::new();
    for ch in pattern.chars() {
        match ch {
            '*' | '%' => out.push('%'),
            '_' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) struct SqlFilter {
    pub(crate) source: SourceId,
    pub(crate) sql: String,
    pub(crate) params: Vec<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RowidRange {
    first: i64,
    last: i64,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum FtsTableRange {
    Chunks,
    Titles,
}

fn validate_rowid_range_metadata(
    source_id: &SourceId,
    label: &str,
    first: Option<i64>,
    last: Option<i64>,
    count: i64,
) -> Result<Option<RowidRange>> {
    match (first, last, count) {
        (None, None, 0) => Ok(None),
        (Some(first), Some(last), count) if count > 0 && first <= last => {
            Ok(Some(RowidRange { first, last }))
        }
        _ => bail!(
            "source `{source_id}` {label} rowid range metadata is inconsistent: first={first:?}, last={last:?}, rows={count}"
        ),
    }
}

fn load_chunk_rowid_range(conn: &Connection, source_id: &SourceId) -> Result<Option<RowidRange>> {
    let (first, last, count) = conn.query_row(
        "SELECT MIN(chunk_id), MAX(chunk_id), COUNT(*) FROM chunks WHERE source_id = ?1",
        [source_id.as_str()],
        |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )?;
    let range = validate_rowid_range_metadata(source_id, "chunk", first, last, count)?;
    if let Some(range) = range {
        let overlaps: i64 = conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM chunks
                 WHERE chunk_id BETWEEN ?1 AND ?2 AND source_id <> ?3
                 LIMIT 1
             )",
            params![range.first, range.last, source_id.as_str()],
            |row| row.get(0),
        )?;
        if overlaps != 0 {
            bail!(
                "source `{source_id}` chunk rowids do not occupy one isolated range: first={}, last={}",
                range.first,
                range.last
            );
        }
    }
    Ok(range)
}

fn load_title_rowid_range(conn: &Connection, source_id: &SourceId) -> Result<Option<RowidRange>> {
    let (first, last, count) = conn.query_row(
        "SELECT MIN(rowid), MAX(rowid), COUNT(*) FROM title_fts WHERE source_id = ?1",
        [source_id.as_str()],
        |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )?;
    let range = validate_rowid_range_metadata(source_id, "title", first, last, count)?;
    if let Some(range) = range {
        let overlaps: i64 = conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM title_fts
                 WHERE rowid BETWEEN ?1 AND ?2 AND source_id <> ?3
                 LIMIT 1
             )",
            params![range.first, range.last, source_id.as_str()],
            |row| row.get(0),
        )?;
        if overlaps != 0 {
            bail!(
                "source `{source_id}` title rowids do not occupy one isolated range: first={}, last={}",
                range.first,
                range.last
            );
        }
    }
    Ok(range)
}

fn cached_source_fts_range(
    conn: &Connection,
    generation: &str,
    source_id: &SourceId,
    table: FtsTableRange,
) -> Result<Option<RowidRange>> {
    type CacheValue = std::result::Result<Option<RowidRange>, String>;
    type CacheKey = (String, String, String, FtsTableRange);
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, CacheValue>>> = OnceLock::new();

    // Production connections always have a generation-specific path. Avoid
    // caching anonymous in-memory databases, which are mutable test fixtures.
    let Some(path) = conn.path().filter(|path| !path.is_empty()) else {
        return match table {
            FtsTableRange::Chunks => load_chunk_rowid_range(conn, source_id),
            FtsTableRange::Titles => load_title_rowid_range(conn, source_id),
        };
    };
    let key = (
        path.to_string(),
        generation.to_string(),
        source_id.as_str().to_string(),
        table,
    );
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let cache = cache
            .lock()
            .map_err(|_| anyhow!("source FTS range cache lock poisoned"))?;
        if let Some(result) = cache.get(&key) {
            return result.clone().map_err(|message| anyhow!(message));
        }
    }

    let loaded = match table {
        FtsTableRange::Chunks => load_chunk_rowid_range(conn, source_id),
        FtsTableRange::Titles => load_title_rowid_range(conn, source_id),
    }
    .map_err(|error| error.to_string());
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow!("source FTS range cache lock poisoned"))?;
    cache
        .entry(key)
        .or_insert(loaded)
        .clone()
        .map_err(|message| anyhow!(message))
}

pub(crate) struct DocumentFilterSpec<'a> {
    pub(crate) source_id: &'a SourceId,
    pub(crate) types: Option<&'a [String]>,
    pub(crate) date_from: Option<&'a str>,
    pub(crate) date_to: Option<&'a str>,
    pub(crate) doc_scope: Option<&'a str>,
    pub(crate) include_old: bool,
    pub(crate) current_only: bool,
}

pub(crate) fn build_doc_filter(alias: &str, spec: DocumentFilterSpec<'_>) -> SqlFilter {
    let DocumentFilterSpec {
        source_id,
        types,
        date_from,
        date_to,
        doc_scope,
        include_old,
        current_only,
    } = spec;
    // ATO owns its edited-private-advice and old-content defaults. Other
    // sources must not inherit ATO document codes or date policy.
    let mut clauses = vec![format!("{alias}.source_id = ?")];
    let mut params_out = vec![Value::Text(source_id.as_str().to_string())];

    if let Some(types) = types {
        if !types.is_empty() {
            let mut ors = Vec::new();
            for t in types {
                if t.contains('*') {
                    ors.push(format!("{alias}.type LIKE ? ESCAPE '\\'"));
                    params_out.push(Value::Text(glob_to_like(t)));
                } else {
                    ors.push(format!("{alias}.type = ?"));
                    params_out.push(Value::Text(t.clone()));
                }
            }
            clauses.push(format!("({})", ors.join(" OR ")));
        }
    } else if source_id.as_str() == "ato" && !DEFAULT_EXCLUDED_TYPES.is_empty() {
        let placeholders = vec!["?"; DEFAULT_EXCLUDED_TYPES.len()].join(",");
        clauses.push(format!("{alias}.type NOT IN ({placeholders})"));
        for t in DEFAULT_EXCLUDED_TYPES {
            params_out.push(Value::Text((*t).to_string()));
        }
    }

    if let Some(date_from) = date_from {
        clauses.push(format!("{alias}.date >= ?"));
        params_out.push(Value::Text(date_from.to_string()));
    }
    if let Some(date_to) = date_to {
        clauses.push(format!("{alias}.date <= ?"));
        params_out.push(Value::Text(date_to.to_string()));
    }
    if let Some(doc_scope) = doc_scope {
        clauses.push(format!("{alias}.native_id LIKE ? ESCAPE '\\'"));
        params_out.push(Value::Text(glob_to_like(doc_scope)));
    }
    if source_id.as_str() == "ato" && !include_old && date_from.is_none() {
        let placeholders = vec!["?"; LEGISLATION_TYPE_PREFIXES.len()].join(",");
        clauses.push(format!(
            "({alias}.date IS NULL OR {alias}.date >= ? OR {alias}.type IN ({placeholders}))"
        ));
        params_out.push(Value::Text(OLD_CONTENT_CUTOFF.to_string()));
        for t in LEGISLATION_TYPE_PREFIXES {
            params_out.push(Value::Text((*t).to_string()));
        }
    }
    if current_only {
        // W2.4: drop rulings with a known withdrawal/supersession date by
        // default. Callers that explicitly want the historical/withdrawn
        // material pass current_only=false.
        clauses.push(format!("{alias}.withdrawn_date IS NULL"));
    }

    SqlFilter {
        source: source_id.clone(),
        sql: clauses.join(" AND "),
        params: params_out,
    }
}

fn validate_doc_types(
    conn: &Connection,
    source_id: &SourceId,
    types: Option<&[String]>,
) -> Result<()> {
    let Some(types) = types else {
        return Ok(());
    };

    let mut seen = HashSet::new();
    let mut unknown = Vec::new();
    let mut stmt = conn.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM documents \
         WHERE source_id = ?1 AND type = ?2 LIMIT 1)",
    )?;
    for doc_type in types {
        if doc_type.contains('*') || !seen.insert(doc_type.as_str()) {
            continue;
        }
        let exists = stmt.query_row(params![source_id.as_str(), doc_type], |row| {
            row.get::<_, i64>(0)
        })? != 0;
        if !exists {
            unknown.push(doc_type.as_str());
        }
    }

    if !unknown.is_empty() {
        bail!(
            "unknown exact document type(s) for source `{source_id}`: {}. Use `stats.source_stats.{source_id}.types` to discover corpus type codes, or pass a `*` glob",
            unknown.join(", ")
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub(crate) struct Hit {
    // Search-family hits stay slim; bodies materialize through follow-up tools.
    pub(crate) document: DocumentId,
    pub(crate) title: String,
    #[serde(rename = "type")]
    pub(crate) doc_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) anchor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) snippet: Option<String>,
    pub(crate) canonical_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) chunk: Option<ChunkRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_call: Option<String>,
    /// W2.2 currency markers — only serialised when set so JSON output for
    /// in-force docs stays clean.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) withdrawn_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) superseded_by: Option<DocumentId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) replaces: Option<DocumentId>,
    /// Navigation hint flags — only serialised when set (so a doc with no
    /// matching anchors keeps the slim hit clean). `Some(true)` tells the
    /// agent to call `get_doc_anchors(document)` to navigate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) has_in_doc_links: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) has_related_docs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) has_history: Option<bool>,
}

#[derive(Debug, Clone)]
pub(crate) struct VectorHit {
    pub(crate) chunk_id: i64,
    pub(crate) score: f64,
}

fn rank_hits(hits: &mut [VectorHit]) {
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.chunk_id.cmp(&b.chunk_id))
    });
}

pub(crate) struct SearchOptions<'a> {
    pub(crate) source: SourceId,
    pub(crate) k: usize,
    pub(crate) types: Option<&'a [String]>,
    pub(crate) date_from: Option<&'a str>,
    pub(crate) date_to: Option<&'a str>,
    pub(crate) doc_scope: Option<&'a str>,
    pub(crate) mode: SearchMode,
    pub(crate) sort_by: SortBy,
    pub(crate) include_old: bool,
    /// W2.4: when true (default), withdrawn rulings are excluded from
    /// results. Set to false to include them so the caller sees the
    /// `withdrawn_date`, `superseded_by`, and `replaces` fields on the
    /// hit and can decide whether the source still applies.
    pub(crate) current_only: bool,
    /// Internal-only: maximum chunks returned per document. Capped at
    /// `HARD_MAX_PER_DOC`. NOT exposed in the MCP tool descriptor for
    /// Wave 1 (would inflate the public surface).
    pub(crate) max_per_doc: usize,
    /// When false, hit serialization omits the `snippet` field — callers
    /// that intend to follow up with `get_chunks` save the BM25-windowed
    /// snippet text and the highlight markup pass.
    pub(crate) include_snippet: bool,
    /// When set, vector search uses this chunk's stored embedding as the
    /// query vector and the input `query` string is ignored for the
    /// semantic stage. Forces vector-only mode (no BM25 stage). The input
    /// chunk is filtered out of results so the agent never sees their
    /// seed chunk reflected back.
    pub(crate) similar_to_chunk: Option<ChunkRef>,
    /// When set, this arbitrary text is runtime-embedded and used as the
    /// query vector — the same mechanism as `similar_to_chunk` but for
    /// text that isn't a corpus chunk (e.g. a chunk returned by
    /// `fetch`). Forces vector-only mode and skips title hits,
    /// like `similar_to_chunk`. `similar_to_chunk` wins if both are set.
    pub(crate) seed_text: Option<&'a str>,
}

/// Metadata required to rank and dedup candidate chunks across documents.
#[derive(Debug, Clone)]
pub(crate) struct CandidateMeta {
    pub(crate) document: DocumentId,
    /// True when this chunk's plaintext is short (< 100 chars) and the
    /// chunk sits at the start of the document — typically a stub
    /// preamble that crowds out more useful chunks. We approximate "intro"
    /// as ord == 0 with short text, which correctly demotes the leading
    /// stub chunks.
    pub(crate) is_intro: bool,
}

/// Group candidate `(chunk_id, score)` entries by typed document identity, demote
/// intros, and emit at most `max_per_doc` chunks per document until `k`
/// is reached. Per-document score is the max of the top three chunk
/// scores within that document. Pure function — no DB access — so it
/// can be tested in isolation.
pub(crate) fn dedup_per_doc(
    ranked: Vec<VectorHit>,
    meta: &HashMap<i64, CandidateMeta>,
    k: usize,
    max_per_doc: usize,
) -> Vec<VectorHit> {
    let cap = max_per_doc.clamp(1, HARD_MAX_PER_DOC);
    if ranked.is_empty() || cap == 0 || k == 0 {
        return Vec::new();
    }

    // Bucket per doc, keep insertion order (which matches incoming
    // ranking order so each bucket is already sorted by score desc when
    // the caller did its own sort).
    let mut buckets: BTreeMap<usize, (DocumentId, Vec<(VectorHit, bool)>)> = BTreeMap::new();
    let mut order: HashMap<DocumentId, usize> = HashMap::new();
    let mut next_idx = 0usize;
    for hit in ranked {
        let Some(m) = meta.get(&hit.chunk_id) else {
            continue;
        };
        let idx = match order.get(&m.document) {
            Some(i) => *i,
            None => {
                let i = next_idx;
                order.insert(m.document.clone(), i);
                buckets.insert(i, (m.document.clone(), Vec::new()));
                next_idx += 1;
                i
            }
        };
        buckets
            .get_mut(&idx)
            .expect("bucket present")
            .1
            .push((hit, m.is_intro));
    }

    // For each doc, sort its candidate list by (is_intro asc, score desc)
    // so non-intro chunks always come before intro chunks within a doc.
    // Then compute the per-doc score as max of the top 3 chunk scores in
    // that ordered list.
    let mut docs: Vec<(DocumentId, f64, Vec<VectorHit>)> = Vec::new();
    for (_idx, (document, mut items)) in buckets {
        items.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.0.score.total_cmp(&a.0.score))
                .then_with(|| a.0.chunk_id.cmp(&b.0.chunk_id))
        });
        let doc_score = items
            .iter()
            .take(3)
            .map(|(h, _)| h.score)
            .fold(f64::NEG_INFINITY, f64::max);
        let chunks: Vec<VectorHit> = items.into_iter().map(|(h, _)| h).collect();
        docs.push((document, doc_score, chunks));
    }
    docs.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // Single pass: take up to `cap` chunks from each doc in score order
    // until we hit `k`. We do not back-fill beyond the cap — the user
    // wants per-doc diversity to be a hard constraint, not a soft one.
    // Callers that need more chunks from the same doc should follow up
    // with `get_chunks`.
    let mut out: Vec<VectorHit> = Vec::with_capacity(k);
    for (_document, _score, chunks) in &docs {
        if out.len() >= k {
            break;
        }
        let take = cap.min(k - out.len()).min(chunks.len());
        for hit in chunks.iter().take(take) {
            out.push(hit.clone());
        }
    }
    out
}

pub(crate) fn search(
    query: &str,
    opts: SearchOptions<'_>,
    server_state: Option<&ServerState>,
) -> Result<String> {
    let _corpus_lock = crate::config::corpus_read_lock()?;
    let resolved_source = opts.source.clone();
    source_registry().source(&resolved_source)?;
    let generation = active_generation_key()?.ok_or_else(|| {
        anyhow!("no active corpus generation; install a corpus generation before search")
    })?;
    if let Some(state) = server_state {
        state.ensure_corpus_generation_unchanged()?;
    }
    let conn = open_read()?;
    validate_doc_types(&conn, &resolved_source, opts.types)?;
    let k = opts.k.clamp(1, MAX_K);
    let max_per_doc = opts.max_per_doc.clamp(1, HARD_MAX_PER_DOC);
    let filter = build_doc_filter(
        "d",
        DocumentFilterSpec {
            source_id: &resolved_source,
            types: opts.types,
            date_from: opts.date_from,
            date_to: opts.date_to,
            doc_scope: opts.doc_scope,
            include_old: opts.include_old,
            current_only: opts.current_only,
        },
    );
    // k is clamped, first-stage recall is widened, then candidates dedupe per document.
    let internal_limit = std::cmp::max(k * 5, 50);
    // `similar_to_chunk` short-circuits semantic encode: load the seed
    // chunk's stored embedding and use it as the query vector. Force
    // vector-only mode (no BM25 stage — no real query text to rank against).
    let similar_seed: Option<(i64, [i8; EMBEDDING_DIM])> = match opts.similar_to_chunk.as_ref() {
        Some(reference) => {
            if reference.source != resolved_source {
                bail!(
                    "similar_to_chunk source `{}` does not match resolved search source `{resolved_source}`",
                    reference.source
                );
            }
            if reference.generation != generation {
                bail!(
                    "similar_to_chunk generation `{}` does not match active generation `{generation}`",
                    reference.generation
                );
            }
            let seed_id = i64::try_from(reference.chunk_id)
                .map_err(|_| anyhow!("similar_to_chunk id exceeds SQLite integer range"))?;
            Some((
                seed_id,
                load_chunk_embedding(&conn, &resolved_source, seed_id)?,
            ))
        }
        None => None,
    };
    // `seed_text` runtime-embeds arbitrary text as the query vector — the
    // same seed-driven path as `similar_to_chunk`, but for text that
    // isn't a corpus chunk (e.g. a chunk from `fetch`).
    // `similar_to_chunk` wins if both are set.
    let seed_text: Option<&str> = if similar_seed.is_some() {
        None
    } else {
        opts.seed_text.map(str::trim).filter(|s| !s.is_empty())
    };
    // A "seed search" is driven by a seed vector rather than the `query`
    // string: forces vector-only mode and returns no title hits.
    let is_seed_search = similar_seed.is_some() || seed_text.is_some();
    let effective_mode = if is_seed_search {
        SearchMode::Vector
    } else {
        opts.mode
    };
    let chunk_rowid_range = if matches!(effective_mode, SearchMode::Hybrid | SearchMode::Keyword) {
        Some(cached_source_fts_range(
            &conn,
            &generation,
            &resolved_source,
            FtsTableRange::Chunks,
        )?)
    } else {
        None
    };
    let title_rowid_range = if is_seed_search {
        None
    } else {
        Some(cached_source_fts_range(
            &conn,
            &generation,
            &resolved_source,
            FtsTableRange::Titles,
        )?)
    };
    let source_ann = if matches!(effective_mode, SearchMode::Hybrid | SearchMode::Vector) {
        Some(ensure_vector_search_ready(&conn, &resolved_source)?)
    } else {
        None
    };
    let lexical_hits = if matches!(effective_mode, SearchMode::Hybrid | SearchMode::Keyword) {
        lexical_search_in_range(
            &conn,
            &resolved_source,
            query,
            &filter,
            internal_limit,
            chunk_rowid_range
                .ok_or_else(|| anyhow!("lexical search did not prepare source FTS bounds"))?,
        )?
    } else {
        Vec::new()
    };
    let ranked_hits = match effective_mode {
        SearchMode::Hybrid | SearchMode::Vector => {
            let source_ann = source_ann
                .as_ref()
                .ok_or_else(|| anyhow!("vector mode did not prepare source ANN state"))?;
            let query_embedding = if let Some((_, ref seed_vec)) = similar_seed {
                *seed_vec
            } else {
                // `seed_text`, when set, replaces the `query` string as the
                // text to embed for the semantic stage.
                let embed_input = seed_text.unwrap_or(query);
                match server_state {
                    Some(state) => state.encode_query_embedding(embed_input)?,
                    None => encode_query_embedding(embed_input)?,
                }
            };
            let vector_hits = vector_search(
                &conn,
                &resolved_source,
                source_ann,
                &query_embedding,
                &filter,
                internal_limit,
            )?;
            // Filter the seed chunk out of its own similar-chunks results.
            let vector_hits = if let Some((seed_id, _)) = similar_seed {
                vector_hits
                    .into_iter()
                    .filter(|h| h.chunk_id != seed_id)
                    .collect()
            } else {
                vector_hits
            };
            if matches!(effective_mode, SearchMode::Hybrid) {
                rrf_fuse(&vector_hits, &lexical_hits)
            } else {
                vector_hits
            }
        }
        SearchMode::Keyword => lexical_hits.clone(),
    };
    let candidate_count = ranked_hits.len();

    let frontier = match opts.sort_by {
        SortBy::Relevance => k,
        SortBy::Recency => std::cmp::max(k * 5, 50),
    };

    // Batch-load (chunk_id -> document, is_intro) for all candidates so the
    // dedup pass doesn't have to round-trip per chunk.
    let candidate_meta = load_candidate_meta(&conn, &resolved_source, &ranked_hits)?;
    let deduped = dedup_per_doc(ranked_hits, &candidate_meta, frontier, max_per_doc);

    let ranked_ids = deduped.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>();
    let mut hydrated = load_hits(
        &conn,
        &resolved_source,
        &generation,
        &ranked_ids,
        query,
        opts.include_snippet,
    )?;
    let mut records = ranked_ids
        .into_iter()
        .filter_map(|chunk_id| hydrated.remove(&chunk_id))
        .collect::<Vec<_>>();
    if matches!(opts.sort_by, SortBy::Recency) {
        // Recency sort materializes a widened frontier, then sorts by date descending.
        records.sort_by(|a, b| {
            b.date
                .cmp(&a.date)
                .then_with(|| a.document.cmp(&b.document))
                .then_with(|| a.chunk.cmp(&b.chunk))
        });
        records.truncate(k);
    }
    // JSON metadata preserves query/filter state in next_call when k can grow.
    let next_call = if candidate_count > records.len() && k < MAX_K {
        Some(search_next_call(query, std::cmp::min(k * 2, MAX_K), &opts)?)
    } else {
        None
    };

    let mut meta = serde_json::Map::new();
    meta.insert("resolved_source".to_string(), json!(resolved_source));
    if candidate_count > records.len() {
        meta.insert("truncated".to_string(), json!(true));
        if let Some(nc) = next_call {
            meta.insert("next_call".to_string(), json!(nc));
        }
    }

    // Title-level hits — a parallel algorithm over the separate `title_fts`
    // table, surfaced as a sidebar alongside the chunk `hits`. Reuses the
    // same document filter so chunk and title queries stay consistently
    // scoped. Skipped for a seed search (`similar_to_chunk` / `seed_text`)
    // — there's no real query text to BM25 against; `query` is ignored.
    let title_hits: Vec<Hit> = if is_seed_search {
        Vec::new()
    } else {
        collect_title_hits_in_range(
            &conn,
            &resolved_source,
            query,
            TITLE_HITS_K,
            &filter,
            title_rowid_range
                .ok_or_else(|| anyhow!("title search did not prepare source FTS bounds"))?,
        )?
    };

    let mut response = serde_json::Map::new();
    response.insert("hits".to_string(), json!(records));
    response.insert("title_hits".to_string(), json!(title_hits));
    if !meta.is_empty() {
        response.insert("meta".to_string(), JsonValue::Object(meta));
    }
    Ok(serde_json::to_string_pretty(&JsonValue::Object(response))?)
}

pub(crate) fn search_cli(query: &str, opts: SearchOptions<'_>) -> Result<(String, ServerState)> {
    let state = ServerState::default();
    let out = search(query, opts, Some(&state))?;
    Ok((out, state))
}

pub(crate) fn load_candidate_meta(
    conn: &Connection,
    source_id: &SourceId,
    ranked: &[VectorHit],
) -> Result<HashMap<i64, CandidateMeta>> {
    if ranked.is_empty() {
        return Ok(HashMap::new());
    }
    // Deduplicate ids; ranked may include the same chunk via both vector
    // and lexical paths in degenerate cases.
    let mut ids: Vec<i64> = ranked.iter().map(|h| h.chunk_id).collect();
    ids.sort_unstable();
    ids.dedup();

    let placeholders = vec!["?"; ids.len()].join(",");
    // Two-step query: first read (chunk_id, native_id, ord) for every
    // candidate cheaply; then decompress the text BLOB only for the
    // small minority sitting at ord == 0 so we can measure the *plain*
    // text length precisely. Heading-path is gone; "intro" now means
    // "leading stub chunk" (ord 0 with short text) which still
    // correctly demotes the typical preamble pattern.
    let sql = format!(
        "SELECT chunk_id, native_id, ord FROM chunks \
         WHERE source_id = ? AND chunk_id IN ({placeholders})"
    );
    let mut params_vec = vec![Value::Text(source_id.as_str().to_string())];
    params_vec.extend(ids.into_iter().map(Value::Integer));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params_vec), |row| {
        let chunk_id: i64 = row.get("chunk_id")?;
        let native_id: String = row.get("native_id")?;
        let ord: i64 = row.get("ord")?;
        Ok((chunk_id, native_id, ord))
    })?;
    let mut leading_chunk_ids: Vec<i64> = Vec::new();
    let mut staged: Vec<(i64, String, i64)> = Vec::new();
    for row in rows {
        let (chunk_id, native_id, ord) = row?;
        if ord == 0 {
            leading_chunk_ids.push(chunk_id);
        }
        staged.push((chunk_id, native_id, ord));
    }

    let mut intro_set: HashSet<i64> = HashSet::new();
    if !leading_chunk_ids.is_empty() {
        let placeholders2 = vec!["?"; leading_chunk_ids.len()].join(",");
        let sql2 = format!(
            "SELECT chunk_id, text FROM chunks \
             WHERE source_id = ? AND chunk_id IN ({placeholders2})"
        );
        let mut params_vec2 = vec![Value::Text(source_id.as_str().to_string())];
        params_vec2.extend(leading_chunk_ids.into_iter().map(Value::Integer));
        let mut stmt2 = conn.prepare(&sql2)?;
        let rows2 = stmt2.query_map(params_from_iter(params_vec2), |row| {
            let chunk_id: i64 = row.get("chunk_id")?;
            let text_blob: Vec<u8> = row.get("text")?;
            Ok((chunk_id, text_blob))
        })?;
        for row in rows2 {
            let (chunk_id, text_blob) = row?;
            let plain = decompress_text(text_blob)?;
            if plain.len() < 100 {
                intro_set.insert(chunk_id);
            }
        }
    }

    let mut out = HashMap::new();
    for (chunk_id, native_id, _ord) in staged {
        let is_intro = intro_set.contains(&chunk_id);
        out.insert(
            chunk_id,
            CandidateMeta {
                document: DocumentId {
                    source: source_id.clone(),
                    native_id,
                },
                is_intro,
            },
        );
    }
    Ok(out)
}

pub(crate) fn search_next_call(query: &str, k: usize, opts: &SearchOptions<'_>) -> Result<String> {
    let mut args = vec![
        format!("query={}", mcp_string(query)),
        format!("source={}", mcp_string(opts.source.as_str())),
        format!("k={k}"),
        format!("mode=\"{}\"", opts.mode.as_str()),
    ];
    if let Some(types) = opts.types {
        let rendered = types
            .iter()
            .map(|value| mcp_string(value))
            .collect::<Vec<_>>()
            .join(", ");
        args.push(format!("types=[{rendered}]"));
    }
    if let Some(date_from) = opts.date_from {
        args.push(format!("date_from={}", mcp_string(date_from)));
    }
    if let Some(date_to) = opts.date_to {
        args.push(format!("date_to={}", mcp_string(date_to)));
    }
    if let Some(doc_scope) = opts.doc_scope {
        args.push(format!("doc_scope={}", mcp_string(doc_scope)));
    }
    if !matches!(opts.sort_by, SortBy::Relevance) {
        args.push(format!("sort_by=\"{}\"", opts.sort_by.as_str()));
    }
    if opts.include_old {
        args.push("include_old=true".to_string());
    }
    if !opts.current_only {
        args.push("current_only=false".to_string());
    }
    if !opts.include_snippet {
        args.push("include_snippet=false".to_string());
    }
    // Seed-driven searches: preserve the seed so paging re-runs the same
    // semantic query rather than falling back to a plain `query` search.
    if let Some(similar_to_chunk) = opts.similar_to_chunk.as_ref() {
        args.push(format!(
            "similar_to_chunk={}",
            serde_json::to_string(similar_to_chunk)?
        ));
    } else if let Some(seed) = opts.seed_text {
        args.push(format!("seed_text={}", mcp_string(seed)));
    }
    Ok(format!("search({})", args.join(", ")))
}

pub(crate) fn mcp_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// Load a chunk's stored int8 embedding from `chunk_embeddings`. Used by
/// `similar_to` to bypass query encoding and run vector search
/// directly against the seed chunk's vector.
pub(crate) fn load_chunk_embedding(
    conn: &Connection,
    source_id: &SourceId,
    chunk_id: i64,
) -> Result<[i8; EMBEDDING_DIM]> {
    let blob: Vec<u8> = conn
        .query_row(
            "SELECT e.embedding FROM chunk_embeddings AS e \
             JOIN chunks AS c ON c.chunk_id = e.chunk_id \
             WHERE c.source_id = ?1 AND e.chunk_id = ?2",
            params![source_id.as_str(), chunk_id],
            |row| row.get(0),
        )
        .with_context(|| {
            format!("no stored embedding for source `{source_id}` chunk_id={chunk_id}")
        })?;
    if blob.len() != EMBEDDING_DIM {
        bail!(
            "stored embedding for chunk_id={chunk_id} has wrong length: got {}, expected {}",
            blob.len(),
            EMBEDDING_DIM
        );
    }
    let mut out = [0i8; EMBEDDING_DIM];
    for (i, b) in blob.iter().enumerate() {
        out[i] = *b as i8;
    }
    Ok(out)
}

#[derive(Clone, Debug)]
pub(crate) struct SourceAnn {
    sidecar: Arc<crate::ann::FlatAnn>,
    generation: String,
}

struct SourceAnnInstall {
    info: crate::ann::ManifestAnn,
    path: PathBuf,
    model: crate::source::ModelInfo,
    generation: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct SourceAnnCacheKey {
    generation: String,
    source: SourceId,
    index_version: String,
    embedding_model_id: String,
    corpus_id: String,
    sidecar_sha256: String,
}

fn load_source_ann(source_id: &SourceId) -> Result<SourceAnnInstall> {
    let generation = active_generation_key()?
        .ok_or_else(|| anyhow!("semantic search unavailable: no active corpus generation"))?;
    let installed = load_active_generation_manifest()?.ok_or_else(|| {
        anyhow!("semantic search unavailable: active generation manifest missing")
    })?;
    let info = installed.ann.get(source_id).cloned().ok_or_else(|| {
        anyhow!("semantic search unavailable: manifest has no ANN sidecar for source `{source_id}`")
    })?;
    crate::ann::validate_manifest_ann(source_id, &info)?;
    let path = ann_path(source_id)?;
    Ok(SourceAnnInstall {
        info,
        path,
        model: installed.model,
        generation,
    })
}

pub(crate) fn ensure_vector_search_ready(
    conn: &Connection,
    source_id: &SourceId,
) -> Result<SourceAnn> {
    const MAX_READY_SIDECARS: usize = 20;
    type ReadyResult = std::result::Result<Arc<crate::ann::FlatAnn>, String>;
    static READINESS: OnceLock<Mutex<HashMap<SourceAnnCacheKey, ReadyResult>>> = OnceLock::new();
    let install = load_source_ann(source_id)?;
    let cache_key = SourceAnnCacheKey {
        generation: install.generation.clone(),
        source: source_id.clone(),
        index_version: get_corpus_meta(conn, "index_version")?.unwrap_or_default(),
        embedding_model_id: get_corpus_meta(conn, "embedding_model_id")?.unwrap_or_default(),
        corpus_id: install.info.corpus_id.clone(),
        sidecar_sha256: install.info.sha256.clone(),
    };
    let cache = READINESS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow!("semantic readiness cache lock poisoned"))?;
    if let Some(result) = cache.get(&cache_key) {
        let sidecar = result.clone().map_err(|message| anyhow!(message))?;
        return Ok(SourceAnn {
            sidecar,
            generation: install.generation,
        });
    }
    // A process normally sees one immutable generation. Keep at most two full
    // ten-source generations if a non-server command switches the pointer.
    if cache.len() >= MAX_READY_SIDECARS {
        cache.clear();
    }
    let result = check_vector_search_ready(conn, source_id, &install)
        .map(Arc::new)
        .map_err(|error| error.to_string());
    cache.insert(cache_key, result.clone());
    let sidecar = result.map_err(|message| anyhow!(message))?;
    Ok(SourceAnn {
        sidecar,
        generation: install.generation,
    })
}

fn check_vector_search_ready(
    conn: &Connection,
    source_id: &SourceId,
    source_ann: &SourceAnnInstall,
) -> Result<crate::ann::FlatAnn> {
    // Hybrid/vector modes require the current semantic corpus model.
    let model_id = get_corpus_meta(conn, "embedding_model_id")?.ok_or_else(|| {
        anyhow!("semantic search unavailable: missing embedding_model_id metadata")
    })?;
    if model_id != EMBEDDING_MODEL_ID {
        bail!(
            "semantic search unavailable: installed corpus uses unsupported embedding model `{model_id}`; install a {EMBEDDING_MODEL_ID} corpus"
        );
    }
    if source_ann.model.id != model_id {
        bail!(
            "semantic search unavailable: installed manifest model `{}` does not match corpus metadata `{model_id}`",
            source_ann.model.id
        );
    }
    if !embedding_model_installed_matches(&source_ann.model)? {
        bail!(
            "semantic search unavailable: active generation model files do not match generation.json"
        );
    }
    if !table_exists(conn, "chunk_embeddings")? {
        bail!("semantic search unavailable: active generation has no chunk_embeddings table");
    }
    let embeddings: i64 = conn.query_row(
        "SELECT COUNT(*) FROM chunk_embeddings AS e \
         JOIN chunks AS c ON c.chunk_id = e.chunk_id \
         WHERE c.source_id = ?1",
        [source_id.as_str()],
        |row| row.get(0),
    )?;
    if embeddings == 0 {
        bail!("semantic search unavailable: installed corpus has no chunk embeddings");
    }
    let (first_chunk_id, last_chunk_id): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT MIN(e.chunk_id), MAX(e.chunk_id) FROM chunk_embeddings AS e \
         JOIN chunks AS c ON c.chunk_id = e.chunk_id \
         WHERE c.source_id = ?1",
        [source_id.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let corpus_id = get_source_meta(conn, source_id.as_str(), "corpus_id")?
        .ok_or_else(|| anyhow!("source `{source_id}` is missing corpus_id metadata"))?;
    let embedding_set_sha256 = get_source_meta(conn, source_id.as_str(), "embedding_set_sha256")?
        .ok_or_else(|| {
        anyhow!("source `{source_id}` is missing embedding_set_sha256 metadata")
    })?;
    if corpus_id != source_ann.info.corpus_id
        || embedding_set_sha256 != source_ann.info.embedding_set_sha256
        || u64::try_from(embeddings).ok() != Some(source_ann.info.vector_count)
        || first_chunk_id.and_then(|value| u32::try_from(value).ok())
            != Some(source_ann.info.first_chunk_id)
        || last_chunk_id.and_then(|value| u32::try_from(value).ok())
            != Some(source_ann.info.last_chunk_id)
    {
        bail!("ANN sidecar metadata does not match source `{source_id}` in the corpus database");
    }
    crate::ann::open_verified_sidecar(&source_ann.path, source_id, &source_ann.info).map_err(|error| {
        anyhow!(
            "semantic search unavailable: required ANN sidecar for source `{source_id}` is not ready: {error}"
        )
    })
}

pub(crate) fn vector_search(
    conn: &Connection,
    source_id: &SourceId,
    source_ann: &SourceAnn,
    query_embedding: &[i8; EMBEDDING_DIM],
    filter: &SqlFilter,
    limit: usize,
) -> Result<Vec<VectorHit>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let eligible = eligible_ann_items(conn, source_id, source_ann, filter)?;
    if eligible.is_empty() {
        return Ok(Vec::new());
    }
    let candidates =
        crate::ann::scan_sidecar(&source_ann.sidecar, query_embedding, &eligible, limit)?;
    exact_rerank_candidates(conn, source_id, query_embedding, &candidates, limit)
}

#[cfg(test)]
pub(crate) fn benchmark_cached_vector_search(
    conn: &Connection,
    source_id: &SourceId,
    sidecar: &Arc<crate::ann::FlatAnn>,
    generation: &str,
    query_embedding: &[i8; EMBEDDING_DIM],
    filter: &SqlFilter,
    limit: usize,
) -> Result<Vec<VectorHit>> {
    vector_search(
        conn,
        source_id,
        &SourceAnn {
            sidecar: Arc::clone(sidecar),
            generation: generation.to_string(),
        },
        query_embedding,
        filter,
        limit,
    )
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum FilterParamKey {
    Null,
    Integer(i64),
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<&Value> for FilterParamKey {
    fn from(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Integer(value) => Self::Integer(*value),
            Value::Real(value) => Self::Real(value.to_bits()),
            Value::Text(value) => Self::Text(value.clone()),
            Value::Blob(value) => Self::Blob(value.clone()),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct EligibilityCacheKey {
    db_path: PathBuf,
    generation: String,
    source: SourceId,
    sql: String,
    params: Vec<FilterParamKey>,
}

type EligibilityResult = std::result::Result<Arc<crate::ann::EligibleRows>, String>;

struct EligibilityCacheEntry {
    value: Arc<OnceLock<EligibilityResult>>,
    last_used: u64,
}

struct EligibilityCache {
    entries: HashMap<EligibilityCacheKey, EligibilityCacheEntry>,
    clock: u64,
    capacity: usize,
}

impl EligibilityCache {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "eligibility cache capacity must be positive");
        Self {
            entries: HashMap::new(),
            clock: 0,
            capacity,
        }
    }

    fn cell(&mut self, key: EligibilityCacheKey) -> Arc<OnceLock<EligibilityResult>> {
        self.clock = self.clock.saturating_add(1);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.clock;
            return Arc::clone(&entry.value);
        }
        if self.entries.len() >= self.capacity {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        let value = Arc::new(OnceLock::new());
        self.entries.insert(
            key,
            EligibilityCacheEntry {
                value: Arc::clone(&value),
                last_used: self.clock,
            },
        );
        value
    }
}

const ELIGIBILITY_CACHE_CAPACITY: usize = 32;
const MAX_ELIGIBILITY_CACHE_KEY_BYTES: usize = 16 * 1024;

fn eligible_ann_items(
    conn: &Connection,
    source_id: &SourceId,
    source_ann: &SourceAnn,
    filter: &SqlFilter,
) -> Result<Arc<crate::ann::EligibleRows>> {
    if &filter.source != source_id {
        bail!(
            "eligible filter source `{}` does not match ANN source `{source_id}`",
            filter.source
        );
    }
    let params = filter
        .params
        .iter()
        .map(FilterParamKey::from)
        .collect::<Vec<_>>();
    let key_bytes = params.iter().fold(filter.sql.len(), |total, value| {
        total.saturating_add(match value {
            FilterParamKey::Null => 1,
            FilterParamKey::Integer(_) | FilterParamKey::Real(_) => 8,
            FilterParamKey::Text(value) => value.len(),
            FilterParamKey::Blob(value) => value.len(),
        })
    });
    let cache_key = conn
        .path()
        .filter(|path| !path.is_empty())
        .map(std::fs::canonicalize)
        .transpose()
        .context("canonicalizing immutable corpus database path")?
        .filter(|_| key_bytes <= MAX_ELIGIBILITY_CACHE_KEY_BYTES)
        .map(|db_path| EligibilityCacheKey {
            db_path,
            generation: source_ann.generation.clone(),
            source: source_id.clone(),
            sql: filter.sql.clone(),
            params,
        });

    let load = || load_eligible_ann_items(conn, source_id, &source_ann.sidecar, filter);
    let Some(cache_key) = cache_key else {
        return load().map(Arc::new);
    };
    static CACHE: OnceLock<Mutex<EligibilityCache>> = OnceLock::new();
    let cell = {
        let mut cache = CACHE
            .get_or_init(|| Mutex::new(EligibilityCache::new(ELIGIBILITY_CACHE_CAPACITY)))
            .lock()
            .map_err(|_| anyhow!("eligibility cache lock poisoned"))?;
        cache.cell(cache_key)
    };
    cell.get_or_init(|| load().map(Arc::new).map_err(|error| error.to_string()))
        .clone()
        .map_err(|message| anyhow!(message))
}

fn load_eligible_ann_items(
    conn: &Connection,
    source_id: &SourceId,
    sidecar: &crate::ann::FlatAnn,
    filter: &SqlFilter,
) -> Result<crate::ann::EligibleRows> {
    if &filter.source != source_id {
        bail!("eligible filter source changed while its bitmap was materialized");
    }
    let where_filter = if filter.sql.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", filter.sql)
    };
    let sql = format!(
        r#"
        SELECT e.chunk_id
        FROM chunk_embeddings e
        JOIN chunks c ON c.chunk_id = e.chunk_id
        JOIN documents d
          ON d.source_id = c.source_id AND d.native_id = c.native_id
        {where_filter}
        "#
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(filter.params.clone()), |row| {
        row.get::<_, i64>("chunk_id")
    })?;
    // RoaringBitmap canonicalizes item order. Sorting hundreds of thousands
    // of eligible IDs in SQLite only creates a redundant temporary B-tree.
    let mut eligible = roaring::RoaringBitmap::new();
    for row in rows {
        let chunk_id = row?;
        let item_id = u32::try_from(chunk_id).map_err(|_| {
            anyhow!("installed ANN corpus contains out-of-range chunk_id {chunk_id}")
        })?;
        if !eligible.insert(item_id) {
            bail!("installed ANN corpus contains duplicate chunk_id {chunk_id}");
        }
    }
    sidecar.eligible_rows(&eligible)
}

fn exact_rerank_candidates(
    conn: &Connection,
    source_id: &SourceId,
    query_embedding: &[i8; EMBEDDING_DIM],
    candidates: &[crate::ann::AnnSearchHit],
    limit: usize,
) -> Result<Vec<VectorHit>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.embedding FROM chunk_embeddings AS e \
         JOIN chunks AS c ON c.chunk_id = e.chunk_id \
         WHERE c.source_id = ?1 AND e.chunk_id = ?2",
    )?;
    let mut seen = HashSet::with_capacity(candidates.len());
    let mut hits = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let chunk_id = i64::from(candidate.chunk_id);
        if !seen.insert(chunk_id) {
            bail!("ANN sidecar returned duplicate chunk_id {chunk_id}");
        }
        let embedding = stmt
            .query_row(params![source_id.as_str(), chunk_id], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .optional()?
            .ok_or_else(|| anyhow!("ANN sidecar returned unknown chunk_id {chunk_id}"))?;
        if embedding.len() != EMBEDDING_DIM {
            bail!(
                "stored embedding for chunk_id={chunk_id} has wrong length: got {}, expected {EMBEDDING_DIM}",
                embedding.len()
            );
        }
        let raw_score = query_embedding
            .iter()
            .zip(&embedding)
            .map(|(&query, &document)| i64::from(query) * i64::from(document as i8))
            .sum::<i64>();
        if raw_score != i64::from(candidate.score) {
            bail!(
                "ANN sidecar score for chunk_id={chunk_id} differs from authoritative SQLite embedding"
            );
        }
        hits.push((chunk_id, raw_score, dot_i8(query_embedding, &embedding)?));
    }
    hits.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    hits.truncate(limit);
    Ok(hits
        .into_iter()
        .map(|(chunk_id, _raw_score, normalized_score)| VectorHit {
            chunk_id,
            score: normalized_score,
        })
        .collect())
}

#[cfg(test)]
pub(crate) fn lexical_search(
    conn: &Connection,
    source_id: &SourceId,
    query: &str,
    filter: &SqlFilter,
    limit: usize,
) -> Result<Vec<VectorHit>> {
    let rowid_range = load_chunk_rowid_range(conn, source_id)?;
    lexical_search_in_range(conn, source_id, query, filter, limit, rowid_range)
}

fn lexical_search_in_range(
    conn: &Connection,
    source_id: &SourceId,
    query: &str,
    filter: &SqlFilter,
    limit: usize,
    rowid_range: Option<RowidRange>,
) -> Result<Vec<VectorHit>> {
    let strict_query = fts_strict_query(query);
    let Some(rowid_range) = rowid_range else {
        return Ok(Vec::new());
    };
    if strict_query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let mut hits =
        lexical_search_stage(conn, source_id, &strict_query, filter, limit, rowid_range)?;
    if hits.len() < limit {
        if let Some(broad_query) = fts_corroborated_query(query) {
            let broad =
                lexical_search_stage(conn, source_id, &broad_query, filter, limit, rowid_range)?;
            let mut seen = hits.iter().map(|hit| hit.chunk_id).collect::<HashSet<_>>();
            hits.extend(broad.into_iter().filter(|hit| seen.insert(hit.chunk_id)));
            hits.truncate(limit);
        }
    }
    Ok(hits)
}

fn lexical_search_stage(
    conn: &Connection,
    source_id: &SourceId,
    fts_query: &str,
    filter: &SqlFilter,
    limit: usize,
    rowid_range: RowidRange,
) -> Result<Vec<VectorHit>> {
    let where_filter = if filter.sql.is_empty() {
        String::new()
    } else {
        format!(" AND {}", filter.sql)
    };
    let sql = lexical_search_stage_sql(&where_filter);
    let mut params_vec = vec![
        Value::Text(fts_query.to_string()),
        Value::Integer(rowid_range.first),
        Value::Integer(rowid_range.last),
    ];
    params_vec.extend(filter.params.clone());
    params_vec.push(Value::Integer(limit as i64));

    let mut stmt = conn.prepare(&sql)?;
    debug_assert_eq!(&filter.source, source_id);
    let rows = stmt
        .query_map(params_from_iter(params_vec), |row| {
            Ok(VectorHit {
                chunk_id: row.get::<_, i64>("chunk_id")?,
                score: row.get::<_, f64>("score")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn lexical_search_stage_sql(where_filter: &str) -> String {
    format!(
        r#"
        SELECT f.rowid AS chunk_id, -bm25(chunks_fts) AS score
        FROM chunks_fts f
        CROSS JOIN chunks c ON c.chunk_id = f.rowid
        CROSS JOIN documents d
          ON d.source_id = c.source_id AND d.native_id = c.native_id
        WHERE chunks_fts MATCH ?
          AND f.rowid BETWEEN ? AND ? {where_filter}
        ORDER BY score DESC, chunk_id ASC
        LIMIT ?
        "#
    )
}

pub(crate) fn rrf_fuse(vector_hits: &[VectorHit], lexical_hits: &[VectorHit]) -> Vec<VectorHit> {
    // Hybrid ranking fuses vector and lexical ranks via RRF with K=60.
    const RRF_K: f64 = 60.0;
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for (rank, hit) in vector_hits.iter().enumerate() {
        scores
            .entry(hit.chunk_id)
            .and_modify(|score| *score += 1.0 / (RRF_K + rank as f64 + 1.0))
            .or_insert_with(|| 1.0 / (RRF_K + rank as f64 + 1.0));
    }
    for (rank, hit) in lexical_hits.iter().enumerate() {
        scores
            .entry(hit.chunk_id)
            .and_modify(|score| *score += 1.0 / (RRF_K + rank as f64 + 1.0))
            .or_insert_with(|| 1.0 / (RRF_K + rank as f64 + 1.0));
    }
    let mut out = scores
        .into_iter()
        .map(|(chunk_id, score)| VectorHit { chunk_id, score })
        .collect::<Vec<_>>();
    rank_hits(&mut out);
    out
}

// HTTP transport keeps one ServerState shared across worker threads.
// The semantic runtime is loaded lazily and reused across tool calls. Search-time
// inference holds the lock for one query embedding; read-only tools
// (get_chunks, get_definition, get_doc_anchors, get_asset, stats) run fully
// concurrently.

fn load_hits(
    conn: &Connection,
    source_id: &SourceId,
    generation: &str,
    chunk_ids: &[i64],
    query: &str,
    include_snippet: bool,
) -> Result<HashMap<i64, Hit>> {
    if chunk_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = vec!["?"; chunk_ids.len()].join(",");
    let text_column = if include_snippet { "c.text" } else { "NULL" };
    let sql = format!(
        r#"
        SELECT c.chunk_id, c.native_id, c.anchor, {text_column} AS text,
               d.type, d.title, d.date, d.canonical_url,
               d.withdrawn_date, d.superseded_by, d.replaces,
               d.has_in_doc_links, d.has_related_docs, d.has_history
        FROM chunks c
        JOIN documents d
          ON d.source_id = c.source_id AND d.native_id = c.native_id
        WHERE c.source_id = ? AND c.chunk_id IN ({placeholders})
        "#
    );
    let mut params = vec![Value::Text(source_id.as_str().to_string())];
    params.extend(chunk_ids.iter().copied().map(Value::Integer));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params), |row| {
        Ok((
            row.get::<_, i64>("chunk_id")?,
            row.get::<_, String>("native_id")?,
            row.get::<_, Option<String>>("anchor")?,
            row.get::<_, Option<Vec<u8>>>("text")?,
            row.get::<_, String>("type")?,
            row.get::<_, String>("title")?,
            row.get::<_, Option<String>>("date")?,
            row.get::<_, String>("canonical_url")?,
            row.get::<_, Option<String>>("withdrawn_date")?,
            row.get::<_, Option<String>>("superseded_by")?,
            row.get::<_, Option<String>>("replaces")?,
            row.get::<_, i64>("has_in_doc_links")?,
            row.get::<_, i64>("has_related_docs")?,
            row.get::<_, i64>("has_history")?,
        ))
    })?;
    let mut hits = HashMap::with_capacity(chunk_ids.len());
    for row in rows {
        let (
            chunk_id,
            native_id,
            anchor,
            text_blob,
            doc_type,
            title,
            date,
            canonical_url,
            withdrawn_date,
            superseded_by,
            replaces,
            has_in_doc_links,
            has_related_docs,
            has_history,
        ) = row?;
        let document = DocumentId {
            source: source_id.clone(),
            native_id,
        };
        let snippet = match text_blob {
            Some(blob) => {
                let raw_text = decompress_text(blob)?;
                let text = crate::retrieval::annotate_doc_refs(&raw_text, &document)?;
                Some(highlight_snippet(&text, query, SNIPPET_CHARS))
            }
            None => None,
        };
        let chunk = ChunkRef::new(
            generation,
            source_id.clone(),
            u64::try_from(chunk_id).map_err(|_| {
                anyhow!("stored chunk_id {chunk_id} cannot be represented publicly")
            })?,
        )?;
        let next_call = format!("get_chunks(chunks=[{}])", serde_json::to_string(&chunk)?);
        hits.insert(
            chunk_id,
            Hit {
                document,
                title,
                doc_type,
                date,
                anchor,
                snippet,
                canonical_url,
                chunk: Some(chunk),
                next_call: Some(next_call),
                withdrawn_date,
                superseded_by: superseded_by.map(|native_id| DocumentId {
                    source: source_id.clone(),
                    native_id,
                }),
                replaces: replaces.map(|native_id| DocumentId {
                    source: source_id.clone(),
                    native_id,
                }),
                has_in_doc_links: (has_in_doc_links != 0).then_some(true),
                has_related_docs: (has_related_docs != 0).then_some(true),
                has_history: (has_history != 0).then_some(true),
            },
        );
    }
    Ok(hits)
}

/// Tokenize a query into the same lowercase word forms used by [`fts_query`]
/// — short tokens are dropped to match FTS5's behaviour and to keep BM25
/// from being dominated by stopwords.
pub(crate) fn snippet_query_terms(query: &str) -> Vec<String> {
    let re = Regex::new(r"[A-Za-z0-9']+(?:-[A-Za-z0-9']+)*").expect("valid regex");
    re.find_iter(query)
        .map(|m| m.as_str().to_ascii_lowercase())
        .filter(|t| t.len() >= 2)
        .collect()
}

/// Score a window of `window_words` lowercase tokens against `query_terms`
/// using a self-IDF BM25 (the chunk *is* the corpus). Self-IDF is enough
/// to rank windows because rare-in-chunk terms are exactly what we want
/// the snippet to contain — no need to consult the global statistics.
pub(crate) fn bm25_score_window(
    window_words: &[&str],
    query_terms: &[String],
    chunk_term_freq: &HashMap<String, usize>,
    chunk_token_count: usize,
    avg_chunk_window_len: f64,
) -> f64 {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    if window_words.is_empty() {
        return 0.0;
    }
    let dl = window_words.len() as f64;
    let mut window_tf: HashMap<&str, usize> = HashMap::new();
    for w in window_words {
        *window_tf.entry(*w).or_insert(0) += 1;
    }
    let mut score = 0.0;
    for term in query_terms {
        let tf = match window_tf.get(term.as_str()) {
            Some(c) => *c as f64,
            None => continue,
        };
        // Self-IDF: rare in the surrounding chunk -> higher weight in the
        // window. Treat the chunk as a single "document corpus": idf is
        // log((N - df + 0.5) / (df + 0.5) + 1), where N is the number of
        // tokens in the chunk and df is the term's chunk-wide frequency.
        // This mirrors classic BM25 closely enough for the ranking task
        // (we only care about ordering windows, not absolute scores).
        let df = *chunk_term_freq.get(term).unwrap_or(&0) as f64;
        let n = chunk_token_count.max(1) as f64;
        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
        let denom = tf + K1 * (1.0 - B + B * dl / avg_chunk_window_len.max(1.0));
        score += idf * (tf * (K1 + 1.0)) / denom.max(1e-9);
    }
    score
}

/// Pick the highest-BM25 sliding window from `text` against `query`,
/// trim to `max_chars`, and return it. Heading text now lives inside
/// the chunk body (rendered inline via the chunker), so there is no
/// separate prefix to attach.
pub(crate) fn highlight_snippet(text: &str, query: &str, max_chars: usize) -> String {
    const WINDOW_WORDS: usize = 20;
    const STRIDE_WORDS: usize = 10;
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        return cleaned;
    }
    let query_terms = snippet_query_terms(query);
    if query_terms.is_empty() {
        // No tokens worth ranking against — fall back to the document's
        // opening fragment.
        return trim_chars(&cleaned, max_chars);
    }

    // Tokenise the cleaned text once. We keep both the lowercase form (for
    // BM25) and char-offsets into `cleaned` so we can rebuild the original
    // window verbatim after picking it.
    let token_re = Regex::new(r"[A-Za-z0-9']+(?:-[A-Za-z0-9']+)*").expect("valid regex");
    let mut tokens: Vec<(usize, usize, String)> = Vec::new();
    for m in token_re.find_iter(&cleaned) {
        tokens.push((m.start(), m.end(), m.as_str().to_ascii_lowercase()));
    }
    if tokens.is_empty() {
        return trim_chars(&cleaned, max_chars);
    }

    let mut chunk_term_freq: HashMap<String, usize> = HashMap::new();
    for (_, _, lower) in &tokens {
        *chunk_term_freq.entry(lower.clone()).or_insert(0) += 1;
    }
    let chunk_token_count = tokens.len();

    let n = tokens.len();
    let mut best_score = f64::NEG_INFINITY;
    let mut best_start_token = 0usize;
    let avg_window_len = WINDOW_WORDS.min(n) as f64;
    let mut idx = 0usize;
    let mut produced_any = false;
    while idx < n {
        let end = (idx + WINDOW_WORDS).min(n);
        let window_lower: Vec<&str> = tokens[idx..end].iter().map(|t| t.2.as_str()).collect();
        let score = bm25_score_window(
            &window_lower,
            &query_terms,
            &chunk_term_freq,
            chunk_token_count,
            avg_window_len,
        );
        if score > best_score {
            best_score = score;
            best_start_token = idx;
        }
        produced_any = true;
        if end == n {
            break;
        }
        idx += STRIDE_WORDS;
    }
    if !produced_any {
        return trim_chars(&cleaned, max_chars);
    }

    // Expand the chosen window outward to fill the snippet budget while
    // staying centred on the high-density region. We do this in characters
    // because the budget is character-bounded.
    let win_start_char = tokens[best_start_token].0;
    let win_end_token = (best_start_token + WINDOW_WORDS).min(n) - 1;
    let win_end_char = tokens[win_end_token].1;
    let center = (win_start_char + win_end_char) / 2;
    let half = max_chars / 2;
    let mut start = center.saturating_sub(half);
    while start > 0 && !cleaned.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + max_chars).min(cleaned.len());
    while end < cleaned.len() && !cleaned.is_char_boundary(end) {
        end += 1;
    }
    let mut snippet = cleaned[start..end].to_string();
    if start > 0 {
        snippet.insert_str(0, "...");
    }
    if end < cleaned.len() {
        snippet.push_str("...");
    }
    snippet
}

pub(crate) fn trim_chars(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let mut end = max_chars;
    while end < s.len() && !s.is_char_boundary(end) {
        end += 1;
    }
    s[..end].to_string()
}

/// Title-level hits for a query use BM25 over the source-qualified
/// `title_fts` table. A parallel algorithm to
/// chunk search — `search` calls this to populate its `title_hits`
/// sidebar. The caller supplies the connection and the already-built
/// document filter so chunk and title queries stay consistently scoped.
#[cfg(test)]
pub(crate) fn collect_title_hits(
    conn: &Connection,
    source_id: &SourceId,
    query: &str,
    k: usize,
    filter: &SqlFilter,
) -> Result<Vec<Hit>> {
    let rowid_range = load_title_rowid_range(conn, source_id)?;
    collect_title_hits_in_range(conn, source_id, query, k, filter, rowid_range)
}

fn collect_title_hits_in_range(
    conn: &Connection,
    source_id: &SourceId,
    query: &str,
    k: usize,
    filter: &SqlFilter,
    rowid_range: Option<RowidRange>,
) -> Result<Vec<Hit>> {
    // Title hits rank title_fts independently and reuse the chunk
    // query's document filter.
    let k = k.clamp(1, 100);
    let mut hits = Vec::new();
    if let Some(direct) = query_direct_document_hit(conn, source_id, query.trim(), filter)? {
        hits.push(direct);
    }
    let strict_query = fts_strict_query(query);
    if strict_query.is_empty() {
        return Ok(hits);
    }
    let mut seen = hits
        .iter()
        .map(|hit| hit.document.clone())
        .collect::<HashSet<_>>();
    if let Some(rowid_range) = rowid_range {
        hits.extend(
            query_title_fts(conn, source_id, &strict_query, filter, k, rowid_range)?
                .into_iter()
                .filter(|hit| seen.insert(hit.document.clone())),
        );
    }
    if hits.len() < k {
        if let (Some(broad_query), Some(rowid_range)) = (fts_broad_query(query), rowid_range) {
            hits.extend(
                query_title_fts(conn, source_id, &broad_query, filter, k, rowid_range)?
                    .into_iter()
                    .filter(|hit| seen.insert(hit.document.clone())),
            );
        }
    }
    hits.truncate(k);
    Ok(hits)
}

fn query_direct_document_hit(
    conn: &Connection,
    source_id: &SourceId,
    native_id: &str,
    filter: &SqlFilter,
) -> Result<Option<Hit>> {
    if native_id.is_empty() {
        return Ok(None);
    }
    let where_filter = if filter.sql.is_empty() {
        String::new()
    } else {
        format!(" AND {}", filter.sql)
    };
    let sql = format!(
        r#"
        SELECT d.native_id, d.type, d.title, d.date, d.canonical_url,
               d.withdrawn_date, d.superseded_by, d.replaces,
               d.has_in_doc_links, d.has_related_docs, d.has_history
        FROM documents d
        WHERE d.native_id = ? {where_filter}
        LIMIT 1
        "#
    );
    let mut params = vec![Value::Text(native_id.to_string())];
    params.extend(filter.params.clone());
    debug_assert_eq!(&filter.source, source_id);
    conn.query_row(&sql, params_from_iter(params), |row| {
        let native_id: String = row.get("native_id")?;
        let title: String = row.get("title")?;
        Ok(Hit {
            canonical_url: row.get("canonical_url")?,
            document: DocumentId {
                source: source_id.clone(),
                native_id,
            },
            title: title.clone(),
            doc_type: row.get("type")?,
            date: row.get("date")?,
            anchor: None,
            snippet: Some(title),
            chunk: None,
            next_call: None,
            withdrawn_date: row.get("withdrawn_date")?,
            superseded_by: row
                .get::<_, Option<String>>("superseded_by")?
                .map(|native_id| DocumentId {
                    source: source_id.clone(),
                    native_id,
                }),
            replaces: row
                .get::<_, Option<String>>("replaces")?
                .map(|native_id| DocumentId {
                    source: source_id.clone(),
                    native_id,
                }),
            has_in_doc_links: (row.get::<_, i64>("has_in_doc_links")? != 0).then_some(true),
            has_related_docs: (row.get::<_, i64>("has_related_docs")? != 0).then_some(true),
            has_history: (row.get::<_, i64>("has_history")? != 0).then_some(true),
        })
    })
    .optional()
    .map_err(Into::into)
}

fn query_title_fts(
    conn: &Connection,
    source_id: &SourceId,
    title_query: &str,
    filter: &SqlFilter,
    limit: usize,
    rowid_range: RowidRange,
) -> Result<Vec<Hit>> {
    let where_filter = if filter.sql.is_empty() {
        String::new()
    } else {
        format!(" AND {}", filter.sql)
    };
    let sql = title_search_stage_sql(&where_filter);
    let mut params_vec = vec![
        Value::Text(title_query.to_string()),
        Value::Integer(rowid_range.first),
        Value::Integer(rowid_range.last),
    ];
    params_vec.extend(filter.params.clone());
    params_vec.push(Value::Integer(limit as i64));

    let mut stmt = conn.prepare(&sql)?;
    debug_assert_eq!(&filter.source, source_id);
    let rows = stmt
        .query_map(params_from_iter(params_vec), |row| {
            let native_id: String = row.get("native_id")?;
            let title: String = row.get("title")?;
            Ok(Hit {
                canonical_url: row.get("canonical_url")?,
                document: DocumentId {
                    source: source_id.clone(),
                    native_id,
                },
                title: title.clone(),
                doc_type: row.get("type")?,
                date: row.get("date")?,
                anchor: None,
                snippet: Some(title),
                chunk: None,
                next_call: None,
                withdrawn_date: row.get("withdrawn_date")?,
                superseded_by: row
                    .get::<_, Option<String>>("superseded_by")?
                    .map(|native_id| DocumentId {
                        source: source_id.clone(),
                        native_id,
                    }),
                replaces: row
                    .get::<_, Option<String>>("replaces")?
                    .map(|native_id| DocumentId {
                        source: source_id.clone(),
                        native_id,
                    }),
                has_in_doc_links: row
                    .get::<_, i64>("has_in_doc_links")
                    .ok()
                    .filter(|v| *v != 0)
                    .map(|_| true),
                has_related_docs: row
                    .get::<_, i64>("has_related_docs")
                    .ok()
                    .filter(|v| *v != 0)
                    .map(|_| true),
                has_history: row
                    .get::<_, i64>("has_history")
                    .ok()
                    .filter(|v| *v != 0)
                    .map(|_| true),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn title_search_stage_sql(where_filter: &str) -> String {
    format!(
        r#"
        SELECT t.native_id AS native_id, bm25(title_fts) AS score,
               d.type, d.title, d.date, d.canonical_url,
               d.withdrawn_date, d.superseded_by, d.replaces,
               d.has_in_doc_links, d.has_related_docs, d.has_history
        FROM title_fts t
        CROSS JOIN documents d
          ON d.source_id = t.source_id AND d.native_id = t.native_id
        WHERE title_fts MATCH ?
          AND t.rowid BETWEEN ? AND ? {where_filter}
        ORDER BY score ASC, t.source_id ASC, t.native_id ASC
        LIMIT ?
        "#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{compress_text, init_db};

    fn source() -> SourceId {
        "ato".parse().expect("valid source")
    }

    fn insert_lexical_fixture_row(
        conn: &Connection,
        source_id: &str,
        chunk_id: i64,
        text: &str,
    ) -> Result<()> {
        let native_id = format!("{source_id}-{chunk_id}");
        conn.execute(
            "INSERT INTO documents(
                 source_id, native_id, type, title, canonical_url,
                 downloaded_at, content_hash, html
             ) VALUES (?1, ?2, 'Act', ?2, ?3, '2026-01-01T00:00:00Z', ?2, X'00')",
            params![
                source_id,
                native_id,
                format!("https://example.invalid/{source_id}/{chunk_id}")
            ],
        )?;
        conn.execute(
            "INSERT INTO chunks(chunk_id, source_id, native_id, ord, text)
             VALUES (?1, ?2, ?3, 0, ?4)",
            params![chunk_id, source_id, native_id, compress_text(text)?],
        )?;
        conn.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)",
            params![chunk_id, text],
        )?;
        Ok(())
    }

    fn lexical_source_fixture() -> Result<Connection> {
        let conn = Connection::open_in_memory()?;
        init_db(&conn)?;
        conn.execute_batch(
            "INSERT INTO sources(source_id, display_name) VALUES
                 ('ato', 'ATO'),
                 ('frl', 'Federal Register of Legislation');",
        )?;
        for chunk_id in 1..=3 {
            insert_lexical_fixture_row(&conn, "ato", chunk_id, "alpha beta gamma global decoy")?;
        }
        insert_lexical_fixture_row(&conn, "frl", 100, "alpha gamma corroborated")?;
        insert_lexical_fixture_row(&conn, "frl", 101, "alpha singleton")?;
        insert_lexical_fixture_row(&conn, "frl", 102, "gamma singleton")?;
        Ok(conn)
    }

    fn unrestricted_filter(source_id: &SourceId) -> SqlFilter {
        build_doc_filter(
            "d",
            DocumentFilterSpec {
                source_id,
                types: None,
                date_from: None,
                date_to: None,
                doc_scope: None,
                include_old: true,
                current_only: false,
            },
        )
    }

    #[test]
    fn fts_terms_are_bounded_deduplicated_and_drop_stopwords() {
        let repeated = std::iter::repeat_n("the Tax tax", 40)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(fts_terms(&repeated), vec!["\"Tax\"".to_string()]);
        assert_eq!(
            fts_terms("develop development alpha"),
            vec!["\"develop\"".to_string(), "\"alpha\"".to_string()],
            "Porter-equivalent forms must not count as independent terms"
        );
        let many = (0..100)
            .map(|n| format!("term{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(fts_terms(&many).len(), MAX_FTS_TERMS);
    }

    #[test]
    fn fts_broadening_is_a_distinct_second_stage() {
        assert_eq!(
            fts_strict_query("capital gains tax"),
            "\"capital\" \"gains\" \"tax\""
        );
        assert_eq!(
            fts_broad_query("capital gains tax").as_deref(),
            Some("\"capital\" OR \"gains\" OR \"tax\"")
        );
        let corroborated = fts_corroborated_query("capital gains tax").expect("relaxed query");
        assert!(corroborated.contains(" AND "));
        assert_ne!(corroborated, "\"capital\" OR \"gains\" OR \"tax\"");
        assert!(fts_corroborated_query("develop development alpha").is_none());
    }

    #[test]
    fn broad_fallback_requires_corroboration_but_keeps_every_term_pair() -> Result<()> {
        let conn = lexical_source_fixture()?;
        let source: SourceId = "frl".parse()?;
        let filter = unrestricted_filter(&source);

        let hits = lexical_search(&conn, &source, "alpha beta gamma", &filter, 10)?;
        assert_eq!(
            hits.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>(),
            vec![100],
            "the non-adjacent alpha/gamma pair should survive, while singleton and other-source postings must not"
        );

        let short_query_hits = lexical_search(&conn, &source, "alpha beta", &filter, 10)?;
        assert!(
            short_query_hits.is_empty(),
            "two-term chunk search must not degrade to a one-term posting scan"
        );
        Ok(())
    }

    #[test]
    fn source_rowid_ranges_allow_gaps_but_fail_closed_on_interleaving() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE chunks(chunk_id INTEGER PRIMARY KEY, source_id TEXT NOT NULL);
             CREATE VIRTUAL TABLE title_fts USING fts5(
                 source_id UNINDEXED, native_id UNINDEXED, title, headings
             );
             INSERT INTO chunks VALUES (1, 'ato'), (2, 'ato');
             INSERT INTO title_fts(rowid, source_id, native_id, title, headings) VALUES
                 (1, 'ato', 'a', 'alpha', ''),
                 (2, 'ato', 'b', 'beta', '');",
        )?;
        assert_eq!(
            load_chunk_rowid_range(&conn, &source())?,
            Some(RowidRange { first: 1, last: 2 })
        );
        assert_eq!(
            load_title_rowid_range(&conn, &source())?,
            Some(RowidRange { first: 1, last: 2 })
        );

        conn.execute("UPDATE chunks SET chunk_id = 3 WHERE chunk_id = 2", [])?;
        assert_eq!(
            load_chunk_rowid_range(&conn, &source())?,
            Some(RowidRange { first: 1, last: 3 })
        );
        conn.execute("INSERT INTO chunks VALUES (2, 'frl')", [])?;
        let chunk_error = load_chunk_rowid_range(&conn, &source()).unwrap_err();
        assert!(chunk_error.to_string().contains("one isolated range"));

        conn.execute_batch(
            "INSERT INTO title_fts(rowid, source_id, native_id, title, headings) VALUES
                 (3, 'frl', 'c', 'gamma', ''),
                 (4, 'ato', 'd', 'delta', '');",
        )?;
        let title_error = load_title_rowid_range(&conn, &source()).unwrap_err();
        assert!(title_error.to_string().contains("one isolated range"));
        Ok(())
    }

    #[test]
    fn fts_query_plans_apply_rowid_bounds_inside_both_virtual_tables() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        init_db(&conn)?;
        let source: SourceId = "frl".parse()?;
        let filter = unrestricted_filter(&source);
        let where_filter = format!(" AND {}", filter.sql);

        for (alias, query) in [
            ("f", lexical_search_stage_sql(&where_filter)),
            ("t", title_search_stage_sql(&where_filter)),
        ] {
            let explain = format!("EXPLAIN QUERY PLAN {query}");
            let mut values = vec![
                Value::Text("alpha".to_string()),
                Value::Integer(100),
                Value::Integer(200),
            ];
            values.extend(filter.params.clone());
            values.push(Value::Integer(10));
            let mut statement = conn.prepare(&explain)?;
            let plan = statement
                .query_map(params_from_iter(values), |row| row.get::<_, String>(3))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let fts_step = plan
                .iter()
                .find(|step| step.contains("VIRTUAL TABLE"))
                .unwrap_or_else(|| panic!("missing FTS step in plan: {plan:?}"));
            assert!(
                fts_step.starts_with(&format!("SCAN {alias} ")) && fts_step.contains("><"),
                "FTS rowid bounds were not consumed by the virtual table: {plan:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn document_filter_always_binds_the_resolved_source_first() {
        let source = source();
        let filter = build_doc_filter(
            "d",
            DocumentFilterSpec {
                source_id: &source,
                types: None,
                date_from: None,
                date_to: None,
                doc_scope: None,
                include_old: true,
                current_only: false,
            },
        );
        assert_eq!(filter.source, source);
        assert!(filter.sql.starts_with("d.source_id = ?"));
        assert_eq!(filter.params[0], Value::Text("ato".to_string()));
    }

    #[test]
    fn hybrid_ties_use_chunk_id_order() {
        let vector = vec![
            VectorHit {
                chunk_id: 2,
                score: 1.0,
            },
            VectorHit {
                chunk_id: 1,
                score: 1.0,
            },
        ];
        let lexical = vec![
            VectorHit {
                chunk_id: 1,
                score: 1.0,
            },
            VectorHit {
                chunk_id: 2,
                score: 1.0,
            },
        ];
        let fused = rrf_fuse(&vector, &lexical);
        assert_eq!(
            fused.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn ann_candidate_rerank_uses_authoritative_scores_and_stable_ties() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE chunks(\
                 chunk_id INTEGER PRIMARY KEY, source_id TEXT NOT NULL\
             );\
             CREATE TABLE chunk_embeddings(\
                 chunk_id INTEGER PRIMARY KEY, embedding BLOB NOT NULL\
             );\
             INSERT INTO chunks VALUES (9, 'ato'), (3, 'ato');",
        )?;
        let embedding = vec![1u8; EMBEDDING_DIM];
        conn.execute(
            "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (9, ?1)",
            params![&embedding],
        )?;
        conn.execute(
            "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (3, ?1)",
            params![&embedding],
        )?;
        let hits = exact_rerank_candidates(
            &conn,
            &source(),
            &[1i8; EMBEDDING_DIM],
            &[
                crate::ann::AnnSearchHit {
                    chunk_id: 9,
                    score: EMBEDDING_DIM as i32,
                },
                crate::ann::AnnSearchHit {
                    chunk_id: 3,
                    score: EMBEDDING_DIM as i32,
                },
            ],
            2,
        )?;
        assert_eq!(
            hits.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>(),
            vec![3, 9]
        );
        let duplicate = crate::ann::AnnSearchHit {
            chunk_id: 3,
            score: EMBEDDING_DIM as i32,
        };
        let error = exact_rerank_candidates(
            &conn,
            &source(),
            &[1i8; EMBEDDING_DIM],
            &[duplicate, duplicate],
            2,
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate"));
        let error = exact_rerank_candidates(
            &conn,
            &source(),
            &[1i8; EMBEDDING_DIM],
            &[crate::ann::AnnSearchHit {
                chunk_id: 3,
                score: 0,
            }],
            1,
        )
        .unwrap_err();
        assert!(error.to_string().contains("authoritative SQLite"));
        Ok(())
    }

    #[test]
    fn eligibility_cache_reuses_exact_keys_and_evicts_oldest_entries() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let source: SourceId = "ato".parse().expect("valid source");
        let key = |suffix: &str| EligibilityCacheKey {
            db_path: PathBuf::from(format!("/immutable/{suffix}/legal.db")),
            generation: suffix.repeat(64),
            source: source.clone(),
            sql: "d.source_id = ? AND d.type = ?".to_string(),
            params: vec![
                FilterParamKey::Text("ato".to_string()),
                FilterParamKey::Text(suffix.to_string()),
            ],
        };
        let mut cache = EligibilityCache::new(2);
        let first = cache.cell(key("a"));
        let second = cache.cell(key("b"));
        let repeated = cache.cell(key("a"));
        assert!(Arc::ptr_eq(&first, &repeated));
        let loads = AtomicUsize::new(0);
        let loaded = first.get_or_init(|| {
            loads.fetch_add(1, Ordering::Relaxed);
            Ok(Arc::new(crate::ann::EligibleRows::empty_for_test(0)))
        });
        assert!(loaded.is_ok());
        let loaded_again = repeated.get_or_init(|| {
            loads.fetch_add(1, Ordering::Relaxed);
            Ok(Arc::new(crate::ann::EligibleRows::empty_for_test(0)))
        });
        assert!(loaded_again.is_ok());
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        let third = cache.cell(key("c"));
        assert_eq!(cache.entries.len(), 2);
        assert!(cache.entries.contains_key(&key("a")));
        assert!(!cache.entries.contains_key(&key("b")));
        assert!(cache.entries.contains_key(&key("c")));
        assert!(!Arc::ptr_eq(&second, &third));
    }

    #[test]
    fn repeated_installed_filter_uses_cached_bitmap_without_rescanning_sqlite() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let db_path = directory.path().join("legal.db");
        let conn = Connection::open(&db_path)?;
        init_db(&conn)?;
        conn.execute(
            "INSERT INTO sources(source_id, display_name) VALUES ('ato', 'ATO')",
            [],
        )?;
        conn.execute(
            "INSERT INTO documents(
                 source_id, native_id, type, title, canonical_url,
                 downloaded_at, content_hash, html
             ) VALUES ('ato', 'one', 'KEEP', 'One', 'https://example.invalid/one',
                       '2026-01-01T00:00:00Z', 'hash', X'00')",
            [],
        )?;
        conn.execute(
            "INSERT INTO chunks(chunk_id, source_id, native_id, ord, text)
             VALUES (1, 'ato', 'one', 0, ?1)",
            params![compress_text("one")?],
        )?;
        conn.execute(
            "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (1, ?1)",
            params![vec![1u8; EMBEDDING_DIM]],
        )?;
        let source = source();
        let source_sha = "a".repeat(64);
        let identity = crate::ann::compute_identity(&conn, &source, &source_sha)?;
        let info =
            crate::ann::build_sidecar(&conn, &source, directory.path(), &source_sha, &identity)?;
        let sidecar = Arc::new(crate::ann::open_verified_sidecar(
            &directory
                .path()
                .join(crate::ann::sidecar_relative_path(&source)),
            &source,
            &info,
        )?);
        let source_ann = SourceAnn {
            sidecar,
            generation: "f".repeat(64),
        };
        let types = vec!["KEEP".to_string()];
        let filter = build_doc_filter(
            "d",
            DocumentFilterSpec {
                source_id: &source,
                types: Some(&types),
                date_from: None,
                date_to: None,
                doc_scope: None,
                include_old: true,
                current_only: false,
            },
        );
        let first = eligible_ann_items(&conn, &source, &source_ann, &filter)?;
        assert_eq!(first.len(), 1);
        // Production cache keys name immutable generation databases. This
        // test-only mutation is a sentinel: a second SQLite scan would now
        // return no rows, while a cache hit retains the first bitmap.
        conn.execute(
            "UPDATE documents SET type = 'CHANGED' WHERE source_id = 'ato' AND native_id = 'one'",
            [],
        )?;
        let repeated = eligible_ann_items(&conn, &source, &source_ann, &filter)?;
        assert!(Arc::ptr_eq(&first, &repeated));
        assert_eq!(repeated.len(), 1);
        Ok(())
    }

    #[test]
    fn search_continuation_preserves_metadata_only_hydration() {
        let opts = SearchOptions {
            source: source(),
            k: 5,
            types: None,
            date_from: None,
            date_to: None,
            doc_scope: None,
            mode: SearchMode::Keyword,
            sort_by: SortBy::Relevance,
            include_old: false,
            current_only: true,
            max_per_doc: 1,
            include_snippet: false,
            similar_to_chunk: None,
            seed_text: None,
        };
        let next = search_next_call("tax", 10, &opts).expect("valid continuation");
        assert!(next.contains("source=\"ato\""));
        assert!(next.contains("include_snippet=false"));
    }
}
