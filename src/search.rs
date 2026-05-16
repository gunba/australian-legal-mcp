//! Hybrid BM25 + vector search, candidate dedup, RRF fusion, snippet
//! rendering, and title hits. Port of the runtime search path.

use crate::db::{canonical_url, decompress_text, get_meta, open_read, table_exists};
use crate::semantic::{dot_i8, encode_query_embedding};
use crate::source::load_installed_manifest;
use crate::{
    embedding_model_installed_matches, SearchMode, ServerState, SortBy, DEFAULT_EXCLUDED_TYPES, EMBEDDING_DIM, EMBEDDING_MODEL_ID, HARD_MAX_PER_DOC, LEGISLATION_TYPE_PREFIXES,
    MAX_K, OLD_CONTENT_CUTOFF, SNIPPET_CHARS, TITLE_HITS_K,
};
use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};
use serde::Serialize;
use serde_json::{json, Value as JsonValue};
use std::collections::{BTreeMap, HashMap, HashSet};
use url::Url;

pub(crate) fn fts_query(query: &str) -> String {
    // [MT-08] FTS query construction quotes >=2-char terms and preserves hyphenated phrases.
    let re = Regex::new(r"[A-Za-z0-9']+(?:-[A-Za-z0-9']+)*").expect("valid regex");
    let tokens: Vec<String> = re
        .find_iter(query)
        .map(|m| m.as_str())
        .filter(|t| t.len() >= 2)
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if tokens.is_empty() {
        "\"\"".to_string()
    } else {
        tokens.join(" ")
    }
}

pub(crate) fn glob_to_like(pattern: &str) -> String {
    // [MT-13] Accept both '*' and '%' as wildcards (the prefix idiom the
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

#[derive(Default)]
pub(crate) struct SqlFilter {
    pub(crate) sql: String,
    pub(crate) params: Vec<Value>,
}

pub(crate) fn build_doc_filter(
    alias: &str,
    types: Option<&[String]>,
    date_from: Option<&str>,
    date_to: Option<&str>,
    doc_scope: Option<&str>,
    include_old: bool,
    current_only: bool,
) -> SqlFilter {
    // [MT-10] Default search policy excludes EPA and old non-legislation unless overridden.
    let mut clauses = Vec::new();
    let mut params_out = Vec::new();

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
    } else if !DEFAULT_EXCLUDED_TYPES.is_empty() {
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
        clauses.push(format!("{alias}.doc_id LIKE ? ESCAPE '\\'"));
        params_out.push(Value::Text(glob_to_like(doc_scope)));
    }
    if !include_old && date_from.is_none() {
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
        sql: clauses.join(" AND "),
        params: params_out,
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Hit {
    // [MT-04] Search-family hits stay slim; bodies materialize through follow-up tools.
    pub(crate) doc_id: String,
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
    pub(crate) chunk_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_call: Option<String>,
    /// W2.2 currency markers — only serialised when set so JSON output for
    /// in-force docs stays clean.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) withdrawn_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) superseded_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) replaces: Option<String>,
    /// Navigation hint flags — only serialised when set (so a doc with no
    /// matching anchors keeps the slim hit clean). `Some(true)` tells the
    /// agent to call `get_doc_anchors(doc_id)` to navigate.
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

pub(crate) struct SearchOptions<'a> {
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
    pub(crate) similar_to_chunk_id: Option<i64>,
    /// When set, this arbitrary text is runtime-embedded and used as the
    /// query vector — the same mechanism as `similar_to_chunk_id` but for
    /// text that isn't a corpus chunk (e.g. a chunk returned by
    /// `fetch_external_doc`). Forces vector-only mode and skips title hits,
    /// like `similar_to_chunk_id`. `similar_to_chunk_id` wins if both are set.
    pub(crate) seed_text: Option<&'a str>,
}

/// Metadata required to rank and dedup candidate chunks across documents.
#[derive(Debug, Clone)]
pub(crate) struct CandidateMeta {
    pub(crate) doc_id: String,
    /// True when this chunk's plaintext is short (< 100 chars) and the
    /// chunk sits at the start of the document — typically a stub
    /// preamble that crowds out more useful chunks. We approximate "intro"
    /// as ord == 0 with short text, which correctly demotes the leading
    /// stub chunks.
    pub(crate) is_intro: bool,
}

/// Group candidate `(chunk_id, score)` entries by `doc_id`, demote
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
    let mut buckets: BTreeMap<usize, (String, Vec<(VectorHit, bool)>)> = BTreeMap::new();
    let mut order: HashMap<String, usize> = HashMap::new();
    let mut next_idx = 0usize;
    for hit in ranked {
        let Some(m) = meta.get(&hit.chunk_id) else {
            continue;
        };
        let idx = match order.get(&m.doc_id) {
            Some(i) => *i,
            None => {
                let i = next_idx;
                order.insert(m.doc_id.clone(), i);
                buckets.insert(i, (m.doc_id.clone(), Vec::new()));
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
    let mut docs: Vec<(String, f64, Vec<VectorHit>)> = Vec::new();
    for (_idx, (doc_id, mut items)) in buckets {
        items.sort_by(|a, b| {
            a.1.cmp(&b.1).then_with(|| {
                b.0.score
                    .partial_cmp(&a.0.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        let doc_score = items
            .iter()
            .take(3)
            .map(|(h, _)| h.score)
            .fold(f64::NEG_INFINITY, f64::max);
        let chunks: Vec<VectorHit> = items.into_iter().map(|(h, _)| h).collect();
        docs.push((doc_id, doc_score, chunks));
    }
    docs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Single pass: take up to `cap` chunks from each doc in score order
    // until we hit `k`. We do not back-fill beyond the cap — the user
    // wants per-doc diversity to be a hard constraint, not a soft one.
    // Callers that need more chunks from the same doc should follow up
    // with `get_chunks`.
    let mut out: Vec<VectorHit> = Vec::with_capacity(k);
    for (_doc_id, _score, chunks) in &docs {
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
    let conn = open_read()?;
    let k = opts.k.clamp(1, MAX_K);
    let max_per_doc = opts.max_per_doc.clamp(1, HARD_MAX_PER_DOC);
    let filter = build_doc_filter(
        "d",
        opts.types,
        opts.date_from,
        opts.date_to,
        opts.doc_scope,
        opts.include_old,
        opts.current_only,
    );
    // [MT-02] k is clamped, first-stage recall is widened, then candidates dedupe per document.
    let internal_limit = std::cmp::max(k * 5, 50);
    // [MT-16] `similar_to_chunk_id` short-circuits semantic encode: load the seed
    // chunk's stored embedding and use it as the query vector. Force
    // vector-only mode (no BM25 stage — no real query text to rank against).
    let similar_seed: Option<(i64, [i8; EMBEDDING_DIM])> = match opts.similar_to_chunk_id {
        Some(seed_id) => {
            ensure_vector_search_ready(&conn)?;
            Some((seed_id, load_chunk_embedding(&conn, seed_id)?))
        }
        None => None,
    };
    // `seed_text` runtime-embeds arbitrary text as the query vector — the
    // same seed-driven path as `similar_to_chunk_id`, but for text that
    // isn't a corpus chunk (e.g. a chunk from `fetch_external_doc`).
    // `similar_to_chunk_id` wins if both are set.
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
    let lexical_hits = if matches!(effective_mode, SearchMode::Hybrid | SearchMode::Keyword) {
        lexical_search(&conn, query, &filter, internal_limit)?
    } else {
        Vec::new()
    };
    let ranked_hits = match effective_mode {
        SearchMode::Hybrid | SearchMode::Vector => {
            ensure_vector_search_ready(&conn)?;
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
            let vector_hits = vector_search(&conn, &query_embedding, &filter, internal_limit)?;
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

    // Batch-load (chunk_id -> doc_id, is_intro) for all candidates so the
    // dedup pass doesn't have to round-trip per chunk.
    let candidate_meta = load_candidate_meta(&conn, &ranked_hits)?;
    let deduped = dedup_per_doc(ranked_hits, &candidate_meta, frontier, max_per_doc);

    let mut records = Vec::new();
    for ranked_hit in deduped.into_iter() {
        if let Some(hit) = load_hit(&conn, ranked_hit.chunk_id, query, opts.include_snippet)? {
            // First-stage scores drive `deduped` ordering; we just iterate in
            // that order. Internal scores never reach the agent — results are
            // presented sorted by relevance.
            records.push(hit);
        }
    }
    if matches!(opts.sort_by, SortBy::Recency) {
        // [MT-06] Recency sort materializes a widened frontier, then sorts by date descending.
        records.sort_by(|a, b| b.date.cmp(&a.date));
        records.truncate(k);
    }
    // [MT-03] JSON metadata preserves query/filter state in next_call when k can grow.
    let next_call = if candidate_count > records.len() && k < MAX_K {
        Some(search_next_call(query, std::cmp::min(k * 2, MAX_K), &opts))
    } else {
        None
    };

    let mut meta = serde_json::Map::new();
    if candidate_count > records.len() {
        meta.insert("truncated".to_string(), json!(true));
        if let Some(nc) = next_call {
            meta.insert("next_call".to_string(), json!(nc));
        }
    }

    // Title-level hits — a parallel algorithm over the separate `title_fts`
    // table, surfaced as a sidebar alongside the chunk `hits`. Reuses the
    // same document filter so chunk and title queries stay consistently
    // scoped. Skipped for a seed search (`similar_to_chunk_id` / `seed_text`)
    // — there's no real query text to BM25 against; `query` is ignored.
    let title_hits: Vec<Hit> = if is_seed_search {
        Vec::new()
    } else {
        collect_title_hits(&conn, query, TITLE_HITS_K, &filter)?
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
    // Two-step query: first read (chunk_id, doc_id, ord) for every
    // candidate cheaply; then decompress the text BLOB only for the
    // small minority sitting at ord == 0 so we can measure the *plain*
    // text length precisely. Heading-path is gone; "intro" now means
    // "leading stub chunk" (ord 0 with short text) which still
    // correctly demotes the typical preamble pattern.
    let sql =
        format!("SELECT chunk_id, doc_id, ord FROM chunks WHERE chunk_id IN ({placeholders})");
    let params_vec: Vec<Value> = ids.into_iter().map(Value::Integer).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params_vec), |row| {
        let chunk_id: i64 = row.get("chunk_id")?;
        let doc_id: String = row.get("doc_id")?;
        let ord: i64 = row.get("ord")?;
        Ok((chunk_id, doc_id, ord))
    })?;
    let mut leading_chunk_ids: Vec<i64> = Vec::new();
    let mut staged: Vec<(i64, String, i64)> = Vec::new();
    for row in rows {
        let (chunk_id, doc_id, ord) = row?;
        if ord == 0 {
            leading_chunk_ids.push(chunk_id);
        }
        staged.push((chunk_id, doc_id, ord));
    }

    let mut intro_set: HashSet<i64> = HashSet::new();
    if !leading_chunk_ids.is_empty() {
        let placeholders2 = vec!["?"; leading_chunk_ids.len()].join(",");
        let sql2 = format!("SELECT chunk_id, text FROM chunks WHERE chunk_id IN ({placeholders2})");
        let params_vec2: Vec<Value> = leading_chunk_ids.into_iter().map(Value::Integer).collect();
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
    for (chunk_id, doc_id, _ord) in staged {
        let is_intro = intro_set.contains(&chunk_id);
        out.insert(chunk_id, CandidateMeta { doc_id, is_intro });
    }
    Ok(out)
}

pub(crate) fn search_next_call(query: &str, k: usize, opts: &SearchOptions<'_>) -> String {
    let mut args = vec![
        format!("query={}", mcp_string(query)),
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
    // Seed-driven searches: preserve the seed so paging re-runs the same
    // semantic query rather than falling back to a plain `query` search.
    if let Some(seed_id) = opts.similar_to_chunk_id {
        args.push(format!("similar_to_chunk_id={seed_id}"));
    } else if let Some(seed) = opts.seed_text {
        args.push(format!("seed_text={}", mcp_string(seed)));
    }
    format!("search({})", args.join(", "))
}

pub(crate) fn mcp_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// Load a chunk's stored int8 embedding from `chunk_embeddings`. Used by
/// `similar_to_chunk_id` to bypass query encoding and run vector search
/// directly against the seed chunk's vector.
pub(crate) fn load_chunk_embedding(conn: &Connection, chunk_id: i64) -> Result<[i8; EMBEDDING_DIM]> {
    let blob: Vec<u8> = conn
        .query_row(
            "SELECT embedding FROM chunk_embeddings WHERE chunk_id = ?",
            [chunk_id],
            |row| row.get(0),
        )
        .with_context(|| format!("no stored embedding for chunk_id={chunk_id}"))?;
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

pub(crate) fn ensure_vector_search_ready(conn: &Connection) -> Result<()> {
    // [MT-09] Hybrid/vector modes require the current semantic corpus model.
    let model_id = get_meta(conn, "embedding_model_id")?.ok_or_else(|| {
        anyhow!("semantic search unavailable: missing embedding_model_id metadata")
    })?;
    if model_id != EMBEDDING_MODEL_ID {
        bail!(
            "semantic search unavailable: installed corpus uses unsupported embedding model `{model_id}`; install a {EMBEDDING_MODEL_ID} corpus"
        );
    }
    let installed = load_installed_manifest()?.ok_or_else(|| {
        anyhow!("semantic search unavailable: installed manifest missing; run `ato-mcp update`")
    })?;
    if installed.model.id != model_id {
        bail!(
            "semantic search unavailable: installed manifest model `{}` does not match corpus metadata `{model_id}`",
            installed.model.id
        );
    }
    if !embedding_model_installed_matches(&installed.model)? {
        bail!(
            "semantic search unavailable: installed semantic model files do not match installed_manifest.json; run `ato-mcp update`"
        );
    }
    if !table_exists(conn, "chunk_embeddings")? {
        bail!("semantic search unavailable: installed corpus has no chunk_embeddings table; run `ato-mcp update`");
    }
    let embeddings: i64 = conn.query_row("SELECT COUNT(*) FROM chunk_embeddings", [], |row| {
        row.get(0)
    })?;
    if embeddings == 0 {
        bail!("semantic search unavailable: installed corpus has no chunk embeddings");
    }
    Ok(())
}

pub(crate) fn vector_search(
    conn: &Connection,
    query_embedding: &[i8; EMBEDDING_DIM],
    filter: &SqlFilter,
    limit: usize,
) -> Result<Vec<VectorHit>> {
    let where_filter = if filter.sql.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", filter.sql)
    };
    let sql = format!(
        r#"
        SELECT e.chunk_id, e.embedding
        FROM chunk_embeddings e
        JOIN chunks c ON c.chunk_id = e.chunk_id
        JOIN documents d ON d.doc_id = c.doc_id
        {where_filter}
        "#
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(filter.params.clone()), |row| {
        Ok((
            row.get::<_, i64>("chunk_id")?,
            row.get::<_, Vec<u8>>("embedding")?,
        ))
    })?;
    let mut hits = Vec::new();
    for row in rows {
        let (chunk_id, embedding) = row?;
        hits.push(VectorHit {
            chunk_id,
            score: dot_i8(query_embedding, &embedding)?,
        });
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    Ok(hits)
}

pub(crate) fn lexical_search(
    conn: &Connection,
    query: &str,
    filter: &SqlFilter,
    limit: usize,
) -> Result<Vec<VectorHit>> {
    let where_filter = if filter.sql.is_empty() {
        String::new()
    } else {
        format!(" AND {}", filter.sql)
    };
    let sql = format!(
        r#"
        SELECT f.rowid AS chunk_id, bm25(chunks_fts) AS score
        FROM chunks_fts f
        JOIN chunks c ON c.chunk_id = f.rowid
        JOIN documents d ON d.doc_id = c.doc_id
        WHERE chunks_fts MATCH ? {where_filter}
        ORDER BY score ASC
        LIMIT ?
        "#
    );
    let mut params_vec = vec![Value::Text(fts_query(query))];
    params_vec.extend(filter.params.clone());
    params_vec.push(Value::Integer(limit as i64));

    let mut stmt = conn.prepare(&sql)?;
    let rows = match stmt.query_map(params_from_iter(params_vec), |row| {
        Ok(VectorHit {
            chunk_id: row.get::<_, i64>("chunk_id")?,
            score: row.get::<_, f64>("score")?,
        })
    }) {
        Ok(rows) => rows.collect::<rusqlite::Result<Vec<_>>>()?,
        Err(rusqlite::Error::SqliteFailure(_, _)) => Vec::new(),
        Err(err) => return Err(err.into()),
    };
    Ok(rows)
}

pub(crate) fn rrf_fuse(vector_hits: &[VectorHit], lexical_hits: &[VectorHit]) -> Vec<VectorHit> {
    // [MT-05] Hybrid ranking fuses vector and lexical ranks via RRF with K=60.
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
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

// [MT-01] HTTP transport keeps one ServerState shared across worker threads.
// The semantic runtime is loaded lazily and reused across tool calls. Search-time
// inference holds the lock for one query embedding; read-only tools
// (get_chunks, get_definition, get_doc_anchors, get_asset, stats) run fully
// concurrently.

pub(crate) fn load_hit(
    conn: &Connection,
    chunk_id: i64,
    query: &str,
    include_snippet: bool,
) -> Result<Option<Hit>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT c.chunk_id, c.doc_id, c.anchor, c.text,
               d.type, d.title, d.date,
               d.withdrawn_date, d.superseded_by, d.replaces,
               d.has_in_doc_links, d.has_related_docs, d.has_history
        FROM chunks c
        JOIN documents d ON d.doc_id = c.doc_id
        WHERE c.chunk_id = ?
        "#,
    )?;
    let mut rows = stmt.query([chunk_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let doc_id: String = row.get("doc_id")?;
    let text = decompress_text(row.get("text")?)?;
    let chunk_id: i64 = row.get("chunk_id")?;
    Ok(Some(Hit {
        doc_id: doc_id.clone(),
        title: row.get("title")?,
        doc_type: row.get("type")?,
        date: row.get("date")?,
        anchor: row.get("anchor")?,
        snippet: if include_snippet {
            Some(highlight_snippet(&text, query, SNIPPET_CHARS))
        } else {
            None
        },
        canonical_url: canonical_url(&doc_id),
        chunk_id: Some(chunk_id),
        next_call: Some(format!("get_chunks(chunk_ids=[{chunk_id}])")),
        withdrawn_date: row.get("withdrawn_date")?,
        superseded_by: row.get("superseded_by")?,
        replaces: row.get("replaces")?,
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
    }))
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

pub(crate) fn ato_doc_id_from_link(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_matches('<').trim_matches('>');
    let parsed = Url::parse(trimmed)
        .or_else(|_| Url::parse("https://www.ato.gov.au").and_then(|base| base.join(trimmed)))
        .ok()?;
    if parsed.domain() != Some("www.ato.gov.au") && parsed.domain() != Some("ato.gov.au") {
        return None;
    }
    if parsed.path() != "/law/view/document" {
        return None;
    }
    for (key, value) in parsed.query_pairs() {
        if key.eq_ignore_ascii_case("docid") || key.eq_ignore_ascii_case("locid") {
            let doc_id = value.trim().trim_matches('"').to_string();
            if !doc_id.is_empty() {
                return Some(doc_id);
            }
        }
    }
    None
}

pub(crate) fn direct_doc_id_from_query(query: &str) -> Option<String> {
    if let Some(doc_id) = ato_doc_id_from_link(query) {
        return Some(doc_id);
    }
    let candidate = query.trim().trim_matches('<').trim_matches('>');
    if candidate.is_empty()
        || candidate.contains(char::is_whitespace)
        || !candidate.contains('/')
        || !candidate
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '_' | '.' | '(' | ')' | '-'))
    {
        return None;
    }
    Some(candidate.to_string())
}

pub(crate) fn direct_title_hits(
    conn: &Connection,
    query: &str,
    k: usize,
    filter: &SqlFilter,
) -> Result<Vec<Hit>> {
    let mut doc_ids = Vec::new();
    if let Some(doc_id) = direct_doc_id_from_query(query) {
        doc_ids.push(doc_id);
    }

    let mut hits = Vec::new();
    let mut seen = HashSet::new();
    for doc_id in doc_ids {
        if !seen.insert(doc_id.clone()) {
            continue;
        }
        if let Some(hit) = load_title_hit(conn, &doc_id, filter)? {
            hits.push(hit);
        }
        if hits.len() >= k {
            break;
        }
    }
    Ok(hits)
}

pub(crate) fn load_title_hit(conn: &Connection, doc_id: &str, filter: &SqlFilter) -> Result<Option<Hit>> {
    let where_filter = if filter.sql.is_empty() {
        String::new()
    } else {
        format!(" AND {}", filter.sql)
    };
    let sql = format!(
        r#"
        SELECT d.doc_id, d.type, d.title, d.date,
               d.withdrawn_date, d.superseded_by, d.replaces,
               d.has_in_doc_links, d.has_related_docs, d.has_history
        FROM documents d
        WHERE d.doc_id = ? {where_filter}
        "#
    );
    let mut params_vec = vec![Value::Text(doc_id.to_string())];
    params_vec.extend(filter.params.clone());
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params_from_iter(params_vec))?;
    if let Some(row) = rows.next()? {
        let doc_id: String = row.get("doc_id")?;
        let title: String = row.get("title")?;
        Ok(Some(Hit {
            canonical_url: canonical_url(&doc_id),
            doc_id: doc_id.clone(),
            title: title.clone(),
            doc_type: row.get("type")?,
            date: row.get("date")?,
            anchor: None,
            snippet: Some(title),
            chunk_id: None,
            next_call: None,
            withdrawn_date: row.get("withdrawn_date")?,
            superseded_by: row.get("superseded_by")?,
            replaces: row.get("replaces")?,
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
        }))
    } else {
        Ok(None)
    }
}

/// Title-level hits for a query: exact doc_id / ATO-link lookups first,
/// then BM25 over the separate `title_fts` table. A parallel algorithm to
/// chunk search — `search` calls this to populate its `title_hits`
/// sidebar. The caller supplies the connection and the already-built
/// document filter so chunk and title queries stay consistently scoped.
pub(crate) fn collect_title_hits(
    conn: &Connection,
    query: &str,
    k: usize,
    filter: &SqlFilter,
) -> Result<Vec<Hit>> {
    // [MT-14] Title hits rank title_fts independently and reuse the chunk
    // query's document filter.
    let k = k.clamp(1, 100);
    let direct_hits = direct_title_hits(conn, query, k, filter)?;
    let where_filter = if filter.sql.is_empty() {
        String::new()
    } else {
        format!(" AND {}", filter.sql)
    };
    let sql = format!(
        r#"
        SELECT t.doc_id AS doc_id, bm25(title_fts) AS score,
               d.type, d.title, d.date,
               d.withdrawn_date, d.superseded_by, d.replaces,
               d.has_in_doc_links, d.has_related_docs, d.has_history
        FROM title_fts t
        JOIN documents d ON d.doc_id = t.doc_id
        WHERE title_fts MATCH ? {where_filter}
        ORDER BY score ASC
        LIMIT ?
        "#
    );
    let mut params_vec = vec![Value::Text(fts_query(query))];
    params_vec.extend(filter.params.clone());
    params_vec.push(Value::Integer(k as i64 + 1));

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = match stmt.query_map(params_from_iter(params_vec), |row| {
        let doc_id: String = row.get("doc_id")?;
        let title: String = row.get("title")?;
        Ok(Hit {
            canonical_url: canonical_url(&doc_id),
            doc_id: doc_id.clone(),
            title: title.clone(),
            doc_type: row.get("type")?,
            date: row.get("date")?,
            anchor: None,
            snippet: Some(title),
            chunk_id: None,
            next_call: None,
            withdrawn_date: row.get("withdrawn_date")?,
            superseded_by: row.get("superseded_by")?,
            replaces: row.get("replaces")?,
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
    }) {
        Ok(rows) => rows.collect::<rusqlite::Result<Vec<_>>>()?,
        Err(rusqlite::Error::SqliteFailure(_, _)) => Vec::new(),
        Err(err) => return Err(err.into()),
    };
    let direct_doc_ids: HashSet<String> =
        direct_hits.iter().map(|hit| hit.doc_id.clone()).collect();
    rows.retain(|hit| !direct_doc_ids.contains(&hit.doc_id));
    rows.splice(0..0, direct_hits);
    rows.truncate(k);
    Ok(rows)
}
