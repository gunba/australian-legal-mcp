use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
#[allow(unused_imports)]
use fs2::FileExt;
use reqwest::blocking::Client;
#[allow(unused_imports)]
use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension};
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use url::Url;

mod ann;
mod build;
mod chunker;
mod config;
mod db;
mod extract;
mod frl;
mod html;
mod legal_source;
mod pipeline;
mod retrieval;
mod rules;
mod search;
mod semantic;
mod source;
mod source_update;
mod uri;

use build::{build_corpus, package_corpus, update_manifest_with_db, BuildCorpusArgs};
use config::{live_dir, model_data_path, model_path, tokenizer_path};
use legal_model::{AssetRef, ChunkRef, DocumentId, SourceId};
use legal_source::source_registry;
use retrieval::{
    fetch, get_asset, get_chunks, get_definition, get_doc_anchors, pit_to_date, GetChunksOptions,
    GetDefinitionOptions,
};
use search::{search, search_cli, SearchOptions};
#[cfg(test)]
use semantic::dot_i8_scalar_reference;
use semantic::{
    SemanticEncodeStats, SemanticModelPaths, SemanticRuntime, EMBEDDING_MODEL_HF_FILES,
};
use source::{
    apply_update, check_for_update_availability, link_download, resolve_latest_corpus_manifest_url,
    resolve_latest_corpus_manifest_url_probe, scrape_diff, snapshot_reduce, stats, tree_crawl,
    LinkDownloadArgs, Manifest, ModelInfo, StagedModel, UpdateAvailability,
};
use source_update::{run_source_updates, SourceUpdateRequest};

pub(crate) const APP_NAME: &str = "australian-legal-mcp";
pub(crate) const DEFAULT_GITHUB_REPOSITORY: &str = "gunba/australian-legal-mcp";
pub(crate) const DEFAULT_RELEASES_API_URL: &str =
    "https://api.github.com/repos/gunba/australian-legal-mcp/releases";
pub(crate) const DEFAULT_K: usize = 8;
pub(crate) const MAX_K: usize = 50;
/// Cap on the `title_hits` sidebar `search` returns alongside chunk hits.
/// Direct doc_id / ATO-link matches always lead; the BM25 title remainder
/// fills the rest.
pub(crate) const TITLE_HITS_K: usize = 10;
pub(crate) const SNIPPET_CHARS: usize = 280;
// Stored semantic vectors are the first 256 dimensions of the
// model output after normalization + int8 quantization.
pub(crate) const EMBEDDING_DIM: usize = 256;
// The tokenizer truncates semantic inputs and pads dynamically to
// each batch's max sequence length.
pub(crate) const EMBEDDING_INPUT_MAX_TOKENS: usize = 1024;
// Granite ONNX inputs use source-derived text directly; no
// query/passage prompt prefix is stored in chunks or added at runtime.
const EMBEDDING_TEXT_PREFIX: &str = "";
pub(crate) const EMBEDDING_MODEL_FINGERPRINT: &str =
    "granite-small-r2-fp16:ee200de55cb2f94e858aabca54be7697a9c0805a14c858ee26ad0922b05f57d7:28d16e29cd623f25cc6fa0968700c5bc31036466091a5fa06d1353c1777f050e:feeb83348dcb033bc6b9d2e1f7906ca9eb2d122845000c9416d894d7c2927149";
pub(crate) const OLD_CONTENT_CUTOFF: &str = "2000-01-01";
pub(crate) const DEFAULT_EXCLUDED_TYPES: &[&str] = &["EV"];
pub(crate) const EDITED_PRIVATE_ADVICE_LABEL: &str = "Edited_private_advice";
pub(crate) const LEGISLATION_TYPE: &str = "Legislation_and_supporting_material";
pub(crate) const LEGISLATION_TYPE_PREFIXES: &[&str] = &["PAC", "REG", "RPC", "RRG"];
pub(crate) const STATUTORY_DEFINITION_TYPE_PREFIXES: &[&str] = &["PAC", "REG"];
pub(crate) const OEWN_2024_URL: &str = "https://en-word.net/static/english-wordnet-2024.zip";
pub(crate) const OEWN_2024_SOURCE: &str = "Open English WordNet 2024 (CC-BY 4.0)";
pub(crate) const ORDINARY_DICTIONARY_PATH_ENV: &str = "LEGAL_MCP_DICTIONARY_PATH";
/// Single corpus version this binary supports. Stamped into both
/// `meta.schema_version` in the SQLite DB and `schema_version` in the
/// published manifest.json — the two were previously distinct numbers that
/// always moved together; now they share one identifier. Bump on any
/// breaking change to the on-disk layout or the manifest contract.
pub(crate) const SUPPORTED_SCHEMA_VERSION: u32 = 10;
pub(crate) const EMBEDDING_MODEL_ID: &str = "granite-embedding-small-r2-fp16-256d";

/// Compile-time switch: corpus build and runtime semantic search use the
/// CUDA execution provider when this binary was built with `--features cuda`,
/// otherwise CPU. There is no runtime override — the build flavour IS the
/// switch.
pub(crate) const USE_GPU: bool = cfg!(feature = "cuda");
pub(crate) const DEFAULT_MAX_PER_DOC: usize = 2;
pub(crate) const HARD_MAX_PER_DOC: usize = 3;
const HTTP_WORKERS: usize = 8;
const HTTP_QUEUE_CAPACITY: usize = 32;
const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;

#[cfg(test)]
static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
struct TestEnvironment {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

#[cfg(test)]
impl TestEnvironment {
    fn set(values: &[(&'static str, &std::ffi::OsStr)]) -> Self {
        let lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let previous = values
            .iter()
            .map(|(name, _)| (*name, std::env::var_os(name)))
            .collect();
        let environment = Self {
            _lock: lock,
            previous,
        };
        for (name, value) in values {
            std::env::set_var(name, value);
        }
        environment
    }
}

#[cfg(test)]
impl Drop for TestEnvironment {
    fn drop(&mut self) {
        for (name, previous) in self.previous.iter().rev() {
            match previous {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

#[derive(Parser)]
#[command(
    name = "legal-mcp",
    version,
    about = "Standalone Australian legal MCP server"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceWorkspaceArg {
    source: String,
    path: PathBuf,
}

impl std::str::FromStr for SourceWorkspaceArg {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (source, path) = value
            .split_once('=')
            .ok_or_else(|| "expected SOURCE=PATH".to_string())?;
        if source.is_empty() || source.trim() != source {
            return Err(
                "source identifier must be nonempty and contain no surrounding whitespace"
                    .to_string(),
            );
        }
        if path.is_empty() {
            return Err("source workspace path must be nonempty".to_string());
        }
        Ok(Self {
            source: source.to_string(),
            path: PathBuf::from(path),
        })
    }
}

// One Rust binary owns both end-user commands and maintainer-only
// source/corpus commands; AGENTS.md documents which commands require the
// maintainer checkout, source corpus, model assets, and GPU.
// The external CLI is a closed clap enum: every command is explicit
// here, with no dynamic plugin subcommands or shell-completion surface.
#[derive(Subcommand)]
enum Command {
    /// Run the MCP stdio entry point. This is the preferred MCP-host command:
    /// it connects to one shared local HTTP server, starting that server in
    /// the background when needed, so agent hosts share one loaded semantic
    /// runtime.
    Mcp {},
    /// Run the HTTP MCP backend in the foreground. `legal-mcp mcp` starts this
    /// automatically for MCP hosts; use `serve` directly for manual HTTP
    /// testing. Override the port with `--port` for
    /// testing or to force a specific binding.
    Serve {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, hide = true)]
        ready_stdout: bool,
    },
    /// Download or refresh the complete local corpus generation.
    Update {
        /// Install from an explicit manifest URL or local manifest path.
        /// Primarily used for verified offline bundles and release testing.
        #[arg(long)]
        manifest_url: Option<String>,
    },
    /// Print index version, counts, and search-policy status (JSON).
    Stats {},
    /// Run a search from the CLI.
    Search {
        query: String,
        /// Legal source identifier. Defaults to `ato`; discover available
        /// identifiers from the search tool schema and `stats`.
        #[arg(long)]
        source: Option<SourceId>,
        #[arg(short, long, default_value_t = DEFAULT_K)]
        k: usize,
        /// Exact corpus type codes or `*` globs; discover codes with `legal-mcp stats`.
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
        /// Include content dated before 2000-01-01 (matches MCP `include_old`).
        #[arg(long)]
        include_old: bool,
        /// Runtime-embed this text as the query vector instead of `query`
        /// (e.g. a chunk from `fetch`). Forces vector-only mode and returns
        /// no title hits. Matches MCP `seed_text`.
        #[arg(long)]
        seed_text: Option<String>,
    },
    /// Live-fetch a document by canonical legal URI.
    /// Example: `legal://ato/JUD%2F2025ATC20-969%2F00002`.
    /// The cleaned HTML is chunked the same way the build pipeline chunks
    /// corpus docs, so live docs read like corpus docs.
    Fetch { uri: String },
    /// In-binary build orchestrator. Reads `pages_dir/index.jsonl` (one
    /// record per line with canonical_id, payload_path, sha256, and size), runs each doc
    /// through the cleaning pipeline, the chunker, the rules-engine
    /// metadata classifier, and the Granite embedder in-process, then writes
    /// documents, chunks, embeddings, FTS, anchors, definitions and citations
    /// and emits manifest.json
    /// into --out-dir. Supports same-output-dir checkpoint resume.
    /// Source refresh and corpus build are separate commands, so a validated
    /// source workspace can feed repeated builds or release dry runs.
    Build {
        #[arg(long)]
        pages_dir: PathBuf,
        /// Federal Register workspace populated by `source-update`.
        #[arg(long)]
        frl_workspace: PathBuf,
        #[arg(long)]
        db_path: PathBuf,
        /// Granite embedding model checkout. Must contain tokenizer.json,
        /// onnx/model_fp16.onnx, and onnx/model_fp16.onnx_data.
        #[arg(long)]
        model_dir: PathBuf,
        /// Optional completed schema-v10 legal.db whose model-keyed embedding
        /// cache seeds this otherwise fresh generation.
        #[arg(long)]
        embedding_cache_db: Option<PathBuf>,
        /// Output directory for manifest.json, legal.db, and source ANN sidecars.
        #[arg(long)]
        out_dir: PathBuf,
        #[arg(long, default_value_t = 3)]
        zstd_level: i32,
        /// Print cumulative build-stage timings to stderr.
        #[arg(long)]
        profile: bool,
    },
    /// Incrementally refresh one or more source workspaces in parallel. Each
    /// source uses its incremental discovery strategy, request pacing,
    /// concurrency and timeout policy.
    SourceUpdate {
        /// Repeat as SOURCE=PATH. The existing ATO workspace can be supplied
        /// directly, for example `--workspace ato=../ato_pages`.
        #[arg(long = "workspace", required = true, value_name = "SOURCE=PATH")]
        workspaces: Vec<SourceWorkspaceArg>,
        /// New directory in which source-specific discovery inventories and
        /// provenance are retained for this run.
        #[arg(long)]
        run_dir: PathBuf,
    },
    /// Perform a bounded live validation of the official FRL API, one current
    /// title, one rendition page and one normalized document.
    FrlProbe,
    /// Fetch compact statutory definitions for a term.
    GetDefinition {
        term: String,
        #[arg(long)]
        source: Option<SourceId>,
        #[arg(long)]
        context_document: Option<DocumentId>,
        #[arg(long, default_value_t = 5)]
        max_defs: usize,
    },
    /// Crawl the ATO browse-content tree and write nodes.jsonl + meta.json
    /// to a snapshot directory that preserves hierarchy and canonical links.
    //
    // Maintainer source modes: whats-new + scrape-diff for incremental
    // pulls, tree-crawl + snapshot-reduce for full snapshots, and scrape-diff
    // over deduped links for catch-up gaps.
    TreeCrawl {
        #[arg(long, default_value = "Mode=type&Action=initialise")]
        root_query: String,
        #[arg(long)]
        out_dir: PathBuf,
        #[arg(
            long,
            default_value = "https://www.ato.gov.au/API/v1/law/lawservices/browse-content/"
        )]
        base_url: String,
        #[arg(long, default_value_t = 30.0)]
        timeout_seconds: f64,
        #[arg(long, default_value_t = 0.05)]
        request_interval_seconds: f64,
        #[arg(long)]
        max_nodes: Option<usize>,
    },
    /// Reduce a snapshot into deterministic deduplicated links, summary,
    /// redundant paths, and skipped data URLs.
    SnapshotReduce {
        #[arg(long)]
        nodes_path: PathBuf,
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
    /// Maintainer source download defaults to 0.05s request pacing
    /// and four link-download workers; the rate lock serializes HTTP issuance.
    /// Download deduplicated ATO links to immutable SHA-256-named payloads in
    /// the source hierarchy and commit integrity-pinned index.jsonl records.
    LinkDownload {
        #[arg(long)]
        deduped_links: PathBuf,
        #[arg(long)]
        out_dir: PathBuf,
        #[arg(long, default_value = "https://www.ato.gov.au")]
        base_url: String,
        #[arg(long, default_value_t = 0.05)]
        request_delay_seconds: f64,
        #[arg(long, default_value_t = 4)]
        max_workers: usize,
        #[arg(long, default_value_t = 30.0)]
        timeout_seconds: f64,
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Compute the subset of dedup-style link records not already present
    /// in an existing index.jsonl. Used by maintainer-sync.sh for incremental
    /// (--whats-new-url) and catch-up (--deduped from snapshot-reduce) runs.
    ScrapeDiff {
        /// Existing payloads index.jsonl. Each line has canonical_id;
        /// any link already present here is skipped.
        #[arg(long)]
        index: PathBuf,
        /// Source A (catch-up): a deduped_links.jsonl from snapshot-reduce.
        #[arg(long)]
        deduped: Option<PathBuf>,
        /// Source B (incremental): pull What's New entries live and use them.
        #[arg(long)]
        whats_new_url: Option<String>,
        /// Optional path-prefix segments prepended to each link's
        /// representative_path. Used for scoped catch_up runs that don't
        /// start from the absolute root.
        #[arg(long)]
        path_prefix: Option<String>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Strip FTS5 indexes off a copy of legal.db, VACUUM, and zstd-compress
    /// it into the shippable legal.db.zst artifact.
    /// Emits {path, sha256, size} JSON on stdout so the release writer can
    /// embed those into manifest.json. If --manifest is set, updates that
    /// manifest in-place to point `db: {url, sha256, size}` at the new artifact
    /// (URL = bare filename; publish-release rewrites it to a GitHub release URL
    /// later).
    PackageCorpus {
        /// Path to the canonical built legal.db (e.g. release/<tag>/legal.db).
        #[arg(long)]
        db_path: PathBuf,
        /// Output path for the compressed artifact (e.g. release/<tag>/legal.db.zst).
        #[arg(long)]
        out: PathBuf,
        /// zstd compression level. 19 maximises ratio; 3 is faster but bigger.
        #[arg(long, default_value_t = 19)]
        level: i32,
        /// Optional: update this manifest's `db` field in place with the new
        /// artifact's SHA-256 and size.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
    /// Publish a corpus release to GitHub: finalize and validate remote artifact
    /// URLs, create a non-latest draft, upload the database, source ANNs and
    /// model artifacts, upload manifest.json last, then publish the draft.
    PublishRelease {
        #[arg(long)]
        out_dir: PathBuf,
        #[arg(long)]
        tag: String,
        /// Target GitHub repository. Defaults to `gunba/australian-legal-mcp`.
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        /// Override the embedding-model URL recorded in the manifest. Use
        /// this when hosting the model bundle outside HuggingFace.
        #[arg(long)]
        model_url: Option<String>,
        /// SHA256 hex of the embedding-model bundle when --model-url is used.
        #[arg(long)]
        model_sha256: Option<String>,
        /// Size in bytes of the embedding-model bundle when --model-url is used.
        #[arg(long)]
        model_size: Option<u64>,
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
        Command::Mcp {} => serve_stdio_mcp(),
        Command::Serve {
            port,
            bind,
            ready_stdout,
        } => {
            let choice = config::resolve_serve_port(port)?;
            let update_notice = resolve_startup_update_notice();
            let cached_instructions = server_instructions(update_notice.as_ref());
            let state = ServerState {
                cached_instructions,
                ..Default::default()
            };
            serve(choice, &bind, ready_stdout, Arc::new(state))
        }
        Command::Update { manifest_url } => {
            let manifest_url = match manifest_url {
                Some(value) => value,
                None => resolve_latest_corpus_manifest_url()?,
            };
            let stats = apply_update(&manifest_url)?;
            println!(
                "update complete ({:.2} MB downloaded)",
                stats.bytes_downloaded as f64 / 1_000_000.0,
            );
            Ok(())
        }
        Command::Stats {} => {
            println!("{}", stats()?);
            Ok(())
        }
        Command::Search {
            query,
            source,
            k,
            types,
            date_from,
            date_to,
            doc_scope,
            mode,
            sort_by,
            include_old,
            seed_text,
        } => {
            let types = empty_vec_as_none(types);
            let resolved_source =
                source_registry().resolve(source.as_ref().map(SourceId::as_str))?;
            // Construct a transient ServerState so the CLI's `search` call
            // reuses the same lazy semantic runtime the MCP server does for
            // modes that need it.
            let (out, _state) = search_cli(
                &query,
                SearchOptions {
                    source: resolved_source,
                    k,
                    types: types.as_deref(),
                    date_from: date_from.as_deref(),
                    date_to: date_to.as_deref(),
                    doc_scope: doc_scope.as_deref(),
                    mode,
                    sort_by,
                    include_old,
                    current_only: true,
                    max_per_doc: DEFAULT_MAX_PER_DOC,
                    include_snippet: true,
                    similar_to_chunk: None,
                    seed_text: seed_text.as_deref(),
                },
            )?;
            println!("{}", out);
            Ok(())
        }
        Command::GetDefinition {
            term,
            source,
            context_document,
            max_defs,
        } => {
            println!(
                "{}",
                get_definition(
                    &term,
                    GetDefinitionOptions {
                        source,
                        context_document,
                        max_defs,
                    },
                )?
            );
            Ok(())
        }
        Command::PublishRelease {
            out_dir,
            tag,
            repo,
            title,
            notes,
            model_url,
            model_sha256,
            model_size,
        } => publish_release(PublishReleaseArgs {
            out_dir,
            tag,
            repo,
            title,
            notes,
            model_url,
            model_sha256,
            model_size,
        }),
        Command::TreeCrawl {
            root_query,
            out_dir,
            base_url,
            timeout_seconds,
            request_interval_seconds,
            max_nodes,
        } => tree_crawl(
            &root_query,
            &out_dir,
            &base_url,
            timeout_seconds,
            request_interval_seconds,
            max_nodes,
        ),
        Command::SnapshotReduce {
            nodes_path,
            out_dir,
        } => snapshot_reduce(&nodes_path, out_dir.as_deref()),
        Command::LinkDownload {
            deduped_links,
            out_dir,
            base_url,
            request_delay_seconds,
            max_workers,
            timeout_seconds,
            force,
        } => link_download(LinkDownloadArgs {
            deduped_links,
            out_dir,
            base_url,
            request_delay_seconds,
            max_workers,
            timeout_seconds,
            force,
            workspace_lock_held: false,
        })
        .map(|_| ()),
        Command::ScrapeDiff {
            index,
            deduped,
            whats_new_url,
            path_prefix,
            out,
        } => scrape_diff(
            &index,
            deduped.as_deref(),
            whats_new_url.as_deref(),
            path_prefix.as_deref(),
            &out,
        ),
        Command::PackageCorpus {
            db_path,
            out,
            level,
            manifest,
        } => {
            let summary = package_corpus(&db_path, &out, level)?;
            if let Some(path) = manifest {
                update_manifest_with_db(&path, &out, &summary)?;
            }
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        Command::Fetch { uri } => {
            validate_fetch_uri(&uri)?;
            println!("{}", fetch(&uri)?);
            Ok(())
        }
        Command::Build {
            pages_dir,
            frl_workspace,
            db_path,
            model_dir,
            embedding_cache_db,
            out_dir,
            zstd_level,
            profile,
        } => build_corpus(BuildCorpusArgs {
            pages_dir: &pages_dir,
            frl_workspace: &frl_workspace,
            db_path: &db_path,
            model_dir: &model_dir,
            embedding_cache_db: embedding_cache_db.as_deref(),
            out_dir: &out_dir,
            zstd_level,
            profile_enabled: profile,
        }),
        Command::SourceUpdate {
            workspaces,
            run_dir,
        } => {
            let registry = source_registry();
            let requests = workspaces
                .into_iter()
                .map(|workspace| {
                    let source = registry.resolve(Some(&workspace.source))?;
                    let source_run_dir = run_dir.join(source.as_str());
                    Ok(SourceUpdateRequest {
                        source,
                        workspace: workspace.path,
                        run_dir: source_run_dir,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let outcomes = run_source_updates(requests)?;
            let succeeded = outcomes.iter().all(|outcome| outcome.is_success());
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "sources": outcomes }))?
            );
            if !succeeded {
                eprintln!(
                    "legal-mcp source-update: one or more sources retained their last committed state"
                );
            }
            Ok(())
        }
        Command::FrlProbe => {
            println!(
                "{}",
                serde_json::to_string_pretty(&frl::probe_official_source()?)?
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

struct ServerState {
    semantic_runtime: Mutex<Option<SemanticRuntime>>,
    semantic_model_paths: Option<SemanticModelPaths>,
    corpus_generation: Option<String>,
    // Rendered once at server start so MCP initialize is a cheap field read
    // instead of re-running stats() (~5-10s on a cold 4 GB corpus) per call.
    // The corpus is immutable for the server lifetime — `legal-mcp update`
    // requires a restart — so a single render is correct. Carries the
    // startup update-notice via server_instructions(); no separate field
    // is needed because nothing else reads it.
    cached_instructions: String,
}

impl ServerState {
    fn new() -> Self {
        Self {
            semantic_runtime: Mutex::new(None),
            semantic_model_paths: None,
            corpus_generation: config::active_generation_key().ok().flatten(),
            cached_instructions: String::new(),
        }
    }

    fn with_model_paths(semantic_model_paths: SemanticModelPaths) -> Self {
        Self {
            semantic_runtime: Mutex::new(None),
            semantic_model_paths: Some(semantic_model_paths),
            corpus_generation: config::active_generation_key().ok().flatten(),
            cached_instructions: String::new(),
        }
    }

    fn encode_query_embedding(&self, query: &str) -> Result<[i8; EMBEDDING_DIM]> {
        let mut embeddings = self.encode_query_embeddings(&[query.to_string()])?;
        embeddings
            .pop()
            .ok_or_else(|| anyhow!("semantic encoder returned no query embedding"))
    }

    fn ensure_corpus_generation_unchanged(&self) -> Result<()> {
        let active = config::active_generation_key()?;
        if active != self.corpus_generation {
            bail!(
                "the installed corpus changed after this process started; restart the australian-legal-mcp backend"
            );
        }
        Ok(())
    }

    fn encode_query_embeddings(&self, queries: &[String]) -> Result<Vec<[i8; EMBEDDING_DIM]>> {
        let (embeddings, _stats) = self.encode_query_embeddings_with_stats(queries)?;
        Ok(embeddings)
    }

    fn count_embedding_tokens(&self, text: &str) -> Result<usize> {
        let mut guard = self
            .semantic_runtime
            .lock()
            .expect("semantic_runtime mutex");
        if guard.is_none() {
            let model_paths = match &self.semantic_model_paths {
                Some(paths) => paths.clone(),
                None => SemanticModelPaths::live()?,
            };
            *guard = Some(SemanticRuntime::load(USE_GPU, &model_paths)?);
        }
        guard
            .as_ref()
            .expect("semantic runtime was just initialized")
            .count_tokens(text)
    }

    fn encode_query_embeddings_with_stats(
        &self,
        queries: &[String],
    ) -> Result<(Vec<[i8; EMBEDDING_DIM]>, SemanticEncodeStats)> {
        let mut guard = self
            .semantic_runtime
            .lock()
            .expect("semantic_runtime mutex");
        if guard.is_none() {
            // ServerState lazily loads SemanticRuntime on the first
            // semantic query and reuses it for the process lifetime.
            let model_paths = match &self.semantic_model_paths {
                Some(paths) => paths.clone(),
                None => SemanticModelPaths::live()?,
            };
            *guard = Some(SemanticRuntime::load(USE_GPU, &model_paths)?);
        }
        guard
            .as_mut()
            .expect("semantic runtime was just initialized")
            .encode_queries_with_stats(queries)
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

// ----- External fetch (live ATO source retrieval) -----
//
// Live retrieval selects the legal-content container, strips navigation noise
// and history controls, then renders readable text for [doc:X] targets absent
// from the local corpus, including subdivisions, paragraph references,
// footnotes, and historical PiT views.

pub(crate) const ATO_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const ATO_USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; australian-legal-mcp/",
    env!("CARGO_PKG_VERSION"),
    "; +https://github.com/gunba/australian-legal-mcp)"
);

// ----- Deterministic GitHub corpus release publication -----

pub(crate) const EMBEDDING_MODEL_HF_URL: &str =
    "hf://onnx-community/granite-embedding-small-english-r2-ONNX@1dc7835ba0cb9c76a3618d0bf0c427c97671b3c8";
pub(crate) const EMBEDDING_MODEL_HF_SIZE: u64 = 99_732_286;

struct PublishReleaseArgs {
    out_dir: PathBuf,
    tag: String,
    repo: Option<String>,
    title: Option<String>,
    notes: Option<String>,
    model_url: Option<String>,
    model_sha256: Option<String>,
    model_size: Option<u64>,
}

fn is_placeholder_model_url(url: &str) -> bool {
    let u = url.trim();
    u.is_empty()
        || u == "PENDING"
        || u.contains("releases/download/PENDING")
        || u.contains("placeholder")
}

fn is_github_url(url: &str) -> bool {
    url.starts_with("https://github.com/") || url.starts_with("http://github.com/")
}

fn is_hf_http_url(url: &str) -> bool {
    url.starts_with("https://huggingface.co/") || url.starts_with("http://huggingface.co/")
}

fn is_hf_scheme_url(url: &str) -> bool {
    url.starts_with("hf://")
}

fn is_hf_model_source(url: &str) -> bool {
    parse_hf_model_url(url).is_some()
}

fn non_empty_model_sha(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn resolve_publish_model_info(
    current: &ModelInfo,
    model_url: Option<&str>,
    model_sha256: Option<&str>,
    model_size: Option<u64>,
) -> Result<ModelInfo> {
    let mut resolved = current.clone();
    if current.id != EMBEDDING_MODEL_ID {
        return Ok(resolved);
    }

    let explicit_model_url = model_url.is_some();
    let mut resolved_url = model_url.unwrap_or(&current.url).to_string();
    let requested_sha = model_sha256.and_then(non_empty_model_sha);
    let requested_size = model_size.filter(|n| *n > 0);
    let manifest_sha = non_empty_model_sha(&current.sha256);
    let manifest_size = (current.size > 0).then_some(current.size);

    let needs_default = is_placeholder_model_url(&resolved_url)
        || (is_github_url(&resolved_url) && !explicit_model_url);

    let (sha256, size) = if needs_default {
        resolved_url = EMBEDDING_MODEL_HF_URL.to_string();
        (
            EMBEDDING_MODEL_FINGERPRINT.to_string(),
            EMBEDDING_MODEL_HF_SIZE,
        )
    } else if is_hf_model_source(&resolved_url) {
        if requested_sha
            .as_deref()
            .is_some_and(|sha| sha != EMBEDDING_MODEL_FINGERPRINT)
        {
            bail!("Hugging Face semantic model sha256 must match the pinned Granite fingerprint");
        }
        if requested_size.is_some_and(|size| size != EMBEDDING_MODEL_HF_SIZE) {
            bail!("Hugging Face semantic model size must match the pinned Granite file set");
        }
        if manifest_sha
            .as_deref()
            .is_some_and(|sha| sha != EMBEDDING_MODEL_FINGERPRINT)
        {
            bail!("Hugging Face semantic model sha256 must match the pinned Granite fingerprint");
        }
        if manifest_size.is_some_and(|size| size != EMBEDDING_MODEL_HF_SIZE) {
            bail!("Hugging Face semantic model size must match the pinned Granite file set");
        }
        (
            EMBEDDING_MODEL_FINGERPRINT.to_string(),
            EMBEDDING_MODEL_HF_SIZE,
        )
    } else {
        if is_hf_scheme_url(&resolved_url) {
            bail!("Hugging Face semantic model sources must use hf://repo@revision with an explicit revision");
        }
        if is_hf_http_url(&resolved_url) {
            bail!("Hugging Face semantic model sources must use hf://repo@revision, not HTTPS model URLs");
        }
        if is_github_url(&resolved_url) {
            bail!("semantic model bundles must not be hosted on GitHub");
        }
        if explicit_model_url && (requested_sha.is_none() || requested_size.is_none()) {
            bail!(
                "non-Hugging Face semantic model mirrors require --model-sha256 and --model-size"
            );
        }
        (
            requested_sha.or(manifest_sha).unwrap_or_default(),
            requested_size.or(manifest_size).unwrap_or(0),
        )
    };

    if sha256.is_empty() || size == 0 {
        bail!("semantic model releases require model sha256 and size");
    }
    resolved.sha256 = sha256;
    resolved.size = size;
    resolved.url = resolved_url;
    Ok(resolved)
}

fn publish_release(args: PublishReleaseArgs) -> Result<()> {
    let manifest_path = args.out_dir.join("manifest.json");
    if !manifest_path.exists() {
        bail!("no manifest at {}", manifest_path.display());
    }

    let repo = args
        .repo
        .clone()
        .or_else(|| std::env::var("LEGAL_MCP_GH_REPO").ok())
        .unwrap_or_else(|| DEFAULT_GITHUB_REPOSITORY.to_string());

    // Load manifest, fix model fields if necessary.
    let raw = fs::read_to_string(&manifest_path)?;
    let mut manifest: Manifest = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;

    manifest.model = resolve_publish_model_info(
        &manifest.model,
        args.model_url.as_deref(),
        args.model_sha256.as_deref(),
        args.model_size,
    )?;

    // The release ships one legal.db.zst artifact. Rewrite manifest.db.url
    // to the GitHub release URL and queue the local file for upload.
    let mut artifacts: Vec<PathBuf> = Vec::new();
    let db_info = &mut manifest.db;
    let filename = std::path::Path::new(&db_info.url)
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| db_info.url.clone());
    let local = args.out_dir.join(&filename);
    if !local.exists() {
        bail!(
            "manifest.db points at {} but {} does not exist; run `legal-mcp package-corpus --manifest <path>` first",
            filename,
            local.display()
        );
    }
    if fs::metadata(&local)?.len() != db_info.size {
        bail!("manifest DB size does not match {}", local.display());
    }
    verify_sha256_file(&local, &db_info.sha256)?;
    db_info.url = format!(
        "https://github.com/{repo}/releases/download/{tag}/{filename}",
        tag = args.tag,
    );
    artifacts.push(local);

    for (source_id, ann_info) in &mut manifest.ann {
        let filename = Path::new(&ann_info.url)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("manifest ANN URL has no UTF-8 filename"))?
            .to_string();
        let local = args.out_dir.join(&ann_info.url);
        if !local.is_file() {
            bail!(
                "manifest ANN sidecar for source `{source_id}` points at missing {}",
                local.display()
            );
        }
        if fs::metadata(&local)?.len() != ann_info.size {
            bail!(
                "manifest ANN size for source `{source_id}` does not match {}",
                local.display()
            );
        }
        verify_sha256_file(&local, &ann_info.sha256)?;
        ann_info.url = format!(
            "https://github.com/{repo}/releases/download/{tag}/{filename}",
            tag = args.tag,
        );
        artifacts.push(local);
    }

    crate::source::validate_manifest(&manifest).context("validating finalized release manifest")?;

    // Keep the validated build manifest immutable. The release-specific copy
    // carries remote artifact URLs and is uploaded as the commit point.
    let pretty = serde_json::to_vec_pretty(&manifest)?;
    let publish_dir = tempfile::Builder::new()
        .prefix(".publish-")
        .tempdir_in(&args.out_dir)?;
    let published_manifest_path = publish_dir.path().join("manifest.json");
    fs::write(&published_manifest_path, &pretty)?;

    // Ensure release exists.
    let view_status = std::process::Command::new("gh")
        .args(["release", "view", &args.tag, "--repo", &repo])
        .status()
        .context("running gh release view (is the gh CLI installed?)")?;
    if !view_status.success() {
        eprintln!("creating release {}", args.tag);
        let mut create = std::process::Command::new("gh");
        create.args([
            "release",
            "create",
            &args.tag,
            "--repo",
            &repo,
            "--title",
            args.title.as_deref().unwrap_or(&args.tag),
            "--draft",
            "--latest=false",
        ]);
        if let Some(notes) = args.notes.as_deref() {
            create.args(["--notes", notes]);
        } else {
            create.arg("--generate-notes");
        }
        let st = create.status().context("running gh release create")?;
        if !st.success() {
            bail!("gh release create failed (exit {:?})", st.code());
        }
    }

    let assets = std::process::Command::new("gh")
        .args([
            "release",
            "view",
            &args.tag,
            "--repo",
            &repo,
            "--json",
            "assets",
            "--jq",
            ".assets[].name",
        ])
        .output()
        .context("listing existing release assets")?;
    if !assets.status.success() {
        bail!(
            "gh release asset listing failed (exit {:?})",
            assets.status.code()
        );
    }
    let has_remote_manifest = String::from_utf8(assets.stdout)?
        .lines()
        .any(|name| name == "manifest.json");
    let draft = std::process::Command::new("gh")
        .args([
            "release", "view", &args.tag, "--repo", &repo, "--json", "isDraft", "--jq", ".isDraft",
        ])
        .output()
        .context("reading release draft status")?;
    if !draft.status.success() {
        bail!(
            "gh release draft-status lookup failed (exit {:?})",
            draft.status.code()
        );
    }
    let is_draft = String::from_utf8(draft.stdout)?.trim() == "true";
    if has_remote_manifest {
        if is_draft {
            publish_corpus_release(&args.tag, &repo)?;
            return Ok(());
        }
        bail!("published corpus release {} is immutable", args.tag);
    }
    if !is_draft {
        bail!(
            "release {} is already public and has no corpus manifest; use a fresh corpus tag",
            args.tag
        );
    }

    // Publish immutable data first and the discoverable manifest last. Each
    // upload is a separate command so an earlier failure cannot expose a
    // manifest that references a missing database or ANN sidecar.
    for artifact in artifacts
        .iter()
        .chain(std::iter::once(&published_manifest_path))
    {
        let mut upload = std::process::Command::new("gh");
        upload.args(["release", "upload", &args.tag, "--repo", &repo]);
        upload.arg(artifact);
        let st = upload.status().context("running gh release upload")?;
        if !st.success() {
            bail!(
                "gh release upload failed for {} (exit {:?})",
                artifact.display(),
                st.code()
            );
        }
    }

    eprintln!(
        "legal-mcp publish-release: uploaded {} artifacts to {}@{}",
        artifacts.len() + 1,
        repo,
        args.tag,
    );
    publish_corpus_release(&args.tag, &repo)?;
    Ok(())
}

fn publish_corpus_release(tag: &str, repo: &str) -> Result<()> {
    let status = std::process::Command::new("gh")
        .args([
            "release",
            "edit",
            tag,
            "--repo",
            repo,
            "--draft=false",
            "--latest=false",
        ])
        .status()
        .context("publishing verified corpus release")?;
    if !status.success() {
        bail!(
            "gh release publication failed for {tag} (exit {:?})",
            status.code()
        );
    }
    eprintln!("legal-mcp publish-release: published {repo}@{tag}");
    Ok(())
}

fn embedding_model_marker_value(info: &ModelInfo) -> String {
    if parse_hf_model_url(&info.url).is_some() {
        EMBEDDING_MODEL_FINGERPRINT.to_string()
    } else {
        info.sha256.clone()
    }
}

pub(crate) fn embedding_model_installed_matches(info: &ModelInfo) -> Result<bool> {
    if info.id != EMBEDDING_MODEL_ID {
        return Ok(false);
    }
    let marker_value = embedding_model_marker_value(info);
    let marker = live_dir()?.join(".model.sha256");
    Ok(model_path()?.exists()
        && model_data_path()?.exists()
        && tokenizer_path()?.exists()
        && marker.exists()
        && fs::read_to_string(marker)?.trim() == marker_value)
}

#[derive(Clone)]
pub(crate) struct UrlContext {
    pub(crate) manifest_dir: Option<PathBuf>,
    pub(crate) manifest_base_url: Option<String>,
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

pub(crate) fn resolve_manifest_asset(asset_url: &str, context: &UrlContext) -> String {
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

pub(crate) fn local_path_from_urlish(value: &str) -> Option<PathBuf> {
    let bytes = value.as_bytes();
    let windows_absolute = (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/'))
        || value.starts_with(r"\\");
    let path = PathBuf::from(value);
    if windows_absolute || path.is_absolute() || path.exists() {
        return Some(path);
    }
    if let Ok(url) = Url::parse(value) {
        if url.scheme() == "file" {
            return url.to_file_path().ok();
        }
    }
    None
}

// Fetch helpers resolve local paths, file://, manifest-relative
// assets, HTTP(S), and hf:// Granite model files; downloaded bytes are
// sha256-verified against the manifest's pinned hash.
pub(crate) fn fetch_bytes(url_or_path: &str, context: &UrlContext) -> Result<Vec<u8>> {
    fetch_bytes_with(url_or_path, context, &http_client()?)
}

// Downloads resolve through the supplied local or remote source context.
pub(crate) fn fetch_bytes_with(
    url_or_path: &str,
    context: &UrlContext,
    client: &Client,
) -> Result<Vec<u8>> {
    if let Some(path) = local_path_from_urlish(url_or_path) {
        return Ok(fs::read(path)?);
    }
    if let Some(dir) = &context.manifest_dir {
        if let Some(name) = url_or_path.rsplit('/').next() {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Ok(fs::read(candidate)?);
            }
        }
        if !url_or_path.starts_with("http://") && !url_or_path.starts_with("https://") {
            bail!("release asset not found: {url_or_path}");
        }
    }
    let mut resp = client.get(url_or_path).send()?.error_for_status().with_context(|| {
        format!(
            "download failed for {url_or_path}; use a public release, an authenticated mirror, or a file URL"
        )
    })?;
    let mut out = Vec::new();
    resp.copy_to(&mut out)?;
    Ok(out)
}

pub(crate) fn validate_manifest_model_source(model: &ModelInfo) -> Result<()> {
    if model.id != EMBEDDING_MODEL_ID {
        return Ok(());
    }
    if parse_hf_model_url(&model.url).is_some() {
        if model.sha256 != EMBEDDING_MODEL_FINGERPRINT {
            bail!("Hugging Face semantic model sha256 must match the pinned Granite fingerprint");
        }
        if model.size != EMBEDDING_MODEL_HF_SIZE {
            bail!("Hugging Face semantic model size must match the pinned Granite file set");
        }
        return Ok(());
    }
    if is_hf_scheme_url(&model.url) {
        bail!(
            "Hugging Face semantic model sources must use hf://repo@revision with an explicit revision"
        );
    }
    if model.url.starts_with("https://huggingface.co/")
        || model.url.starts_with("http://huggingface.co/")
    {
        bail!("Hugging Face semantic model sources must use hf://repo@revision, not HTTPS URLs");
    }
    if model.sha256.trim().is_empty() || model.size == 0 {
        bail!("non-Hugging Face semantic model sources require sha256 and positive size");
    }
    Ok(())
}

pub(crate) fn parse_hf_model_url(value: &str) -> Option<(&str, &str)> {
    let spec = value.strip_prefix("hf://")?;
    let (repo, revision) = spec.split_once('@')?;
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

fn stage_model_artifact(
    value: &str,
    context: &UrlContext,
    destination: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<u64> {
    let resolved = resolve_manifest_asset(value, context);
    if let Some(path) = local_path_from_urlish(&resolved) {
        let path = path
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", path.display()))?;
        if let Some(root) = &context.manifest_dir {
            let root = root.canonicalize()?;
            if !path.starts_with(&root) {
                bail!("model artifact escaped {}", root.display());
            }
        }
        if !path.is_file() {
            bail!("model artifact is not a regular file: {}", path.display());
        }
        let size = fs::copy(&path, destination)?;
        if size != expected_size {
            let _ = fs::remove_file(destination);
            bail!("model artifact size mismatch: got {size}, expected {expected_size}");
        }
        if let Err(error) = verify_sha256_file(destination, expected_sha256) {
            let _ = fs::remove_file(destination);
            return Err(error);
        }
        return Ok(size);
    }

    source::download_approved_https_to_file(
        &resolved,
        destination,
        expected_size,
        expected_sha256,
        Duration::from_secs(60 * 60),
    )
}

pub(crate) fn stage_model(
    manifest: &Manifest,
    context: &UrlContext,
    staging: &Path,
) -> Result<Option<StagedModel>> {
    if manifest.model.id != EMBEDDING_MODEL_ID {
        bail!(
            "semantic search requires a {EMBEDDING_MODEL_ID} model bundle; manifest uses `{}`",
            manifest.model.id,
        );
    }
    validate_manifest_model_source(&manifest.model)?;
    let live_model = model_path()?;
    let live_model_data = model_data_path()?;
    let tokenizer = tokenizer_path()?;
    let marker = live_dir()?.join(".model.sha256");
    let marker_value = embedding_model_marker_value(&manifest.model);
    if live_model.exists()
        && live_model_data.exists()
        && tokenizer.exists()
        && marker.exists()
        && fs::read_to_string(&marker)?.trim() == marker_value
    {
        return Ok(None);
    }
    if staging.exists() {
        fs::remove_dir_all(staging)?;
    }
    fs::create_dir_all(staging)?;

    if let Some((repo, revision)) = parse_hf_model_url(&manifest.model.url) {
        stage_hf_embedding_model(repo, revision, staging)?;
        return Ok(Some(StagedModel {
            dir: staging.to_path_buf(),
            marker_value,
        }));
    }
    if is_hf_scheme_url(&manifest.model.url) {
        bail!(
            "Hugging Face semantic model sources must use hf://repo@revision with an explicit revision"
        );
    }

    let bundle = staging.join("model-bundle.tar.zst.part");
    stage_model_artifact(
        &manifest.model.url,
        context,
        &bundle,
        manifest.model.size,
        &manifest.model.sha256,
    )?;
    let extract_dir = staging.join("model-bundle-extracted");
    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)?;
    }
    fs::create_dir_all(&extract_dir)?;
    let bundle_file = File::open(&bundle)?;
    let decoder = zstd::stream::read::Decoder::new(bundle_file)?;
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(&extract_dir)?;

    for file in EMBEDDING_MODEL_HF_FILES {
        let path = extract_dir.join(file.output_name);
        if !path.is_file() {
            bail!("model bundle missing required file {}", file.output_name);
        }
    }
    for file in EMBEDDING_MODEL_HF_FILES {
        let source = extract_dir.join(file.output_name);
        let dest = staging.join(file.output_name);
        fs::rename(source, dest)?;
    }
    let _ = fs::remove_file(bundle);
    let _ = fs::remove_dir_all(extract_dir);
    Ok(Some(StagedModel {
        dir: staging.to_path_buf(),
        marker_value,
    }))
}

fn stage_hf_embedding_model(repo: &str, revision: &str, staging: &Path) -> Result<()> {
    fs::create_dir_all(staging)?;
    let download_dir = staging.join("hf-model-download");
    if download_dir.exists() {
        fs::remove_dir_all(&download_dir)?;
    }
    fs::create_dir_all(&download_dir)?;
    for file in EMBEDDING_MODEL_HF_FILES {
        let url = hf_resolve_url(repo, revision, file.path);
        let part = download_dir.join(format!("{}.part", file.output_name));
        source::download_approved_https_to_file(
            &url,
            &part,
            file.size,
            file.sha256,
            Duration::from_secs(60 * 60),
        )
        .with_context(|| format!("downloading Hugging Face model file {}", file.path))?;
        fs::rename(part, download_dir.join(file.output_name))?;
    }
    for file in EMBEDDING_MODEL_HF_FILES {
        let source = download_dir.join(file.output_name);
        let dest = staging.join(file.output_name);
        fs::rename(source, dest)?;
    }
    let _ = fs::remove_dir_all(download_dir);
    Ok(())
}

fn serve(
    choice: config::PortChoice,
    bind: &str,
    ready_stdout: bool,
    state: Arc<ServerState>,
) -> Result<()> {
    if bind != "127.0.0.1" {
        bail!(
            "HTTP MCP transport is loopback-only; --bind must be the canonical address 127.0.0.1"
        );
    }
    let requested_port = match &choice {
        config::PortChoice::Cli(p) => *p,
        config::PortChoice::PluginUnchanged(p) => *p,
        config::PortChoice::PluginNeedsRewrite { port, .. } => *port,
    };
    let addr = format!("{bind}:{requested_port}");
    let server = tiny_http::Server::http(&addr).map_err(|e| {
        anyhow!(
            "bind {addr}: {e}. If the port is in use, start with `legal-mcp serve --port <free-port>`."
        )
    })?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("HTTP server did not bind a TCP address"))?
        .port();
    let url = config::server_url(port);
    write_http_state(port, &url)?;
    emit_ready_line(&url, ready_stdout)?;
    if let config::PortChoice::PluginNeedsRewrite { mcp_json, .. } = &choice {
        match config::update_plugin_mcp_json_url(mcp_json, &url) {
            Ok(true) => {
                eprintln!(
                    "legal-mcp wrote the new URL to {}; exit and resume your Claude Code session for it to take effect.",
                    mcp_json.display()
                );
            }
            Ok(false) => {}
            Err(err) => {
                eprintln!(
                    "legal-mcp: warning: could not update {}: {err}. Update its `url` to {url} manually and restart your MCP client.",
                    mcp_json.display()
                );
            }
        }
    }

    let (sender, receiver) = mpsc::sync_channel::<tiny_http::Request>(HTTP_QUEUE_CAPACITY);
    let receiver = Arc::new(Mutex::new(receiver));
    for worker in 0..HTTP_WORKERS {
        let receiver = Arc::clone(&receiver);
        let state = Arc::clone(&state);
        std::thread::Builder::new()
            .name(format!("legal-mcp-http-{worker}"))
            .spawn(move || loop {
                let request = {
                    let receiver = receiver.lock().unwrap_or_else(|err| err.into_inner());
                    receiver.recv()
                };
                let Ok(request) = request else {
                    break;
                };
                if let Err(err) = handle_http(request, &state) {
                    eprintln!("legal-mcp http handler error: {err}");
                }
            })
            .context("starting bounded HTTP worker")?;
    }

    for request in server.incoming_requests() {
        match sender.try_send(request) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(request)) => {
                let response = tiny_http::Response::from_string(
                    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"server busy"}}"#,
                )
                .with_status_code(503)
                .with_header(json_content_type());
                let _ = request.respond(response);
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                bail!("all HTTP workers stopped unexpectedly")
            }
        }
    }
    Ok(())
}

fn emit_ready_line(url: &str, ready_stdout: bool) -> Result<()> {
    if ready_stdout {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "legal-mcp listening on {url}")?;
        stdout.flush()?;
    } else {
        eprintln!("legal-mcp listening on {url}");
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpState {
    port: u16,
    url: String,
}

fn write_http_state(port: u16, url: &str) -> Result<()> {
    if port == 0 || url != config::server_url(port) {
        bail!("refusing to advertise noncanonical HTTP MCP endpoint `{url}`");
    }
    let path = config::http_state_path()?;
    let state = HttpState {
        port,
        url: url.to_string(),
    };
    let serialised = format!("{}\n", serde_json::to_string_pretty(&state)?);
    config::atomic_write(&path, serialised.as_bytes())?;
    Ok(())
}

fn read_http_state() -> Result<Option<HttpState>> {
    let path = config::http_state_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let state: HttpState = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if state.port == 0 || state.url != config::server_url(state.port) {
        return Ok(None);
    }
    Ok(Some(state))
}

fn serve_stdio_mcp() -> Result<()> {
    let mut url = ensure_http_server()?;
    let client = mcp_http_client(Duration::from_secs(300))?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

    for line in stdin.lock().lines() {
        let line = line.context("reading MCP stdio message")?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match post_mcp_line(&client, &url, &line) {
            Ok(response) => response,
            Err(first_err) => {
                url = ensure_http_server().with_context(|| {
                    format!(
                        "restarting local australian-legal-mcp HTTP server after request failed: {first_err}"
                    )
                })?;
                post_mcp_line(&client, &url, &line)
                    .with_context(|| format!("forwarding MCP request to {url}"))?
            }
        };
        if let Some(value) = response {
            serde_json::to_writer(&mut stdout, &value)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

fn ensure_http_server() -> Result<String> {
    // Health probe must outlast a cold first initialize: stats() can take
    // 5-10 s the first time on a multi-GB corpus before the OS page cache
    // is warm. 30 s gives that headroom without exceeding any normal MCP
    // host startup timeout.
    let health_client = mcp_http_client(Duration::from_secs(30))?;
    if let Some(state) = read_http_state()? {
        if http_backend_ready(&health_client, &state.url) {
            return Ok(state.url);
        }
    }

    let _guard = config::server_lock_file()?;
    if let Some(state) = read_http_state()? {
        if http_backend_ready(&health_client, &state.url) {
            return Ok(state.url);
        }
    }

    let port = config::pick_free_port()?;
    let url = config::server_url(port);
    spawn_http_server(port, &url)?;
    write_http_state(port, &url)?;
    if !http_backend_ready(&health_client, &url) {
        bail!(
            "local australian-legal-mcp HTTP server printed readiness but did not answer initialize at {url}"
        );
    }
    Ok(url)
}

fn mcp_http_client(timeout: Duration) -> Result<Client> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(timeout)
        .build()?)
}

fn http_backend_ready(client: &Client, url: &str) -> bool {
    let message = r#"{"jsonrpc":"2.0","id":"legal-mcp-health","method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"legal-mcp-shim","version":"0"}}}"#;
    match post_mcp_line(client, url, message) {
        Ok(Some(value)) => {
            let server_info = value.pointer("/result/serverInfo");
            server_info
                .and_then(|info| info.get("name"))
                .and_then(JsonValue::as_str)
                == Some(APP_NAME)
                && server_info
                    .and_then(|info| info.get("version"))
                    .and_then(JsonValue::as_str)
                    == Some(env!("CARGO_PKG_VERSION"))
        }
        _ => false,
    }
}

fn spawn_http_server(port: u16, expected_url: &str) -> Result<()> {
    let exe = std::env::current_exe().context("resolving legal-mcp executable path")?;
    let log_path = config::server_log_path()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let stderr = log.try_clone()?;
    let mut child = ProcessCommand::new(exe)
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .arg("--ready-stdout")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("starting local australian-legal-mcp HTTP server")?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("local australian-legal-mcp HTTP server stdout was not piped"))?;
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let result = reader.read_line(&mut line).map(|read| (read, line));
        let _ = ready_sender.send(result);
    });
    let (read, line) = match ready_receiver.recv_timeout(Duration::from_secs(20)) {
        Ok(result) => {
            result.context("waiting for local australian-legal-mcp HTTP server readiness")?
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "timed out waiting for local australian-legal-mcp HTTP server readiness; see {}",
                log_path.display()
            );
        }
    };
    if read == 0 {
        let status = child.try_wait().ok().flatten();
        bail!(
            "local australian-legal-mcp HTTP server exited before readiness{}; see {}",
            status
                .map(|s| format!(" ({s})"))
                .unwrap_or_else(String::new),
            log_path.display()
        );
    }
    let expected_line = format!("legal-mcp listening on {expected_url}");
    if line.trim() != expected_line {
        bail!(
            "unexpected local australian-legal-mcp HTTP server readiness line `{}`; expected `{expected_line}`; see {}",
            line.trim(),
            log_path.display()
        );
    }
    Ok(())
}

fn post_mcp_line(client: &Client, url: &str, line: &str) -> Result<Option<JsonValue>> {
    let response = client
        .post(url)
        .header("content-type", "application/json")
        .body(line.to_string())
        .send()
        .with_context(|| format!("POST {url}"))?;
    let status = response.status();
    if status.as_u16() == 204 {
        return Ok(None);
    }
    let body = response
        .text()
        .with_context(|| format!("reading response from {url}"))?;
    if !status.is_success() {
        bail!("POST {url} returned HTTP {status}: {body}");
    }
    let value: JsonValue =
        serde_json::from_str(&body).with_context(|| format!("parsing response from {url}"))?;
    Ok(Some(value))
}

fn json_content_type() -> tiny_http::Header {
    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
}

fn handle_http(mut request: tiny_http::Request, state: &ServerState) -> Result<()> {
    use tiny_http::{Header, Method, Response};

    if request
        .remote_addr()
        .is_some_and(|address| !address.ip().is_loopback())
    {
        let resp = Response::from_string("forbidden").with_status_code(403);
        return request.respond(resp).map_err(|e| anyhow!("respond: {e}"));
    }

    let path = request.url().split('?').next().unwrap_or("/").to_string();
    let is_mcp = path == "/mcp" || path == "/mcp/";

    if !is_mcp {
        let resp = Response::from_string("not found").with_status_code(404);
        return request.respond(resp).map_err(|e| anyhow!("respond: {e}"));
    }
    if !matches!(request.method(), Method::Post) {
        let resp = Response::from_string("method not allowed")
            .with_status_code(405)
            .with_header(Header::from_bytes(&b"Allow"[..], &b"POST"[..]).unwrap());
        return request.respond(resp).map_err(|e| anyhow!("respond: {e}"));
    }

    let is_json = request.headers().iter().any(|header| {
        header.field.equiv("Content-Type")
            && header
                .value
                .as_str()
                .split(';')
                .next()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    });
    if !is_json {
        let resp = Response::from_string(serde_json::to_string(&json_rpc_error(
            JsonValue::Null,
            -32600,
            "content-type must be application/json",
        ))?)
        .with_status_code(415)
        .with_header(json_content_type());
        return request.respond(resp).map_err(|e| anyhow!("respond: {e}"));
    }

    if request
        .body_length()
        .is_some_and(|length| length > MAX_HTTP_BODY_BYTES)
    {
        let resp = Response::from_string(serde_json::to_string(&json_rpc_error(
            JsonValue::Null,
            -32600,
            "request body exceeds 1 MiB limit",
        ))?)
        .with_status_code(413)
        .with_header(json_content_type());
        return request.respond(resp).map_err(|e| anyhow!("respond: {e}"));
    }

    let mut body = Vec::new();
    request
        .as_reader()
        .take((MAX_HTTP_BODY_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .context("reading request body")?;
    if body.len() > MAX_HTTP_BODY_BYTES {
        let resp = Response::from_string(serde_json::to_string(&json_rpc_error(
            JsonValue::Null,
            -32600,
            "request body exceeds 1 MiB limit",
        ))?)
        .with_status_code(413)
        .with_header(json_content_type());
        return request.respond(resp).map_err(|e| anyhow!("respond: {e}"));
    }

    let response_json: Option<JsonValue> = match serde_json::from_slice::<JsonValue>(&body) {
        Ok(message) => handle_rpc(message, state),
        Err(err) => Some(json_rpc_error(
            JsonValue::Null,
            -32700,
            &format!("parse error: {err}"),
        )),
    };

    // MCP notifications (no id) produce no response; reply 204 so the
    // client knows the request was accepted.
    let Some(value) = response_json else {
        let resp = Response::from_string("").with_status_code(204);
        return request.respond(resp).map_err(|e| anyhow!("respond: {e}"));
    };

    let body = serde_json::to_string(&value)?;
    let resp = Response::from_string(body).with_header(json_content_type());
    request.respond(resp).map_err(|e| anyhow!("respond: {e}"))?;
    Ok(())
}

fn handle_rpc(message: JsonValue, state: &ServerState) -> Option<JsonValue> {
    if let Some(messages) = message.as_array() {
        if messages.is_empty() {
            return Some(json_rpc_error(
                JsonValue::Null,
                -32600,
                "invalid request: empty batch",
            ));
        }
        let responses: Vec<JsonValue> = messages
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

fn handle_single_rpc(message: JsonValue, state: &ServerState) -> Option<JsonValue> {
    let Some(object) = message.as_object() else {
        return Some(json_rpc_error(JsonValue::Null, -32600, "invalid request"));
    };
    if object.get("jsonrpc").and_then(JsonValue::as_str) != Some("2.0") {
        return Some(json_rpc_error(JsonValue::Null, -32600, "invalid request"));
    }
    let has_id = object.contains_key("id");
    let id = object.get("id").cloned().unwrap_or(JsonValue::Null);
    if has_id && !(id.is_null() || id.is_string() || id.is_number()) {
        return Some(json_rpc_error(
            JsonValue::Null,
            -32600,
            "invalid request id",
        ));
    }
    let Some(method) = object.get("method").and_then(JsonValue::as_str) else {
        return Some(json_rpc_error(JsonValue::Null, -32600, "invalid request"));
    };
    if object
        .get("params")
        .is_some_and(|params| !params.is_object() && !params.is_array())
    {
        return has_id.then(|| json_rpc_error(id, -32602, "invalid params"));
    }

    let result: std::result::Result<JsonValue, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": APP_NAME, "version": env!("CARGO_PKG_VERSION") },
            "instructions": state.cached_instructions.as_str(),
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_descriptors() })),
        "tools/call" => {
            let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
            match validate_tool_call(&params) {
                Ok(()) => Ok(call_tool(params, state).unwrap_or_else(|err| {
                    json!({
                        "content": [{ "type": "text", "text": err.to_string() }],
                        "isError": true
                    })
                })),
                Err(err) => Err((-32602, err.to_string())),
            }
        }
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    if !has_id {
        return None;
    }
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json_rpc_error(id, code, &message),
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

fn exact_object<'a>(
    value: &'a JsonValue,
    field: &str,
    required_fields: &[&str],
) -> Result<&'a serde_json::Map<String, JsonValue>> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("`{field}` must be an object"))?;
    for name in object.keys() {
        if !required_fields.contains(&name.as_str()) {
            bail!("unknown `{field}` field `{name}`");
        }
    }
    for name in required_fields {
        if !object.contains_key(*name) {
            bail!("`{field}` is missing required field `{name}`");
        }
    }
    Ok(object)
}

fn parse_document(value: &JsonValue, field: &str) -> Result<DocumentId> {
    exact_object(value, field, &["source", "native_id"])?;
    let decoded: DocumentId = serde_json::from_value(value.clone())
        .with_context(|| format!("invalid `{field}` document identity"))?;
    let document = DocumentId::new(decoded.source, decoded.native_id)?;
    source_registry().source(&document.source)?;
    Ok(document)
}

fn parse_chunk(value: &JsonValue, field: &str) -> Result<ChunkRef> {
    exact_object(value, field, &["generation", "source", "chunk_id"])?;
    let decoded: ChunkRef = serde_json::from_value(value.clone())
        .with_context(|| format!("invalid `{field}` chunk identity"))?;
    let chunk = ChunkRef::new(decoded.generation, decoded.source, decoded.chunk_id)?;
    source_registry().source(&chunk.source)?;
    Ok(chunk)
}

fn parse_asset(value: &JsonValue, field: &str) -> Result<AssetRef> {
    exact_object(value, field, &["source", "asset_id"])?;
    let decoded: AssetRef = serde_json::from_value(value.clone())
        .with_context(|| format!("invalid `{field}` asset identity"))?;
    let asset = AssetRef::new(decoded.source, decoded.asset_id)?;
    source_registry().source(&asset.source)?;
    Ok(asset)
}

fn parse_source(value: &JsonValue, field: &str) -> Result<SourceId> {
    let source: SourceId = serde_json::from_value(value.clone())
        .with_context(|| format!("invalid `{field}` source identifier"))?;
    source_registry().source(&source)?;
    Ok(source)
}

fn parse_definition_scope(
    args: &serde_json::Map<String, JsonValue>,
) -> Result<(Option<SourceId>, Option<DocumentId>)> {
    let source = args
        .get("source")
        .map(|value| parse_source(value, "source"))
        .transpose()?;
    let context_document = args
        .get("context_document")
        .map(|value| parse_document(value, "context_document"))
        .transpose()?;
    if let (Some(source), Some(context)) = (&source, &context_document) {
        if source != &context.source {
            bail!(
                "definition source `{source}` does not match context document source `{}`",
                context.source
            );
        }
    }
    Ok((source, context_document))
}

fn validate_fetch_uri(value: &str) -> Result<()> {
    let uri = crate::uri::parse_doc_uri(value)?;
    let (document, _, _) = uri.into_parts();
    source_registry().source(&document.source)?;
    Ok(())
}

fn validate_tool_call(params: &JsonValue) -> Result<()> {
    let params = params
        .as_object()
        .ok_or_else(|| anyhow!("tools/call params must be an object"))?;
    let name = params
        .get("name")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| anyhow!("tools/call params.name must be a string"))?;
    let empty = serde_json::Map::new();
    let args = match params.get("arguments") {
        Some(value) => value
            .as_object()
            .ok_or_else(|| anyhow!("tools/call params.arguments must be an object"))?,
        None => &empty,
    };
    for field in params.keys() {
        if !["name", "arguments"].contains(&field.as_str()) {
            bail!("unknown tools/call parameter `{field}`");
        }
    }
    let reject_unknown_args = |allowed: &[&str]| -> Result<()> {
        for field in args.keys() {
            if !allowed.contains(&field.as_str()) {
                bail!("unknown argument `{field}` for tool `{name}`");
            }
        }
        Ok(())
    };

    let require_string = |field: &str| -> Result<()> {
        match args.get(field) {
            Some(JsonValue::String(value)) if !value.is_empty() => Ok(()),
            Some(JsonValue::String(_)) => bail!("`{field}` must not be empty"),
            Some(_) => bail!("`{field}` must be a string"),
            None => bail!("missing required string argument `{field}`"),
        }
    };
    let optional_string = |field: &str| -> Result<()> {
        if args.get(field).is_none_or(JsonValue::is_string) {
            Ok(())
        } else {
            bail!("`{field}` must be a string")
        }
    };
    let optional_bool = |field: &str| -> Result<()> {
        if args.get(field).is_none_or(JsonValue::is_boolean) {
            Ok(())
        } else {
            bail!("`{field}` must be a boolean")
        }
    };
    let bounded_u64 = |field: &str, minimum: u64, maximum: Option<u64>| -> Result<()> {
        let Some(value) = args.get(field) else {
            return Ok(());
        };
        let value = value
            .as_u64()
            .ok_or_else(|| anyhow!("`{field}` must be a non-negative integer"))?;
        if value < minimum || maximum.is_some_and(|maximum| value > maximum) {
            bail!("`{field}` is outside the allowed range")
        }
        Ok(())
    };
    let enum_string = |field: &str, allowed: &[&str]| -> Result<()> {
        let Some(value) = args.get(field) else {
            return Ok(());
        };
        let value = value
            .as_str()
            .ok_or_else(|| anyhow!("`{field}` must be a string"))?;
        if !allowed.contains(&value) {
            bail!("`{field}` must be one of {}", allowed.join(", "))
        }
        Ok(())
    };
    let string_array = |field: &str| -> Result<()> {
        let Some(value) = args.get(field) else {
            return Ok(());
        };
        let values = value
            .as_array()
            .ok_or_else(|| anyhow!("`{field}` must be an array of strings"))?;
        if values.iter().any(|value| !value.is_string()) {
            bail!("`{field}` must be an array of strings")
        }
        Ok(())
    };
    let chunk_array = |field: &str| -> Result<()> {
        let Some(value) = args.get(field) else {
            bail!("missing required array argument `{field}`");
        };
        let values = value
            .as_array()
            .ok_or_else(|| anyhow!("`{field}` must be an array of chunk identity objects"))?;
        for value in values {
            parse_chunk(value, field)?;
        }
        Ok(())
    };

    match name {
        "search" => {
            reject_unknown_args(&[
                "query",
                "source",
                "k",
                "types",
                "date_from",
                "date_to",
                "doc_scope",
                "mode",
                "include_old",
                "current_only",
                "include_snippet",
                "sort_by",
                "seed_text",
                "similar_to_chunk",
            ])?;
            require_string("query")?;
            let resolved_source = match args.get("source") {
                Some(value) => parse_source(value, "source")?,
                None => source_registry().resolve(None)?,
            };
            bounded_u64("k", 1, Some(MAX_K as u64))?;
            string_array("types")?;
            for field in ["date_from", "date_to", "doc_scope", "seed_text"] {
                optional_string(field)?;
            }
            enum_string("mode", &["hybrid", "vector", "keyword"])?;
            enum_string("sort_by", &["relevance", "recency"])?;
            for field in ["include_old", "current_only", "include_snippet"] {
                optional_bool(field)?;
            }
            if let Some(value) = args.get("similar_to_chunk") {
                let chunk_ref = parse_chunk(value, "similar_to_chunk")?;
                if chunk_ref.source != resolved_source {
                    bail!(
                        "`similar_to_chunk` source `{}` does not match resolved search source `{resolved_source}`",
                        chunk_ref.source
                    );
                }
            }
        }
        "get_asset" => {
            reject_unknown_args(&["asset"])?;
            let asset = args
                .get("asset")
                .ok_or_else(|| anyhow!("missing required object argument `asset`"))?;
            parse_asset(asset, "asset")?;
        }
        "get_doc_anchors" => {
            reject_unknown_args(&["document"])?;
            let document = args
                .get("document")
                .ok_or_else(|| anyhow!("missing required object argument `document`"))?;
            parse_document(document, "document")?;
        }
        "get_chunks" => {
            reject_unknown_args(&["chunks", "before", "after", "max_chars", "cursor"])?;
            chunk_array("chunks")?;
            let chunk_count = args
                .get("chunks")
                .and_then(JsonValue::as_array)
                .map_or(0, Vec::len);
            if !(1..=100).contains(&chunk_count) {
                bail!("`chunks` must contain between 1 and 100 chunk identities");
            }
            bounded_u64("before", 0, Some(20))?;
            bounded_u64("after", 0, Some(20))?;
            bounded_u64("max_chars", 1, Some(200_000))?;
            optional_string("cursor")?;
        }
        "get_definition" => {
            reject_unknown_args(&["term", "source", "context_document", "max_defs"])?;
            require_string("term")?;
            parse_definition_scope(args)?;
            bounded_u64("max_defs", 1, Some(20))?;
        }
        "stats" => {
            reject_unknown_args(&[])?;
        }
        "fetch" => {
            reject_unknown_args(&["uri"])?;
            require_string("uri")?;
            validate_fetch_uri(
                args.get("uri")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| anyhow!("missing required string argument `uri`"))?,
            )?;
        }
        _ => bail!("unknown tool `{name}`"),
    }
    Ok(())
}

fn call_tool(params: JsonValue, state: &ServerState) -> Result<JsonValue> {
    let _corpus_lock = config::corpus_read_lock()?;
    state.ensure_corpus_generation_unchanged()?;
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("tools/call missing params.name"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if name == "get_asset" {
        let asset = parse_asset(
            args.get("asset")
                .ok_or_else(|| anyhow!("missing required object argument `asset`"))?,
            "asset",
        )?;
        let content = get_asset(asset)?;
        return Ok(json!({ "content": content, "isError": false }));
    }
    let text = match name {
        "search" => {
            let query = required_str(&args, "query")?;
            let types = optional_string_array(&args, "types")?;
            let resolved_source = match args.get("source") {
                Some(value) => parse_source(value, "source")?,
                None => source_registry().resolve(None)?,
            };
            let similar_to_chunk = args
                .get("similar_to_chunk")
                .map(|value| parse_chunk(value, "similar_to_chunk"))
                .transpose()?;
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
                    source: resolved_source,
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
                    similar_to_chunk,
                    seed_text: args.get("seed_text").and_then(|v| v.as_str()),
                },
                Some(state),
            )?
        }
        "get_doc_anchors" => {
            let document = parse_document(
                args.get("document")
                    .ok_or_else(|| anyhow!("missing required object argument `document`"))?,
                "document",
            )?;
            get_doc_anchors(document)?
        }
        "get_chunks" => {
            let chunks = args
                .get("chunks")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| anyhow!("missing required array argument `chunks`"))?
                .iter()
                .map(|value| parse_chunk(value, "chunks"))
                .collect::<Result<Vec<_>>>()?;
            get_chunks(
                chunks,
                GetChunksOptions {
                    before: optional_usize(&args, "before").unwrap_or(0),
                    after: optional_usize(&args, "after").unwrap_or(0),
                    max_chars: optional_usize(&args, "max_chars"),
                },
                args.get("cursor").and_then(JsonValue::as_str),
            )?
        }
        "get_definition" => {
            let term = required_str(&args, "term")?;
            let argument_map = args
                .as_object()
                .ok_or_else(|| anyhow!("tools/call arguments must be an object"))?;
            let (source, context_document) = parse_definition_scope(argument_map)?;
            get_definition(
                term,
                GetDefinitionOptions {
                    source,
                    context_document,
                    max_defs: optional_usize(&args, "max_defs").unwrap_or(5),
                },
            )?
        }
        "stats" => stats()?,
        "fetch" => {
            let uri = required_str(&args, "uri")?;
            validate_fetch_uri(uri)?;
            fetch(uri)?
        }
        _ => bail!("unknown tool: {name}"),
    };
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    }))
}

pub(crate) fn required_str<'a>(args: &'a JsonValue, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing required string argument `{name}`"))
}

pub(crate) fn optional_usize(args: &JsonValue, name: &str) -> Option<usize> {
    args.get(name).and_then(|v| v.as_u64()).map(|v| v as usize)
}

pub(crate) fn optional_bool(args: &JsonValue, name: &str) -> Option<bool> {
    args.get(name).and_then(|v| v.as_bool())
}

pub(crate) fn optional_string_array(args: &JsonValue, name: &str) -> Result<Option<Vec<String>>> {
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

const LEGAL_MCP_USE_INSTRUCTIONS: &str = r##"Use `search` first; hits carry typed `document` and generation-bound `chunk` identities accepted directly by follow-up tools. Call `get_chunks` for text and `get_doc_anchors` for navigation. `[doc:X]` and `[asset:X]` markers use canonical source-qualified public references; `[fetch:X]` uses a canonical `legal://` URI. For historical/withdrawn material set `current_only=false` and `include_old=true`."##;

/// Probe for a newer corpus index at startup. Runs
/// `check_for_update_availability` once and is folded directly into the
/// cached MCP `initialize` instructions by `server_instructions`, so the
/// agent can offer to run `legal-mcp update`.
fn resolve_startup_update_notice() -> Option<UpdateAvailability> {
    let manifest_url = resolve_latest_corpus_manifest_url_probe().ok()?;
    check_for_update_availability(&manifest_url).ok().flatten()
}

// Server instructions are built dynamically at start time from corpus
// stats so the agent sees up-to-date corpus shape without restart-time config.
// If stats cannot be read (corpus not yet installed), return a static
// install message asking the user to run `legal-mcp update`. When the startup
// probe found an UpdateAvailability, both branches append a newer-index notice.
fn server_instructions(update_notice: Option<&UpdateAvailability>) -> String {
    let body = match stats()
        .ok()
        .and_then(|s| serde_json::from_str::<JsonValue>(&s).ok())
    {
        Some(s) => format!(
            "ATO legal corpus. Documents: {}, chunks: {}. Index: {}. Default search excludes EV edited private advice, withdrawn rulings, and content dated before {} except legislation prefixes PAC/REG/RPC/RRG; override with current_only=false and include_old=true.\n\n{}",
            s["documents"].as_i64().unwrap_or(0),
            s["chunks"].as_i64().unwrap_or(0),
            s["index_version"].as_str().unwrap_or("?"),
            OLD_CONTENT_CUTOFF,
            LEGAL_MCP_USE_INSTRUCTIONS,
        ),
        None => format!(
            "The ATO corpus is not yet installed on this machine. Offer to run `legal-mcp update` for the user as a visible setup step (~1.5 GB, takes 5-10 min), then restart the MCP host/backend after the download completes.\n\n{}",
            LEGAL_MCP_USE_INSTRUCTIONS
        ),
    };
    match update_notice {
        Some(notice) => format!(
            "{body}\n\nAn updated ATO corpus generation is available (index {}). Ask whether to run `legal-mcp update` now or continue with the installed corpus; restart the MCP host/backend after updating.",
            notice.available_index_version
        ),
        None => body,
    }
}

fn tool_descriptors() -> JsonValue {
    // Seven MCP tools are exposed by tool_descriptors/call_tool:
    // search, get_chunks, get_asset, get_doc_anchors, get_definition, stats,
    // fetch.
    //   The surface stays small and explicit; unsupported tools fail through the
    //   normal tools/call error path.
    let registry = source_registry();
    let source_ids = registry.source_ids();
    let default_source = registry
        .resolve(None)
        .expect("source registry must contain its configured default");
    let source_schema = json!({"type": "string", "enum": source_ids.clone()});
    let document_schema = json!({
        "type": "object",
        "properties": {
            "source": source_schema.clone(),
            "native_id": {"type": "string", "minLength": 1}
        },
        "required": ["source", "native_id"],
        "additionalProperties": false
    });
    let chunk_schema = json!({
        "type": "object",
        "properties": {
            "generation": {"type": "string", "minLength": 1},
            "source": source_schema.clone(),
            "chunk_id": {"type": "integer", "minimum": 0}
        },
        "required": ["generation", "source", "chunk_id"],
        "additionalProperties": false
    });
    let asset_schema = json!({
        "type": "object",
        "properties": {
            "source": source_schema.clone(),
            "asset_id": {"type": "string", "minLength": 1}
        },
        "required": ["source", "asset_id"],
        "additionalProperties": false
    });
    json!([
        {
            "name": "search",
            "description": "Search one source; returns document and chunk refs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1},
                    "source": {"type": "string", "enum": source_ids, "default": default_source},
                    "k": {"type": "integer", "minimum": 1, "maximum": 50},
                    "types": {"type": "array", "items": {"type": "string"}},
                    "date_from": {"type": "string"},
                    "date_to": {"type": "string"},
                    "doc_scope": {"type": "string"},
                    "mode": {"type": "string", "enum": ["hybrid", "vector", "keyword"]},
                    "sort_by": {"type": "string", "enum": ["relevance", "recency"]},
                    "include_old": {"type": "boolean"},
                    "current_only": {"type": "boolean"},
                    "similar_to_chunk": chunk_schema.clone(),
                    "seed_text": {"type": "string"},
                    "include_snippet": {"type": "boolean"}
                },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_asset",
            "description": "Resolve a typed asset to bytes and caption.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "asset": asset_schema
                },
                "required": ["asset"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_doc_anchors",
            "description": "Return anchors, links, history, and citations.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "document": document_schema.clone()
                },
                "required": ["document"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_chunks",
            "description": "Fetch typed chunks and optional neighbours; follow meta.next_call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chunks": {"type": "array", "items": chunk_schema, "minItems": 1, "maxItems": 100},
                    "before": {"type": "integer", "minimum": 0, "maximum": 20},
                    "after": {"type": "integer", "minimum": 0, "maximum": 20},
                    "max_chars": {"type": "integer", "minimum": 1, "maximum": 200000},
                    "cursor": {"type": "string"}
                },
                "required": ["chunks"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_definition",
            "description": "Find definitions within one legal source.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "term": {"type": "string", "minLength": 1},
                    "source": source_schema,
                    "context_document": document_schema,
                    "max_defs": {"type": "integer", "minimum": 1, "maximum": 20}
                },
                "required": ["term"],
                "additionalProperties": false
            }
        },
        {
            "name": "stats",
            "description": "Return counts, index metadata, and search policy.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "fetch",
            "description": "Live-fetch a canonical legal:// URI from a fetch marker.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri": {"type": "string", "minLength": 1}
                },
                "required": ["uri"],
                "additionalProperties": false
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ann::*;
    use crate::chunker::{chunk_html, chunk_html_with_token_count, EMBED_MAX_TOKENS};
    use crate::config::*;
    use crate::db::*;
    use crate::extract::*;
    #[allow(unused_imports)]
    use crate::html::*;
    use crate::retrieval::*;
    use crate::search::*;
    use crate::semantic::*;
    use crate::source::*;
    use chrono::Utc;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use rusqlite::types::Value;
    use rusqlite::Connection;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::io::Cursor;
    use tempfile::tempdir;

    const TEST_SOURCE_ID: &str = "ato";
    const TEST_GENERATION: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn test_source() -> legal_model::SourceId {
        TEST_SOURCE_ID.parse().expect("valid test source")
    }

    fn test_doc_filter(source_id: &SourceId, include_old: bool, current_only: bool) -> SqlFilter {
        build_doc_filter(
            "d",
            DocumentFilterSpec {
                source_id,
                types: None,
                date_from: None,
                date_to: None,
                doc_scope: None,
                include_old,
                current_only,
            },
        )
    }

    fn prepare_test_generation(data_dir: &Path) -> Result<PathBuf> {
        let generation = data_dir.join("generations").join(TEST_GENERATION);
        fs::create_dir_all(&generation)?;
        fs::write(data_dir.join("active-generation"), TEST_GENERATION)?;
        Ok(generation)
    }

    #[test]
    fn source_update_cli_accepts_repeated_source_workspaces() {
        let cli = Cli::try_parse_from([
            "legal-mcp",
            "source-update",
            "--workspace",
            "ato=/data/ato",
            "--workspace",
            "wa=/data/wa",
            "--run-dir",
            "/data/runs/one",
        ])
        .expect("valid source-update arguments");
        let Command::SourceUpdate {
            workspaces,
            run_dir,
        } = cli.command
        else {
            panic!("expected source-update command");
        };
        assert_eq!(
            workspaces,
            vec![
                SourceWorkspaceArg {
                    source: "ato".to_string(),
                    path: PathBuf::from("/data/ato"),
                },
                SourceWorkspaceArg {
                    source: "wa".to_string(),
                    path: PathBuf::from("/data/wa"),
                },
            ]
        );
        assert_eq!(run_dir, PathBuf::from("/data/runs/one"));
        assert!("ato".parse::<SourceWorkspaceArg>().is_err());
        assert!("ato=".parse::<SourceWorkspaceArg>().is_err());
    }

    // ----- W1.1 SIMD parity -----

    #[test]
    fn dot_i8_matches_scalar_reference() -> Result<()> {
        let mut rng = StdRng::seed_from_u64(42);
        for _ in 0..100 {
            let q: [i8; EMBEDDING_DIM] = std::array::from_fn(|_| rng.gen());
            let d: Vec<u8> = (0..EMBEDDING_DIM).map(|_| rng.gen::<u8>()).collect();
            let scalar = dot_i8_scalar_reference(&q, &d)?;
            let actual = dot_i8(&q, &d)?;
            assert!(
                (scalar - actual).abs() < 1e-6,
                "scalar {} vs actual {}",
                scalar,
                actual
            );
        }
        Ok(())
    }

    #[test]
    fn dot_i8_rejects_wrong_length() {
        let q = [0i8; EMBEDDING_DIM];
        let bad = vec![0u8; EMBEDDING_DIM - 1];
        assert!(dot_i8(&q, &bad).is_err());
    }

    #[test]
    fn quantize_embedding_rejects_non_finite_values() {
        use crate::semantic::quantize_embedding;
        let mut values = vec![0.1f32; EMBEDDING_DIM];
        values[7] = f32::NAN;
        assert!(quantize_embedding(&values).is_err());

        values[7] = f32::INFINITY;
        assert!(quantize_embedding(&values).is_err());
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
            document: DocumentId::new(TEST_SOURCE_ID.parse().expect("valid test source"), doc_id)
                .expect("valid test document"),
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
            let doc_id = metas.get(&h.chunk_id).unwrap().document.native_id.as_str();
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

    // ----- shared in-memory corpus helpers -----

    /// Build an in-memory test corpus, return the open Connection.
    fn make_test_db() -> Result<(tempfile::TempDir, std::path::PathBuf)> {
        let dir = tempdir()?;
        let db_dir = prepare_test_generation(dir.path())?;
        let db = db_dir.join(LEGAL_DB_FILENAME);
        let conn = open_write_at(&db)?;
        init_db(&conn)?;
        conn.execute(
            "INSERT INTO sources(source_id, display_name) VALUES (?1, ?2)",
            params![TEST_SOURCE_ID, "Australian Taxation Office"],
        )?;
        set_corpus_meta(&conn, "index_version", "test")?;
        set_source_meta(&conn, TEST_SOURCE_ID, "documents_count", "0")?;
        Ok((dir, db))
    }

    fn insert_doc(conn: &Connection, doc_id: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO documents
                (source_id, native_id, type, title, canonical_url, downloaded_at, content_hash, html)
             VALUES (?1, ?2, 'Public_ruling', ?3, ?4, ?5, ?6, ?7)",
            params![
                TEST_SOURCE_ID,
                doc_id,
                format!("{doc_id} title"),
                format!("https://www.ato.gov.au/law/view/document?docid={doc_id}"),
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
            "INSERT INTO documents
                (source_id, native_id, type, title, date, canonical_url, downloaded_at,
                 content_hash, html, withdrawn_date, superseded_by, replaces)
             VALUES (?1, ?2, 'Public_ruling', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                TEST_SOURCE_ID,
                doc_id,
                format!("{doc_id} title"),
                date,
                format!("https://www.ato.gov.au/law/view/document?docid={doc_id}"),
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
            "INSERT INTO chunks(chunk_id, source_id, native_id, ord, anchor, text)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            params![chunk_id, TEST_SOURCE_ID, doc_id, ord, compress_text(text)?],
        )?;
        Ok(())
    }

    #[test]
    fn local_path_detection_precedes_url_scheme_parsing() {
        let windows = r"C:\corpus\model-bundle.tar.zst";
        assert_eq!(
            local_path_from_urlish(windows),
            Some(PathBuf::from(windows))
        );
        let unc = r"\\server\share\model-bundle.tar.zst";
        assert_eq!(local_path_from_urlish(unc), Some(PathBuf::from(unc)));
        assert!(local_path_from_urlish("https://example.com/model").is_none());
    }

    #[test]
    fn metadata_extract_pub_date_handles_utf8_boundary() {
        let mut text = "a".repeat(1999);
        text.push('•');
        text.push_str(" 1 January 2024");
        assert_eq!(metadata_extract_pub_date(&text), None);
    }

    #[test]
    fn ato_link_normalization_preserves_canonical_document_identity() -> Result<()> {
        let source = r#"<p>See <a href="https://www.ato.gov.au/law/view/document?docid=pac%2F19970038%2F203-55">section 203-55</a>.</p>"#;
        let rewritten = rewrite_links_html(source);
        assert!(rewritten.contains(r#"data-doc-id="ato:PAC/19970038/203-55""#));
        assert!(!rewritten.contains("href="));

        let anchors = extract_anchors(&rewritten, "TR/2026/1");
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].kind, "sister");
        assert_eq!(
            anchors[0].target_doc_id.as_deref(),
            Some("PAC/19970038/203-55")
        );

        let chunks = chunk_html(&rewritten, Some("Example"), EMBED_MAX_TOKENS);
        assert!(chunks
            .iter()
            .any(|chunk| chunk.text.contains("[doc:ato:PAC/19970038/203-55]")));
        Ok(())
    }

    #[test]
    fn real_ato_fixture_normalization_is_stable() -> Result<()> {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ato/cr-2025-13.html");
        let raw = fs::read_to_string(&fixture)?;
        assert_eq!(
            format!("{:x}", Sha256::digest(raw.as_bytes())),
            "c81e129935e1748e87b80e1be91a672511763660e36a6ef685e14bb0854c4a0e"
        );
        let cleaned = clean_ato_html(&raw);
        let (with_assets, assets) = rewrite_images_html(
            &cleaned.html,
            Some("CLR/CR202513/NAT/ATO/00001"),
            Some(&fixture),
        );
        let normalized = normalise_named_anchors(&with_assets);
        let linked = rewrite_links_html(&normalized);
        let final_html = strip_attributes(&linked);
        let chunks = chunk_html_with_token_count(
            &final_html,
            cleaned.title.as_deref(),
            EMBED_MAX_TOKENS,
            |text| Ok(text.split_whitespace().count().max(1)),
        )?;
        let chunk_projection = chunks
            .iter()
            .map(|chunk| format!("{}\t{:?}\t{}", chunk.ord, chunk.anchor, chunk.text))
            .collect::<Vec<_>>()
            .join("\n");
        let anchors = extract_anchors(&final_html, "CLR/CR202513/NAT/ATO/00001");
        assert_eq!(assets.len(), 0);
        assert_eq!(
            format!("{:x}", Sha256::digest(final_html.as_bytes())),
            "d35a6cb8d7df1f4cb8bed9a700dc4cdf7ccc78cf9763dc240b6404443faba0cf"
        );
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            format!("{:x}", Sha256::digest(chunk_projection.as_bytes())),
            "10f1333f7ab7d1bb09c6ae95a75335d7e6e59481837383ad78fac5225e9b8cf9"
        );
        assert_eq!(anchors.len(), 9);
        assert_eq!(
            anchors
                .iter()
                .filter(|anchor| anchor.kind == "in_doc")
                .count(),
            5
        );
        assert_eq!(
            anchors
                .iter()
                .filter(|anchor| anchor.kind == "sister")
                .filter_map(|anchor| anchor.target_doc_id.as_deref())
                .collect::<Vec<_>>(),
            vec![
                "PAC/19360027/6(1)",
                "PAC/19970038/83A-33",
                "PAC/19970038/83A-45(4)",
                "PAC/19970038/83A-45(5)(a)",
            ]
        );
        assert!(final_html.contains(r#"data-doc-id="ato:PAC/19970038/83A-33""#));
        assert!(chunks[0].text.contains("[doc:ato:PAC/19970038/83A-33]"));
        Ok(())
    }

    #[test]
    fn html_elements_and_assets_are_queryable() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        let html = r#"<div id="LawContent"><h1 id="top">Example</h1><p>See <a data-doc-id="ato:PAC/19970038/203-55">203-55</a>.</p><span data-asset-ref="ato:DOC_HTML/0">[image: Diagram]</span></div>"#;
        insert_doc(&conn, "DOC_HTML")?;
        conn.execute(
            "UPDATE documents SET title = ?1, html = ?2
             WHERE source_id = ?3 AND native_id = ?4",
            params!["HTML doc", compress_text(html)?, TEST_SOURCE_ID, "DOC_HTML"],
        )?;
        let asset_bytes: &[u8] = b"GIF89a-fake-payload";
        conn.execute(
            "INSERT INTO document_assets
                (source_id, asset_id, native_id, media_type, alt, title, sha256, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                TEST_SOURCE_ID,
                "DOC_HTML/0",
                "DOC_HTML",
                "image/gif",
                Option::<String>::None,
                "Diagram",
                format!("{:x}", Sha256::digest(asset_bytes)),
                asset_bytes,
            ],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let source: legal_model::SourceId = TEST_SOURCE_ID.parse()?;
            let asset = AssetRef::new(source.clone(), "DOC_HTML/0")?;
            let content = get_asset(asset)?;
            let items = content.as_array().expect("content array");
            assert_eq!(
                items.len(),
                2,
                "expected caption + image item, got {content:?}"
            );
            assert_eq!(items[0]["type"], "text");
            assert!(
                items[0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("ato:DOC_HTML/0"),
                "caption should reference asset_ref: {}",
                items[0]["text"]
            );
            assert_eq!(items[1]["type"], "image");
            assert_eq!(items[1]["mimeType"], "image/gif");
            let b64 = items[1]["data"].as_str().expect("base64 data");
            let decoded = {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.decode(b64)?
            };
            assert_eq!(decoded, asset_bytes);

            let missing = get_asset(AssetRef::new(source, "DOC_HTML/missing")?)?;
            let items = missing.as_array().expect("content array");
            assert_eq!(items.len(), 1);
            assert_eq!(items[0]["type"], "text");
            assert!(items[0]["text"].as_str().unwrap().contains("not found"));
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
        insert_definition_with_source(conn, definition_id, term, doc_id, body, "PAC")
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
            "INSERT INTO definitions
                (source_id, definition_id, term, norm_term, native_id, source_title,
                 source_type, scope, anchor, ord, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, 0, ?9)",
            params![
                TEST_SOURCE_ID,
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
        let _environment = TestEnvironment::set(&[("LEGAL_MCP_DATA_DIR", dir.as_os_str())]);
        f()
    }

    fn lock_test_db() -> std::sync::MutexGuard<'static, ()> {
        TEST_DB_LOCK.lock().unwrap_or_else(|err| err.into_inner())
    }

    #[test]
    fn read_http_state_requires_the_exact_endpoint_contract() -> Result<()> {
        let _lock = lock_test_db();
        let dir = tempdir()?;
        fs::write(
            dir.path().join("http.json"),
            r#"{
  "bind": "127.0.0.1",
  "port": 37409
}
"#,
        )?;
        with_data_dir(dir.path(), || {
            assert!(read_http_state()?.is_none());
            Ok(())
        })
    }

    #[test]
    fn read_http_state_rejects_noncanonical_endpoint() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let dir = tempdir()?;
        fs::write(
            dir.path().join("http.json"),
            r#"{"port":37409,"url":"http://example.test:37409/mcp"}"#,
        )?;
        with_data_dir(dir.path(), || -> Result<()> {
            assert!(read_http_state()?.is_none());
            Ok(())
        })
    }

    #[test]
    fn json_rpc_empty_batch_and_strict_tool_arguments() {
        let state = ServerState::new();
        let empty = handle_rpc(json!([]), &state).expect("empty batch response");
        assert_eq!(empty["error"]["code"], -32600);

        let malformed = handle_rpc(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "get_chunks",
                    "arguments": {"chunks": [{"generation": TEST_GENERATION, "source": TEST_SOURCE_ID, "chunk_id": 1}], "before": "2"}
                }
            }),
            &state,
        )
        .expect("invalid params response");
        assert_eq!(malformed["error"]["code"], -32602);

        let malformed_cursor = handle_rpc(
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "get_chunks",
                    "arguments": {"chunks": [{"generation": TEST_GENERATION, "source": TEST_SOURCE_ID, "chunk_id": 1}], "cursor": false}
                }
            }),
            &state,
        )
        .expect("invalid cursor response");
        assert_eq!(malformed_cursor["error"]["code"], -32602);

        let unknown_argument = handle_rpc(
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "stats",
                    "arguments": {"unexpected": true}
                }
            }),
            &state,
        )
        .expect("unknown argument response");
        assert_eq!(unknown_argument["error"]["code"], -32602);

        let unknown_param = handle_rpc(
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "stats",
                    "arguments": {},
                    "unexpected": true
                }
            }),
            &state,
        )
        .expect("unknown tools/call parameter response");
        assert_eq!(unknown_param["error"]["code"], -32602);

        let unknown_source = handle_rpc(
            json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {
                    "name": "search",
                    "arguments": {"query": "tax", "source": "wa-legislation"}
                }
            }),
            &state,
        )
        .expect("unknown source response");
        assert_eq!(unknown_source["error"]["code"], -32602);
        assert!(unknown_source["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("available sources: ato")));
    }

    #[test]
    fn public_tool_protocol_requires_canonical_source_qualified_references() {
        let chunk = json!({
            "generation": TEST_GENERATION,
            "source": TEST_SOURCE_ID,
            "chunk_id": 91,
        });
        let document = json!({"source": TEST_SOURCE_ID, "native_id": "PAC/1"});
        let asset = json!({"source": TEST_SOURCE_ID, "asset_id": "DOC/0"});
        for params in [
            json!({
                "name": "search",
                "arguments": {
                    "query": "tax",
                    "similar_to_chunk": chunk.clone(),
                }
            }),
            json!({"name": "get_chunks", "arguments": {"chunks": [chunk.clone()]}}),
            json!({"name": "get_asset", "arguments": {"asset": asset}}),
            json!({"name": "get_doc_anchors", "arguments": {"document": document.clone()}}),
            json!({
                "name": "get_definition",
                "arguments": {"term": "car", "source": TEST_SOURCE_ID, "context_document": document}
            }),
            json!({"name": "stats", "arguments": {}}),
            json!({"name": "fetch", "arguments": {"uri": "legal://ato/PAC%2F1?pit=20200101000000"}}),
        ] {
            validate_tool_call(&params).expect("canonical protocol arguments must validate");
        }

        for params in [
            json!({"name": "search", "arguments": {"query": "tax", "similar_to_chunk_id": 91}}),
            json!({"name": "search", "arguments": {"query": "tax", "similar_to": chunk.clone()}}),
            json!({"name": "get_chunks", "arguments": {"chunk_ids": [91]}}),
            json!({"name": "get_asset", "arguments": {"asset_ref": "ato-image://DOC/0"}}),
            json!({"name": "get_doc_anchors", "arguments": {"doc_id": "PAC/1"}}),
            json!({
                "name": "get_definition",
                "arguments": {"term": "car", "context_doc_id": "PAC/1"}
            }),
            json!({"name": "fetch", "arguments": {"uri": "ato:PAC/1"}}),
            json!({"name": "fetch", "arguments": {"uri": "legal://ato/PAC/1"}}),
            json!({"name": "get_chunks", "arguments": {"chunks": [{"generation": TEST_GENERATION, "chunk_id": 1}]}}),
        ] {
            assert!(
                validate_tool_call(&params).is_err(),
                "alternate identity unexpectedly validated: {params}"
            );
        }
    }

    #[test]
    fn fetch_uri_validation_requires_canonical_legal_uri() {
        for uri in [
            "legal://ato/PAC%2F1",
            "legal://ato/PAC%2F1?pit=20200101000000",
            "legal://ato/PAC%2F1?pit=20200101000000&view=HISTFT",
        ] {
            validate_fetch_uri(uri).unwrap_or_else(|error| {
                panic!("canonical fetch URI `{uri}` must validate: {error}")
            });
        }

        for uri in [
            "",
            "PAC/1",
            "ato:PAC/1",
            "legal://ato/PAC/1",
            "legal://ato/PAC%2f1",
            "legal://ato/PAC%2F1?",
            "legal://ato/PAC%2F1#top",
            "legal://ato/PAC%ZZ1",
            "legal://ato/%E2%28",
            "legal://user@ato/PAC%2F1",
            "legal://wa-legislation/PAC%2F1",
        ] {
            assert!(
                validate_fetch_uri(uri).is_err(),
                "noncanonical fetch URI unexpectedly validated: {uri}"
            );
        }
    }

    #[test]
    fn get_chunks_cursor_round_trips_through_json_rpc() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        insert_doc(&conn, "DOC_CURSOR")?;
        insert_chunk(&conn, 91, "DOC_CURSOR", 0, "abcdefghij")?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let state = ServerState::new();
            let mut cursor = None;
            let mut combined = String::new();
            let chunk_ref = ChunkRef::new(TEST_GENERATION, TEST_SOURCE_ID.parse()?, 91)?;
            for id in 1..=4 {
                let mut arguments = json!({"chunks": [chunk_ref.clone()], "max_chars": 4});
                if let Some(value) = cursor.take() {
                    arguments["cursor"] = JsonValue::String(value);
                }
                let response = handle_rpc(
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "method": "tools/call",
                        "params": {"name": "get_chunks", "arguments": arguments}
                    }),
                    &state,
                )
                .expect("get_chunks response");
                assert!(response.get("error").is_none(), "response: {response}");
                let text = response["result"]["content"][0]["text"]
                    .as_str()
                    .ok_or_else(|| anyhow!("missing get_chunks text result"))?;
                let body: JsonValue = serde_json::from_str(text)?;
                for chunk in body["chunks"].as_array().into_iter().flatten() {
                    combined.push_str(chunk["text"].as_str().unwrap_or_default());
                }
                let Some(next_call) = body.pointer("/meta/next_call").and_then(JsonValue::as_str)
                else {
                    break;
                };
                let encoded = next_call
                    .split_once("cursor=")
                    .ok_or_else(|| anyhow!("next_call omitted cursor: {next_call}"))?
                    .1
                    .strip_suffix(')')
                    .ok_or_else(|| anyhow!("next_call was not a complete call: {next_call}"))?;
                cursor = Some(serde_json::from_str(encoded)?);
            }
            assert_eq!(combined, "abcdefghij");
            assert!(cursor.is_none(), "continuation did not terminate");
            assert_eq!(
                tool_descriptors()
                    .as_array()
                    .and_then(|tools| tools.iter().find(|tool| tool["name"] == "get_chunks"))
                    .and_then(|tool| tool.pointer("/inputSchema/properties/cursor"))
                    .and_then(|schema| schema["type"].as_str()),
                Some("string")
            );
            Ok(())
        })
    }

    // ----- W1.5 manifest version guards -----

    #[test]
    fn manifest_accepts_the_exact_current_schema() {
        let m = sample_manifest(SUPPORTED_SCHEMA_VERSION, "");
        assert!(validate_manifest(&m).is_ok());
    }

    #[test]
    fn manifest_rejects_a_different_newer_schema() {
        let m = sample_manifest(SUPPORTED_SCHEMA_VERSION + 1, "");
        let err = validate_manifest(&m).unwrap_err();
        assert!(
            err.to_string().contains("not supported"),
            "expected unsupported-schema message, got: {err}"
        );
    }

    #[test]
    fn manifest_rejects_a_different_older_schema() {
        let m = sample_manifest(SUPPORTED_SCHEMA_VERSION - 1, "");
        let err = validate_manifest(&m).unwrap_err();
        assert!(
            err.to_string().contains("not supported"),
            "expected unsupported-schema message, got: {err}"
        );
    }

    #[test]
    fn manifest_json_rejects_unknown_fields_at_every_level() -> Result<()> {
        let mut top_level = serde_json::to_value(sample_manifest(
            SUPPORTED_SCHEMA_VERSION,
            env!("CARGO_PKG_VERSION"),
        ))?;
        top_level["unexpected"] = JsonValue::Null;
        assert!(
            serde_json::from_value::<Manifest>(top_level).is_err(),
            "unknown manifest fields must be rejected"
        );

        let mut nested = serde_json::to_value(sample_manifest(
            SUPPORTED_SCHEMA_VERSION,
            env!("CARGO_PKG_VERSION"),
        ))?;
        nested["model"]["unexpected"] = json!(true);
        assert!(
            serde_json::from_value::<Manifest>(nested).is_err(),
            "unknown nested fields must be rejected"
        );
        Ok(())
    }

    #[test]
    fn publish_model_info_rejects_non_hf_mirror_without_metadata() {
        let current = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: EMBEDDING_MODEL_FINGERPRINT.to_string(),
            size: EMBEDDING_MODEL_HF_SIZE,
            url: EMBEDDING_MODEL_HF_URL.to_string(),
        };
        let err = resolve_publish_model_info(
            &current,
            Some("https://mirror.example.com/granite.tar.zst"),
            None,
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("--model-sha256") && err.to_string().contains("--model-size"),
            "expected explicit mirror metadata error, got: {err}"
        );
    }

    #[test]
    fn publish_model_info_accepts_non_hf_mirror_with_metadata() -> Result<()> {
        let current = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: EMBEDDING_MODEL_FINGERPRINT.to_string(),
            size: EMBEDDING_MODEL_HF_SIZE,
            url: EMBEDDING_MODEL_HF_URL.to_string(),
        };
        let resolved = resolve_publish_model_info(
            &current,
            Some("https://mirror.example.com/granite.tar.zst"),
            Some("abc123"),
            Some(42),
        )?;
        assert_eq!(resolved.url, "https://mirror.example.com/granite.tar.zst");
        assert_eq!(resolved.sha256, "abc123");
        assert_eq!(resolved.size, 42);
        Ok(())
    }

    #[test]
    fn publish_model_info_rejects_https_huggingface_url() {
        let current = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: EMBEDDING_MODEL_FINGERPRINT.to_string(),
            size: EMBEDDING_MODEL_HF_SIZE,
            url: EMBEDDING_MODEL_HF_URL.to_string(),
        };
        let err = resolve_publish_model_info(
            &current,
            Some("https://huggingface.co/onnx-community/granite-embedding-small-english-r2-ONNX/resolve/main/model.tar.zst"),
            None,
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("hf://repo@revision"),
            "expected hf:// guidance, got: {err}"
        );
    }

    #[test]
    fn publish_model_info_rejects_hf_url_without_revision() {
        let current = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: EMBEDDING_MODEL_FINGERPRINT.to_string(),
            size: EMBEDDING_MODEL_HF_SIZE,
            url: EMBEDDING_MODEL_HF_URL.to_string(),
        };
        let err =
            resolve_publish_model_info(&current, Some("hf://owner/model"), None, None).unwrap_err();
        assert!(
            err.to_string().contains("explicit revision"),
            "expected explicit revision error, got: {err}"
        );
        assert!(parse_hf_model_url("hf://owner/model").is_none());
    }

    #[test]
    fn publish_model_info_rejects_hf_metadata_mismatch() {
        let current = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: EMBEDDING_MODEL_FINGERPRINT.to_string(),
            size: EMBEDDING_MODEL_HF_SIZE,
            url: EMBEDDING_MODEL_HF_URL.to_string(),
        };
        let err = resolve_publish_model_info(
            &current,
            Some(EMBEDDING_MODEL_HF_URL),
            Some("wrong-sha"),
            Some(EMBEDDING_MODEL_HF_SIZE),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("pinned Granite fingerprint"),
            "expected HF sha mismatch error, got: {err}"
        );
        let err = resolve_publish_model_info(
            &current,
            Some(EMBEDDING_MODEL_HF_URL),
            Some(EMBEDDING_MODEL_FINGERPRINT),
            Some(1),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("pinned Granite file set"),
            "expected HF size mismatch error, got: {err}"
        );
    }

    #[test]
    fn publish_model_info_defaults_placeholder_to_pinned_hf_source() -> Result<()> {
        let current = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: String::new(),
            size: 0,
            url: "PENDING".to_string(),
        };
        let resolved = resolve_publish_model_info(&current, None, None, None)?;
        assert_eq!(resolved.url, EMBEDDING_MODEL_HF_URL);
        assert_eq!(resolved.sha256, EMBEDDING_MODEL_FINGERPRINT);
        assert_eq!(resolved.size, EMBEDDING_MODEL_HF_SIZE);
        Ok(())
    }

    #[test]
    fn manifest_rejects_a_higher_minimum_client_version() {
        let m = sample_manifest(SUPPORTED_SCHEMA_VERSION, "999.0.0");
        let err = validate_manifest(&m).unwrap_err();
        assert!(
            err.to_string().contains("999"),
            "expected min_client_version error, got: {err}"
        );
    }

    #[test]
    fn manifest_accepts_a_satisfied_minimum_client_version() {
        // Any version that's <= the current binary's version should pass.
        let current = env!("CARGO_PKG_VERSION");
        let m = sample_manifest(SUPPORTED_SCHEMA_VERSION, current);
        assert!(validate_manifest(&m).is_ok());
        let m = sample_manifest(SUPPORTED_SCHEMA_VERSION, "0.0.1");
        assert!(validate_manifest(&m).is_ok());
    }

    #[test]
    fn manifest_rejects_non_hf_model_without_metadata() {
        let mut m = sample_manifest(SUPPORTED_SCHEMA_VERSION, "");
        m.model = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: String::new(),
            size: 0,
            url: "model-bundle.tar.zst".to_string(),
        };
        let err = validate_manifest(&m).unwrap_err();
        assert!(
            err.to_string().contains("sha256 and positive size"),
            "expected non-HF model metadata error, got: {err}"
        );
    }

    #[test]
    fn manifest_rejects_hf_model_metadata_mismatch() {
        let mut m = sample_manifest(SUPPORTED_SCHEMA_VERSION, "");
        m.model = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: "wrong-sha".to_string(),
            size: EMBEDDING_MODEL_HF_SIZE,
            url: EMBEDDING_MODEL_HF_URL.to_string(),
        };
        let err = validate_manifest(&m).unwrap_err();
        assert!(
            err.to_string().contains("pinned Granite fingerprint"),
            "expected HF sha mismatch error, got: {err}"
        );
        m.model.sha256 = EMBEDDING_MODEL_FINGERPRINT.to_string();
        m.model.size = 1;
        let err = validate_manifest(&m).unwrap_err();
        assert!(
            err.to_string().contains("pinned Granite file set"),
            "expected HF size mismatch error, got: {err}"
        );
    }

    #[test]
    fn stage_model_rejects_wrong_non_hf_bundle_size() -> Result<()> {
        let _lock = lock_test_db();
        let data = tempdir()?;
        prepare_test_generation(data.path())?;
        let release = tempdir()?;
        let model_bundle = release.path().join("model-bundle.tar.zst");
        let bundle_bytes = write_test_model_bundle(&model_bundle)?;
        let mut manifest = sample_manifest(SUPPORTED_SCHEMA_VERSION, env!("CARGO_PKG_VERSION"));
        manifest.index_version = "wrong-model-size".to_string();
        manifest.model = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: sha256_hex(&bundle_bytes),
            size: bundle_bytes.len() as u64 + 1,
            url: "model-bundle.tar.zst".to_string(),
        };
        let context = UrlContext {
            manifest_dir: Some(release.path().to_path_buf()),
            manifest_base_url: None,
        };
        with_data_dir(data.path(), || -> Result<()> {
            let err = stage_model(&manifest, &context, &staging_dir()?).unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains("artifact") && message.contains("size mismatch"),
                "expected artifact size validation error, got: {err}"
            );
            Ok(())
        })?;
        Ok(())
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
    fn build_model_dir_rejects_wrong_model_bytes() -> Result<()> {
        let dir = tempdir()?;
        fs::create_dir_all(dir.path().join("onnx"))?;
        fs::write(dir.path().join("onnx/model_fp16.onnx"), b"wrong onnx")?;
        fs::write(dir.path().join("onnx/model_fp16.onnx_data"), b"wrong data")?;
        fs::write(dir.path().join("tokenizer.json"), b"wrong tokenizer")?;

        let err = SemanticModelPaths::from_model_dir(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("size mismatch"),
            "expected build model-dir size validation error, got: {err}"
        );
        Ok(())
    }

    #[test]
    fn open_read_rejects_unsupported_schema_version() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        // Force a bogus schema version via raw SQL.
        set_corpus_meta(&conn, "schema_version", "99")?;
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
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        // init_db wrote the row; now delete it to simulate a corrupt /
        // partial install.
        conn.execute("DELETE FROM corpus_meta WHERE key = 'schema_version'", [])?;
        drop(conn);
        with_data_dir(dir.path(), || -> Result<()> {
            let err = open_read().unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("corrupt or incomplete")
                    && msg.contains("install a complete corpus generation"),
                "expected corrupt/incomplete error with init hint, got: {msg}"
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn vector_search_requires_model_marker_match() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        set_corpus_meta(&conn, "embedding_model_id", EMBEDDING_MODEL_ID)?;
        drop(conn);
        let model_sha = "e".repeat(64);
        let mut manifest = sample_manifest(SUPPORTED_SCHEMA_VERSION, env!("CARGO_PKG_VERSION"));
        manifest.index_version = "test-marker-readiness".to_string();
        manifest.model = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: model_sha,
            size: 5,
            url: "model-bundle.tar.zst".to_string(),
        };

        with_data_dir(dir.path(), || -> Result<()> {
            fs::write(
                installed_manifest_path()?,
                serde_json::to_vec_pretty(&manifest)?,
            )?;
            fs::write(model_path()?, b"model")?;
            fs::write(model_data_path()?, b"model-data")?;
            fs::write(tokenizer_path()?, br#"{"version":"1.0"}"#)?;
            fs::write(live_dir()?.join(".model.sha256"), "wrong-marker")?;

            let conn = open_read()?;
            let source_id: legal_model::SourceId = TEST_SOURCE_ID.parse()?;
            let err = ensure_vector_search_ready(&conn, &source_id).unwrap_err();
            assert!(
                err.to_string().contains("installed semantic model files"),
                "expected model marker readiness error, got: {err}"
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn ann_generation_update_commits_with_one_active_pointer() -> Result<()> {
        let _lock = lock_test_db();
        let data = tempdir()?;
        with_data_dir(data.path(), || -> Result<()> {
            let staging_root = staging_dir()?.join("generation-promotion-test");
            fs::create_dir_all(&staging_root)?;
            let staged_db = staging_root.join(LEGAL_DB_FILENAME);
            let source_id: legal_model::SourceId = TEST_SOURCE_ID.parse()?;
            fs::write(&staged_db, b"database-generation")?;

            let model_dir = staging_dir()?.join("generation-model-test");
            fs::create_dir_all(&model_dir)?;
            for file in EMBEDDING_MODEL_HF_FILES {
                fs::write(
                    model_dir.join(file.output_name),
                    file.output_name.as_bytes(),
                )?;
            }
            let staged_model = StagedModel {
                dir: model_dir,
                marker_value: "model-marker".to_string(),
            };
            let mut manifest = sample_manifest(SUPPORTED_SCHEMA_VERSION, "");
            manifest
                .ann
                .insert(source_id.clone(), sample_ann(TEST_SOURCE_ID, "d"));
            let mut staged_sidecars = BTreeMap::new();
            for manifest_source in manifest.ann.keys() {
                let staged_ann = staging_root.join(sidecar_relative_path(manifest_source));
                fs::create_dir_all(
                    staged_ann
                        .parent()
                        .ok_or_else(|| anyhow!("test ANN sidecar has no parent"))?,
                )?;
                fs::write(&staged_ann, format!("{manifest_source}-ann"))?;
                staged_sidecars.insert(manifest_source.clone(), staged_ann);
            }
            promote_staged_update(
                Some(&staged_model),
                StagedCorpusUpdate {
                    staging_root,
                    staged_db,
                    staged_ann: staged_sidecars,
                    stats: UpdateStats::default(),
                },
                &manifest,
            )?;

            let key = active_generation_key()?.expect("generation must be active");
            assert_eq!(key.len(), 64);
            assert_eq!(fs::read(db_path()?)?, b"database-generation");
            for manifest_source in manifest.ann.keys() {
                assert_eq!(
                    fs::read(live_dir()?.join(sidecar_relative_path(manifest_source)))?,
                    format!("{manifest_source}-ann").into_bytes()
                );
            }
            let installed: Manifest =
                serde_json::from_slice(&fs::read(installed_manifest_path()?)?)?;
            assert_eq!(installed, manifest);
            Ok(())
        })
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
        let snippet = highlight_snippet(text, "completely unrelated query terms", SNIPPET_CHARS);
        assert!(!snippet.is_empty(), "snippet should not be empty");
        assert!(
            !snippet.contains(" — "),
            "snippet should not carry a heading prefix, got: {snippet}"
        );
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
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

    fn write_test_model_bundle(path: &Path) -> Result<Vec<u8>> {
        let files: &[(&str, &[u8])] = &[
            ("model_fp16.onnx", b"dummy onnx bytes"),
            ("model_fp16.onnx_data", b"dummy external data"),
            ("tokenizer.json", br#"{"version":"1.0","truncation":null}"#),
        ];
        write_test_tar_zst(path, files)?;
        fs::read(path).map_err(Into::into)
    }

    fn sample_manifest(schema_version: u32, min_client_version: &str) -> Manifest {
        let source_id: legal_model::SourceId = TEST_SOURCE_ID.parse().expect("valid source id");
        let frl_source_id: legal_model::SourceId = "frl".parse().expect("valid source id");
        Manifest {
            schema_version,
            index_version: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            min_client_version: if min_client_version.is_empty() {
                env!("CARGO_PKG_VERSION").to_string()
            } else {
                min_client_version.to_string()
            },
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: "4".repeat(64),
                size: 1,
                url: "model-bundle.tar.zst".to_string(),
            },
            db: ManifestDb {
                url: LEGAL_DB_ARCHIVE_FILENAME.to_string(),
                sha256: "5".repeat(64),
                size: 1,
            },
            ann: BTreeMap::from([
                (source_id, sample_ann(TEST_SOURCE_ID, "3")),
                (frl_source_id, sample_ann("frl", "6")),
            ]),
        }
    }

    fn sample_ann(source: &str, digest_character: &str) -> ManifestAnn {
        let source_id: legal_model::SourceId = source.parse().expect("valid test source id");
        ManifestAnn {
            source_id: source_id.clone(),
            format: ANN_FORMAT.to_string(),
            format_version: ANN_FORMAT_VERSION,
            library: ANN_LIBRARY.to_string(),
            library_version: ANN_LIBRARY_VERSION.to_string(),
            url: sidecar_relative_path(&source_id)
                .to_string_lossy()
                .into_owned(),
            sha256: digest_character.repeat(64),
            size: 1,
            corpus_id: format!("sha256:{}", digest_character.repeat(64)),
            embedding_model_id: EMBEDDING_MODEL_ID.to_string(),
            embedding_dimension: EMBEDDING_DIM as u32,
            embedding_set_sha256: digest_character.repeat(64),
            vector_count: 1,
            seed: ANN_SEED,
            rng: ANN_RNG.to_string(),
            trees: ANN_TREES as u32,
            split_after: ANN_SPLIT_AFTER as u32,
            id_encoding: ANN_ID_ENCODING.to_string(),
            metric: ANN_METRIC.to_string(),
        }
    }

    // ----- serve startup: probe + server_instructions -----

    #[test]
    fn server_instructions_no_db_tells_user_to_run_update() -> Result<()> {
        let _lock = lock_test_db();
        let data = tempdir()?;
        with_data_dir(data.path(), || {
            let text = server_instructions(None);
            assert!(
                text.contains("not yet installed"),
                "missing not-installed prefix in: {text}"
            );
            assert!(
                text.contains("legal-mcp update"),
                "missing install command in: {text}"
            );
            assert!(text.contains("1.5 GB"), "missing size hint in: {text}");
        });
        Ok(())
    }

    #[test]
    fn server_instructions_appends_update_available_notice() -> Result<()> {
        let _lock = lock_test_db();
        let data = tempdir()?;
        with_data_dir(data.path(), || {
            let notice = UpdateAvailability {
                available_index_version: "2026.05.20".to_string(),
            };
            let text = server_instructions(Some(&notice));
            assert!(
                text.contains("updated ATO corpus generation is available"),
                "missing update notice in: {text}"
            );
            assert!(
                text.contains("2026.05.20"),
                "missing available index_version in: {text}"
            );
            assert!(
                text.contains("legal-mcp update"),
                "missing update command in: {text}"
            );
        });
        Ok(())
    }

    #[test]
    fn mcp_startup_guidance_stays_compact() -> Result<()> {
        let _lock = lock_test_db();
        let data = tempdir()?;
        with_data_dir(data.path(), || {
            let static_chars = LEGAL_MCP_USE_INSTRUCTIONS.chars().count();
            let static_words = LEGAL_MCP_USE_INSTRUCTIONS.split_whitespace().count();
            assert!(
                static_chars <= 600,
                "static MCP use instructions are too large: {static_chars} chars"
            );
            assert!(
                static_words <= 100,
                "static MCP use instructions are too large: {static_words} words"
            );

            let text = server_instructions(None);
            let boot_chars = text.chars().count();
            assert!(
                boot_chars <= 1_100,
                "missing-corpus startup instructions are too large: {boot_chars} chars"
            );
        });
        Ok(())
    }

    #[test]
    fn mcp_tool_descriptors_stay_compact() -> Result<()> {
        let tools = tool_descriptors();
        let array = tools
            .as_array()
            .expect("tool_descriptors must return an array");
        let mut total_chars = 0usize;
        for tool in array {
            let name = tool
                .get("name")
                .and_then(JsonValue::as_str)
                .unwrap_or("<unnamed>");
            let desc_chars = tool
                .get("description")
                .and_then(JsonValue::as_str)
                .map(|description| description.chars().count())
                .unwrap_or(0);
            assert!(
                desc_chars <= 300,
                "{name} tool description is too large: {desc_chars} chars"
            );

            let schema_chars = serde_json::to_string(
                tool.get("inputSchema")
                    .expect("tool descriptor must include inputSchema"),
            )?
            .chars()
            .count();
            total_chars += desc_chars + schema_chars;
        }
        assert!(
            total_chars <= 3_000,
            "MCP tool descriptor payload is too large: {total_chars} chars"
        );
        Ok(())
    }

    #[test]
    fn check_for_update_availability_returns_none_when_no_installed_manifest() -> Result<()> {
        let _lock = lock_test_db();
        let data = tempdir()?;
        let result = with_data_dir(data.path(), || {
            check_for_update_availability("https://example.invalid/manifest.json")
        });
        assert!(
            result?.is_none(),
            "probe must return None when no installed manifest is present"
        );
        Ok(())
    }

    #[test]
    fn corpus_manifest_discovery_paginates_past_binary_only_releases() -> Result<()> {
        let first_page = (0..100)
            .map(|index| {
                json!({
                    "tag_name": format!("v0.15.{index}"),
                    "assets": [{
                        "name": "legal-mcp-x86_64-unknown-linux-gnu.tar.gz",
                        "browser_download_url": format!("https://example.test/{index}/binary.tar.gz")
                    }]
                })
            })
            .collect::<Vec<_>>();
        let second_page = vec![json!({
            "tag_name": "corpus",
            "assets": [{
                "name": "manifest.json",
                "browser_download_url": "https://example.test/corpus/manifest.json"
            }]
        })];
        let pages = [
            serde_json::to_vec(&first_page)?,
            serde_json::to_vec(&second_page)?,
        ];
        let mut fetched_pages = Vec::new();
        let manifest = source::resolve_corpus_manifest_url_with(|page| {
            fetched_pages.push(page);
            pages
                .get(page - 1)
                .cloned()
                .ok_or_else(|| anyhow!("unexpected release page {page}"))
        })?;
        assert_eq!(fetched_pages, vec![1, 2]);
        assert_eq!(manifest, "https://example.test/corpus/manifest.json");
        Ok(())
    }

    #[test]
    fn check_for_update_availability_suppresses_incompatible_schema() -> Result<()> {
        let _lock = lock_test_db();
        let data = tempdir()?;
        prepare_test_generation(data.path())?;
        let release = tempdir()?;
        let release_dir = release.path();
        let manifest_path = release_dir.join("manifest.json");

        let mut installed = sample_manifest(SUPPORTED_SCHEMA_VERSION, env!("CARGO_PKG_VERSION"));
        installed.index_version = "test-installed".to_string();
        // Published manifest declares a schema this binary can't ingest.
        let published = Manifest {
            schema_version: SUPPORTED_SCHEMA_VERSION + 1,
            index_version: "test-future".to_string(),
            ..installed.clone()
        };
        fs::write(&manifest_path, serde_json::to_vec_pretty(&published)?)?;

        let result = with_data_dir(data.path(), || -> Result<Option<UpdateAvailability>> {
            fs::write(
                installed_manifest_path()?,
                serde_json::to_vec_pretty(&installed)?,
            )?;
            check_for_update_availability(manifest_path.to_str().expect("utf-8 path"))
        });
        assert!(
            result?.is_none(),
            "probe must suppress the notice when the published index requires a newer binary"
        );
        Ok(())
    }

    #[test]
    fn check_for_update_availability_returns_none_when_already_current() -> Result<()> {
        let _lock = lock_test_db();
        let data = tempdir()?;
        prepare_test_generation(data.path())?;
        let release = tempdir()?;
        let release_dir = release.path();
        let manifest_path = release_dir.join("manifest.json");
        let mut manifest = sample_manifest(SUPPORTED_SCHEMA_VERSION, env!("CARGO_PKG_VERSION"));
        manifest.index_version = "test-probe-current".to_string();
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

        let result = with_data_dir(data.path(), || -> Result<Option<UpdateAvailability>> {
            fs::write(
                installed_manifest_path()?,
                serde_json::to_vec_pretty(&manifest)?,
            )?;
            check_for_update_availability(manifest_path.to_str().expect("utf-8 path"))
        });
        assert!(
            result?.is_none(),
            "probe must return None when installed corpus already matches the published manifest"
        );
        Ok(())
    }

    // ===== Wave 2 ===========================================================

    // ----- Schema v10 -----

    #[test]
    fn schema_init_writes_v10_metadata() -> Result<()> {
        let _lock = lock_test_db();
        let (_dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        let value = get_corpus_meta(&conn, "schema_version")?
            .expect("init_db should have written schema_version");
        assert_eq!(value, SUPPORTED_SCHEMA_VERSION.to_string());
        assert_eq!(SUPPORTED_SCHEMA_VERSION, 10);
        Ok(())
    }

    #[test]
    fn open_read_rejects_unsupported_schema_corpus() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        // Stamp an unsupported schema version. The user-facing error must
        // refuse the corpus cleanly instead of trying to mutate it in place.
        set_corpus_meta(&conn, "schema_version", "5")?;
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
        let f = test_doc_filter(&test_source(), true, true);
        assert!(
            f.sql.contains("d.withdrawn_date IS NULL"),
            "current_only=true must add withdrawn_date IS NULL clause; sql={}",
            f.sql
        );
    }

    #[test]
    fn build_doc_filter_omits_withdrawn_clause_when_disabled() {
        let f = test_doc_filter(&test_source(), true, false);
        assert!(
            !f.sql.contains("withdrawn_date"),
            "current_only=false must not mention withdrawn_date; sql={}",
            f.sql
        );
    }

    #[test]
    fn build_doc_filter_uses_current_prefix_policy() {
        let f = test_doc_filter(&test_source(), false, true);
        assert!(
            f.sql.contains("d.type NOT IN (?)"),
            "default policy must exclude EV by prefix; sql={}",
            f.sql
        );
        assert!(
            f.sql.contains("d.type IN (?,?,?,?)"),
            "old-content exception must use legislation prefixes; sql={}",
            f.sql
        );
        let params: Vec<String> = f
            .params
            .iter()
            .filter_map(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert!(
            params.contains(&"EV".to_string()),
            "default exclusion must use EV prefix; params={params:?}"
        );
        for expected in LEGISLATION_TYPE_PREFIXES {
            assert!(
                params.contains(&expected.to_string()),
                "missing legislation prefix {expected}; params={params:?}"
            );
        }
    }

    #[test]
    fn build_doc_filter_does_not_apply_ato_policy_to_frl() -> Result<()> {
        let source: SourceId = "frl".parse()?;
        let filter = test_doc_filter(&source, false, true);
        assert!(!filter.sql.contains("type NOT IN"));
        assert!(!filter.sql.contains("date >="));
        assert!(filter.sql.contains("d.withdrawn_date IS NULL"));
        assert_eq!(filter.params, vec![Value::Text("frl".to_string())]);
        Ok(())
    }

    #[test]
    fn fts_query_uses_or_term_semantics() {
        assert_eq!(
            fts_query("a Body research-development evidence"),
            "\"Body\" OR \"research-development\" OR \"evidence\""
        );
    }

    #[test]
    fn search_next_call_preserves_current_only_false() -> Result<()> {
        let source: legal_model::SourceId = TEST_SOURCE_ID.parse()?;
        let opts = SearchOptions {
            source: source.clone(),
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
            similar_to_chunk: None,
            seed_text: None,
        };
        let call = search_next_call("depreciation", 16, &opts)?;
        assert!(
            call.contains("current_only=false"),
            "continuation must preserve withdrawn-doc inclusion; got: {call}"
        );
        assert!(
            call.contains(r#"source="ato""#),
            "continuation must preserve the selected source; got: {call}"
        );
        Ok(())
    }

    #[test]
    fn search_next_call_preserves_seed_text() -> Result<()> {
        let source: legal_model::SourceId = TEST_SOURCE_ID.parse()?;
        let opts = SearchOptions {
            source: source.clone(),
            k: 8,
            types: None,
            date_from: None,
            date_to: None,
            doc_scope: None,
            mode: SearchMode::Hybrid,
            sort_by: SortBy::Relevance,
            include_old: false,
            current_only: true,
            max_per_doc: DEFAULT_MAX_PER_DOC,
            include_snippet: true,
            similar_to_chunk: None,
            seed_text: Some("an external passage about depreciation"),
        };
        let call = search_next_call("ignored", 16, &opts)?;
        assert!(
            call.contains(r#"seed_text="an external passage about depreciation""#),
            "continuation must preserve seed_text; got: {call}"
        );
        Ok(())
    }

    #[test]
    fn search_next_call_prefers_similar_to_over_seed_text() -> Result<()> {
        let source: legal_model::SourceId = TEST_SOURCE_ID.parse()?;
        let chunk = ChunkRef::new(TEST_GENERATION, source.clone(), 42)?;
        // similar_to_chunk wins if both are set — the continuation must
        // not also carry seed_text.
        let opts = SearchOptions {
            source: source.clone(),
            k: 8,
            types: None,
            date_from: None,
            date_to: None,
            doc_scope: None,
            mode: SearchMode::Vector,
            sort_by: SortBy::Relevance,
            include_old: false,
            current_only: true,
            max_per_doc: DEFAULT_MAX_PER_DOC,
            include_snippet: true,
            similar_to_chunk: Some(chunk),
            seed_text: Some("should be ignored"),
        };
        let call = search_next_call("ignored", 16, &opts)?;
        assert!(
            call.contains("similar_to_chunk={"),
            "continuation must preserve similar_to_chunk; got: {call}"
        );
        assert!(
            !call.contains("seed_text"),
            "similar_to_chunk wins — seed_text must not appear; got: {call}"
        );
        Ok(())
    }

    // ----- W2.4 Hit JSON serialisation skips unset currency fields -----

    #[test]
    fn hit_json_skips_unset_currency_fields() -> Result<()> {
        let hit = Hit {
            document: DocumentId::new(TEST_SOURCE_ID.parse()?, "DOC")?,
            title: "T".to_string(),
            doc_type: "Public_ruling".to_string(),
            date: None,
            anchor: None,
            snippet: Some("snip".to_string()),
            canonical_url: "https://x".to_string(),
            chunk: None,
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
            document: DocumentId::new(TEST_SOURCE_ID.parse()?, "DOC")?,
            title: "T".to_string(),
            doc_type: "Public_ruling".to_string(),
            date: Some("2022-07-01".to_string()),
            anchor: None,
            snippet: Some("snip".to_string()),
            canonical_url: "https://x".to_string(),
            chunk: None,
            next_call: None,
            withdrawn_date: Some("2025-10-31".to_string()),
            superseded_by: Some(DocumentId::new(TEST_SOURCE_ID.parse()?, "TR 2025/1")?),
            replaces: Some(DocumentId::new(TEST_SOURCE_ID.parse()?, "TR 2021/3")?),
            has_in_doc_links: None,
            has_related_docs: None,
            has_history: None,
        };
        let parsed: serde_json::Value = serde_json::from_str(&serde_json::to_string(&hit)?)?;
        assert_eq!(parsed["withdrawn_date"], json!("2025-10-31"));
        assert_eq!(
            parsed["superseded_by"],
            json!({"source": "ato", "native_id": "TR 2025/1"})
        );
        assert_eq!(
            parsed["replaces"],
            json!({"source": "ato", "native_id": "TR 2021/3"})
        );
        Ok(())
    }

    // ----- W2.4 integration: title hits filter out withdrawn docs by default -----

    #[test]
    fn collect_title_hits_excludes_withdrawn_by_default() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
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
            "INSERT INTO title_fts(source_id, native_id, title, headings)
             VALUES (?1, ?2, ?3, '')",
            params![
                TEST_SOURCE_ID,
                "DOC_CURRENT",
                "depreciation effective life rulings"
            ],
        )?;
        conn.execute(
            "INSERT INTO title_fts(source_id, native_id, title, headings)
             VALUES (?1, ?2, ?3, '')",
            params![
                TEST_SOURCE_ID,
                "DOC_WITHDRAWN",
                "depreciation effective life rulings"
            ],
        )?;
        // Update documents.title to match what title_fts holds (collect_title_hits
        // joins documents to fetch the displayed title back).
        conn.execute(
            "UPDATE documents SET title = ?1 WHERE source_id = ?2 AND native_id = ?3",
            params![
                "depreciation effective life rulings",
                TEST_SOURCE_ID,
                "DOC_CURRENT"
            ],
        )?;
        conn.execute(
            "UPDATE documents SET title = ?1 WHERE source_id = ?2 AND native_id = ?3",
            params![
                "depreciation effective life rulings",
                TEST_SOURCE_ID,
                "DOC_WITHDRAWN"
            ],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let conn = open_read()?;
            let source = test_source();
            // Default: current_only=true → withdrawn doc filtered out.
            let filter = test_doc_filter(&source, true, true);
            let hits = collect_title_hits(&conn, &source, "depreciation", 10, &filter)?;
            let doc_ids: Vec<String> = hits.iter().map(|h| h.document.public_ref()).collect();
            assert!(
                doc_ids.contains(&"ato:DOC_CURRENT".to_string()),
                "current doc should appear; got: {doc_ids:?}"
            );
            assert!(
                !doc_ids.contains(&"ato:DOC_WITHDRAWN".to_string()),
                "withdrawn doc should be filtered out by default; got: {doc_ids:?}"
            );

            // current_only=false → withdrawn doc returned with marker visible.
            let filter = test_doc_filter(&source, true, false);
            let hits = collect_title_hits(&conn, &source, "depreciation", 10, &filter)?;
            let withdrawn_hit = hits
                .iter()
                .find(|h| h.document.native_id == "DOC_WITHDRAWN")
                .expect("withdrawn doc should appear when current_only=false");
            assert_eq!(withdrawn_hit.withdrawn_date.as_deref(), Some("2023-06-15"));
            assert_eq!(
                withdrawn_hit
                    .superseded_by
                    .as_ref()
                    .map(|document| document.native_id.as_str()),
                Some("TR 2024/1")
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn collect_title_hits_prefers_direct_doc_id_hits() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        insert_doc_full(
            &conn,
            "PAC/19970038/203-50",
            Some("1997-01-01"),
            None,
            None,
            None,
        )?;
        conn.execute(
            "UPDATE documents SET type = 'PAC', title = ?1
             WHERE source_id = ?2 AND native_id = ?3",
            params![
                "Income Tax Assessment Act 1997 s 203-50",
                TEST_SOURCE_ID,
                "PAC/19970038/203-50"
            ],
        )?;
        conn.execute(
            "INSERT INTO title_fts(source_id, native_id, title, headings)
             VALUES (?1, ?2, ?3, '')",
            params![
                TEST_SOURCE_ID,
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
            "UPDATE documents SET type = 'PAC', title = ?1
             WHERE source_id = ?2 AND native_id = ?3",
            params![
                "Income Tax Assessment Act 1997 s 8-1",
                TEST_SOURCE_ID,
                "PAC/19970038/8-1"
            ],
        )?;
        conn.execute(
            "INSERT INTO title_fts(source_id, native_id, title, headings)
             VALUES (?1, ?2, ?3, '')",
            params![
                TEST_SOURCE_ID,
                "PAC/19970038/8-1",
                "Income Tax Assessment Act 1997 s 8-1"
            ],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let conn = open_read()?;
            let source = test_source();
            let filter = test_doc_filter(&source, false, true);
            let hits = collect_title_hits(&conn, &source, "PAC/19970038/203-50", 5, &filter)?;
            assert_eq!(hits[0].document.public_ref(), "ato:PAC/19970038/203-50");
            let hits = collect_title_hits(
                &conn,
                &source,
                "Income Tax Assessment Act 1997 s 8-1",
                5,
                &filter,
            )?;
            assert_eq!(hits[0].document.public_ref(), "ato:PAC/19970038/8-1");
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn get_definition_returns_matching_entry_only() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
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
            let context = DocumentId::new(TEST_SOURCE_ID.parse()?, "PAC/19970038/203-50")?;
            let json_str = get_definition(
                "corporate tax gross-up rate",
                GetDefinitionOptions {
                    source: None,
                    context_document: Some(context),
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
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
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
            "EV",
        )?;
        insert_definition_with_source(
            &conn,
            "def-car-aid",
            "car",
            "AID/AID20021000",
            "An interpretative decision glossary entry.",
            "AID",
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let context = DocumentId::new(TEST_SOURCE_ID.parse()?, "PAC/19860039/136")?;
            let json_str = get_definition(
                "car",
                GetDefinitionOptions {
                    source: None,
                    context_document: Some(context),
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
            assert_eq!(definitions[0]["source"]["type"], json!("PAC"));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn get_definition_falls_back_to_configured_ordinary_dictionary() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, _db) = make_test_db()?;
        let dictionary_path = dir.path().join("ordinary.tsv");
        fs::write(
            &dictionary_path,
            "car\tcar\tnoun\ta road vehicle powered by an engine\n",
        )?;
        let _environment = TestEnvironment::set(&[
            ("LEGAL_MCP_DATA_DIR", dir.path().as_os_str()),
            (ORDINARY_DICTIONARY_PATH_ENV, dictionary_path.as_os_str()),
        ]);
        let result = get_definition(
            "car",
            GetDefinitionOptions {
                source: None,
                context_document: None,
                max_defs: 5,
            },
        );
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
        use crate::retrieval::parse_oewn_data_file;
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
            "INSERT INTO documents
                (source_id, native_id, type, title, canonical_url, downloaded_at,
                 content_hash, html, has_in_doc_links, has_related_docs, has_history)
             VALUES (?1, ?2, 'Public_ruling', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                TEST_SOURCE_ID,
                doc_id,
                format!("{doc_id} title"),
                format!("https://www.ato.gov.au/law/view/document?docid={doc_id}"),
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
    fn lexical_search_keeps_best_bm25_match_first() -> Result<()> {
        let _lock = lock_test_db();
        let (_dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        for (doc_id, chunk_id, text) in [
            ("DOC_RELEVANT", 1_i64, "common distinctive"),
            ("DOC_COMMON_A", 2_i64, "common"),
            ("DOC_COMMON_B", 3_i64, "common"),
        ] {
            insert_doc(&conn, doc_id)?;
            insert_chunk(&conn, chunk_id, doc_id, 0, text)?;
            conn.execute(
                "INSERT INTO chunks_fts(rowid, text) VALUES (?, ?)",
                params![chunk_id, text],
            )?;
        }

        let source = test_source();
        let filter = test_doc_filter(&source, false, true);
        let hits = lexical_search(&conn, &source, "common distinctive", &filter, 10)?;
        assert_eq!(hits.len(), 3, "OR semantics should retain partial matches");
        assert_eq!(hits[0].chunk_id, 1);
        assert!(hits[0].score > hits[1].score);
        Ok(())
    }

    #[test]
    fn keyword_search_matches_mixed_judgment_title_and_body_terms() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        let doc_id = "JUD/2025ATC10-742/00001";
        let title = "Body by Michael Pty Ltd v Industry Innovation and Science Australia";
        insert_doc_full(&conn, doc_id, Some("2025-01-24"), None, None, None)?;
        conn.execute(
            "UPDATE documents SET type = 'JUD', title = ?1
             WHERE source_id = ?2 AND native_id = ?3",
            params![title, TEST_SOURCE_ID, doc_id],
        )?;
        conn.execute(
            "INSERT INTO title_fts(source_id, native_id, title, headings)
             VALUES (?1, ?2, ?3, '')",
            params![TEST_SOURCE_ID, doc_id, title],
        )?;
        let citation_chunk =
            "Body by Michael Pty Ltd v Industry Innovation and Science Australia [2025] ARTA 44";
        let evidence_chunk = "The absence of documentary evidence may create evidentiary difficulty, but any such absence is not of itself determinative.";
        for (chunk_id, ord, text) in [
            (10_i64, 0_i64, citation_chunk),
            (11_i64, 1_i64, evidence_chunk),
        ] {
            insert_chunk(&conn, chunk_id, doc_id, ord, text)?;
            conn.execute(
                "INSERT INTO chunks_fts(rowid, text) VALUES (?, ?)",
                params![chunk_id, text],
            )?;
        }

        insert_doc_full(
            &conn,
            "RUL/DECOY/00001",
            Some("2025-01-24"),
            None,
            None,
            None,
        )?;
        conn.execute(
            "UPDATE documents SET type = 'RUL', title = ?1
             WHERE source_id = ?2 AND native_id = 'RUL/DECOY/00001'",
            params![title, TEST_SOURCE_ID],
        )?;
        insert_chunk(
            &conn,
            12,
            "RUL/DECOY/00001",
            0,
            "Body Michael Industry Innovation Science Australia 2025 ARTA 44 absence documentary evidence not determinative",
        )?;
        conn.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (12, ?)",
            params!["Body Michael Industry Innovation Science Australia 2025 ARTA 44 absence documentary evidence not determinative"],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let types = vec!["JUD".to_string()];
            let source = test_source();
            let json_str = search(
                "Body by Michael Industry Innovation Science Australia 2025 ARTA 44 absence documentary evidence not determinative",
                SearchOptions {
                    source: source.clone(),
                    k: 10,
                    types: Some(&types),
                    date_from: None,
                    date_to: None,
                    doc_scope: None,
                    mode: SearchMode::Keyword,
                    sort_by: SortBy::Relevance,
                    include_old: false,
                    current_only: true,
                    max_per_doc: DEFAULT_MAX_PER_DOC,
                    include_snippet: true,
                    similar_to_chunk: None,
                    seed_text: None,
                },
                None,
            )?;
            let parsed: JsonValue = serde_json::from_str(&json_str)?;
            let hits = parsed["hits"].as_array().expect("hits array");
            assert!(hits.iter().any(|hit| hit["document"]
                == json!({
                    "source": TEST_SOURCE_ID,
                    "native_id": doc_id,
                })));
            assert!(hits.iter().all(|hit| hit["type"] == "JUD"));
            let title_hits = parsed["title_hits"].as_array().expect("title_hits array");
            assert!(title_hits.iter().any(|hit| hit["document"]
                == json!({
                    "source": TEST_SOURCE_ID,
                    "native_id": doc_id,
                })));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn keyword_search_isolates_sources_with_the_same_native_id() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        insert_doc(&conn, "SHARED")?;
        insert_chunk(&conn, 201, "SHARED", 0, "shared needle ATO material")?;
        conn.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (201, 'shared needle ATO material')",
            [],
        )?;
        conn.execute(
            "INSERT INTO sources(source_id, display_name) VALUES ('frl', 'Federal Register of Legislation')",
            [],
        )?;
        conn.execute(
            "INSERT INTO documents
                (source_id, native_id, type, title, canonical_url, downloaded_at, content_hash, html)
             VALUES ('frl', 'SHARED', 'Act', 'Shared Act',
                     'https://www.legislation.gov.au/SHARED/latest/text', ?1, 'frl-hash', ?2)",
            params![Utc::now().to_rfc3339(), compress_text("<article>FRL</article>")?],
        )?;
        conn.execute(
            "INSERT INTO chunks(chunk_id, source_id, native_id, ord, text)
             VALUES (202, 'frl', 'SHARED', 0, ?1)",
            [compress_text("shared needle Federal Register material")?],
        )?;
        conn.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (202, 'shared needle Federal Register material')",
            [],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let response = search(
                "shared needle",
                SearchOptions {
                    source: "frl".parse()?,
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
                    similar_to_chunk: None,
                    seed_text: None,
                },
                None,
            )?;
            let value: JsonValue = serde_json::from_str(&response)?;
            let hits = value["hits"].as_array().expect("hits");
            assert!(!hits.is_empty());
            assert!(hits.iter().all(|hit| hit["document"]["source"] == "frl"));
            Ok(())
        })
    }

    #[test]
    fn search_rejects_unknown_exact_doc_type() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        insert_doc(&conn, "JUD/TEST/00001")?;
        conn.execute(
            "UPDATE documents SET type = 'JUD'
             WHERE source_id = ?1 AND native_id = 'JUD/TEST/00001'",
            [TEST_SOURCE_ID],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let types = vec!["case".to_string()];
            let source = test_source();
            let err = search(
                "documentary evidence",
                SearchOptions {
                    source: source.clone(),
                    k: 10,
                    types: Some(&types),
                    date_from: None,
                    date_to: None,
                    doc_scope: None,
                    mode: SearchMode::Keyword,
                    sort_by: SortBy::Relevance,
                    include_old: false,
                    current_only: true,
                    max_per_doc: DEFAULT_MAX_PER_DOC,
                    include_snippet: true,
                    similar_to_chunk: None,
                    seed_text: None,
                },
                None,
            )
            .expect_err("unknown exact type should fail");
            let message = err.to_string();
            assert!(message.contains("case"));
            assert!(message.contains("stats.types"));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn test_search_hit_carries_navigation_flags() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
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
            let source = test_source();
            let json_str = search(
                "research development",
                SearchOptions {
                    source: source.clone(),
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
                    similar_to_chunk: None,
                    seed_text: None,
                },
                None,
            )?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let hit = parsed["hits"]
                .as_array()
                .and_then(|a| a.first())
                .expect("expected at least one hit");
            assert_eq!(
                hit["document"],
                json!({"source": TEST_SOURCE_ID, "native_id": "DOC_NAV"})
            );
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
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        insert_doc_with_nav_flags(&conn, "DOC_ANCHORS", 1, 1, 1)?;
        // One chunk to satisfy the in_doc target_chunk_id reference.
        insert_chunk(&conn, 100, "DOC_ANCHORS", 0, "body")?;
        conn.execute(
            "INSERT INTO doc_anchors
                (source_id, native_id, ord, kind, label, target_source_id,
                 target_native_id, target_chunk_id, target_pit)
             VALUES (?1, ?2, ?3, 'in_doc', 'Section A', ?1, ?2, ?4, NULL)",
            params![TEST_SOURCE_ID, "DOC_ANCHORS", 0_i64, 100_i64],
        )?;
        conn.execute(
            "INSERT INTO doc_anchors
                (source_id, native_id, ord, kind, label, target_source_id,
                 target_native_id, target_chunk_id, target_pit)
             VALUES (?1, ?2, ?3, 'sister', 'Errata', ?1, ?4, NULL, NULL)",
            params![TEST_SOURCE_ID, "DOC_ANCHORS", 1_i64, "DOC_SISTER"],
        )?;
        conn.execute(
            "INSERT INTO doc_anchors
                (source_id, native_id, ord, kind, label, target_source_id,
                 target_native_id, target_chunk_id, target_pit)
             VALUES (?1, ?2, ?3, 'history', 'Earlier version', ?1, ?4, NULL, ?5)",
            params![
                TEST_SOURCE_ID,
                "DOC_ANCHORS",
                2_i64,
                "DOC_HISTORY",
                "20200101000000"
            ],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = get_doc_anchors(DocumentId::new(test_source(), "DOC_ANCHORS")?)?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            assert_eq!(
                parsed["document"],
                json!({"source": TEST_SOURCE_ID, "native_id": "DOC_ANCHORS"})
            );
            let in_doc = parsed["in_doc"].as_array().unwrap();
            let related = parsed["related_docs"].as_array().unwrap();
            let history = parsed["historical_versions"].as_array().unwrap();
            assert_eq!(in_doc.len(), 1, "expected one in_doc anchor");
            assert_eq!(
                in_doc[0]["chunk"],
                json!({
                    "generation": TEST_GENERATION,
                    "source": TEST_SOURCE_ID,
                    "chunk_id": 100,
                })
            );
            assert_eq!(in_doc[0]["label"], json!("Section A"));
            assert_eq!(related.len(), 1);
            assert_eq!(
                related[0]["document"],
                json!({"source": TEST_SOURCE_ID, "native_id": "DOC_SISTER"})
            );
            assert_eq!(related[0]["label"], json!("Errata"));
            assert_eq!(history.len(), 1);
            assert_eq!(
                history[0]["document"],
                json!({"source": TEST_SOURCE_ID, "native_id": "DOC_HISTORY"})
            );
            assert_eq!(history[0]["pit"], json!("20200101000000"));
            assert_eq!(history[0]["label"], json!("Earlier version"));
            assert_eq!(history[0]["date"], json!("2020-01-01"));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn get_doc_anchors_resolves_in_doc_chunks_from_stored_html() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        insert_doc_full(
            &conn,
            "DOC_HTML_ANCHORS",
            Some("2024-01-01"),
            None,
            None,
            None,
        )?;
        conn.execute(
            "UPDATE documents SET html = ?1 WHERE source_id = ?2 AND native_id = ?3",
            params![
                compress_text(
                    r##"<nav><a href="#target">Target section</a></nav><h2 id="target">Target</h2>"##
                )?,
                TEST_SOURCE_ID,
                "DOC_HTML_ANCHORS"
            ],
        )?;
        conn.execute(
            "INSERT INTO chunks(chunk_id, source_id, native_id, ord, anchor, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                9001i64,
                TEST_SOURCE_ID,
                "DOC_HTML_ANCHORS",
                0i64,
                "target",
                compress_text("Target body")?,
            ],
        )?;
        conn.execute(
            "INSERT INTO doc_anchors
                (source_id, native_id, ord, kind, label, target_source_id,
                 target_native_id, target_chunk_id, target_pit)
             VALUES (?1, ?2, 0, 'in_doc', 'Target section', ?1, ?2, NULL, NULL)",
            params![TEST_SOURCE_ID, "DOC_HTML_ANCHORS"],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = get_doc_anchors(DocumentId::new(test_source(), "DOC_HTML_ANCHORS")?)?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            assert_eq!(parsed["in_doc"].as_array().unwrap().len(), 1);
            assert_eq!(parsed["in_doc"][0]["label"], json!("Target section"));
            assert_eq!(
                parsed["in_doc"][0]["chunk"],
                json!({
                    "generation": TEST_GENERATION,
                    "source": TEST_SOURCE_ID,
                    "chunk_id": 9001,
                })
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn test_get_doc_anchors_pit_to_date() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        insert_doc_with_nav_flags(&conn, "DOC_PIT", 0, 0, 1)?;
        conn.execute(
            "INSERT INTO doc_anchors
                (source_id, native_id, ord, kind, label, target_source_id,
                 target_native_id, target_chunk_id, target_pit)
             VALUES (?1, ?2, ?3, 'history', 'Original ruling', ?1, ?4, NULL, ?5)",
            params![
                TEST_SOURCE_ID,
                "DOC_PIT",
                0_i64,
                "TR_1996_X",
                "19960320000001"
            ],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = get_doc_anchors(DocumentId::new(test_source(), "DOC_PIT")?)?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let history = parsed["historical_versions"].as_array().unwrap();
            assert_eq!(history.len(), 1);
            assert_eq!(
                history[0]["document"],
                json!({"source": TEST_SOURCE_ID, "native_id": "TR_1996_X"})
            );
            assert_eq!(history[0]["pit"], json!("19960320000001"));
            assert_eq!(history[0]["date"], json!("1996-03-20"));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn test_pit_to_date_handles_short_or_non_numeric_input() {
        assert_eq!(
            pit_to_date("19960320000001"),
            Some("1996-03-20".to_string())
        );
        assert_eq!(pit_to_date("19960320"), Some("1996-03-20".to_string()));
        assert!(
            pit_to_date("1996032").is_none(),
            "shorter than 8 chars returns None"
        );
        assert!(pit_to_date("abcd0320000000").is_none());
    }

    #[test]
    fn ensure_model_rejects_incomplete_bundle_before_marker() -> Result<()> {
        let _lock = lock_test_db();
        let data = tempdir()?;
        prepare_test_generation(data.path())?;
        let release = tempdir()?;
        let release_dir = release.path();
        let model_bundle = release_dir.join("model-bundle.tar.zst");
        write_test_tar_zst(
            &model_bundle,
            &[
                ("model_fp16.onnx", b"dummy onnx bytes"),
                ("tokenizer.json", br#"{"version":"1.0","truncation":null}"#),
            ],
        )?;
        let model_bundle_bytes = fs::read(&model_bundle)?;
        let mut manifest = sample_manifest(SUPPORTED_SCHEMA_VERSION, env!("CARGO_PKG_VERSION"));
        manifest.index_version = "test-incomplete-model-bundle".to_string();
        manifest.model = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: sha256_hex(&model_bundle_bytes),
            size: model_bundle_bytes.len() as u64,
            url: "model-bundle.tar.zst".to_string(),
        };
        let context = UrlContext {
            manifest_dir: Some(release_dir.to_path_buf()),
            manifest_base_url: None,
        };

        with_data_dir(data.path(), || -> Result<()> {
            let err = stage_model(&manifest, &context, &staging_dir()?).unwrap_err();
            assert!(
                err.to_string().contains("model_fp16.onnx_data"),
                "expected missing model data error, got: {err}"
            );
            assert!(
                !live_dir()?.join(".model.sha256").exists(),
                "incomplete bundle must not mark the model installed"
            );
            assert!(
                !model_data_path()?.exists(),
                "incomplete bundle must not partially install model data"
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn stage_model_rejects_insecure_or_unapproved_remote_sources() -> Result<()> {
        let _lock = lock_test_db();
        let data = tempdir()?;
        prepare_test_generation(data.path())?;
        with_data_dir(data.path(), || -> Result<()> {
            for (url, expected) in [
                ("http://github.com/model.tar.zst", "must use HTTPS"),
                (
                    "https://example.com/model.tar.zst",
                    "unapproved acquisition hostname",
                ),
                (
                    "https://127.0.0.1/model.tar.zst",
                    "unapproved acquisition hostname",
                ),
            ] {
                let mut manifest = sample_manifest(SUPPORTED_SCHEMA_VERSION, "");
                manifest.model = ModelInfo {
                    id: EMBEDDING_MODEL_ID.to_string(),
                    sha256: "0000000000000000000000000000000000000000000000000000000000000000"
                        .to_string(),
                    size: 1,
                    url: url.to_string(),
                };
                let context = UrlContext {
                    manifest_dir: None,
                    manifest_base_url: None,
                };
                let error = stage_model(
                    &manifest,
                    &context,
                    &data.path().join("remote-model-staging"),
                )
                .unwrap_err();
                assert!(
                    error.to_string().contains(expected),
                    "expected `{expected}` for {url}, got: {error:#}"
                );
            }
            Ok(())
        })
    }

    #[test]
    fn stage_model_rejects_local_artifacts_outside_manifest_directory() -> Result<()> {
        let _lock = lock_test_db();
        let data = tempdir()?;
        prepare_test_generation(data.path())?;
        let release = tempdir()?;
        let outside = tempdir()?;
        let artifact = outside.path().join("model-bundle.tar.zst");
        fs::write(&artifact, b"x")?;
        let mut manifest = sample_manifest(SUPPORTED_SCHEMA_VERSION, "");
        manifest.model = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: sha256_hex(b"x"),
            size: 1,
            url: artifact.display().to_string(),
        };
        let context = UrlContext {
            manifest_dir: Some(release.path().to_path_buf()),
            manifest_base_url: None,
        };

        with_data_dir(data.path(), || -> Result<()> {
            let error = stage_model(
                &manifest,
                &context,
                &data.path().join("local-model-staging"),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("model artifact escaped"),
                "unexpected error: {error:#}"
            );
            Ok(())
        })
    }

    // ===== Slim Search Surface ============================================

    /// Helper: build a Hit with the slim contract. Tests below pin that the
    /// wire shape stays slim (no score, no ord, no debug metadata).
    fn make_test_hit() -> Hit {
        let source: legal_model::SourceId = TEST_SOURCE_ID.parse().expect("valid test source");
        Hit {
            document: DocumentId::new(source.clone(), "DOC").expect("valid test document"),
            title: "T".to_string(),
            doc_type: "Public_ruling".to_string(),
            date: None,
            anchor: None,
            snippet: Some("snip".to_string()),
            canonical_url: "https://x".to_string(),
            chunk: Some(ChunkRef::new(TEST_GENERATION, source, 1).expect("valid test chunk")),
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
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&hit).unwrap()).unwrap();
        for forbidden in ["score", "ord", "ranking", "embedding_model_id"] {
            assert!(
                parsed.get(forbidden).is_none(),
                "slim Hit JSON must not expose `{forbidden}`; got {parsed:?}"
            );
        }
    }

    #[test]
    fn dedup_per_doc_uses_best_chunk_score_without_tail_sum() {
        let mut hits: Vec<VectorHit> = Vec::with_capacity(15);
        for i in 0..10 {
            hits.push(VectorHit {
                chunk_id: i as i64 + 1,
                score: 0.95 - (i as f64) * 0.02,
            });
        }
        for j in 0..5 {
            hits.push(VectorHit {
                chunk_id: 11 + j as i64,
                score: 0.010 + (j as f64) * 0.005,
            });
        }

        let mut meta: HashMap<i64, CandidateMeta> = HashMap::new();
        for i in 0..10 {
            meta.insert(
                i as i64 + 1,
                CandidateMeta {
                    document: DocumentId::new(
                        TEST_SOURCE_ID.parse().expect("valid test source"),
                        format!("DOC_H{i}"),
                    )
                    .expect("valid test document"),
                    is_intro: false,
                },
            );
        }
        for j in 0..5 {
            meta.insert(
                11 + j as i64,
                CandidateMeta {
                    document: DocumentId::new(
                        TEST_SOURCE_ID.parse().expect("valid test source"),
                        "DOC_TAIL_ONLY",
                    )
                    .expect("valid test document"),
                    is_intro: false,
                },
            );
        }

        let deduped = dedup_per_doc(hits, &meta, 11, 1);
        let tail_position = deduped
            .iter()
            .position(|hit| meta[&hit.chunk_id].document.native_id == "DOC_TAIL_ONLY")
            .expect("tail-only doc should appear in frontier");
        assert_eq!(tail_position, 10);

        let mut counts: HashMap<&str, usize> = HashMap::new();
        for hit in &deduped {
            *counts
                .entry(meta[&hit.chunk_id].document.native_id.as_str())
                .or_insert(0) += 1;
        }
        for (doc, n) in &counts {
            assert_eq!(*n, 1, "max_per_doc=1 violated for {doc}: {n} chunks");
        }
    }

    #[test]
    fn manifest_rejects_an_unknown_format_field() {
        let m = sample_manifest(SUPPORTED_SCHEMA_VERSION + 1, "");
        let err = validate_manifest(&m).expect_err("newer manifest should be rejected");
        assert!(
            err.to_string().contains("not supported"),
            "expected unsupported-schema error, got: {err}"
        );
    }

    #[test]
    fn test_get_doc_anchors_includes_cited_by() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        // Three docs cite TARGET. Two are 2024 dated (modern); one 2010.
        // `cited_by` should order by date DESC.
        insert_doc(&conn, "TARGET")?;
        insert_doc_full(&conn, "CITER_2024A", Some("2024-06-15"), None, None, None)?;
        insert_doc_full(&conn, "CITER_2024B", Some("2024-01-10"), None, None, None)?;
        insert_doc_full(&conn, "CITER_2010", Some("2010-02-02"), None, None, None)?;
        conn.execute(
            "UPDATE documents SET title = 'Citer 2024'
             WHERE source_id = ?1 AND native_id = 'CITER_2024A'",
            [TEST_SOURCE_ID],
        )?;
        conn.execute(
            "UPDATE documents SET title = 'Citer 2024 B'
             WHERE source_id = ?1 AND native_id = 'CITER_2024B'",
            [TEST_SOURCE_ID],
        )?;
        conn.execute(
            "UPDATE documents SET type = 'Cases', title = 'Citer 2010'
             WHERE source_id = ?1 AND native_id = 'CITER_2010'",
            [TEST_SOURCE_ID],
        )?;
        // One citing chunk per citer; TARGET is the citation target.
        insert_chunk(&conn, 10, "CITER_2024A", 0, "see [doc:ato:TARGET]")?;
        insert_chunk(&conn, 11, "CITER_2024B", 0, "also [doc:ato:TARGET]")?;
        insert_chunk(&conn, 12, "CITER_2010", 0, "refer [doc:ato:TARGET]")?;
        conn.execute(
            "INSERT INTO citations
                (source_chunk_id, source_id, source_native_id, target_source_id, target_native_id)
             VALUES (?1, ?2, ?3, ?2, ?4)",
            params![10_i64, TEST_SOURCE_ID, "CITER_2024A", "TARGET"],
        )?;
        conn.execute(
            "INSERT INTO citations
                (source_chunk_id, source_id, source_native_id, target_source_id, target_native_id)
             VALUES (?1, ?2, ?3, ?2, ?4)",
            params![11_i64, TEST_SOURCE_ID, "CITER_2024B", "TARGET"],
        )?;
        conn.execute(
            "INSERT INTO citations
                (source_chunk_id, source_id, source_native_id, target_source_id, target_native_id)
             VALUES (?1, ?2, ?3, ?2, ?4)",
            params![12_i64, TEST_SOURCE_ID, "CITER_2010", "TARGET"],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = get_doc_anchors(DocumentId::new(test_source(), "TARGET")?)?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let cited_by = parsed["cited_by"].as_array().unwrap();
            assert_eq!(cited_by.len(), 3);
            // Date-DESC order.
            assert_eq!(
                cited_by[0]["document"],
                json!({"source": TEST_SOURCE_ID, "native_id": "CITER_2024A"})
            );
            assert_eq!(cited_by[0]["date"], json!("2024-06-15"));
            assert_eq!(cited_by[0]["title"], json!("Citer 2024"));
            assert_eq!(cited_by[0]["type"], json!("Public_ruling"));
            assert_eq!(
                cited_by[1]["document"],
                json!({"source": TEST_SOURCE_ID, "native_id": "CITER_2024B"})
            );
            assert_eq!(
                cited_by[2]["document"],
                json!({"source": TEST_SOURCE_ID, "native_id": "CITER_2010"})
            );
            // Total field omitted when no truncation occurred.
            assert!(parsed.get("cited_by_total").is_none());
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn test_get_doc_anchors_cited_by_truncates_with_total() -> Result<()> {
        let _lock = lock_test_db();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        insert_doc(&conn, "POPULAR")?;
        // Insert CITED_BY_LIMIT + 5 citers so truncation kicks in.
        let count = CITED_BY_LIMIT + 5;
        for i in 0..count {
            let citer = format!("CITER_{:03}", i);
            insert_doc_full(&conn, &citer, Some("2024-01-01"), None, None, None)?;
            conn.execute(
                "UPDATE documents SET title = ?1 WHERE source_id = ?2 AND native_id = ?3",
                params![format!("Citer {i}"), TEST_SOURCE_ID, &citer],
            )?;
            insert_chunk(&conn, (1000 + i) as i64, &citer, 0, "[doc:ato:POPULAR]")?;
            conn.execute(
                "INSERT INTO citations
                    (source_chunk_id, source_id, source_native_id,
                     target_source_id, target_native_id)
                 VALUES (?1, ?2, ?3, ?2, ?4)",
                params![(1000 + i) as i64, TEST_SOURCE_ID, citer, "POPULAR"],
            )?;
        }
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = get_doc_anchors(DocumentId::new(test_source(), "POPULAR")?)?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            let cited_by = parsed["cited_by"].as_array().unwrap();
            assert_eq!(cited_by.len(), CITED_BY_LIMIT);
            assert_eq!(parsed["cited_by_total"], json!(count as i64));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn test_load_chunk_embedding_roundtrip() -> Result<()> {
        let _lock = lock_test_db();
        let (_dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
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
        let loaded = load_chunk_embedding(&conn, &test_source(), 42)?;
        let expected: Vec<i8> = bytes.iter().map(|b| *b as i8).collect();
        assert_eq!(loaded.to_vec(), expected);
        Ok(())
    }

    #[test]
    fn test_load_chunk_embedding_missing_chunk_errors() -> Result<()> {
        let _lock = lock_test_db();
        let (_dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        let err = load_chunk_embedding(&conn, &test_source(), 99999).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no stored embedding"),
            "unexpected error: {msg}"
        );
        Ok(())
    }

    #[test]
    fn test_derive_citations_extracts_doc_markers() -> Result<()> {
        let _lock = lock_test_db();
        let (_dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
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
            "see [doc:ato:T1] and [doc:ato:T2@19960320000001] and [doc:ato:T2 view=HISTFT] and [doc:ato:SRC] and [doc:ato:T1]",
        )?;
        // pre-populate with stale rows so we can confirm clear + repopulate
        conn.execute(
            "INSERT INTO citations
                (source_chunk_id, source_id, source_native_id,
                 target_source_id, target_native_id)
             VALUES (?1, ?2, ?3, ?2, ?4)",
            params![10_i64, TEST_SOURCE_ID, "SRC", "STALE"],
        )?;

        let source_id: legal_model::SourceId = TEST_SOURCE_ID.parse()?;
        derive_citations(&conn, &source_id)?;

        let rows: Vec<(i64, String, String)> = conn
            .prepare(
                "SELECT source_chunk_id, source_native_id, target_native_id
                 FROM citations
                 WHERE source_id = ?1
                 ORDER BY target_native_id",
            )?
            .query_map([TEST_SOURCE_ID], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
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

    // Tests that touch the global data dir env var cannot run in
    // parallel — serialise them through a single mutex.
    static TEST_DB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_annotate_doc_refs_rewrites_external_markers_to_fetch() -> Result<()> {
        let source_id: legal_model::SourceId = TEST_SOURCE_ID.parse()?;
        let self_document = DocumentId::new(source_id.clone(), "SRC")?;
        let mut native_ids = HashSet::new();
        native_ids.insert("JUD/HAYES".to_string());
        native_ids.insert("PAC/19360027/6".to_string());
        let mut corpus = HashMap::new();
        corpus.insert(source_id, Arc::new(native_ids));
        assert_eq!(
            annotate_doc_refs_with("see [doc:ato:JUD/HAYES]", &self_document, &corpus,)?,
            "see [doc:ato:JUD/HAYES]"
        );
        assert_eq!(
            annotate_doc_refs_with("see [doc:ato:LRP/117CLR514]", &self_document, &corpus,)?,
            "see [fetch:legal://ato/LRP%2F117CLR514]"
        );
        assert_eq!(
            annotate_doc_refs_with("see [doc:ato:SRC]", &self_document, &corpus)?,
            "see [doc:ato:SRC]"
        );
        assert_eq!(
            annotate_doc_refs_with("see [fetch:legal://ato/LRP%2FX]", &self_document, &corpus,)?,
            "see [fetch:legal://ato/LRP%2FX]"
        );
        assert_eq!(
            annotate_doc_refs_with("see [doc:ato:LRP/X view=HISTFT]", &self_document, &corpus,)?,
            "see [fetch:legal://ato/LRP%2FX?view=HISTFT]"
        );
        assert_eq!(
            annotate_doc_refs_with(
                "see [doc:ato:LRP/X@19960320000001]",
                &self_document,
                &corpus,
            )?,
            "see [fetch:legal://ato/LRP%2FX?pit=19960320000001]"
        );
        assert_eq!(
            annotate_doc_refs_with(
                "see [doc:ato:LRP/X@19960320000001 view=HISTFT]",
                &self_document,
                &corpus,
            )?,
            "see [fetch:legal://ato/LRP%2FX?pit=19960320000001&view=HISTFT]"
        );
        assert_eq!(
            annotate_doc_refs_with(
                "[doc:ato:JUD/HAYES] and [doc:ato:LRP/X] and [doc:ato:PAC/19360027/6]",
                &self_document,
                &corpus,
            )?,
            "[doc:ato:JUD/HAYES] and [fetch:legal://ato/LRP%2FX] and [doc:ato:PAC/19360027/6]"
        );
        Ok(())
    }

    #[test]
    fn test_ato_marker_tail_to_query_suffix_known_shapes() {
        assert_eq!(ato_marker_tail_to_query_suffix(""), "");
        assert_eq!(
            ato_marker_tail_to_query_suffix("@19960320000001"),
            "?pit=19960320000001"
        );
        assert_eq!(
            ato_marker_tail_to_query_suffix(" view=HISTFT"),
            "?view=HISTFT"
        );
        assert_eq!(
            ato_marker_tail_to_query_suffix("@19960320000001 view=HISTFT"),
            "?pit=19960320000001&view=HISTFT"
        );
        // Unknown qualifier shape: empty suffix so the rewritten marker
        // stays a syntactically valid URI form.
        assert_eq!(ato_marker_tail_to_query_suffix(" something=weird"), "");
    }
}
