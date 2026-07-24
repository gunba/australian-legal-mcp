//! Hybrid BM25 + vector search with candidate deduplication, RRF fusion,
//! bounded snippet rendering, and direct title hits.

use crate::config::{active_generation_key, ann_path};
use crate::db::{decompress_text, get_corpus_meta, get_source_meta, open_read, table_exists};
use crate::legal_source::source_registry;
use crate::search_timing::{SearchPhase, SearchRequestTiming, SearchTimings};
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
use serde::Serialize;
use serde_json::{json, Value as JsonValue};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(test)]
pub(crate) fn fts_query(query: &str) -> String {
    fts_strict_query(query)
}

const MAX_FTS_INPUT_CHARS: usize = 4_096;
const MAX_FTS_TERMS: usize = 16;
const FTS_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "was", "were", "with",
];

fn fts_terms(query: &str) -> Vec<String> {
    let re = Regex::new(r"[A-Za-z0-9']+(?:-[A-Za-z0-9']+)*").expect("valid regex");
    let bounded = query.chars().take(MAX_FTS_INPUT_CHARS).collect::<String>();
    let mut seen = HashSet::new();
    re.find_iter(&bounded)
        .map(|m| m.as_str().to_string())
        .filter(|term| {
            let normalized = term.to_ascii_lowercase();
            term.len() >= 2
                && !FTS_STOPWORDS.contains(&normalized.as_str())
                // SQLite owns Porter stemming. Deduplicate only identical
                // normalized source terms so every SQLite-effective term
                // remains in the strict conjunction, independent of order.
                && seen.insert(normalized)
        })
        .take(MAX_FTS_TERMS)
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect()
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

fn type_glob_to_like(pattern: &str) -> String {
    let mut out = String::new();
    for ch in pattern.chars() {
        match ch {
            '*' => out.push('%'),
            '%' | '_' | '\\' => {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LexicalBounds {
    Empty,
    Unbounded,
    Range(RowidRange),
}

fn scoped_lexical_bounds(conn: &Connection, doc_scope: Option<&str>) -> Result<LexicalBounds> {
    let Some(doc_scope) = doc_scope else {
        return Ok(LexicalBounds::Unbounded);
    };
    let mut statement = conn.prepare(
        "SELECT first_chunk_id, last_chunk_id
         FROM document_filter
         WHERE native_id LIKE ?1 ESCAPE '\\'
         ORDER BY native_id COLLATE BINARY
         LIMIT 2",
    )?;
    let mut rows = statement.query([glob_to_like(doc_scope)])?;
    let Some(first) = rows.next()? else {
        return Ok(LexicalBounds::Empty);
    };
    let range = match (
        first.get::<_, Option<i64>>(0)?,
        first.get::<_, Option<i64>>(1)?,
    ) {
        (None, None) => LexicalBounds::Empty,
        (Some(first), Some(last)) if first > 0 && first <= last => {
            LexicalBounds::Range(RowidRange { first, last })
        }
        _ => bail!("lexical document chunk range metadata is inconsistent"),
    };
    if rows.next()?.is_some() {
        Ok(LexicalBounds::Unbounded)
    } else {
        Ok(range)
    }
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

#[derive(Clone, Copy)]
enum FilterStore {
    Corpus,
    Lexical,
}

pub(crate) fn build_doc_filter(alias: &str, spec: DocumentFilterSpec<'_>) -> SqlFilter {
    build_doc_filter_for(alias, spec, FilterStore::Corpus)
}

pub(crate) fn build_lexical_doc_filter(alias: &str, spec: DocumentFilterSpec<'_>) -> SqlFilter {
    build_doc_filter_for(alias, spec, FilterStore::Lexical)
}

fn build_doc_filter_for(
    alias: &str,
    spec: DocumentFilterSpec<'_>,
    store: FilterStore,
) -> SqlFilter {
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
    let (mut clauses, mut params_out) = if matches!(store, FilterStore::Corpus) {
        (
            vec![format!("{alias}.source_id = ?")],
            vec![Value::Text(source_id.as_str().to_string())],
        )
    } else {
        (Vec::new(), Vec::new())
    };

    if let Some(types) = types.filter(|types| !types.is_empty()) {
        let mut ors = Vec::new();
        for t in types {
            if t.contains('*') {
                ors.push(format!("{alias}.type LIKE ? ESCAPE '\\'"));
                params_out.push(Value::Text(type_glob_to_like(t)));
            } else {
                ors.push(format!("{alias}.type = ?"));
                params_out.push(Value::Text(t.clone()));
            }
        }
        clauses.push(format!("({})", ors.join(" OR ")));
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
    if source_id.as_str() == "ato" && !include_old {
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
        let superseded = if matches!(store, FilterStore::Corpus) {
            format!("{alias}.superseded_by IS NULL")
        } else {
            format!("{alias}.is_superseded = 0")
        };
        clauses.push(format!("{alias}.withdrawn_date IS NULL AND {superseded}"));
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
    pub(crate) date: Option<String>,
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
    search_with_request_timing(query, opts, server_state, None)
}

pub(crate) fn search_with_request_timing(
    query: &str,
    opts: SearchOptions<'_>,
    server_state: Option<&ServerState>,
    request_timing: Option<SearchRequestTiming>,
) -> Result<String> {
    let (result, timing_record) = execute_timed_search(query, opts, server_state, request_timing);
    if let Some(record) = timing_record {
        eprintln!("{record}");
    }
    result
}

fn execute_timed_search(
    query: &str,
    opts: SearchOptions<'_>,
    server_state: Option<&ServerState>,
    request_timing: Option<SearchRequestTiming>,
) -> (Result<String>, Option<JsonValue>) {
    let effective_mode = if opts.similar_to_chunk.is_some()
        || opts
            .seed_text
            .map(str::trim)
            .is_some_and(|seed| !seed.is_empty())
    {
        SearchMode::Vector
    } else {
        opts.mode
    };
    let mut timings = SearchTimings::new(request_timing, effective_mode);
    let result = search_inner(query, opts, server_state, effective_mode, &mut timings);
    let timing_record = timings.finish(result.is_ok());
    (result, timing_record)
}

#[cfg(test)]
pub(crate) fn search_with_timing_record(
    query: &str,
    opts: SearchOptions<'_>,
    server_state: Option<&ServerState>,
    request_timing: SearchRequestTiming,
) -> (Result<String>, JsonValue) {
    let (result, record) = execute_timed_search(query, opts, server_state, Some(request_timing));
    (
        result,
        record.expect("a supplied request timing context produces one timing record"),
    )
}

fn search_inner(
    query: &str,
    opts: SearchOptions<'_>,
    server_state: Option<&ServerState>,
    effective_mode: SearchMode,
    timings: &mut SearchTimings,
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
    let corpus_filter = build_doc_filter(
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
                timings.measure(SearchPhase::Embedding, || {
                    load_chunk_embedding(&conn, &resolved_source, seed_id)
                })?,
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
    let lexical_filter = (!is_seed_search).then(|| {
        build_lexical_doc_filter(
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
        )
    });
    let (lexical_conn, lexical_bounds) = timings.measure(SearchPhase::LexicalIndex, || {
        let installed_manifest = load_active_generation_manifest()?
            .ok_or_else(|| anyhow!("active generation manifest is missing"))?;
        let info = installed_manifest
            .lexical
            .get(&resolved_source)
            .ok_or_else(|| {
                anyhow!("active manifest has no lexical sidecar for source `{resolved_source}`")
            })?;
        let lexical_conn = crate::lexical::open_runtime_sidecar(
            &conn,
            &resolved_source,
            info,
            &installed_manifest.db.sha256,
        )?;
        let lexical_bounds = scoped_lexical_bounds(&lexical_conn, opts.doc_scope)?;
        Ok((lexical_conn, lexical_bounds))
    })?;
    let source_ann = if matches!(effective_mode, SearchMode::Hybrid | SearchMode::Vector) {
        Some(timings.measure(SearchPhase::VectorScan, || {
            ensure_vector_search_ready(&conn, &resolved_source)
        })?)
    } else {
        None
    };
    let lexical_hits = if matches!(effective_mode, SearchMode::Hybrid | SearchMode::Keyword) {
        timings.measure(SearchPhase::LexicalIndex, || {
            lexical_search_in_range(
                &lexical_conn,
                &resolved_source,
                query,
                lexical_filter
                    .as_ref()
                    .ok_or_else(|| anyhow!("lexical search did not prepare its source filter"))?,
                internal_limit,
                lexical_bounds,
            )
        })?
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
                timings.measure(SearchPhase::Embedding, || match server_state {
                    Some(state) => state.encode_query_embedding(embed_input),
                    None => encode_query_embedding(embed_input),
                })?
            };
            let vector_hits = timings.measure(SearchPhase::VectorScan, || {
                vector_search(
                    &conn,
                    &resolved_source,
                    source_ann,
                    &query_embedding,
                    &corpus_filter,
                    internal_limit,
                )
            })?;
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
                timings.measure_value(SearchPhase::Fusion, || {
                    rrf_fuse(&vector_hits, &lexical_hits)
                })
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
    let candidate_meta = timings.measure(SearchPhase::LexicalIndex, || {
        load_candidate_meta(&lexical_conn, &resolved_source, &ranked_hits)
    })?;
    let mut deduped = dedup_per_doc(ranked_hits, &candidate_meta, frontier, max_per_doc);
    if matches!(opts.sort_by, SortBy::Recency) {
        deduped.sort_by(|a, b| {
            let a_meta = candidate_meta
                .get(&a.chunk_id)
                .expect("deduped candidate has lexical metadata");
            let b_meta = candidate_meta
                .get(&b.chunk_id)
                .expect("deduped candidate has lexical metadata");
            b_meta
                .date
                .cmp(&a_meta.date)
                .then_with(|| a_meta.document.cmp(&b_meta.document))
                .then_with(|| a.chunk_id.cmp(&b.chunk_id))
        });
        deduped.truncate(k);
    }

    let ranked_ids = deduped.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>();
    let records = timings.measure(SearchPhase::PayloadHydration, || {
        let mut hydrated = load_hits(
            &conn,
            &resolved_source,
            &generation,
            &ranked_ids,
            query,
            opts.include_snippet,
        )?;
        if hydrated.len() != ranked_ids.len() {
            bail!("search sidecar returned an unknown legal.db chunk");
        }
        ranked_ids
            .into_iter()
            .map(|chunk_id| {
                hydrated
                    .remove(&chunk_id)
                    .ok_or_else(|| anyhow!("search winner disappeared during hydration"))
            })
            .collect::<Result<Vec<_>>>()
    })?;
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
            &lexical_conn,
            &resolved_source,
            query,
            TITLE_HITS_K,
            (
                &corpus_filter,
                lexical_filter
                    .as_ref()
                    .ok_or_else(|| anyhow!("title search did not prepare its source filter"))?,
            ),
            Some(timings),
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
    lexical: &Connection,
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
    let sql = format!(
        "SELECT c.chunk_id, d.native_id, d.date, c.is_intro \
         FROM chunk_filter AS c \
         JOIN document_filter AS d ON d.doc_key = c.doc_key \
         WHERE c.chunk_id IN ({placeholders})"
    );
    let expected = ids.len();
    let mut stmt = lexical.prepare(&sql)?;
    let rows = stmt.query_map(
        params_from_iter(ids.into_iter().map(Value::Integer)),
        |row| {
            let chunk_id: i64 = row.get("chunk_id")?;
            let native_id: String = row.get("native_id")?;
            let date: Option<String> = row.get("date")?;
            let is_intro = row.get::<_, i64>("is_intro")? != 0;
            Ok((chunk_id, native_id, date, is_intro))
        },
    )?;
    let mut out = HashMap::new();
    for row in rows {
        let (chunk_id, native_id, date, is_intro) = row?;
        out.insert(
            chunk_id,
            CandidateMeta {
                document: DocumentId {
                    source: source_id.clone(),
                    native_id,
                },
                date,
                is_intro,
            },
        );
    }
    if out.len() != expected {
        bail!("search candidate is absent from its source lexical sidecar");
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
    lexical: &Connection,
    source_id: &SourceId,
    query: &str,
    filter: &SqlFilter,
    limit: usize,
) -> Result<Vec<VectorHit>> {
    lexical_search_in_range(
        lexical,
        source_id,
        query,
        filter,
        limit,
        LexicalBounds::Unbounded,
    )
}

fn lexical_search_in_range(
    lexical: &Connection,
    source_id: &SourceId,
    query: &str,
    filter: &SqlFilter,
    limit: usize,
    bounds: LexicalBounds,
) -> Result<Vec<VectorHit>> {
    let strict_query = fts_strict_query(query);
    if matches!(bounds, LexicalBounds::Empty) {
        return Ok(Vec::new());
    }
    if strict_query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    lexical_search_stage(lexical, source_id, &strict_query, filter, limit, bounds)
}

fn lexical_search_stage(
    lexical: &Connection,
    source_id: &SourceId,
    fts_query: &str,
    filter: &SqlFilter,
    limit: usize,
    bounds: LexicalBounds,
) -> Result<Vec<VectorHit>> {
    let where_filter = if filter.sql.is_empty() {
        String::new()
    } else {
        format!(" AND {}", filter.sql)
    };
    let range = matches!(bounds, LexicalBounds::Range(_));
    let sql = lexical_search_stage_sql(&where_filter, range);
    let mut params_vec = vec![Value::Text(fts_query.to_string())];
    if let LexicalBounds::Range(rowid_range) = bounds {
        params_vec.push(Value::Integer(rowid_range.first));
        params_vec.push(Value::Integer(rowid_range.last));
    }
    params_vec.extend(filter.params.clone());
    params_vec.push(Value::Integer(i64::try_from(limit)?));

    let mut stmt = lexical.prepare(&sql)?;
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

fn lexical_search_stage_sql(where_filter: &str, bounded: bool) -> String {
    let rowid_filter = if bounded {
        " AND f.rowid BETWEEN ? AND ?"
    } else {
        ""
    };
    format!(
        r#"
        SELECT f.rowid AS chunk_id, -bm25(chunk_fts) AS score
        FROM chunk_fts AS f
        CROSS JOIN chunk_filter AS c ON c.chunk_id = f.rowid
        CROSS JOIN document_filter AS d ON d.doc_key = c.doc_key
        WHERE chunk_fts MATCH ? {rowid_filter} {where_filter}
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
    legal: &Connection,
    lexical: &Connection,
    source_id: &SourceId,
    query: &str,
    k: usize,
    corpus_filter: &SqlFilter,
    lexical_filter: &SqlFilter,
) -> Result<Vec<Hit>> {
    collect_title_hits_in_range(
        legal,
        lexical,
        source_id,
        query,
        k,
        (corpus_filter, lexical_filter),
        None,
    )
}

fn collect_title_hits_in_range(
    legal: &Connection,
    lexical: &Connection,
    source_id: &SourceId,
    query: &str,
    k: usize,
    filters: (&SqlFilter, &SqlFilter),
    mut timings: Option<&mut SearchTimings>,
) -> Result<Vec<Hit>> {
    let (corpus_filter, lexical_filter) = filters;
    let k = k.clamp(1, 100);
    let mut hits = Vec::new();
    if let Some(direct) =
        measure_optional_phase(&mut timings, SearchPhase::PayloadHydration, || {
            query_direct_document_hit(legal, source_id, query.trim(), corpus_filter)
        })?
    {
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
    let remaining = k.saturating_sub(hits.len());
    if remaining == 0 {
        return Ok(hits);
    }
    let title_ids = measure_optional_phase(&mut timings, SearchPhase::LexicalIndex, || {
        query_title_fts(lexical, source_id, &strict_query, lexical_filter, k)
    })?;
    let title_ids = title_ids
        .into_iter()
        .filter(|native_id| {
            seen.insert(DocumentId {
                source: source_id.clone(),
                native_id: native_id.clone(),
            })
        })
        .take(remaining)
        .collect::<Vec<_>>();
    let title_hits = measure_optional_phase(&mut timings, SearchPhase::PayloadHydration, || {
        let mut hydrated = load_title_hits(legal, source_id, &title_ids)?;
        if hydrated.len() != title_ids.len() {
            bail!("lexical title sidecar returned an unknown legal.db document");
        }
        title_ids
            .into_iter()
            .map(|native_id| {
                hydrated
                    .remove(&native_id)
                    .ok_or_else(|| anyhow!("lexical title winner disappeared during hydration"))
            })
            .collect::<Result<Vec<_>>>()
    })?;
    hits.extend(title_hits);
    Ok(hits)
}

fn measure_optional_phase<T>(
    timings: &mut Option<&mut SearchTimings>,
    phase: SearchPhase,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    match timings.as_deref_mut() {
        Some(timings) => timings.measure(phase, operation),
        None => operation(),
    }
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
    lexical: &Connection,
    source_id: &SourceId,
    title_query: &str,
    filter: &SqlFilter,
    limit: usize,
) -> Result<Vec<String>> {
    let where_filter = if filter.sql.is_empty() {
        String::new()
    } else {
        format!(" AND {}", filter.sql)
    };
    let sql = title_search_stage_sql(&where_filter);
    let mut params_vec = vec![Value::Text(title_query.to_string())];
    params_vec.extend(filter.params.clone());
    params_vec.push(Value::Integer(i64::try_from(limit)?));

    let mut stmt = lexical.prepare(&sql)?;
    debug_assert_eq!(&filter.source, source_id);
    let rows = stmt
        .query_map(params_from_iter(params_vec), |row| row.get("native_id"))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn load_title_hits(
    legal: &Connection,
    source_id: &SourceId,
    native_ids: &[String],
) -> Result<HashMap<String, Hit>> {
    if native_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = vec!["?"; native_ids.len()].join(",");
    let sql = format!(
        r#"
        SELECT d.native_id, d.type, d.title, d.date, d.canonical_url,
               d.withdrawn_date, d.superseded_by, d.replaces,
               d.has_in_doc_links, d.has_related_docs, d.has_history
        FROM documents AS d
        WHERE d.source_id = ? AND d.native_id IN ({placeholders})
        "#
    );
    let mut parameters = vec![Value::Text(source_id.as_str().to_string())];
    parameters.extend(native_ids.iter().cloned().map(Value::Text));
    let mut statement = legal.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(parameters), |row| {
        let native_id = row.get::<_, String>("native_id")?;
        let title = row.get::<_, String>("title")?;
        let hit = Hit {
            canonical_url: row.get("canonical_url")?,
            document: DocumentId {
                source: source_id.clone(),
                native_id: native_id.clone(),
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
        };
        Ok((native_id, hit))
    })?;
    Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
}

fn title_search_stage_sql(where_filter: &str) -> String {
    format!(
        r#"
        SELECT d.native_id AS native_id, -bm25(title_fts) AS score
        FROM title_fts AS t
        CROSS JOIN document_filter AS d ON d.doc_key = t.rowid
        WHERE title_fts MATCH ? {where_filter}
        ORDER BY score DESC, d.native_id COLLATE BINARY ASC
        LIMIT ?
        "#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{compress_text, init_db};

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires LEGAL_MCP_BENCH_DATA_DIR and LEGAL_MCP_BENCH_EXPECTED_IDS"]
    fn benchmark_installed_production_lexical_phase() -> Result<()> {
        use std::os::fd::AsRawFd;
        use std::time::Instant;

        let data_dir = PathBuf::from(
            std::env::var("LEGAL_MCP_BENCH_DATA_DIR")
                .context("LEGAL_MCP_BENCH_DATA_DIR is required")?,
        );
        let _environment =
            crate::TestEnvironment::set(&[("LEGAL_MCP_DATA_DIR", data_dir.as_os_str())]);
        let source_id: SourceId = std::env::var("LEGAL_MCP_BENCH_SOURCE")
            .unwrap_or_else(|_| "federal-court".to_string())
            .parse()?;
        let query = std::env::var("LEGAL_MCP_BENCH_QUERY").unwrap_or_else(|_| {
            "moreton resources innovation science australia activities".to_string()
        });
        let expected_ids = std::env::var("LEGAL_MCP_BENCH_EXPECTED_IDS")?
            .split(',')
            .map(|item| item.trim().parse::<u64>().map_err(Into::into))
            .collect::<Result<Vec<_>>>()?;
        if expected_ids.is_empty() {
            bail!("LEGAL_MCP_BENCH_EXPECTED_IDS must not be empty");
        }
        let runs = std::env::var("LEGAL_MCP_BENCH_COLD_RUNS")
            .unwrap_or_else(|_| "30".to_string())
            .parse::<usize>()?;
        if runs == 0 {
            bail!("LEGAL_MCP_BENCH_COLD_RUNS must be positive");
        }
        let limit_ms = std::env::var("LEGAL_MCP_BENCH_COLD_P95_LIMIT_MS")
            .unwrap_or_else(|_| "100".to_string())
            .parse::<f64>()?;
        if !limit_ms.is_finite() || limit_ms <= 0.0 {
            bail!("LEGAL_MCP_BENCH_COLD_P95_LIMIT_MS must be finite and positive");
        }

        let active = crate::config::active_generation_key()?
            .ok_or_else(|| anyhow!("benchmark data directory has no active generation"))?;
        crate::source::verify_active_generation_startup(&active)?;
        let manifest = load_active_generation_manifest()?
            .ok_or_else(|| anyhow!("benchmark data directory has no generation manifest"))?;
        let info = manifest
            .lexical
            .get(&source_id)
            .ok_or_else(|| anyhow!("benchmark source has no lexical sidecar"))?;
        let sidecar_path = crate::config::generation_dir(&active)?.join(&info.path);
        let mut times_ms = Vec::with_capacity(runs);
        for run in 0..runs {
            let file = std::fs::File::open(&sidecar_path)?;
            // SAFETY: `file` owns a valid descriptor; offset and length are
            // non-negative, and a zero length extends through EOF.
            let status =
                unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
            if status != 0 {
                bail!("POSIX_FADV_DONTNEED failed with status {status}");
            }
            drop(file);

            let admitted = Instant::now();
            let options = SearchOptions {
                source: source_id.clone(),
                k: expected_ids.len(),
                types: None,
                date_from: None,
                date_to: None,
                doc_scope: None,
                mode: SearchMode::Keyword,
                sort_by: SortBy::Relevance,
                include_old: false,
                current_only: true,
                max_per_doc: HARD_MAX_PER_DOC,
                include_snippet: false,
                similar_to_chunk: None,
                seed_text: None,
            };
            let (result, timing) = search_with_timing_record(
                &query,
                options,
                None,
                SearchRequestTiming::new(run as u64 + 1, admitted, admitted),
            );
            let result: JsonValue = serde_json::from_str(&result?)?;
            let ids = result["hits"]
                .as_array()
                .ok_or_else(|| anyhow!("benchmark search has no hits array"))?
                .iter()
                .map(|hit| {
                    hit["chunk"]["chunk_id"]
                        .as_u64()
                        .ok_or_else(|| anyhow!("benchmark hit has no typed chunk ID"))
                })
                .collect::<Result<Vec<_>>>()?;
            if ids != expected_ids {
                bail!("production lexical benchmark returned unexpected chunk IDs: {ids:?}");
            }
            let duration_us = timing["duration_us"]["lexical_index"]
                .as_u64()
                .ok_or_else(|| anyhow!("benchmark timing has no lexical_index duration"))?;
            times_ms.push(duration_us as f64 / 1_000.0);
        }
        times_ms.sort_by(f64::total_cmp);
        let p95_index = ((0.95 * times_ms.len() as f64).ceil() as usize)
            .saturating_sub(1)
            .min(times_ms.len() - 1);
        let p95_ms = times_ms[p95_index];
        assert!(
            p95_ms < limit_ms,
            "production lexical phase cold p95 {p95_ms:.3} ms does not satisfy the {limit_ms:.3} ms limit"
        );
        eprintln!(
            "PRODUCTION_LEXICAL_BENCH source={} runs={} median_ms={:.3} p95_ms={:.3} max_ms={:.3}",
            source_id,
            runs,
            times_ms[times_ms.len() / 2],
            p95_ms,
            times_ms[times_ms.len() - 1]
        );
        Ok(())
    }

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
        Ok(())
    }

    fn lexical_source_fixture() -> Result<Connection> {
        let legal = Connection::open_in_memory()?;
        init_db(&legal)?;
        legal.execute_batch(
            "INSERT INTO sources(source_id, display_name) VALUES
                 ('ato', 'ATO'),
                 ('frl', 'Federal Register of Legislation');",
        )?;
        for chunk_id in 1..=3 {
            insert_lexical_fixture_row(&legal, "ato", chunk_id, "alpha beta gamma global decoy")?;
        }
        insert_lexical_fixture_row(&legal, "frl", 100, "alpha gamma corroborated")?;
        insert_lexical_fixture_row(&legal, "frl", 101, "alpha singleton")?;
        insert_lexical_fixture_row(&legal, "frl", 102, "gamma singleton")?;
        insert_lexical_fixture_row(&legal, "frl", 103, "alpha beta gamma strict")?;
        insert_lexical_fixture_row(&legal, "frl", 104, "sky skies strict")?;
        insert_lexical_fixture_row(&legal, "frl", 105, "sky singleton")?;
        insert_lexical_fixture_row(&legal, "frl", 106, "skies singleton")?;
        Ok(legal)
    }

    fn build_lexical_fixture(legal: &Connection, source_id: &SourceId) -> Result<Connection> {
        crate::lexical::test_sidecar_connection(legal, source_id)
    }

    fn unrestricted_filter(source_id: &SourceId) -> SqlFilter {
        build_lexical_doc_filter(
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
            vec![
                "\"develop\"".to_string(),
                "\"development\"".to_string(),
                "\"alpha\"".to_string(),
            ],
            "distinct source terms must be left for SQLite to tokenize"
        );
        assert_eq!(
            fts_terms("sky skies"),
            vec!["\"sky\"".to_string(), "\"skies\"".to_string()]
        );
        assert_eq!(
            fts_terms("skies sky"),
            vec!["\"skies\"".to_string(), "\"sky\"".to_string()]
        );
        let many = (0..100)
            .map(|n| format!("term{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(fts_terms(&many).len(), MAX_FTS_TERMS);
    }

    #[test]
    fn fts_query_is_strict_only() {
        assert_eq!(
            fts_strict_query("capital gains tax"),
            "\"capital\" \"gains\" \"tax\""
        );
        assert!(!fts_strict_query("capital gains tax").contains(" OR "));
    }

    #[test]
    fn strict_search_never_falls_back_to_any_two_terms() -> Result<()> {
        let legal = lexical_source_fixture()?;
        let source: SourceId = "frl".parse()?;
        let lexical = build_lexical_fixture(&legal, &source)?;
        let filter = unrestricted_filter(&source);

        let hits = lexical_search(&lexical, &source, "alpha beta gamma", &filter, 10)?;
        assert_eq!(
            hits.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>(),
            vec![103],
            "only the strict all-term match may survive"
        );

        let short_query_hits = lexical_search(&lexical, &source, "alpha delta", &filter, 10)?;
        assert!(
            short_query_hits.is_empty(),
            "strict search must not broaden when it has too few results"
        );
        Ok(())
    }

    #[test]
    fn sqlite_porter_strict_terms_are_order_independent() -> Result<()> {
        let legal = lexical_source_fixture()?;
        let source: SourceId = "frl".parse()?;
        let lexical = build_lexical_fixture(&legal, &source)?;
        let filter = unrestricted_filter(&source);

        for query in ["sky skies", "skies sky"] {
            let hits = lexical_search(&lexical, &source, query, &filter, 10)?;
            assert_eq!(
                hits.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>(),
                vec![104],
                "strict SQLite Porter matching changed with query order for `{query}`"
            );
        }
        Ok(())
    }

    #[test]
    fn candidate_ranking_metadata_is_sidecar_only_and_unknown_ids_fail() -> Result<()> {
        let legal = lexical_source_fixture()?;
        let source: SourceId = "frl".parse()?;
        let lexical = build_lexical_fixture(&legal, &source)?;
        drop(legal);
        let ranked = vec![VectorHit {
            chunk_id: 103,
            score: 1.0,
        }];
        let metadata = load_candidate_meta(&lexical, &source, &ranked)?;
        assert_eq!(metadata[&103].document.native_id, "frl-103");
        assert!(metadata[&103].is_intro);
        assert!(load_candidate_meta(
            &lexical,
            &source,
            &[VectorHit {
                chunk_id: 999,
                score: 1.0,
            }],
        )
        .is_err());
        Ok(())
    }

    #[test]
    fn exact_casefolded_and_wildcard_scopes_preserve_lexical_results() -> Result<()> {
        let legal = lexical_source_fixture()?;
        let source = source();
        let lexical = build_lexical_fixture(&legal, &source)?;

        let cases = [
            ("ato-1", vec![1]),
            ("ATO-*", vec![1, 2, 3]),
            ("ATO-%", vec![1, 2, 3]),
            ("missing", Vec::new()),
        ];
        for (scope, expected) in cases {
            let filter = build_lexical_doc_filter(
                "d",
                DocumentFilterSpec {
                    source_id: &source,
                    types: None,
                    date_from: None,
                    date_to: None,
                    doc_scope: Some(scope),
                    include_old: true,
                    current_only: false,
                },
            );
            let bounds = scoped_lexical_bounds(&lexical, Some(scope))?;
            let hits = lexical_search_in_range(
                &lexical,
                &source,
                "alpha beta gamma",
                &filter,
                10,
                bounds,
            )?;
            assert_eq!(
                hits.into_iter().map(|hit| hit.chunk_id).collect::<Vec<_>>(),
                expected,
                "lexical doc_scope result changed for {scope}"
            );
        }
        Ok(())
    }

    fn filtered_fixture() -> Result<(Connection, Connection, SourceId)> {
        let legal = Connection::open_in_memory()?;
        init_db(&legal)?;
        legal.execute(
            "INSERT INTO sources(source_id, display_name) VALUES ('ato', 'ATO')",
            [],
        )?;
        for (chunk_id, native_id, document_type, date, withdrawn, superseded_by) in [
            (1_i64, "PAC/OLD", "PAC", "1990-01-01", None, None),
            (2, "EV/NEW", "EV", "2025-01-01", None, None),
            (3, "TXR/OLD", "TXR", "1990-01-01", None, None),
            (4, "TXR/CURRENT", "TXR", "2025-02-01", None, None),
            (
                5,
                "TXR/WITHDRAWN",
                "TXR",
                "2025-03-01",
                Some("2026-01-01"),
                None,
            ),
            (
                6,
                "TXR/SUPERSEDED",
                "TXR",
                "2025-04-01",
                None,
                Some("TXR/CURRENT"),
            ),
        ] {
            legal.execute(
                "INSERT INTO documents(
                     source_id, native_id, type, title, date, canonical_url,
                     downloaded_at, content_hash, html, withdrawn_date, superseded_by
                 ) VALUES ('ato', ?1, ?2, ?1, ?3, ?4,
                           '2026-01-01T00:00:00Z', ?1, X'00', ?5, ?6)",
                params![
                    native_id,
                    document_type,
                    date,
                    format!("https://example.invalid/{native_id}"),
                    withdrawn,
                    superseded_by,
                ],
            )?;
            legal.execute(
                "INSERT INTO chunks(chunk_id, source_id, native_id, ord, text)
                 VALUES (?1, 'ato', ?2, 0, ?3)",
                params![chunk_id, native_id, compress_text("needle")?],
            )?;
        }
        let source = source();
        let lexical = crate::lexical::test_sidecar_connection(&legal, &source)?;
        Ok((legal, lexical, source))
    }

    #[allow(clippy::too_many_arguments)]
    fn assert_filter_exact(
        legal: &Connection,
        lexical: &Connection,
        source_id: &SourceId,
        types: Option<&[String]>,
        date_from: Option<&str>,
        date_to: Option<&str>,
        doc_scope: Option<&str>,
        include_old: bool,
        current_only: bool,
    ) -> Result<()> {
        let corpus_filter = build_doc_filter(
            "d",
            DocumentFilterSpec {
                source_id,
                types,
                date_from,
                date_to,
                doc_scope,
                include_old,
                current_only,
            },
        );
        let lexical_filter = build_lexical_doc_filter(
            "d",
            DocumentFilterSpec {
                source_id,
                types,
                date_from,
                date_to,
                doc_scope,
                include_old,
                current_only,
            },
        );
        let sql = format!(
            "SELECT c.chunk_id
             FROM chunks AS c
             JOIN documents AS d
               ON d.source_id=c.source_id AND d.native_id=c.native_id
             WHERE {}
             ORDER BY c.chunk_id",
            corpus_filter.sql
        );
        let mut statement = legal.prepare(&sql)?;
        let expected = statement
            .query_map(params_from_iter(corpus_filter.params), |row| {
                row.get::<_, i64>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let bounds = scoped_lexical_bounds(lexical, doc_scope)?;
        let actual =
            lexical_search_in_range(lexical, source_id, "needle", &lexical_filter, 100, bounds)?
                .into_iter()
                .map(|hit| hit.chunk_id)
                .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn lexical_filters_exactly_match_authoritative_document_semantics() -> Result<()> {
        let (legal, lexical, source) = filtered_fixture()?;
        assert_filter_exact(
            &legal, &lexical, &source, None, None, None, None, false, true,
        )?;
        let empty: Vec<String> = Vec::new();
        assert_filter_exact(
            &legal,
            &lexical,
            &source,
            Some(&empty),
            None,
            None,
            None,
            false,
            true,
        )?;
        let ev = vec!["EV".to_string()];
        assert_filter_exact(
            &legal,
            &lexical,
            &source,
            Some(&ev),
            None,
            None,
            None,
            false,
            true,
        )?;
        assert_filter_exact(
            &legal, &lexical, &source, None, None, None, None, true, true,
        )?;
        assert_filter_exact(
            &legal, &lexical, &source, None, None, None, None, true, false,
        )?;
        let exact = vec!["TXR".to_string()];
        assert_filter_exact(
            &legal,
            &lexical,
            &source,
            Some(&exact),
            None,
            None,
            None,
            true,
            false,
        )?;
        let glob = vec!["T*".to_string()];
        assert_filter_exact(
            &legal,
            &lexical,
            &source,
            Some(&glob),
            None,
            None,
            None,
            true,
            false,
        )?;
        let literal_percent = vec!["T%*".to_string()];
        assert_filter_exact(
            &legal,
            &lexical,
            &source,
            Some(&literal_percent),
            None,
            None,
            None,
            true,
            false,
        )?;
        assert_filter_exact(
            &legal,
            &lexical,
            &source,
            None,
            Some("2025-01-15"),
            Some("2025-12-31"),
            None,
            false,
            false,
        )?;
        for scope in ["txr/current", "txr/*", "txr/%"] {
            assert_filter_exact(
                &legal,
                &lexical,
                &source,
                None,
                None,
                None,
                Some(scope),
                false,
                true,
            )?;
        }
        Ok(())
    }

    fn filtered_chunk_ids(
        lexical: &Connection,
        source: &SourceId,
        types: Option<&[String]>,
        date_from: Option<&str>,
        include_old: bool,
        current_only: bool,
    ) -> Result<Vec<i64>> {
        let filter = build_lexical_doc_filter(
            "d",
            DocumentFilterSpec {
                source_id: source,
                types,
                date_from,
                date_to: None,
                doc_scope: None,
                include_old,
                current_only,
            },
        );
        Ok(lexical_search_in_range(
            lexical,
            source,
            "needle",
            &filter,
            100,
            LexicalBounds::Unbounded,
        )?
        .into_iter()
        .map(|hit| hit.chunk_id)
        .collect())
    }

    #[test]
    fn ato_default_old_private_advice_and_current_policies_are_exact() -> Result<()> {
        let (_legal, lexical, source) = filtered_fixture()?;
        assert_eq!(
            filtered_chunk_ids(&lexical, &source, None, None, false, true)?,
            vec![1, 4]
        );
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            filtered_chunk_ids(&lexical, &source, Some(&empty), None, false, true)?,
            vec![1, 4]
        );
        assert_eq!(
            filtered_chunk_ids(&lexical, &source, None, Some("1990-01-01"), false, false,)?,
            vec![1, 4, 5, 6]
        );
        assert_eq!(
            filtered_chunk_ids(&lexical, &source, None, None, true, false)?,
            vec![1, 3, 4, 5, 6]
        );
        let edited_private_advice = vec!["EV".to_string()];
        assert_eq!(
            filtered_chunk_ids(
                &lexical,
                &source,
                Some(&edited_private_advice),
                None,
                false,
                true,
            )?,
            vec![2]
        );
        let literal_percent = vec!["T%*".to_string()];
        assert!(
            filtered_chunk_ids(&lexical, &source, Some(&literal_percent), None, true, false,)?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn chunk_fts_query_plan_consumes_exact_document_bounds() -> Result<()> {
        let legal = lexical_source_fixture()?;
        let source: SourceId = "frl".parse()?;
        let conn = build_lexical_fixture(&legal, &source)?;
        let filter = unrestricted_filter(&source);
        let query = lexical_search_stage_sql("", true);
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {query}"))?;
        let plan = statement
            .query_map(params!["alpha", 100_i64, 103_i64, 10_i64], |row| {
                row.get::<_, String>(3)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let fts_step = plan
            .iter()
            .find(|step| step.contains("VIRTUAL TABLE"))
            .unwrap_or_else(|| panic!("missing FTS step in plan: {plan:?}"));
        assert!(
            fts_step.starts_with("SCAN f ") && fts_step.contains("><"),
            "FTS rowid bounds were not consumed by the virtual table: {plan:?}"
        );
        assert!(filter.sql.is_empty());
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
