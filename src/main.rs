use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use fs2::FileExt;
use reqwest::blocking::Client;
use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::Url;

mod build;
mod chunker;
mod config;
mod db;
mod extract;
mod html;
mod pack;
mod retrieval;
mod rules;
mod search;
mod semantic;
mod source;

use config::{
    daemon_log_path, default_manifest_url, http_config_path, live_dir, model_data_path, model_path,
    pick_free_port, spawn_lock_path, tokenizer_path, HttpConfig,
};
use db::enforce_db_schema_version;
use build::{
    build_corpus, bundle_localize_manifest, check_base_release, check_build_checkpoint,
    materialize_base_release, package_corpus, update_manifest_with_db, BuildCorpusArgs,
};
use retrieval::{
    fetch_external_doc, get_asset_mcp, get_chunks_mcp,
    get_definition, get_doc_anchors_mcp, pit_to_date,
    GetDefinitionOptions,
};
use search::{
    search, search_cli, SearchOptions,
};
use source::{
    apply_update, check_for_update_availability, doctor, link_download, manifest_fingerprint,
    preview_update, scrape_diff, snapshot_reduce, stats, tree_crawl, DocRef, LinkDownloadArgs,
    Manifest, ModelInfo, StagedModel, UpdateAvailability, UpdateSummary,
};
use semantic::{
    SemanticEncodeStats, SemanticModelPaths, SemanticRuntime,
    EMBEDDING_MODEL_HF_FILES,
};
#[cfg(test)]
use semantic::dot_i8_scalar_reference;

pub(crate) const APP_NAME: &str = "ato-mcp";
pub(crate) const DEFAULT_RELEASES_URL: &str =
    "https://github.com/gunba/ato-mcp/releases/latest/download";
pub(crate) const DEFAULT_K: usize = 8;
pub(crate) const MAX_K: usize = 50;
/// Cap on the `title_hits` sidebar `search` returns alongside chunk hits.
/// Direct doc_id / ATO-link matches always lead; the BM25 title remainder
/// fills the rest.
pub(crate) const TITLE_HITS_K: usize = 10;
pub(crate) const SNIPPET_CHARS: usize = 280;
// [EM-05] Stored semantic vectors are the first 256 dimensions of the
// model output after normalization + int8 quantization.
pub(crate) const EMBEDDING_DIM: usize = 256;
// [EM-03] The tokenizer truncates semantic inputs and pads dynamically to
// each batch's max sequence length.
pub(crate) const EMBEDDING_INPUT_MAX_TOKENS: usize = 1024;
// [EM-02] Granite ONNX inputs use source-derived text directly; no
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
pub(crate) const ORDINARY_DICTIONARY_PATH_ENV: &str = "ATO_MCP_DICTIONARY_PATH";
/// On-disk schema version this binary supports. Bump when introducing
/// schema changes; binaries reject any corpus whose schema does not match.
const SUPPORTED_SCHEMA_VERSION: u32 = 9;
/// Single release manifest format (`Manifest.schema_version`) this binary
/// ingests. No legacy manifest layouts are accepted.
const SUPPORTED_MANIFEST_VERSION: u32 = 5;
pub(crate) const EMBEDDING_MODEL_ID: &str = "granite-embedding-small-r2-fp16-256d";
pub(crate) const DEFAULT_MAX_PER_DOC: usize = 2;
pub(crate) const HARD_MAX_PER_DOC: usize = 3;

#[derive(Parser)]
#[command(name = "ato-mcp", version, about = "Standalone ATO MCP server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

// [CC-01] One Rust binary owns both end-user commands and maintainer-only
// source/corpus commands; AGENTS.md documents which commands require the
// maintainer checkout, source corpus, model assets, and GPU.
// [CC-06] The external CLI is a closed clap enum: every command is explicit
// here, with no dynamic plugin subcommands or shell-completion surface.
#[derive(Subcommand)]
enum Command {
    /// stdio MCP shim. Auto-spawns the HTTP daemon on first use and proxies
    /// stdin/stdout to it. This is what MCP clients (Claude Code, Cursor, …)
    /// launch — no manual daemon management required.
    Serve {},
    /// Run the long-lived HTTP MCP daemon directly. Bypasses the stdio
    /// shim — use this for systemd / launchd / Scheduled Task setups, or
    /// when an MCP client connects over HTTP transport.
    Daemon {},
    /// Pick a port and persist HTTP config; prints the MCP client config to paste.
    InstallHttp {
        /// Override the auto-picked port (rare).
        #[arg(long)]
        port: Option<u16>,
        /// Suppress the MCP config block (only write http.json).
        #[arg(long)]
        quiet: bool,
    },
    /// Download or refresh the local corpus.
    Update {
        #[arg(long)]
        manifest_url: Option<String>,
        /// Print what would change vs the installed corpus without
        /// downloading anything or touching the live DB.
        #[arg(long)]
        check: bool,
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
        /// Runtime-embed this text as the query vector instead of `query`
        /// (e.g. a chunk from `fetch-external-doc`). Forces vector-only
        /// mode and returns no title hits.
        #[arg(long)]
        seed_text: Option<String>,
    },
    /// Fetch a document from ATO's live website and print its chunks.
    FetchExternalDoc {
        doc_id: String,
        #[arg(long)]
        pit: Option<String>,
        #[arg(long)]
        view: Option<String>,
    },
    /// In-binary build orchestrator (port of build.py). Reads
    /// `pages_dir/index.jsonl` (one record per line, with the fields
    /// canonical_id and payload_path), runs each doc through the cleaning
    /// pipeline, the chunker, the rules-engine metadata classifier and the
    /// embedder in-process, then writes the documents, chunks,
    /// chunk_embeddings, chunks_fts, title_fts, doc_anchors, definitions
    /// and citations tables, then writes pack files, per-doc asset blobs,
    /// manifest.json, and update.json to --out-dir.
    /// Supports same-output-dir checkpoint resume and previous-release
    /// seeding through --base-release-dir.
    /// [CC-05] Source refresh/download and corpus build are separate
    /// commands: the same ato_pages/ tree can feed repeated builds,
    /// base-release seeded builds, or release dry runs.
    Build {
        #[arg(long)]
        pages_dir: PathBuf,
        #[arg(long)]
        db_path: PathBuf,
        /// Granite embedding model checkout. Must contain tokenizer.json,
        /// onnx/model_fp16.onnx, and onnx/model_fp16.onnx_data.
        #[arg(long)]
        model_dir: PathBuf,
        /// Previous release directory to seed unchanged documents/packs from.
        #[arg(long)]
        base_release_dir: Option<PathBuf>,
        /// Output directory for pack files, asset blobs, manifest.json, and update.json.
        #[arg(long)]
        out_dir: PathBuf,
        #[arg(long, default_value_t = 3)]
        zstd_level: i32,
        #[arg(long)]
        limit: Option<usize>,
        /// Use the CUDA execution provider and fail if CUDA is unavailable.
        #[arg(long)]
        gpu: bool,
        /// Print cumulative build-stage timings to stderr.
        #[arg(long)]
        profile: bool,
    },
    /// Maintainer-only: validate that a release dir can seed the current
    /// build without falling off the fast path.
    #[command(hide = true)]
    CheckBaseRelease { release_dir: PathBuf },
    /// Maintainer-only: reconstruct a local base release from a published
    /// manifest and pack assets without running the embedding model.
    #[command(hide = true)]
    MaterializeBaseRelease {
        #[arg(long)]
        manifest_url: String,
        #[arg(long)]
        out_dir: PathBuf,
    },
    /// Maintainer-only: validate that a build output dir has a checkpoint
    /// compatible with the current source index/model settings.
    #[command(hide = true)]
    CheckBuildCheckpoint {
        #[arg(long)]
        release_dir: PathBuf,
        #[arg(long)]
        source_index_sha256: String,
        #[arg(long, default_value_t = 3)]
        zstd_level: i32,
    },
    /// Fetch compact statutory definitions for a term.
    GetDefinition {
        term: String,
        #[arg(long)]
        context_doc_id: Option<String>,
        #[arg(long, default_value_t = 5)]
        max_defs: usize,
    },
    /// Crawl the ATO browse-content tree and write nodes.jsonl + meta.json
    /// to a snapshot directory. Mirrors src/ato_mcp/scraper/tree_crawler.py
    /// + src/ato_mcp/scraper/snapshot.py.
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
    /// Reduce a snapshot to deduped_links.jsonl + dedup_summary.json +
    /// redundant_paths.json + skip_data_urls.json. Mirrors
    /// src/ato_mcp/scraper/reducer.py.
    SnapshotReduce {
        #[arg(long)]
        nodes_path: PathBuf,
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
    /// [SS-04] Maintainer source download defaults to 0.05s request pacing
    /// and four link-download workers; the rate lock serializes HTTP issuance.
    /// Download deduped ATO links to local payloads/<Category>/<slug>.html
    /// + index.jsonl. Mirrors src/ato_mcp/scraper/downloader.py:LinkDownloader.
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
    /// Strip FTS5 indexes off a copy of the built ato.db, VACUUM, and zstd-compress
    /// it into a shippable ato.db.zst artifact. Leaves the input DB untouched.
    /// Emits {path, sha256, size} JSON on stdout so the release writer can
    /// embed those into manifest.json. If --manifest is set, updates that
    /// manifest in-place to point `db: {url, sha256, size}` at the new artifact
    /// (URL = bare filename; publish-release rewrites it to a GitHub release URL
    /// later) and clears the legacy `documents[]`/`packs[]` arrays.
    PackageCorpus {
        /// Path to the canonical built ato.db (e.g. release/<tag>/ato.db).
        #[arg(long)]
        db_path: PathBuf,
        /// Output path for the compressed artifact (e.g. release/<tag>/ato.db.zst).
        #[arg(long)]
        out: PathBuf,
        /// zstd compression level. 19 maximises ratio; 3 is faster but bigger.
        #[arg(long, default_value_t = 19)]
        level: i32,
        /// Optional: in-place update this manifest's `db` field with the new
        /// artifact's sha256/size and clear the legacy pack arrays.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
    /// Rewrite a manifest.json so packs/model URLs point at filenames
    /// inside an offline bundle (with fresh SHA256 + size). Used by
    /// scripts/make-offline-bundle.sh as the post-bundling step. Also
    /// emits update.json (UpdateSummary).
    BundleLocalizeManifest {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        packs_dir: PathBuf,
        #[arg(long)]
        model_bundle: PathBuf,
    },
    /// Publish a corpus release to GitHub. Mirrors
    /// src/ato_mcp/indexer/release.py:publish — rewrites manifest URLs to
    /// release asset URLs, fixes embedding-model fields if they're
    /// placeholder/GitHub-hosted, optionally signs the manifest with
    /// minisign, then `gh release create` + `gh release upload`s
    /// manifest.json + manifest.json.minisig + update.json + every pack.
    PublishRelease {
        #[arg(long)]
        out_dir: PathBuf,
        #[arg(long)]
        tag: String,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        /// Force-overwrite existing release assets (`gh release upload --clobber`).
        #[arg(long, default_value_t = false)]
        overwrite: bool,
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
        /// Filesystem path to a minisign secret key. When set, the manifest
        /// is signed and the .minisig is uploaded alongside.
        #[arg(long)]
        sign_key: Option<PathBuf>,
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
        Command::Serve {} => serve_stdio_shim(),
        Command::Daemon {} => {
            let state = ServerState {
                update_notice: check_for_update_availability(&default_manifest_url())
                    .ok()
                    .flatten(),
                ..Default::default()
            };
            daemon(Arc::new(state))
        }
        Command::InstallHttp { port, quiet } => install_http(port, !quiet),
        Command::Update {
            manifest_url,
            check,
        } => {
            let url = manifest_url.unwrap_or_else(default_manifest_url);
            if check {
                let report = preview_update(&url)?;
                println!("{}", report);
            } else {
                let stats = apply_update(&url)?;
                println!(
                    "update complete: +{} ~{} -{} ({:.2} MB downloaded)",
                    stats.added,
                    stats.changed,
                    stats.removed,
                    stats.bytes_downloaded as f64 / 1_000_000.0
                );
            }
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
            seed_text,
        } => {
            let types = empty_vec_as_none(types);
            // Construct a transient ServerState so the CLI's `search` call
            // reuses the same lazy semantic runtime the MCP server does for
            // modes that need it.
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
                    seed_text: seed_text.as_deref(),
                },
            )?;
            println!("{}", out);
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
        Command::PublishRelease {
            out_dir,
            tag,
            repo,
            title,
            notes,
            overwrite,
            model_url,
            model_sha256,
            model_size,
            sign_key,
        } => publish_release(PublishReleaseArgs {
            out_dir,
            tag,
            repo,
            title,
            notes,
            overwrite,
            model_url,
            model_sha256,
            model_size,
            sign_key,
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
        }),
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
        Command::BundleLocalizeManifest {
            manifest,
            packs_dir,
            model_bundle,
        } => bundle_localize_manifest(&manifest, &packs_dir, &model_bundle),
        Command::FetchExternalDoc { doc_id, pit, view } => {
            println!(
                "{}",
                fetch_external_doc(&doc_id, pit.as_deref(), view.as_deref())?
            );
            Ok(())
        }
        Command::Build {
            pages_dir,
            db_path,
            model_dir,
            base_release_dir,
            out_dir,
            zstd_level,
            limit,
            gpu,
            profile,
        } => build_corpus(BuildCorpusArgs {
            pages_dir: &pages_dir,
            db_path: &db_path,
            model_dir: &model_dir,
            base_release_dir: base_release_dir.as_deref(),
            out_dir: &out_dir,
            zstd_level,
            limit,
            use_gpu: gpu,
            profile_enabled: profile,
        }),
        Command::CheckBaseRelease { release_dir } => check_base_release(&release_dir),
        Command::MaterializeBaseRelease {
            manifest_url,
            out_dir,
        } => materialize_base_release(&manifest_url, &out_dir),
        Command::CheckBuildCheckpoint {
            release_dir,
            source_index_sha256,
            zstd_level,
        } => check_build_checkpoint(&release_dir, &source_index_sha256, zstd_level),
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
    update_notice: Option<UpdateAvailability>,
    use_gpu: bool,
}

impl ServerState {
    fn new(use_gpu: bool) -> Self {
        Self {
            semantic_runtime: Mutex::new(None),
            semantic_model_paths: None,
            update_notice: None,
            use_gpu,
        }
    }

    fn with_model_paths(use_gpu: bool, semantic_model_paths: SemanticModelPaths) -> Self {
        Self {
            semantic_runtime: Mutex::new(None),
            semantic_model_paths: Some(semantic_model_paths),
            update_notice: None,
            use_gpu,
        }
    }

    fn encode_query_embedding(&self, query: &str) -> Result<[i8; EMBEDDING_DIM]> {
        let mut embeddings = self.encode_query_embeddings(&[query.to_string()])?;
        embeddings
            .pop()
            .ok_or_else(|| anyhow!("semantic encoder returned no query embedding"))
    }

    fn encode_query_embeddings(&self, queries: &[String]) -> Result<Vec<[i8; EMBEDDING_DIM]>> {
        let (embeddings, _stats) = self.encode_query_embeddings_with_stats(queries)?;
        Ok(embeddings)
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
            // [SW-04] ServerState lazily loads SemanticRuntime on the first
            // semantic query and reuses it for the process lifetime.
            let model_paths = match &self.semantic_model_paths {
                Some(paths) => paths.clone(),
                None => SemanticModelPaths::live()?,
            };
            *guard = Some(SemanticRuntime::load(self.use_gpu, &model_paths)?);
        }
        guard
            .as_mut()
            .expect("semantic runtime was just initialized")
            .encode_queries_with_stats(queries)
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new(false)
    }
}

// ----- External fetch (live ATO scrape) -----
//
// Ported subset of src/ato_mcp/indexer/extract.py: pick_container, strip_noise,
// strip_history_ui_controls, html_to_text. Used at runtime to follow [doc:X]
// markers whose target id isn't in the local corpus (subdivisions, paragraph
// refs, footnote pointers, historical PiT views). The full Python pipeline
// remains the build-time path; this is the minimum-viable subset that returns
// legible plain text to an agent.

pub(crate) const ATO_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const ATO_USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; ato-mcp/",
    env!("CARGO_PKG_VERSION"),
    "; +https://github.com/gunba/ato-mcp)"
);



// ----- publish-release (port of src/ato_mcp/indexer/release.py:publish) -----

pub(crate) const EMBEDDING_MODEL_HF_URL: &str =
    "hf://onnx-community/granite-embedding-small-english-r2-ONNX@1dc7835ba0cb9c76a3618d0bf0c427c97671b3c8";
pub(crate) const EMBEDDING_MODEL_HF_SIZE: u64 = 99_732_286;

struct PublishReleaseArgs {
    out_dir: PathBuf,
    tag: String,
    repo: Option<String>,
    title: Option<String>,
    notes: Option<String>,
    overwrite: bool,
    model_url: Option<String>,
    model_sha256: Option<String>,
    model_size: Option<u64>,
    sign_key: Option<PathBuf>,
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
    let packs_dir = args.out_dir.join("packs");
    if !manifest_path.exists() {
        bail!("no manifest at {}", manifest_path.display());
    }

    let repo = args
        .repo
        .clone()
        .or_else(|| std::env::var("GH_REPO").ok())
        .ok_or_else(|| anyhow!("--repo required (or set $GH_REPO)"))?;

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

    // Manifest schema 5+ ships a single ato.db.zst artifact. Schema 4
    // ships per-doc packs. Detect which shape we have and upload accordingly.
    let mut artifacts: Vec<PathBuf> = vec![manifest_path.clone()];
    let mut pack_files: Vec<PathBuf> = Vec::new();

    if let Some(db_info) = manifest.db.as_mut() {
        let filename = std::path::Path::new(&db_info.url)
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| db_info.url.clone());
        let local = args.out_dir.join(&filename);
        if !local.exists() {
            bail!(
                "manifest.db points at {} but {} does not exist; run `ato-mcp package-corpus --manifest <path>` first",
                filename,
                local.display()
            );
        }
        db_info.url = format!(
            "https://github.com/{repo}/releases/download/{tag}/{filename}",
            tag = args.tag,
        );
        artifacts.push(local);
    } else {
        if !packs_dir.exists() {
            bail!("no packs/ dir at {} and manifest.db is unset", packs_dir.display());
        }
        pack_files = fs::read_dir(&packs_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with("pack-") && s.ends_with(".bin.zst"))
            })
            .collect();
        pack_files.sort();
        if pack_files.is_empty() {
            bail!("no pack files and no manifest.db — nothing to upload");
        }
        for pack in &mut manifest.packs {
            let filename = std::path::Path::new(&pack.url)
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| pack.url.clone());
            pack.url = format!(
                "https://github.com/{repo}/releases/download/{tag}/{filename}",
                tag = args.tag,
            );
        }
        artifacts.extend(pack_files.iter().cloned());
    }

    // Save updated manifest.
    let pretty = serde_json::to_vec_pretty(&manifest)?;
    fs::write(&manifest_path, &pretty)?;

    // Generate update.json so end-users can probe quickly.
    let summary = UpdateSummary {
        schema_version: manifest.schema_version,
        index_version: manifest.index_version.clone(),
        min_client_version: manifest.min_client_version.clone(),
        model: manifest.model.clone(),
        document_count: manifest.documents.len(),
        pack_count: manifest.packs.len(),
        db_sha256: manifest.db.as_ref().map(|d| d.sha256.clone()),
        db_size: manifest.db.as_ref().map(|d| d.size),
        manifest_fingerprint: Some(manifest_fingerprint(&manifest)?),
    };
    let summary_path = args.out_dir.join("update.json");
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)?;
    artifacts.insert(1, summary_path);

    // [SL-07] Optional release signing shells out to the maintainer minisign
    // CLI and uploads the generated manifest.json.minisig artifact.
    if let Some(sign_key) = args.sign_key.as_deref() {
        let sig_path = args.out_dir.join("manifest.json.minisig");
        let status = std::process::Command::new("minisign")
            .args([
                "-S".as_ref(),
                "-s".as_ref(),
                sign_key.as_os_str(),
                "-m".as_ref(),
                manifest_path.as_os_str(),
                "-x".as_ref(),
                sig_path.as_os_str(),
            ])
            .status()
            .context("running minisign (is it installed?)")?;
        if !status.success() {
            bail!("minisign signing failed (exit {:?})", status.code());
        }
        artifacts.insert(1, sig_path);
    }

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

    // Upload artifacts.
    let mut upload = std::process::Command::new("gh");
    upload.args(["release", "upload", &args.tag, "--repo", &repo]);
    for a in &artifacts {
        upload.arg(a);
    }
    if args.overwrite {
        upload.arg("--clobber");
    }
    let st = upload.status().context("running gh release upload")?;
    if !st.success() {
        bail!("gh release upload failed (exit {:?})", st.code());
    }

    eprintln!(
        "ato-mcp publish-release: uploaded {} artifacts to {}@{}",
        artifacts.len(),
        repo,
        args.tag,
    );
    Ok(())
}

pub(crate) fn model_info_matches(left: &ModelInfo, right: &ModelInfo) -> bool {
    left.id == right.id
        && left.sha256 == right.sha256
        && left.size == right.size
        && left.url == right.url
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

/// Compute the (added, changed, removed) doc-set difference between the
/// installed manifest and a newly fetched one. The update flow always
/// rebuilds the live DB wholesale; this counter exists only to render the
/// "+a ~c -r" CLI summary printed by `ato-mcp update`. No code path
/// branches on the result.
/// [SL-08] Equality compares content_hash, pack_sha8, offset, and length;
/// the result is cosmetic update-summary telemetry, not delta-install logic.
pub(crate) fn diff_manifests(
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

pub(crate) fn fetch_bytes(url_or_path: &str, context: &UrlContext) -> Result<Vec<u8>> {
    fetch_bytes_with(url_or_path, context, &http_client()?)
}

// [UM-04] The Rust downloader is credential-free: no GitHub token env vars and no gh shell-out.
pub(crate) fn fetch_bytes_with(url_or_path: &str, context: &UrlContext, client: &Client) -> Result<Vec<u8>> {
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
        if !is_http_url(url_or_path) {
            bail!("release asset not found: {url_or_path}");
        }
    }
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
        if !is_http_url(url_or_path) {
            bail!("release asset not found: {url_or_path}");
        }
        fetch_http_to_file(url_or_path, dest)
    } else {
        fetch_http_to_file(url_or_path, dest)
    }
}

fn is_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
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

pub(crate) fn verify_sha256_bytes(bytes: &[u8], expected: &str) -> Result<()> {
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

    let bundle_url = resolve_manifest_asset(&manifest.model.url, context);
    let bundle = staging.join("model-bundle.tar.zst.part");
    let size = fetch_to_file(&bundle_url, context, &bundle)?;
    if size != manifest.model.size {
        bail!(
            "model bundle size mismatch: got {}, expected {}",
            size,
            manifest.model.size
        );
    }
    verify_sha256_file(&bundle, &manifest.model.sha256)?;
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

/// Cheap liveness check — returns true iff a request to the configured URL
/// gets a JSON-RPC parse error back (i.e. the daemon is up and willing to
/// reject malformed bodies). Any HTTP success on /mcp signals a live daemon
/// because the endpoint always responds to POSTs.
fn ping_daemon(url: &str) -> bool {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    client
        .post(url)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Spawn `ato-mcp daemon` as a detached background process and block until
/// it prints its readiness line on stderr. The child:
///   * runs in its own process group / detached so SIGINT to the shim doesn't kill it
///   * inherits no stdin/stdout (null), so it survives shim teardown
///   * pipes stderr to us only for the readiness handshake; afterwards we
///     drain the rest into the daemon log file on a background thread so the
///     pipe never fills and the daemon keeps running.
fn spawn_daemon_detached(cfg: &HttpConfig) -> Result<()> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("locating ato-mcp binary")?;
    let mut cmd = Command::new(&exe);
    cmd.arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    // Detach so the child survives the shim's exit and isn't in our process
    // group (so Ctrl-C / SIGTERM to the shim doesn't propagate).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS = 0x00000008
        cmd.creation_flags(0x00000008);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", exe.display()))?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("daemon stderr not captured"))?;
    let needle = format!("listening on {}", cfg.url());
    let mut reader = std::io::BufReader::new(stderr);
    let mut line = String::new();
    let mut early_lines = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .context("reading daemon readiness")?;
        if n == 0 {
            // Daemon exited before readiness. Try to surface what it said.
            let _ = child.wait();
            if early_lines.is_empty() {
                bail!("daemon exited before becoming ready");
            }
            bail!(
                "daemon exited before becoming ready: {}",
                early_lines.trim()
            );
        }
        if line.contains(&needle) {
            break;
        }
        early_lines.push_str(&line);
    }

    // Daemon is up. Drain the rest of its stderr into the log file on a
    // background thread so the pipe buffer never fills.
    if let Ok(log_path) = daemon_log_path() {
        std::thread::spawn(move || {
            if let Ok(mut log) = OpenOptions::new().create(true).append(true).open(&log_path) {
                let mut buf = [0u8; 4096];
                while let Ok(n) = reader.get_mut().read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let _ = log.write_all(&buf[..n]);
                }
            }
        });
    }

    // We intentionally drop `child` here so the OS reparents it; the
    // detached process keeps running.
    std::mem::forget(child);
    Ok(())
}

/// Ensure a daemon is running at the configured URL, spawning one if not.
/// Serialised across concurrent shim invocations via an exclusive file lock
/// so two parallel sessions don't race on the bind.
fn ensure_daemon_running(cfg: &HttpConfig) -> Result<()> {
    if ping_daemon(&cfg.url()) {
        return Ok(());
    }
    let lock_path = spawn_lock_path()?;
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    lock_file
        .lock_exclusive()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    // Recheck after acquiring the lock — another shim may have spawned the
    // daemon while we were waiting for the lock.
    if !ping_daemon(&cfg.url()) {
        spawn_daemon_detached(cfg)?;
    }
    // Lock releases on drop.
    drop(lock_file);
    Ok(())
}

/// MCP stdio shim: zero-config front-end that auto-starts the HTTP daemon
/// and proxies stdin/stdout to it. This is what MCP clients launch — they
/// never see HTTP, the daemon, or its port; the user never starts anything
/// manually.
fn serve_stdio_shim() -> Result<()> {
    let cfg = HttpConfig::load_or_init()?;
    ensure_daemon_running(&cfg)?;

    let url = cfg.url();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .context("building HTTP client")?;

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match post_to_daemon(&client, &url, &line) {
            Ok(r) => r,
            Err(_) => {
                // Daemon may have died mid-session. Try once to bring it
                // back and retry the request.
                ensure_daemon_running(&cfg)?;
                post_to_daemon(&client, &url, &line)?
            }
        };
        if let Some(body) = response {
            stdout.write_all(body.as_bytes())?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

fn post_to_daemon(
    client: &reqwest::blocking::Client,
    url: &str,
    body: &str,
) -> Result<Option<String>> {
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()?;
    let status = resp.status();
    if status.as_u16() == 204 {
        // Notification — no body to forward.
        return Ok(None);
    }
    let text = resp.text()?;
    if !status.is_success() {
        bail!("daemon returned HTTP {status}: {text}");
    }
    Ok(Some(text))
}

fn daemon(state: Arc<ServerState>) -> Result<()> {
    let cfg = HttpConfig::load_or_init()?;
    let addr = format!("{}:{}", cfg.bind, cfg.port);
    let server = tiny_http::Server::http(&addr).map_err(|e| {
        anyhow!(
            "bind {addr}: {e}. If the port is already in use, re-run `ato-mcp install-http --port <free-port>`."
        )
    })?;
    eprintln!("ato-mcp listening on {}", cfg.url());

    for request in server.incoming_requests() {
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            if let Err(err) = handle_http(request, &state) {
                eprintln!("ato-mcp http handler error: {err}");
            }
        });
    }
    Ok(())
}

fn handle_http(mut request: tiny_http::Request, state: &ServerState) -> Result<()> {
    use tiny_http::{Header, Method, Response};

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

    let mut body = String::new();
    request
        .as_reader()
        .read_to_string(&mut body)
        .context("reading request body")?;

    let response_json: Option<JsonValue> = match serde_json::from_str::<JsonValue>(&body) {
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
    let resp = Response::from_string(body)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap());
    request.respond(resp).map_err(|e| anyhow!("respond: {e}"))?;
    Ok(())
}

fn install_http(port_override: Option<u16>, print_config: bool) -> Result<()> {
    let cfg = match (HttpConfig::load()?, port_override) {
        (Some(existing), None) => existing,
        _ => {
            let port = match port_override {
                Some(p) => p,
                None => pick_free_port()?,
            };
            let cfg = HttpConfig {
                bind: "127.0.0.1".to_string(),
                port,
            };
            cfg.save()?;
            cfg
        }
    };
    if print_config {
        let url = cfg.url();
        println!("ato-mcp will listen on {url}");
        println!("Config written to {}", http_config_path()?.display());
        println!();
        println!("Claude Code:");
        println!("  claude mcp add --scope user --transport http ato {url}");
        println!();
        println!("Claude Desktop (claude_desktop_config.json):");
        let block = json!({
            "mcpServers": {
                "ato": { "type": "http", "url": url }
            }
        });
        println!("{}", serde_json::to_string_pretty(&block)?);
        println!();
        println!("Start the daemon with: ato-mcp serve");
    }
    Ok(())
}

fn handle_rpc(message: JsonValue, state: &ServerState) -> Option<JsonValue> {
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

fn handle_single_rpc(message: JsonValue, state: &ServerState) -> Option<JsonValue> {
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
            "instructions": server_instructions(state.update_notice.as_ref()),
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

fn call_tool(params: JsonValue, state: &ServerState) -> Result<JsonValue> {
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
                    seed_text: args.get("seed_text").and_then(|v| v.as_str()),
                },
                Some(state),
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
        "fetch_external_doc" => {
            let doc_id = required_str(&args, "doc_id")?;
            fetch_external_doc(
                doc_id,
                args.get("pit").and_then(|v| v.as_str()),
                args.get("view").and_then(|v| v.as_str()),
            )?
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

pub(crate) fn optional_i64(args: &JsonValue, name: &str) -> Option<i64> {
    args.get(name).and_then(|v| v.as_i64())
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


const ATO_MCP_USE_INSTRUCTIONS: &str = r##"Use `search` first. Search hits are chunk pointers, not authority; call `get_chunks` before relying on text. Use `get_doc_anchors` for in-doc navigation, related/history links, and cited-by. Use `fetch_external_doc` only for unindexed `[doc:X]` links; pass fetched chunk text to `search(seed_text=...)` to pivot back into the corpus. For historical or withdrawn material, set `current_only=false` and `include_old=true`."##;

fn server_instructions(update_notice: Option<&UpdateAvailability>) -> String {
    // [SW-02] Instructions are generated from live corpus stats.
    // [SW-03] Missing/unreadable stats fall back to static install guidance.
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
            ATO_MCP_USE_INSTRUCTIONS,
        ),
        None => format!(
            "ATO corpus is not yet installed on this machine. Tell the user to run `ato-mcp update` in their terminal to download the corpus (~4 GB, takes 5-10 min). After install completes, the MCP client should be restarted so this server picks up the new corpus.\n\n{}",
            ATO_MCP_USE_INSTRUCTIONS
        ),
    };
    match update_notice {
        Some(notice) => format!(
            "{body}\n\nA newer ATO corpus index is available (available: {}). Tell the user to run `ato-mcp update` in their terminal when convenient. The current MCP session will continue using the installed corpus until the update completes and the MCP client is restarted.",
            notice.available_index_version
        ),
        None => body,
    }
}

fn tool_descriptors() -> JsonValue {
    // [SW-01] Seven MCP tools are exposed by tool_descriptors/call_tool:
    // search, get_chunks, get_asset, get_doc_anchors, get_definition, stats,
    // fetch_external_doc.
    //   The surface stays small and explicit; unsupported tools fail through the
    //   normal tools/call error path.
    json!([
        {
            "name": "search",
            "description": "Search the local ATO corpus. Returns slim `hits` with chunk_id plus `title_hits`; fetch hit bodies with get_chunks. Use doc_scope for a doc_id or prefix like `PAC/%`. `seed_text` or `similar_to_chunk_id` runs vector-only related search.",
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
                    "current_only": {"type": "boolean"},
                    "similar_to_chunk_id": {"type": "integer"},
                    "seed_text": {"type": "string"},
                    "include_snippet": {"type": "boolean"},
                    "format": {"type": "string", "enum": ["json"], "default": "json"}
                },
                "required": ["query"]
            }
        },
        {
            "name": "get_asset",
            "description": "Resolve an `[asset:X]` reference to a local file path and metadata.",
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
            "description": "Return in-doc anchors, related/history links, and cited-by docs for a doc_id. `in_doc` entries carry chunk_id for get_chunks.",
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
            "description": "Fetch chunk bodies by chunk_id, with optional before/after neighbour chunks. Text may contain `[doc:X]` and `[asset:X]` markers.",
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
            "description": "Fetch compact statutory definitions for a term, with ordinary-meaning fallback when none are indexed.",
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
            "description": "Return index version, counts, search policy, and prefix breakdown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "format": {"type": "string", "enum": ["json"], "default": "json"}
                }
            }
        },
        {
            "name": "fetch_external_doc",
            "description": "Fetch an unindexed live ATO doc_id as `{ord, anchor, text}` chunks. Use for specific `[doc:X]` links; some TOC/container ids are not externally fetchable.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "doc_id": {"type": "string"},
                    "pit": {"type": "string"},
                    "view": {"type": "string"}
                },
                "required": ["doc_id"]
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::*;
    use crate::config::*;
    use crate::db::*;
    use crate::extract::*;
    use crate::html::*;
    use crate::pack::*;
    use crate::retrieval::*;
    use crate::search::*;
    use crate::semantic::*;
    use crate::source::*;
    use base64::Engine as _;
    use chrono::Utc;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use rusqlite::types::Value;
    use rusqlite::Connection;
    use std::collections::HashSet;
    use std::io::Cursor;
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
    fn metadata_extract_pub_date_handles_utf8_boundary() {
        let mut text = "a".repeat(1999);
        text.push('•');
        text.push_str(" 1 January 2024");
        assert_eq!(metadata_extract_pub_date(&text), None);
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

    // ----- W1.5 manifest version guards -----

    #[test]
    fn manifest_compat_accepts_current_schema() {
        let m = sample_manifest(SUPPORTED_MANIFEST_VERSION as i64, "");
        assert!(enforce_manifest_compatibility(&m).is_ok());
    }

    #[test]
    fn manifest_compat_rejects_newer_schema() {
        let m = sample_manifest((SUPPORTED_MANIFEST_VERSION + 1) as i64, "");
        let err = enforce_manifest_compatibility(&m).unwrap_err();
        assert!(
            err.to_string().contains("not supported"),
            "expected unsupported-schema message, got: {err}"
        );
    }

    #[test]
    fn manifest_compat_rejects_older_schema() {
        let m = sample_manifest((SUPPORTED_MANIFEST_VERSION - 1) as i64, "");
        let err = enforce_manifest_compatibility(&m).unwrap_err();
        assert!(
            err.to_string().contains("not supported"),
            "expected unsupported-schema message, got: {err}"
        );
    }

    #[test]
    fn manifest_json_rejects_legacy_or_unknown_fields() -> Result<()> {
        let mut with_reranker = serde_json::to_value(sample_manifest(
            SUPPORTED_MANIFEST_VERSION as i64,
            env!("CARGO_PKG_VERSION"),
        ))?;
        with_reranker["reranker"] = JsonValue::Null;
        assert!(
            serde_json::from_value::<Manifest>(with_reranker).is_err(),
            "legacy reranker field must not be silently ignored"
        );

        let mut with_legacy_docs = serde_json::to_value(sample_manifest(
            SUPPORTED_MANIFEST_VERSION as i64,
            env!("CARGO_PKG_VERSION"),
        ))?;
        with_legacy_docs["docs"] = json!([]);
        assert!(
            serde_json::from_value::<Manifest>(with_legacy_docs).is_err(),
            "legacy docs field must not be silently ignored"
        );

        let mut with_tokenizer_sha = serde_json::to_value(sample_manifest(
            SUPPORTED_MANIFEST_VERSION as i64,
            env!("CARGO_PKG_VERSION"),
        ))?;
        with_tokenizer_sha["model"]["tokenizer_sha256"] = json!("legacy");
        assert!(
            serde_json::from_value::<Manifest>(with_tokenizer_sha).is_err(),
            "legacy tokenizer_sha256 field must not be silently ignored"
        );
        Ok(())
    }

    #[test]
    fn base_release_manifest_rejects_non_current_embedding_model() {
        let m = sample_manifest(SUPPORTED_MANIFEST_VERSION as i64, env!("CARGO_PKG_VERSION"));
        let err = validate_base_release_manifest(&m).unwrap_err();
        assert!(
            err.to_string().contains(EMBEDDING_MODEL_ID),
            "expected current-model base release rejection, got: {err}"
        );
    }

    #[test]
    fn check_base_release_accepts_current_local_shape() -> Result<()> {
        let dir = tempdir()?;
        let base = dir.path();
        let packs = base.join("packs");
        fs::create_dir_all(&packs)?;
        let conn = open_write_at(&base.join("ato.db"))?;
        init_db(&conn)?;
        drop(conn);
        fs::write(packs.join("pack-test.bin.zst"), [])?;
        let mut manifest =
            sample_manifest(SUPPORTED_MANIFEST_VERSION as i64, env!("CARGO_PKG_VERSION"));
        manifest.model = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: EMBEDDING_MODEL_FINGERPRINT.to_string(),
            size: EMBEDDING_MODEL_HF_SIZE,
            url: EMBEDDING_MODEL_HF_URL.to_string(),
        };
        manifest.packs.push(PackInfo {
            sha8: "test".to_string(),
            sha256: String::new(),
            size: 0,
            url: "packs/pack-test.bin.zst".to_string(),
        });
        fs::write(
            base.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        check_base_release(base)?;
        Ok(())
    }

    #[test]
    fn check_build_checkpoint_requires_matching_source_hash() -> Result<()> {
        let dir = tempdir()?;
        let conn = open_write_at(&dir.path().join("ato.db"))?;
        init_db(&conn)?;
        drop(conn);
        let verified = HashSet::new();
        let base_hashes = HashMap::new();
        save_build_checkpoint(SaveBuildCheckpointArgs {
            out_dir: dir.path(),
            source_index_sha256: "source-a",
            zstd_level: 3,
            documents: &[],
            packs: &[],
            base_documents: &[],
            base_source_hash_by_doc_id: &base_hashes,
            verified_source_doc_ids: &verified,
        })?;
        check_build_checkpoint(dir.path(), "source-a", 3)?;
        let err = check_build_checkpoint(dir.path(), "source-b", 3).unwrap_err();
        assert!(
            err.to_string().contains("source index hash differs"),
            "expected source-hash mismatch, got: {err}"
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
    fn manifest_compat_rejects_higher_min_client_version() {
        let m = sample_manifest(SUPPORTED_MANIFEST_VERSION as i64, "999.0.0");
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
        let m = sample_manifest(SUPPORTED_MANIFEST_VERSION as i64, current);
        assert!(enforce_manifest_compatibility(&m).is_ok());
        let m = sample_manifest(SUPPORTED_MANIFEST_VERSION as i64, "0.0.1");
        assert!(enforce_manifest_compatibility(&m).is_ok());
    }

    #[test]
    fn manifest_compat_rejects_non_hf_model_without_metadata() {
        let mut m = sample_manifest(SUPPORTED_MANIFEST_VERSION as i64, "");
        m.model = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: String::new(),
            size: 0,
            url: "model-bundle.tar.zst".to_string(),
        };
        let err = enforce_manifest_compatibility(&m).unwrap_err();
        assert!(
            err.to_string().contains("sha256 and positive size"),
            "expected non-HF model metadata error, got: {err}"
        );
    }

    #[test]
    fn manifest_compat_rejects_hf_model_metadata_mismatch() {
        let mut m = sample_manifest(SUPPORTED_MANIFEST_VERSION as i64, "");
        m.model = ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: "wrong-sha".to_string(),
            size: EMBEDDING_MODEL_HF_SIZE,
            url: EMBEDDING_MODEL_HF_URL.to_string(),
        };
        let err = enforce_manifest_compatibility(&m).unwrap_err();
        assert!(
            err.to_string().contains("pinned Granite fingerprint"),
            "expected HF sha mismatch error, got: {err}"
        );
        m.model.sha256 = EMBEDDING_MODEL_FINGERPRINT.to_string();
        m.model.size = 1;
        let err = enforce_manifest_compatibility(&m).unwrap_err();
        assert!(
            err.to_string().contains("pinned Granite file set"),
            "expected HF size mismatch error, got: {err}"
        );
    }

    #[test]
    fn stage_model_rejects_wrong_non_hf_bundle_size() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let release = tempdir()?;
        let model_bundle = release.path().join("model-bundle.tar.zst");
        let bundle_bytes = write_test_model_bundle(&model_bundle)?;
        let manifest = Manifest {
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "wrong-model-size".to_string(),
            created_at: "2026-05-04T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: sha256_hex(&bundle_bytes),
                size: bundle_bytes.len() as u64 + 1,
                url: "model-bundle.tar.zst".to_string(),
            },
            documents: Vec::new(),
            packs: Vec::new(),
            db: None,
        };
        let context = UrlContext {
            manifest_dir: Some(release.path().to_path_buf()),
            manifest_base_url: None,
        };
        with_data_dir(data.path(), || -> Result<()> {
            let err = stage_model(&manifest, &context, &staging_dir()?).unwrap_err();
            assert!(
                err.to_string().contains("model bundle size mismatch"),
                "expected model size validation error, got: {err}"
            );
            Ok(())
        })?;
        Ok(())
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
            ..sample_manifest(SUPPORTED_MANIFEST_VERSION as i64, "")
        };
        let new = Manifest {
            documents: vec![DocRef {
                doc_id: "DOC".to_string(),
                content_hash: "same-content".to_string(),
                pack_sha8: "newpack".to_string(),
                offset: 0,
                length: 11,
            }],
            ..sample_manifest(SUPPORTED_MANIFEST_VERSION as i64, "")
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

    fn source_hash_test_record(title: &str) -> PackRecord {
        PackRecord {
            doc_id: "DOC_HASH".to_string(),
            doc_type: "Public_ruling".to_string(),
            title: title.to_string(),
            date: Some("2026-05-01".to_string()),
            downloaded_at: "2026-05-01T00:00:00Z".to_string(),
            content_hash: "same-cleaned-text-hash".to_string(),
            html: "<h1>Heading</h1><p>Same body</p>".to_string(),
            withdrawn_date: None,
            superseded_by: None,
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
                text: "Same body".to_string(),
                embedding_b64: Some("ignored-by-source-hash".to_string()),
            }],
        }
    }

    #[test]
    fn source_fingerprint_catches_non_body_metadata_changes() -> Result<()> {
        let first = source_hash_test_record("Original title");
        let changed = source_hash_test_record("Changed title");
        let first_hash = source_fingerprint_hash(&pack_record_source_fingerprint_value(&first))?;
        let changed_hash =
            source_fingerprint_hash(&pack_record_source_fingerprint_value(&changed))?;
        assert_ne!(
            first_hash, changed_hash,
            "base-release reuse must notice source-derived metadata changes even when body text hash is unchanged"
        );
        Ok(())
    }

    #[test]
    fn base_seed_checkpoint_preserves_verified_source_ids() -> Result<()> {
        let dir = tempdir()?;
        let base_documents = vec![DocRef {
            doc_id: "BASE_DOC".to_string(),
            content_hash: "body-hash".to_string(),
            pack_sha8: "basepack".to_string(),
            offset: 0,
            length: 10,
        }];
        let mut source_hashes = HashMap::new();
        source_hashes.insert("BASE_DOC".to_string(), "source-hash".to_string());
        let verified: HashSet<String> = ["BASE_DOC".to_string()].into_iter().collect();

        save_build_checkpoint(SaveBuildCheckpointArgs {
            out_dir: dir.path(),
            source_index_sha256: "source-index-sha",
            zstd_level: 3,
            documents: &base_documents,
            packs: &[],
            base_documents: &base_documents,
            base_source_hash_by_doc_id: &source_hashes,
            verified_source_doc_ids: &verified,
        })?;
        let loaded = load_build_checkpoint(dir.path(), "source-index-sha", 3)?
            .expect("checkpoint should load");
        assert_eq!(loaded.base_documents.len(), 1);
        assert_eq!(
            loaded.base_source_hash_by_doc_id.get("BASE_DOC"),
            Some(&"source-hash".to_string())
        );
        assert_eq!(loaded.verified_source_doc_ids, vec!["BASE_DOC".to_string()]);
        Ok(())
    }

    #[test]
    fn base_seed_allows_non_hf_mirror_model_checksum() -> Result<()> {
        let base = tempdir()?;
        let out = tempdir()?;
        let db_path = out.path().join("ato.db");
        let packs_dir = base.path().join("packs");
        fs::create_dir_all(&packs_dir)?;

        let record = json!({
            "doc_id": "BASE_MIRROR_DOC",
            "type": "Public_ruling",
            "title": "Mirror base",
            "date": "2026-05-01",
            "downloaded_at": "2026-05-01T00:00:00Z",
            "content_hash": "body-hash",
            "html": "<h1>Mirror base</h1>",
            "withdrawn_date": null,
            "superseded_by": null,
            "replaces": null,
            "has_in_doc_links": 0,
            "has_related_docs": 0,
            "has_history": 0,
            "anchors": [],
            "definitions": [],
            "chunks": [{"ord": 0, "anchor": null, "text": "Mirror base", "embedding_b64": null}],
            "assets": [],
        });
        let pack_bytes = encode_test_pack_record(&record)?;
        fs::write(packs_dir.join("pack-mirror.bin.zst"), &pack_bytes)?;
        let manifest = Manifest {
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "mirror-base".to_string(),
            created_at: "2026-05-01T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: "published-mirror-bundle-sha".to_string(),
                size: 123,
                url: "model-bundle.tar.zst".to_string(),
            },
            documents: vec![DocRef {
                doc_id: "BASE_MIRROR_DOC".to_string(),
                content_hash: "body-hash".to_string(),
                pack_sha8: "mirror".to_string(),
                offset: 0,
                length: pack_bytes.len() as u64,
            }],
            packs: vec![PackInfo {
                sha8: "mirror".to_string(),
                sha256: sha256_hex(&pack_bytes),
                size: pack_bytes.len() as u64,
                url: "packs/pack-mirror.bin.zst".to_string(),
            }],
            db: None,
        };
        fs::write(
            base.path().join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        let conn = open_write_at(&base.path().join("ato.db"))?;
        init_db(&conn)?;
        drop(conn);

        let seed = seed_build_from_base_release(base.path(), out.path(), &db_path)?;
        assert_eq!(seed.documents.len(), 1);
        assert!(seed.source_hash_by_doc_id.contains_key("BASE_MIRROR_DOC"));
        Ok(())
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
                msg.contains("corrupt or incomplete") && msg.contains("ato-mcp update"),
                "expected corrupt/incomplete error with init hint, got: {msg}"
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn vector_search_requires_model_marker_match() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        set_meta(&conn, "embedding_model_id", EMBEDDING_MODEL_ID)?;
        drop(conn);
        let model_sha = "expected-model-marker";
        let manifest = Manifest {
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "test-marker-readiness".to_string(),
            created_at: "2026-05-04T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: model_sha.to_string(),
                size: 5,
                url: "model-bundle.tar.zst".to_string(),
            },
            documents: Vec::new(),
            packs: Vec::new(),
            db: None,
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
            let err = ensure_vector_search_ready(&conn).unwrap_err();
            assert!(
                err.to_string().contains("installed semantic model files"),
                "expected model marker readiness error, got: {err}"
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

    fn write_test_model_bundle(path: &Path) -> Result<Vec<u8>> {
        let files: &[(&str, &[u8])] = &[
            ("model_fp16.onnx", b"dummy onnx bytes"),
            ("model_fp16.onnx_data", b"dummy external data"),
            ("tokenizer.json", br#"{"version":"1.0","truncation":null}"#),
        ];
        write_test_tar_zst(path, files)?;
        fs::read(path).map_err(Into::into)
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
            },
            documents: Vec::new(),
            packs: Vec::new(),
            db: None,
        }
    }

    // ----- serve startup: probe + server_instructions -----

    #[test]
    fn server_instructions_no_db_tells_user_to_run_update() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        with_data_dir(data.path(), || {
            let text = server_instructions(None);
            assert!(
                text.contains("not yet installed"),
                "missing not-installed prefix in: {text}"
            );
            assert!(
                text.contains("ato-mcp update"),
                "missing install command in: {text}"
            );
            assert!(text.contains("4 GB"), "missing size hint in: {text}");
        });
        Ok(())
    }

    #[test]
    fn server_instructions_appends_update_available_notice() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        with_data_dir(data.path(), || {
            let notice = UpdateAvailability {
                available_index_version: "2026.05.20".to_string(),
            };
            let text = server_instructions(Some(&notice));
            assert!(
                text.contains("newer ATO corpus index is available"),
                "missing update notice in: {text}"
            );
            assert!(
                text.contains("2026.05.20"),
                "missing available index_version in: {text}"
            );
            assert!(
                text.contains("ato-mcp update"),
                "missing update command in: {text}"
            );
        });
        Ok(())
    }

    #[test]
    fn mcp_startup_guidance_stays_compact() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        with_data_dir(data.path(), || {
            let static_chars = ATO_MCP_USE_INSTRUCTIONS.chars().count();
            let static_words = ATO_MCP_USE_INSTRUCTIONS.split_whitespace().count();
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
    fn check_for_update_availability_returns_none_when_offline() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let prev = std::env::var("ATO_MCP_OFFLINE").ok();
        std::env::set_var("ATO_MCP_OFFLINE", "1");
        let result = check_for_update_availability("https://example.invalid/manifest.json");
        if let Some(value) = prev {
            std::env::set_var("ATO_MCP_OFFLINE", value);
        } else {
            std::env::remove_var("ATO_MCP_OFFLINE");
        }
        assert!(
            result?.is_none(),
            "offline probe must short-circuit before any network attempt"
        );
        Ok(())
    }

    #[test]
    fn check_for_update_availability_returns_none_when_no_installed_manifest() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let prev = std::env::var("ATO_MCP_OFFLINE").ok();
        std::env::remove_var("ATO_MCP_OFFLINE");
        let result = with_data_dir(data.path(), || {
            check_for_update_availability("https://example.invalid/manifest.json")
        });
        if let Some(value) = prev {
            std::env::set_var("ATO_MCP_OFFLINE", value);
        }
        assert!(
            result?.is_none(),
            "probe must return None when no installed manifest is present"
        );
        Ok(())
    }

    #[test]
    fn check_for_update_availability_suppresses_incompatible_schema() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let release = tempdir()?;
        let release_dir = release.path();
        let manifest_path = release_dir.join("manifest.json");

        let installed = Manifest {
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "test-installed".to_string(),
            created_at: "2026-05-04T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: "installed-sha".to_string(),
                size: 5,
                url: "model-bundle.tar.zst".to_string(),
            },
            documents: Vec::new(),
            packs: Vec::new(),
            db: None,
        };
        let summary = UpdateSummary {
            schema_version: (SUPPORTED_MANIFEST_VERSION + 1) as i64,
            index_version: "test-future".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: installed.model.clone(),
            document_count: 0,
            pack_count: 0,
            db_sha256: None,
            db_size: None,
            manifest_fingerprint: Some("future-fingerprint".to_string()),
        };
        fs::write(
            release_dir.join("update.json"),
            serde_json::to_vec_pretty(&summary)?,
        )?;

        let prev = std::env::var("ATO_MCP_OFFLINE").ok();
        std::env::remove_var("ATO_MCP_OFFLINE");
        let result = with_data_dir(data.path(), || -> Result<Option<UpdateAvailability>> {
            fs::write(
                installed_manifest_path()?,
                serde_json::to_vec_pretty(&installed)?,
            )?;
            check_for_update_availability(manifest_path.to_str().expect("utf-8 path"))
        });
        if let Some(value) = prev {
            std::env::set_var("ATO_MCP_OFFLINE", value);
        }
        assert!(
            result?.is_none(),
            "probe must suppress the notice when the published index requires a newer binary"
        );
        Ok(())
    }

    #[test]
    fn check_for_update_availability_returns_none_when_already_current() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let release = tempdir()?;
        let release_dir = release.path();
        let manifest_path = release_dir.join("manifest.json");
        let model_sha = "current-probe-sha";
        let manifest = Manifest {
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "test-probe-current".to_string(),
            created_at: "2026-05-04T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: model_sha.to_string(),
                size: 5,
                url: "model-bundle.tar.zst".to_string(),
            },
            documents: Vec::new(),
            packs: Vec::new(),
            db: None,
        };
        let summary = UpdateSummary {
            schema_version: manifest.schema_version,
            index_version: manifest.index_version.clone(),
            min_client_version: manifest.min_client_version.clone(),
            model: manifest.model.clone(),
            document_count: 0,
            pack_count: 0,
            db_sha256: None,
            db_size: None,
            manifest_fingerprint: Some(manifest_fingerprint(&manifest)?),
        };
        fs::write(
            release_dir.join("update.json"),
            serde_json::to_vec_pretty(&summary)?,
        )?;

        let prev = std::env::var("ATO_MCP_OFFLINE").ok();
        std::env::remove_var("ATO_MCP_OFFLINE");
        let result = with_data_dir(data.path(), || -> Result<Option<UpdateAvailability>> {
            let conn = open_write_at(&db_path()?)?;
            init_db(&conn)?;
            drop(conn);
            fs::write(
                installed_manifest_path()?,
                serde_json::to_vec_pretty(&manifest)?,
            )?;
            fs::write(model_path()?, b"model")?;
            fs::write(model_data_path()?, b"model-data")?;
            fs::write(live_dir()?.join("tokenizer.json"), br#"{"version":"1.0"}"#)?;
            fs::write(live_dir()?.join(".model.sha256"), model_sha)?;
            check_for_update_availability(manifest_path.to_str().expect("utf-8 path"))
        });
        if let Some(value) = prev {
            std::env::set_var("ATO_MCP_OFFLINE", value);
        }
        assert!(
            result?.is_none(),
            "probe must return None when installed corpus already matches the published summary"
        );
        Ok(())
    }

    // ===== Wave 2 ===========================================================

    // ----- Schema v8 -----

    #[test]
    fn schema_init_writes_v9_metadata() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (_dir, db) = make_test_db()?;
        let conn = open_write_at(&db)?;
        let value =
            get_meta(&conn, "schema_version")?.expect("init_db should have written schema_version");
        assert_eq!(value, SUPPORTED_SCHEMA_VERSION.to_string());
        assert_eq!(SUPPORTED_SCHEMA_VERSION, 9);
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
    fn build_doc_filter_uses_current_prefix_policy() {
        let f = build_doc_filter("d", None, None, None, None, false, true);
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
            seed_text: None,
        };
        let call = search_next_call("depreciation", 16, &opts);
        assert!(
            call.contains("current_only=false"),
            "continuation must preserve withdrawn-doc inclusion; got: {call}"
        );
    }

    #[test]
    fn search_next_call_preserves_seed_text() {
        let opts = SearchOptions {
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
            similar_to_chunk_id: None,
            seed_text: Some("an external passage about depreciation"),
        };
        let call = search_next_call("ignored", 16, &opts);
        assert!(
            call.contains(r#"seed_text="an external passage about depreciation""#),
            "continuation must preserve seed_text; got: {call}"
        );
    }

    #[test]
    fn search_next_call_prefers_similar_to_chunk_id_over_seed_text() {
        // similar_to_chunk_id wins if both are set — the continuation must
        // not also carry seed_text.
        let opts = SearchOptions {
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
            similar_to_chunk_id: Some(42),
            seed_text: Some("should be ignored"),
        };
        let call = search_next_call("ignored", 16, &opts);
        assert!(
            call.contains("similar_to_chunk_id=42"),
            "continuation must preserve similar_to_chunk_id; got: {call}"
        );
        assert!(
            !call.contains("seed_text"),
            "similar_to_chunk_id wins — seed_text must not appear; got: {call}"
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

    // ----- W2.4 integration: title hits filter out withdrawn docs by default -----

    #[test]
    fn collect_title_hits_excludes_withdrawn_by_default() -> Result<()> {
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
        // Update documents.title to match what title_fts holds (collect_title_hits
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
            let conn = open_read()?;
            // Default: current_only=true → withdrawn doc filtered out.
            let filter = build_doc_filter("d", None, None, None, None, true, true);
            let hits = collect_title_hits(&conn, "depreciation", 10, &filter)?;
            let doc_ids: Vec<&str> = hits.iter().map(|h| h.doc_id.as_str()).collect();
            assert!(
                doc_ids.contains(&"DOC_CURRENT"),
                "current doc should appear; got: {doc_ids:?}"
            );
            assert!(
                !doc_ids.contains(&"DOC_WITHDRAWN"),
                "withdrawn doc should be filtered out by default; got: {doc_ids:?}"
            );

            // current_only=false → withdrawn doc returned with marker visible.
            let filter = build_doc_filter("d", None, None, None, None, true, false);
            let hits = collect_title_hits(&conn, "depreciation", 10, &filter)?;
            let withdrawn_hit = hits
                .iter()
                .find(|h| h.doc_id == "DOC_WITHDRAWN")
                .expect("withdrawn doc should appear when current_only=false");
            assert_eq!(withdrawn_hit.withdrawn_date.as_deref(), Some("2023-06-15"));
            assert_eq!(withdrawn_hit.superseded_by.as_deref(), Some("TR 2024/1"));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn collect_title_hits_prefers_direct_doc_id_hits() -> Result<()> {
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
            "UPDATE documents SET type = 'PAC', title = ? WHERE doc_id = ?",
            params![
                "Income Tax Assessment Act 1997 s 203-50",
                "PAC/19970038/203-50"
            ],
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
            "UPDATE documents SET type = 'PAC', title = ? WHERE doc_id = ?",
            params!["Income Tax Assessment Act 1997 s 8-1", "PAC/19970038/8-1"],
        )?;
        conn.execute(
            "INSERT INTO title_fts(doc_id, title, headings) VALUES (?, ?, '')",
            params!["PAC/19970038/8-1", "Income Tax Assessment Act 1997 s 8-1"],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let conn = open_read()?;
            let filter = build_doc_filter("d", None, None, None, None, false, true);
            let hits = collect_title_hits(&conn, "PAC/19970038/203-50", 5, &filter)?;
            assert_eq!(hits[0].doc_id, "PAC/19970038/203-50");
            let hits =
                collect_title_hits(&conn, "Income Tax Assessment Act 1997 s 8-1", 5, &filter)?;
            assert_eq!(hits[0].doc_id, "PAC/19970038/8-1");
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
            assert_eq!(definitions[0]["source"]["type"], json!("PAC"));
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
                    seed_text: None,
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
                    seed_text: None,
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
                    seed_text: None,
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
                json!(
                    "https://www.ato.gov.au/law/view/document?docid=DOC_HISTORY&PiT=20200101000000"
                )
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn get_doc_anchors_resolves_in_doc_chunks_from_stored_html() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let (dir, _db) = make_test_db()?;
        let conn = open_write_at(&dir.path().join("live/ato.db"))?;
        insert_doc_full(
            &conn,
            "DOC_HTML_ANCHORS",
            Some("2024-01-01"),
            None,
            None,
            None,
        )?;
        conn.execute(
            "UPDATE documents SET html = ? WHERE doc_id = ?",
            params![
                compress_text(
                    r##"<nav><a href="#target">Target section</a></nav><h2 id="target">Target</h2>"##
                )?,
                "DOC_HTML_ANCHORS"
            ],
        )?;
        conn.execute(
            "INSERT INTO chunks(chunk_id, doc_id, ord, anchor, text) VALUES (?, ?, ?, ?, ?)",
            params![
                9001i64,
                "DOC_HTML_ANCHORS",
                0i64,
                "target",
                compress_text("Target body")?,
            ],
        )?;
        conn.execute(
            "INSERT INTO doc_anchors(doc_id, ord, kind, label, target_chunk_id, target_doc_id, target_pit) VALUES (?, 0, 'in_doc', 'Target section', NULL, NULL, NULL)",
            params!["DOC_HTML_ANCHORS"],
        )?;
        drop(conn);

        with_data_dir(dir.path(), || -> Result<()> {
            let json_str = get_doc_anchors("DOC_HTML_ANCHORS")?;
            let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
            assert_eq!(parsed["in_doc"].as_array().unwrap().len(), 1);
            assert_eq!(parsed["in_doc"][0]["label"], json!("Target section"));
            assert_eq!(parsed["in_doc"][0]["chunk_id"], json!(9001));
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
                json!(
                    "https://www.ato.gov.au/law/view/document?docid=TR_1996_X&PiT=19960320000001"
                )
            );
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
    fn apply_update_locked_ingests_real_manifest_and_pack() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let release = tempdir()?;
        let release_dir = release.path();
        let packs_dir = release_dir.join("packs");
        fs::create_dir_all(&packs_dir)?;

        let model_bundle = release_dir.join("model-bundle.tar.zst");
        let model_bundle_bytes = write_test_model_bundle(&model_bundle)?;

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
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "test-real-update".to_string(),
            created_at: "2026-05-01T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: sha256_hex(&model_bundle_bytes),
                size: model_bundle_bytes.len() as u64,
                url: "model-bundle.tar.zst".to_string(),
            },
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
            db: None,
        };
        let manifest_path = release_dir.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

        with_data_dir(data.path(), || -> Result<()> {
            let stats = apply_update_locked(manifest_path.to_str().expect("utf-8 path"))?;
            assert_eq!(stats.added, 1);
            assert_eq!(stats.changed, 0);
            assert_eq!(stats.removed, 0);
            assert!(model_path()?.exists(), "model should be installed");
            assert!(
                model_data_path()?.exists(),
                "model data should be installed"
            );
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

    /// Regression test for the empty-citations-table-after-full-rebuild bug.
    /// `apply_update_locked` always routes through `rebuild_live_db_from_manifest`,
    /// and that path must call `derive_citations` so the live DB ships a
    /// populated `citations` table on every install.
    #[test]
    fn apply_update_locked_derives_citations_after_full_rebuild() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let release = tempdir()?;
        let release_dir = release.path();
        let packs_dir = release_dir.join("packs");
        fs::create_dir_all(&packs_dir)?;

        let model_bundle = release_dir.join("model-bundle.tar.zst");
        let model_bundle_bytes = write_test_model_bundle(&model_bundle)?;
        let embedding_b64 =
            base64::engine::general_purpose::STANDARD.encode(vec![0u8; EMBEDDING_DIM]);

        let record = json!({
            "doc_id": "DOC_CITATION_SOURCE",
            "type": "Public_ruling",
            "title": "Citation source",
            "date": "2026-05-01",
            "downloaded_at": "2026-05-01T00:00:00Z",
            "content_hash": "citation-source-hash",
            "html": "<div><p>See <a data-doc-id=\"DOC_CITATION_TARGET\">target</a>.</p></div>",
            "assets": [],
            "chunks": [{
                "ord": 0,
                "anchor": "ruling",
                "text": "Refer to [doc:DOC_CITATION_TARGET] for details.",
                "embedding_b64": embedding_b64
            }]
        });
        let pack_bytes = encode_test_pack_record(&record)?;
        fs::write(packs_dir.join("pack-citation.bin.zst"), &pack_bytes)?;

        let manifest = Manifest {
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "test-citation".to_string(),
            created_at: "2026-05-01T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: sha256_hex(&model_bundle_bytes),
                size: model_bundle_bytes.len() as u64,
                url: "model-bundle.tar.zst".to_string(),
            },
            documents: vec![DocRef {
                doc_id: "DOC_CITATION_SOURCE".to_string(),
                content_hash: "citation-source-hash".to_string(),
                pack_sha8: "citation".to_string(),
                offset: 0,
                length: pack_bytes.len() as u64,
            }],
            packs: vec![PackInfo {
                sha8: "citation".to_string(),
                sha256: sha256_hex(&pack_bytes),
                size: pack_bytes.len() as u64,
                url: "packs/pack-citation.bin.zst".to_string(),
            }],
            db: None,
        };
        let manifest_path = release_dir.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

        with_data_dir(data.path(), || -> Result<()> {
            apply_update_locked(manifest_path.to_str().expect("utf-8 path"))?;
            let conn = open_read()?;
            let citations: i64 =
                conn.query_row("SELECT COUNT(*) FROM citations", [], |row| row.get(0))?;
            assert_eq!(
                citations, 1,
                "rebuild_live_db_from_manifest must call derive_citations so cited_by works"
            );
            let target: String = conn.query_row(
                "SELECT target_doc_id FROM citations WHERE source_doc_id = ?",
                ["DOC_CITATION_SOURCE"],
                |row| row.get(0),
            )?;
            assert_eq!(target, "DOC_CITATION_TARGET");
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
        let model_bundle_bytes = write_test_model_bundle(&model_bundle)?;
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
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "defs-v1".to_string(),
            created_at: "2026-05-01T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: sha256_hex(&model_bundle_bytes),
                size: model_bundle_bytes.len() as u64,
                url: "model-bundle.tar.zst".to_string(),
            },
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
            db: None,
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
                document_count: new_manifest.documents.len(),
                pack_count: new_manifest.packs.len(),
            db_sha256: None,
            db_size: None,
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
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "test-summary-fast-path".to_string(),
            created_at: "2026-05-04T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: model_sha.to_string(),
                size: 5,
                url: "model-bundle.tar.zst".to_string(),
            },
            documents: Vec::new(),
            packs: Vec::new(),
            db: None,
        };
        let summary = UpdateSummary {
            schema_version: manifest.schema_version,
            index_version: manifest.index_version.clone(),
            min_client_version: manifest.min_client_version.clone(),
            model: manifest.model.clone(),
            document_count: 0,
            pack_count: 0,
            db_sha256: None,
            db_size: None,
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
            fs::write(model_path()?, b"model")?;
            fs::write(model_data_path()?, b"model-data")?;
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
    fn apply_update_locked_does_not_skip_when_model_data_missing() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let release = tempdir()?;
        let release_dir = release.path();
        let manifest_path = release_dir.join("manifest.json");
        let model_bundle = release_dir.join("model-bundle.tar.zst");
        let model_bundle_bytes = write_test_model_bundle(&model_bundle)?;
        let manifest = Manifest {
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "test-missing-model-data".to_string(),
            created_at: "2026-05-04T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: sha256_hex(&model_bundle_bytes),
                size: model_bundle_bytes.len() as u64,
                url: "model-bundle.tar.zst".to_string(),
            },
            documents: Vec::new(),
            packs: Vec::new(),
            db: None,
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        fs::write(&manifest_path, &manifest_bytes)?;
        let summary = UpdateSummary {
            schema_version: manifest.schema_version,
            index_version: manifest.index_version.clone(),
            min_client_version: manifest.min_client_version.clone(),
            model: manifest.model.clone(),
            document_count: 0,
            pack_count: 0,
            db_sha256: None,
            db_size: None,
            manifest_fingerprint: Some(manifest_fingerprint(&manifest)?),
        };
        let summary_bytes = serde_json::to_vec_pretty(&summary)?;
        fs::write(release_dir.join("update.json"), &summary_bytes)?;

        with_data_dir(data.path(), || -> Result<()> {
            let conn = open_write_at(&db_path()?)?;
            init_db(&conn)?;
            drop(conn);
            fs::write(
                installed_manifest_path()?,
                serde_json::to_vec_pretty(&manifest)?,
            )?;
            fs::write(model_path()?, b"model")?;
            fs::write(live_dir()?.join("tokenizer.json"), br#"{"version":"1.0"}"#)?;
            fs::write(live_dir()?.join(".model.sha256"), &manifest.model.sha256)?;

            let stats = apply_update_locked(manifest_path.to_str().expect("utf-8 path"))?;
            assert_eq!(
                stats.bytes_downloaded,
                manifest_bytes.len() as u64,
                "missing model_fp16.onnx_data must force manifest fetch instead of update.json fast-path"
            );
            assert!(
                model_data_path()?.exists(),
                "full update should install the missing external model data file"
            );
            Ok(())
        })?;
        Ok(())
    }

    fn write_installed_model_marker(data_dir: &Path, marker_value: &str) -> Result<()> {
        with_data_dir(data_dir, || -> Result<()> {
            fs::write(model_path()?, b"old-model")?;
            fs::write(model_data_path()?, b"old-model-data")?;
            fs::write(tokenizer_path()?, br#"{"version":"1.0"}"#)?;
            fs::write(model_marker_path()?, marker_value)?;
            Ok(())
        })
    }

    fn old_installed_manifest(marker_value: &str) -> Manifest {
        Manifest {
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "old-install".to_string(),
            created_at: "2026-05-01T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: marker_value.to_string(),
                size: 1,
                url: "old-model-bundle.tar.zst".to_string(),
            },
            documents: Vec::new(),
            packs: Vec::new(),
            db: None,
        }
    }

    #[test]
    fn failed_model_fetch_keeps_existing_model_marker() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let release = tempdir()?;
        let release_dir = release.path();
        let old_marker = "old-good-marker";
        let new_manifest = Manifest {
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "failed-model-fetch".to_string(),
            created_at: "2026-05-04T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: "new-model-sha".to_string(),
                size: 123,
                url: "missing-model-bundle.tar.zst".to_string(),
            },
            documents: Vec::new(),
            packs: Vec::new(),
            db: None,
        };
        let manifest_path = release_dir.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&new_manifest)?)?;
        write_installed_model_marker(data.path(), old_marker)?;

        with_data_dir(data.path(), || -> Result<()> {
            fs::write(
                installed_manifest_path()?,
                serde_json::to_vec_pretty(&old_installed_manifest(old_marker))?,
            )?;
            let err = apply_update_locked(manifest_path.to_str().expect("utf-8 path")).unwrap_err();
            assert!(
                err.to_string().contains("missing-model-bundle"),
                "expected missing model bundle error, got: {err}"
            );
            assert_eq!(fs::read_to_string(model_marker_path()?)?, old_marker);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn failed_db_rebuild_after_model_staging_keeps_existing_model_marker() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let release = tempdir()?;
        let release_dir = release.path();
        let model_bundle = release_dir.join("model-bundle.tar.zst");
        let model_bundle_bytes = write_test_model_bundle(&model_bundle)?;
        let old_marker = "old-good-marker";
        let manifest = Manifest {
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "failed-db-rebuild".to_string(),
            created_at: "2026-05-04T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: sha256_hex(&model_bundle_bytes),
                size: model_bundle_bytes.len() as u64,
                url: "model-bundle.tar.zst".to_string(),
            },
            documents: vec![DocRef {
                doc_id: "DOC_MISSING_PACK".to_string(),
                content_hash: "missing-pack-doc".to_string(),
                pack_sha8: "missingpack".to_string(),
                offset: 0,
                length: 10,
            }],
            packs: vec![PackInfo {
                sha8: "missingpack".to_string(),
                sha256: String::new(),
                size: 10,
                url: "packs/pack-missingpack.bin.zst".to_string(),
            }],
            db: None,
        };
        let manifest_path = release_dir.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
        write_installed_model_marker(data.path(), old_marker)?;

        with_data_dir(data.path(), || -> Result<()> {
            fs::write(
                installed_manifest_path()?,
                serde_json::to_vec_pretty(&old_installed_manifest(old_marker))?,
            )?;
            let err = apply_update_locked(manifest_path.to_str().expect("utf-8 path")).unwrap_err();
            assert!(
                err.to_string().contains("pack-missingpack"),
                "expected missing pack error, got: {err}"
            );
            assert_eq!(fs::read_to_string(model_marker_path()?)?, old_marker);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn promotion_rolls_back_db_when_assets_promotion_fails() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        with_data_dir(data.path(), || -> Result<()> {
            let conn = open_write_at(&db_path()?)?;
            init_db(&conn)?;
            set_meta(&conn, "index_version", "old-db")?;
            drop(conn);
            let live_assets = live_dir()?.join("assets");
            fs::create_dir_all(&live_assets)?;
            fs::write(live_assets.join("old.txt"), b"old asset")?;
            let rollback_backup = backups_dir()?.join("ato.db.prev");
            fs::write(&rollback_backup, b"previous rollback target")?;

            let staging_root = staging_dir()?.join("promotion-rollback-test");
            remove_path_if_exists(&staging_root)?;
            fs::create_dir_all(&staging_root)?;
            let staged_db = staging_root.join("ato.db");
            let staged_conn = open_write_at(&staged_db)?;
            init_db(&staged_conn)?;
            set_meta(&staged_conn, "index_version", "new-db")?;
            drop(staged_conn);
            let staged_asset_root = staging_root.join("live");
            fs::create_dir_all(&staged_asset_root)?;
            fs::write(staged_asset_root.join("assets"), b"not a directory")?;

            let manifest = old_installed_manifest("old-marker");
            let err = promote_staged_update(
                None,
                StagedCorpusUpdate {
                    staging_root,
                    staged_db,
                    staged_asset_root,
                    stats: UpdateStats::default(),
                },
                &manifest,
            )
            .unwrap_err();
            assert!(
                err.to_string()
                    .contains("staged assets path is not a directory"),
                "expected assets promotion error, got: {err}"
            );

            let conn = open_read()?;
            assert_eq!(get_meta(&conn, "index_version")?.as_deref(), Some("old-db"));
            assert!(live_assets.join("old.txt").exists());
            assert!(
                !installed_manifest_path()?.exists(),
                "manifest must not be written after failed promotion"
            );
            assert_eq!(
                fs::read(&rollback_backup)?,
                b"previous rollback target",
                "failed promotion must not replace the previous doctor rollback backup"
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn promotion_rolls_back_model_when_persistent_backup_fails() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        with_data_dir(data.path(), || -> Result<()> {
            let conn = open_write_at(&db_path()?)?;
            init_db(&conn)?;
            set_meta(&conn, "index_version", "old-db")?;
            drop(conn);
            fs::write(model_path()?, b"old-model")?;
            fs::write(model_data_path()?, b"old-model-data")?;
            fs::write(tokenizer_path()?, b"old-tokenizer")?;
            fs::write(model_marker_path()?, "old-marker")?;

            let staging_root = staging_dir()?.join("promotion-model-rollback-test");
            remove_path_if_exists(&staging_root)?;
            fs::create_dir_all(&staging_root)?;
            let staged_db = staging_root.join("ato.db");
            let staged_conn = open_write_at(&staged_db)?;
            init_db(&staged_conn)?;
            set_meta(&staged_conn, "index_version", "new-db")?;
            drop(staged_conn);
            let staged_asset_root = staging_root.join("live");
            fs::create_dir_all(staged_asset_root.join("assets"))?;
            let staged_model_dir = staging_root.join("model-stage");
            fs::create_dir_all(&staged_model_dir)?;
            fs::write(staged_model_dir.join("model_fp16.onnx"), b"new-model")?;
            fs::write(
                staged_model_dir.join("model_fp16.onnx_data"),
                b"new-model-data",
            )?;
            fs::write(staged_model_dir.join("tokenizer.json"), b"new-tokenizer")?;
            let staged_model = StagedModel {
                dir: staged_model_dir,
                marker_value: "new-marker".to_string(),
            };
            fs::write(data.path().join("backups"), b"not a directory")?;

            let manifest = old_installed_manifest("new-marker");
            let err = promote_staged_update(
                Some(&staged_model),
                StagedCorpusUpdate {
                    staging_root,
                    staged_db,
                    staged_asset_root,
                    stats: UpdateStats::default(),
                },
                &manifest,
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("File exists"),
                "expected persistent backup failure, got: {err}"
            );
            assert_eq!(fs::read(model_path()?)?, b"old-model");
            assert_eq!(fs::read(model_data_path()?)?, b"old-model-data");
            assert_eq!(fs::read(tokenizer_path()?)?, b"old-tokenizer");
            assert_eq!(fs::read_to_string(model_marker_path()?)?, "old-marker");
            let conn = open_read()?;
            assert_eq!(get_meta(&conn, "index_version")?.as_deref(), Some("old-db"));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn successful_update_keeps_persistent_doctor_rollback_backup() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        with_data_dir(data.path(), || -> Result<()> {
            let conn = open_write_at(&db_path()?)?;
            init_db(&conn)?;
            set_meta(&conn, "index_version", "old-db")?;
            drop(conn);
            fs::write(backups_dir()?.join("ato.db.prev"), b"previous backup")?;

            let staging_root = staging_dir()?.join("promotion-success-test");
            remove_path_if_exists(&staging_root)?;
            fs::create_dir_all(&staging_root)?;
            let staged_db = staging_root.join("ato.db");
            let staged_conn = open_write_at(&staged_db)?;
            init_db(&staged_conn)?;
            set_meta(&staged_conn, "index_version", "new-db")?;
            drop(staged_conn);
            let staged_asset_root = staging_root.join("live");
            fs::create_dir_all(staged_asset_root.join("assets"))?;

            let manifest = old_installed_manifest("old-marker");
            promote_staged_update(
                None,
                StagedCorpusUpdate {
                    staging_root,
                    staged_db,
                    staged_asset_root,
                    stats: UpdateStats::default(),
                },
                &manifest,
            )?;
            assert!(
                backups_dir()?.join("ato.db.prev").exists(),
                "successful promotion must keep doctor rollback backup"
            );
            {
                let conn = open_read()?;
                assert_eq!(get_meta(&conn, "index_version")?.as_deref(), Some("new-db"));
            }
            doctor(true)?;
            let conn = open_read()?;
            assert_eq!(get_meta(&conn, "index_version")?.as_deref(), Some("old-db"));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn ensure_model_rejects_incomplete_bundle_before_marker() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
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
        let manifest = Manifest {
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "test-incomplete-model-bundle".to_string(),
            created_at: "2026-05-04T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: sha256_hex(&model_bundle_bytes),
                size: model_bundle_bytes.len() as u64,
                url: "model-bundle.tar.zst".to_string(),
            },
            documents: Vec::new(),
            packs: Vec::new(),
            db: None,
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
    fn apply_update_locked_rebuilds_unsupported_schema_db() -> Result<()> {
        let _lock = TEST_DB_LOCK.lock().unwrap();
        let data = tempdir()?;
        let release = tempdir()?;
        let release_dir = release.path();
        let packs_dir = release_dir.join("packs");
        fs::create_dir_all(&packs_dir)?;

        let model_bundle = release_dir.join("model-bundle.tar.zst");
        let model_bundle_bytes = write_test_model_bundle(&model_bundle)?;

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
            schema_version: SUPPORTED_MANIFEST_VERSION as i64,
            index_version: "test-rebuild-schema".to_string(),
            created_at: "2026-05-03T00:00:00Z".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: ModelInfo {
                id: EMBEDDING_MODEL_ID.to_string(),
                sha256: sha256_hex(&model_bundle_bytes),
                size: model_bundle_bytes.len() as u64,
                url: "model-bundle.tar.zst".to_string(),
            },
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
            db: None,
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
                    SUPPORTED_MANIFEST_VERSION as i64,
                    env!("CARGO_PKG_VERSION"),
                ))?,
            )?;

            let stats = apply_update_locked(manifest_path.to_str().expect("utf-8 path"))?;
            assert_eq!(stats.added, 1);
            assert_eq!(stats.changed, 0);
            assert_eq!(stats.removed, 0);

            let conn = open_read()?;
            assert_eq!(get_meta(&conn, "schema_version")?.as_deref(), Some("9"));
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

    // ===== Slim Search Surface ============================================

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
                    doc_id: format!("DOC_H{i}"),
                    is_intro: false,
                },
            );
        }
        for j in 0..5 {
            meta.insert(
                11 + j as i64,
                CandidateMeta {
                    doc_id: "DOC_TAIL_ONLY".to_string(),
                    is_intro: false,
                },
            );
        }

        let deduped = dedup_per_doc(hits, &meta, 11, 1);
        let tail_position = deduped
            .iter()
            .position(|hit| meta[&hit.chunk_id].doc_id == "DOC_TAIL_ONLY")
            .expect("tail-only doc should appear in frontier");
        assert_eq!(tail_position, 10);

        let mut counts: HashMap<&str, usize> = HashMap::new();
        for hit in &deduped {
            *counts
                .entry(meta[&hit.chunk_id].doc_id.as_str())
                .or_insert(0) += 1;
        }
        for (doc, n) in &counts {
            assert_eq!(*n, 1, "max_per_doc=1 violated for {doc}: {n} chunks");
        }
    }

    #[test]
    fn manifest_compat_rejects_newer_manifest_format() {
        let m = sample_manifest((SUPPORTED_MANIFEST_VERSION + 1) as i64, "");
        let err =
            enforce_manifest_compatibility(&m).expect_err("newer manifest should be rejected");
        assert!(
            err.to_string().contains("not supported"),
            "expected unsupported-schema error, got: {err}"
        );
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
        assert!(
            msg.contains("no stored embedding"),
            "unexpected error: {msg}"
        );
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

    // Tests that touch the global data dir env var cannot run in
    // parallel — serialise them through a single mutex.
    static TEST_DB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
