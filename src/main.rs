use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use fs2::FileExt;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::TensorRef;
use regex::Regex;
use reqwest::blocking::Client;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
// Pull `simsimd` into the dependency graph so its build script's
// `cargo:rustc-link-lib=static=simsimd` directive reaches the linker;
// we then call `simsimd_dot_i8` directly via the extern block below.
#[allow(unused_imports)]
use simsimd::SpatialSimilarity as _;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};
use url::Url;
use zip::ZipArchive;

const APP_NAME: &str = "ato-mcp";
const DEFAULT_RELEASES_URL: &str = "https://github.com/gunba/ato-mcp/releases/latest/download";
const DEFAULT_K: usize = 8;
const MAX_K: usize = 50;
const SNIPPET_CHARS: usize = 280;
const EMBEDDING_DIM: usize = 256;
const MAX_TOKENS: usize = 1024;
const QUERY_PREFIX: &str = "task: search result | query: ";
const EMBEDDINGGEMMA_HF_FINGERPRINT: &str =
    "5d4d31914cdb65cd84d3248390946461efdd4ec4f99afd13d23218cd4060d706";
const OLD_CONTENT_CUTOFF: &str = "2000-01-01";
const DEFAULT_EXCLUDED_TYPES: &[&str] = &["Edited_private_advice"];
const LEGISLATION_TYPE: &str = "Legislation_and_supporting_material";
const OEWN_2024_URL: &str = "https://en-word.net/static/english-wordnet-2024.zip";
const OEWN_2024_SOURCE: &str = "Open English WordNet 2024 (CC-BY 4.0)";
const ORDINARY_DICTIONARY_PATH_ENV: &str = "ATO_MCP_DICTIONARY_PATH";
/// On-disk schema version this binary supports. Bump when introducing
/// schema changes; older binaries reject newer corpora via [`open_read`]
/// / [`open_write`] / [`apply_update_locked`] guards.
const SUPPORTED_SCHEMA_VERSION: u32 = 8;
/// Highest manifest format version (`Manifest.schema_version`) this binary
/// will ingest. v2 (released alongside ato-mcp 0.5.0) signals that
/// `min_client_version` is now meaningfully populated by the builder
/// (older "v1" manifests left it at "0.1.0", making the version gate
/// dormant). v3 (released alongside ato-mcp 0.6.0) adds the optional
/// `reranker` field for cross-encoder rerank stage. The
/// `min_client_version > CARGO_PKG_VERSION` check inside
/// `enforce_manifest_compatibility` is the actual cross-version gate;
/// this constant is a belt-and-suspenders upper bound for future format
/// bumps that older binaries can't decode. v4 carries cleaned HTML and image
/// asset records in release packs.
const MAX_SUPPORTED_MANIFEST_VERSION: u32 = 4;
/// Maximum number of RRF top-N candidates we feed into the cross-encoder
/// reranker per query. Reranking is O(N) ONNX inference; the quantized
/// ModernBERT cross-encoder is still CPU-expensive, so keep the rerank
/// head tight and let the RRF tail preserve recall for paging.
const RERANK_CANDIDATE_LIMIT: usize = 24;
/// Cross-encoder query side max-token budget. We reserve the remaining
/// tokens for the document side so a long snippet does not evict the query.
const RERANK_QUERY_MAX_TOKENS: usize = 64;
/// Cross-encoder total sequence max length (`[CLS] q [SEP] d [SEP]`).
const RERANK_PAIR_MAX_TOKENS: usize = 512;
const DEFAULT_MAX_PER_DOC: usize = 2;
const HARD_MAX_PER_DOC: usize = 3;
const RERANKER_HF_MODEL_PATH: &str = "onnx/model_quantized.onnx";
// Avoid expensive online transformer graph rewrites on every fresh CLI/MCP
// process. The ONNX models are shipped pre-quantized; Level1 keeps cheap
// semantics-preserving cleanup without the high startup cost of Level2/All.
const ONLINE_MODEL_OPTIMIZATION_LEVEL: GraphOptimizationLevel = GraphOptimizationLevel::Level1;

// SimSIMD's Rust 5.x trait wires `i8::dot` through `simsimd_cos_i8`
// (cosine distance), which is not what the ranking pipeline expects.
// We need the raw `sum(q[i] * d[i])` so that `score = dot/(127*127)`
// continues to approximate cosine similarity for L2-normalised vectors.
// `simsimd_dot_i8` is exported by the C library with runtime SIMD dispatch
// (AVX2/AVX-512 on x86_64, NEON on aarch64) and is linked transparently
// because we depend on the `simsimd` crate elsewhere.
extern "C" {
    fn simsimd_dot_i8(a: *const i8, b: *const i8, n: usize, out: *mut f64);
}

#[derive(Parser)]
#[command(name = "ato-mcp", version, about = "Standalone ATO MCP server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP stdio server.
    Serve {
        /// Check for corpus updates before starting the MCP stdio loop.
        #[arg(long)]
        check_update: bool,
    },
    /// First-run install of the corpus into the local data directory.
    Init {
        #[arg(long)]
        manifest_url: Option<String>,
    },
    /// Apply a manifest delta to the local corpus.
    Update {
        #[arg(long)]
        manifest_url: Option<String>,
    },
    /// Verify the local corpus, optionally restoring the previous DB snapshot.
    Doctor {
        #[arg(long)]
        rollback: bool,
    },
    /// Print index version and counts.
    Stats {},
    /// Run a search from the CLI.
    Search {
        query: String,
        #[arg(short, long, default_value_t = DEFAULT_K)]
        k: usize,
        #[arg(long, value_delimiter = ',')]
        types: Vec<String>,
        #[arg(long)]
        date_from: Option<String>,
        #[arg(long)]
        date_to: Option<String>,
        #[arg(long)]
        doc_scope: Option<String>,
        #[arg(long, default_value = "hybrid")]
        mode: SearchMode,
        #[arg(long, default_value = "relevance")]
        sort_by: SortBy,
        #[arg(long)]
        include_old: bool,
        /// Include withdrawn / superseded rulings (default excludes them).
        #[arg(long)]
        include_withdrawn: bool,
    },
    /// Search document titles, plus exact doc_id / ATO document links.
    SearchTitles {
        query: String,
        #[arg(short, long, default_value_t = 20)]
        k: usize,
        #[arg(long, value_delimiter = ',')]
        types: Vec<String>,
        #[arg(long)]
        include_old: bool,
        /// Include withdrawn / superseded rulings (default excludes them).
        #[arg(long)]
        include_withdrawn: bool,
    },
    /// Fetch a document or a slice of it.
    GetDocument {
        doc_id: String,
        /// When set, return the raw cleaned HTML instead of plaintext.
        #[arg(long, default_value_t = false)]
        as_html: bool,
        #[arg(long)]
        max_chars: Option<usize>,
        #[arg(long)]
        from_char: Option<usize>,
    },
    /// Fetch compact statutory definitions for a term.
    GetDefinition {
        term: String,
        #[arg(long)]
        context_doc_id: Option<String>,
        #[arg(long, default_value_t = 5)]
        max_defs: usize,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum SortBy {
    Relevance,
    Recency,
}

impl SortBy {
    fn as_str(self) -> &'static str {
        match self {
            SortBy::Relevance => "relevance",
            SortBy::Recency => "recency",
        }
    }
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum SearchMode {
    Hybrid,
    Vector,
    Keyword,
}

impl SearchMode {
    fn as_str(self) -> &'static str {
        match self {
            SearchMode::Hybrid => "hybrid",
            SearchMode::Vector => "vector",
            SearchMode::Keyword => "keyword",
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { check_update } => {
            if serve_should_check_update(check_update) {
                update_before_serve()?;
            } else {
                ensure_installed_db()?;
            }
            serve()
        }
        Command::Init { manifest_url } => {
            let url = manifest_url.unwrap_or_else(default_manifest_url);
            let stats = apply_update(&url)?;
            println!(
                "init complete: +{} ~{} -{} ({:.1} MB downloaded)",
                stats.added,
                stats.changed,
                stats.removed,
                stats.bytes_downloaded as f64 / 1_000_000.0
            );
            Ok(())
        }
        Command::Update { manifest_url } => {
            let url = manifest_url.unwrap_or_else(default_manifest_url);
            let stats = apply_update(&url)?;
            println!(
                "update complete: +{} ~{} -{} ({:.2} MB downloaded)",
                stats.added,
                stats.changed,
                stats.removed,
                stats.bytes_downloaded as f64 / 1_000_000.0
            );
            Ok(())
        }
        Command::Doctor { rollback } => doctor(rollback),
        Command::Stats {} => {
            println!("{}", stats()?);
            Ok(())
        }
        Command::Search {
            query,
            k,
            types,
            date_from,
            date_to,
            doc_scope,
            mode,
            sort_by,
            include_old,
            include_withdrawn,
        } => {
            let types = empty_vec_as_none(types);
            // Construct a transient ServerState so the CLI's `search` call
            // reuses the same lazy semantic/reranker runtimes the MCP server
            // does for modes that need them.
            let (out, _state) = search_cli(
                &query,
                SearchOptions {
                    k,
                    types: types.as_deref(),
                    date_from: date_from.as_deref(),
                    date_to: date_to.as_deref(),
                    doc_scope: doc_scope.as_deref(),
                    mode,
                    sort_by,
                    include_old,
                    current_only: !include_withdrawn,
                    max_per_doc: DEFAULT_MAX_PER_DOC,
                    include_snippet: true,
                    similar_to_chunk_id: None,
                },
            )?;
            println!("{}", out);
            Ok(())
        }
        Command::SearchTitles {
            query,
            k,
            types,
            include_old,
            include_withdrawn,
        } => {
            let types = empty_vec_as_none(types);
            println!(
                "{}",
                search_titles(
                    &query,
                    k,
                    types.as_deref(),
                    include_old,
                    !include_withdrawn,
                )?
            );
            Ok(())
        }
        Command::GetDocument {
            doc_id,
            as_html,
            max_chars,
            from_char,
        } => {
            println!(
                "{}",
                get_document(
                    &doc_id,
                    GetDocumentOptions {
                        as_html,
                        max_chars,
                        from_char,
                    },
                )?
            );
            Ok(())
        }
        Command::GetDefinition {
            term,
            context_doc_id,
            max_defs,
        } => {
            println!(
                "{}",
                get_definition(
                    &term,
                    GetDefinitionOptions {
                        context_doc_id: context_doc_id.as_deref(),
                        max_defs,
                    },
                )?
            );
            Ok(())
        }
    }
}

fn empty_vec_as_none(values: Vec<String>) -> Option<Vec<String>> {
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn default_manifest_url() -> String {
    format!("{}/manifest.json", releases_url().trim_end_matches('/'))
}

fn releases_url() -> String {
    std::env::var("ATO_MCP_RELEASES_URL").unwrap_or_else(|_| DEFAULT_RELEASES_URL.to_string())
}

fn data_dir() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("ATO_MCP_DATA_DIR") {
        let path = PathBuf::from(path);
        fs::create_dir_all(&path)?;
        return Ok(path);
    }
    let mut path =
        dirs::data_dir().ok_or_else(|| anyhow!("could not resolve user data directory"))?;
    path.push(APP_NAME);
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn live_dir() -> Result<PathBuf> {
    let path = data_dir()?.join("live");
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn live_assets_root() -> Result<PathBuf> {
    let path = live_dir()?;
    fs::create_dir_all(path.join("assets"))?;
    Ok(path)
}

fn staging_dir() -> Result<PathBuf> {
    let path = data_dir()?.join("staging");
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn backups_dir() -> Result<PathBuf> {
    let path = data_dir()?.join("backups");
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn db_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("ato.db"))
}

fn installed_manifest_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("installed_manifest.json"))
}

fn lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("LOCK"))
}

fn model_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("model_quantized.onnx"))
}

fn tokenizer_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("tokenizer.json"))
}

fn reranker_model_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("reranker.onnx"))
}

fn reranker_tokenizer_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("reranker_tokenizer.json"))
}

fn lock_file() -> Result<File> {
    // [UM-02] fs2::FileExt gives the update/install path a cross-platform advisory lock.
    let path = lock_path()?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock_exclusive()?;
    Ok(file)
}

fn open_read() -> Result<Connection> {
    let path = db_path()?;
    if !path.exists() {
        bail!(
            "no live DB found at {}; run `ato-mcp init` first",
            path.display()
        );
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening local corpus database")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    enforce_db_schema_version(&conn)?;
    Ok(conn)
}

fn open_write_at(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path).context("opening local corpus database for writing")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // Skip the schema check on a brand-new DB (no `meta` table yet);
    // `init_db` will populate it. For an existing DB, validate up front
    // so callers don't operate against an incompatible schema.
    if table_exists(&conn, "meta")? {
        enforce_db_schema_version(&conn)?;
    }
    Ok(conn)
}

/// Reject DBs whose stored `meta.schema_version` doesn't match what this
/// binary supports. A missing entry is treated as a corrupt/incomplete
/// install — refuse with a recovery hint rather than silently operating
/// on a DB that may be missing required tables/indexes.
fn enforce_db_schema_version(conn: &Connection) -> Result<()> {
    // [CC-04] DB compatibility is fail-fast; the Rust runtime does not run Python-era migrations.
    if !table_exists(conn, "meta")? {
        bail!(
            "no schema_version metadata; corpus may be corrupt or incomplete; run `ato-mcp init`"
        );
    }
    match get_meta(conn, "schema_version")? {
        None => bail!(
            "no schema_version metadata; corpus may be corrupt or incomplete; run `ato-mcp init`"
        ),
        Some(value) => {
            let parsed: u32 = value
                .parse()
                .with_context(|| format!("schema_version `{value}` is not a valid integer"))?;
            if parsed != SUPPORTED_SCHEMA_VERSION {
                bail!(
                    "DB schema version {parsed} not supported by this binary (expects {}); reinstall the corpus or upgrade ato-mcp",
                    SUPPORTED_SCHEMA_VERSION
                );
            }
        }
    }
    Ok(())
}

fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS documents (
            doc_id           TEXT PRIMARY KEY,
            type             TEXT NOT NULL,
            title            TEXT NOT NULL,
            date             TEXT,
            downloaded_at    TEXT NOT NULL,
            content_hash     TEXT NOT NULL,
            pack_sha8        TEXT NOT NULL,
            html             BLOB NOT NULL,
            withdrawn_date   TEXT,
            superseded_by    TEXT,
            replaces         TEXT,
            has_in_doc_links INTEGER NOT NULL DEFAULT 0,
            has_related_docs INTEGER NOT NULL DEFAULT 0,
            has_history      INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_doc_type ON documents(type);
        CREATE INDEX IF NOT EXISTS idx_doc_date ON documents(date);
        CREATE INDEX IF NOT EXISTS idx_doc_withdrawn ON documents(withdrawn_date);

        CREATE TABLE IF NOT EXISTS chunks (
            chunk_id      INTEGER PRIMARY KEY,
            doc_id        TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
            ord           INTEGER NOT NULL,
            anchor        TEXT,
            text          BLOB NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_chunks_doc ON chunks(doc_id);
        CREATE INDEX IF NOT EXISTS idx_chunks_doc_ord ON chunks(doc_id, ord);

        CREATE TABLE IF NOT EXISTS definitions (
            definition_id TEXT PRIMARY KEY,
            term          TEXT NOT NULL,
            norm_term     TEXT NOT NULL,
            doc_id        TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
            source_title  TEXT NOT NULL,
            source_type   TEXT NOT NULL,
            scope         TEXT,
            anchor        TEXT,
            ord           INTEGER NOT NULL,
            body          TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_definitions_norm_term ON definitions(norm_term);
        CREATE INDEX IF NOT EXISTS idx_definitions_doc ON definitions(doc_id);

        CREATE TABLE IF NOT EXISTS document_assets (
            asset_ref     TEXT PRIMARY KEY,
            doc_id        TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
            source_path   TEXT NOT NULL,
            relative_path TEXT NOT NULL,
            media_type    TEXT,
            alt           TEXT,
            title         TEXT,
            sha256        TEXT NOT NULL,
            bytes         INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_assets_doc ON document_assets(doc_id);

        CREATE TABLE IF NOT EXISTS doc_anchors (
            doc_id           TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
            ord              INTEGER NOT NULL,
            kind             TEXT NOT NULL,
            label            TEXT NOT NULL,
            target_chunk_id  INTEGER,
            target_doc_id    TEXT,
            target_pit       TEXT,
            PRIMARY KEY (doc_id, ord)
        );
        CREATE INDEX IF NOT EXISTS idx_doc_anchors_doc ON doc_anchors(doc_id);

        CREATE TABLE IF NOT EXISTS citations (
            source_chunk_id  INTEGER NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
            source_doc_id    TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
            target_doc_id    TEXT NOT NULL,
            PRIMARY KEY (source_chunk_id, target_doc_id)
        );
        CREATE INDEX IF NOT EXISTS idx_citations_target ON citations(target_doc_id);

        CREATE TABLE IF NOT EXISTS chunk_embeddings (
            chunk_id   INTEGER PRIMARY KEY REFERENCES chunks(chunk_id) ON DELETE CASCADE,
            embedding  BLOB NOT NULL CHECK(length(embedding) = 256)
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS title_fts USING fts5(
            doc_id UNINDEXED,
            title,
            headings,
            tokenize = "porter unicode61 remove_diacritics 2"
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
            text,
            tokenize = "porter unicode61 remove_diacritics 2"
        );

        CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )?;
    set_meta(conn, "schema_version", "8")?;
    Ok(())
}

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = ?")?;
    let mut rows = stmt.query([key])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn canonical_url(doc_id: &str) -> String {
    // [OF-01] canonical_url is synthesized directly from doc_id.
    format!("https://www.ato.gov.au/law/view/document?docid={}", doc_id)
}

fn decompress_text(blob: Vec<u8>) -> Result<String> {
    let bytes = zstd::stream::decode_all(Cursor::new(blob))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn compress_text(text: &str) -> Result<Vec<u8>> {
    Ok(zstd::stream::encode_all(Cursor::new(text.as_bytes()), 3)?)
}

fn fts_query(query: &str) -> String {
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

fn glob_to_like(pattern: &str) -> String {
    // [MT-13] User glob filters translate '*' to LIKE '%' and escape LIKE metacharacters.
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

#[derive(Default)]
struct SqlFilter {
    sql: String,
    params: Vec<Value>,
}

fn build_doc_filter(
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
        clauses.push(format!(
            "({alias}.date IS NULL OR {alias}.date >= ? OR {alias}.type = ?)"
        ));
        params_out.push(Value::Text(OLD_CONTENT_CUTOFF.to_string()));
        params_out.push(Value::Text(LEGISLATION_TYPE.to_string()));
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
struct Hit {
    // [MT-04] Search-family hits stay slim; bodies materialize through follow-up tools.
    doc_id: String,
    title: String,
    #[serde(rename = "type")]
    doc_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<String>,
    canonical_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunk_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_call: Option<String>,
    /// W2.2 currency markers — only serialised when set so JSON output for
    /// in-force docs stays clean.
    #[serde(skip_serializing_if = "Option::is_none")]
    withdrawn_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replaces: Option<String>,
    /// Navigation hint flags — only serialised when set (so a doc with no
    /// matching anchors keeps the slim hit clean). `Some(true)` tells the
    /// agent to call `get_doc_anchors(doc_id)` to navigate.
    #[serde(skip_serializing_if = "Option::is_none")]
    has_in_doc_links: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_related_docs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_history: Option<bool>,
}

#[derive(Debug, Clone)]
struct VectorHit {
    chunk_id: i64,
    score: f64,
}

struct SearchOptions<'a> {
    k: usize,
    types: Option<&'a [String]>,
    date_from: Option<&'a str>,
    date_to: Option<&'a str>,
    doc_scope: Option<&'a str>,
    mode: SearchMode,
    sort_by: SortBy,
    include_old: bool,
    /// W2.4: when true (default), withdrawn rulings are excluded from
    /// results. Set to false to include them so the caller sees the
    /// `withdrawn_date`, `superseded_by`, and `replaces` fields on the
    /// hit and can decide whether the source still applies.
    current_only: bool,
    /// Internal-only: maximum chunks returned per document. Capped at
    /// `HARD_MAX_PER_DOC`. NOT exposed in the MCP tool descriptor for
    /// Wave 1 (would inflate the public surface).
    max_per_doc: usize,
    /// When false, hit serialization omits the `snippet` field — callers
    /// that intend to follow up with `get_chunks` save the BM25-windowed
    /// snippet text and the highlight markup pass.
    include_snippet: bool,
    /// When set, vector search uses this chunk's stored embedding as the
    /// query vector and the input `query` string is ignored for the
    /// semantic stage. Forces vector-only mode (no BM25 stage). The input
    /// chunk is filtered out of results so the agent never sees their
    /// seed chunk reflected back.
    similar_to_chunk_id: Option<i64>,
}

/// Metadata required to rank and dedup candidate chunks across documents.
#[derive(Debug, Clone)]
struct CandidateMeta {
    doc_id: String,
    /// True when this chunk's plaintext is short (< 100 chars) and the
    /// chunk sits at the start of the document — typically a stub
    /// preamble that crowds out more useful chunks. We approximate "intro"
    /// as ord == 0 with short text, which correctly demotes the leading
    /// stub chunks.
    is_intro: bool,
}

/// Group candidate `(chunk_id, score)` entries by `doc_id`, demote
/// intros, and emit at most `max_per_doc` chunks per document until `k`
/// is reached. Per-document score is the max of the top three chunk
/// scores within that document. Pure function — no DB access — so it
/// can be tested in isolation.
fn dedup_per_doc(
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
    // with `get_chunks` / `get_document`.
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

fn rerank_head_count(k: usize, candidate_count: usize) -> usize {
    let desired = std::cmp::max(k.saturating_mul(5), 12);
    std::cmp::min(
        candidate_count,
        std::cmp::min(RERANK_CANDIDATE_LIMIT, desired),
    )
}

fn search(
    query: &str,
    opts: SearchOptions<'_>,
    mut server_state: Option<&mut ServerState>,
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
    let effective_mode = if similar_seed.is_some() {
        SearchMode::Vector
    } else {
        opts.mode
    };
    let lexical_hits = if matches!(effective_mode, SearchMode::Hybrid | SearchMode::Keyword) {
        lexical_search(&conn, query, &filter, internal_limit)?
    } else {
        Vec::new()
    };
    let mut ranked_hits = match effective_mode {
        SearchMode::Hybrid | SearchMode::Vector => {
            ensure_vector_search_ready(&conn)?;
            let query_embedding = if let Some((_, ref seed_vec)) = similar_seed {
                *seed_vec
            } else {
                match server_state.as_mut() {
                    Some(state) => state.encode_query_embedding(query)?,
                    None => encode_query_embedding(query)?,
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

    // Cross-encoder rerank stage. We rerank only the top head because
    // each ONNX inference is O(N) and the marginal hit past the first
    // few pages is dominated by first-stage recall. Tail candidates retain
    // their RRF order so they can still surface via `next_call` paging or
    // recency sort. Skip rerank when the caller passed
    // `similar_to_chunk_id` — there's no meaningful query string to use
    // as the rerank pivot.
    if similar_seed.is_none() && !matches!(effective_mode, SearchMode::Keyword) {
        if let Some(state) = server_state.as_mut() {
        let head_count = rerank_head_count(k, ranked_hits.len());
        if head_count > 0 {
            // Load text for the top-N candidates in one batch. We hold
            // them as `String`s because the tokenizer wants `&str`s and
            // the rusqlite blob borrow doesn't survive across iterations.
            let head_ids: Vec<i64> = ranked_hits[..head_count]
                .iter()
                .map(|h| h.chunk_id)
                .collect();
            let texts = load_chunk_texts(&conn, &head_ids)?;
            let candidate_refs: Vec<(i64, &str)> = head_ids
                .iter()
                .filter_map(|id| texts.get(id).map(|t| (*id, t.as_str())))
                .collect();
            if !candidate_refs.is_empty() {
                if let Some(scores) = state.rerank_candidates(query, &candidate_refs)? {
                    let reranker_scores: HashMap<i64, f64> = scores.iter().copied().collect();
                    // Re-order the head by reranker score (desc). Tail
                    // (below RERANK_CANDIDATE_LIMIT) keeps RRF order. We
                    // overwrite the per-chunk score with the reranker
                    // value for the head so downstream code (dedup,
                    // recency sort) can rank by overall merit without a
                    // second branch.
                    let mut head: Vec<VectorHit> = ranked_hits.drain(..head_count).collect();
                    for hit in head.iter_mut() {
                        if let Some(s) = reranker_scores.get(&hit.chunk_id) {
                            hit.score = *s;
                        }
                    }
                    head.sort_by(|a, b| {
                        b.score
                            .partial_cmp(&a.score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    let mut new_ranked: Vec<VectorHit> = Vec::with_capacity(candidate_count);
                    new_ranked.extend(head);
                    new_ranked.append(&mut ranked_hits);
                    ranked_hits = new_ranked;
                }
            }
        }
    }
    }

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
            // First-stage RRF / reranker scores drive `deduped` ordering;
            // we just iterate in that order. Internal scores never reach
            // the agent — results are presented sorted by relevance.
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
    let mut response = serde_json::Map::new();
    response.insert("hits".to_string(), json!(records));
    if !meta.is_empty() {
        response.insert("meta".to_string(), JsonValue::Object(meta));
    }
    Ok(serde_json::to_string_pretty(&JsonValue::Object(response))?)
}

fn search_cli(query: &str, opts: SearchOptions<'_>) -> Result<(String, ServerState)> {
    let mut state = ServerState::default();
    let out = search(query, opts, Some(&mut state))?;
    Ok((out, state))
}

/// Batch-load decompressed chunk text for the given ids. Returns a map
/// keyed by chunk_id; missing rows are silently dropped (caller treats
/// missing texts as "no rerank candidate" and they fall through to the
/// tail). One round-trip + one zstd decode per id.
fn load_chunk_texts(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut unique = ids.to_vec();
    unique.sort_unstable();
    unique.dedup();
    let placeholders = vec!["?"; unique.len()].join(",");
    let sql = format!("SELECT chunk_id, text FROM chunks WHERE chunk_id IN ({placeholders})");
    let params_vec: Vec<Value> = unique.into_iter().map(Value::Integer).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params_vec), |row| {
        let chunk_id: i64 = row.get("chunk_id")?;
        let blob: Vec<u8> = row.get("text")?;
        Ok((chunk_id, blob))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (chunk_id, blob) = row?;
        let text = decompress_text(blob)?;
        out.insert(chunk_id, text);
    }
    Ok(out)
}

fn load_candidate_meta(
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
    let sql = format!(
        "SELECT chunk_id, doc_id, ord FROM chunks WHERE chunk_id IN ({placeholders})"
    );
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
        let params_vec2: Vec<Value> = leading_chunk_ids
            .into_iter()
            .map(Value::Integer)
            .collect();
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

fn search_next_call(query: &str, k: usize, opts: &SearchOptions<'_>) -> String {
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
    format!("search({})", args.join(", "))
}

fn mcp_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type IN ('table', 'virtual table') AND name = ?1)",
        [table],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
}

/// Load a chunk's stored int8 embedding from `chunk_embeddings`. Used by
/// `similar_to_chunk_id` to bypass query encoding and run vector search
/// directly against the seed chunk's vector.
fn load_chunk_embedding(conn: &Connection, chunk_id: i64) -> Result<[i8; EMBEDDING_DIM]> {
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

fn ensure_vector_search_ready(conn: &Connection) -> Result<()> {
    // [MT-09] Hybrid/vector modes require an installed EmbeddingGemma semantic corpus.
    let model_id = get_meta(conn, "embedding_model_id")?.ok_or_else(|| {
        anyhow!("semantic search unavailable: missing embedding_model_id metadata")
    })?;
    if !model_id.starts_with("embeddinggemma") {
        bail!(
            "semantic search unavailable: installed corpus uses unsupported embedding model `{model_id}`; install an EmbeddingGemma corpus"
        );
    }
    if !model_path()?.exists() {
        bail!(
            "semantic search unavailable: model file missing at {}",
            model_path()?.display()
        );
    }
    if !tokenizer_path()?.exists() {
        bail!(
            "semantic search unavailable: tokenizer missing at {}",
            tokenizer_path()?.display()
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

fn vector_search(
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

fn lexical_search(
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

fn rrf_fuse(vector_hits: &[VectorHit], lexical_hits: &[VectorHit]) -> Vec<VectorHit> {
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

fn dot_i8(query: &[i8; EMBEDDING_DIM], document: &[u8]) -> Result<f64> {
    if document.len() != EMBEDDING_DIM {
        bail!(
            "invalid stored embedding length: got {}, expected {}",
            document.len(),
            EMBEDDING_DIM
        );
    }
    // Reinterpret the stored u8 BLOB as i8 by casting the pointer
    // directly. The bit pattern is identical; the BLOB just happens to be
    // loaded with rusqlite's default unsigned typing.
    let mut raw = 0.0f64;
    // Safety: both pointers reference EMBEDDING_DIM-sized slices we just
    // bounds-checked; simsimd_dot_i8 reads exactly `n` bytes from each.
    unsafe {
        simsimd_dot_i8(
            query.as_ptr(),
            document.as_ptr() as *const i8,
            EMBEDDING_DIM,
            &mut raw,
        );
    }
    Ok(raw / (127.0 * 127.0))
}

#[cfg(test)]
fn dot_i8_scalar_reference(query: &[i8; EMBEDDING_DIM], document: &[u8]) -> Result<f64> {
    if document.len() != EMBEDDING_DIM {
        bail!(
            "invalid stored embedding length: got {}, expected {}",
            document.len(),
            EMBEDDING_DIM
        );
    }
    let mut dot = 0i32;
    for (q, d) in query.iter().zip(document.iter()) {
        dot += i32::from(*q) * i32::from(*d as i8);
    }
    Ok(dot as f64 / (127.0 * 127.0))
}

struct SemanticRuntime {
    tokenizer: Tokenizer,
    session: Session,
    has_token_type_ids: bool,
}

impl SemanticRuntime {
    fn load() -> Result<Self> {
        let mut tokenizer = Tokenizer::from_file(tokenizer_path()?)
            .map_err(|err| anyhow!("loading tokenizer: {err}"))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_TOKENS,
                ..TruncationParams::default()
            }))
            .map_err(|err| anyhow!("configuring tokenizer truncation: {err}"))?;
        tokenizer.with_padding(Some(PaddingParams::default()));

        let session = Session::builder()
            .map_err(|err| anyhow!("creating ONNX Runtime session: {err}"))?
            .with_optimization_level(ONLINE_MODEL_OPTIMIZATION_LEVEL)
            .map_err(|err| anyhow!("configuring ONNX Runtime session: {err}"))?
            .commit_from_file(model_path()?)
            .map_err(|err| anyhow!("loading ONNX model: {err}"))?;
        let has_token_type_ids = session
            .inputs()
            .iter()
            .any(|input| input.name() == "token_type_ids");

        Ok(Self {
            tokenizer,
            session,
            has_token_type_ids,
        })
    }

    fn encode_query(&mut self, query: &str) -> Result<[i8; EMBEDDING_DIM]> {
        let prefixed = format!("{QUERY_PREFIX}{query}");
        let mut encodings = self
            .tokenizer
            .encode_batch(vec![prefixed], true)
            .map_err(|err| anyhow!("tokenizing query: {err}"))?;
        let encoding = encodings
            .pop()
            .ok_or_else(|| anyhow!("tokenizer returned no query encoding"))?;
        let input_ids: Vec<i64> = encoding.get_ids().iter().map(|id| i64::from(*id)).collect();
        let attention_mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|mask| i64::from(*mask))
            .collect();
        let seq_len = input_ids.len();
        if seq_len == 0 {
            bail!("semantic search unavailable: query produced no tokens");
        }

        let input_ids_tensor =
            TensorRef::from_array_view(([1usize, seq_len], input_ids.as_slice()))?;
        let attention_mask_tensor =
            TensorRef::from_array_view(([1usize, seq_len], attention_mask.as_slice()))?;
        let outputs = if self.has_token_type_ids {
            let token_type_ids = vec![0i64; seq_len];
            let token_type_ids_tensor =
                TensorRef::from_array_view(([1usize, seq_len], token_type_ids.as_slice()))?;
            self.session.run(ort::inputs! {
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
                "token_type_ids" => token_type_ids_tensor,
            })?
        } else {
            self.session.run(ort::inputs! {
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
            })?
        };
        let output = outputs
            .get("sentence_embedding")
            .unwrap_or_else(|| &outputs[0]);
        let (shape, data) = output.try_extract_tensor::<f32>()?;
        let embedding = pooled_embedding(shape, data, &attention_mask)?;
        quantize_embedding(&embedding)
    }
}

/// Cross-encoder reranker ONNX model.
/// Loaded lazily on first search and cached on `ServerState`. Inputs are
/// `[CLS] query [SEP] doc [SEP]` token pairs; the model emits a single
/// relevance logit per pair which we squash through sigmoid into [0, 1].
struct Reranker {
    tokenizer: Tokenizer,
    session: Session,
    has_token_type_ids: bool,
}

impl Reranker {
    fn load() -> Result<Self> {
        let mut tokenizer = Tokenizer::from_file(reranker_tokenizer_path()?)
            .map_err(|err| anyhow!("loading reranker tokenizer: {err}"))?;
        // Cap each side at PAIR_MAX_TOKENS; the tokenizer trims the
        // longest segment first so a long doc won't push the query out.
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: RERANK_PAIR_MAX_TOKENS,
                ..TruncationParams::default()
            }))
            .map_err(|err| anyhow!("configuring reranker truncation: {err}"))?;
        tokenizer.with_padding(Some(PaddingParams::default()));

        let session = Session::builder()
            .map_err(|err| anyhow!("creating reranker ONNX Runtime session: {err}"))?
            .with_optimization_level(ONLINE_MODEL_OPTIMIZATION_LEVEL)
            .map_err(|err| anyhow!("configuring reranker ONNX Runtime session: {err}"))?
            .commit_from_file(reranker_model_path()?)
            .map_err(|err| anyhow!("loading reranker ONNX model: {err}"))?;
        let has_token_type_ids = session
            .inputs()
            .iter()
            .any(|input| input.name() == "token_type_ids");

        Ok(Self {
            tokenizer,
            session,
            has_token_type_ids,
        })
    }

    /// Score `(chunk_id, doc_text)` candidates against `query`. Returns
    /// pairs in input order; the caller is responsible for re-sorting.
    /// The query is hard-truncated to roughly `RERANK_QUERY_MAX_TOKENS`
    /// tokens upstream of tokenization. Note: the constant is in TOKENS;
    /// we approximate ~4 chars per token for the pre-tokenization trim
    /// (cheaper than re-running the tokenizer twice). The tokenizer's own
    /// truncation handles the doc side; we leave a wide margin so the
    /// 512-token budget can absorb a long chunk body.
    fn rerank(&mut self, query: &str, candidates: &[(i64, &str)]) -> Result<Vec<(i64, f64)>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        // Trim query token budget by approximating ~4 chars/token for
        // English (cheaper than re-running the tokenizer twice). The
        // model's truncation guarantees we stay within 512 total.
        let query_max_chars = RERANK_QUERY_MAX_TOKENS * 4;
        let query_trimmed: String = query.chars().take(query_max_chars).collect();

        let inputs: Vec<(String, String)> = candidates
            .iter()
            .map(|(_, doc)| (query_trimmed.clone(), (*doc).to_string()))
            .collect();
        let encodings = self
            .tokenizer
            .encode_batch(inputs, true)
            .map_err(|err| anyhow!("tokenizing reranker pairs: {err}"))?;
        let batch = encodings.len();
        if batch == 0 {
            return Ok(Vec::new());
        }
        let seq_len = encodings[0].get_ids().len();
        if seq_len == 0 {
            bail!("reranker tokenizer returned zero-length encoding");
        }

        let mut input_ids: Vec<i64> = Vec::with_capacity(batch * seq_len);
        let mut attention_mask: Vec<i64> = Vec::with_capacity(batch * seq_len);
        let mut token_type_ids: Vec<i64> = Vec::with_capacity(batch * seq_len);
        for enc in &encodings {
            // BatchLongest padding guarantees uniform seq_len, but we
            // assert defensively to avoid silently feeding ragged shapes.
            if enc.get_ids().len() != seq_len {
                bail!(
                    "reranker batch produced ragged encodings: expected {seq_len}, got {}",
                    enc.get_ids().len()
                );
            }
            input_ids.extend(enc.get_ids().iter().map(|id| i64::from(*id)));
            attention_mask.extend(enc.get_attention_mask().iter().map(|m| i64::from(*m)));
            token_type_ids.extend(enc.get_type_ids().iter().map(|t| i64::from(*t)));
        }

        let input_ids_tensor =
            TensorRef::from_array_view(([batch, seq_len], input_ids.as_slice()))?;
        let attention_mask_tensor =
            TensorRef::from_array_view(([batch, seq_len], attention_mask.as_slice()))?;
        let outputs = if self.has_token_type_ids {
            let token_type_ids_tensor =
                TensorRef::from_array_view(([batch, seq_len], token_type_ids.as_slice()))?;
            self.session.run(ort::inputs! {
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
                "token_type_ids" => token_type_ids_tensor,
            })?
        } else {
            self.session.run(ort::inputs! {
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
            })?
        };
        // Cross-encoders typically output `logits` as `[batch, 1]`. Some
        // exports emit a flat `[batch]` instead. Try the named output
        // first so users with non-standard wrappers still work.
        let output = outputs.get("logits").unwrap_or_else(|| &outputs[0]);
        let (shape, data) = output.try_extract_tensor::<f32>()?;
        let logits = extract_rerank_logits(shape, data, batch)?;
        if logits.len() != batch {
            bail!(
                "reranker produced {} logits for batch of {}",
                logits.len(),
                batch
            );
        }
        Ok(candidates
            .iter()
            .zip(logits)
            .map(|((id, _), logit)| (*id, sigmoid(logit as f64)))
            .collect())
    }
}

fn extract_rerank_logits(shape: &[i64], data: &[f32], batch: usize) -> Result<Vec<f32>> {
    match shape {
        [b] if *b as usize == batch => Ok(data[..batch].to_vec()),
        [b, 1] if *b as usize == batch => Ok(data[..batch].to_vec()),
        [b, d] if *b as usize == batch && *d as usize >= 1 => {
            // Some reranker exports emit `[batch, 2]` (positive/negative
            // logits). Take the positive class only — index 1 is the
            // standard convention for ms-marco rerankers.
            let dims = *d as usize;
            let positive = if dims == 1 { 0 } else { 1 };
            Ok((0..batch).map(|i| data[i * dims + positive]).collect())
        }
        _ => bail!("unexpected reranker output shape {:?}", shape),
    }
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// Tracks reranker availability across server lifetime. Once a load
/// attempt fails (or the model file is missing) we record `Disabled` so
/// every subsequent search short-circuits to RRF without a retry storm.
#[derive(Default)]
enum RerankerState {
    /// Not yet attempted in this process. Triggers a single load on first
    /// `rerank_candidates` call.
    #[default]
    Pending,
    /// Cross-encoder loaded and ready. Boxed so the enum stays small —
    /// `Reranker` owns an ONNX `Session` and a `Tokenizer`, both of
    /// which are large enough that an unboxed variant would inflate
    /// every `RerankerState` instance.
    Loaded(Box<Reranker>),
    /// Either `ATO_MCP_DISABLE_RERANKER` was set, the model files were
    /// missing, or load failed. We do not retry within this process.
    Disabled,
}

// [MT-01] MCP stdio keeps one ServerState per process and reuses lazy runtimes.
// [SW-04] SemanticRuntime/reranker load once; failed reranker loads disable reranking for the session.
#[derive(Default)]
struct ServerState {
    semantic_runtime: Option<SemanticRuntime>,
    reranker_state: RerankerState,
}

impl ServerState {
    fn encode_query_embedding(&mut self, query: &str) -> Result<[i8; EMBEDDING_DIM]> {
        if self.semantic_runtime.is_none() {
            self.semantic_runtime = Some(SemanticRuntime::load()?);
        }
        self.semantic_runtime
            .as_mut()
            .expect("semantic runtime was just initialized")
            .encode_query(query)
    }

    /// Cross-encoder rerank entry point. Returns `Ok(None)` whenever the
    /// reranker is unavailable (env-var disabled, model files missing, or
    /// previously failed to load) so the caller falls back to RRF.
    fn rerank_candidates(
        &mut self,
        query: &str,
        candidates: &[(i64, &str)],
    ) -> Result<Option<Vec<(i64, f64)>>> {
        if env_truthy("ATO_MCP_DISABLE_RERANKER") {
            // Once disabled (via env var or model-load failure), the
            // reranker stays disabled for the rest of this server session
            // — no per-request retry. Restart the server to re-enable.
            self.reranker_state = RerankerState::Disabled;
            return Ok(None);
        }
        if candidates.is_empty() {
            return Ok(Some(Vec::new()));
        }
        // Drive the state machine. We replace `Pending` once and never
        // again — failed loads stick at `Disabled`.
        if matches!(self.reranker_state, RerankerState::Pending) {
            let model_present = reranker_model_path().map(|p| p.exists()).unwrap_or(false);
            let tokenizer_present = reranker_tokenizer_path()
                .map(|p| p.exists())
                .unwrap_or(false);
            if !model_present || !tokenizer_present {
                eprintln!(
                    "ato-mcp: reranker model files not present (model={}, tokenizer={}); falling back to RRF for the rest of this session",
                    model_present, tokenizer_present
                );
                self.reranker_state = RerankerState::Disabled;
                return Ok(None);
            }
            match Reranker::load() {
                Ok(r) => self.reranker_state = RerankerState::Loaded(Box::new(r)),
                Err(err) => {
                    eprintln!(
                        "ato-mcp: failed to load reranker ({err}); falling back to RRF for the rest of this session"
                    );
                    self.reranker_state = RerankerState::Disabled;
                    return Ok(None);
                }
            }
        }
        match &mut self.reranker_state {
            RerankerState::Loaded(r) => Ok(Some(r.rerank(query, candidates)?)),
            RerankerState::Disabled => Ok(None),
            // Unreachable: we just ensured Pending was resolved above.
            RerankerState::Pending => Ok(None),
        }
    }
}

fn encode_query_embedding(query: &str) -> Result<[i8; EMBEDDING_DIM]> {
    let mut runtime = SemanticRuntime::load()?;
    runtime.encode_query(query)
}

fn pooled_embedding(shape: &[i64], data: &[f32], attention_mask: &[i64]) -> Result<Vec<f32>> {
    match shape {
        [1, dims] => {
            let dims = *dims as usize;
            if data.len() < dims {
                bail!("model output too short for shape {:?}", shape);
            }
            Ok(data[..dims].to_vec())
        }
        [1, seq_len, dims] => {
            let seq_len = *seq_len as usize;
            let dims = *dims as usize;
            if data.len() < seq_len * dims {
                bail!("model output too short for shape {:?}", shape);
            }
            let mut pooled = vec![0.0f32; dims];
            let mut denom = 0.0f32;
            for token_idx in 0..seq_len {
                let mask = attention_mask.get(token_idx).copied().unwrap_or(0) as f32;
                denom += mask;
                let offset = token_idx * dims;
                for dim in 0..dims {
                    pooled[dim] += data[offset + dim] * mask;
                }
            }
            let denom = denom.max(1e-6);
            for value in &mut pooled {
                *value /= denom;
            }
            Ok(pooled)
        }
        _ => bail!("unsupported model output shape {:?}", shape),
    }
}

fn quantize_embedding(values: &[f32]) -> Result<[i8; EMBEDDING_DIM]> {
    if values.len() < EMBEDDING_DIM {
        bail!(
            "model output has {} dimensions, expected at least {}",
            values.len(),
            EMBEDDING_DIM
        );
    }
    let values = &values[..EMBEDDING_DIM];
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm <= 1e-12 {
        return Ok([0; EMBEDDING_DIM]);
    }
    let mut out = [0i8; EMBEDDING_DIM];
    for (idx, value) in values.iter().enumerate() {
        out[idx] = ((*value / norm).clamp(-1.0, 1.0) * 127.0).round() as i8;
    }
    Ok(out)
}

fn load_hit(
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
fn snippet_query_terms(query: &str) -> Vec<String> {
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
fn bm25_score_window(
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
fn highlight_snippet(text: &str, query: &str, max_chars: usize) -> String {
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

fn trim_chars(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let mut end = max_chars;
    while end < s.len() && !s.is_char_boundary(end) {
        end += 1;
    }
    s[..end].to_string()
}

fn ato_doc_id_from_link(value: &str) -> Option<String> {
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

fn direct_doc_id_from_query(query: &str) -> Option<String> {
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

fn direct_title_hits(
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

fn load_title_hit(conn: &Connection, doc_id: &str, filter: &SqlFilter) -> Result<Option<Hit>> {
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

fn search_titles(
    query: &str,
    k: usize,
    types: Option<&[String]>,
    include_old: bool,
    current_only: bool,
) -> Result<String> {
    // [MT-14] search_titles ranks title_fts independently and uses the same default filters.
    let conn = open_read()?;
    let k = k.clamp(1, 100);
    let filter = build_doc_filter(
        "d",
        types,
        None,
        None,
        None,
        include_old,
        current_only,
    );
    let direct_hits = direct_title_hits(&conn, query, k, &filter)?;
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
    params_vec.extend(filter.params);
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
    let truncated = rows.len() > k;
    rows.truncate(k);
    let mut response = serde_json::Map::new();
    response.insert("hits".to_string(), json!(rows));
    if truncated {
        response.insert("meta".to_string(), json!({"truncated": true}));
    }
    Ok(serde_json::to_string_pretty(&JsonValue::Object(response))?)
}

struct GetDocumentOptions {
    /// `false` (default) joins chunk plaintext; `true` serves the raw
    /// cleaned-HTML blob from `documents.html`. Pagination applies to
    /// whichever body is selected.
    as_html: bool,
    max_chars: Option<usize>,
    from_char: Option<usize>,
}

fn get_document(doc_id: &str, opts: GetDocumentOptions) -> Result<String> {
    let conn = open_read()?;
    let doc = load_document_row(&conn, doc_id)?;
    let Some(doc) = doc else {
        return Ok(format!("_Document not found: `{}`_", doc_id));
    };
    let body = if opts.as_html {
        load_document_html(&conn, &doc.doc_id)?.unwrap_or_default()
    } else {
        load_document_plaintext(&conn, &doc.doc_id)?
    };
    format_document_response(
        &doc,
        &body,
        opts.as_html,
        opts.from_char.unwrap_or(0),
        opts.max_chars,
    )
}

/// Reassemble the document's plaintext as the chunk pipeline saw it. The
/// chunks already carry the `[doc:X]` / `[asset:X]` annotations after a
/// rebuild, so the agent sees the same text it would in `get_chunks`.
fn load_document_plaintext(conn: &Connection, doc_id: &str) -> Result<String> {
    let mut stmt =
        conn.prepare("SELECT text FROM chunks WHERE doc_id = ? ORDER BY ord ASC")?;
    let rows = stmt.query_map([doc_id], |row| row.get::<_, Vec<u8>>("text"))?;
    let mut parts: Vec<String> = Vec::new();
    for row in rows {
        let blob = row?;
        parts.push(decompress_text(blob)?);
    }
    Ok(parts.join("\n\n"))
}

fn format_document_response(
    doc: &DocumentRow,
    body: &str,
    as_html: bool,
    from_char: usize,
    max_chars: Option<usize>,
) -> Result<String> {
    let total_chars = body.chars().count();
    let start_char = from_char.min(total_chars);
    let start_byte = char_offset_to_byte(body, start_char);
    let remaining = &body[start_byte..];
    let (slice, next_offset) = if let Some(max_chars) = max_chars {
        if remaining.chars().count() > max_chars {
            let end = char_offset_to_byte(remaining, max_chars);
            (&remaining[..end], Some(start_char + max_chars))
        } else {
            (remaining, None)
        }
    } else {
        (remaining, None)
    };
    let next_call = next_offset.map(|next| {
        let suffix = if as_html { ", as_html=true" } else { "" };
        format!(
            "get_document(doc_id=\"{}\", from_char={}, max_chars={}{})",
            doc.doc_id,
            next,
            max_chars.unwrap_or(20_000),
            suffix,
        )
    });
    let mut meta = serde_json::Map::new();
    meta.insert(
        "total_chars".to_string(),
        JsonValue::Number(serde_json::Number::from(total_chars as u64)),
    );
    if next_offset.is_some() {
        meta.insert("truncated".to_string(), JsonValue::Bool(true));
        if let Some(call) = next_call.as_ref() {
            meta.insert(
                "next_call".to_string(),
                JsonValue::String(call.to_string()),
            );
        }
    }
    Ok(serde_json::to_string_pretty(&json!({
        "document": doc,
        "text": slice,
        "meta": meta,
    }))?)
}

#[derive(Debug, Serialize)]
struct DocumentRow {
    doc_id: String,
    #[serde(rename = "type")]
    doc_type: String,
    title: String,
    date: Option<String>,
    downloaded_at: String,
    canonical_url: String,
}

fn load_document_row(conn: &Connection, doc_id: &str) -> Result<Option<DocumentRow>> {
    let mut stmt = conn.prepare(
        "SELECT doc_id, type, title, date, downloaded_at FROM documents WHERE doc_id = ?",
    )?;
    let mut rows = stmt.query([doc_id])?;
    if let Some(row) = rows.next()? {
        let doc_id: String = row.get("doc_id")?;
        Ok(Some(DocumentRow {
            canonical_url: canonical_url(&doc_id),
            doc_id,
            doc_type: row.get("type")?,
            title: row.get("title")?,
            date: row.get("date")?,
            downloaded_at: row.get("downloaded_at")?,
        }))
    } else {
        Ok(None)
    }
}

fn load_document_html(conn: &Connection, doc_id: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT html FROM documents WHERE doc_id = ?")?;
    let mut rows = stmt.query([doc_id])?;
    if let Some(row) = rows.next()? {
        let blob: Vec<u8> = row.get("html")?;
        Ok(Some(decompress_text(blob)?))
    } else {
        Ok(None)
    }
}

fn char_offset_to_byte(value: &str, chars: usize) -> usize {
    value
        .char_indices()
        .nth(chars)
        .map(|(idx, _)| idx)
        .unwrap_or(value.len())
}

struct GetDefinitionOptions<'a> {
    context_doc_id: Option<&'a str>,
    max_defs: usize,
}

#[derive(Debug, Serialize, Clone)]
struct DefinitionSource {
    doc_id: String,
    title: String,
    #[serde(rename = "type")]
    source_type: String,
    scope: Option<String>,
    anchor: Option<String>,
    canonical_url: String,
}

#[derive(Debug, Serialize, Clone)]
struct DefinitionHit {
    definition_id: String,
    term: String,
    kind: String,
    body: String,
    source: DefinitionSource,
}

#[derive(Debug, Serialize, Clone)]
struct OrdinaryDefinition {
    part_of_speech: Option<String>,
    definition: String,
}

#[derive(Debug, Serialize, Clone)]
struct OrdinaryMeaningHit {
    term: String,
    kind: String,
    source: String,
    definitions: Vec<OrdinaryDefinition>,
}

#[derive(Debug, Deserialize)]
struct DictionaryEntry {
    term: String,
    definition: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    part_of_speech: Option<String>,
}

fn normalize_definition_term(term: &str) -> String {
    let cleaned = term
        .replace("\\*", "*")
        .trim_matches(|ch: char| ch.is_whitespace() || ch == ':' || ch == '*')
        .to_string();
    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn context_prefix(context_doc_id: Option<&str>) -> Option<String> {
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

fn definition_rank(hit: &DefinitionHit, opts: &GetDefinitionOptions<'_>) -> usize {
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

fn get_definition(term: &str, opts: GetDefinitionOptions<'_>) -> Result<String> {
    let conn = open_read()?;
    if !table_exists(&conn, "definitions")? {
        let (ordinary, ordinary_error) = ordinary_meaning_or_error(term);
        return format_definition_response(term, &[], ordinary, ordinary_error, false);
    }
    let norm = normalize_definition_term(term);
    let max_defs = opts.max_defs.clamp(1, 20);
    let mut stmt = conn.prepare(
        r#"
        SELECT definition_id, term, doc_id, source_title, source_type, scope,
               anchor, body
        FROM definitions
        WHERE norm_term = ? AND source_type = ?
        ORDER BY doc_id, ord, term
        LIMIT 100
        "#,
    )?;
    let mut hits = stmt
        .query_map(params![norm, LEGISLATION_TYPE], |row| {
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

fn ordinary_meaning_or_error(term: &str) -> (Option<OrdinaryMeaningHit>, Option<String>) {
    match lookup_ordinary_meaning(term) {
        Ok(hit) => (hit, None),
        Err(err) => (None, Some(err.to_string())),
    }
}

fn ordinary_dictionary_dir() -> Result<PathBuf> {
    let path = data_dir()?.join("ordinary-meaning");
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn ordinary_dictionary_index_path() -> Result<PathBuf> {
    Ok(ordinary_dictionary_dir()?.join("open-english-wordnet-2024.tsv"))
}

fn lookup_ordinary_meaning(term: &str) -> Result<Option<OrdinaryMeaningHit>> {
    if let Some(path) = std::env::var_os(ORDINARY_DICTIONARY_PATH_ENV) {
        let path = PathBuf::from(path);
        let source = path.display().to_string();
        return lookup_ordinary_meaning_file(&path, &source, term);
    }
    let path = ensure_oewn_ordinary_dictionary()?;
    lookup_ordinary_meaning_file(&path, OEWN_2024_SOURCE, term)
}

fn ensure_oewn_ordinary_dictionary() -> Result<PathBuf> {
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

fn build_oewn_dictionary_index(zip_bytes: &[u8]) -> Result<String> {
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

fn read_zip_member_by_suffix(
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

fn parse_oewn_data_file(content: &str, part_of_speech: &str, rows: &mut Vec<String>) {
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

fn clean_ordinary_field(value: &str) -> String {
    value
        .replace(['\t', '\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches('"')
        .to_string()
}

fn lookup_ordinary_meaning_file(
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

fn ordinary_from_dictionary_entries(
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

fn ordinary_from_tsv(raw: &str, wanted: &str, source: &str) -> Result<Option<OrdinaryMeaningHit>> {
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

fn format_definition_response(
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

fn stats() -> Result<String> {
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
    let payload = json!({
        "data_dir": data_dir()?.display().to_string(),
        "index_version": get_meta(&conn, "index_version")?,
        "last_update_at": get_meta(&conn, "last_update_at")?,
        "embedding_model_id": get_meta(&conn, "embedding_model_id")?,
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
            "old_content_cutoff": OLD_CONTENT_CUTOFF,
            "old_content_exception_types": [LEGISLATION_TYPE],
        }
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
fn collect_prefix_breakdown(conn: &rusqlite::Connection) -> Result<Vec<JsonValue>> {
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
fn description_from_title(title: &str) -> String {
    const SEP: &str = " \u{2014} ";
    match title.find(SEP) {
        Some(idx) => title[..idx].trim().to_string(),
        None => title.trim().to_string(),
    }
}

fn doctor(rollback: bool) -> Result<()> {
    if rollback {
        // [UM-06] Rollback restores the previous DB snapshot from backups/ato.db.prev.
        let backup = backups_dir()?.join("ato.db.prev");
        if !backup.exists() {
            bail!("no backup found at {}", backup.display());
        }
        fs::copy(&backup, db_path()?)?;
        println!("rollback complete.");
        return Ok(());
    }
    let conn = open_read()?;
    let docs: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?;
    let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
    if docs == 0 || chunks == 0 {
        bail!("corpus is empty: documents={docs}, chunks={chunks}");
    }
    println!("documents: {docs}");
    println!("chunks: {chunks}");
    let model_id = get_meta(&conn, "embedding_model_id")?.unwrap_or_default();
    if model_id.starts_with("embeddinggemma") {
        ensure_vector_search_ready(&conn)?;
        let embeddings: i64 =
            conn.query_row("SELECT COUNT(*) FROM chunk_embeddings", [], |r| r.get(0))?;
        println!("chunk_embeddings: {embeddings}");
        println!("semantic_search: ready");
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
struct Manifest {
    schema_version: i64,
    index_version: String,
    created_at: String,
    min_client_version: String,
    model: ModelInfo,
    /// Optional cross-encoder reranker. Manifests without one degrade
    /// gracefully to RRF-only ranking.
    #[serde(skip_serializing_if = "Option::is_none")]
    reranker: Option<ModelInfo>,
    documents: Vec<DocRef>,
    packs: Vec<PackInfo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ModelInfo {
    id: String,
    sha256: String,
    size: u64,
    url: String,
    /// Optional sha256 of the companion tokenizer file. Reranker manifests
    /// must set it; tar.zst model bundles verify the bundle as a whole and
    /// ignore this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokenizer_sha256: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct UpdateSummary {
    schema_version: i64,
    index_version: String,
    #[serde(default)]
    min_client_version: String,
    model: ModelInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reranker: Option<ModelInfo>,
    document_count: usize,
    pack_count: usize,
    #[serde(default)]
    manifest_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DocRef {
    doc_id: String,
    content_hash: String,
    pack_sha8: String,
    offset: u64,
    length: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PackInfo {
    sha8: String,
    sha256: String,
    size: u64,
    url: String,
}

struct HfModelFile {
    path: &'static str,
    output_name: &'static str,
    sha256: &'static str,
    size: u64,
}

const EMBEDDINGGEMMA_HF_FILES: &[HfModelFile] = &[
    HfModelFile {
        path: "onnx/model_quantized.onnx",
        output_name: "model_quantized.onnx",
        sha256: "172efde319fe1542dc41f31be6154910b05b78f7a861c265c4600eec906bd6d8",
        size: 567_874,
    },
    HfModelFile {
        path: "onnx/model_quantized.onnx_data",
        output_name: "model_quantized.onnx_data",
        sha256: "705626e28e4c23c82ade34566b4197d97f534c12275fa406dfb71e9937d388c0",
        size: 308_890_624,
    },
    HfModelFile {
        path: "tokenizer.json",
        output_name: "tokenizer.json",
        sha256: "4dda02faaf32bc91031dc8c88457ac272b00c1016cc679757d1c441b248b9c47",
        size: 20_323_312,
    },
];

#[derive(Default)]
struct UpdateStats {
    added: usize,
    changed: usize,
    removed: usize,
    bytes_downloaded: u64,
}

fn apply_update(manifest_url: &str) -> Result<UpdateStats> {
    // [UM-01] apply_update holds the app LOCK around all install/update mutation.
    let lock = lock_file()?;
    let result = apply_update_locked(manifest_url);
    lock.unlock()?;
    result
}

fn update_before_serve() -> Result<()> {
    // [CC-02] serve only checks for updates when explicitly opted in, and falls back to the installed DB if that update fails.
    let url = default_manifest_url();
    match apply_update(&url) {
        Ok(stats) => {
            eprintln!(
                "ato-mcp serve: update complete (+{} ~{} -{}, {:.2} MB downloaded)",
                stats.added,
                stats.changed,
                stats.removed,
                stats.bytes_downloaded as f64 / 1_000_000.0
            );
            Ok(())
        }
        Err(err) => {
            if db_path()?.exists() {
                eprintln!("ato-mcp serve: update failed; serving installed corpus: {err}");
                Ok(())
            } else {
                Err(err).context("ato-mcp serve could not install the corpus before startup")
            }
        }
    }
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.as_str(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(false)
}

fn serve_should_check_update(check_update: bool) -> bool {
    !env_truthy("ATO_MCP_OFFLINE") && (check_update || env_truthy("ATO_MCP_AUTO_UPDATE"))
}

fn ensure_installed_db() -> Result<()> {
    if db_path()?.exists() {
        Ok(())
    } else {
        bail!("no live DB found; run `ato-mcp init` before serving offline")
    }
}

/// Reject a manifest whose `schema_version` exceeds what this binary knows
/// how to ingest, or whose `min_client_version` is newer than the
/// currently-running binary.
fn enforce_manifest_compatibility(manifest: &Manifest) -> Result<()> {
    // [CC-03] init/update and opted-in serve startup checks share manifest compatibility gates through apply_update.
    let schema_version = manifest.schema_version;
    if schema_version < 0 {
        bail!("manifest schema_version is negative ({schema_version}); manifest is malformed");
    }
    let schema_version = schema_version as u32;
    if schema_version > MAX_SUPPORTED_MANIFEST_VERSION {
        bail!(
            "installed corpus requires ato-mcp >= newer version (manifest schema_version={schema_version}, this binary supports up to {MAX_SUPPORTED_MANIFEST_VERSION}); please upgrade the ato-mcp binary"
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
    Ok(())
}

/// Compare two dotted version strings (`a.b.c[-suffix]`) by their numeric
/// components only. Returns `Ordering::Less/Equal/Greater` for the first
/// arg relative to the second. Pre-release suffixes are ignored.
fn cmp_dotted_version(a: &str, b: &str) -> std::cmp::Ordering {
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

fn apply_update_locked(manifest_url: &str) -> Result<UpdateStats> {
    // [UM-05] Delta updates mutate SQLite transactionally, verify, then write installed_manifest last.
    let manifest_context = UrlContext::from_manifest_url(manifest_url);
    if let Some(stats) = try_skip_update_from_summary(manifest_url, &manifest_context)? {
        return Ok(stats);
    }
    let staging = staging_dir()?;
    let manifest_bytes = fetch_bytes(manifest_url, &manifest_context)
        .with_context(|| format!("fetching manifest from {manifest_url}"))?;
    let new_manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
    enforce_manifest_compatibility(&new_manifest)?;
    let old_manifest = load_installed_manifest()?;

    ensure_model(&new_manifest, &manifest_context, &staging)?;
    // Reranker is optional and best-effort. Failures here log to stderr
    // but never abort an otherwise-successful corpus update — search
    // falls back to RRF when the cross-encoder isn't available.
    if new_manifest.reranker.is_some() {
        if let Err(err) = ensure_reranker(&new_manifest, &manifest_context, &staging) {
            eprintln!("ato-mcp: reranker download failed ({err}); search will fall back to RRF");
        }
    }

    let db = db_path()?;
    let had_existing_db = db.exists();
    let (added, mut changed, removed) = diff_manifests(old_manifest.as_ref(), &new_manifest);
    let rebuild_for_schema = if had_existing_db {
        live_db_requires_rebuild(&db)?
    } else {
        false
    };
    let rebuild_for_missing_manifest = had_existing_db && old_manifest.is_none();
    let rebuild_for_replacement = whole_corpus_replacement(
        old_manifest.as_ref(),
        &new_manifest,
        &added,
        &changed,
        &removed,
    );
    let semantic_backfill = if had_existing_db
        && !rebuild_for_schema
        && !rebuild_for_missing_manifest
        && !rebuild_for_replacement
    {
        let conn = open_read()?;
        semantic_backfill_required(&conn, &new_manifest)?
    } else {
        false
    };
    if semantic_backfill {
        let added_ids = added
            .iter()
            .map(|doc| doc.doc_id.as_str())
            .collect::<HashSet<_>>();
        changed = new_manifest
            .documents
            .iter()
            .filter(|doc| !added_ids.contains(doc.doc_id.as_str()))
            .cloned()
            .collect();
    }

    if !had_existing_db
        || rebuild_for_schema
        || rebuild_for_missing_manifest
        || semantic_backfill
        || rebuild_for_replacement
    {
        return rebuild_live_db_from_manifest(
            &new_manifest,
            &manifest_context,
            manifest_bytes.len() as u64,
            added.len(),
            changed.len(),
            removed.len(),
        );
    }

    let conn = open_write_at(&db_path()?)?;
    init_db(&conn)?;

    let backup = backups_dir()?.join("ato.db.prev");
    if had_existing_db {
        fs::copy(&db, &backup)?;
    }

    let mut bytes_downloaded = manifest_bytes.len() as u64;
    let tx = conn.unchecked_transaction()?;
    let apply_result = (|| -> Result<()> {
        for doc_id in &removed {
            delete_doc(&tx, doc_id)?;
        }
        for doc in &changed {
            delete_doc(&tx, &doc.doc_id)?;
        }

        let docs_to_insert = added
            .iter()
            .chain(changed.iter())
            .cloned()
            .collect::<Vec<_>>();
        insert_docs_from_packs(
            &tx,
            &new_manifest,
            &manifest_context,
            &docs_to_insert,
            &mut bytes_downloaded,
            &live_assets_root()?,
        )?;
        set_meta(&tx, "index_version", &new_manifest.index_version)?;
        set_meta(&tx, "embedding_model_id", &new_manifest.model.id)?;
        if let Some(reranker) = &new_manifest.reranker {
            set_meta(&tx, "reranker_model_id", &reranker.id)?;
        }
        set_meta(&tx, "last_update_at", &Utc::now().to_rfc3339())?;
        // [UM-07] citations is a derived index of `[doc:X]` markers in
        // chunks.text. When apply_update rewrites changed chunks, the FK
        // ON DELETE CASCADE wipes the corresponding citation rows; the
        // newly-inserted chunks have no citations yet. Re-derive at the
        // tail so the table stays in sync without requiring a separate
        // maintenance command.
        derive_citations(&tx)?;
        verify_semantic_install(&tx, &new_manifest)?;
        Ok(())
    })();

    if let Err(err) = apply_result {
        tx.rollback()?;
        if backup.exists() {
            fs::copy(&backup, db_path()?)?;
        }
        return Err(err);
    }
    tx.commit()?;
    let manifest_json = serde_json::to_vec_pretty(&new_manifest)?;
    fs::write(installed_manifest_path()?, manifest_json)?;
    Ok(UpdateStats {
        added: added.len(),
        changed: changed.len(),
        removed: removed.len(),
        bytes_downloaded,
    })
}

fn whole_corpus_replacement(
    old: Option<&Manifest>,
    new_manifest: &Manifest,
    added: &[DocRef],
    changed: &[DocRef],
    removed: &[String],
) -> bool {
    old.is_some()
        && removed.is_empty()
        && !new_manifest.documents.is_empty()
        && added.len() + changed.len() == new_manifest.documents.len()
}

fn live_db_requires_rebuild(path: &Path) -> Result<bool> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening local corpus database for schema check")?;
    if !table_exists(&conn, "meta")? {
        return Ok(true);
    }
    let Some(value) = get_meta(&conn, "schema_version")? else {
        return Ok(true);
    };
    let Ok(parsed) = value.parse::<u32>() else {
        return Ok(true);
    };
    Ok(parsed != SUPPORTED_SCHEMA_VERSION)
}

fn rebuild_live_db_from_manifest(
    manifest: &Manifest,
    context: &UrlContext,
    manifest_bytes: u64,
    added: usize,
    changed: usize,
    removed: usize,
) -> Result<UpdateStats> {
    let staging_root = staging_dir()?.join("corpus-rebuild");
    if staging_root.exists() {
        fs::remove_dir_all(&staging_root)?;
    }
    fs::create_dir_all(&staging_root)?;
    let staged_db = staging_root.join("ato.db");
    let staged_asset_root = staging_root.join("live");
    let conn = open_write_at(&staged_db)?;
    init_db(&conn)?;

    let mut bytes_downloaded = manifest_bytes;
    let tx = conn.unchecked_transaction()?;
    let apply_result = (|| -> Result<()> {
        insert_docs_from_packs(
            &tx,
            manifest,
            context,
            &manifest.documents,
            &mut bytes_downloaded,
            &staged_asset_root,
        )?;
        set_meta(&tx, "index_version", &manifest.index_version)?;
        set_meta(&tx, "embedding_model_id", &manifest.model.id)?;
        if let Some(reranker) = &manifest.reranker {
            set_meta(&tx, "reranker_model_id", &reranker.id)?;
        }
        set_meta(&tx, "last_update_at", &Utc::now().to_rfc3339())?;
        verify_semantic_install(&tx, manifest)?;
        Ok(())
    })();
    if let Err(err) = apply_result {
        tx.rollback()?;
        return Err(err);
    }
    tx.commit()?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    drop(conn);

    replace_live_db(&staged_db)?;
    replace_live_assets(&staged_asset_root)?;
    let manifest_json = serde_json::to_vec_pretty(manifest)?;
    fs::write(installed_manifest_path()?, manifest_json)?;
    let _ = fs::remove_dir_all(&staging_root);

    Ok(UpdateStats {
        added,
        changed,
        removed,
        bytes_downloaded,
    })
}

fn replace_live_db(staged_db: &Path) -> Result<()> {
    let live = live_dir()?;
    let db = db_path()?;
    let backup = backups_dir()?.join("ato.db.prev");
    if db.exists() {
        fs::copy(&db, &backup)?;
    }
    for suffix in ["-wal", "-shm"] {
        let path = live.join(format!("ato.db{suffix}"));
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    if db.exists() {
        fs::remove_file(&db)?;
    }
    fs::rename(staged_db, &db).inspect_err(|_err| {
        if backup.exists() {
            let _ = fs::copy(&backup, &db);
        }
    })?;
    Ok(())
}

fn replace_live_assets(staged_asset_root: &Path) -> Result<()> {
    let live_assets = live_dir()?.join("assets");
    if live_assets.exists() {
        fs::remove_dir_all(&live_assets)?;
    }
    let staged_assets = staged_asset_root.join("assets");
    if staged_assets.exists() {
        fs::rename(staged_assets, &live_assets)?;
    } else {
        fs::create_dir_all(&live_assets)?;
    }
    Ok(())
}

fn insert_docs_from_packs(
    conn: &Connection,
    manifest: &Manifest,
    context: &UrlContext,
    docs: &[DocRef],
    bytes_downloaded: &mut u64,
    asset_root: &Path,
) -> Result<()> {
    // [UM-03] Pack bytes are fetched from manifest-resolved assets and sha256-verified.
    let mut pack_to_refs: HashMap<String, Vec<DocRef>> = HashMap::new();
    for doc in docs {
        pack_to_refs
            .entry(doc.pack_sha8.clone())
            .or_default()
            .push(doc.clone());
    }
    let pack_index: HashMap<String, PackInfo> = manifest
        .packs
        .iter()
        .map(|p| (p.sha8.clone(), p.clone()))
        .collect();
    for (sha8, refs) in pack_to_refs {
        let info = pack_index
            .get(&sha8)
            .ok_or_else(|| anyhow!("manifest missing pack info for {sha8}"))?;
        let pack_url = resolve_manifest_asset(&info.url, context);
        let pack_bytes = fetch_bytes(&pack_url, context)
            .with_context(|| format!("fetching pack {}", info.url))?;
        if !info.sha256.is_empty() {
            verify_sha256_bytes(&pack_bytes, &info.sha256)
                .with_context(|| format!("verifying {}", info.url))?;
        }
        *bytes_downloaded += pack_bytes.len() as u64;
        for doc_ref in refs {
            let record = read_record_from_pack_bytes(&pack_bytes, doc_ref.offset, doc_ref.length)?;
            insert_record(conn, &record, &doc_ref, asset_root)?;
        }
    }
    Ok(())
}

fn semantic_backfill_required(conn: &Connection, manifest: &Manifest) -> Result<bool> {
    semantic_backfill_required_for_model(conn, &manifest.model.id)
}

fn semantic_backfill_required_for_model(conn: &Connection, model_id: &str) -> Result<bool> {
    if !model_id.starts_with("embeddinggemma") {
        return Ok(false);
    }
    let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;
    if chunks == 0 {
        return Ok(false);
    }
    let embeddings = chunk_embedding_count(conn)?;
    Ok(embeddings < chunks)
}

fn verify_semantic_install(conn: &Connection, manifest: &Manifest) -> Result<()> {
    if !manifest.model.id.starts_with("embeddinggemma") {
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

fn chunk_embedding_count(conn: &Connection) -> Result<i64> {
    if table_exists(conn, "chunk_embeddings")? {
        conn.query_row("SELECT COUNT(*) FROM chunk_embeddings", [], |row| {
            row.get(0)
        })
        .map_err(Into::into)
    } else {
        Ok(0)
    }
}

fn load_installed_manifest() -> Result<Option<Manifest>> {
    let path = installed_manifest_path()?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

fn update_summary_url_for_manifest(manifest_url: &str) -> String {
    if let Some(path) = local_path_from_urlish(manifest_url) {
        return path.with_file_name("update.json").display().to_string();
    }
    manifest_url
        .rsplit_once('/')
        .map(|(base, _)| format!("{base}/update.json"))
        .unwrap_or_else(|| "update.json".to_string())
}

fn try_skip_update_from_summary(
    manifest_url: &str,
    context: &UrlContext,
) -> Result<Option<UpdateStats>> {
    let Some(installed) = load_installed_manifest()? else {
        return Ok(None);
    };
    let summary_url = update_summary_url_for_manifest(manifest_url);
    let summary_bytes = match fetch_bytes(&summary_url, context) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let summary: UpdateSummary = match serde_json::from_slice(&summary_bytes) {
        Ok(summary) => summary,
        Err(_) => return Ok(None),
    };
    enforce_update_summary_compatibility(&summary)?;
    if !installed_matches_update_summary(&installed, &summary)? {
        return Ok(None);
    }
    Ok(Some(UpdateStats {
        added: 0,
        changed: 0,
        removed: 0,
        bytes_downloaded: summary_bytes.len() as u64,
    }))
}

fn enforce_update_summary_compatibility(summary: &UpdateSummary) -> Result<()> {
    let manifest = Manifest {
        schema_version: summary.schema_version,
        index_version: summary.index_version.clone(),
        created_at: String::new(),
        min_client_version: summary.min_client_version.clone(),
        model: summary.model.clone(),
        reranker: summary.reranker.clone(),
        documents: Vec::new(),
        packs: Vec::new(),
    };
    enforce_manifest_compatibility(&manifest)
}

fn installed_matches_update_summary(installed: &Manifest, summary: &UpdateSummary) -> Result<bool> {
    let Some(summary_fingerprint) = summary.manifest_fingerprint.as_deref() else {
        return Ok(false);
    };
    if installed.schema_version != summary.schema_version
        || installed.index_version != summary.index_version
        || installed.min_client_version != summary.min_client_version
        || installed.documents.len() != summary.document_count
        || installed.packs.len() != summary.pack_count
        || manifest_fingerprint(installed)? != summary_fingerprint
        || !model_info_matches(&installed.model, &summary.model)
        || !optional_model_info_matches(installed.reranker.as_ref(), summary.reranker.as_ref())
    {
        return Ok(false);
    }

    let db = db_path()?;
    if !db.exists() || live_db_requires_rebuild(&db)? {
        return Ok(false);
    }
    if !embedding_model_installed_matches(&summary.model)? {
        return Ok(false);
    }
    if let Some(reranker) = &summary.reranker {
        if !reranker_installed_matches(reranker)? {
            return Ok(false);
        }
    }
    let conn = open_read()?;
    Ok(!semantic_backfill_required_for_model(
        &conn,
        &summary.model.id,
    )?)
}

fn manifest_fingerprint(manifest: &Manifest) -> Result<String> {
    let mut documents = manifest.documents.clone();
    documents.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
    let mut packs = manifest.packs.clone();
    packs.sort_by(|a, b| a.sha8.cmp(&b.sha8));
    let payload = json!({
        "documents": documents.iter().map(|d| json!({
            "doc_id": d.doc_id,
            "content_hash": d.content_hash,
            "pack_sha8": d.pack_sha8,
            "offset": d.offset,
            "length": d.length,
        })).collect::<Vec<_>>(),
        "packs": packs.iter().map(|p| json!({
            "sha8": p.sha8,
            "sha256": p.sha256,
            "size": p.size,
            "url": p.url,
        })).collect::<Vec<_>>(),
    });
    let bytes = serde_json::to_vec(&payload)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn model_info_matches(left: &ModelInfo, right: &ModelInfo) -> bool {
    left.id == right.id
        && left.sha256 == right.sha256
        && left.size == right.size
        && left.url == right.url
        && left.tokenizer_sha256 == right.tokenizer_sha256
}

fn optional_model_info_matches(left: Option<&ModelInfo>, right: Option<&ModelInfo>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => model_info_matches(left, right),
        _ => false,
    }
}

fn embedding_model_marker_value(info: &ModelInfo) -> String {
    if info.sha256.is_empty() && parse_hf_model_url(&info.url).is_some() {
        EMBEDDINGGEMMA_HF_FINGERPRINT.to_string()
    } else {
        info.sha256.clone()
    }
}

fn embedding_model_installed_matches(info: &ModelInfo) -> Result<bool> {
    if !info.id.starts_with("embeddinggemma") {
        return Ok(false);
    }
    let marker_value = embedding_model_marker_value(info);
    let marker = live_dir()?.join(".model.sha256");
    Ok(model_path()?.exists()
        && tokenizer_path()?.exists()
        && marker.exists()
        && fs::read_to_string(marker)?.trim() == marker_value)
}

fn reranker_installed_matches(info: &ModelInfo) -> Result<bool> {
    if info.sha256.is_empty() {
        return Ok(false);
    }
    let marker = live_dir()?.join(".reranker.sha256");
    Ok(reranker_model_path()?.exists()
        && reranker_tokenizer_path()?.exists()
        && marker.exists()
        && fs::read_to_string(marker)?.trim() == info.sha256)
}

fn diff_manifests(
    old: Option<&Manifest>,
    new: &Manifest,
) -> (Vec<DocRef>, Vec<DocRef>, Vec<String>) {
    let old_docs: HashMap<&str, &DocRef> = old
        .map(|m| m.documents.iter().map(|d| (d.doc_id.as_str(), d)).collect())
        .unwrap_or_default();
    let new_docs: HashMap<&str, &DocRef> = new
        .documents
        .iter()
        .map(|d| (d.doc_id.as_str(), d))
        .collect();
    let mut added = Vec::new();
    let mut changed = Vec::new();
    for doc in &new.documents {
        match old_docs.get(doc.doc_id.as_str()) {
            None => added.push(doc.clone()),
            Some(old_doc) if !doc_ref_matches(old_doc, doc) => changed.push(doc.clone()),
            _ => {}
        }
    }
    let removed = old_docs
        .keys()
        .filter(|doc_id| !new_docs.contains_key(**doc_id))
        .map(|doc_id| (*doc_id).to_string())
        .collect();
    (added, changed, removed)
}

fn doc_ref_matches(old: &DocRef, new: &DocRef) -> bool {
    old.content_hash == new.content_hash
        && old.pack_sha8 == new.pack_sha8
        && old.offset == new.offset
        && old.length == new.length
}

#[derive(Clone)]
struct UrlContext {
    manifest_dir: Option<PathBuf>,
    manifest_base_url: Option<String>,
}

impl UrlContext {
    fn from_manifest_url(manifest_url: &str) -> Self {
        if let Some(path) = local_path_from_urlish(manifest_url) {
            return Self {
                manifest_dir: path.parent().map(Path::to_path_buf),
                manifest_base_url: None,
            };
        }
        let manifest_base_url = manifest_url
            .rsplit_once('/')
            .map(|(base, _)| base.to_string());
        Self {
            manifest_dir: None,
            manifest_base_url,
        }
    }
}

fn resolve_manifest_asset(asset_url: &str, context: &UrlContext) -> String {
    if asset_url.starts_with("http://")
        || asset_url.starts_with("https://")
        || asset_url.starts_with("file://")
    {
        return asset_url.to_string();
    }
    if let Some(dir) = &context.manifest_dir {
        return dir.join(asset_url).display().to_string();
    }
    if let Some(base) = &context.manifest_base_url {
        return format!(
            "{}/{}",
            base.trim_end_matches('/'),
            asset_url.trim_start_matches('/')
        );
    }
    asset_url.to_string()
}

fn local_path_from_urlish(value: &str) -> Option<PathBuf> {
    if let Ok(url) = Url::parse(value) {
        if url.scheme() == "file" {
            return url.to_file_path().ok();
        }
        return None;
    }
    let path = PathBuf::from(value);
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

fn fetch_bytes(url_or_path: &str, context: &UrlContext) -> Result<Vec<u8>> {
    // [UM-04] The Rust downloader is credential-free: no GitHub token env vars and no gh shell-out.
    if let Some(path) = local_path_from_urlish(url_or_path) {
        return Ok(fs::read(path)?);
    }
    if let Some(dir) = &context.manifest_dir {
        if let Some(name) = url_or_path.rsplit('/').next() {
            for candidate in [dir.join(name), dir.join("packs").join(name)] {
                if candidate.exists() {
                    return Ok(fs::read(candidate)?);
                }
            }
        }
    }
    let client = http_client()?;
    let mut resp = client.get(url_or_path).send()?.error_for_status().with_context(|| {
        format!(
            "download failed for {url_or_path}. This Rust client does not read GitHub tokens or invoke gh; use a public release, an authenticated mirror, or a file URL."
        )
    })?;
    let mut out = Vec::new();
    resp.copy_to(&mut out)?;
    Ok(out)
}

fn fetch_to_file(url_or_path: &str, context: &UrlContext, dest: &Path) -> Result<u64> {
    if let Some(path) = local_path_from_urlish(url_or_path) {
        fs::copy(path, dest).map_err(Into::into)
    } else if let Some(dir) = &context.manifest_dir {
        if let Some(name) = url_or_path.rsplit('/').next() {
            for candidate in [dir.join(name), dir.join("packs").join(name)] {
                if candidate.exists() {
                    return fs::copy(candidate, dest).map_err(Into::into);
                }
            }
        }
        fetch_http_to_file(url_or_path, dest)
    } else {
        fetch_http_to_file(url_or_path, dest)
    }
}

fn fetch_http_to_file(url: &str, dest: &Path) -> Result<u64> {
    let client = http_client()?;
    let mut resp = client.get(url).send()?.error_for_status().with_context(|| {
        format!(
            "download failed for {url}. This Rust client does not read GitHub tokens or invoke gh; use a public release, an authenticated mirror, or a file URL."
        )
    })?;
    let mut file = File::create(dest)?;
    Ok(resp.copy_to(&mut file)?)
}

fn parse_hf_model_url(value: &str) -> Option<(&str, &str)> {
    let spec = value.strip_prefix("hf://")?;
    let (repo, revision) = spec.split_once('@').unwrap_or((spec, "main"));
    if repo.is_empty() || revision.is_empty() {
        None
    } else {
        Some((repo, revision))
    }
}

fn hf_resolve_url(repo: &str, revision: &str, path: &str) -> String {
    format!("https://huggingface.co/{repo}/resolve/{revision}/{path}")
}

fn http_client() -> Result<Client> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()?)
}

fn verify_sha256_bytes(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        bail!("sha256 mismatch: got {actual}, expected {expected}");
    }
    Ok(())
}

fn verify_sha256_file(path: &Path, expected: &str) -> Result<()> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1024 * 64];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        bail!(
            "sha256 mismatch for {}: got {actual}, expected {expected}",
            path.display()
        );
    }
    Ok(())
}

fn ensure_model(manifest: &Manifest, context: &UrlContext, staging: &Path) -> Result<()> {
    if !manifest.model.id.starts_with("embeddinggemma") {
        bail!(
            "semantic search requires an EmbeddingGemma model bundle; manifest uses `{}`",
            manifest.model.id
        );
    }
    let live_model = model_path()?;
    let tokenizer = tokenizer_path()?;
    let marker = live_dir()?.join(".model.sha256");
    let marker_value =
        if manifest.model.sha256.is_empty() && parse_hf_model_url(&manifest.model.url).is_some() {
            EMBEDDINGGEMMA_HF_FINGERPRINT
        } else {
            manifest.model.sha256.as_str()
        };
    if live_model.exists()
        && tokenizer.exists()
        && marker.exists()
        && fs::read_to_string(&marker)?.trim() == marker_value
    {
        return Ok(());
    }

    if let Some((repo, revision)) = parse_hf_model_url(&manifest.model.url) {
        install_hf_embedding_model(repo, revision, staging)?;
        fs::write(marker, marker_value)?;
        return Ok(());
    }

    let bundle_url = resolve_manifest_asset(&manifest.model.url, context);
    let bundle = staging.join("model-bundle.tar.zst.part");
    fetch_to_file(&bundle_url, context, &bundle)?;
    if !manifest.model.sha256.is_empty() {
        verify_sha256_file(&bundle, &manifest.model.sha256)?;
    }
    let extract_dir = staging.join("model-bundle-extracted");
    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)?;
    }
    fs::create_dir_all(&extract_dir)?;
    let bundle_file = File::open(&bundle)?;
    let decoder = zstd::stream::read::Decoder::new(bundle_file)?;
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(&extract_dir)?;

    for entry in fs::read_dir(&extract_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::rename(entry.path(), live_dir()?.join(entry.file_name()))?;
        }
    }
    fs::write(marker, marker_value)?;
    let _ = fs::remove_file(bundle);
    let _ = fs::remove_dir_all(extract_dir);
    Ok(())
}

fn install_hf_embedding_model(repo: &str, revision: &str, staging: &Path) -> Result<()> {
    fs::create_dir_all(staging)?;
    for file in EMBEDDINGGEMMA_HF_FILES {
        let url = hf_resolve_url(repo, revision, file.path);
        let part = staging.join(format!("{}.part", file.output_name));
        fetch_http_to_file(&url, &part)
            .with_context(|| format!("downloading Hugging Face model file {}", file.path))?;
        verify_sha256_file(&part, file.sha256)
            .with_context(|| format!("verifying Hugging Face model file {}", file.path))?;
        let size = part.metadata()?.len();
        if size != file.size {
            bail!(
                "size mismatch for Hugging Face model file {}: got {}, expected {}",
                file.path,
                size,
                file.size
            );
        }
        let dest = live_dir()?.join(file.output_name);
        if dest.exists() {
            fs::remove_file(&dest)?;
        }
        fs::rename(&part, dest)?;
    }
    Ok(())
}

/// Download (or refresh) the optional cross-encoder reranker into
/// `live_dir()`. Caller is responsible for checking `manifest.reranker
/// .is_some()` before invoking. Mirrors `ensure_model`'s caching:
/// if the local files match the manifest's sha256 we skip the download.
///
/// Two download shapes are accepted:
///   1. `hf://owner/repo[@revision]` — fetch `onnx/model_quantized.onnx` +
///      `tokenizer.json` from the Hugging Face mirror, sha-verify the model.
///   2. Any other URL — treated as a tar.zst bundle (the EmbeddingGemma
///      pattern). The bundle MUST contain `reranker.onnx` AND
///      `reranker_tokenizer.json` at the archive root. The bundle's
///      sha256 is verified against `manifest.reranker.sha256`.
fn ensure_reranker(manifest: &Manifest, context: &UrlContext, staging: &Path) -> Result<()> {
    let info = manifest
        .reranker
        .as_ref()
        .ok_or_else(|| anyhow!("ensure_reranker called with no reranker entry in manifest"))?;
    let live_model = reranker_model_path()?;
    let live_tokenizer = reranker_tokenizer_path()?;
    let marker = live_dir()?.join(".reranker.sha256");
    let marker_value = info.sha256.as_str();
    if !marker_value.is_empty()
        && live_model.exists()
        && live_tokenizer.exists()
        && marker.exists()
        && fs::read_to_string(&marker)?.trim() == marker_value
    {
        return Ok(());
    }

    if let Some((repo, revision)) = parse_hf_model_url(&info.url) {
        install_hf_reranker(repo, revision, info, staging)?;
        if !marker_value.is_empty() {
            fs::write(marker, marker_value)?;
        }
        return Ok(());
    }

    let bundle_url = resolve_manifest_asset(&info.url, context);
    let bundle = staging.join("reranker-bundle.tar.zst.part");
    fetch_to_file(&bundle_url, context, &bundle)?;
    if !info.sha256.is_empty() {
        verify_sha256_file(&bundle, &info.sha256)?;
    }
    let extract_dir = staging.join("reranker-bundle-extracted");
    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)?;
    }
    fs::create_dir_all(&extract_dir)?;
    let bundle_file = File::open(&bundle)?;
    let decoder = zstd::stream::read::Decoder::new(bundle_file)?;
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(&extract_dir)?;

    let staged_model = extract_dir.join("reranker.onnx");
    let staged_tokenizer = extract_dir.join("reranker_tokenizer.json");
    if !staged_model.exists() || !staged_tokenizer.exists() {
        bail!(
            "reranker bundle is missing required files (expected reranker.onnx + reranker_tokenizer.json)"
        );
    }
    if live_model.exists() {
        fs::remove_file(&live_model)?;
    }
    if live_tokenizer.exists() {
        fs::remove_file(&live_tokenizer)?;
    }
    fs::rename(&staged_model, &live_model)?;
    fs::rename(&staged_tokenizer, &live_tokenizer)?;
    if !marker_value.is_empty() {
        fs::write(marker, marker_value)?;
    }
    let _ = fs::remove_file(bundle);
    let _ = fs::remove_dir_all(extract_dir);
    Ok(())
}

fn install_hf_reranker(repo: &str, revision: &str, info: &ModelInfo, staging: &Path) -> Result<()> {
    fs::create_dir_all(staging)?;
    let model_part = download_hf_reranker_model(repo, revision, info, staging)?;
    let tokenizer_part = staging.join("reranker_tokenizer.json.part");
    let tokenizer_url = hf_resolve_url(repo, revision, "tokenizer.json");
    fetch_http_to_file(&tokenizer_url, &tokenizer_part)
        .with_context(|| format!("downloading reranker tokenizer from {repo}"))?;
    let expected_tokenizer_sha = info
        .tokenizer_sha256
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("reranker manifest is missing tokenizer_sha256 for {repo}"))?;
    verify_sha256_file(&tokenizer_part, expected_tokenizer_sha)
        .with_context(|| format!("verifying reranker tokenizer from {repo}"))?;

    let live_model = reranker_model_path()?;
    let live_tokenizer = reranker_tokenizer_path()?;
    if live_model.exists() {
        fs::remove_file(&live_model)?;
    }
    if live_tokenizer.exists() {
        fs::remove_file(&live_tokenizer)?;
    }
    fs::rename(&model_part, &live_model)?;
    fs::rename(&tokenizer_part, &live_tokenizer)?;
    Ok(())
}

fn download_hf_reranker_model(
    repo: &str,
    revision: &str,
    info: &ModelInfo,
    staging: &Path,
) -> Result<PathBuf> {
    download_hf_reranker_model_with(repo, revision, info, staging, |url, dest| {
        fetch_http_to_file(url, dest)
    })
}

fn download_hf_reranker_model_with<F>(
    repo: &str,
    revision: &str,
    info: &ModelInfo,
    staging: &Path,
    mut fetch: F,
) -> Result<PathBuf>
where
    F: FnMut(&str, &Path) -> Result<u64>,
{
    fs::create_dir_all(staging)?;
    let model_part = staging.join("reranker.onnx.part");
    let url = hf_resolve_url(repo, revision, RERANKER_HF_MODEL_PATH);
    if let Err(err) = fetch(&url, &model_part) {
        let _ = fs::remove_file(&model_part);
        bail!("downloading reranker model {RERANKER_HF_MODEL_PATH} from {repo}: {err}");
    }
    if !info.sha256.is_empty() {
        if let Err(err) = verify_sha256_file(&model_part, &info.sha256) {
            let _ = fs::remove_file(&model_part);
            bail!("verifying reranker model {RERANKER_HF_MODEL_PATH} from {repo}: {err}");
        }
    }
    Ok(model_part)
}

#[derive(Debug, Deserialize)]
struct PackRecord {
    doc_id: String,
    #[serde(default, rename = "type")]
    doc_type: String,
    title: String,
    date: Option<String>,
    downloaded_at: String,
    content_hash: String,
    html: String,
    /// W2.2 currency markers. The insert_record regression test proves these
    /// pack fields survive ingestion into the searchable SQLite corpus.
    #[serde(default)]
    withdrawn_date: Option<String>,
    #[serde(default)]
    superseded_by: Option<String>,
    #[serde(default)]
    replaces: Option<String>,
    /// Navigation hint flags. Set at build time by the maintainer pipeline
    /// from the doc_anchors table; ingestion writes them straight through.
    #[serde(default)]
    has_in_doc_links: i64,
    #[serde(default)]
    has_related_docs: i64,
    #[serde(default)]
    has_history: i64,
    /// Per-doc navigation anchors emitted by the build pipeline; ingested
    /// straight into the doc_anchors table.
    #[serde(default)]
    anchors: Vec<PackDocAnchor>,
    #[serde(default)]
    definitions: Vec<PackDefinition>,
    assets: Vec<PackAsset>,
    #[serde(default)]
    chunks: Vec<PackChunk>,
}

#[derive(Debug, Deserialize)]
struct PackAsset {
    asset_ref: String,
    source_path: String,
    relative_path: String,
    media_type: Option<String>,
    alt: Option<String>,
    title: Option<String>,
    sha256: String,
    size: i64,
    data_b64: String,
}

#[derive(Debug, Deserialize)]
struct PackDefinition {
    definition_id: String,
    term: String,
    norm_term: String,
    doc_id: String,
    source_title: String,
    source_type: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    anchor: Option<String>,
    ord: i64,
    body: String,
}

#[derive(Debug, Deserialize)]
struct PackChunk {
    ord: i64,
    #[serde(default)]
    anchor: Option<String>,
    text: String,
    #[serde(default)]
    embedding_b64: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PackDocAnchor {
    ord: i64,
    kind: String,
    label: String,
    #[serde(default)]
    target_chunk_id: Option<i64>,
    #[serde(default)]
    target_doc_id: Option<String>,
    #[serde(default)]
    target_pit: Option<String>,
}

fn read_record_from_pack_bytes(pack: &[u8], offset: u64, length: u64) -> Result<PackRecord> {
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

fn insert_record(
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
        conn.execute(
            "INSERT INTO chunks (doc_id, ord, anchor, text) VALUES (?, ?, ?, ?)",
            params![record.doc_id, chunk.ord, chunk.anchor, blob,],
        )?;
        let rowid = conn.last_insert_rowid();
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
        )?;
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

fn write_record_assets(conn: &Connection, record: &PackRecord, asset_root: &Path) -> Result<()> {
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

fn decode_embedding_b64(value: &str) -> Result<Vec<u8>> {
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

fn delete_doc(conn: &Connection, doc_id: &str) -> Result<()> {
    let chunk_ids = {
        let mut stmt = conn.prepare("SELECT chunk_id FROM chunks WHERE doc_id = ?")?;
        let rows = stmt
            .query_map([doc_id], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for chunk_id in chunk_ids {
        conn.execute(
            "DELETE FROM chunk_embeddings WHERE chunk_id = ?",
            [chunk_id],
        )?;
        conn.execute("DELETE FROM chunks_fts WHERE rowid = ?", [chunk_id])?;
    }
    conn.execute("DELETE FROM title_fts WHERE doc_id = ?", [doc_id])?;
    conn.execute("DELETE FROM chunks WHERE doc_id = ?", [doc_id])?;
    conn.execute("DELETE FROM documents WHERE doc_id = ?", [doc_id])?;
    Ok(())
}

fn serve() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut state = ServerState::default();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: serde_json::Result<JsonValue> = serde_json::from_str(&line);
        let response = match parsed {
            Ok(message) => handle_rpc(message, &mut state),
            Err(err) => Some(json_rpc_error(
                JsonValue::Null,
                -32700,
                &format!("parse error: {err}"),
            )),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut stdout, &response)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

fn handle_rpc(message: JsonValue, state: &mut ServerState) -> Option<JsonValue> {
    if message.is_array() {
        let responses: Vec<JsonValue> = message
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| handle_single_rpc(m.clone(), state))
            .collect();
        if responses.is_empty() {
            None
        } else {
            Some(JsonValue::Array(responses))
        }
    } else {
        handle_single_rpc(message, state)
    }
}

fn handle_single_rpc(message: JsonValue, state: &mut ServerState) -> Option<JsonValue> {
    let id = message.get("id").cloned();
    let Some(method) = message.get("method").and_then(|m| m.as_str()) else {
        return id.map(|id| json_rpc_error(id, -32600, "invalid request"));
    };
    let id = id?;
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "ato-mcp", "version": env!("CARGO_PKG_VERSION") },
            "instructions": server_instructions(),
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_descriptors() })),
        "tools/call" => {
            let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
            call_tool(params, state)
        }
        _ => Err(anyhow!("method not found: {method}")),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(err) => json_rpc_error(id, -32000, &err.to_string()),
    })
}

fn json_rpc_error(id: JsonValue, code: i64, message: &str) -> JsonValue {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

fn call_tool(params: JsonValue, state: &mut ServerState) -> Result<JsonValue> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("tools/call missing params.name"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let text = match name {
        "search" => {
            let query = required_str(&args, "query")?;
            let types = optional_string_array(&args, "types")?;
            let mode = match args
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("hybrid")
            {
                "hybrid" => SearchMode::Hybrid,
                "vector" => SearchMode::Vector,
                "keyword" => SearchMode::Keyword,
                other => bail!("mode must be one of hybrid, vector, keyword; got `{other}`"),
            };
            let sort_by = match args
                .get("sort_by")
                .and_then(|v| v.as_str())
                .unwrap_or("relevance")
            {
                "recency" => SortBy::Recency,
                _ => SortBy::Relevance,
            };
            search(
                query,
                SearchOptions {
                    k: optional_usize(&args, "k").unwrap_or(DEFAULT_K),
                    types: types.as_deref(),
                    date_from: args.get("date_from").and_then(|v| v.as_str()),
                    date_to: args.get("date_to").and_then(|v| v.as_str()),
                    doc_scope: args.get("doc_scope").and_then(|v| v.as_str()),
                    mode,
                    sort_by,
                    include_old: optional_bool(&args, "include_old").unwrap_or(false),
                    current_only: optional_bool(&args, "current_only").unwrap_or(true),
                    max_per_doc: DEFAULT_MAX_PER_DOC,
                    include_snippet: optional_bool(&args, "include_snippet").unwrap_or(true),
                    similar_to_chunk_id: optional_i64(&args, "similar_to_chunk_id"),
                },
                Some(state),
            )?
        }
        "search_titles" => {
            let query = required_str(&args, "query")?;
            let types = optional_string_array(&args, "types")?;
            search_titles(
                query,
                optional_usize(&args, "k").unwrap_or(20),
                types.as_deref(),
                optional_bool(&args, "include_old").unwrap_or(false),
                optional_bool(&args, "current_only").unwrap_or(true),
            )?
        }
        "get_document" => {
            let doc_id = required_str(&args, "doc_id")?;
            get_document(
                doc_id,
                GetDocumentOptions {
                    as_html: optional_bool(&args, "as_html").unwrap_or(false),
                    max_chars: optional_usize(&args, "max_chars"),
                    from_char: optional_usize(&args, "from_char"),
                },
            )?
        }
        "get_asset" => get_asset_mcp(&args)?,
        "get_doc_anchors" => get_doc_anchors_mcp(&args)?,
        "get_chunks" => get_chunks_mcp(&args)?,
        "get_definition" => {
            let term = required_str(&args, "term")?;
            get_definition(
                term,
                GetDefinitionOptions {
                    context_doc_id: args.get("context_doc_id").and_then(|v| v.as_str()),
                    max_defs: optional_usize(&args, "max_defs").unwrap_or(5),
                },
            )?
        }
        "stats" => stats()?,
        _ => bail!("unknown tool: {name}"),
    };
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    }))
}

fn required_str<'a>(args: &'a JsonValue, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing required string argument `{name}`"))
}

fn optional_usize(args: &JsonValue, name: &str) -> Option<usize> {
    args.get(name).and_then(|v| v.as_u64()).map(|v| v as usize)
}

fn optional_i64(args: &JsonValue, name: &str) -> Option<i64> {
    args.get(name).and_then(|v| v.as_i64())
}

fn optional_bool(args: &JsonValue, name: &str) -> Option<bool> {
    args.get(name).and_then(|v| v.as_bool())
}

fn optional_string_array(args: &JsonValue, name: &str) -> Result<Option<Vec<String>>> {
    let Some(value) = args.get(name) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let array = value
        .as_array()
        .ok_or_else(|| anyhow!("`{name}` must be an array of strings"))?;
    let mut out = Vec::new();
    for item in array {
        out.push(
            item.as_str()
                .ok_or_else(|| anyhow!("`{name}` must be an array of strings"))?
                .to_string(),
        );
    }
    Ok(Some(out))
}

fn get_chunks_mcp(args: &JsonValue) -> Result<String> {
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

struct GetChunksOptions {
    before: usize,
    after: usize,
    max_chars: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
struct HydratedChunk {
    chunk_id: i64,
    requested: bool,
    doc_id: String,
    #[serde(rename = "type")]
    doc_type: String,
    title: String,
    date: Option<String>,
    anchor: Option<String>,
    canonical_url: String,
    text: String,
}

#[derive(Debug, Clone)]
struct ChunkPointer {
    chunk_id: i64,
    doc_id: String,
    ord: i64,
}

fn get_chunks(chunk_ids: &[i64], opts: GetChunksOptions) -> Result<String> {
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
    let next_call = truncated_at.as_ref().map(|chunk| {
        format!("get_chunks(chunk_ids=[{}])", chunk.chunk_id)
    });
    let mut meta = serde_json::Map::new();
    if truncated_at.is_some() {
        meta.insert("truncated".to_string(), JsonValue::Bool(true));
        if let Some(call) = next_call.as_ref() {
            meta.insert(
                "next_call".to_string(),
                JsonValue::String(call.to_string()),
            );
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

fn load_chunks_by_ord_range(
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
    for row in rows {
        let (chunk_id, doc_id, doc_type, title, date, anchor, text_blob) = row?;
        out.push(HydratedChunk {
            chunk_id,
            requested: false,
            doc_id: doc_id.clone(),
            doc_type,
            title,
            date,
            anchor,
            canonical_url: canonical_url(&doc_id),
            text: decompress_text(text_blob)?,
        });
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
struct DocumentAssetOut {
    asset_ref: String,
    doc_id: String,
    source_path: String,
    relative_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    alt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    sha256: String,
    bytes: i64,
    path: String,
}

fn get_asset_mcp(args: &JsonValue) -> Result<String> {
    let asset_ref = required_str(args, "asset_ref")?;
    get_asset(asset_ref)
}

fn get_asset(asset_ref: &str) -> Result<String> {
    let conn = open_read()?;
    let mut stmt = conn.prepare(
        r#"
        SELECT asset_ref, doc_id, source_path, relative_path, media_type,
               alt, title, sha256, bytes
        FROM document_assets
        WHERE asset_ref = ?
        "#,
    )?;
    let mut rows = stmt.query([asset_ref])?;
    let Some(row) = rows.next()? else {
        return Ok(format!("_Asset not found: `{}`_", asset_ref));
    };
    let relative_path: String = row.get("relative_path")?;
    let path = live_dir()?.join(&relative_path);
    if !path.exists() {
        bail!("asset file missing for {asset_ref}: {}", path.display());
    }
    let out = DocumentAssetOut {
        asset_ref: row.get("asset_ref")?,
        doc_id: row.get("doc_id")?,
        source_path: row.get("source_path")?,
        relative_path,
        media_type: row.get("media_type")?,
        alt: row.get("alt")?,
        title: row.get("title")?,
        sha256: row.get("sha256")?,
        bytes: row.get("bytes")?,
        path: path.display().to_string(),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

fn get_doc_anchors_mcp(args: &JsonValue) -> Result<String> {
    let doc_id = required_str(args, "doc_id")?;
    get_doc_anchors(doc_id)
}

/// Convert an ATO point-in-time timestamp (`YYYYMMDDHHMMSS`) to an ISO
/// `YYYY-MM-DD` date. Returns `None` when the input is shorter than 8
/// characters or its first 8 characters are not all digits.
fn pit_to_date(pit: &str) -> Option<String> {
    if pit.len() < 8 {
        return None;
    }
    let head = &pit[..8];
    if !head.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("{}-{}-{}", &head[..4], &head[4..6], &head[6..8]))
}

fn get_doc_anchors(doc_id: &str) -> Result<String> {
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
                        entry.insert(
                            "pit".to_string(),
                            JsonValue::String(pit.to_string()),
                        );
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

/// [MT-17] Per-doc cap on the `cited_by` array surfaced by `get_doc_anchors`. The
/// most heavily-cited docs (ITAA 1997 s 8-1, Pt IVA, ...) have thousands of
/// citers and would otherwise dominate the response. Order by source date
/// DESC so the agent sees the most recent citations first; the total count
/// lives on `cited_by_total` when truncation occurs.
const CITED_BY_LIMIT: usize = 100;

/// [UM-07] Streams `chunks.text` once, regex-extracts every `[doc:X]` marker
/// (PiT / view qualifiers collapse to the base doc_id), and INSERT OR
/// IGNORE-batches into `citations`. Idempotent: clears first.
///
/// Called from `apply_update_locked` so end-user installs stay in sync
/// after a corpus update — the apply_update path rewrites chunks via
/// DELETE+INSERT, which cascades through the FK ON DELETE CASCADE on
/// `citations.source_chunk_id` and wipes the previously-derived rows.
fn derive_citations(conn: &Connection) -> Result<()> {
    if !table_exists(conn, "citations")? {
        return Ok(());
    }
    static DOC_MARKER_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = DOC_MARKER_RE
        .get_or_init(|| Regex::new(r"\[doc:([^\s\]@]+)").expect("valid regex"));

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

fn load_cited_by(conn: &Connection, doc_id: &str) -> Result<(Vec<JsonValue>, i64)> {
    // Gracefully no-op on older corpora that predate the citations table.
    // The next maintainer build (or local `scripts/backfill-citations.py`)
    // populates it; until then this tool just returns an empty list.
    if !table_exists(conn, "citations")? {
        return Ok((Vec::new(), 0));
    }
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

fn server_instructions() -> String {
    // [SW-02] Instructions are generated from live corpus stats.
    // [SW-03] Missing/unreadable stats fall back to static init guidance.
    match stats()
        .ok()
        .and_then(|s| serde_json::from_str::<JsonValue>(&s).ok())
    {
        Some(s) => format!(
            "ATO legal corpus. Documents: {}, chunks: {}. Index: {}. Navigate via search → get_chunks: search returns slim hits (chunk_id, doc_id, anchor, optional snippet); get_chunks fetches bodies by chunk_id with before/after ordinal neighbours within the same doc. doc_scope accepts a single full doc_id for in-doc search or \"<PREFIX>/%\" for a family (see stats). Default search excludes Edited_private_advice, withdrawn rulings, and content dated before {} except legislation; override with current_only=false and include_old=true. Pass `similar_to_chunk_id` on search to find chunks semantically close to one you already have (skips query encoding, vector-only, excludes the seed). Slim hits surface has_in_doc_links / has_related_docs / has_history flags when calling get_doc_anchors(doc_id) would yield useful navigation entries. get_doc_anchors also returns `cited_by`: other documents whose chunks carry a [doc:X] marker pointing at the target (capped at 100, ordered by source date DESC, with `cited_by_total` when truncated). Historical (point-in-time) versions of documents are not stored as separate rows; get_doc_anchors lists them as {{doc_id, pit, date, url}} entries so an agent can fetch the older URL externally.",
            s["documents"].as_i64().unwrap_or(0),
            s["chunks"].as_i64().unwrap_or(0),
            s["index_version"].as_str().unwrap_or("?"),
            OLD_CONTENT_CUTOFF,
        ),
        None => "ATO legal corpus. Run `ato-mcp init` before serving.".to_string(),
    }
}

fn tool_descriptors() -> JsonValue {
    // [SW-01] Eight MCP tools are exposed by tool_descriptors/call_tool: search,
    // search_titles, get_document, get_chunks, get_definition, get_asset,
    // get_doc_anchors, stats.
    //   The surface stays small and explicit; unsupported tools fail through the
    //   normal tools/call error path.
    json!([
        {
            "name": "search",
            "description": "Hybrid semantic+lexical search over chunks (chunk-level). Returns slim pointer hits (chunk_id, doc_id, anchor, optional snippet) — fetch bodies via get_chunks. doc_scope filters by full doc_id (in-doc search) or \"<PREFIX>/%\" (family). mode=keyword forces lexical-only; hybrid/vector require the semantic index. Set include_snippet=false when the caller will follow up with get_chunks. Pass similar_to_chunk_id to find chunks semantically close to one the agent already has (skips query encoding, ignores `query`, forces vector-only mode, filters the seed chunk out of results). Slim hits include navigation hints: has_in_doc_links (doc has paragraph anchors / contents entries — call get_doc_anchors to navigate), has_related_docs (doc has companion documents like errata / addenda), has_history (doc has earlier point-in-time versions — get_doc_anchors lists their URLs).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "k": {"type": "integer", "minimum": 1, "maximum": 50},
                    "types": {"type": "array", "items": {"type": "string"}},
                    "date_from": {"type": "string"},
                    "date_to": {"type": "string"},
                    "doc_scope": {"type": "string"},
                    "mode": {"type": "string", "enum": ["hybrid", "vector", "keyword"]},
                    "sort_by": {"type": "string", "enum": ["relevance", "recency"]},
                    "include_old": {"type": "boolean"},
                    "current_only": {"type": "boolean", "description": "When true (default), excludes withdrawn rulings. Set false to include withdrawn material with a visible marker."},
                    "similar_to_chunk_id": {"type": "integer", "description": "When set, use this chunk's stored embedding as the query vector (skips encoding `query`, forces mode=vector, excludes the seed chunk from results)."},
                    "include_snippet": {"type": "boolean", "description": "When true (default), each hit carries a BM25-windowed snippet. Set false to omit the snippet field entirely — useful when the caller will fetch full text via get_chunks."},
                    "format": {"type": "string", "enum": ["json"], "default": "json"}
                },
                "required": ["query"]
            }
        },
        {
            "name": "search_titles",
            "description": "Title-only search returning doc-level hits (no chunk_id). Use search for chunk-level results. Accepts exact doc_id and ATO document-link lookup. Hits include navigation hints (has_in_doc_links, has_related_docs, has_history) — call get_doc_anchors(doc_id) to navigate.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "k": {"type": "integer", "minimum": 1, "maximum": 100},
                    "types": {"type": "array", "items": {"type": "string"}},
                    "include_old": {"type": "boolean"},
                    "current_only": {"type": "boolean", "description": "When true (default), excludes withdrawn rulings. Set false to include withdrawn material with a visible marker."},
                    "format": {"type": "string", "enum": ["json"], "default": "json"}
                },
                "required": ["query"]
            }
        },
        {
            "name": "get_document",
            "description": "Fetch a full document by doc_id. Default body is plaintext joined from chunks (carries [doc:X] cross-reference and [asset:X] image markers); as_html=true returns cleaned source HTML with data-doc-id and data-asset-ref attributes. Pagination via from_char/max_chars. Prefer search → get_chunks for in-doc reading; this is the full-doc escape hatch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "doc_id": {"type": "string"},
                    "as_html": {"type": "boolean", "description": "When true, return the cleaned source HTML; default plaintext."},
                    "from_char": {"type": "integer"},
                    "max_chars": {"type": "integer"}
                },
                "required": ["doc_id"]
            }
        },
        {
            "name": "get_asset",
            "description": "Resolve an image asset reference (from [asset:X] markers in plaintext or data-asset-ref attributes in HTML) to a local file path plus source metadata.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "asset_ref": {"type": "string"}
                },
                "required": ["asset_ref"]
            }
        },
        {
            "name": "get_doc_anchors",
            "description": "Return the navigation map for a document: in-doc anchors (paragraph references, contents-table entries), sister documents (errata, addenda, withdrawal notices), historical versions (earlier point-in-time publications), and reverse citations (other documents whose chunks carry a [doc:X] marker pointing AT this doc). Slim search hits surface `has_in_doc_links`, `has_related_docs`, or `has_history` when this tool would return useful entries — call it then to navigate. `in_doc` entries carry chunk_id (pass to get_chunks); `related_docs` carry doc_id (pass to search/get_document/get_chunks); `historical_versions` carry {doc_id, pit, date, url}; `cited_by` carries [{doc_id, title, type, date}] ordered by source date DESC and capped at 100 — when more citers exist, `cited_by_total` reports the full count. The corpus does not store historical content; use the historical-version `url` field with WebFetch to retrieve an older version when needed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "doc_id": {"type": "string"}
                },
                "required": ["doc_id"]
            }
        },
        {
            "name": "get_chunks",
            "description": "Fetch chunk bodies by chunk_id. before/after expand the response with ordinal neighbour chunks within the same doc (0-20 each). Plaintext carries [doc:X] cross-reference markers (resolve via search/get_document) and [asset:X] image markers (resolve via get_asset). On max_chars truncation, truncated_at + next_call point at the next chunk_id to continue scrolling.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chunk_ids": {"type": "array", "items": {"type": "integer"}},
                    "before": {"type": "integer", "minimum": 0, "maximum": 20},
                    "after": {"type": "integer", "minimum": 0, "maximum": 20},
                    "max_chars": {"type": "integer", "minimum": 1},
                    "format": {"type": "string", "enum": ["json"], "default": "json"}
                },
                "required": ["chunk_ids"]
            }
        },
        {
            "name": "get_definition",
            "description": "Fetch compact statutory definitions for a term. Returns only matching definition entries, not whole dictionary provisions. If no statutory definition is found, returns a labelled non-statutory ordinary meaning from the configured dictionary source or Open English WordNet.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "term": {"type": "string"},
                    "context_doc_id": {"type": "string"},
                    "max_defs": {"type": "integer", "minimum": 1, "maximum": 20},
                    "format": {"type": "string", "enum": ["json"], "default": "json"}
                },
                "required": ["term"]
            }
        },
        {
            "name": "stats",
            "description": "Index version, document counts, default search policy, and per-prefix corpus breakdown. Use the prefix breakdown to discover the canonical filter idiom doc_scope=\"<PREFIX>/%\" for narrowing searches by document family.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "format": {"type": "string", "enum": ["json"], "default": "json"}
                }
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use rusqlite::Connection;
    use tempfile::tempdir;

    // ----- W1.1 SIMD parity -----

    #[test]
    fn dot_i8_simd_matches_scalar() {
        let mut rng = StdRng::seed_from_u64(42);
        for _ in 0..100 {
            let q: [i8; EMBEDDING_DIM] = std::array::from_fn(|_| rng.gen());
            let d: Vec<u8> = (0..EMBEDDING_DIM).map(|_| rng.gen::<u8>()).collect();
            let scalar = dot_i8_scalar_reference(&q, &d).unwrap();
            let simd = dot_i8(&q, &d).unwrap();
            assert!(
                (scalar - simd).abs() < 1e-6,
                "scalar {} vs simd {}",
                scalar,
                simd
            );
        }
    }

    #[test]
    fn dot_i8_rejects_wrong_length() {
        let q = [0i8; EMBEDDING_DIM];
        let bad = vec![0u8; EMBEDDING_DIM - 1];
        assert!(dot_i8(&q, &bad).is_err());
    }

    // ----- W1.2 BM25 snippets -----

    #[test]
    fn snippet_picks_high_density_window() {
        let mut text = String::new();
        // Filler before
        for _ in 0..30 {
            text.push_str("alpha beta gamma delta epsilon ");
        }
        // The high-density section: query terms cluster here
        text.push_str("the taxpayer claimed an R&D tax incentive deduction for eligible R&D activities in the income year ");
        // Filler after
        for _ in 0..30 {
            text.push_str("zeta eta theta iota kappa ");
        }
        let snippet = highlight_snippet(&text, "R&D tax incentive", SNIPPET_CHARS);
        assert!(
            snippet.contains("R&D tax incentive"),
            "snippet should include the query phrase, got: {snippet}"
        );
    }

    #[test]
    fn snippet_returns_window_without_prefix() {
        let text = "The taxpayer claimed an R&D tax incentive deduction for eligible activities";
        let snippet = highlight_snippet(text, "R&D", SNIPPET_CHARS);
        // Heading text now lives inside chunk.text via inline rendering;
        // the snippet helper no longer produces a heading prefix.
        assert!(snippet.contains("R&D"));
        assert!(
            !snippet.contains(" — "),
            "snippet should not carry a heading prefix delimiter, got: {snippet}"
        );
    }

    // ----- W1.3 hierarchical dedup -----

    fn meta(doc_id: &str, is_intro: bool) -> CandidateMeta {
        CandidateMeta {
            doc_id: doc_id.to_string(),
            is_intro,
        }
    }

    #[test]
    fn dedup_caps_chunks_per_doc() {
        let mut ranked: Vec<VectorHit> = Vec::new();
        let mut metas: HashMap<i64, CandidateMeta> = HashMap::new();
        // 8 chunks for doc A with descending scores
        for i in 0..8 {
            ranked.push(VectorHit {
                chunk_id: i as i64,
                score: 1.0 - (i as f64) * 0.01,
            });
            metas.insert(i as i64, meta("A", false));
        }
        // 2 chunks for doc B
        for j in 0..2 {
            let id = (100 + j) as i64;
            ranked.push(VectorHit {
                chunk_id: id,
                score: 0.5 - (j as f64) * 0.01,
            });
            metas.insert(id, meta("B", false));
        }
        let out = dedup_per_doc(ranked, &metas, 10, DEFAULT_MAX_PER_DOC);
        // Hard cap: no doc should appear more than max_per_doc times in
        // the output, even if there's room left under k.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for h in &out {
            let doc_id = metas.get(&h.chunk_id).unwrap().doc_id.as_str();
            *counts.entry(doc_id).or_insert(0) += 1;
        }
        assert_eq!(counts.get("A"), Some(&2), "A should be capped at 2");
        assert_eq!(counts.get("B"), Some(&2), "B should be capped at 2");
        // 2 docs * 2 = 4 distinct slots filled.
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn dedup_demotes_intro_chunks_within_doc() {
        let mut ranked: Vec<VectorHit> = Vec::new();
        let mut metas: HashMap<i64, CandidateMeta> = HashMap::new();
        // Intro chunk has highest raw score
        ranked.push(VectorHit {
            chunk_id: 1,
            score: 0.9,
        });
        metas.insert(1, meta("A", true));
        // Non-intro chunk in the same doc
        ranked.push(VectorHit {
            chunk_id: 2,
            score: 0.5,
        });
        metas.insert(2, meta("A", false));
        let out = dedup_per_doc(ranked, &metas, 1, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chunk_id, 2, "non-intro chunk should outrank intro");
    }

    #[test]
    fn dedup_orders_docs_by_max_chunk_score() {
        let mut ranked: Vec<VectorHit> = Vec::new();
        let mut metas: HashMap<i64, CandidateMeta> = HashMap::new();
        // Doc A: best chunk score 0.5
        ranked.push(VectorHit {
            chunk_id: 1,
            score: 0.5,
        });
        metas.insert(1, meta("A", false));
        // Doc B: best chunk score 0.8
        ranked.push(VectorHit {
            chunk_id: 2,
            score: 0.8,
        });
        metas.insert(2, meta("B", false));
        let out = dedup_per_doc(ranked, &metas, 2, 1);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].chunk_id, 2, "B should rank first");
        assert_eq!(out[1].chunk_id, 1, "A should rank second");
    }

    #[test]
    fn rerank_head_count_bounds_cpu_work() {
        assert_eq!(rerank_head_count(5, 86), 24);
        assert_eq!(rerank_head_count(1, 50), 12);
        assert_eq!(rerank_head_count(8, 10), 10);
        assert_eq!(rerank_head_count(50, 200), RERANK_CANDIDATE_LIMIT);
    }

    // ----- shared in-memory corpus helpers -----

    /// Build an in-memory test corpus, return the open Connection.
    fn make_test_db() -> Result<(tempfile::TempDir, std::path::PathBuf)> {
        // We can't easily reuse `db_path()` here without setting env vars
        // for the data dir. Instead we set ATO_MCP_DATA_DIR so init_db
        // and the test target the same file.
        let dir = tempdir()?;
        let db_dir = dir.path().join("live");
        fs::create_dir_all(&db_dir)?;
        let db = db_dir.join("ato.db");
        let conn = open_write_at(&db)?;
        init_db(&conn)?;
        Ok((dir, db))
    }

    fn insert_doc(conn: &Connection, doc_id: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO documents(doc_id, type, title, downloaded_at, content_hash, pack_sha8, html) VALUES (?, 'Public_ruling', ?, ?, ?, '00000000', ?)",
            params![
                doc_id,
                format!("{doc_id} title"),
                Utc::now().to_rfc3339(),
                "deadbeef",
                compress_text("<div></div>")?,
            ],
        )?;
        Ok(())
    }

    /// Test helper: insert a document row with explicit currency fields.
    fn insert_doc_full(
        conn: &Connection,
        doc_id: &str,
        date: Option<&str>,
        withdrawn_date: Option<&str>,
        superseded_by: Option<&str>,
        replaces: Option<&str>,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO documents(doc_id, type, title, date, downloaded_at, \
                content_hash, pack_sha8, html, withdrawn_date, superseded_by, replaces) \
             VALUES (?, 'Public_ruling', ?, ?, ?, ?, '00000000', ?, ?, ?, ?)",
            params![
                doc_id,
                format!("{doc_id} title"),
                date,
                Utc::now().to_rfc3339(),
                "deadbeef",
                compress_text("<div></div>")?,
                withdrawn_date,
                superseded_by,
                replaces,
            ],
        )?;
        Ok(())
    }

    fn insert_chunk(
        conn: &Connection,
        chunk_id: i64,
        doc_id: &str,
        ord: i64,
        text: &str,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO chunks(chunk_id, doc_id, ord, anchor, text) VALUES (?, ?, ?, NULL, ?)",
            params![chunk_id, doc_id, ord, compress_text(text)?],
        )?;
        Ok(())
    }

    #[test]
    fn direct_doc_id_parser_accepts_only_deterministic_inputs() {
        assert_eq!(
            direct_doc_id_from_query(
                "https://www.ato.gov.au/law/view/document?docid=PAC%2F19970038%2F203-55"
            ),
            Some("PAC/19970038/203-55".to_string())
        );
        assert_eq!(
            direct_doc_id_from_query("PAC/19970038/203-55"),
            Some("PAC/19970038/203-55".to_string())
        );
        assert_eq!(
            direct_doc_id_from_query("see anchor PAC/19010002/Pt8 in the page"),
            None
        );
        assert_eq!(direct_doc_id_from_query("not/a<script>"), None);
    }

    #[test]
    fn html_elements_and_assets_are_queryable() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        let html = r#"<div id="LawContent"><h1 id="top">Example</h1><p>See <a data-doc-id="PAC/19970038/203-55">203-55</a>.</p><span data-asset-ref="ato-image://DOC_HTML/0">[image: Diagram]</span></div>"#;
        conn.execute(
            "INSERT INTO documents(doc_id, type, title, downloaded_at, content_hash, pack_sha8, html) VALUES (?, 'Public_ruling', ?, ?, ?, '00000000', ?)",
            params![
                "DOC_HTML",
                "HTML doc",
                Utc::now().to_rfc3339(),
                "htmlhash",
                compress_text(html)?,
            ],
        )?;
        let asset_rel = "assets/aa/test.gif";
        let asset_path = dir.path().join("live").join(asset_rel);
        fs::create_dir_all(asset_path.parent().expect("asset parent"))?;
        fs::write(&asset_path, b"gif")?;
        conn.execute(
            "INSERT INTO document_assets(asset_ref, doc_id, source_path, relative_path, media_type, alt, title, sha256, bytes) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                "ato-image://DOC_HTML/0",
                "DOC_HTML",
                "assets/test.gif",
                asset_rel,
                "image/gif",
                Option::<String>::None,
                "Diagram",
                format!("{:x}", Sha256::digest(b"gif")),
                3i64,
            ],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let asset = get_asset("ato-image://DOC_HTML/0")?;
            let parsed: JsonValue = serde_json::from_str(&asset)?;
            assert_eq!(parsed["title"], "Diagram");
            assert_eq!(parsed["path"], asset_path.display().to_string());
            Ok(())
        })?;
        Ok(())
    }

    fn insert_definition(
        conn: &Connection,
        definition_id: &str,
        term: &str,
        doc_id: &str,
        body: &str,
    ) -> Result<()> {
        insert_definition_with_source(conn, definition_id, term, doc_id, body, LEGISLATION_TYPE)
    }

    fn insert_definition_with_source(
        conn: &Connection,
        definition_id: &str,
        term: &str,
        doc_id: &str,
        body: &str,
        source_type: &str,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO definitions(definition_id, term, norm_term, doc_id, source_title, \
             source_type, scope, anchor, ord, body) \
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL, 0, ?)",
            params![
                definition_id,
                term,
                normalize_definition_term(term),
                doc_id,
                format!("{doc_id} title"),
                source_type,
                format!("{doc_id} title"),
                body,
            ],
        )?;
        Ok(())
    }

    fn with_data_dir<R>(dir: &std::path::Path, f: impl FnOnce() -> R) -> R {
        let prev = std::env::var("ATO_MCP_DATA_DIR").ok();
        std::env::set_var("ATO_MCP_DATA_DIR", dir);
        let result = f();
        if let Some(p) = prev {
            std::env::set_var("ATO_MCP_DATA_DIR", p);
        } else {
            std::env::remove_var("ATO_MCP_DATA_DIR");
        }
        result
    }

    #[test]
    fn serve_update_check_is_opt_in() {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let offline_prev = std::env::var("ATO_MCP_OFFLINE").ok();
        let auto_prev = std::env::var("ATO_MCP_AUTO_UPDATE").ok();
        std::env::remove_var("ATO_MCP_OFFLINE");
        std::env::remove_var("ATO_MCP_AUTO_UPDATE");

        assert!(!serve_should_check_update(false));
        assert!(serve_should_check_update(true));

        std::env::set_var("ATO_MCP_AUTO_UPDATE", "1");
        assert!(serve_should_check_update(false));

        std::env::set_var("ATO_MCP_OFFLINE", "1");
        assert!(!serve_should_check_update(true));

        if let Some(value) = offline_prev {
            std::env::set_var("ATO_MCP_OFFLINE", value);
        } else {
            std::env::remove_var("ATO_MCP_OFFLINE");
        }
        if let Some(value) = auto_prev {
            std::env::set_var("ATO_MCP_AUTO_UPDATE", value);
        } else {
            std::env::remove_var("ATO_MCP_AUTO_UPDATE");
        }
    }

    // ----- W1.5 manifest version guards -----

    #[test]
    fn manifest_compat_accepts_current_schema() {
        let m = sample_manifest(MAX_SUPPORTED_MANIFEST_VERSION as i64, "");
        assert!(enforce_manifest_compatibility(&m).is_ok());
    }

    #[test]
    fn manifest_compat_rejects_newer_schema() {
        let m = sample_manifest((MAX_SUPPORTED_MANIFEST_VERSION + 1) as i64, "");
        let err = enforce_manifest_compatibility(&m).unwrap_err();
        assert!(
            err.to_string().contains("upgrade the ato-mcp binary"),
            "expected upgrade-binary message, got: {err}"
        );
    }

    #[test]
    fn manifest_compat_rejects_higher_min_client_version() {
        let m = sample_manifest(MAX_SUPPORTED_MANIFEST_VERSION as i64, "999.0.0");
        let err = enforce_manifest_compatibility(&m).unwrap_err();
        assert!(
            err.to_string().contains("999"),
            "expected min_client_version error, got: {err}"
        );
    }

    #[test]
    fn manifest_compat_accepts_min_client_version_at_or_below_current() {
        // Any version that's <= the current binary's version should pass.
        let current = env!("CARGO_PKG_VERSION");
        let m = sample_manifest(MAX_SUPPORTED_MANIFEST_VERSION as i64, current);
        assert!(enforce_manifest_compatibility(&m).is_ok());
        let m = sample_manifest(MAX_SUPPORTED_MANIFEST_VERSION as i64, "0.0.1");
        assert!(enforce_manifest_compatibility(&m).is_ok());
    }

    #[test]
    fn diff_manifests_marks_pack_slot_change_as_changed() {
        let old = Manifest {
            documents: vec![DocRef {
                doc_id: "DOC".to_string(),
                content_hash: "same-content".to_string(),
                pack_sha8: "oldpack".to_string(),
                offset: 0,
                length: 10,
            }],
            ..sample_manifest(MAX_SUPPORTED_MANIFEST_VERSION as i64, "")
        };
        let new = Manifest {
            documents: vec![DocRef {
                doc_id: "DOC".to_string(),
                content_hash: "same-content".to_string(),
                pack_sha8: "newpack".to_string(),
                offset: 0,
                length: 11,
            }],
            ..sample_manifest(MAX_SUPPORTED_MANIFEST_VERSION as i64, "")
        };

        let (added, changed, removed) = diff_manifests(Some(&old), &new);
        assert!(added.is_empty());
        assert!(removed.is_empty());
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].doc_id, "DOC");
    }

    #[test]
    fn cmp_dotted_version_basics() {
        use std::cmp::Ordering;
        assert_eq!(cmp_dotted_version("1.2.3", "1.2.3"), Ordering::Equal);
        assert_eq!(cmp_dotted_version("1.2", "1.2.0"), Ordering::Equal);
        assert_eq!(cmp_dotted_version("1.2.4", "1.2.3"), Ordering::Greater);
        assert_eq!(cmp_dotted_version("1.3.0", "1.2.99"), Ordering::Greater);
        assert_eq!(cmp_dotted_version("0.4.0", "0.4.0-rc1"), Ordering::Equal);
    }

    #[test]
    fn open_read_rejects_unsupported_schema_version() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        // Force a bogus schema version via raw SQL.
        set_meta(&conn, "schema_version", "99")?;
        drop(conn);
        with_data_dir(dir.path(), || -> Result<()> {
            let err = open_read().unwrap_err();
            assert!(
                err.to_string().contains("not supported by this binary"),
                "expected schema mismatch error, got: {err}"
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn open_read_rejects_missing_schema_version_row() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        // init_db wrote the row; now delete it to simulate a corrupt /
        // partial install.
        conn.execute("DELETE FROM meta WHERE key = 'schema_version'", [])?;
        drop(conn);
        with_data_dir(dir.path(), || -> Result<()> {
            let err = open_read().unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("corrupt or incomplete") && msg.contains("ato-mcp init"),
                "expected corrupt/incomplete error with init hint, got: {msg}"
            );
            Ok(())
        })?;
        Ok(())
    }

    // ----- Cleanup: highlight_snippet fallback paths -----

    #[test]
    fn snippet_falls_back_when_query_has_no_usable_tokens() {
        // Query reduces to only single-character / punctuation tokens,
        // which `snippet_query_terms` filters out. We expect the opening
        // fragment of the chunk text without any heading prefix.
        let text = "The quick brown fox jumps over the lazy dog repeatedly throughout the day.";
        let snippet = highlight_snippet(text, "a", SNIPPET_CHARS);
        assert!(
            snippet.contains("The quick brown fox"),
            "fallback should preserve the opening fragment, got: {snippet}"
        );
    }

    #[test]
    fn snippet_falls_back_when_chunk_text_is_empty() {
        let snippet = highlight_snippet("", "anything goes here", SNIPPET_CHARS);
        // Empty cleaned text -> returns the empty string (no heading prefix
        // any more).
        assert_eq!(
            snippet, "",
            "empty chunk text should produce an empty snippet, got: {snippet:?}"
        );
    }

    #[test]
    fn snippet_emits_window_when_no_query_tokens_match() {
        // The chunk only contains tokens that don't appear in the query.
        // BM25 still picks *some* window so the snippet emits a sensible
        // body window. Heading text now lives inside chunk.text so there
        // is no separate prefix.
        let text = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor";
        let snippet = highlight_snippet(
            text,
            "completely unrelated query terms",
            SNIPPET_CHARS,
        );
        assert!(!snippet.is_empty(), "snippet should not be empty");
        assert!(
            !snippet.contains(" — "),
            "snippet should not carry a heading prefix, got: {snippet}"
        );
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn encode_test_pack_record(record: &JsonValue) -> Result<Vec<u8>> {
        let payload = serde_json::to_vec(record)?;
        let compressed = zstd::stream::encode_all(Cursor::new(payload), 3)?;
        let mut out = Vec::with_capacity(4 + compressed.len());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&compressed);
        Ok(out)
    }

    fn write_test_tar_zst(path: &Path, files: &[(&str, &[u8])]) -> Result<()> {
        let file = File::create(path)?;
        let encoder = zstd::stream::write::Encoder::new(file, 3)?;
        let mut archive = tar::Builder::new(encoder);
        for (name, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, *name, Cursor::new(*bytes))?;
        }
        let encoder = archive.into_inner()?;
        encoder.finish()?;
        Ok(())
    }

    fn sample_manifest(schema_version: i64, min_client_version: &str) -> Manifest {
        Manifest {
            schema_version,
            index_version: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            min_client_version: min_client_version.to_string(),
            model: ModelInfo {
                id: "test-model".to_string(),
                sha256: "0".to_string(),
                size: 0,
                url: "https://example.com".to_string(),
                tokenizer_sha256: None,
            },
            reranker: None,
            documents: Vec::new(),
            packs: Vec::new(),
        }
    }

    // ===== Wave 2 ===========================================================

    // ----- Schema v8 -----

    #[test]
    fn schema_init_writes_v8_metadata() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (_dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        let value =
            get_meta(&conn, "schema_version")?.expect("init_db should have written schema_version");
        assert_eq!(value, SUPPORTED_SCHEMA_VERSION.to_string());
        assert_eq!(SUPPORTED_SCHEMA_VERSION, 8);
        Ok(())
    }

    #[test]
    fn open_read_rejects_unsupported_schema_corpus() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        // Stamp an unsupported schema version. The user-facing error must
        // refuse the corpus cleanly instead of trying to mutate it in place.
        set_meta(&conn, "schema_version", "5")?;
        drop(conn);
        with_data_dir(dir.path(), || -> Result<()> {
            let err = open_read().unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("not supported"),
                "expected schema-mismatch error, got: {msg}"
            );
            assert!(
                msg.contains("reinstall") || msg.contains("upgrade"),
                "expected rebuild/reinstall hint, got: {msg}"
            );
            Ok(())
        })?;
        Ok(())
    }

    // ----- W2.4 build_doc_filter current_only -----

    #[test]
    fn build_doc_filter_includes_withdrawn_clause_by_default() {
        let f = build_doc_filter("d", None, None, None, None, true, true);
        assert!(
            f.sql.contains("d.withdrawn_date IS NULL"),
            "current_only=true must add withdrawn_date IS NULL clause; sql={}",
            f.sql
        );
    }

    #[test]
    fn build_doc_filter_omits_withdrawn_clause_when_disabled() {
        let f = build_doc_filter("d", None, None, None, None, true, false);
        assert!(
            !f.sql.contains("withdrawn_date"),
            "current_only=false must not mention withdrawn_date; sql={}",
            f.sql
        );
    }

    #[test]
    fn search_next_call_preserves_current_only_false() {
        let opts = SearchOptions {
            k: 8,
            types: None,
            date_from: None,
            date_to: None,
            doc_scope: None,
            mode: SearchMode::Hybrid,
            sort_by: SortBy::Relevance,
            include_old: false,
            current_only: false,
            max_per_doc: DEFAULT_MAX_PER_DOC,
            include_snippet: true,
            similar_to_chunk_id: None,
        };
        let call = search_next_call("depreciation", 16, &opts);
        assert!(
            call.contains("current_only=false"),
            "continuation must preserve withdrawn-doc inclusion; got: {call}"
        );
    }

    // ----- W2.4 Hit JSON serialisation skips unset currency fields -----

    #[test]
    fn hit_json_skips_unset_currency_fields() -> Result<()> {
        let hit = Hit {
            doc_id: "DOC".to_string(),
            title: "T".to_string(),
            doc_type: "Public_ruling".to_string(),
            date: None,
            anchor: None,
            snippet: Some("snip".to_string()),
            canonical_url: "https://x".to_string(),
            chunk_id: None,
            next_call: None,
            withdrawn_date: None,
            superseded_by: None,
            replaces: None,
            has_in_doc_links: None,
            has_related_docs: None,
            has_history: None,
        };
        let json_str = serde_json::to_string(&hit)?;
        assert!(
            !json_str.contains("withdrawn_date"),
            "withdrawn_date should be omitted when None; json={json_str}"
        );
        assert!(!json_str.contains("superseded_by"));
        assert!(!json_str.contains("replaces"));
        assert!(!json_str.contains("has_in_doc_links"));
        assert!(!json_str.contains("has_related_docs"));
        assert!(!json_str.contains("has_history"));
        Ok(())
    }

    #[test]
    fn hit_json_emits_currency_fields_when_set() -> Result<()> {
        let hit = Hit {
            doc_id: "DOC".to_string(),
            title: "T".to_string(),
            doc_type: "Public_ruling".to_string(),
            date: Some("2022-07-01".to_string()),
            anchor: None,
            snippet: Some("snip".to_string()),
            canonical_url: "https://x".to_string(),
            chunk_id: None,
            next_call: None,
            withdrawn_date: Some("2025-10-31".to_string()),
            superseded_by: Some("TR 2025/1".to_string()),
            replaces: Some("TR 2021/3".to_string()),
            has_in_doc_links: None,
            has_related_docs: None,
            has_history: None,
        };
        let parsed: serde_json::Value = serde_json::from_str(&serde_json::to_string(&hit)?)?;
        assert_eq!(parsed["withdrawn_date"], json!("2025-10-31"));
        assert_eq!(parsed["superseded_by"], json!("TR 2025/1"));
        assert_eq!(parsed["replaces"], json!("TR 2021/3"));
        Ok(())
    }

    // ----- W2.4 integration: search filters out withdrawn docs by default -----

    #[test]
    fn search_titles_excludes_withdrawn_by_default() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        // Two docs sharing a query keyword in their titles. One is withdrawn.
        insert_doc_full(&conn, "DOC_CURRENT", Some("2024-01-01"), None, None, None)?;
        insert_doc_full(
            &conn,
            "DOC_WITHDRAWN",
            Some("2020-01-01"),
            Some("2023-06-15"),
            Some("TR 2024/1"),
            None,
        )?;
        // title_fts must be populated for the bm25 path.
        conn.execute(
            "INSERT INTO title_fts(doc_id, title, headings) VALUES (?, ?, '')",
            params!["DOC_CURRENT", "depreciation effective life rulings"],
        )?;
        conn.execute(
            "INSERT INTO title_fts(doc_id, title, headings) VALUES (?, ?, '')",
            params!["DOC_WITHDRAWN", "depreciation effective life rulings"],
        )?;
        // Update documents.title to match what title_fts holds (search_titles
        // joins documents to fetch the displayed title back).
        conn.execute(
            "UPDATE documents SET title = ? WHERE doc_id = ?",
            params!["depreciation effective life rulings", "DOC_CURRENT"],
        )?;
        conn.execute(
            "UPDATE documents SET title = ? WHERE doc_id = ?",
            params!["depreciation effective life rulings", "DOC_WITHDRAWN"],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            // Default: current_only=true → withdrawn doc filtered out.
            let json_str = search_titles(
                "depreciation",
                10,
                None,
                true, // include_old (date filter doesn't apply since title query)
                true, // current_only
            )?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let doc_ids: Vec<&str> = parsed["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["doc_id"].as_str().unwrap())
                .collect();
            assert!(
                doc_ids.contains(&"DOC_CURRENT"),
                "current doc should appear; got: {doc_ids:?}"
            );
            assert!(
                !doc_ids.contains(&"DOC_WITHDRAWN"),
                "withdrawn doc should be filtered out by default; got: {doc_ids:?}"
            );

            // current_only=false → withdrawn doc returned with marker visible
            // in JSON via the dedicated field.
            let json_str = search_titles(
                "depreciation",
                10,
                None,
                true,
                false, // current_only off
            )?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let withdrawn_hit = parsed["hits"]
                .as_array()
                .unwrap()
                .iter()
                .find(|h| h["doc_id"].as_str() == Some("DOC_WITHDRAWN"))
                .expect("withdrawn doc should appear when current_only=false");
            assert_eq!(withdrawn_hit["withdrawn_date"], json!("2023-06-15"));
            assert_eq!(withdrawn_hit["superseded_by"], json!("TR 2024/1"));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn search_titles_prefers_direct_doc_id_hits() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        insert_doc_full(
            &conn,
            "PAC/19970038/203-50",
            Some("1997-01-01"),
            None,
            None,
            None,
        )?;
        conn.execute(
            "UPDATE documents SET type = 'Legislation_and_supporting_material', title = ? WHERE doc_id = ?",
            params!["Income Tax Assessment Act 1997 s 203-50", "PAC/19970038/203-50"],
        )?;
        conn.execute(
            "INSERT INTO title_fts(doc_id, title, headings) VALUES (?, ?, '')",
            params![
                "PAC/19970038/203-50",
                "Income Tax Assessment Act 1997 s 203-50"
            ],
        )?;
        insert_doc_full(
            &conn,
            "PAC/19970038/8-1",
            Some("1997-01-01"),
            None,
            None,
            None,
        )?;
        conn.execute(
            "UPDATE documents SET type = 'Legislation_and_supporting_material', title = ? WHERE doc_id = ?",
            params!["Income Tax Assessment Act 1997 s 8-1", "PAC/19970038/8-1"],
        )?;
        conn.execute(
            "INSERT INTO title_fts(doc_id, title, headings) VALUES (?, ?, '')",
            params!["PAC/19970038/8-1", "Income Tax Assessment Act 1997 s 8-1"],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = search_titles(
                "PAC/19970038/203-50",
                5,
                None,
                false,
                true,
            )?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            assert_eq!(parsed["hits"][0]["doc_id"], json!("PAC/19970038/203-50"));
            let json_str = search_titles(
                "Income Tax Assessment Act 1997 s 8-1",
                5,
                None,
                false,
                true,
            )?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            assert_eq!(parsed["hits"][0]["doc_id"], json!("PAC/19970038/8-1"));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn get_definition_returns_matching_entry_only() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        insert_doc_full(
            &conn,
            "PAC/19970038/995-1",
            Some("1997-01-01"),
            None,
            None,
            None,
        )?;
        insert_definition(
            &conn,
            "def-corporate-gross-up",
            "corporate tax gross-up rate",
            "PAC/19970038/995-1",
            ", of an entity for an income year, means the amount worked out using the formula.",
        )?;
        insert_definition(
            &conn,
            "def-other",
            "corporate tax rate",
            "PAC/19970038/995-1",
            "means the rate of tax.",
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = get_definition(
                "corporate tax gross-up rate",
                GetDefinitionOptions {
                    context_doc_id: Some("PAC/19970038/203-50"),
                    max_defs: 5,
                },
            )?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            assert_eq!(parsed["statutory_definition_found"], json!(true));
            assert_eq!(parsed["definitions"].as_array().unwrap().len(), 1);
            assert_eq!(
                parsed["definitions"][0]["definition_id"],
                json!("def-corporate-gross-up")
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn get_definition_ignores_non_legislation_sources() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        insert_doc_full(
            &conn,
            "PAC/19860039/136",
            Some("1986-01-01"),
            None,
            None,
            None,
        )?;
        insert_doc_full(&conn, "EV/123456", Some("2024-01-01"), None, None, None)?;
        insert_doc_full(
            &conn,
            "AID/AID20021000",
            Some("2002-01-01"),
            None,
            None,
            None,
        )?;
        insert_definition(
            &conn,
            "def-car-legislation",
            "car",
            "PAC/19860039/136",
            "has the meaning given by section 995-1.",
        )?;
        insert_definition_with_source(
            &conn,
            "def-car-epa",
            "car",
            "EV/123456",
            "A private advice glossary entry.",
            "Edited_private_advice",
        )?;
        insert_definition_with_source(
            &conn,
            "def-car-aid",
            "car",
            "AID/AID20021000",
            "An interpretative decision glossary entry.",
            "ATO_interpretative_decisions",
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = get_definition(
                "car",
                GetDefinitionOptions {
                    context_doc_id: Some("PAC/19860039/136"),
                    max_defs: 10,
                },
            )?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let definitions = parsed["definitions"].as_array().unwrap();
            assert_eq!(definitions.len(), 1);
            assert_eq!(
                definitions[0]["definition_id"],
                json!("def-car-legislation")
            );
            assert_eq!(definitions[0]["source"]["type"], json!(LEGISLATION_TYPE));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn get_definition_falls_back_to_configured_ordinary_dictionary() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let dictionary_path = dir.path().join("ordinary.tsv");
        fs::write(
            &dictionary_path,
            "car\tcar\tnoun\ta road vehicle powered by an engine\n",
        )?;
        let prev = std::env::var_os(ORDINARY_DICTIONARY_PATH_ENV);
        std::env::set_var(ORDINARY_DICTIONARY_PATH_ENV, &dictionary_path);

        let result = with_data_dir(dir.path(), || -> Result<String> {
            get_definition(
                "car",
                GetDefinitionOptions {
                    context_doc_id: None,
                    max_defs: 5,
                },
            )
        });

        if let Some(value) = prev {
            std::env::set_var(ORDINARY_DICTIONARY_PATH_ENV, value);
        } else {
            std::env::remove_var(ORDINARY_DICTIONARY_PATH_ENV);
        }
        let parsed: serde_json::Value = serde_json::from_str(&result?)?;
        assert_eq!(parsed["statutory_definition_found"], json!(false));
        assert_eq!(parsed["ordinary_meaning"]["kind"], json!("ordinary"));
        assert_eq!(
            parsed["ordinary_meaning"]["definitions"][0]["definition"],
            json!("a road vehicle powered by an engine")
        );
        Ok(())
    }

    #[test]
    fn parse_oewn_data_file_builds_ordinary_rows() {
        let mut rows = Vec::new();
        parse_oewn_data_file(
            "00001740 03 n 02 car 0 motor_vehicle 0 001 @ 00001930 n 0000 | a road vehicle powered by an engine\n",
            "noun",
            &mut rows,
        );
        assert!(rows.contains(&"car\tcar\tnoun\ta road vehicle powered by an engine".to_string()));
        assert!(rows.contains(
            &"motor vehicle\tmotor vehicle\tnoun\ta road vehicle powered by an engine".to_string()
        ));
    }

    // ----- C1 regression: currency fields survive insert_record -------------
    //
    // Earlier currency-filter tests used the
    // manual `insert_doc_full` seeder, which writes `withdrawn_date` /
    // `superseded_by` / `replaces` directly. The production code path is
    // `apply_update_locked → insert_docs_from_packs → read_record_from_pack_bytes
    // → insert_record`, and the bug they didn't catch was: PackRecord didn't
    // declare those fields, serde silently dropped them, and the INSERT SQL
    // didn't bind them either. End result: every ingested row had NULL
    // currency columns and `current_only=true` never excluded anything.
    //
    // This test goes through the production `insert_record` path (NOT the
    // manual seeder) so a regression in PackRecord struct shape OR the INSERT
    // SQL OR the currency filter would all fire it.

    #[test]
    fn currency_fields_round_trip_through_insert_record() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;

        let withdrawn_record = PackRecord {
            doc_id: "TR_2018_WITHDRAWN".to_string(),
            doc_type: "Public_ruling".to_string(),
            title: "depreciation effective life rulings".to_string(),
            date: Some("2018-01-01".to_string()),
            downloaded_at: Utc::now().to_rfc3339(),
            content_hash: "deadbeef".to_string(),
            html: "<div><p>depreciation effective life schedule for plant.</p></div>".to_string(),
            withdrawn_date: Some("2024-06-15".to_string()),
            superseded_by: Some("TR 2024/1".to_string()),
            replaces: None,
            has_in_doc_links: 0,
            has_related_docs: 0,
            has_history: 0,
            anchors: Vec::new(),
            definitions: Vec::new(),
            assets: Vec::new(),
            chunks: vec![PackChunk {
                ord: 0,
                anchor: None,
                text: "depreciation effective life schedule for plant.".to_string(),
                embedding_b64: None,
            }],
        };
        let asset_bytes = b"diagram";
        let asset_sha = format!("{:x}", Sha256::digest(asset_bytes));
        let asset_b64 = base64::engine::general_purpose::STANDARD.encode(asset_bytes);
        let current_record = PackRecord {
            doc_id: "TR_2024_CURRENT".to_string(),
            doc_type: "Public_ruling".to_string(),
            title: "depreciation effective life rulings 2024".to_string(),
            date: Some("2024-01-01".to_string()),
            downloaded_at: Utc::now().to_rfc3339(),
            content_hash: "feedface".to_string(),
            html: "<div><p>depreciation effective life schedule for plant.</p></div>".to_string(),
            withdrawn_date: None,
            superseded_by: None,
            replaces: Some("TR 2018/X".to_string()),
            has_in_doc_links: 0,
            has_related_docs: 0,
            has_history: 0,
            anchors: Vec::new(),
            definitions: Vec::new(),
            assets: vec![PackAsset {
                asset_ref: "ato-image://TR_2024_CURRENT/0".to_string(),
                source_path: "assets/source.gif".to_string(),
                relative_path: "assets/aa/current.gif".to_string(),
                media_type: Some("image/gif".to_string()),
                alt: None,
                title: Some("Current diagram".to_string()),
                sha256: asset_sha.clone(),
                size: asset_bytes.len() as i64,
                data_b64: asset_b64,
            }],
            chunks: vec![PackChunk {
                ord: 0,
                anchor: None,
                text: "depreciation effective life schedule for plant.".to_string(),
                embedding_b64: None,
            }],
        };
        let withdrawn_ref = DocRef {
            doc_id: "TR_2018_WITHDRAWN".to_string(),
            content_hash: "deadbeef".to_string(),
            pack_sha8: "00000000".to_string(),
            offset: 0,
            length: 0,
        };
        let current_ref = DocRef {
            doc_id: "TR_2024_CURRENT".to_string(),
            content_hash: "feedface".to_string(),
            pack_sha8: "00000000".to_string(),
            offset: 0,
            length: 0,
        };

        // Production insert path — DO NOT swap for `insert_doc_full`.
        insert_record(
            &conn,
            &withdrawn_record,
            &withdrawn_ref,
            &dir.path().join("live"),
        )?;
        insert_record(
            &conn,
            &current_record,
            &current_ref,
            &dir.path().join("live"),
        )?;

        // Sanity: the SELECT returns what insert_record wrote (catches the
        // INSERT-SQL drop-column bug directly, even before search runs).
        let (wd, sb, rep): (Option<String>, Option<String>, Option<String>) = conn.query_row(
            "SELECT withdrawn_date, superseded_by, replaces FROM documents \
                 WHERE doc_id = 'TR_2018_WITHDRAWN'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(wd.as_deref(), Some("2024-06-15"));
        assert_eq!(sb.as_deref(), Some("TR 2024/1"));
        assert_eq!(rep, None);
        let (wd2, sb2, rep2): (Option<String>, Option<String>, Option<String>) = conn.query_row(
            "SELECT withdrawn_date, superseded_by, replaces FROM documents \
                 WHERE doc_id = 'TR_2024_CURRENT'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(wd2, None);
        assert_eq!(sb2, None);
        assert_eq!(rep2.as_deref(), Some("TR 2018/X"));
        assert!(dir.path().join("live/assets/aa/current.gif").exists());
        let stored_asset: String = conn.query_row(
            "SELECT sha256 FROM document_assets WHERE asset_ref = ?",
            ["ato-image://TR_2024_CURRENT/0"],
            |row| row.get(0),
        )?;
        assert_eq!(stored_asset, asset_sha);
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            // current_only=true (default) → withdrawn doc must be excluded.
            // Use Keyword mode so the test doesn't need the embedding model.
            let json_str = search(
                "depreciation",
                SearchOptions {
                    k: 10,
                    types: None,
                    date_from: None,
                    date_to: None,
                    doc_scope: None,
                    mode: SearchMode::Keyword,
                    sort_by: SortBy::Relevance,
                    include_old: true,
                    current_only: true,
                    max_per_doc: DEFAULT_MAX_PER_DOC,
                    include_snippet: true,
                    similar_to_chunk_id: None,
                },
                None,
            )?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let doc_ids: Vec<&str> = parsed["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["doc_id"].as_str().unwrap())
                .collect();
            assert!(
                doc_ids.contains(&"TR_2024_CURRENT"),
                "current doc should appear with current_only=true; got: {doc_ids:?}"
            );
            assert!(
                !doc_ids.contains(&"TR_2018_WITHDRAWN"),
                "withdrawn doc must be excluded by current_only=true; got: {doc_ids:?} \
                 — this is the C1 canary: PackRecord lost the currency fields"
            );

            // current_only=false → both docs returned, withdrawn one carries
            // its currency markers through the JSON shape.
            let json_str = search(
                "depreciation",
                SearchOptions {
                    k: 10,
                    types: None,
                    date_from: None,
                    date_to: None,
                    doc_scope: None,
                    mode: SearchMode::Keyword,
                    sort_by: SortBy::Relevance,
                    include_old: true,
                    current_only: false,
                    max_per_doc: DEFAULT_MAX_PER_DOC,
                    include_snippet: true,
                    similar_to_chunk_id: None,
                },
                None,
            )?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let withdrawn_hit = parsed["hits"]
                .as_array()
                .unwrap()
                .iter()
                .find(|h| h["doc_id"].as_str() == Some("TR_2018_WITHDRAWN"))
                .expect("withdrawn doc should appear when current_only=false");
            assert_eq!(
                withdrawn_hit["withdrawn_date"],
                json!("2024-06-15"),
                "withdrawn_date must round-trip through insert_record"
            );
            assert_eq!(withdrawn_hit["superseded_by"], json!("TR 2024/1"));
            let current_hit = parsed["hits"]
                .as_array()
                .unwrap()
                .iter()
                .find(|h| h["doc_id"].as_str() == Some("TR_2024_CURRENT"))
                .expect("current doc should appear in current_only=false too");
            assert_eq!(current_hit["replaces"], json!("TR 2018/X"));
            Ok(())
        })?;
        Ok(())
    }

    // ----- Wave 4 navigation flags + doc anchors -----

    /// Test helper: insert a document row with the navigation flags set.
    fn insert_doc_with_nav_flags(
        conn: &Connection,
        doc_id: &str,
        has_in_doc: i64,
        has_related: i64,
        has_history: i64,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO documents(doc_id, type, title, downloaded_at, content_hash, pack_sha8, html, has_in_doc_links, has_related_docs, has_history) \
             VALUES (?, 'Public_ruling', ?, ?, ?, '00000000', ?, ?, ?, ?)",
            params![
                doc_id,
                format!("{doc_id} title"),
                Utc::now().to_rfc3339(),
                "deadbeef",
                compress_text("<div></div>")?,
                has_in_doc,
                has_related,
                has_history,
            ],
        )?;
        Ok(())
    }

    #[test]
    fn test_search_hit_carries_navigation_flags() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        // Doc with has_in_doc_links=1; one chunk so search returns it.
        insert_doc_with_nav_flags(&conn, "DOC_NAV", 1, 0, 0)?;
        let text = "Research and development tax incentive paragraph navigation flag canary text.";
        insert_chunk(&conn, 1, "DOC_NAV", 0, text)?;
        conn.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (?, ?)",
            params![1_i64, text],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = search(
                "research development",
                SearchOptions {
                    k: 5,
                    types: None,
                    date_from: None,
                    date_to: None,
                    doc_scope: None,
                    mode: SearchMode::Keyword,
                    sort_by: SortBy::Relevance,
                    include_old: false,
                    current_only: true,
                    max_per_doc: DEFAULT_MAX_PER_DOC,
                    include_snippet: true,
                    similar_to_chunk_id: None,
                },
                None,
            )?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let hit = parsed["hits"]
                .as_array()
                .and_then(|a| a.first())
                .expect("expected at least one hit");
            assert_eq!(hit["doc_id"], json!("DOC_NAV"));
            assert_eq!(hit["has_in_doc_links"], json!(true));
            // Unset flags must stay absent on the wire (skip_serializing_if).
            assert!(hit.get("has_related_docs").is_none());
            assert!(hit.get("has_history").is_none());
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn test_get_doc_anchors_returns_three_kinds() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        insert_doc_with_nav_flags(&conn, "DOC_ANCHORS", 1, 1, 1)?;
        // One chunk to satisfy the in_doc target_chunk_id reference.
        insert_chunk(&conn, 100, "DOC_ANCHORS", 0, "body")?;
        // Sister and history docs — referenced as targets but only need
        // documents rows for FK integrity (doc_anchors.target_doc_id is
        // not a FK, so unreferenced is fine — but we'll insert anyway).
        conn.execute(
            "INSERT INTO doc_anchors(doc_id, ord, kind, label, target_chunk_id, target_doc_id, target_pit) VALUES (?, ?, 'in_doc', 'Section A', ?, NULL, NULL)",
            params!["DOC_ANCHORS", 0_i64, 100_i64],
        )?;
        conn.execute(
            "INSERT INTO doc_anchors(doc_id, ord, kind, label, target_chunk_id, target_doc_id, target_pit) VALUES (?, ?, 'sister', 'Errata', NULL, ?, NULL)",
            params!["DOC_ANCHORS", 1_i64, "DOC_SISTER"],
        )?;
        // History anchor target_doc_id is the BASE doc_id; the timestamp
        // travels alongside in target_pit.
        conn.execute(
            "INSERT INTO doc_anchors(doc_id, ord, kind, label, target_chunk_id, target_doc_id, target_pit) VALUES (?, ?, 'history', 'Earlier version', NULL, ?, ?)",
            params!["DOC_ANCHORS", 2_i64, "DOC_HISTORY", "20200101000000"],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = get_doc_anchors("DOC_ANCHORS")?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            assert_eq!(parsed["doc_id"], json!("DOC_ANCHORS"));
            let in_doc = parsed["in_doc"].as_array().unwrap();
            let related = parsed["related_docs"].as_array().unwrap();
            let history = parsed["historical_versions"].as_array().unwrap();
            assert_eq!(in_doc.len(), 1, "expected one in_doc anchor");
            assert_eq!(in_doc[0]["chunk_id"], json!(100));
            assert_eq!(in_doc[0]["label"], json!("Section A"));
            assert_eq!(related.len(), 1);
            assert_eq!(related[0]["doc_id"], json!("DOC_SISTER"));
            assert_eq!(related[0]["label"], json!("Errata"));
            assert_eq!(history.len(), 1);
            // doc_id is the BASE doc_id; pit carries the timestamp; date is
            // derived from pit.
            assert_eq!(history[0]["doc_id"], json!("DOC_HISTORY"));
            assert_eq!(history[0]["pit"], json!("20200101000000"));
            assert_eq!(history[0]["label"], json!("Earlier version"));
            assert_eq!(history[0]["date"], json!("2020-01-01"));
            assert_eq!(
                history[0]["url"],
                json!("https://www.ato.gov.au/law/view/document?docid=DOC_HISTORY&PiT=20200101000000")
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn test_get_doc_anchors_pit_to_date() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        insert_doc_with_nav_flags(&conn, "DOC_PIT", 0, 0, 1)?;
        conn.execute(
            "INSERT INTO doc_anchors(doc_id, ord, kind, label, target_chunk_id, target_doc_id, target_pit) VALUES (?, ?, 'history', 'Original ruling', NULL, ?, ?)",
            params!["DOC_PIT", 0_i64, "TR_1996_X", "19960320000001"],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = get_doc_anchors("DOC_PIT")?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let history = parsed["historical_versions"].as_array().unwrap();
            assert_eq!(history.len(), 1);
            // Base doc_id is preserved; the timestamp is exposed separately
            // on the response so the agent can construct the external URL.
            assert_eq!(history[0]["doc_id"], json!("TR_1996_X"));
            assert_eq!(history[0]["pit"], json!("19960320000001"));
            assert_eq!(history[0]["date"], json!("1996-03-20"));
            assert_eq!(
                history[0]["url"],
                json!("https://www.ato.gov.au/law/view/document?docid=TR_1996_X&PiT=19960320000001")
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn test_pit_to_date_handles_short_or_non_numeric_input() {
        assert_eq!(pit_to_date("19960320000001"), Some("1996-03-20".to_string()));
        assert_eq!(pit_to_date("19960320"), Some("1996-03-20".to_string()));
        assert!(pit_to_date("1996032").is_none(), "shorter than 8 chars returns None");
        assert!(pit_to_date("abcd0320000000").is_none());
    }

    #[test]
    fn apply_update_locked_ingests_real_manifest_and_pack() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let release = tempdir()?;
        let release_dir = release.path();
        let packs_dir = release_dir.join("packs");
        fs::create_dir_all(&packs_dir)?;

        let model_bundle = release_dir.join("model-bundle.tar.zst");
        write_test_tar_zst(
            &model_bundle,
            &[
                ("model_quantized.onnx", b"dummy onnx bytes"),
                ("tokenizer.json", br#"{"version":"1.0","truncation":null}"#),
            ],
        )?;
        let model_bundle_bytes = fs::read(&model_bundle)?;

        let embedding_b64 =
            base64::engine::general_purpose::STANDARD.encode(vec![0u8; EMBEDDING_DIM]);
        let record = json!({
            "doc_id": "DOC_UPDATE_REAL",
            "type": "Public_ruling",
            "title": "Real manifest update path",
            "date": "2026-05-01",
            "downloaded_at": "2026-05-01T00:00:00Z",
            "content_hash": "hash-real-update",
            "html": "<div><p>Research and development tax incentive update path text.</p></div>",
            "assets": [],
            "withdrawn_date": "2026-05-02",
            "superseded_by": "TR 2026/2",
            "replaces": JsonValue::Null,
            "chunks": [{
                "ord": 0,
                "anchor": "ruling",
                "text": "Research and development tax incentive update path text.",
                "embedding_b64": embedding_b64
            }]
        });
        let pack_bytes = encode_test_pack_record(&record)?;
        let pack_path = packs_dir.join("pack-deadbeef.bin.zst");
        fs::write(&pack_path, &pack_bytes)?;

        let manifest = Manifest {
            schema_version: MAX_SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "test-real-update".to_string(),
            created_at: "2026-05-01T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: "embeddinggemma-test".to_string(),
                sha256: sha256_hex(&model_bundle_bytes),
                size: model_bundle_bytes.len() as u64,
                url: "model-bundle.tar.zst".to_string(),
                tokenizer_sha256: None,
            },
            reranker: None,
            documents: vec![DocRef {
                doc_id: "DOC_UPDATE_REAL".to_string(),
                content_hash: "hash-real-update".to_string(),
                pack_sha8: "deadbeef".to_string(),
                offset: 0,
                length: pack_bytes.len() as u64,
            }],
            packs: vec![PackInfo {
                sha8: "deadbeef".to_string(),
                sha256: sha256_hex(&pack_bytes),
                size: pack_bytes.len() as u64,
                url: "packs/pack-deadbeef.bin.zst".to_string(),
            }],
        };
        let manifest_path = release_dir.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

        with_data_dir(data.path(), || -> Result<()> {
            let stats = apply_update_locked(manifest_path.to_str().expect("utf-8 path"))?;
            assert_eq!(stats.added, 1);
            assert_eq!(stats.changed, 0);
            assert_eq!(stats.removed, 0);
            assert!(model_path()?.exists(), "model should be installed");
            assert!(tokenizer_path()?.exists(), "tokenizer should be installed");
            assert!(
                installed_manifest_path()?.exists(),
                "installed manifest should be written"
            );

            let conn = open_read()?;
            let row: (String, Option<String>, Option<String>) = conn.query_row(
                "SELECT title, withdrawn_date, superseded_by FROM documents WHERE doc_id = ?",
                ["DOC_UPDATE_REAL"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            assert_eq!(row.0, "Real manifest update path");
            assert_eq!(row.1.as_deref(), Some("2026-05-02"));
            assert_eq!(row.2.as_deref(), Some("TR 2026/2"));

            let embeddings = chunk_embedding_count(&conn)?;
            assert_eq!(
                embeddings, 1,
                "pack embedding_b64 should populate chunk_embeddings"
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn apply_update_locked_ingests_repacked_definitions_with_same_content_hash() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let release = tempdir()?;
        let release_dir = release.path();
        let packs_dir = release_dir.join("packs");
        fs::create_dir_all(&packs_dir)?;

        let model_bundle = release_dir.join("model-bundle.tar.zst");
        write_test_tar_zst(
            &model_bundle,
            &[
                ("model_quantized.onnx", b"dummy onnx bytes"),
                ("tokenizer.json", br#"{"version":"1.0","truncation":null}"#),
            ],
        )?;
        let model_bundle_bytes = fs::read(&model_bundle)?;
        let embedding_b64 =
            base64::engine::general_purpose::STANDARD.encode(vec![0u8; EMBEDDING_DIM]);

        let base_record = json!({
            "doc_id": "DOC_DEF_REPACK",
            "type": "Public_ruling",
            "title": "Definition repack",
            "date": "2026-05-01",
            "downloaded_at": "2026-05-01T00:00:00Z",
            "content_hash": "same-content-hash",
            "html": "<div><p><strong>test term</strong> means the first definition.</p></div>",
            "assets": [],
            "chunks": [{
                "ord": 0,
                "anchor": "ruling",
                "text": "***test term*** means the first definition.",
                "embedding_b64": embedding_b64
            }]
        });
        let old_pack_bytes = encode_test_pack_record(&base_record)?;
        fs::write(packs_dir.join("pack-olddefs.bin.zst"), &old_pack_bytes)?;

        let old_manifest = Manifest {
            schema_version: MAX_SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "defs-v1".to_string(),
            created_at: "2026-05-01T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: "embeddinggemma-test".to_string(),
                sha256: sha256_hex(&model_bundle_bytes),
                size: model_bundle_bytes.len() as u64,
                url: "model-bundle.tar.zst".to_string(),
                tokenizer_sha256: None,
            },
            reranker: None,
            documents: vec![DocRef {
                doc_id: "DOC_DEF_REPACK".to_string(),
                content_hash: "same-content-hash".to_string(),
                pack_sha8: "olddefs".to_string(),
                offset: 0,
                length: old_pack_bytes.len() as u64,
            }],
            packs: vec![PackInfo {
                sha8: "olddefs".to_string(),
                sha256: sha256_hex(&old_pack_bytes),
                size: old_pack_bytes.len() as u64,
                url: "packs/pack-olddefs.bin.zst".to_string(),
            }],
        };
        let manifest_path = release_dir.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&old_manifest)?)?;

        with_data_dir(data.path(), || -> Result<()> {
            let stats = apply_update_locked(manifest_path.to_str().expect("utf-8 path"))?;
            assert_eq!(stats.added, 1);
            let conn = open_read()?;
            let definitions: i64 =
                conn.query_row("SELECT COUNT(*) FROM definitions", [], |row| row.get(0))?;
            assert_eq!(definitions, 0);
            drop(conn);

            let mut new_record = base_record;
            new_record["definitions"] = json!([{
                "definition_id": "def-test-term",
                "term": "test term",
                "norm_term": "test term",
                "doc_id": "DOC_DEF_REPACK",
                "source_title": "Definition repack",
                "source_type": "Public_ruling",
                "scope": "Definition repack",
                "anchor": "ruling",
                "ord": 0,
                "body": "means the first definition."
            }]);
            let new_pack_bytes = encode_test_pack_record(&new_record)?;
            fs::write(packs_dir.join("pack-newdefs.bin.zst"), &new_pack_bytes)?;
            let mut new_manifest = old_manifest;
            new_manifest.documents[0].pack_sha8 = "newdefs".to_string();
            new_manifest.documents[0].length = new_pack_bytes.len() as u64;
            new_manifest.packs = vec![PackInfo {
                sha8: "newdefs".to_string(),
                sha256: sha256_hex(&new_pack_bytes),
                size: new_pack_bytes.len() as u64,
                url: "packs/pack-newdefs.bin.zst".to_string(),
            }];
            fs::write(&manifest_path, serde_json::to_vec_pretty(&new_manifest)?)?;
            let summary = UpdateSummary {
                schema_version: new_manifest.schema_version,
                index_version: new_manifest.index_version.clone(),
                min_client_version: new_manifest.min_client_version.clone(),
                model: new_manifest.model.clone(),
                reranker: new_manifest.reranker.clone(),
                document_count: new_manifest.documents.len(),
                pack_count: new_manifest.packs.len(),
                manifest_fingerprint: Some(manifest_fingerprint(&new_manifest)?),
            };
            fs::write(
                release_dir.join("update.json"),
                serde_json::to_vec_pretty(&summary)?,
            )?;

            let stats = apply_update_locked(manifest_path.to_str().expect("utf-8 path"))?;
            assert_eq!(stats.added, 0);
            assert_eq!(stats.changed, 1);
            let conn = open_read()?;
            let definitions: i64 =
                conn.query_row("SELECT COUNT(*) FROM definitions", [], |row| row.get(0))?;
            assert_eq!(definitions, 1);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn apply_update_locked_skips_full_manifest_when_update_summary_matches() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let release = tempdir()?;
        let release_dir = release.path();
        let manifest_path = release_dir.join("manifest.json");
        let model_sha = "abc123";
        let manifest = Manifest {
            schema_version: MAX_SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "test-summary-fast-path".to_string(),
            created_at: "2026-05-04T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: "embeddinggemma-test".to_string(),
                sha256: model_sha.to_string(),
                size: 5,
                url: "model-bundle.tar.zst".to_string(),
                tokenizer_sha256: None,
            },
            reranker: None,
            documents: Vec::new(),
            packs: Vec::new(),
        };
        let summary = UpdateSummary {
            schema_version: manifest.schema_version,
            index_version: manifest.index_version.clone(),
            min_client_version: manifest.min_client_version.clone(),
            model: manifest.model.clone(),
            reranker: None,
            document_count: 0,
            pack_count: 0,
            manifest_fingerprint: Some(manifest_fingerprint(&manifest)?),
        };
        fs::write(
            release_dir.join("update.json"),
            serde_json::to_vec_pretty(&summary)?,
        )?;

        with_data_dir(data.path(), || -> Result<()> {
            let conn = open_write_at(&db_path()?)?;
            init_db(&conn)?;
            drop(conn);
            fs::write(
                installed_manifest_path()?,
                serde_json::to_vec_pretty(&manifest)?,
            )?;
            fs::write(live_dir()?.join("model_quantized.onnx"), b"model")?;
            fs::write(live_dir()?.join("tokenizer.json"), br#"{"version":"1.0"}"#)?;
            fs::write(live_dir()?.join(".model.sha256"), model_sha)?;

            let stats = apply_update_locked(manifest_path.to_str().expect("utf-8 path"))?;
            assert_eq!(stats.added, 0);
            assert_eq!(stats.changed, 0);
            assert_eq!(stats.removed, 0);
            assert!(
                stats.bytes_downloaded < 512,
                "fast path should fetch only update.json, not the full manifest"
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn apply_update_locked_rebuilds_unsupported_schema_db() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let release = tempdir()?;
        let release_dir = release.path();
        let packs_dir = release_dir.join("packs");
        fs::create_dir_all(&packs_dir)?;

        let model_bundle = release_dir.join("model-bundle.tar.zst");
        write_test_tar_zst(
            &model_bundle,
            &[
                ("model_quantized.onnx", b"dummy onnx bytes"),
                ("tokenizer.json", br#"{"version":"1.0","truncation":null}"#),
            ],
        )?;
        let model_bundle_bytes = fs::read(&model_bundle)?;

        let embedding_b64 =
            base64::engine::general_purpose::STANDARD.encode(vec![0u8; EMBEDDING_DIM]);
        let record = json!({
            "doc_id": "DOC_REBUILD_SCHEMA",
            "type": "Public_ruling",
            "title": "Rebuilt unsupported schema corpus",
            "date": "2026-05-03",
            "downloaded_at": "2026-05-03T00:00:00Z",
            "content_hash": "hash-rebuild-schema",
            "html": "<div><p>Unsupported schema update path must rebuild before semantic probes.</p></div>",
            "assets": [],
            "withdrawn_date": JsonValue::Null,
            "superseded_by": JsonValue::Null,
            "replaces": JsonValue::Null,
            "chunks": [{
                "ord": 0,
                "anchor": "ruling",
                "text": "Unsupported schema update path must rebuild before semantic probes.",
                "embedding_b64": embedding_b64
            }]
        });
        let pack_bytes = encode_test_pack_record(&record)?;
        let pack_path = packs_dir.join("pack-feedface.bin.zst");
        fs::write(&pack_path, &pack_bytes)?;

        let manifest = Manifest {
            schema_version: MAX_SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "test-rebuild-schema".to_string(),
            created_at: "2026-05-03T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: "embeddinggemma-test".to_string(),
                sha256: sha256_hex(&model_bundle_bytes),
                size: model_bundle_bytes.len() as u64,
                url: "model-bundle.tar.zst".to_string(),
                tokenizer_sha256: None,
            },
            reranker: None,
            documents: vec![DocRef {
                doc_id: "DOC_REBUILD_SCHEMA".to_string(),
                content_hash: "hash-rebuild-schema".to_string(),
                pack_sha8: "feedface".to_string(),
                offset: 0,
                length: pack_bytes.len() as u64,
            }],
            packs: vec![PackInfo {
                sha8: "feedface".to_string(),
                sha256: sha256_hex(&pack_bytes),
                size: pack_bytes.len() as u64,
                url: "packs/pack-feedface.bin.zst".to_string(),
            }],
        };
        let manifest_path = release_dir.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

        with_data_dir(data.path(), || -> Result<()> {
            let conn = open_write_at(&db_path()?)?;
            init_db(&conn)?;
            set_meta(&conn, "schema_version", "5")?;
            drop(conn);
            fs::write(
                installed_manifest_path()?,
                serde_json::to_vec_pretty(&sample_manifest(
                    MAX_SUPPORTED_MANIFEST_VERSION as i64,
                    env!("CARGO_PKG_VERSION"),
                ))?,
            )?;

            let stats = apply_update_locked(manifest_path.to_str().expect("utf-8 path"))?;
            assert_eq!(stats.added, 1);
            assert_eq!(stats.changed, 0);
            assert_eq!(stats.removed, 0);

            let conn = open_read()?;
            assert_eq!(get_meta(&conn, "schema_version")?.as_deref(), Some("8"));
            let title: String = conn.query_row(
                "SELECT title FROM documents WHERE doc_id = ?",
                ["DOC_REBUILD_SCHEMA"],
                |row| row.get(0),
            )?;
            assert_eq!(title, "Rebuilt unsupported schema corpus");
            assert_eq!(chunk_embedding_count(&conn)?, 1);
            Ok(())
        })?;
        Ok(())
    }

    // ===== Wave 3-B Reranker ===============================================

    /// Helper: build a Hit with the slim contract. Tests below pin that the
    /// wire shape stays slim (no score, no ord, no debug metadata).
    fn make_test_hit() -> Hit {
        Hit {
            doc_id: "DOC".to_string(),
            title: "T".to_string(),
            doc_type: "Public_ruling".to_string(),
            date: None,
            anchor: None,
            snippet: Some("snip".to_string()),
            canonical_url: "https://x".to_string(),
            chunk_id: Some(1),
            next_call: None,
            withdrawn_date: None,
            superseded_by: None,
            replaces: None,
            has_in_doc_links: None,
            has_related_docs: None,
            has_history: None,
        }
    }

    #[test]
    fn hit_json_is_slim_no_score_no_ord_no_debug() {
        let hit = make_test_hit();
        let json_str = serde_json::to_string(&hit).unwrap();
        for forbidden in [
            "\"score\"",
            "\"ord\"",
            "\"ranking\"",
            "reranker_score",
            "embedding_model_id",
        ] {
            assert!(
                !json_str.contains(forbidden),
                "slim Hit JSON must not carry `{forbidden}`; got {json_str}"
            );
        }
    }

    #[test]
    fn reranker_disabled_when_env_var_set() {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        // Snapshot+restore so concurrent tests in the same process don't
        // inherit the kill-switch.
        let prev = std::env::var("ATO_MCP_DISABLE_RERANKER").ok();
        std::env::set_var("ATO_MCP_DISABLE_RERANKER", "1");
        let mut state = ServerState::default();
        let candidates: Vec<(i64, &str)> = vec![(1, "doc one"), (2, "doc two")];
        let result = state
            .rerank_candidates("query", &candidates)
            .expect("env-disable returns Ok(None)");
        assert!(
            result.is_none(),
            "ATO_MCP_DISABLE_RERANKER=1 must short-circuit to RRF"
        );
        // After the env-toggle path runs, state should be Disabled.
        assert!(matches!(state.reranker_state, RerankerState::Disabled));
        if let Some(p) = prev {
            std::env::set_var("ATO_MCP_DISABLE_RERANKER", p);
        } else {
            std::env::remove_var("ATO_MCP_DISABLE_RERANKER");
        }
    }

    #[test]
    fn reranker_disabled_when_model_files_missing() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        // Make sure env kill-switch is off so we exercise the
        // missing-files branch, not the env branch.
        let env_prev = std::env::var("ATO_MCP_DISABLE_RERANKER").ok();
        std::env::remove_var("ATO_MCP_DISABLE_RERANKER");
        let dir = tempdir()?;
        with_data_dir(dir.path(), || -> Result<()> {
            // live/ exists (it's auto-created by live_dir()), but neither
            // reranker.onnx nor reranker_tokenizer.json do.
            assert!(!reranker_model_path()?.exists());
            assert!(!reranker_tokenizer_path()?.exists());
            let mut state = ServerState::default();
            let candidates: Vec<(i64, &str)> = vec![(1, "alpha"), (2, "beta")];
            let result = state.rerank_candidates("q", &candidates)?;
            assert!(result.is_none(), "missing model -> Ok(None)");
            assert!(matches!(state.reranker_state, RerankerState::Disabled));
            // Second call must NOT re-attempt load — Disabled is sticky.
            let result2 = state.rerank_candidates("q", &candidates)?;
            assert!(result2.is_none());
            Ok(())
        })?;
        if let Some(p) = env_prev {
            std::env::set_var("ATO_MCP_DISABLE_RERANKER", p);
        }
        Ok(())
    }

    #[test]
    fn reranker_disabled_after_failed_load() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let env_prev = std::env::var("ATO_MCP_DISABLE_RERANKER").ok();
        std::env::remove_var("ATO_MCP_DISABLE_RERANKER");
        let dir = tempdir()?;
        with_data_dir(dir.path(), || -> Result<()> {
            // Plant garbage file contents so the path-exists check passes
            // but the loader bails — this simulates a corrupted download.
            fs::write(reranker_model_path()?, b"not really an onnx model")?;
            fs::write(reranker_tokenizer_path()?, b"not really a tokenizer json")?;
            let mut state = ServerState::default();
            let candidates: Vec<(i64, &str)> = vec![(1, "alpha")];
            // First call: load attempt triggers, fails, transitions to Disabled.
            let result = state.rerank_candidates("q", &candidates)?;
            assert!(result.is_none(), "failed load -> Ok(None)");
            assert!(matches!(state.reranker_state, RerankerState::Disabled));
            // Second call: still Disabled, no retry.
            let result2 = state.rerank_candidates("q", &candidates)?;
            assert!(result2.is_none());
            Ok(())
        })?;
        if let Some(p) = env_prev {
            std::env::set_var("ATO_MCP_DISABLE_RERANKER", p);
        }
        Ok(())
    }

    #[test]
    fn reranker_returns_empty_for_empty_candidates() {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let env_prev = std::env::var("ATO_MCP_DISABLE_RERANKER").ok();
        std::env::remove_var("ATO_MCP_DISABLE_RERANKER");
        let mut state = ServerState::default();
        // Empty candidates short-circuit before the load path so we get
        // Some(empty) and never touch the model files.
        let result = state.rerank_candidates("q", &[]).expect("ok");
        assert_eq!(result, Some(Vec::new()));
        if let Some(p) = env_prev {
            std::env::set_var("ATO_MCP_DISABLE_RERANKER", p);
        }
    }

    // ----- I3: env-var falsy values do NOT disable the reranker ------------
    //
    // env_truthy() is the gate for ATO_MCP_DISABLE_RERANKER. Anything other
    // than the recognised truthy spellings (`1`, `true`, `TRUE`, `yes`,
    // `YES`, `on`, `ON`) is a no-op — including the empty string, `0`,
    // `false`, and unusual spellings like `True` (mixed-case Python style).
    // A regression here would silently disable the reranker for users who
    // copied an env-var template and left a benign value in place.

    #[test]
    fn reranker_env_var_falsy_does_not_disable() {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let env_prev = std::env::var("ATO_MCP_DISABLE_RERANKER").ok();
        // Each value below MUST NOT trigger the kill-switch.
        for value in [
            "0", "false", "False", "FALSE", "", "True", "no", "off", "disabled",
        ] {
            std::env::set_var("ATO_MCP_DISABLE_RERANKER", value);
            // Use empty candidates so we don't hit the load path (which
            // would also disable us via missing-files in this test env).
            // The empty-candidates short-circuit only runs AFTER the env
            // gate, so this still tells us the gate didn't fire.
            let mut state = ServerState::default();
            let out = state
                .rerank_candidates("q", &[])
                .expect("rerank_candidates returns Ok for empty input");
            assert_eq!(
                out,
                Some(Vec::new()),
                "ATO_MCP_DISABLE_RERANKER={value:?} must NOT short-circuit; \
                 falsy/unknown values should leave reranker eligible"
            );
            assert!(
                !matches!(state.reranker_state, RerankerState::Disabled),
                "state must stay Pending for ATO_MCP_DISABLE_RERANKER={value:?}"
            );
        }
        if let Some(p) = env_prev {
            std::env::set_var("ATO_MCP_DISABLE_RERANKER", p);
        } else {
            std::env::remove_var("ATO_MCP_DISABLE_RERANKER");
        }
    }

    #[test]
    fn cli_keyword_search_skips_reranker_state_machine() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let env_prev = std::env::var("ATO_MCP_DISABLE_RERANKER").ok();
        std::env::remove_var("ATO_MCP_DISABLE_RERANKER");
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        insert_doc(&conn, "DOC_CLI_RERANK")?;
        let text = "Research and development tax incentive material for CLI search.";
        insert_chunk(&conn, 1, "DOC_CLI_RERANK", 0, text)?;
        conn.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (?, ?)",
            params![1_i64, text],
        )?;
        drop(conn);

        let result = with_data_dir(dir.path(), || -> Result<(String, ServerState)> {
            search_cli(
                "research development",
                SearchOptions {
                    k: 1,
                    types: None,
                    date_from: None,
                    date_to: None,
                    doc_scope: None,
                    mode: SearchMode::Keyword,
                    sort_by: SortBy::Relevance,
                    include_old: false,
                    current_only: true,
                    max_per_doc: DEFAULT_MAX_PER_DOC,
                    include_snippet: true,
                    similar_to_chunk_id: None,
                },
            )
        });
        if let Some(p) = env_prev {
            std::env::set_var("ATO_MCP_DISABLE_RERANKER", p);
        } else {
            std::env::remove_var("ATO_MCP_DISABLE_RERANKER");
        }

        let (json_str, state) = result?;
        assert!(
            matches!(state.reranker_state, RerankerState::Pending),
            "keyword search should stay lexical-only and avoid touching the reranker"
        );
        let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
        assert_eq!(parsed["hits"][0]["doc_id"], json!("DOC_CLI_RERANK"));
        // Slim response: no `ranking` block, no reranker debug metadata
        // surfaces to the agent.
        assert!(parsed.get("ranking").is_none(), "ranking block must not be exposed");
        Ok(())
    }

    #[test]
    fn hf_reranker_download_reports_model_path_failure() -> Result<()> {
        let dir = tempdir()?;
        let info = ModelInfo {
            id: "rerank-id".to_string(),
            sha256: "a".repeat(64),
            size: 1,
            url: "hf://example/repo@rev".to_string(),
            tokenizer_sha256: None,
        };
        let err = download_hf_reranker_model_with(
            "example/repo",
            "rev",
            &info,
            dir.path(),
            |url, _dest| Err(anyhow!("404 for {url}")),
        )
        .expect_err("download should fail");
        let msg = err.to_string();
        assert!(
            msg.contains(RERANKER_HF_MODEL_PATH),
            "error should include canonical model path; got: {msg}"
        );
        assert!(
            !dir.path().join("reranker.onnx.part").exists(),
            "failed download should clean partial model file"
        );
        Ok(())
    }

    #[test]
    fn hf_reranker_download_rejects_sha_mismatch() -> Result<()> {
        let dir = tempdir()?;
        let expected = b"correct model bytes";
        let info = ModelInfo {
            id: "rerank-id".to_string(),
            sha256: sha256_hex(expected),
            size: expected.len() as u64,
            url: "hf://example/repo@rev".to_string(),
            tokenizer_sha256: None,
        };
        let mut calls = Vec::new();
        let err = download_hf_reranker_model_with(
            "example/repo",
            "rev",
            &info,
            dir.path(),
            |url, dest| {
                calls.push(url.to_string());
                fs::write(dest, b"wrong model bytes")?;
                Ok(fs::metadata(dest)?.len())
            },
        )
        .expect_err("sha mismatch must be fatal");
        assert_eq!(calls.len(), 1, "only the canonical model path is tried");
        assert!(err.to_string().contains("sha256 mismatch"));
        assert!(
            !dir.path().join("reranker.onnx.part").exists(),
            "failed sha verification should clean partial model file"
        );
        Ok(())
    }

    // ----- I3: slim Hit JSON surface ---------------------------------------
    //
    // The slim Hit contract (MT-04 + MT-04-slim) hides internal scoring from
    // the agent — no `score`, no `ord`, no `ranking` block, no
    // `reranker_score`. Pins the JSON shape so a future refactor can't
    // reintroduce internal-metric leakage.

    #[test]
    fn hit_json_hides_internal_scoring_metadata() {
        let hit = Hit {
            doc_id: "DOC".to_string(),
            title: "doc".to_string(),
            doc_type: "Public_ruling".to_string(),
            date: None,
            anchor: None,
            snippet: Some("snip".to_string()),
            canonical_url: "https://x".to_string(),
            chunk_id: Some(1),
            next_call: None,
            withdrawn_date: None,
            superseded_by: None,
            replaces: None,
            has_in_doc_links: None,
            has_related_docs: None,
            has_history: None,
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&hit).unwrap()).unwrap();
        for forbidden in [
            "score",
            "ord",
            "ranking",
            "reranker_score",
            "embedding_model_id",
        ] {
            assert!(
                parsed.get(forbidden).is_none(),
                "slim Hit JSON must not expose `{forbidden}`; got {parsed:?}"
            );
        }
    }

    // ----- I3: dedup behaviour at the mixed-scale boundary ------------------
    //
    // The dedup pass picks the BEST chunk per doc, where "best" means the
    // single highest score — there is no tail-sum aggregation. This test
    // pins that contract: a doc with one barely-positive head chunk must
    // still beat a doc with multiple weaker tail chunks, IFF that head
    // chunk's individual score is higher than every tail chunk's individual
    // score.
    //
    // Synthetic candidates: 50 head-scored chunks (sigmoid range 0.30-0.95)
    // distributed across many docs, plus tail chunks with RRF scores
    // (0.010-0.030) for one doc that has no head representation. We confirm
    // the boundary at 50/51:
    //   - the strong head chunks dedup naturally to one-per-doc
    //   - the weak-tail-only doc is correctly placed BELOW any head doc
    //   - per-doc max selection produces deterministic ordering
    //
    // If a future implementation adds tail-sum aggregation, this test will
    // still pass (the strong head doc still wins on max) — that's correct
    // and intentional. If a future change instead capriciously promotes
    // weak head over strong tail, this catches it at the boundary.

    #[test]
    fn reranker_dedup_handles_mixed_scale_boundary() {
        // Build 50 strong head hits across 10 docs (5 chunks per doc),
        // sigmoid scores in [0.30, 0.95] descending — interleaved so doc
        // ordering isn't a sort-stable accident.
        let mut hits: Vec<VectorHit> = Vec::with_capacity(60);
        for i in 0..50 {
            let doc_idx = i % 10;
            let score = 0.95 - (i as f64) * 0.013; // 0.95 -> 0.30
            hits.push(VectorHit {
                chunk_id: i as i64 + 1, // 1..=50
                score,
            });
            // candidate_meta entry built below.
            let _ = doc_idx;
        }
        // Now add 5 weak-tail chunks for DOC_TAIL_ONLY (chunk_ids 51..=55).
        // RRF scores in [0.010, 0.030]. Each individual score is well below
        // every head chunk's score.
        for j in 0..5 {
            hits.push(VectorHit {
                chunk_id: 51 + j as i64,
                score: 0.010 + (j as f64) * 0.005, // 0.010..=0.030
            });
        }

        // Build candidate_meta: head chunks belong to DOC_H{0..9}; tail
        // chunks all belong to DOC_TAIL_ONLY.
        let mut meta: HashMap<i64, CandidateMeta> = HashMap::new();
        for i in 0..50 {
            let doc_idx = i % 10;
            meta.insert(
                i as i64 + 1,
                CandidateMeta {
                    doc_id: format!("DOC_H{doc_idx}"),
                    is_intro: false,
                },
            );
        }
        for j in 0..5 {
            meta.insert(
                51 + j as i64,
                CandidateMeta {
                    doc_id: "DOC_TAIL_ONLY".to_string(),
                    is_intro: false,
                },
            );
        }

        // Frontier 11 (just enough to cover the 10 head docs + the tail
        // doc). max_per_doc=1 so each doc contributes exactly one chunk —
        // confirming each head doc earns its slot via per-doc max, and the
        // weak tail doc still appears LAST despite having more candidate
        // chunks than any individual head doc.
        let deduped = dedup_per_doc(hits, &meta, 11, 1);

        // Confirm DOC_TAIL_ONLY appears at most once in the deduped output
        // (per-doc dedup), and confirm at least one head chunk from each
        // DOC_H{0..9} appears.
        let mut head_doc_count = std::collections::HashSet::new();
        let mut tail_seen = false;
        let mut tail_position = None;
        for (idx, hit) in deduped.iter().enumerate() {
            let doc_id = &meta[&hit.chunk_id].doc_id;
            if doc_id == "DOC_TAIL_ONLY" {
                tail_seen = true;
                tail_position = Some(idx);
            } else {
                head_doc_count.insert(doc_id.clone());
            }
        }
        assert_eq!(
            head_doc_count.len(),
            10,
            "all 10 head docs should be represented; got: {:?}",
            head_doc_count
        );
        assert!(
            tail_seen,
            "DOC_TAIL_ONLY (weak tail) should still appear in frontier=11"
        );
        // The weak tail doc must rank LAST (after all 10 head docs whose
        // best chunk score >= 0.30 > 0.030 max tail score). This pins the
        // per-doc-max behaviour: the head's barely-positive chunk wins
        // over the tail's sum-of-weak chunks.
        let pos = tail_position.expect("tail position recorded above");
        assert!(
            pos >= 10,
            "DOC_TAIL_ONLY should rank below all 10 head docs; \
             got position {pos}/{} — implementation may have started \
             promoting weak-head over strong-tail (regression)",
            deduped.len()
        );

        // No doc should appear more than max_per_doc times.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for hit in &deduped {
            *counts
                .entry(meta[&hit.chunk_id].doc_id.as_str())
                .or_insert(0) += 1;
        }
        for (doc, n) in &counts {
            assert!(*n <= 1, "max_per_doc=1 violated for {doc}: {n} chunks");
        }
    }

    #[test]
    fn manifest_compat_accepts_v3_with_reranker() -> Result<()> {
        let mut m = sample_manifest(3, "");
        m.reranker = Some(ModelInfo {
            id: "ms-marco-MiniLM-L-6-v2".to_string(),
            sha256: "abc".to_string(),
            size: 25_000_000,
            url: "hf://cross-encoder/ms-marco-MiniLM-L-6-v2".to_string(),
            tokenizer_sha256: None,
        });
        enforce_manifest_compatibility(&m)?;
        Ok(())
    }

    #[test]
    fn manifest_compat_rejects_newer_manifest_format() {
        let m = sample_manifest((MAX_SUPPORTED_MANIFEST_VERSION + 1) as i64, "");
        let err =
            enforce_manifest_compatibility(&m).expect_err("newer manifest should be rejected");
        assert!(
            err.to_string().contains("upgrade the ato-mcp binary"),
            "expected upgrade-binary error, got: {err}"
        );
    }

    #[test]
    fn manifest_round_trips_reranker_field() -> Result<()> {
        // Ensure serde round-trips the optional reranker entry without
        // losing the inner ModelInfo shape (the contract Python depends on).
        let mut m = sample_manifest(3, "0.6.0");
        m.reranker = Some(ModelInfo {
            id: "rerank-id".to_string(),
            sha256: "deadbeef".to_string(),
            size: 1234,
            url: "https://example.com/reranker.tar.zst".to_string(),
            tokenizer_sha256: Some("cafef00d".to_string()),
        });
        let json_str = serde_json::to_string(&m)?;
        let v: serde_json::Value = serde_json::from_str(&json_str)?;
        assert_eq!(v["reranker"]["id"], json!("rerank-id"));
        assert_eq!(v["reranker"]["sha256"], json!("deadbeef"));
        assert_eq!(v["reranker"]["size"], json!(1234));
        assert_eq!(
            v["reranker"]["url"],
            json!("https://example.com/reranker.tar.zst")
        );
        // C4: tokenizer_sha256 round-trips through the manifest when set.
        assert_eq!(v["reranker"]["tokenizer_sha256"], json!("cafef00d"));
        let parsed: Manifest = serde_json::from_str(&json_str)?;
        assert!(parsed.reranker.is_some());
        let rr = parsed.reranker.as_ref().unwrap();
        assert_eq!(rr.tokenizer_sha256.as_deref(), Some("cafef00d"));
        Ok(())
    }

    #[test]
    fn manifest_omits_reranker_when_none() -> Result<()> {
        let m = sample_manifest(3, "0.6.0");
        assert!(m.reranker.is_none());
        let json_str = serde_json::to_string(&m)?;
        // skip_serializing_if drops the key entirely when None — Python
        // side relies on this so v2 manifests round-trip identically.
        assert!(
            !json_str.contains("reranker"),
            "reranker key must be omitted when None; json={json_str}"
        );
        Ok(())
    }

    #[test]
    fn extract_rerank_logits_handles_batch_one_shape() {
        let logits = extract_rerank_logits(&[3, 1], &[0.1, 0.2, 0.3], 3).unwrap();
        assert_eq!(logits, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn extract_rerank_logits_handles_flat_batch_shape() {
        let logits = extract_rerank_logits(&[3], &[0.1, 0.2, 0.3], 3).unwrap();
        assert_eq!(logits, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn extract_rerank_logits_picks_positive_class_for_two_class_output() {
        // Some MS-MARCO exports emit `[batch, 2]` (negative, positive).
        // We must take index 1 — the positive class.
        let logits = extract_rerank_logits(&[2, 2], &[0.1, 0.9, 0.2, 0.8], 2).unwrap();
        assert_eq!(logits, vec![0.9, 0.8]);
    }

    #[test]
    fn extract_rerank_logits_rejects_unexpected_shape() {
        let err = extract_rerank_logits(&[2, 2, 2], &[0.0; 8], 2).unwrap_err();
        assert!(err.to_string().contains("unexpected reranker output shape"));
    }

    #[test]
    fn sigmoid_squashes_into_unit_interval() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-9);
        assert!(sigmoid(10.0) > 0.9999);
        assert!(sigmoid(-10.0) < 0.0001);
    }

    /// Integration test: actually load the reranker model and score a
    /// small batch. Skipped automatically when no model is installed —
    /// CI without a reranker bundle will simply log "skipped" rather
    /// than fail.
    #[test]
    fn reranker_scores_real_batch_when_model_present() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let model = match dirs::data_dir() {
            Some(mut p) => {
                p.push(APP_NAME);
                p.push("live");
                p.push("reranker.onnx");
                p
            }
            None => return Ok(()),
        };
        let tokenizer = model.parent().unwrap().join("reranker_tokenizer.json");
        if !model.exists() || !tokenizer.exists() {
            eprintln!(
                "(skipped: reranker model not installed at {})",
                model.display()
            );
            return Ok(());
        }
        let mut reranker = Reranker::load()?;
        let candidates: Vec<(i64, &str)> = vec![
            (
                1,
                "Income tax assessment of foreign superannuation lump sums.",
            ),
            (2, "Recipe for spaghetti bolognese with garlic bread."),
            (
                3,
                "Tax treatment of foreign superannuation transfers under section 305-70.",
            ),
        ];
        let scores = reranker.rerank("how are foreign super transfers taxed?", &candidates)?;
        assert_eq!(scores.len(), 3);
        for (_, s) in &scores {
            assert!(*s >= 0.0 && *s <= 1.0, "sigmoid score out of range: {s}");
        }
        // The off-topic recipe should score lowest. Order isn't strict
        // but the worst score should be <= the best by a healthy margin.
        let recipe_score = scores.iter().find(|(id, _)| *id == 2).unwrap().1;
        let best = scores.iter().map(|(_, s)| *s).fold(0.0_f64, f64::max);
        assert!(
            best - recipe_score > 0.05,
            "expected non-trivial gap between best and off-topic; got best={best}, recipe={recipe_score}"
        );

        // Cheap latency sanity check: 50 pairs in well under 5s on any
        // dev box. We keep the bound generous so CI doesn't flake.
        let many: Vec<(i64, &str)> = (0..50)
            .map(|i| {
                (
                    i as i64,
                    "Section 8-1 deduction for expenses incurred in earning assessable income.",
                )
            })
            .collect();
        let start = std::time::Instant::now();
        let _ = reranker.rerank("section 8-1 deductions", &many)?;
        let elapsed = start.elapsed();
        eprintln!("rerank-50-pair latency: {:?} (informational)", elapsed);
        assert!(
            elapsed < Duration::from_secs(5),
            "50-pair rerank took {elapsed:?}; check ONNX runtime config"
        );
        Ok(())
    }

    #[test]
    fn test_get_doc_anchors_includes_cited_by() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        // Three docs cite TARGET. Two are 2024 dated (modern); one 2010.
        // `cited_by` should order by date DESC.
        insert_doc(&conn, "TARGET")?;
        conn.execute(
            "INSERT INTO documents(doc_id, type, title, downloaded_at, content_hash, pack_sha8, html, date) VALUES (?, 'Public_ruling', 'Citer 2024', ?, ?, '00000000', x'00', '2024-06-15')",
            params!["CITER_2024A", Utc::now().to_rfc3339(), "h1"],
        )?;
        conn.execute(
            "INSERT INTO documents(doc_id, type, title, downloaded_at, content_hash, pack_sha8, html, date) VALUES (?, 'Public_ruling', 'Citer 2024 B', ?, ?, '00000000', x'00', '2024-01-10')",
            params!["CITER_2024B", Utc::now().to_rfc3339(), "h2"],
        )?;
        conn.execute(
            "INSERT INTO documents(doc_id, type, title, downloaded_at, content_hash, pack_sha8, html, date) VALUES (?, 'Cases', 'Citer 2010', ?, ?, '00000000', x'00', '2010-02-02')",
            params!["CITER_2010", Utc::now().to_rfc3339(), "h3"],
        )?;
        // One citing chunk per citer; TARGET is the citation target.
        insert_chunk(&conn, 10, "CITER_2024A", 0, "see [doc:TARGET]")?;
        insert_chunk(&conn, 11, "CITER_2024B", 0, "also [doc:TARGET]")?;
        insert_chunk(&conn, 12, "CITER_2010", 0, "refer [doc:TARGET]")?;
        conn.execute(
            "INSERT INTO citations(source_chunk_id, source_doc_id, target_doc_id) VALUES (?, ?, ?)",
            params![10_i64, "CITER_2024A", "TARGET"],
        )?;
        conn.execute(
            "INSERT INTO citations(source_chunk_id, source_doc_id, target_doc_id) VALUES (?, ?, ?)",
            params![11_i64, "CITER_2024B", "TARGET"],
        )?;
        conn.execute(
            "INSERT INTO citations(source_chunk_id, source_doc_id, target_doc_id) VALUES (?, ?, ?)",
            params![12_i64, "CITER_2010", "TARGET"],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = get_doc_anchors("TARGET")?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let cited_by = parsed["cited_by"].as_array().unwrap();
            assert_eq!(cited_by.len(), 3);
            // Date-DESC order.
            assert_eq!(cited_by[0]["doc_id"], json!("CITER_2024A"));
            assert_eq!(cited_by[0]["date"], json!("2024-06-15"));
            assert_eq!(cited_by[0]["title"], json!("Citer 2024"));
            assert_eq!(cited_by[0]["type"], json!("Public_ruling"));
            assert_eq!(cited_by[1]["doc_id"], json!("CITER_2024B"));
            assert_eq!(cited_by[2]["doc_id"], json!("CITER_2010"));
            // Total field omitted when no truncation occurred.
            assert!(parsed.get("cited_by_total").is_none());
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn test_get_doc_anchors_cited_by_truncates_with_total() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        insert_doc(&conn, "POPULAR")?;
        // Insert CITED_BY_LIMIT + 5 citers so truncation kicks in.
        let count = CITED_BY_LIMIT + 5;
        for i in 0..count {
            let citer = format!("CITER_{:03}", i);
            conn.execute(
                "INSERT INTO documents(doc_id, type, title, downloaded_at, content_hash, pack_sha8, html, date) VALUES (?, 'Public_ruling', ?, ?, ?, '00000000', x'00', '2024-01-01')",
                params![citer.clone(), format!("Citer {i}"), Utc::now().to_rfc3339(), format!("h{i}")],
            )?;
            insert_chunk(&conn, (1000 + i) as i64, &citer, 0, "[doc:POPULAR]")?;
            conn.execute(
                "INSERT INTO citations(source_chunk_id, source_doc_id, target_doc_id) VALUES (?, ?, ?)",
                params![(1000 + i) as i64, citer, "POPULAR"],
            )?;
        }
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = get_doc_anchors("POPULAR")?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let cited_by = parsed["cited_by"].as_array().unwrap();
            assert_eq!(cited_by.len(), CITED_BY_LIMIT);
            assert_eq!(parsed["cited_by_total"], json!(count as i64));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn test_get_doc_anchors_cited_by_empty_when_no_table() -> Result<()> {
        // Older corpora that predate the citations table should degrade
        // gracefully — `cited_by` is just an empty array.
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        conn.execute("DROP TABLE IF EXISTS citations", [])?;
        insert_doc(&conn, "NOCITES")?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = get_doc_anchors("NOCITES")?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let cited_by = parsed["cited_by"].as_array().unwrap();
            assert!(cited_by.is_empty());
            assert!(parsed.get("cited_by_total").is_none());
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn test_load_chunk_embedding_roundtrip() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        insert_doc(&conn, "EMB")?;
        insert_chunk(&conn, 42, "EMB", 0, "body")?;
        let mut bytes = vec![0u8; EMBEDDING_DIM];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as i8).wrapping_mul(3) as u8;
        }
        conn.execute(
            "INSERT INTO chunk_embeddings(chunk_id, embedding) VALUES (?, ?)",
            params![42_i64, bytes.clone()],
        )?;
        let loaded = load_chunk_embedding(&conn, 42)?;
        let expected: Vec<i8> = bytes.iter().map(|b| *b as i8).collect();
        assert_eq!(loaded.to_vec(), expected);
        Ok(())
    }

    #[test]
    fn test_load_chunk_embedding_missing_chunk_errors() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        let err = load_chunk_embedding(&conn, 99999).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no stored embedding"), "unexpected error: {msg}");
        Ok(())
    }

    #[test]
    fn test_derive_citations_extracts_doc_markers() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        insert_doc(&conn, "SRC")?;
        insert_doc(&conn, "T1")?;
        insert_doc(&conn, "T2")?;
        // chunk text exercises: base marker, PiT-qualified marker (must
        // collapse to base), view-qualified marker (must collapse to base),
        // self-citation (must be skipped), and repeated marker (deduped
        // per-chunk).
        insert_chunk(
            &conn,
            10,
            "SRC",
            0,
            "see [doc:T1] and [doc:T2@19960320000001] and [doc:T2 view=HISTFT] and [doc:SRC] and [doc:T1]",
        )?;
        // pre-populate with stale rows so we can confirm clear + repopulate
        conn.execute(
            "INSERT INTO citations(source_chunk_id, source_doc_id, target_doc_id) VALUES (?, ?, ?)",
            params![10_i64, "SRC", "STALE"],
        )?;

        derive_citations(&conn)?;

        let rows: Vec<(i64, String, String)> = conn
            .prepare("SELECT source_chunk_id, source_doc_id, target_doc_id FROM citations ORDER BY target_doc_id")?
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Stale row gone; T1 deduped to one entry; T2 base extracted twice
        // but INSERT OR IGNORE keeps one row; SRC self-citation excluded.
        assert_eq!(
            rows,
            vec![
                (10, "SRC".to_string(), "T1".to_string()),
                (10, "SRC".to_string(), "T2".to_string()),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_derive_citations_handles_missing_table() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        conn.execute("DROP TABLE citations", [])?;
        // Should no-op without erroring.
        derive_citations(&conn)?;
        Ok(())
    }

    // Tests that touch the global data dir env var cannot run in
    // parallel — serialise them through a single mutex.
    static TEST_DB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
