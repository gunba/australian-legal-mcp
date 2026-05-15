use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use fs2::FileExt;
#[cfg(feature = "cuda")]
use ort::ep;
use ort::session::{
    builder::{GraphOptimizationLevel, SessionBuilder},
    Session,
};
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
use std::io::{BufRead, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};
use url::Url;
use zip::ZipArchive;

const APP_NAME: &str = "ato-mcp";
const DEFAULT_RELEASES_URL: &str = "https://github.com/gunba/ato-mcp/releases/latest/download";
const DEFAULT_K: usize = 8;
const MAX_K: usize = 50;
/// Cap on the `title_hits` sidebar `search` returns alongside chunk hits.
/// Direct doc_id / ATO-link matches always lead; the BM25 title remainder
/// fills the rest.
const TITLE_HITS_K: usize = 10;
const SNIPPET_CHARS: usize = 280;
// [EM-05] Stored semantic vectors are the first 256 dimensions of the
// model output after normalization + int8 quantization.
const EMBEDDING_DIM: usize = 256;
// [EM-03] The tokenizer truncates semantic inputs and pads dynamically to
// each batch's max sequence length.
const EMBEDDING_INPUT_MAX_TOKENS: usize = 1024;
// [EM-02] Granite ONNX inputs use source-derived text directly; no
// query/passage prompt prefix is stored in chunks or added at runtime.
const EMBEDDING_TEXT_PREFIX: &str = "";
const EMBEDDING_MODEL_FINGERPRINT: &str =
    "granite-small-r2-fp16:ee200de55cb2f94e858aabca54be7697a9c0805a14c858ee26ad0922b05f57d7:28d16e29cd623f25cc6fa0968700c5bc31036466091a5fa06d1353c1777f050e:feeb83348dcb033bc6b9d2e1f7906ca9eb2d122845000c9416d894d7c2927149";
const OLD_CONTENT_CUTOFF: &str = "2000-01-01";
const DEFAULT_EXCLUDED_TYPES: &[&str] = &["Edited_private_advice"];
const LEGISLATION_TYPE: &str = "Legislation_and_supporting_material";
const OEWN_2024_URL: &str = "https://en-word.net/static/english-wordnet-2024.zip";
const OEWN_2024_SOURCE: &str = "Open English WordNet 2024 (CC-BY 4.0)";
const ORDINARY_DICTIONARY_PATH_ENV: &str = "ATO_MCP_DICTIONARY_PATH";
/// On-disk schema version this binary supports. Bump when introducing
/// schema changes; binaries reject any corpus whose schema does not match.
const SUPPORTED_SCHEMA_VERSION: u32 = 8;
/// Single release manifest format (`Manifest.schema_version`) this binary
/// ingests. No legacy manifest layouts are accepted.
const SUPPORTED_MANIFEST_VERSION: u32 = 4;
const EMBEDDING_MODEL_ID: &str = "granite-embedding-small-r2-fp16-256d";
const BUILD_EMBED_BATCH_SIZE: usize = 32;
const BUILD_EMBED_PENDING_FLUSH_CHUNKS: usize = 4096;
// [IB-10] Build pack shards are bounded by document count, not target
// bytes, so downloads stay tractable while pack offsets remain stable.
const BUILD_PACK_RECORDS_PER_SHARD: usize = 4096;
const BUILD_CHECKPOINT_SCHEMA_VERSION: u32 = 2;
const DEFAULT_MAX_PER_DOC: usize = 2;
const HARD_MAX_PER_DOC: usize = 3;
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
    /// Run the Rust HTML cleaner on a saved doc and emit JSON. Used by the
    /// Python build pipeline as a subprocess (replaces the in-process call
    /// to ato_mcp.indexer.extract.extract). Reads HTML from --html-file
    /// (or stdin if omitted) and writes
    /// {text, html, title, html_title} to stdout.
    Extract {
        /// Path to the source HTML. Reads stdin if absent.
        #[arg(long)]
        html_file: Option<PathBuf>,
        /// doc_id is reserved for future asset-extraction support; ignored today.
        #[arg(long)]
        doc_id: Option<String>,
        /// source_path is reserved for future asset-resolution support; ignored today.
        #[arg(long)]
        source_path: Option<PathBuf>,
    },
    /// Extract definitions from a list of pre-chunked DefinitionChunks.
    /// Reads JSON from --input-file (or stdin) of the shape
    /// {doc_id, source_title, source_type, chunks: [{ord, anchor, text}, ...]}.
    /// Writes a JSON array of Definition records to stdout.
    /// Mirrors src/ato_mcp/indexer/definitions.py:extract_definitions.
    ExtractDefinitions {
        #[arg(long)]
        input_file: Option<PathBuf>,
    },
    /// Extract doc-navigation anchors from cleaned HTML. Mirrors
    /// src/ato_mcp/indexer/anchors.py:extract_anchors. JSON out:
    /// [{kind, label, target_anchor?, target_doc_id?, target_pit?}, ...].
    ExtractAnchors {
        /// Path to the cleaned HTML. Reads stdin if absent.
        #[arg(long)]
        html_file: Option<PathBuf>,
        /// doc_id of the source doc — needed to filter self-links.
        #[arg(long)]
        source_doc_id: String,
    },
    /// Extract currency / withdrawal markers from raw doc HTML. Mirrors
    /// src/ato_mcp/indexer/extract.py:extract_currency. JSON out:
    /// {withdrawn_date, superseded_by, replaces}.
    ExtractCurrency {
        #[arg(long)]
        html_file: Option<PathBuf>,
    },
    /// Block-aware chunker for cleaned ATO HTML. Mirrors
    /// src/ato_mcp/indexer/chunk.py:chunk_html. JSON out:
    /// [{ord, anchor, text, definition_text}, ...].
    ChunkHtml {
        #[arg(long)]
        html_file: Option<PathBuf>,
        #[arg(long)]
        root_title: Option<String>,
        #[arg(long, default_value_t = 1024)]
        max_tokens: usize,
    },
    /// Derive metadata fields from an ATO canonical_id / doc_id. Mirrors
    /// src/ato_mcp/indexer/metadata.py public helpers. JSON out:
    /// {doc_id, type_prefix, year, human_code}.
    DocMeta { canonical_id: String },
    /// Parse an ATO link href into (doc_id, pit, view). Mirrors
    /// extract.py:_doc_id_from_ato_link. JSON out: null OR
    /// {doc_id, pit, view}.
    DocIdFromLink { href: String },
    /// Write a pack file from a JSONL stream of {"doc_id": str, "record": {...}}
    /// records on stdin. Mirrors src/ato_mcp/indexer/pack.py:PackWriter.
    /// JSON out on stdout: {pack_path, sha8, sha256, size, refs}.
    PackWrite {
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 3)]
        level: i32,
    },
    /// Rewrite the `packs[*].url` in a manifest.json to point at GitHub
    /// release asset download URLs. Mirrors
    /// src/ato_mcp/indexer/release.py:rewrite_manifest_urls.
    ManifestRewriteUrls {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        repo: String,
        #[arg(long)]
        tag: String,
    },
    /// Bundle the embedding model + tokenizer into a single `.tar.zst`.
    /// Mirrors src/ato_mcp/indexer/release.py:bundle_model. JSON out:
    /// {sha256, size}.
    BundleModel {
        #[arg(long)]
        model_dir: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Files to include. Names starting with `model_`
        /// are looked up under `<model_dir>/onnx/`; others under `<model_dir>/`.
        #[arg(long, value_delimiter = ',', default_values_t = vec![
            "model_fp16.onnx".to_string(),
            "model_fp16.onnx_data".to_string(),
            "tokenizer.json".to_string(),
        ])]
        include: Vec<String>,
        #[arg(long, default_value_t = 3)]
        level: i32,
    },
    /// [SS-02] Fetch a node list from the ATO browse-content API. Mirrors
    /// src/ato_mcp/scraper/client.py:AtoBrowseClient.fetch_nodes.
    /// JSON out: [{...}, ...] (verbatim ATO response).
    AtoFetchNodes {
        /// Either a raw query string or "key=value,key=value,..." pairs.
        query: String,
        #[arg(
            long,
            default_value = "https://www.ato.gov.au/API/v1/law/lawservices/browse-content/"
        )]
        base_url: String,
        #[arg(long, default_value_t = 30.0)]
        timeout_seconds: f64,
    },
    /// Encode each input line as a quantized semantic embedding.
    /// Reads texts (one per line) from stdin or --input-file. Emits a JSON
    /// array of base64-encoded raw int8 byte strings (256 dims, 256 bytes
    /// per embedding) to stdout — same shape the build pipeline writes
    /// into pack records.
    Embed {
        #[arg(long)]
        input_file: Option<PathBuf>,
        /// Use the CUDA execution provider. Requires a binary built with --features cuda.
        #[arg(long)]
        gpu: bool,
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
    /// [SS-01] Source acquisition is split into whats-new + scrape-diff
    /// incremental, tree-crawl + snapshot-reduce full, and deduped catch-up.
    /// Fetch the ATO "What's New" feed and return the deduped doc entries
    /// as JSON. Mirrors src/ato_mcp/scraper/whats_new.py:WhatsNewFetcher.
    WhatsNew {
        #[arg(
            long,
            default_value = "https://www.ato.gov.au/law/view/whatsnew.htm?fid=whatsnew"
        )]
        url: String,
        #[arg(long, default_value_t = 30.0)]
        timeout_seconds: f64,
    },
    /// Normalise an ATO law/view/document href to its canonical relative
    /// form (drops PiT, decodes percent-encoded docid, etc.). Mirrors
    /// src/ato_mcp/scraper/whats_new.py:normalize_doc_href.
    NormalizeDocHref { href: String },
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
    /// in an existing index.jsonl. Used by maintainer-sync.sh for both
    /// incremental (input from `ato-mcp whats-new`) and catch_up
    /// (input from `ato-mcp snapshot-reduce`'s deduped_links.jsonl).
    /// Mirrors src/ato_mcp/scraper/pipeline.py:_run_whats_new
    /// and pipeline.py:_run_catch_up's diff step.
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
        Command::Extract {
            html_file,
            doc_id,
            source_path,
        } => {
            let html = match html_file.as_ref() {
                Some(p) => {
                    fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?
                }
                None => {
                    let mut s = String::new();
                    std::io::stdin()
                        .read_to_string(&mut s)
                        .context("reading stdin")?;
                    s
                }
            };
            let cleaned = clean_ato_html(&html);
            // EM front-matter + leading-heading title composition read raw
            // (pre-mutation) container HTML, matching Python.
            let leading = extract_leading_headings(&cleaned.html);
            let composed_title = extract_compose_title(&leading);
            let title = composed_title.or(cleaned.title.clone());
            let (fm_refs, fm_phrase) = extract_em_front_matter(&cleaned.html);
            // Mutation chain on the cleaned container HTML, mirroring Python's
            // _rewrite_images_html + _normalise_named_anchors + _strip_attributes.
            let (rewritten_html, assets) = if doc_id.is_some() && source_path.is_some() {
                rewrite_images_html(&cleaned.html, doc_id.as_deref(), source_path.as_deref())
            } else {
                (cleaned.html.clone(), Vec::new())
            };
            let normalised = normalise_named_anchors(&rewritten_html);
            let with_links = rewrite_links_html(&normalised);
            let final_html = strip_attributes(&with_links);
            // Re-render text from the mutated HTML using the chunker's walker
            // so [asset:X] markers (from rewrite_images_html spans) appear and
            // tables render as pipe-separated rows — matches Python's
            // chunk.html_to_text used by extract.extract().
            let text = chunker_html_to_text(&final_html);
            // Headings + heading_levels + anchors are read AFTER mutations.
            let final_doc = scraper::Html::parse_fragment(&final_html);
            let heading_sel = scraper::Selector::parse("h1, h2, h3, h4, h5, h6").unwrap();
            let mut headings: Vec<String> = Vec::new();
            let mut heading_levels: Vec<u32> = Vec::new();
            for h in final_doc.select(&heading_sel) {
                let t = anchors_node_text(h);
                headings.push(t);
                let lvl: u32 = h.value().name()[1..].parse().unwrap_or(0);
                heading_levels.push(lvl);
            }
            let anchors_pairs = extract_collect_anchors(&final_doc);
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "text": text,
                    "html": final_html,
                    "title": title,
                    "html_title": cleaned.title,
                    "headings": headings,
                    "heading_levels": heading_levels,
                    "anchors": anchors_pairs,
                    "front_matter_refs": fm_refs,
                    "front_matter_phrase": fm_phrase,
                    "assets": assets,
                }))?
            );
            Ok(())
        }
        Command::ExtractDefinitions { input_file } => {
            #[derive(Deserialize)]
            struct Input {
                doc_id: String,
                source_title: String,
                source_type: String,
                chunks: Vec<DefinitionChunk>,
            }
            let raw = match input_file {
                Some(p) => {
                    fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?
                }
                None => {
                    let mut s = String::new();
                    std::io::stdin()
                        .read_to_string(&mut s)
                        .context("reading stdin")?;
                    s
                }
            };
            let input: Input =
                serde_json::from_str(&raw).context("parsing extract-definitions input")?;
            let defs = extract_definitions(
                &input.doc_id,
                &input.source_title,
                &input.source_type,
                &input.chunks,
            );
            println!("{}", serde_json::to_string_pretty(&defs)?);
            Ok(())
        }
        Command::ExtractAnchors {
            html_file,
            source_doc_id,
        } => {
            let html = match html_file {
                Some(p) => {
                    fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?
                }
                None => {
                    let mut s = String::new();
                    std::io::stdin()
                        .read_to_string(&mut s)
                        .context("reading stdin")?;
                    s
                }
            };
            let refs = extract_anchors(&html, &source_doc_id);
            println!("{}", serde_json::to_string_pretty(&refs)?);
            Ok(())
        }
        Command::ExtractCurrency { html_file } => {
            let html = match html_file {
                Some(p) => {
                    fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?
                }
                None => {
                    let mut s = String::new();
                    std::io::stdin()
                        .read_to_string(&mut s)
                        .context("reading stdin")?;
                    s
                }
            };
            let info = extract_currency(&html);
            println!("{}", serde_json::to_string_pretty(&info)?);
            Ok(())
        }
        Command::ChunkHtml {
            html_file,
            root_title,
            max_tokens,
        } => {
            let html = match html_file {
                Some(p) => {
                    fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?
                }
                None => {
                    let mut s = String::new();
                    std::io::stdin()
                        .read_to_string(&mut s)
                        .context("reading stdin")?;
                    s
                }
            };
            let chunks = chunk_html(&html, root_title.as_deref(), max_tokens);
            println!("{}", serde_json::to_string_pretty(&chunks)?);
            Ok(())
        }
        Command::DocMeta { canonical_id } => {
            let doc_id = metadata_doc_id_for(&canonical_id);
            let type_prefix = metadata_parse_docid(&canonical_id);
            let year = metadata_year_for_docid(&canonical_id);
            let human_code = metadata_human_code_for_doc_id(&doc_id);
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "doc_id": doc_id,
                    "type_prefix": type_prefix,
                    "year": year,
                    "human_code": human_code,
                }))?
            );
            Ok(())
        }
        Command::DocIdFromLink { href } => {
            let resolved = doc_id_from_ato_link(&href);
            let value = match resolved {
                Some((doc_id, pit, view)) => json!({
                    "doc_id": doc_id,
                    "pit": pit,
                    "view": view,
                }),
                None => JsonValue::Null,
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        Command::PackWrite { out, level } => {
            use std::io::BufRead as _;
            let stdin = std::io::stdin();
            let lock = stdin.lock();
            let records = lock.lines().map(|line_res| -> Result<(String, JsonValue)> {
                let line = line_res?;
                if line.trim().is_empty() {
                    bail!("empty JSONL line");
                }
                let v: JsonValue = serde_json::from_str(&line)?;
                let doc_id = v
                    .get("doc_id")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| anyhow!("missing 'doc_id' in JSONL record"))?
                    .to_string();
                let record = v
                    .get("record")
                    .cloned()
                    .ok_or_else(|| anyhow!("missing 'record' in JSONL record"))?;
                Ok((doc_id, record))
            });
            let summary = write_pack(&out, level, records)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        Command::ManifestRewriteUrls {
            manifest,
            repo,
            tag,
        } => {
            let raw = fs::read_to_string(&manifest)
                .with_context(|| format!("reading {}", manifest.display()))?;
            let mut value: JsonValue = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", manifest.display()))?;
            if let Some(packs) = value.get_mut("packs").and_then(|v| v.as_array_mut()) {
                for pack in packs {
                    if let Some(url) = pack.get("url").and_then(|v| v.as_str()) {
                        let filename = std::path::Path::new(url)
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or(url)
                            .to_string();
                        let new_url =
                            format!("https://github.com/{repo}/releases/download/{tag}/{filename}");
                        pack["url"] = JsonValue::String(new_url);
                    }
                }
            }
            let pretty = serde_json::to_vec_pretty(&value)?;
            fs::write(&manifest, pretty)
                .with_context(|| format!("writing {}", manifest.display()))?;
            Ok(())
        }
        Command::BundleModel {
            model_dir,
            out,
            include,
            level,
        } => {
            use std::io::Write as _;
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            // Build the uncompressed tar in memory.
            let mut tar_buf: Vec<u8> = Vec::new();
            {
                let mut builder = tar::Builder::new(&mut tar_buf);
                for name in &include {
                    let candidate = if name.starts_with("model_") {
                        model_dir.join("onnx").join(name)
                    } else {
                        model_dir.join(name)
                    };
                    if !candidate.exists() {
                        bail!("model bundle missing {}", candidate.display());
                    }
                    let mut f = File::open(&candidate)
                        .with_context(|| format!("opening {}", candidate.display()))?;
                    builder.append_file(name, &mut f)?;
                }
                builder.finish()?;
            }
            // Stream zstd-compress to disk + sha256 the output.
            let compressed = zstd::stream::encode_all(std::io::Cursor::new(&tar_buf), level)?;
            let mut file =
                File::create(&out).with_context(|| format!("creating {}", out.display()))?;
            file.write_all(&compressed)?;
            file.flush()?;
            let mut hasher = Sha256::new();
            hasher.update(&compressed);
            let digest = hasher.finalize();
            let sha256_hex = digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let size = fs::metadata(&out)?.len();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "sha256": sha256_hex,
                    "size": size,
                }))?
            );
            Ok(())
        }
        Command::AtoFetchNodes {
            query,
            base_url,
            timeout_seconds,
        } => {
            // Build the URL — accept either a raw "key=value&key=value" query
            // string or a "key=value,key=value" comma-form (same as Python
            // accepts `Union[str, Dict[str, str]]`).
            let query_string = if query.contains('=') && !query.contains('&') {
                if query.contains(',') {
                    query.replace(',', "&")
                } else {
                    query.clone()
                }
            } else {
                query.trim_start_matches('?').to_string()
            };
            let url = if query_string.is_empty() {
                base_url.clone()
            } else {
                format!("{}?{}", base_url.trim_end_matches('?'), query_string)
            };
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs_f64(timeout_seconds))
                .build()?;
            let resp = client
                .get(&url)
                .send()
                .with_context(|| format!("fetching {url}"))?;
            let status = resp.status();
            if !status.is_success() {
                bail!("ATO API returned HTTP {status} for {url}");
            }
            let body = resp.text().context("reading ATO API body")?;
            let payload: JsonValue = serde_json::from_str(&body).context("parsing ATO API JSON")?;
            if !payload.is_array() {
                bail!("ATO response payload is not a list");
            }
            println!("{}", serde_json::to_string_pretty(&payload)?);
            Ok(())
        }
        Command::Embed { input_file, gpu } => {
            use base64::Engine as _;
            let raw = match input_file.as_ref() {
                Some(p) => {
                    fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?
                }
                None => {
                    let mut s = String::new();
                    std::io::stdin()
                        .read_to_string(&mut s)
                        .context("reading stdin")?;
                    s
                }
            };
            let state = ServerState::new(gpu);
            let inputs: Vec<String> = raw
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect();
            let embeddings = state
                .encode_query_embeddings(&inputs)
                .context("encoding embeddings")?;
            let mut out: Vec<JsonValue> = Vec::with_capacity(embeddings.len());
            for emb in embeddings {
                let bytes: &[u8] =
                    unsafe { std::slice::from_raw_parts(emb.as_ptr() as *const u8, emb.len()) };
                out.push(JsonValue::String(
                    base64::engine::general_purpose::STANDARD.encode(bytes),
                ));
            }
            println!("{}", serde_json::to_string_pretty(&out)?);
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
        Command::WhatsNew {
            url,
            timeout_seconds,
        } => {
            let client = reqwest::blocking::Client::builder()
                .user_agent(ATO_USER_AGENT)
                .timeout(Duration::from_secs_f64(timeout_seconds))
                .build()?;
            let resp = client
                .get(&url)
                .send()
                .with_context(|| format!("fetching {url}"))?;
            let status = resp.status();
            if !status.is_success() {
                bail!("ATO whatsnew returned HTTP {status} for {url}");
            }
            let html = resp.text().context("reading whatsnew body")?;
            let entries = parse_whats_new(&html, "https://www.ato.gov.au")?;
            println!("{}", serde_json::to_string_pretty(&entries)?);
            Ok(())
        }
        Command::NormalizeDocHref { href } => {
            println!("{}", normalize_doc_href(&href));
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

fn http_config_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("http.json"))
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct HttpConfig {
    bind: String,
    port: u16,
}

impl HttpConfig {
    fn load() -> Result<Option<Self>> {
        let p = http_config_path()?;
        if !p.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let cfg: Self =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", p.display()))?;
        Ok(Some(cfg))
    }

    /// Return the existing config or pick a free port and persist a new one.
    /// Used by both the shim (auto-init on first run) and the daemon itself
    /// so the user never has to call `install-http` for the auto-managed
    /// path to work. Serialised against concurrent creators via the spawn
    /// lock so two parallel shims don't write conflicting ports.
    fn load_or_init() -> Result<Self> {
        if let Some(cfg) = Self::load()? {
            return Ok(cfg);
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
        // Re-check after acquiring the lock — another writer may have
        // initialised the config while we were waiting.
        if let Some(cfg) = Self::load()? {
            return Ok(cfg);
        }
        let cfg = Self {
            bind: "127.0.0.1".to_string(),
            port: pick_free_port()?,
        };
        cfg.save()?;
        drop(lock_file);
        Ok(cfg)
    }

    fn save(&self) -> Result<()> {
        let p = http_config_path()?;
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(&p, raw).with_context(|| format!("writing {}", p.display()))?;
        Ok(())
    }

    fn url(&self) -> String {
        format!("http://{}:{}/mcp", self.bind, self.port)
    }
}

/// Bind 127.0.0.1:0, ask the OS for a free port in the ephemeral range,
/// release the socket, and return the chosen port. The port is not held
/// across the function so a tight race with another process can still claim
/// it; `serve()` then errors at bind time and the user can re-run install.
fn pick_free_port() -> Result<u16> {
    use std::net::TcpListener;
    let listener =
        TcpListener::bind("127.0.0.1:0").context("binding 127.0.0.1:0 to discover a free port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("LOCK"))
}

fn model_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("model_fp16.onnx"))
}

fn model_data_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("model_fp16.onnx_data"))
}

fn tokenizer_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("tokenizer.json"))
}

fn model_marker_path() -> Result<PathBuf> {
    Ok(live_dir()?.join(".model.sha256"))
}

#[derive(Clone, Debug)]
struct SemanticModelPaths {
    model: PathBuf,
    tokenizer: PathBuf,
}

impl SemanticModelPaths {
    fn live() -> Result<Self> {
        let model = model_path()?;
        let model_data = model_data_path()?;
        let tokenizer = tokenizer_path()?;
        for path in [&model, &model_data, &tokenizer] {
            if !path.is_file() {
                bail!("missing installed Granite model file at {}", path.display());
            }
        }
        Ok(Self { model, tokenizer })
    }

    fn from_model_dir(model_dir: &Path) -> Result<Self> {
        for file in EMBEDDING_MODEL_HF_FILES {
            let path = model_dir.join(file.path);
            validate_embedding_model_file(&path, file)?;
        }
        let model = model_dir.join("onnx").join("model_fp16.onnx");
        let tokenizer = model_dir.join("tokenizer.json");
        Ok(Self { model, tokenizer })
    }
}

fn validate_embedding_model_file(path: &Path, file: &HfModelFile) -> Result<()> {
    if !path.is_file() {
        bail!("missing Granite model file at {}", path.display());
    }
    let size = path.metadata()?.len();
    if size != file.size {
        bail!(
            "size mismatch for Granite model file {}: got {}, expected {}",
            path.display(),
            size,
            file.size
        );
    }
    verify_sha256_file(path, file.sha256)
        .with_context(|| format!("verifying Granite model file {}", path.display()))
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
            "no live DB found at {}; run `ato-mcp update` first",
            path.display()
        );
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening local corpus database")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // [SL-05] Read-only handles skip WAL/synchronous mutation pragmas but
    // still use in-memory temp storage for query work.
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
    // [SL-05] Write handles enable WAL + synchronous=NORMAL and temp_store
    // MEMORY before schema initialization or mutation.
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
            "no schema_version metadata; corpus may be corrupt or incomplete; run `ato-mcp update`"
        );
    }
    match get_meta(conn, "schema_version")? {
        None => bail!(
            "no schema_version metadata; corpus may be corrupt or incomplete; run `ato-mcp update`"
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
            -- [SL-03] Chunk bodies are zstd-compressed UTF-8 BLOBs; heading
            -- and inline markers are part of the stored chunk text.
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
            -- [SL-10] Build-time anchors cover in-doc, sister-doc, and
            -- historical-version navigation for get_doc_anchors.
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
            -- [SL-11] Reverse citations are derived from [doc:X] markers
            -- and keyed by source chunk + target doc.
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
            -- [SL-04] FTS uses porter unicode61 with diacritic folding for
            -- English legal text in titles/headings and chunks.
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
    /// When set, this arbitrary text is runtime-embedded and used as the
    /// query vector — the same mechanism as `similar_to_chunk_id` but for
    /// text that isn't a corpus chunk (e.g. a chunk returned by
    /// `fetch_external_doc`). Forces vector-only mode and skips title hits,
    /// like `similar_to_chunk_id`. `similar_to_chunk_id` wins if both are set.
    seed_text: Option<&'a str>,
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

fn search(
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

fn search_cli(query: &str, opts: SearchOptions<'_>) -> Result<(String, ServerState)> {
    let state = ServerState::default();
    let out = search(query, opts, Some(&state))?;
    Ok((out, state))
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
    // Seed-driven searches: preserve the seed so paging re-runs the same
    // semantic query rather than falling back to a plain `query` search.
    if let Some(seed_id) = opts.similar_to_chunk_id {
        args.push(format!("similar_to_chunk_id={seed_id}"));
    } else if let Some(seed) = opts.seed_text {
        args.push(format!("seed_text={}", mcp_string(seed)));
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
    fn load(use_gpu: bool, model_paths: &SemanticModelPaths) -> Result<Self> {
        let mut tokenizer = Tokenizer::from_file(&model_paths.tokenizer)
            .map_err(|err| anyhow!("loading tokenizer: {err}"))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: EMBEDDING_INPUT_MAX_TOKENS,
                ..TruncationParams::default()
            }))
            .map_err(|err| anyhow!("configuring tokenizer truncation: {err}"))?;
        tokenizer.with_padding(Some(PaddingParams::default()));

        let optimization_level = if use_gpu {
            GraphOptimizationLevel::All
        } else {
            ONLINE_MODEL_OPTIMIZATION_LEVEL
        };
        let mut builder = Session::builder()
            .map_err(|err| anyhow!("creating ONNX Runtime session: {err}"))?
            .with_optimization_level(optimization_level)
            .map_err(|err| anyhow!("configuring ONNX Runtime session: {err}"))?;
        if use_gpu {
            // [EM-01] CPU is the default runtime; maintainer GPU builds
            // require the cuda feature and fail if CUDA EP registration fails.
            builder = configure_cuda_execution_provider(builder)?;
        }
        let session = builder
            .commit_from_file(&model_paths.model)
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
        let mut embeddings = self.encode_queries(&[query.to_string()])?;
        embeddings
            .pop()
            .ok_or_else(|| anyhow!("semantic encoder returned no query embedding"))
    }

    fn encode_queries(&mut self, queries: &[String]) -> Result<Vec<[i8; EMBEDDING_DIM]>> {
        let (embeddings, _stats) = self.encode_queries_with_stats(queries)?;
        Ok(embeddings)
    }

    fn encode_queries_with_stats(
        &mut self,
        queries: &[String],
    ) -> Result<(Vec<[i8; EMBEDDING_DIM]>, SemanticEncodeStats)> {
        if queries.is_empty() {
            return Ok((Vec::new(), SemanticEncodeStats::default()));
        }
        let prefixed: Vec<String> = queries
            .iter()
            .map(|query| format!("{EMBEDDING_TEXT_PREFIX}{query}"))
            .collect();
        let mut stats = SemanticEncodeStats::default();
        let started = std::time::Instant::now();
        let encodings = self
            .tokenizer
            .encode_batch(prefixed, true)
            .map_err(|err| anyhow!("tokenizing queries: {err}"))?;
        stats.tokenize += started.elapsed();
        let batch = encodings.len();
        if batch != queries.len() {
            bail!(
                "tokenizer returned {} encodings for {} inputs",
                batch,
                queries.len()
            );
        }
        let seq_len = encodings
            .first()
            .map(|encoding| encoding.get_ids().len())
            .unwrap_or(0);
        if seq_len == 0 {
            bail!("semantic search unavailable: query produced no tokens");
        }
        let started = std::time::Instant::now();
        let mut input_ids = Vec::with_capacity(batch * seq_len);
        let mut attention_mask = Vec::with_capacity(batch * seq_len);
        let mut active_tokens = 0usize;
        for encoding in &encodings {
            if encoding.get_ids().len() != seq_len {
                bail!(
                    "tokenizer produced ragged encodings: expected {seq_len}, got {}",
                    encoding.get_ids().len()
                );
            }
            input_ids.extend(encoding.get_ids().iter().map(|id| i64::from(*id)));
            for mask in encoding.get_attention_mask() {
                active_tokens += usize::try_from(*mask).unwrap_or(0);
                attention_mask.push(i64::from(*mask));
            }
        }
        stats.record_batch(batch, seq_len, active_tokens);

        let input_ids_tensor =
            TensorRef::from_array_view(([batch, seq_len], input_ids.as_slice()))?;
        let attention_mask_tensor =
            TensorRef::from_array_view(([batch, seq_len], attention_mask.as_slice()))?;
        stats.prepare += started.elapsed();
        let started = std::time::Instant::now();
        let outputs = if self.has_token_type_ids {
            let token_type_ids = vec![0i64; batch * seq_len];
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
        stats.run += started.elapsed();
        let started = std::time::Instant::now();
        let output = outputs
            .get("sentence_embedding")
            .unwrap_or_else(|| &outputs[0]);
        // [EM-04] Prefer sentence_embedding when present; otherwise
        // pooled_embeddings mean-pools 3D token outputs with the attention mask.
        let (shape, data) = output.try_extract_tensor::<f32>()?;
        let embeddings = pooled_embeddings(shape, data, &attention_mask, batch, seq_len)?;
        let embeddings = embeddings
            .iter()
            .map(|embedding| quantize_embedding(embedding))
            .collect::<Result<Vec<_>>>()?;
        stats.postprocess += started.elapsed();
        Ok((embeddings, stats))
    }
}

#[cfg(feature = "cuda")]
fn configure_cuda_execution_provider(builder: SessionBuilder) -> Result<SessionBuilder> {
    let cuda = ep::CUDA::default()
        .with_device_id(0)
        .with_conv_algorithm_search(ep::cuda::ConvAlgorithmSearch::Heuristic)
        .build()
        .error_on_failure();
    builder
        .with_execution_providers([cuda])
        .map_err(|err| anyhow!("registering CUDA execution provider: {err}"))
}

#[cfg(not(feature = "cuda"))]
fn configure_cuda_execution_provider(_builder: SessionBuilder) -> Result<SessionBuilder> {
    bail!("GPU build requested but this ato-mcp binary was built without CUDA support; rebuild with `cargo build --release --features cuda`")
}

// [MT-01] HTTP transport keeps one ServerState shared across worker threads.
// The semantic runtime is loaded lazily and reused across tool calls. Search-time
// inference holds the lock for one query embedding; read-only tools
// (get_chunks, get_definition, get_doc_anchors, get_asset, stats) run fully
// concurrently.
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

fn encode_query_embedding(query: &str) -> Result<[i8; EMBEDDING_DIM]> {
    let model_paths = SemanticModelPaths::live()?;
    let mut runtime = SemanticRuntime::load(false, &model_paths)?;
    runtime.encode_query(query)
}

fn pooled_embeddings(
    shape: &[i64],
    data: &[f32],
    attention_mask: &[i64],
    batch: usize,
    seq_len: usize,
) -> Result<Vec<Vec<f32>>> {
    match shape {
        [out_batch, dims] => {
            let out_batch = *out_batch as usize;
            let dims = *dims as usize;
            if out_batch != batch {
                bail!("model output batch {out_batch} does not match input batch {batch}");
            }
            if data.len() < batch * dims {
                bail!("model output too short for shape {:?}", shape);
            }
            Ok((0..batch)
                .map(|idx| data[idx * dims..(idx + 1) * dims].to_vec())
                .collect())
        }
        [out_batch, out_seq_len, dims] => {
            let out_batch = *out_batch as usize;
            let out_seq_len = *out_seq_len as usize;
            let dims = *dims as usize;
            if out_batch != batch || out_seq_len != seq_len {
                bail!(
                    "model output shape {:?} does not match input batch={batch} seq_len={seq_len}",
                    shape
                );
            }
            if data.len() < batch * seq_len * dims {
                bail!("model output too short for shape {:?}", shape);
            }
            let mut out = Vec::with_capacity(batch);
            for batch_idx in 0..batch {
                let mut pooled = vec![0.0f32; dims];
                let mut denom = 0.0f32;
                for token_idx in 0..seq_len {
                    let mask = attention_mask[batch_idx * seq_len + token_idx] as f32;
                    denom += mask;
                    let offset = (batch_idx * seq_len + token_idx) * dims;
                    for dim in 0..dims {
                        pooled[dim] += data[offset + dim] * mask;
                    }
                }
                let denom = denom.max(1e-6);
                for value in &mut pooled {
                    *value /= denom;
                }
                out.push(pooled);
            }
            Ok(out)
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
    if values.iter().any(|value| !value.is_finite()) {
        bail!("model output contains non-finite embedding values");
    }
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() {
        bail!("model output produced a non-finite embedding norm");
    }
    if norm <= 1e-12 {
        return Ok([0; EMBEDDING_DIM]);
    }
    // [EM-06] After L2 normalisation, values are clipped, scaled by 127,
    // rounded, and stored as int8 bytes.
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

/// Title-level hits for a query: exact doc_id / ATO-link lookups first,
/// then BM25 over the separate `title_fts` table. A parallel algorithm to
/// chunk search — `search` calls this to populate its `title_hits`
/// sidebar. The caller supplies the connection and the already-built
/// document filter so chunk and title queries stay consistently scoped.
fn collect_title_hits(
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

// ----- External fetch (live ATO scrape) -----
//
// Ported subset of src/ato_mcp/indexer/extract.py: pick_container, strip_noise,
// strip_history_ui_controls, html_to_text. Used at runtime to follow [doc:X]
// markers whose target id isn't in the local corpus (subdivisions, paragraph
// refs, footnote pointers, historical PiT views). The full Python pipeline
// remains the build-time path; this is the minimum-viable subset that returns
// legible plain text to an agent.

const ATO_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const ATO_USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; ato-mcp/",
    env!("CARGO_PKG_VERSION"),
    "; +https://github.com/gunba/ato-mcp)"
);
// [IB-06] Containers ATO has used over the years. First selector match wins;
// pick_container_html falls back to <main>/<body> if none match.
const ATO_CONTAINER_SELECTORS: &[&str] =
    &["#LawContent", "#lawContents", "#LawContents", "#contents"];
// Strip these wholesale before any text extraction. Mirrors extract.py:_strip_noise.
const ATO_NOISE_SELECTORS: &[&str] = &[
    "script",
    "style",
    "noscript",
    "template",
    "nav",
    "#LawMiniMenuHeader",
    ".minimenu",
    ".minimenu-bar",
];
// History-toggle UI labels — case-insensitive match on text-node content and
// img title/alt attributes. Mirrors extract.py:_HISTORY_UI_LABELS.
const ATO_HISTORY_UI_LABELS: &[&str] = &[
    "view history note",
    "hide history note",
    "view history reference",
    "hide history reference",
];

// ----- Definition extraction (port of src/ato_mcp/indexer/definitions.py) -----

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DefinitionChunk {
    ord: i64,
    anchor: Option<String>,
    text: String,
}

#[derive(Debug, Clone, Serialize)]
struct Definition {
    definition_id: String,
    term: String,
    norm_term: String,
    doc_id: String,
    source_title: String,
    source_type: String,
    scope: Option<String>,
    anchor: Option<String>,
    ord: i64,
    body: String,
}

fn defs_normalise_term(term: &str) -> String {
    let t: String = term.replace("\\*", "*").replace("\\&", "&");
    let t = t.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\r' | '\n' | ':' | '*'));
    let mut out = String::with_capacity(t.len());
    let mut last_ws = false;
    for c in t.chars() {
        if c.is_whitespace() {
            if !last_ws {
                out.push(' ');
                last_ws = true;
            }
        } else {
            out.push(c);
            last_ws = false;
        }
    }
    out.to_lowercase()
}

fn defs_clean_term(term: &str) -> String {
    let s = term.replace('\n', " ");
    let mut out = String::with_capacity(s.len());
    let mut last_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_ws {
                out.push(' ');
                last_ws = true;
            }
        } else {
            out.push(c);
            last_ws = false;
        }
    }
    out.trim_matches(|c: char| matches!(c, ' ' | ':' | '*'))
        .to_string()
}

fn defs_clean_body(body: &str) -> String {
    let trimmed = body.trim();
    // Collapse runs of 3+ newlines to two.
    let re = Regex::new(r"\n{3,}").unwrap();
    re.replace_all(trimmed, "\n\n").to_string()
}

fn defs_definition_id(doc_id: &str, ord: i64, term: &str, body: &str, offset: usize) -> String {
    let mut h = Sha256::new();
    h.update(doc_id.as_bytes());
    h.update(b"\0");
    h.update(ord.to_string().as_bytes());
    h.update(b"\0");
    h.update(offset.to_string().as_bytes());
    h.update(b"\0");
    h.update(defs_normalise_term(term).as_bytes());
    h.update(b"\0");
    h.update(body.as_bytes());
    let digest = h.finalize();
    let hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    hex[..20].to_string()
}

fn defs_scope_from_title(title: &str, source_type: &str) -> Option<String> {
    if title.contains(" s ") {
        Some(title.to_string())
    } else if !source_type.is_empty() {
        Some(source_type.to_string())
    } else {
        None
    }
}

fn extract_definitions(
    doc_id: &str,
    source_title: &str,
    source_type: &str,
    chunks: &[DefinitionChunk],
) -> Vec<Definition> {
    // Match `***term***` markers — same regex as definitions.py:_TERM_RE.
    let term_re = Regex::new(r"\*\*\*\s*([^*\n][^*]{0,180}?)\s*\*\*\*").unwrap();
    let cue_re = Regex::new(
        r"(?im)^\s*(?:,?\s*of\b|,?\s*in relation\b|:|means\b|includes\b|has\b|is\b|\(Repealed\b)",
    )
    .unwrap();

    let mut out: Vec<Definition> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();

    for chunk in chunks {
        let matches: Vec<regex::Match> = term_re.find_iter(&chunk.text).collect();
        // Capture groups for each match — need them to extract the term text.
        let captures: Vec<regex::Captures> = term_re.captures_iter(&chunk.text).collect();
        if matches.is_empty() {
            continue;
        }
        for (idx, m) in matches.iter().enumerate() {
            let term_raw = captures[idx].get(1).map(|c| c.as_str()).unwrap_or("");
            let term = defs_clean_term(term_raw);
            if term.is_empty() {
                continue;
            }
            let next_start = matches
                .get(idx + 1)
                .map(|m| m.start())
                .unwrap_or(chunk.text.len());
            let body_start = m.end();
            let body_slice = &chunk.text[body_start..next_start];
            let mut body = defs_clean_body(body_slice);
            // Handle "***term*** or ***other***" / "***term*** and ***other***" pattern:
            // body collapses to "or"/"and"; the real definition follows the next term marker.
            let body_lc = body.to_lowercase();
            if (body_lc == "or" || body_lc == "and") && idx + 1 < matches.len() {
                let next_m = &matches[idx + 1];
                let next_next_start = matches
                    .get(idx + 2)
                    .map(|m| m.start())
                    .unwrap_or(chunk.text.len());
                body = defs_clean_body(&chunk.text[next_m.end()..next_next_start]);
            }
            if body.len() < 4 || cue_re.find(&body).is_none() {
                continue;
            }
            let norm = defs_normalise_term(&term);
            let key = (norm.clone(), doc_id.to_string(), body.clone());
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            out.push(Definition {
                definition_id: defs_definition_id(doc_id, chunk.ord, &term, &body, m.start()),
                term: term.clone(),
                norm_term: norm,
                doc_id: doc_id.to_string(),
                source_title: source_title.to_string(),
                source_type: source_type.to_string(),
                scope: defs_scope_from_title(source_title, source_type),
                anchor: chunk.anchor.clone(),
                ord: chunk.ord,
                body,
            });
        }
    }
    out
}

// ----- Doc-navigation anchors (port of src/ato_mcp/indexer/anchors.py) -----
//
// Walks cleaned HTML for a single doc and classifies every <a href> into
// one of three kinds, mirroring the Python module: in_doc (#X target inside
// this doc), sister (cross-doc link, no PiT), history (cross-doc with PiT
// timestamp pointing at a historical version we don't store).

const ANCHORS_SENTINEL_PITS: &[&str] = &["99991231235958", "10010101000001"];

#[derive(Debug, Clone, Serialize)]
struct AnchorRef {
    kind: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_anchor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_doc_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_pit: Option<String>,
}

fn anchors_pit_to_date(pit: &str) -> String {
    let s = pit.trim();
    if s.len() >= 8 && s[..8].chars().all(|c| c.is_ascii_digit()) {
        format!("{}-{}-{}", &s[..4], &s[4..6], &s[6..8])
    } else {
        s.to_string()
    }
}

fn anchors_collect_targets(doc: &scraper::Html) -> std::collections::HashSet<String> {
    use scraper::Selector;
    let mut targets = std::collections::HashSet::new();
    let a_name = Selector::parse("a[name]").unwrap();
    for el in doc.select(&a_name) {
        if let Some(name) = el.value().attr("name") {
            if !name.is_empty() {
                targets.insert(name.to_string());
            }
        }
    }
    let with_id = Selector::parse("[id]").unwrap();
    for el in doc.select(&with_id) {
        if let Some(nid) = el.value().attr("id") {
            if !nid.is_empty() {
                targets.insert(nid.to_string());
            }
        }
    }
    targets
}

fn anchors_find_ancestor<'a>(
    node: scraper::ElementRef<'a>,
    tags: &[&str],
) -> Option<scraper::ElementRef<'a>> {
    let mut current = node.parent();
    while let Some(p) = current {
        if let Some(el) = scraper::ElementRef::wrap(p) {
            if tags.contains(&el.value().name()) {
                return Some(el);
            }
        }
        current = p.parent();
    }
    None
}

fn anchors_node_text(node: scraper::ElementRef) -> String {
    let mut out = String::new();
    for s in node.text() {
        out.push_str(s);
    }
    let mut collapsed = String::with_capacity(out.len());
    let mut last_ws = true;
    for c in out.chars() {
        if c.is_whitespace() {
            if !last_ws {
                collapsed.push(' ');
                last_ws = true;
            }
        } else {
            collapsed.push(c);
            last_ws = false;
        }
    }
    collapsed.trim().to_string()
}

fn anchors_sibling_cells_text(a: scraper::ElementRef) -> String {
    let row = match anchors_find_ancestor(a, &["tr"]) {
        Some(r) => r,
        None => return String::new(),
    };
    let own_cell = anchors_find_ancestor(a, &["td", "th"]);
    let cell_sel = scraper::Selector::parse("td, th").unwrap();
    let mut parts: Vec<String> = Vec::new();
    for cell in row.select(&cell_sel) {
        if let Some(own) = own_cell {
            if cell.id() == own.id() {
                continue;
            }
        }
        let text = anchors_node_text(cell);
        if !text.is_empty() {
            parts.push(text);
        }
    }
    parts.join(" ").trim().to_string()
}

fn anchors_resolve_label(a: scraper::ElementRef, default_date: Option<&str>) -> String {
    let own = anchors_node_text(a);
    let sibling = anchors_sibling_cells_text(a);
    let mut parts: Vec<String> = Vec::new();
    if !sibling.is_empty() {
        parts.push(sibling);
    }
    if !own.is_empty() && !parts.iter().any(|p| p == &own) {
        parts.push(own);
    }
    let mut label = parts.join(" ").trim().to_string();
    if let Some(date) = default_date {
        label = if label.is_empty() {
            date.to_string()
        } else {
            format!("{label} ({date})")
        };
    }
    if label.is_empty() {
        "(unnamed)".to_string()
    } else {
        label
    }
}

fn extract_anchors(html: &str, source_doc_id: &str) -> Vec<AnchorRef> {
    if html.trim().is_empty() {
        return Vec::new();
    }
    let doc = scraper::Html::parse_document(html);
    let targets = anchors_collect_targets(&doc);
    let mut refs: Vec<AnchorRef> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, Option<String>, String)> =
        std::collections::HashSet::new();

    let a_sel = scraper::Selector::parse("a[href]").unwrap();
    for a in doc.select(&a_sel) {
        let href = a.value().attr("href").unwrap_or("");
        if let Some(target) = href.strip_prefix('#') {
            if target.is_empty() || !targets.contains(target) {
                continue;
            }
            let label = anchors_resolve_label(a, None);
            let key = (
                "in_doc".to_string(),
                target.to_string(),
                None,
                label.clone(),
            );
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            refs.push(AnchorRef {
                kind: "in_doc".to_string(),
                label,
                target_anchor: Some(target.to_string()),
                target_doc_id: None,
                target_pit: None,
            });
            continue;
        }
        let resolved = doc_id_from_ato_link(href);
        let Some((target_doc_id, mut pit, _view)) = resolved else {
            continue;
        };
        if let Some(p) = pit.as_ref() {
            if ANCHORS_SENTINEL_PITS.iter().any(|s| *s == p) {
                pit = None;
            }
        }
        if let Some(p) = pit {
            let date = anchors_pit_to_date(&p);
            let label = anchors_resolve_label(a, Some(&date));
            let key = (
                "history".to_string(),
                target_doc_id.clone(),
                Some(p.clone()),
                label.clone(),
            );
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            refs.push(AnchorRef {
                kind: "history".to_string(),
                label,
                target_anchor: None,
                target_doc_id: Some(target_doc_id),
                target_pit: Some(p),
            });
            continue;
        }
        if target_doc_id == source_doc_id {
            continue;
        }
        let label = anchors_resolve_label(a, None);
        let key = (
            "sister".to_string(),
            target_doc_id.clone(),
            None,
            label.clone(),
        );
        if seen.contains(&key) {
            continue;
        }
        seen.insert(key);
        refs.push(AnchorRef {
            kind: "sister".to_string(),
            label,
            target_anchor: None,
            target_doc_id: Some(target_doc_id),
            target_pit: None,
        });
    }
    refs
}

// ----- Title composition + EM front matter + anchor collection -----
// Ports of src/ato_mcp/indexer/extract.py:
//   _collect_anchors, _leading_headings, _compose_title,
//   _collect_em_front_matter

fn extract_collect_anchors(doc: &scraper::Html) -> Vec<(String, String)> {
    use scraper::Selector;
    let heading_sel = Selector::parse("h1, h2, h3, h4, h5, h6").unwrap();
    let inner_a = Selector::parse("a").unwrap();
    let mut out: Vec<(String, String)> = Vec::new();
    for heading in doc.select(&heading_sel) {
        let mut anchor: Option<String> = heading
            .value()
            .attr("id")
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        if anchor.is_none() {
            for a in heading.select(&inner_a) {
                if let Some(name) = a.value().attr("name").or_else(|| a.value().attr("id")) {
                    if !name.is_empty() {
                        anchor = Some(name.to_string());
                        break;
                    }
                }
            }
        }
        if let Some(a) = anchor {
            let text = anchors_node_text(heading);
            out.push((text, a));
        }
    }
    out
}

fn extract_leading_headings(container_html: &str) -> Vec<String> {
    use scraper::Selector;
    let frag = scraper::Html::parse_fragment(container_html);
    let heading_tags = ["h1", "h2", "h3", "h4", "h5", "h6"];
    let nested_heading_sel = Selector::parse("h1, h2, h3, h4, h5, h6").unwrap();

    let mut out: Vec<String> = Vec::new();
    let mut dived = false;
    // Walk direct children of the fragment root (which is a wrapper).
    // scraper's parse_fragment wraps in a synthetic root; we need to find
    // the "real" first-level container's children.
    let root = frag.root_element();
    let direct_children: Vec<_> = root
        .children()
        .filter_map(scraper::ElementRef::wrap)
        .collect();
    // If the root has a single element child, treat that as the container.
    let walk_children: Vec<scraper::ElementRef> = if direct_children.len() == 1 {
        direct_children[0]
            .children()
            .filter_map(scraper::ElementRef::wrap)
            .collect()
    } else {
        direct_children
    };
    for child in walk_children {
        let tag = child.value().name();
        if heading_tags.contains(&tag) {
            let text = anchors_node_text(child);
            if !text.is_empty() {
                out.push(text);
            }
            continue;
        }
        if dived {
            break;
        }
        // Wrapper that only carries headings? Dive once.
        let nested: Vec<_> = child.select(&nested_heading_sel).collect();
        let non_heading_len = anchors_node_text(child).len();
        if !nested.is_empty() && non_heading_len <= 800 {
            for h in nested {
                let t = anchors_node_text(h);
                if !t.is_empty() {
                    out.push(t);
                }
            }
            dived = true;
            continue;
        }
        if !anchors_node_text(child).is_empty() {
            break;
        }
    }
    out.into_iter().take(4).collect()
}

// [IB-07] Titles are composed from leading headings with adjacent prefix
// overlap suppression, then fall back to source title/doc_id in the build path.
fn extract_compose_title(headings: &[String]) -> Option<String> {
    let cleaned: Vec<String> = headings
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.len() == 1 {
        return Some(cleaned[0].clone());
    }
    let mut out: Vec<String> = Vec::new();
    for h in cleaned {
        if let Some(last) = out.last() {
            let h_lc = h.to_lowercase();
            let last_lc = last.to_lowercase();
            if last_lc.starts_with(&h_lc) || h_lc.starts_with(&last_lc) {
                continue;
            }
        }
        out.push(h);
    }
    Some(out.join(" — "))
}

fn extract_em_front_matter(container_html: &str) -> (Vec<String>, Option<String>) {
    use scraper::Selector;
    let frag = scraper::Html::parse_fragment(container_html);
    let lawfront_sel = Selector::parse("#Lawfront").unwrap();
    let Some(front) = frag.select(&lawfront_sel).next() else {
        return (Vec::new(), None);
    };
    let strong_sel = Selector::parse("strong").unwrap();
    let mut refs: Vec<String> = Vec::new();
    let mut phrase: Option<String> = None;
    for child in front.children().filter_map(scraper::ElementRef::wrap) {
        let tag = child.value().name();
        match tag {
            "div" => {
                let classes: Vec<&str> = child
                    .value()
                    .attr("class")
                    .unwrap_or("")
                    .split_whitespace()
                    .collect();
                if classes.contains(&"ref") {
                    if let Some(s) = child.select(&strong_sel).next() {
                        let t = anchors_node_text(s);
                        if !t.is_empty() {
                            refs.push(t);
                        }
                    }
                }
            }
            "p" if phrase.is_none() => {
                if let Some(s) = child.select(&strong_sel).next() {
                    let t = anchors_node_text(s);
                    if t.to_lowercase().starts_with("explanatory ") {
                        phrase = Some(t);
                    }
                }
            }
            _ => {}
        }
    }
    (refs, phrase)
}

// ----- Currency / withdrawal extraction (port of extract.py:extract_currency) -----
//
// Best-effort currency / supersession extraction from raw page HTML, mirroring
// src/ato_mcp/indexer/extract.py:extract_currency and its helpers. Each
// CurrencyInfo field is filled independently — alert panel beats body prose
// beats timeline table beats title-suffix sentinel.

#[derive(Debug, Clone, Default, Serialize)]
struct CurrencyInfo {
    withdrawn_date: Option<String>,
    superseded_by: Option<String>,
    replaces: Option<String>,
}

const CURRENCY_TITLE_SUFFIX_SENTINEL: &str = "0001-01-01";

fn currency_months() -> &'static std::collections::HashMap<&'static str, u32> {
    static MAP: std::sync::OnceLock<std::collections::HashMap<&'static str, u32>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = std::collections::HashMap::new();
        for (i, name) in [
            "january",
            "february",
            "march",
            "april",
            "may",
            "june",
            "july",
            "august",
            "september",
            "october",
            "november",
            "december",
        ]
        .iter()
        .enumerate()
        {
            m.insert(*name, (i + 1) as u32);
        }
        m
    })
}

fn currency_normalise_date(raw: &str) -> Option<String> {
    let s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    // "31 October 2025"
    let prose = Regex::new(r"^(\d{1,2})\s+([A-Za-z]+)\s+(\d{4})$").unwrap();
    if let Some(c) = prose.captures(&s) {
        let day: u32 = c.get(1)?.as_str().parse().ok()?;
        let month_name = c.get(2)?.as_str().to_lowercase();
        let year: u32 = c.get(3)?.as_str().parse().ok()?;
        let month = *currency_months().get(month_name.as_str())?;
        return Some(format!("{year:04}-{month:02}-{day:02}"));
    }
    // "31/10/2025"
    let dmy = Regex::new(r"^(\d{1,2})/(\d{1,2})/(\d{4})$").unwrap();
    if let Some(c) = dmy.captures(&s) {
        let day: u32 = c.get(1)?.as_str().parse().ok()?;
        let month: u32 = c.get(2)?.as_str().parse().ok()?;
        let year: u32 = c.get(3)?.as_str().parse().ok()?;
        return Some(format!("{year:04}-{month:02}-{day:02}"));
    }
    // "2025-10-31"
    let iso = Regex::new(r"^(\d{4})-(\d{2})-(\d{2})$").unwrap();
    if iso.is_match(&s) {
        return Some(s);
    }
    None
}

fn currency_normalise_citation(raw: &str) -> Option<String> {
    let s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

const CURRENCY_RULING_SERIES: &str =
    "SMSFRB|SMSFR|SMSFD|GSTR|GSTD|FBTR|WETR|WETD|LCR|SGR|FTR|PCG|LCG|PRR|CLR|COG|TXD|TPA|FBT|GII|CR|PR|TR|TD|MT|TA|LI|LG|WT|IT";

fn currency_citation_pattern() -> String {
    format!(
        r"(?:{}|ATO\s+ID|PS\s+LA|SMSFRB)\s+\d{{1,4}}/D?\d+[A-Z0-9]*",
        CURRENCY_RULING_SERIES
    )
}

fn currency_date_prose_pattern() -> &'static str {
    r"\d{1,2}\s+(?:January|February|March|April|May|June|July|August|September|October|November|December)\s+\d{4}|\d{1,2}/\d{1,2}/\d{4}|\d{4}-\d{2}-\d{2}"
}

fn currency_re_withdrawn_prose() -> Regex {
    let date = currency_date_prose_pattern();
    let prefix = r"\b(?:was|is|were|are|been|being|has\s+been|have\s+been)?\s*withdrawn(?:\s+(?:with\s+effect)?\s*(?:from|on|as\s+of))?\s+";
    Regex::new(&format!(r"(?i){prefix}(?P<date>{date})")).unwrap()
}

fn currency_re_withdrawn_by_prose() -> Regex {
    let date = currency_date_prose_pattern();
    let prefix = r"\b(?:was|is|were|are|been|being|has\s+been|have\s+been)?\s*withdrawn(?:\s+(?:with\s+effect)?\s*(?:from|on|as\s+of))?\s+";
    let cite = currency_citation_pattern();
    Regex::new(&format!(
        r"(?i){prefix}(?P<date>{date})\s+by\b(?:\s+(?:draft\s+)?(?:Taxation|Class|Product|Practical|GST)?\s*(?:Ruling|Determination|Guideline|Practice\s+Statement)?)?\s+(?P<cite>{cite})"
    )).unwrap()
}

fn currency_re_replacement_verb() -> Regex {
    Regex::new(r"(?i)\b(replaces|replaced\s+by|supersed(?:e|es|ed|ing)|in\s+lieu\s+of)\b").unwrap()
}

fn currency_re_self_anchor() -> Regex {
    Regex::new(r"(?i)\bthis\s+(?:Ruling|Determination|Guideline|Practice\s+Statement)\b").unwrap()
}

fn currency_re_sentence_split() -> Regex {
    Regex::new(r"[.;\n]+").unwrap()
}

fn currency_re_replaces_prose() -> Regex {
    let cite = currency_citation_pattern();
    Regex::new(&format!(
        r"(?i)\b(?:this\s+(?:Ruling|Determination|Guideline|Practice\s+Statement)\s+)?replaces\b(?:\s+(?:draft\s+)?(?:Taxation|Class|Product|Practical|GST)?\s*(?:Ruling|Determination|Guideline|Practice\s+Statement)?)?\s+(?P<cite>{cite})"
    )).unwrap()
}

fn currency_re_superseded_by_prose() -> Regex {
    let cite = currency_citation_pattern();
    Regex::new(&format!(
        r"(?i)\b(?:replaced|superseded)\s+by\b(?:\s+(?:draft\s+)?(?:Taxation|Class|Product|Practical|GST)?\s*(?:Ruling|Determination|Guideline|Practice\s+Statement)?)?\s+(?P<cite>{cite})"
    )).unwrap()
}

fn currency_withdrawal_fragment_is_self(fragment: &str, withdrawn_start: usize) -> bool {
    let rep = currency_re_replacement_verb();
    if !rep.is_match(fragment) {
        return true;
    }
    let anchor = currency_re_self_anchor();
    let Some(am) = anchor.find(fragment) else {
        return false;
    };
    let between_start = am.end();
    if between_start > withdrawn_start {
        return false;
    }
    let between = &fragment[between_start..withdrawn_start];
    !rep.is_match(between)
}

fn currency_extract_self_withdrawn_date(text: &str) -> Option<String> {
    let split = currency_re_sentence_split();
    let withdrawn = currency_re_withdrawn_prose();
    for fragment in split.split(text) {
        let Some(m) = withdrawn.captures(fragment) else {
            continue;
        };
        if !currency_withdrawal_fragment_is_self(fragment, m.get(0)?.start()) {
            continue;
        }
        let date = m.name("date")?.as_str();
        if let Some(iso) = currency_normalise_date(date) {
            return Some(iso);
        }
    }
    None
}

fn currency_extract_self_withdrawn_by(text: &str) -> Option<String> {
    let split = currency_re_sentence_split();
    let withdrawn_by = currency_re_withdrawn_by_prose();
    for fragment in split.split(text) {
        let Some(m) = withdrawn_by.captures(fragment) else {
            continue;
        };
        if !currency_withdrawal_fragment_is_self(fragment, m.get(0)?.start()) {
            continue;
        }
        let cite = m.name("cite")?.as_str();
        if let Some(c) = currency_normalise_citation(cite) {
            return Some(c);
        }
    }
    None
}

fn currency_alert_text(html: &str) -> String {
    let doc = scraper::Html::parse_document(html);
    let sel = scraper::Selector::parse("div.alert").unwrap();
    let parts: Vec<String> = doc
        .select(&sel)
        .map(|el| {
            let raw = el.text().collect::<String>();
            raw.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .filter(|s| !s.is_empty())
        .collect();
    parts.join(" \n ")
}

fn currency_body_text(html: &str) -> String {
    let doc = scraper::Html::parse_document(html);
    for sel_str in &["#LawBody", "#LawContent"] {
        let sel = scraper::Selector::parse(sel_str).unwrap();
        if let Some(el) = doc.select(&sel).next() {
            return anchors_node_text(el);
        }
    }
    if let Ok(body_sel) = scraper::Selector::parse("body") {
        if let Some(el) = doc.select(&body_sel).next() {
            return anchors_node_text(el);
        }
    }
    String::new()
}

fn currency_date_from_history_table(html: &str) -> Option<String> {
    let doc = scraper::Html::parse_document(html);
    let timeline_sel = scraper::Selector::parse("a[name='LawTimeLine']").unwrap();
    let timeline = doc.select(&timeline_sel).next()?;
    // Walk up to enclosing panel or table — at most 8 hops.
    let mut current = timeline.parent();
    let mut panel: Option<scraper::ElementRef> = None;
    for _ in 0..8 {
        let Some(p) = current else { break };
        if let Some(el) = scraper::ElementRef::wrap(p) {
            let tag = el.value().name();
            let classes: Vec<&str> = el
                .value()
                .attr("class")
                .unwrap_or("")
                .split_whitespace()
                .collect();
            if tag == "table" || classes.contains(&"panel") {
                panel = Some(el);
                break;
            }
        }
        current = p.parent();
    }
    let panel = panel?;
    let row_sel = scraper::Selector::parse("tr").unwrap();
    let cell_sel = scraper::Selector::parse("td").unwrap();
    let mut latest: Option<String> = None;
    for row in panel.select(&row_sel) {
        let cells: Vec<scraper::ElementRef> = row.select(&cell_sel).collect();
        if cells.len() < 2 {
            continue;
        }
        let mut date_cell: Option<String> = None;
        let mut label_cell: Option<String> = None;
        for cell in &cells {
            let cls = cell.value().attr("class").unwrap_or("").to_lowercase();
            let text = anchors_node_text(*cell);
            if cls.contains("date") && date_cell.is_none() {
                date_cell = Some(text);
            } else if date_cell.is_some() && label_cell.is_none() {
                label_cell = Some(text.to_lowercase());
            }
        }
        let Some(date) = date_cell else { continue };
        let label = label_cell.unwrap_or_else(|| {
            let last = cells.last().unwrap();
            anchors_node_text(*last).to_lowercase()
        });
        if !label.contains("withdraw") {
            continue;
        }
        if let Some(iso) = currency_normalise_date(&date) {
            latest = Some(iso);
        }
    }
    latest
}

fn currency_scan_text(text: &str) -> (Option<String>, Option<String>, Option<String>) {
    if text.is_empty() {
        return (None, None, None);
    }
    let withdrawn_date = currency_extract_self_withdrawn_date(text);
    let mut superseded_by = currency_extract_self_withdrawn_by(text);
    if superseded_by.is_none() {
        let sup = currency_re_superseded_by_prose();
        if let Some(m) = sup.captures(text) {
            if let Some(cite) = m.name("cite") {
                superseded_by = currency_normalise_citation(cite.as_str());
            }
        }
    }
    let mut replaces: Option<String> = None;
    let rep = currency_re_replaces_prose();
    if let Some(m) = rep.captures(text) {
        if let Some(cite) = m.name("cite") {
            replaces = currency_normalise_citation(cite.as_str());
        }
    }
    (withdrawn_date, superseded_by, replaces)
}

fn currency_has_withdrawn_title_suffix(html: &str) -> bool {
    let doc = scraper::Html::parse_document(html);
    let sel = scraper::Selector::parse("h1, h2, h3").unwrap();
    for el in doc.select(&sel) {
        let text = anchors_node_text(el).to_lowercase();
        if text.contains("(withdrawn)") {
            return true;
        }
    }
    false
}

fn extract_currency(html: &str) -> CurrencyInfo {
    if html.trim().is_empty() {
        return CurrencyInfo::default();
    }
    let alert_text = currency_alert_text(html);
    let body_text = currency_body_text(html);
    let (a_w, a_s, a_r) = currency_scan_text(&alert_text);
    let (p_w, p_s, p_r) = currency_scan_text(&body_text);

    let mut withdrawn_date = a_w;
    if withdrawn_date.is_none() && p_w.is_some() {
        withdrawn_date = p_w;
    }
    if withdrawn_date.is_none() {
        withdrawn_date = currency_date_from_history_table(html);
    }
    if withdrawn_date.is_none() && currency_has_withdrawn_title_suffix(html) {
        withdrawn_date = Some(CURRENCY_TITLE_SUFFIX_SENTINEL.to_string());
    }
    let mut superseded_by = a_s;
    if superseded_by.is_none() && p_s.is_some() {
        superseded_by = p_s;
    }
    let mut replaces = a_r;
    if replaces.is_none() && p_r.is_some() {
        replaces = p_r;
    }
    CurrencyInfo {
        withdrawn_date,
        superseded_by,
        replaces,
    }
}

// ----- Image asset extraction (port of extract.py:_rewrite_images_html) -----
//
// Walks <img> tags in cleaned HTML, reads referenced files (src resolved
// against source_path's parent), SHA256-hashes + base64-encodes them, emits
// ExtractedAsset records and rewrites the HTML so each <img> becomes a
// <span data-asset-ref="..." data-media-type="...">[image: alt]</span>.
// Mirrors src/ato_mcp/indexer/extract.py:_rewrite_images_html.

#[derive(Debug, Clone, Serialize)]
struct ExtractedAsset {
    asset_ref: String,
    source_path: String,
    relative_path: String,
    media_type: Option<String>,
    alt: Option<String>,
    title: Option<String>,
    sha256: String,
    size: u64,
    data_b64: String,
}

fn assets_url_encode_doc_id(doc_id: &str) -> String {
    let mut out = String::with_capacity(doc_id.len() * 3);
    for byte in doc_id.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn assets_asset_ref(doc_id: &str, ordinal: u32) -> String {
    format!(
        "ato-image://{}/{}",
        assets_url_encode_doc_id(doc_id),
        ordinal
    )
}

fn assets_guess_media_type(src: &str) -> Option<String> {
    let path = src.split('?').next().unwrap_or(src);
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())?;
    match ext.as_str() {
        "png" => Some("image/png".to_string()),
        "jpg" | "jpeg" => Some("image/jpeg".to_string()),
        "gif" => Some("image/gif".to_string()),
        "svg" => Some("image/svg+xml".to_string()),
        "webp" => Some("image/webp".to_string()),
        "bmp" => Some("image/bmp".to_string()),
        "ico" => Some("image/vnd.microsoft.icon".to_string()),
        _ => None,
    }
}

fn assets_extension_from_media_type(mt: &Option<String>) -> &'static str {
    match mt.as_deref() {
        Some("image/png") => ".png",
        Some("image/jpeg") => ".jpg",
        Some("image/gif") => ".gif",
        Some("image/svg+xml") => ".svg",
        Some("image/webp") => ".webp",
        Some("image/bmp") => ".bmp",
        Some("image/vnd.microsoft.icon") => ".ico",
        _ => ".bin",
    }
}

fn assets_relative_path(data: &[u8], src: &str, media_type: &Option<String>) -> (String, String) {
    let mut h = Sha256::new();
    h.update(data);
    let sha_full = h.finalize();
    let sha = sha_full
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let path = src.split('?').next().unwrap_or(src);
    let mut suffix = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| format!(".{}", s.to_lowercase()))
        .unwrap_or_default();
    if suffix.is_empty() || suffix.len() > 10 {
        suffix = assets_extension_from_media_type(media_type).to_string();
    }
    (format!("assets/{}/{}{}", &sha[..2], sha, suffix), sha)
}

fn assets_resolve_path(source_path: Option<&Path>, src: &str) -> Option<PathBuf> {
    let sp = source_path?;
    if src.is_empty() {
        return None;
    }
    // Skip URLs with scheme or absolute paths.
    if src.starts_with('/') || src.contains("://") {
        return None;
    }
    sp.parent().map(|p| p.join(src))
}

fn assets_text_norm(s: Option<&str>) -> Option<String> {
    let raw = s.unwrap_or("");
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

fn assets_html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

/// Walk HTML for <img> tags, extract assets from referenced files, and
/// produce (rewritten_html, assets) where every <img> becomes a <span>
/// carrying the asset_ref + alt-text marker.
fn rewrite_images_html(
    html: &str,
    doc_id: Option<&str>,
    source_path: Option<&Path>,
) -> (String, Vec<ExtractedAsset>) {
    use base64::Engine as _;
    let img_re = Regex::new(r#"(?is)<img\b([^>]*)>"#).unwrap();
    let mut assets: Vec<ExtractedAsset> = Vec::new();
    let mut image_ord: u32 = 0;
    let rewritten = img_re
        .replace_all(html, |caps: &regex::Captures| {
            let attrs_str = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let alt = assets_text_norm(extract_attr(attrs_str, "alt"));
            let title = assets_text_norm(extract_attr(attrs_str, "title"));
            let label = alt.clone().or_else(|| title.clone()).unwrap_or_default();
            if label.to_lowercase() == "exclamation" {
                return String::new();
            }
            let src = assets_text_norm(extract_attr(attrs_str, "src")).unwrap_or_default();
            let mut data: Option<Vec<u8>> = None;
            if let Some(p) = assets_resolve_path(source_path, &src) {
                if p.exists() {
                    if let Ok(bytes) = fs::read(&p) {
                        data = Some(bytes);
                    }
                }
            }
            let media_type = assets_guess_media_type(&src);
            let mut asset_ref: Option<String> = None;
            if let (Some(d), Some(did)) = (data.as_ref(), doc_id) {
                let r = assets_asset_ref(did, image_ord);
                let (relpath, sha) = assets_relative_path(d, &src, &media_type);
                assets.push(ExtractedAsset {
                    asset_ref: r.clone(),
                    source_path: src.clone(),
                    relative_path: relpath,
                    media_type: media_type.clone(),
                    alt: alt.clone(),
                    title: title.clone(),
                    sha256: sha,
                    size: d.len() as u64,
                    data_b64: base64::engine::general_purpose::STANDARD.encode(d),
                });
                asset_ref = Some(r);
                image_ord += 1;
            }
            if asset_ref.is_none() && label.is_empty() {
                return String::new();
            }
            let mut attrs: Vec<String> = Vec::new();
            if let Some(r) = &asset_ref {
                attrs.push(format!(r#"data-asset-ref="{}""#, assets_html_escape(r)));
            }
            if let Some(mt) = &media_type {
                if asset_ref.is_some() {
                    attrs.push(format!(r#"data-media-type="{}""#, assets_html_escape(mt)));
                }
            }
            let text = if !label.is_empty() {
                format!("[image: {label}]")
            } else {
                "[image]".to_string()
            };
            let attrs_joined = attrs.join(" ");
            let space = if attrs_joined.is_empty() { "" } else { " " };
            format!(
                "<span{space}{attrs}>{text}</span>",
                attrs = attrs_joined,
                text = assets_html_escape(&text)
            )
        })
        .into_owned();
    (rewritten, assets)
}

fn extract_attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    fn common_re(name: &str) -> Option<&'static Regex> {
        macro_rules! attr_re {
            ($cell:ident, $name:literal) => {{
                static $cell: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
                $cell.get_or_init(|| {
                    Regex::new(concat!(
                        r#"(?is)\b"#,
                        $name,
                        r#"\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]*))"#
                    ))
                    .unwrap()
                })
            }};
        }
        match name.to_ascii_lowercase().as_str() {
            "alt" => Some(attr_re!(ATTR_ALT_RE, "alt")),
            "href" => Some(attr_re!(ATTR_HREF_RE, "href")),
            "id" => Some(attr_re!(ATTR_ID_RE, "id")),
            "name" => Some(attr_re!(ATTR_NAME_RE, "name")),
            "src" => Some(attr_re!(ATTR_SRC_RE, "src")),
            "title" => Some(attr_re!(ATTR_TITLE_RE, "title")),
            _ => None,
        }
    }
    fn capture_attr<'a>(re: &Regex, attrs: &'a str) -> Option<&'a str> {
        let caps = re.captures(attrs)?;
        caps.get(1)
            .or_else(|| caps.get(2))
            .or_else(|| caps.get(3))
            .map(|m| m.as_str())
    }
    if let Some(re) = common_re(name) {
        return capture_attr(re, attrs);
    }
    // Match name="value" or name='value' or name=value (no quotes, up to whitespace).
    // Case-insensitive on the name.
    let pat = format!(
        r#"(?is)\b{}\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]*))"#,
        regex::escape(name)
    );
    let re = Regex::new(&pat).ok()?;
    capture_attr(&re, attrs)
}

/// Strip attributes matching the deny list from HTML. Mirrors
/// extract.py:_strip_attributes / _DROP_ATTRS / _DROP_PREFIXES. We strip via
/// regex over the source HTML rather than mutating a DOM tree because
/// scraper's API is read-only-ish; the regex is bounded by attribute syntax
/// (`name=("..."|'...'|bareword)`) so it doesn't span tag boundaries.
fn strip_attributes(html: &str) -> String {
    static STRIP_ATTR_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = STRIP_ATTR_RE.get_or_init(|| {
        Regex::new(
            r#"(?is)\s+(?:style|width|height|align|valign|bgcolor|name|data-icon|cite|on[a-zA-Z]+)\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]*)"#,
        )
        .unwrap()
    });
    re.replace_all(html, "").into_owned()
}

/// Copy `<a name="X">` to `<a id="X">` (only when no existing id) and drop
/// the bare `name=` attribute. Mirrors extract.py:_normalise_named_anchors.
/// Operates over the HTML string directly.
fn normalise_named_anchors(html: &str) -> String {
    static A_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static NAME_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let a_re = A_RE.get_or_init(|| Regex::new(r#"(?is)<a\b([^>]*)>"#).unwrap());
    let name_re = NAME_RE
        .get_or_init(|| Regex::new(r#"(?is)\s+name\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]*)"#).unwrap());
    a_re.replace_all(html, |caps: &regex::Captures| {
        let attrs = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let name = extract_attr(attrs, "name");
        let id = extract_attr(attrs, "id");
        // Build the new attribute string.
        let mut new_attrs = attrs.to_string();
        // Drop name=... regardless.
        new_attrs = name_re.replace_all(&new_attrs, "").into_owned();
        // If the source had a name and no id, append id="<name>".
        if let Some(n) = name {
            if id.is_none() {
                new_attrs.push_str(&format!(r#" id="{}""#, assets_html_escape(n)));
            }
        }
        format!("<a{new_attrs}>")
    })
    .into_owned()
}

// ----- Chunker (port of src/ato_mcp/indexer/chunk.py) -----
//
// Block-aware chunking for cleaned ATO HTML. Walks the DOM into a flat list
// of atomic blocks, renders each into plaintext with markdown markers, then
// greedy-packs blocks into chunks bounded by max_tokens. Mirrors chunk.py's
// public API (chunk_html, html_to_text, approx_tokens) and intermediate
// shape (_Block, Chunk).

#[allow(dead_code)]
// [IB-21] Checkpoints pin CHUNKER_FORMAT_VERSION; changing output shape
// forces an explicit fresh build instead of resuming stale chunk records.
const CHUNKER_FORMAT_VERSION: u32 = 3;
const EMBED_MAX_TOKENS: usize = 1024;

#[derive(Debug, Clone, Serialize)]
struct Chunk {
    ord: i64,
    anchor: Option<String>,
    text: String,
    definition_text: Option<String>,
}

#[derive(Debug, Clone)]
struct ChunkBlock {
    text: String,
    definition_text: String,
    anchor: Option<String>,
    is_oversize_table: bool,
    /// Set when the block is an oversize table — needed by chunker_split
    /// to walk rows in table-row-split mode.
    table_html: Option<String>,
}

fn chunker_approx_tokens(text: &str) -> usize {
    let words = text.split_whitespace().count();
    std::cmp::max(1, ((words as f64) * 1.3) as usize)
}

/// Tighter whitespace normalisation than `normalise_paragraph_breaks`:
/// matches chunk.py:_normalise_text. Collapses NBSP and horizontal-only
/// runs to single spaces, collapses ` *\n *` to `\n`, caps newline runs at
/// two, normalises numeric-range spacing, and tightens quoted text.
fn chunker_normalise_text(text: &str) -> String {
    static WS_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static NEWLINE_PAD_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static NEWLINE_RUN_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static NUMERIC_RANGE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static SPACED_QUOTE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let s = text.replace('\u{a0}', " ");
    // _WS_RE: horizontal whitespace [ \t\f\v]+
    let ws = WS_RE.get_or_init(|| Regex::new(r"[ \t\x0c\x0b]+").unwrap());
    let s = ws.replace_all(&s, " ").into_owned();
    let newline_pad = NEWLINE_PAD_RE.get_or_init(|| Regex::new(r" *\n *").unwrap());
    let s = newline_pad.replace_all(&s, "\n").into_owned();
    let newline_run = NEWLINE_RUN_RE.get_or_init(|| Regex::new(r"\n{3,}").unwrap());
    let s = newline_run.replace_all(&s, "\n\n").into_owned();
    let s = s.trim().to_string();
    let numeric_range =
        NUMERIC_RANGE_RE.get_or_init(|| Regex::new(r"(?P<a>\d)\s+-\s+(?P<b>\d)").unwrap());
    let s = numeric_range.replace_all(&s, "$a-$b").into_owned();
    let spaced_quote = SPACED_QUOTE_RE.get_or_init(|| Regex::new(r#""\s+([^"\n]*?)\s+""#).unwrap());
    spaced_quote.replace_all(&s, r#""$1""#).into_owned()
}

fn chunker_heading_anchor(node: scraper::ElementRef) -> Option<String> {
    if let Some(id) = node.value().attr("id") {
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    let a_sel = scraper::Selector::parse("a").unwrap();
    for a in node.select(&a_sel) {
        let val = a.value();
        if let Some(name) = val.attr("id").or_else(|| val.attr("name")) {
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn chunker_first_referenced_anchor(
    node: scraper::ElementRef,
    referenced: &std::collections::HashSet<String>,
) -> Option<String> {
    for el in node.descendants() {
        if let Some(eref) = scraper::ElementRef::wrap(el) {
            let val = eref.value();
            if let Some(name) = val.attr("name") {
                if referenced.contains(name) {
                    return Some(name.to_string());
                }
            }
            if let Some(nid) = val.attr("id") {
                if referenced.contains(nid) {
                    return Some(nid.to_string());
                }
            }
        }
    }
    None
}

fn chunker_is_root_title_echo(heading: &str, root_title: Option<&str>) -> bool {
    let Some(rt) = root_title else { return false };
    chunker_normalise_text(heading).to_lowercase() == chunker_normalise_text(rt).to_lowercase()
}

/// Render a single subtree to inline text using the existing render_node
/// machinery (which already produces [doc:X], [anchor:X], [asset:X],
/// **/*/# markers). Used by the chunker for block rendering.
fn chunker_render_inline(
    node: scraper::ElementRef,
    referenced: &std::collections::HashSet<String>,
) -> String {
    let mut buf = String::new();
    for child in node.children() {
        render_node(child, &mut buf, referenced);
    }
    buf
}

fn chunker_is_atomic_block(tag: &str, has_structural_child: bool) -> bool {
    const PURE_ATOMIC: &[&str] = &[
        "table",
        "p",
        "pre",
        "blockquote",
        "li",
        "figcaption",
        "caption",
        "dt",
        "dd",
    ];
    const CONTAINER_BLOCKS: &[&str] = &[
        "article", "aside", "details", "div", "dl", "figure", "footer", "header", "main", "ol",
        "section", "ul",
    ];
    const BLOCK_TAGS: &[&str] = &[
        "address",
        "article",
        "aside",
        "blockquote",
        "caption",
        "dd",
        "details",
        "div",
        "dl",
        "dt",
        "figcaption",
        "figure",
        "footer",
        "header",
        "li",
        "main",
        "ol",
        "p",
        "pre",
        "section",
        "table",
        "td",
        "th",
        "tr",
        "ul",
    ];
    if PURE_ATOMIC.contains(&tag) {
        return true;
    }
    if !BLOCK_TAGS.contains(&tag) {
        return false;
    }
    if CONTAINER_BLOCKS.contains(&tag) {
        return !has_structural_child;
    }
    true
}

fn chunker_child_is_structural(tag: &str) -> bool {
    const HEADING_TAGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];
    const BLOCK_TAGS: &[&str] = &[
        "address",
        "article",
        "aside",
        "blockquote",
        "caption",
        "dd",
        "details",
        "div",
        "dl",
        "dt",
        "figcaption",
        "figure",
        "footer",
        "header",
        "li",
        "main",
        "ol",
        "p",
        "pre",
        "section",
        "table",
        "td",
        "th",
        "tr",
        "ul",
    ];
    HEADING_TAGS.contains(&tag) || BLOCK_TAGS.contains(&tag)
}

fn chunker_has_structural_child(node: scraper::ElementRef) -> bool {
    for child in node.children() {
        if let Some(eref) = scraper::ElementRef::wrap(child) {
            if chunker_child_is_structural(eref.value().name()) {
                return true;
            }
        }
    }
    false
}

fn chunker_render_table_text(
    table: scraper::ElementRef,
    referenced: &std::collections::HashSet<String>,
) -> String {
    let row_sel = scraper::Selector::parse("tr").unwrap();
    let cell_sel = scraper::Selector::parse("th, td").unwrap();
    let mut rows: Vec<String> = Vec::new();
    for row in table.select(&row_sel) {
        let cells: Vec<String> = row
            .select(&cell_sel)
            .map(|cell| chunker_normalise_text(&chunker_render_inline(cell, referenced)))
            .filter(|c| !c.is_empty())
            .collect();
        if !cells.is_empty() {
            rows.push(cells.join(" | "));
        }
    }
    if !rows.is_empty() {
        rows.join("\n")
    } else {
        chunker_normalise_text(&chunker_render_inline(table, referenced))
    }
}

fn chunker_render_block(
    node: scraper::ElementRef,
    referenced: &std::collections::HashSet<String>,
) -> Option<ChunkBlock> {
    let tag = node.value().name();
    let text = match tag {
        "table" => chunker_render_table_text(node, referenced),
        "blockquote" => {
            let inner = chunker_normalise_text(&chunker_render_inline(node, referenced));
            if inner.is_empty() {
                String::new()
            } else {
                inner
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| format!("> {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        "pre" => {
            // Use raw text() rather than rendered (no markers inside <pre>).
            let inner = node.text().collect::<String>();
            let inner = inner.trim();
            if inner.is_empty() {
                String::new()
            } else {
                format!("```\n{inner}\n```")
            }
        }
        "li" => {
            let inner = chunker_normalise_text(&chunker_render_inline(node, referenced));
            if inner.is_empty() {
                String::new()
            } else {
                format!("- {inner}")
            }
        }
        "ul" | "ol" => {
            let li_sel = scraper::Selector::parse("li").unwrap();
            let items: Vec<String> = node
                .select(&li_sel)
                .map(|li| {
                    let t = chunker_normalise_text(&chunker_render_inline(li, referenced));
                    if t.is_empty() {
                        String::new()
                    } else {
                        format!("- {t}")
                    }
                })
                .filter(|s| !s.is_empty())
                .collect();
            items.join("\n")
        }
        _ => chunker_normalise_text(&chunker_render_inline(node, referenced)),
    };
    if text.is_empty() {
        return None;
    }
    let anchor = chunker_first_referenced_anchor(node, referenced);
    let is_oversize_table = tag == "table" && chunker_approx_tokens(&text) > EMBED_MAX_TOKENS;
    let table_html = if is_oversize_table {
        Some(node.html())
    } else {
        None
    };
    Some(ChunkBlock {
        text: text.clone(),
        definition_text: text,
        anchor,
        is_oversize_table,
        table_html,
    })
}

fn chunker_render_dt_dd_pair(
    dt: scraper::ElementRef,
    dd: Option<scraper::ElementRef>,
    referenced: &std::collections::HashSet<String>,
) -> Option<ChunkBlock> {
    let term = chunker_normalise_text(&chunker_render_inline(dt, referenced));
    let body = match dd {
        Some(d) => chunker_normalise_text(&chunker_render_inline(d, referenced)),
        None => String::new(),
    };
    if term.is_empty() && body.is_empty() {
        return None;
    }
    let mut rendered = if term.is_empty() {
        String::new()
    } else {
        format!("**{term}**")
    };
    if !body.is_empty() {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        rendered.push_str(&body);
    }
    let mut anchor = chunker_first_referenced_anchor(dt, referenced);
    if anchor.is_none() {
        if let Some(d) = dd {
            anchor = chunker_first_referenced_anchor(d, referenced);
        }
    }
    Some(ChunkBlock {
        text: rendered.clone(),
        definition_text: rendered,
        anchor,
        is_oversize_table: false,
        table_html: None,
    })
}

/// Walk children of `parent` and emit ChunkBlocks. Mirrors chunk.py:_walk.
fn chunker_walk(
    parent: scraper::ElementRef,
    blocks: &mut Vec<ChunkBlock>,
    referenced: &std::collections::HashSet<String>,
    root_title: Option<&str>,
) {
    const HEADING_TAGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];
    let mut inline_parts: Vec<String> = Vec::new();
    let mut inline_anchors: Vec<String> = Vec::new();

    let children: Vec<_> = parent.children().collect();
    let mut idx = 0;
    while idx < children.len() {
        let child = children[idx];
        let Some(eref) = scraper::ElementRef::wrap(child) else {
            // Text node — accumulate to inline buffer using render_node.
            let mut tmp = String::new();
            render_node(child, &mut tmp, referenced);
            if !tmp.is_empty() {
                inline_parts.push(tmp);
            }
            idx += 1;
            continue;
        };
        let tag = eref.value().name();

        // dt/dd pair: combine adjacent dt + dd.
        if tag == "dt" {
            chunker_flush_inline(&mut inline_parts, &mut inline_anchors, blocks);
            let dd = children
                .get(idx + 1)
                .and_then(|n| scraper::ElementRef::wrap(*n))
                .filter(|e| e.value().name() == "dd");
            if let Some(block) = chunker_render_dt_dd_pair(eref, dd, referenced) {
                blocks.push(block);
            }
            idx += if dd.is_some() { 2 } else { 1 };
            continue;
        }
        // Headings render as their own block with markdown level marker.
        if HEADING_TAGS.contains(&tag) {
            chunker_flush_inline(&mut inline_parts, &mut inline_anchors, blocks);
            let inner = chunker_render_inline(eref, referenced);
            let heading_text = chunker_normalise_text(&inner);
            if !heading_text.is_empty() && !chunker_is_root_title_echo(&heading_text, root_title) {
                let level: usize = tag[1..].parse().unwrap_or(1).clamp(1, 6);
                let rendered = format!("{} {}", "#".repeat(level), heading_text);
                let anchor = chunker_heading_anchor(eref);
                blocks.push(ChunkBlock {
                    text: rendered.clone(),
                    definition_text: rendered,
                    anchor,
                    is_oversize_table: false,
                    table_html: None,
                });
            }
            idx += 1;
            continue;
        }
        let has_struct = chunker_has_structural_child(eref);
        if chunker_is_atomic_block(tag, has_struct) {
            chunker_flush_inline(&mut inline_parts, &mut inline_anchors, blocks);
            if let Some(block) = chunker_render_block(eref, referenced) {
                blocks.push(block);
            }
            idx += 1;
            continue;
        }
        if has_struct {
            chunker_flush_inline(&mut inline_parts, &mut inline_anchors, blocks);
            chunker_walk(eref, blocks, referenced, root_title);
            idx += 1;
            continue;
        }
        // Pure inline element — accumulate.
        let rendered = chunker_render_inline(eref, referenced);
        if !rendered.is_empty() {
            inline_parts.push(rendered);
        }
        if let Some(a) = chunker_first_referenced_anchor(eref, referenced) {
            inline_anchors.push(a);
        }
        idx += 1;
    }
    chunker_flush_inline(&mut inline_parts, &mut inline_anchors, blocks);
}

fn chunker_flush_inline(
    inline_parts: &mut Vec<String>,
    inline_anchors: &mut Vec<String>,
    blocks: &mut Vec<ChunkBlock>,
) {
    let joined = inline_parts.join("");
    let text = chunker_normalise_text(&joined);
    if !text.is_empty() {
        let anchor = inline_anchors.first().cloned();
        blocks.push(ChunkBlock {
            text: text.clone(),
            definition_text: text,
            anchor,
            is_oversize_table: false,
            table_html: None,
        });
    }
    inline_parts.clear();
    inline_anchors.clear();
}

/// Split an oversize block into pieces that each fit within max_tokens.
/// Mirrors chunk.py:_split_oversize_block. Order:
///   1. oversize tables -> row split (rows stay whole).
///   2. prose -> sentence split, greedy-pack within budget.
///   3. word-window split as last-resort (single sentence/row over budget).
fn chunker_split_oversize_block(block: &ChunkBlock, max_tokens: usize) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if block.is_oversize_table {
        if let Some(html) = block.table_html.as_deref() {
            for (piece, defn) in chunker_table_row_split(html, max_tokens) {
                for p in chunker_enforce_max_tokens(&piece, &defn, max_tokens) {
                    out.push(p);
                }
            }
            return out;
        }
    }
    // Prose: sentence-split, greedy-pack.
    let sentences = chunker_sentence_split(&block.text);
    let mut buf: Vec<String> = Vec::new();
    let mut buf_tokens: usize = 0;
    for s in sentences {
        let st = chunker_approx_tokens(&s);
        if !buf.is_empty() && buf_tokens + st > max_tokens {
            let piece = buf.join(" ");
            for p in chunker_enforce_max_tokens(&piece, &piece, max_tokens) {
                out.push(p);
            }
            buf = vec![s];
            buf_tokens = st;
        } else {
            buf.push(s);
            buf_tokens += st;
        }
    }
    if !buf.is_empty() {
        let piece = buf.join(" ");
        for p in chunker_enforce_max_tokens(&piece, &piece, max_tokens) {
            out.push(p);
        }
    }
    out
}

fn chunker_enforce_max_tokens(
    text: &str,
    definition_text: &str,
    max_tokens: usize,
) -> Vec<(String, String)> {
    if chunker_approx_tokens(text) <= max_tokens {
        return vec![(text.to_string(), definition_text.to_string())];
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let target_words = std::cmp::max(1, ((max_tokens as f64) / 1.4) as usize);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let end = std::cmp::min(i + target_words, words.len());
        let piece = words[i..end].join(" ");
        out.push((piece.clone(), piece));
        i = end;
    }
    out
}

fn chunker_table_row_split(table_html: &str, max_tokens: usize) -> Vec<(String, String)> {
    let frag = scraper::Html::parse_fragment(table_html);
    let row_sel = scraper::Selector::parse("tr").unwrap();
    let cell_sel = scraper::Selector::parse("th, td").unwrap();
    let referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rows: Vec<String> = Vec::new();
    for row in frag.select(&row_sel) {
        let cells: Vec<String> = row
            .select(&cell_sel)
            .map(|c| chunker_normalise_text(&chunker_render_inline(c, &referenced)))
            .filter(|s| !s.is_empty())
            .collect();
        if !cells.is_empty() {
            rows.push(cells.join(" | "));
        }
    }
    let mut out: Vec<(String, String)> = Vec::new();
    let mut buf: Vec<String> = Vec::new();
    let mut buf_tokens: usize = 0;
    for row in rows {
        let row_tokens = chunker_approx_tokens(&row);
        if !buf.is_empty() && buf_tokens + row_tokens > max_tokens {
            let piece = buf.join("\n");
            out.push((piece.clone(), piece));
            buf = vec![row];
            buf_tokens = row_tokens;
        } else {
            buf.push(row);
            buf_tokens += row_tokens;
        }
    }
    if !buf.is_empty() {
        let piece = buf.join("\n");
        out.push((piece.clone(), piece));
    }
    out
}

fn chunker_sentence_split(text: &str) -> Vec<String> {
    // Mirrors Python's _SENT_RE: split on whitespace that follows `.!?` and
    // precedes an uppercase letter or `(`. Rust's regex crate doesn't
    // support lookahead, so walk char-by-char.
    let mut sentences: Vec<String> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        current.push(c);
        if matches!(c, '.' | '!' | '?') {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j > i + 1 && j < chars.len() && (chars[j].is_ascii_uppercase() || chars[j] == '(') {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    sentences.push(trimmed);
                }
                current.clear();
                i = j;
                continue;
            }
        }
        i += 1;
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        sentences.push(trimmed);
    }
    sentences
}

/// Greedy-pack blocks into chunks bounded by max_tokens. Mirrors
/// chunk.py:_pack_chunks. Blocks exceeding max_tokens are split via
/// chunker_split_oversize_block (table rows, sentences, or word-window
/// fallback) so every emitted chunk fits the budget.
fn chunker_pack(blocks: Vec<ChunkBlock>, max_tokens: usize) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut ord_counter: i64 = 0;
    let mut current_text: Vec<String> = Vec::new();
    let mut current_def: Vec<String> = Vec::new();
    let mut current_words: usize = 0;
    let mut current_anchor: Option<String> = None;

    let flush = |current_text: &mut Vec<String>,
                 current_def: &mut Vec<String>,
                 current_words: &mut usize,
                 current_anchor: &mut Option<String>,
                 ord_counter: &mut i64,
                 chunks: &mut Vec<Chunk>| {
        if current_text.is_empty() {
            return;
        }
        let text = current_text.join("\n\n").trim().to_string();
        let defn = current_def.join("\n\n").trim().to_string();
        chunks.push(Chunk {
            ord: *ord_counter,
            anchor: current_anchor.take(),
            text: text.clone(),
            definition_text: if defn != text && !defn.is_empty() {
                Some(defn)
            } else {
                None
            },
        });
        *ord_counter += 1;
        current_text.clear();
        current_def.clear();
        *current_words = 0;
    };

    for block in blocks {
        let block_words = block.text.split_whitespace().count();
        let block_tokens = std::cmp::max(1, ((block_words as f64) * 1.3) as usize);
        if block_tokens > max_tokens {
            flush(
                &mut current_text,
                &mut current_def,
                &mut current_words,
                &mut current_anchor,
                &mut ord_counter,
                &mut chunks,
            );
            // Split oversize block into pieces that fit max_tokens.
            for (text, defn) in chunker_split_oversize_block(&block, max_tokens) {
                chunks.push(Chunk {
                    ord: ord_counter,
                    anchor: block.anchor.clone(),
                    text: text.clone(),
                    definition_text: if defn != text { Some(defn) } else { None },
                });
                ord_counter += 1;
            }
            continue;
        }
        // [IB-22] Project token count from accumulated raw words, not summed
        // per-block integer token estimates, so truncation drift cannot build up.
        let projected_tokens =
            std::cmp::max(1, (((current_words + block_words) as f64) * 1.3) as usize);
        if projected_tokens > max_tokens && !current_text.is_empty() {
            flush(
                &mut current_text,
                &mut current_def,
                &mut current_words,
                &mut current_anchor,
                &mut ord_counter,
                &mut chunks,
            );
        }
        current_text.push(block.text.clone());
        current_def.push(block.definition_text);
        current_words += block_words;
        if current_anchor.is_none() && block.anchor.is_some() {
            current_anchor = block.anchor;
        }
    }
    flush(
        &mut current_text,
        &mut current_def,
        &mut current_words,
        &mut current_anchor,
        &mut ord_counter,
        &mut chunks,
    );
    chunks
}

fn chunk_html(html: &str, root_title: Option<&str>, max_tokens: usize) -> Vec<Chunk> {
    if html.trim().is_empty() {
        return Vec::new();
    }
    let doc = scraper::Html::parse_fragment(html);
    let referenced = collect_referenced_anchors(&doc);
    let root = doc.root_element();
    let mut blocks: Vec<ChunkBlock> = Vec::new();
    // Find the first <body> or fall back to root. parse_fragment wraps
    // content in <html><body>, but we want to walk just the body's children.
    let body_sel = scraper::Selector::parse("body").unwrap();
    let walk_root = doc.select(&body_sel).next().unwrap_or(root);
    chunker_walk(walk_root, &mut blocks, &referenced, root_title);
    chunker_pack(blocks, max_tokens)
}

/// Mirror of chunk.py:html_to_text — walks the doc into _Blocks (with empty
/// referenced anchors) and joins block texts with `\n\n`. Used by the
/// extract CLI's `text` field so the output matches Python's behaviour
/// (table rows as pipe-separated, blockquote `> `, list items `- `, etc.)
/// rather than the looser per-block-tag newlines that subtree_text emits.
fn chunker_html_to_text(html: &str) -> String {
    if html.trim().is_empty() {
        return String::new();
    }
    let doc = scraper::Html::parse_fragment(html);
    let referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    let body_sel = scraper::Selector::parse("body").unwrap();
    let walk_root = doc.select(&body_sel).next().unwrap_or(doc.root_element());
    let mut blocks: Vec<ChunkBlock> = Vec::new();
    chunker_walk(walk_root, &mut blocks, &referenced, None);
    blocks
        .into_iter()
        .filter(|b| !b.text.is_empty())
        .map(|b| b.text)
        .collect::<Vec<_>>()
        .join("\n\n")
}

// ----- end chunker -----

// ----- Metadata helpers (port of src/ato_mcp/indexer/metadata.py) -----

#[allow(dead_code)]
const METADATA_OTHER_CATEGORY: &str = "Other_ATO_documents";
#[allow(dead_code)]
const METADATA_PACK_FORMAT_VERSION: u32 = 2;

fn metadata_extract_docid_path(canonical_id: &str) -> Option<String> {
    let parsed = url::Url::parse(canonical_id)
        .ok()
        .or_else(|| url::Url::parse(&format!("https://placeholder/{canonical_id}")).ok())?;
    for (k, v) in parsed.query_pairs() {
        if k.eq_ignore_ascii_case("docid") {
            let s = v.into_owned();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

// [IB-18] doc_id preserves the ATO docid query path verbatim; malformed or
// missing URLs fall back to the canonical_id so every source row has a key.
fn metadata_doc_id_for(canonical_id: &str) -> String {
    metadata_extract_docid_path(canonical_id).unwrap_or_else(|| canonical_id.to_string())
}

fn metadata_parse_docid(canonical_id: &str) -> Option<String> {
    let docid = metadata_extract_docid_path(canonical_id)?;
    docid
        .split('/')
        .find(|s| !s.is_empty())
        .map(|s| s.to_uppercase())
}

fn metadata_year_for_docid(canonical_id: &str) -> Option<String> {
    let docid = metadata_extract_docid_path(canonical_id)?;
    let year_re = Regex::new(r"(?:19|20)\d{2}").unwrap();
    let segments: Vec<&str> = docid.split('/').filter(|s| !s.is_empty()).collect();
    for seg in segments.iter().take(2) {
        if let Some(m) = year_re.find(seg) {
            return Some(m.as_str().to_string());
        }
    }
    None
}

#[allow(dead_code)]
fn metadata_extract_pub_date(text: &str) -> Option<String> {
    let date_re = Regex::new(
        r"(?i)\b(\d{1,2})\s+(January|February|March|April|May|June|July|August|September|October|November|December)\s+(\d{4})\b",
    )
    .unwrap();
    let head = text.chars().take(2000).collect::<String>();
    let m = date_re.captures(&head)?;
    let day: u32 = m.get(1)?.as_str().parse().ok()?;
    let month_name = m.get(2)?.as_str().to_lowercase();
    let year: u32 = m.get(3)?.as_str().parse().ok()?;
    let month = currency_months().get(month_name.as_str()).copied()?;
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn metadata_human_code_for_doc_id(doc_id: &str) -> Option<String> {
    let segments: Vec<&str> = doc_id.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return None;
    }
    let body = segments[1];
    // Year-series codes, longest-first to avoid prefix collisions.
    let year_series: Vec<&str> = vec![
        "SMSFRB", "SMSFR", "SMSFD", "GSTR", "GSTD", "FBTR", "WETR", "WETD", "LCR", "SGR", "FTR",
        "PCG", "LCG", "PRR", "CLR", "COG", "TXD", "TPA", "FBT", "CR", "PR", "TR", "TD", "MT", "TA",
        "LI", "LG", "WT",
    ];
    let alt = year_series.join("|");
    // Modern 4-digit year form: TR20243 -> TR 2024/3
    let re_y4 = Regex::new(&format!(r"^({alt})(\d{{4}})(D?)(\d+)$")).unwrap();
    if let Some(c) = re_y4.captures(body) {
        let series = c.get(1)?.as_str();
        let year = c.get(2)?.as_str();
        let draft = c.get(3)?.as_str();
        let number = c.get(4)?.as_str();
        return Some(format!("{series} {year}/{draft}{number}"));
    }
    // PS LA final
    let re_psla = Regex::new(r"^PSLA(\d{4})(\d+)$").unwrap();
    if let Some(c) = re_psla.captures(body) {
        return Some(format!(
            "PS LA {}/{}",
            c.get(1)?.as_str(),
            c.get(2)?.as_str()
        ));
    }
    // PS LA draft
    let re_psla_d = Regex::new(r"^PSD(\d{4})D?(\d+)$").unwrap();
    if let Some(c) = re_psla_d.captures(body) {
        return Some(format!(
            "PS LA {}/D{}",
            c.get(1)?.as_str(),
            c.get(2)?.as_str()
        ));
    }
    // ATO ID
    let re_atoid = Regex::new(r"^(?:ATOID|AID)(\d{4})(\d+)$").unwrap();
    if let Some(c) = re_atoid.captures(body) {
        return Some(format!(
            "ATO ID {}/{}",
            c.get(1)?.as_str(),
            c.get(2)?.as_str()
        ));
    }
    // Legacy 2-digit-year form: TR9725 -> TR 97/25 (year starts with 8 or 9)
    let re_y2 = Regex::new(&format!(r"^({alt})([89]\d)(\d+)$")).unwrap();
    if let Some(c) = re_y2.captures(body) {
        return Some(format!(
            "{} {}/{}",
            c.get(1)?.as_str(),
            c.get(2)?.as_str(),
            c.get(3)?.as_str()
        ));
    }
    None
}

#[allow(dead_code)]
fn metadata_content_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let digest = h.finalize();
    let hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

#[allow(dead_code)]
const METADATA_SIG_KEYS: &[&str] = &[
    "title",
    "type",
    "date",
    "withdrawn_date",
    "superseded_by",
    "replaces",
    "pack_format_version",
];

#[allow(dead_code)]
fn metadata_signature(fields: &serde_json::Map<String, JsonValue>) -> String {
    let mut h = Sha256::new();
    for key in METADATA_SIG_KEYS {
        let value = fields.get(*key);
        h.update(b"\0");
        h.update(key.as_bytes());
        h.update(b"=");
        if let Some(v) = value {
            if !v.is_null() {
                let s = match v {
                    JsonValue::String(s) => s.clone(),
                    other => other.to_string(),
                };
                h.update(s.as_bytes());
            }
        }
    }
    let digest = h.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Rewrite ATO doc-link `<a href="...">` tags to `<a data-doc-id="X" data-pit="..." data-view="...">`
/// (and drop the original href). Mirrors src/ato_mcp/indexer/extract.py:_rewrite_links_html.
/// Operates string-side over the cleaned HTML.
fn rewrite_links_html(html: &str) -> String {
    let a_re = Regex::new(r#"(?is)<a\b([^>]*)>"#).unwrap();
    a_re.replace_all(html, |caps: &regex::Captures| {
        let attrs = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let Some(href) = extract_attr(attrs, "href") else {
            return caps.get(0).unwrap().as_str().to_string();
        };
        // Try ATO doc-link parse.
        if let Some((doc_id, pit, view)) = doc_id_from_ato_link(href) {
            let href_re = Regex::new(r#"(?is)\s+href\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]*)"#).unwrap();
            let stripped = href_re.replace_all(attrs, "").into_owned();
            let mut new_attrs = stripped;
            new_attrs.push_str(&format!(
                r#" data-doc-id="{}""#,
                assets_html_escape(&doc_id)
            ));
            if let Some(p) = pit {
                new_attrs.push_str(&format!(r#" data-pit="{}""#, assets_html_escape(&p)));
            }
            if let Some(v) = view {
                new_attrs.push_str(&format!(r#" data-view="{}""#, assets_html_escape(&v)));
            }
            return format!("<a{new_attrs}>");
        }
        // Non-ATO link: keep href cleaned (strip javascript:/data:).
        let safe = href.trim();
        if safe.is_empty()
            || Regex::new(r#"(?is)^\s*(?:javascript|data):"#)
                .unwrap()
                .is_match(safe)
        {
            let href_re = Regex::new(r#"(?is)\s+href\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]*)"#).unwrap();
            let stripped = href_re.replace_all(attrs, "").into_owned();
            return format!("<a{stripped}>");
        }
        caps.get(0).unwrap().as_str().to_string()
    })
    .into_owned()
}

// ----- Pack file writer (port of src/ato_mcp/indexer/pack.py) -----
//
// [IB-09] A pack is a single .bin.zst blob: many records back-to-back, each
//   length:uint32 (LE) | zstd(orjson(record))
// Trailer: index_blob (zstd(json([{doc_id, offset, length}, ...]))) followed by
//   MAGIC(6) | count:u32 | index_offset:u64 | index_len:u32
// Mirrors pack.py:PackWriter.

const PACK_TRAILER_MAGIC: &[u8; 6] = b"ATOPK\x01";
const PACK_RECORD_HEADER_LEN: usize = 4;

#[derive(Debug, Clone, Serialize)]
struct PackedDocRef {
    doc_id: String,
    offset: u64,
    length: u64,
}

/// Write a pack file from a stream of (doc_id, record_json) pairs read from
/// stdin as JSONL. Each line: {"doc_id": str, "record": {...}}. Outputs JSON
/// {pack_path, sha8, sha256, size, refs: [PackedDocRef, ...]}.
fn write_pack(
    out_path: &Path,
    level: i32,
    records: impl Iterator<Item = Result<(String, serde_json::Value)>>,
) -> Result<JsonValue> {
    use std::io::Write as _;
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file =
        File::create(out_path).with_context(|| format!("creating {}", out_path.display()))?;
    let mut hasher = Sha256::new();
    let mut offset: u64 = 0;
    let mut refs: Vec<PackedDocRef> = Vec::new();

    for r in records {
        let (doc_id, record) = r?;
        let payload =
            zstd::stream::encode_all(std::io::Cursor::new(serde_json::to_vec(&record)?), level)?;
        let header = (payload.len() as u32).to_le_bytes();
        file.write_all(&header)?;
        file.write_all(&payload)?;
        hasher.update(header);
        hasher.update(&payload);
        let length = (PACK_RECORD_HEADER_LEN + payload.len()) as u64;
        let start = offset;
        offset += length;
        refs.push(PackedDocRef {
            doc_id,
            offset: start,
            length,
        });
    }

    // Trailer.
    let index_offset = offset;
    let index_json = serde_json::to_vec(&refs)?;
    let index_blob = zstd::stream::encode_all(std::io::Cursor::new(index_json), level)?;
    file.write_all(&index_blob)?;
    let mut trailer = Vec::with_capacity(6 + 4 + 8 + 4);
    trailer.extend_from_slice(PACK_TRAILER_MAGIC);
    trailer.extend_from_slice(&(refs.len() as u32).to_le_bytes());
    trailer.extend_from_slice(&index_offset.to_le_bytes());
    trailer.extend_from_slice(&(index_blob.len() as u32).to_le_bytes());
    file.write_all(&trailer)?;
    hasher.update(&index_blob);
    hasher.update(&trailer);
    file.flush()?;

    let digest = hasher.finalize();
    let sha256_hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let sha8 = sha256_hex[..8].to_string();
    let size = fs::metadata(out_path)?.len();
    Ok(json!({
        "pack_path": out_path.display().to_string(),
        "sha8": sha8,
        "sha256": sha256_hex,
        "size": size,
        "refs": refs,
    }))
}

// =====================================================================
// Rules engine — template-based metadata classifier
//
// Port of src/ato_mcp/indexer/rules.py (deleted in v0.8.0). Classifies
// each doc into one of ~10 structural templates (Taxation Ruling, Court
// Case, Act, EM, ...) and runs a positional extractor for each. Output
// is a (title, date) pair the build pipeline writes into the documents
// row.
// =====================================================================

#[derive(Debug, Clone, Default)]
pub struct RuleInputs {
    pub doc_id: String,
    pub title: Option<String>,
    pub headings: Vec<String>,
    pub heading_levels: Vec<u32>,
    pub body_head: String,
    pub category: Option<String>,
    pub pub_date: Option<String>,
    pub front_matter_refs: Vec<String>,
    pub front_matter_phrase: Option<String>,
}

impl RuleInputs {
    fn outer_prefix(&self) -> String {
        self.doc_id
            .split('/')
            .find(|s| !s.is_empty())
            .map(|s| s.to_uppercase())
            .unwrap_or_default()
    }

    fn inner_body(&self) -> String {
        let segs: Vec<&str> = self.doc_id.split('/').filter(|s| !s.is_empty()).collect();
        if segs.len() >= 2 {
            segs[1].to_string()
        } else {
            String::new()
        }
    }

    fn h1(&self) -> String {
        self.headings
            .first()
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    fn h2(&self) -> String {
        self.headings
            .get(1)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DerivedMetadata {
    pub title: Option<String>,
    pub date: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Shape {
    Empty,
    RulingTypePhrase,
    GuidelineTypePhrase,
    AlertPhrase,
    AtoidPhrase,
    PslaPhrase,
    SmsfrbPhrase,
    DisPhrase,
    EmPhrase,
    RulingCitation,
    RulingUnslashed,
    Atoid,
    Psla,
    Smsfrb,
    NeutralCitation,
    NameVName,
    ReX,
    CaseNumber,
    ActTitle,
    BillTitle,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Template {
    OfficialPub,
    CaseH1,
    CaseH2,
    HistCase,
    Dis,
    Act,
    LegislationSection,
    BillEm,
    Smsfrb,
    Epa,
    Other,
}

const RULING_SERIES_LIST: &[&str] = &[
    // Sorted by length desc so longer prefixes match first in the alternation.
    "SMSFRB", "SMSFR", "SMSFD", "GSTR", "GSTD", "FBTR", "WETR", "WETD", "LCR", "SGR", "FTR", "PCG",
    "LCG", "PRR", "CLR", "COG", "TXD", "TPA", "FBT", "GII", "CR", "PR", "TR", "TD", "MT", "TA",
    "LI", "LG", "WT", "IT",
];

fn ruling_series_alt() -> &'static str {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    S.get_or_init(|| RULING_SERIES_LIST.join("|"))
}

const UNSLASHED_LEGACY_LIST: &[&str] = &["IT", "MT", "CRP"];

fn re_ruling_citation() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(&format!(
            r"^({})\s+\d{{1,4}}/D?\d+(?:[A-Z0-9]+)?(?:\s|$|\()",
            ruling_series_alt()
        ))
        .unwrap()
    })
}

fn re_ruling_unslashed() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(&format!(
            r"^({})\s+\d{{1,5}}(?:\s|$|[—\-])",
            UNSLASHED_LEGACY_LIST.join("|")
        ))
        .unwrap()
    })
}

fn re_atoid() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^ATO\s+ID\s+\d{4}/\d+").unwrap())
}

fn re_psla() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^PS\s+LA\s+\d{4}/").unwrap())
}

fn re_smsfrb() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^SMSFRB\s+\d{4}/").unwrap())
}

fn re_neutral() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^\[\d{4}\]\s+[A-Z]+\s+\d+").unwrap())
}

fn re_name_v_name() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^[A-Z][\w'.&\-]*(?:\s+(?:\([^)]+\)|[A-Z][\w'.&\-]*|and|of|the|for|on|in|an|Anor|ors?|No|nee))*(?:,?\s+(?:Pty\s+)?(?:Ltd|Limited|Inc\.?|LLC|Corp|Co\.?|Plc))?\s+(?:v\.?|vs\.?)\s+(?:the|a|an)?\s*(?:\([^)]+\)\s*)?[A-Za-z][\w'.&\-]*",
        )
        .unwrap()
    })
}

fn re_re_x() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r"(?i)^(?:Re|In\s+re|In\s+the\s+Matter\s+of|Ex\s+parte)\s+[A-Z]").unwrap()
    })
}

fn re_case_number() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"(?i)^Case\s+[A-Z]?\d+(?:/\d+)?$").unwrap())
}

fn re_act_title() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^(?:[A-Za-z][\w'&\-]*|\([^)]+\))(?:\s+(?:[A-Za-z][\w'&\-]*|\([^)]+\)))*\s+(?:Act|Regulations?|Code|Rules)\s+(?:19|20)\d{2}(?:\s*\(Cth\))?\s*$",
        )
        .unwrap()
    })
}

fn re_bill_title() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"\bBill\s+(?:19|20)\d{2}\b").unwrap())
}

fn type_phrases(
) -> &'static std::collections::HashMap<Shape, std::collections::HashSet<&'static str>> {
    static M: std::sync::OnceLock<
        std::collections::HashMap<Shape, std::collections::HashSet<&'static str>>,
    > = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let mut m = std::collections::HashMap::new();
        m.insert(
            Shape::RulingTypePhrase,
            [
                "taxation ruling",
                "class ruling",
                "product ruling",
                "law companion ruling",
                "gst ruling",
                "gst determination",
                "taxation determination",
                "superannuation guarantee ruling",
                "fuel tax ruling",
                "fringe benefits tax ruling",
                "income tax ruling",
                "miscellaneous taxation ruling",
                "law companion guideline",
                "wine equalisation tax ruling",
                "wine equalisation tax determination",
                "superannuation guarantee determination",
                "smsf ruling",
                "smsf determination",
                "ruling compendium",
                "goods and services tax ruling",
                "goods and services tax determination",
            ]
            .iter()
            .copied()
            .collect(),
        );
        m.insert(
            Shape::GuidelineTypePhrase,
            [
                "practical compliance guideline",
                "practical compliance guidelines",
            ]
            .iter()
            .copied()
            .collect(),
        );
        m.insert(
            Shape::AlertPhrase,
            ["taxpayer alert"].iter().copied().collect(),
        );
        m.insert(
            Shape::AtoidPhrase,
            ["ato interpretative decision"].iter().copied().collect(),
        );
        m.insert(
            Shape::PslaPhrase,
            [
                "practice statement law administration",
                "ato practice statement law administration",
                "law administration practice statement",
            ]
            .iter()
            .copied()
            .collect(),
        );
        m.insert(
            Shape::SmsfrbPhrase,
            ["smsf regulator's bulletin", "smsf regulators bulletin"]
                .iter()
                .copied()
                .collect(),
        );
        m.insert(
            Shape::DisPhrase,
            ["decision impact statement", "decision impact statements"]
                .iter()
                .copied()
                .collect(),
        );
        m.insert(
            Shape::EmPhrase,
            [
                "explanatory memorandum",
                "supplementary explanatory memorandum",
            ]
            .iter()
            .copied()
            .collect(),
        );
        m
    })
}

fn shape_of(heading: &str) -> Shape {
    let t = heading.split_whitespace().collect::<Vec<&str>>().join(" ");
    if t.is_empty() {
        return Shape::Empty;
    }
    let t_lower = t.to_lowercase();
    if re_neutral().is_match(&t) {
        return Shape::NeutralCitation;
    }
    if re_atoid().is_match(&t) {
        return Shape::Atoid;
    }
    if re_psla().is_match(&t) {
        return Shape::Psla;
    }
    if re_smsfrb().is_match(&t) {
        return Shape::Smsfrb;
    }
    if re_ruling_citation().is_match(&t) {
        return Shape::RulingCitation;
    }
    if re_ruling_unslashed().is_match(&t) {
        return Shape::RulingUnslashed;
    }
    for (sh, phrases) in type_phrases().iter() {
        if phrases.contains(t_lower.as_str()) {
            return *sh;
        }
    }
    if re_act_title().is_match(&t) {
        return Shape::ActTitle;
    }
    if re_bill_title().is_match(&t) {
        return Shape::BillTitle;
    }
    if re_re_x().is_match(&t) {
        return Shape::ReX;
    }
    if re_case_number().is_match(&t) {
        return Shape::CaseNumber;
    }
    if re_name_v_name().is_match(&t) && t.len() < 200 {
        return Shape::NameVName;
    }
    Shape::Other
}

fn re_docid_jud_star() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^\*(\d{4})\*(.+)$").unwrap())
}

fn re_docid_act_section() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^(\d{4})(\d{4})$").unwrap())
}

fn classify(ins: &RuleInputs) -> Template {
    let shapes: Vec<Shape> = ins.headings.iter().take(6).map(|h| shape_of(h)).collect();
    let has = |s: Shape| shapes.contains(&s);
    let any_citation = shapes.iter().any(|s| {
        matches!(
            s,
            Shape::RulingCitation | Shape::RulingUnslashed | Shape::Atoid | Shape::Psla
        )
    });

    if has(Shape::Smsfrb) || has(Shape::SmsfrbPhrase) {
        return Template::Smsfrb;
    }
    let inner = ins.inner_body();
    let outer = ins.outer_prefix();
    if outer == "JUD" && re_docid_jud_star().is_match(&inner) {
        return Template::HistCase;
    }
    if (outer == "PAC" || outer == "REG") && re_docid_act_section().is_match(&inner) {
        return Template::LegislationSection;
    }
    if any_citation {
        return Template::OfficialPub;
    }
    if has(Shape::DisPhrase)
        && shapes
            .iter()
            .any(|s| matches!(s, Shape::NameVName | Shape::ReX | Shape::NeutralCitation))
    {
        return Template::Dis;
    }
    if !shapes.is_empty()
        && matches!(
            shapes[0],
            Shape::NameVName | Shape::ReX | Shape::NeutralCitation | Shape::CaseNumber
        )
    {
        return Template::CaseH1;
    }
    if shapes.len() >= 2
        && shapes[1] == Shape::NameVName
        && ins.category.as_deref() == Some("Cases")
    {
        return Template::CaseH2;
    }
    if ins.category.as_deref() == Some("Cases") {
        if shapes.iter().any(|s| {
            matches!(
                s,
                Shape::NameVName | Shape::ReX | Shape::NeutralCitation | Shape::CaseNumber
            )
        }) {
            return Template::CaseH1;
        }
        return Template::CaseH1;
    }
    if !shapes.is_empty() && shapes[0] == Shape::ActTitle {
        return Template::Act;
    }
    if has(Shape::ActTitle)
        && ins.category.as_deref() == Some("Legislation_and_supporting_material")
    {
        return Template::Act;
    }
    if has(Shape::BillTitle) || has(Shape::EmPhrase) {
        return Template::BillEm;
    }
    if ins.category.as_deref() == Some("Edited_private_advice") {
        return Template::Epa;
    }
    Template::Other
}

// ----- Token regexes (used by extractors to pull year/num) -----

fn re_citation_token() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(&format!(
            r"^({})\s+(?P<year>\d{{1,4}})/(?P<draft>D?)(?P<num>\d+)(?P<suffix>[A-Z0-9]*)",
            ruling_series_alt()
        ))
        .unwrap()
    })
}

fn re_atoid_token() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r"^ATO\s+ID\s+(?P<year>\d{4})/(?P<num>\d+)(?P<suffix>[A-Z0-9]*)").unwrap()
    })
}

fn re_psla_token() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(
            r"^PS\s+LA\s+(?P<year>\d{4})/(?P<draft>D?)(?P<num>\d+)(?P<suffix>[A-Z0-9]*)",
        )
        .unwrap()
    })
}

fn re_smsfrb_token() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^SMSFRB\s+(?P<year>\d{4})/(?P<num>\d+)").unwrap())
}

fn re_neutral_token() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r"^\[(?P<year>\d{4})\]\s+(?P<court>[A-Z]+)\s+(?P<num>\d+)").unwrap()
    })
}

fn re_act_year() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"\b(?P<year>(?:19|20)\d{2})\b").unwrap())
}

fn re_bill_year() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"\bBill\s+(?P<year>(?:19|20)\d{2})\b").unwrap())
}

fn re_withdrawn() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"(?i)\(\s*withdrawn\s*\)").unwrap())
}

fn re_precise_date() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(\d{1,2})\s+(January|February|March|April|May|June|July|August|September|October|November|December)\s+(\d{4})\b").unwrap()
    })
}

fn month_index(name: &str) -> u32 {
    match name.to_ascii_lowercase().as_str() {
        "january" => 1,
        "february" => 2,
        "march" => 3,
        "april" => 4,
        "may" => 5,
        "june" => 6,
        "july" => 7,
        "august" => 8,
        "september" => 9,
        "october" => 10,
        "november" => 11,
        "december" => 12,
        _ => 0,
    }
}

fn re_old_report() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r"\((?P<year>1[89]\d{2}|20\d{2})\)\s+(?:L\.?R\.?|AC|QB|KB|Ch|CLR|ALR|ATC|ATR|FCR|HL|PC|NSWLR|VR|QR|SASR)").unwrap()
    })
}

fn re_mailto_body() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r#"MailTo:\?Subject=[^&]*&Body=([^)\s"]+)"#).unwrap())
}

fn re_case_header_name() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^\*##\s+(?P<name>[^*\n]+?)\s*\*").unwrap())
}

fn clean_citation(raw: &str) -> String {
    let cleaned = re_withdrawn().replace_all(raw, "").trim().to_string();
    let cleaned = regex::Regex::new(r"\s+")
        .unwrap()
        .replace_all(&cleaned, " ")
        .to_string();
    let pattern = format!(
        r"^({}|ATO\s+ID|PS\s+LA|SMSFRB)\s+(\d{{1,4}})/(D?)(\d+)([A-Z]{{1,2}}\d*)?$",
        ruling_series_alt()
    );
    let re = regex::Regex::new(&pattern).unwrap();
    if let Some(c) = re.captures(&cleaned) {
        let series = &c[1];
        let year = &c[2];
        let draft = &c[3];
        let num = &c[4];
        let suffix = c.get(5).map(|m| m.as_str()).unwrap_or("");
        return format!("{series} {year}/{draft}{num}{suffix}");
    }
    cleaned
}

fn year_from_token(token: &str) -> Option<u32> {
    let regs = [
        re_citation_token(),
        re_atoid_token(),
        re_psla_token(),
        re_smsfrb_token(),
        re_neutral_token(),
    ];
    for re in regs.iter() {
        if let Some(c) = re.captures(token) {
            if let Some(y) = c.name("year") {
                let s = y.as_str();
                let v: u32 = s.parse().ok()?;
                return Some(if s.len() == 4 { v } else { 1900 + v });
            }
        }
    }
    None
}

fn precise_date(text: &str) -> Option<String> {
    let m = re_precise_date().captures(text)?;
    let day: u32 = m.get(1)?.as_str().parse().ok()?;
    let month_name = m.get(2)?.as_str();
    let year: u32 = m.get(3)?.as_str().parse().ok()?;
    let month = month_index(month_name);
    if month == 0 {
        return None;
    }
    Some(format!("{:04}-{:02}-{:02}", year, month, day))
}

fn type_phrase_shape(s: Shape) -> bool {
    matches!(
        s,
        Shape::RulingTypePhrase
            | Shape::GuidelineTypePhrase
            | Shape::AtoidPhrase
            | Shape::PslaPhrase
            | Shape::SmsfrbPhrase
            | Shape::DisPhrase
            | Shape::AlertPhrase
            | Shape::EmPhrase
    )
}

fn citation_shape(s: Shape) -> bool {
    matches!(
        s,
        Shape::RulingCitation
            | Shape::RulingUnslashed
            | Shape::Atoid
            | Shape::Psla
            | Shape::Smsfrb
            | Shape::NeutralCitation
            | Shape::NameVName
            | Shape::ReX
            | Shape::CaseNumber
            | Shape::ActTitle
            | Shape::BillTitle
    )
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<&str>>().join(" ")
}

fn compose_title(primary: Option<&str>, ins: &RuleInputs) -> Option<String> {
    let primary = primary?;
    if primary.is_empty() {
        return None;
    }
    let primary = collapse_ws(primary);
    let mut parts = vec![primary.clone()];
    let mut seen = std::collections::HashSet::new();
    seen.insert(primary.to_lowercase());
    for h in ins.headings.iter().take(5) {
        let t = collapse_ws(h);
        if t.is_empty() || seen.contains(&t.to_lowercase()) {
            continue;
        }
        if t.starts_with("/law/view/") {
            continue;
        }
        let s = shape_of(&t);
        if type_phrase_shape(s) || citation_shape(s) {
            continue;
        }
        parts.push(t);
        break;
    }
    Some(parts.join(" — "))
}

fn prefix_overlap(candidate: &str, parts: &[String]) -> bool {
    let cand_lower = candidate.to_lowercase();
    for p in parts {
        let p_lower = p.to_lowercase();
        if cand_lower == p_lower
            || cand_lower.starts_with(&p_lower)
            || p_lower.starts_with(&cand_lower)
        {
            return true;
        }
    }
    false
}

fn compose_from_em_front_matter(ins: &RuleInputs) -> Option<String> {
    let phrase = ins.front_matter_phrase.as_deref()?.trim().to_string();
    if phrase.is_empty() {
        return None;
    }
    let refs: Vec<&String> = ins
        .front_matter_refs
        .iter()
        .filter(|r| !r.trim().is_empty())
        .collect();
    if refs.is_empty() {
        return None;
    }
    let citation = collapse_ws(refs[0]);
    let mut parts = vec![phrase, citation];
    let mut section: Option<String> = None;
    for h in &ins.headings {
        let t = collapse_ws(h);
        if !t.is_empty() {
            section = Some(t);
            break;
        }
    }
    if let Some(s) = section {
        if !prefix_overlap(&s, &parts) {
            parts.push(s);
        }
    }
    Some(parts.join(" — "))
}

fn compose_from_body_h2(ins: &RuleInputs) -> Option<String> {
    if ins.headings.is_empty() || ins.heading_levels.len() != ins.headings.len() {
        return None;
    }
    for (i, lvl) in ins.heading_levels.iter().enumerate() {
        if *lvl == 1 && !collapse_ws(&ins.headings[i]).is_empty() {
            return None;
        }
    }
    for (i, lvl) in ins.heading_levels.iter().enumerate() {
        if *lvl == 2 {
            let t = collapse_ws(&ins.headings[i]);
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

fn compose_from_first_ref(ins: &RuleInputs) -> Option<String> {
    if ins.front_matter_phrase.is_some() {
        return None;
    }
    let first = ins
        .front_matter_refs
        .iter()
        .find(|r| !r.trim().is_empty())?;
    Some(collapse_ws(first))
}

fn compose_from_leading_headings(ins: &RuleInputs) -> Option<String> {
    if ins.headings.is_empty() || ins.heading_levels.len() != ins.headings.len() {
        return None;
    }
    let h1_idx = ins
        .heading_levels
        .iter()
        .enumerate()
        .find(|(i, lvl)| **lvl == 1 && !collapse_ws(&ins.headings[*i]).is_empty())
        .map(|(i, _)| i)?;
    let h1 = collapse_ws(&ins.headings[h1_idx]);
    let h2_idx = ((h1_idx + 1)..ins.headings.len())
        .find(|i| ins.heading_levels[*i] == 2 && !collapse_ws(&ins.headings[*i]).is_empty());
    let h3_anchor = h2_idx.unwrap_or(h1_idx);
    let h3_idx = ((h3_anchor + 1)..ins.headings.len())
        .find(|i| ins.heading_levels[*i] == 3 && !collapse_ws(&ins.headings[*i]).is_empty());
    let mut parts = vec![h1];
    for idx in [h2_idx, h3_idx].iter().flatten() {
        let candidate = collapse_ws(&ins.headings[*idx]);
        if prefix_overlap(&candidate, &parts) {
            continue;
        }
        parts.push(candidate);
    }
    Some(parts.join(" — "))
}

// ----- Per-template extractors -----

fn extract_official_pub(ins: &RuleInputs) -> DerivedMetadata {
    let mut citation_heading: Option<String> = None;
    let mut unslashed_heading: Option<String> = None;
    for h in ins.headings.iter().take(6) {
        let s = shape_of(h);
        if matches!(s, Shape::RulingCitation | Shape::Atoid | Shape::Psla) {
            citation_heading = Some(h.clone());
            break;
        }
        if unslashed_heading.is_none() && s == Shape::RulingUnslashed {
            unslashed_heading = Some(h.clone());
        }
    }
    if citation_heading.is_none() {
        if let Some(uh) = unslashed_heading {
            let t = collapse_ws(&uh);
            let trimmed = regex::Regex::new(r"\s*[—\-].*$")
                .unwrap()
                .replace(&t, "")
                .trim()
                .to_string();
            return DerivedMetadata {
                title: compose_title(Some(&trimmed), ins),
                date: precise_date(&ins.body_head.chars().take(600).collect::<String>()),
            };
        }
        return extract_other(ins);
    }
    let raw = citation_heading.unwrap();
    let mut cleaned = clean_citation(&raw);
    let year = year_from_token(&cleaned);
    let head_slice: String = ins.body_head.chars().take(600).collect();
    let pd = precise_date(&head_slice);
    if re_withdrawn().is_match(&raw) {
        cleaned = format!("{} (Withdrawn)", cleaned);
    }
    DerivedMetadata {
        title: compose_title(Some(&cleaned), ins),
        date: pd.or_else(|| year.map(|y| format!("{}-01-01", y))),
    }
}

fn case_name_from(heading: &str) -> Option<String> {
    let t = collapse_ws(heading);
    if t.is_empty() || t.len() > 200 {
        return None;
    }
    let t = regex::Regex::new(r"\s*\[\d{4}\].*$")
        .unwrap()
        .replace(&t, "")
        .trim()
        .to_string();
    let t = regex::Regex::new(r"\bv\.\s+")
        .unwrap()
        .replace_all(&t, "v ")
        .to_string();
    Some(t)
}

fn extract_case_h1(ins: &RuleInputs) -> DerivedMetadata {
    let mut name: Option<String> = None;
    let mut year: Option<u32> = None;
    for h in ins.headings.iter().take(5) {
        let s = shape_of(h);
        if s == Shape::NeutralCitation {
            if let Some(c) = re_neutral_token().captures(h.trim()) {
                let y_str = &c["year"];
                let court = &c["court"];
                let num = &c["num"];
                name = Some(format!("[{}] {} {}", y_str, court, num));
                year = y_str.parse().ok();
                break;
            }
        }
        if s == Shape::NameVName || s == Shape::ReX {
            name = case_name_from(h);
            break;
        }
        if s == Shape::CaseNumber {
            name = Some(collapse_ws(h));
            break;
        }
    }
    if name.is_none() {
        let em_dash_re = regex::Regex::new(r"\s+—\s+").unwrap();
        for h in ins.headings.iter().take(3) {
            for part in em_dash_re.split(h) {
                let part = collapse_ws(part);
                let ps = shape_of(&part);
                if ps == Shape::NameVName && part != *h {
                    name = case_name_from(&part);
                    break;
                }
                if ps == Shape::NeutralCitation {
                    if let Some(c) = re_neutral_token().captures(&part) {
                        let y_str = &c["year"];
                        name = Some(format!("[{}] {} {}", y_str, &c["court"], &c["num"]));
                        year = y_str.parse().ok();
                        break;
                    }
                }
            }
            if name.is_some() {
                break;
            }
        }
    }
    if name.is_none() && ins.category.as_deref() == Some("Cases") {
        for h in ins.headings.iter().take(3) {
            let clean = collapse_ws(h);
            if !clean.is_empty() && !clean.starts_with("/law/view/") && clean.len() < 200 {
                name = Some(clean);
                break;
            }
        }
    }
    if year.is_none() {
        let mut sources = vec![ins.title.clone().unwrap_or_default()];
        sources.extend(ins.headings.iter().take(5).cloned());
        for src in &sources {
            if let Some(c) = re_neutral_token().find(src) {
                if let Some(cap) = re_neutral_token().captures(c.as_str()) {
                    year = cap["year"].parse().ok();
                    break;
                }
            }
        }
    }
    if year.is_none() {
        let head: String = ins.body_head.chars().take(400).collect();
        if let Some(c) = re_old_report().captures(&head) {
            year = c["year"].parse().ok();
        }
    }
    let head: String = ins.body_head.chars().take(600).collect();
    let pd = precise_date(&head);
    DerivedMetadata {
        title: name,
        date: pd.or_else(|| year.map(|y| format!("{}-01-01", y))),
    }
}

fn extract_case_h2(ins: &RuleInputs) -> DerivedMetadata {
    let name = case_name_from(&ins.h2());
    let head: String = ins.body_head.chars().take(500).collect();
    let mut year: Option<u32> = None;
    if let Some(c) = re_neutral_token().captures(&head) {
        year = c["year"].parse().ok();
    }
    if year.is_none() {
        if let Some(c) = re_old_report().captures(&head) {
            year = c["year"].parse().ok();
        }
    }
    let head6: String = ins.body_head.chars().take(600).collect();
    DerivedMetadata {
        title: name,
        date: precise_date(&head6).or_else(|| year.map(|y| format!("{}-01-01", y))),
    }
}

fn extract_dis(ins: &RuleInputs) -> DerivedMetadata {
    let case_name = case_name_from(&ins.h2()).or_else(|| case_name_from(&ins.h1()));
    let head: String = ins.body_head.chars().take(1200).collect();
    let mut year: Option<u32> = None;
    if let Some(c) = re_neutral_token().captures(&head) {
        year = c["year"].parse().ok();
    }
    let head6: String = ins.body_head.chars().take(600).collect();
    let pd = precise_date(&head6);
    let title = case_name.map(|n| format!("DIS: {}", n));
    DerivedMetadata {
        title,
        date: pd.or_else(|| year.map(|y| format!("{}-01-01", y))),
    }
}

fn extract_act(ins: &RuleInputs) -> DerivedMetadata {
    let name = collapse_ws(&ins.h1());
    let year = re_act_year()
        .captures(&name)
        .and_then(|c| c["year"].parse().ok());
    DerivedMetadata {
        title: if name.is_empty() { None } else { Some(name) },
        date: year.map(|y: u32| format!("{}-01-01", y)),
    }
}

fn parse_mailto_body(body_head: &str) -> Vec<String> {
    let m = match re_mailto_body().captures(body_head) {
        Some(c) => c,
        None => return Vec::new(),
    };
    let raw = &m[1];
    let parts = raw.split("%0D");
    let mut out = Vec::new();
    for p in parts {
        // Manual percent-decode (matches the helper used elsewhere).
        let bytes = p.as_bytes();
        let mut decoded = String::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(byte) = u8::from_str_radix(
                    std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00"),
                    16,
                ) {
                    decoded.push(byte as char);
                    i += 3;
                    continue;
                }
            }
            decoded.push(bytes[i] as char);
            i += 1;
        }
        let text = decoded.trim().to_string();
        if text.is_empty() || text.to_lowercase().starts_with("link:") {
            continue;
        }
        out.push(text);
    }
    out
}

fn extract_legislation_section(ins: &RuleInputs) -> DerivedMetadata {
    let inner = ins.inner_body();
    let cap = re_docid_act_section().captures(&inner);
    let year = cap.as_ref().and_then(|c| c[1].parse::<u32>().ok());
    let act_no = cap
        .as_ref()
        .map(|c| c[2].trim_start_matches('0').to_string());
    let segs: Vec<&str> = ins.doc_id.split('/').filter(|s| !s.is_empty()).collect();
    let section_id = segs.get(2).map(|s| s.to_string()).unwrap_or_default();
    let outer = ins.outer_prefix();

    let mut act_name: Option<String> = None;
    for h in ins.headings.iter().take(6) {
        let t = collapse_ws(h);
        if re_act_title().is_match(&t) {
            act_name = Some(t);
            break;
        }
    }
    if act_name.is_none() {
        for line in parse_mailto_body(&ins.body_head) {
            if re_act_title().is_match(&line) {
                act_name = Some(line);
                break;
            }
        }
    }
    let title = if let Some(n) = act_name.clone() {
        if !section_id.is_empty() {
            if outer == "PAC" {
                format!("{} s {}", n, section_id)
            } else {
                format!("{} reg {}", n, section_id)
            }
        } else {
            n
        }
    } else if outer == "PAC" {
        match (year, act_no.as_deref()) {
            (Some(y), Some(no)) => format!("Act {} No. {} s {}", y, no, section_id),
            _ => format!("PAC {}/{}", inner, section_id),
        }
    } else {
        match year {
            Some(y) => format!("Regulations {} reg {}", y, section_id),
            None => format!("REG {}/{}", inner, section_id),
        }
    };
    let final_year = year.or_else(|| {
        act_name
            .as_ref()
            .and_then(|n| re_act_year().captures(n))
            .and_then(|c| c["year"].parse().ok())
    });
    let head6: String = ins.body_head.chars().take(600).collect();
    DerivedMetadata {
        title: Some(title),
        date: precise_date(&head6)
            .or_else(|| final_year.map(|y| format!("{}-01-01", y)))
            .or_else(|| ins.pub_date.clone()),
    }
}

fn extract_historical_case(ins: &RuleInputs) -> DerivedMetadata {
    let inner = ins.inner_body();
    let year = re_docid_jud_star()
        .captures(&inner)
        .and_then(|c| c[1].parse::<u32>().ok());
    let head4: String = ins.body_head.chars().take(400).collect();
    let mut name: Option<String> = None;
    if let Some(c) = re_case_header_name().captures(&head4) {
        name = Some(collapse_ws(&c["name"]));
    }
    if name.is_none() {
        let trail_re = regex::Regex::new(r"\s*-\s*\([^)]+\)\s*$").unwrap();
        for line in parse_mailto_body(&ins.body_head) {
            if line.to_lowercase() == "cases" {
                continue;
            }
            if line.contains(" v ") || line.contains(" - (") {
                let nm = trail_re.replace(&line, "").trim().to_string();
                if !nm.is_empty() && nm.len() < 200 {
                    name = Some(nm);
                    break;
                }
            }
        }
    }
    if name.is_none() {
        for h in ins.headings.iter().take(4) {
            let t = collapse_ws(h);
            if !t.is_empty() && !t.starts_with("/law/view/") && t.len() < 200 {
                name = Some(t);
                break;
            }
        }
    }
    if name.is_none() {
        name = if inner.is_empty() { None } else { Some(inner) };
    }
    let head6: String = ins.body_head.chars().take(600).collect();
    DerivedMetadata {
        title: name,
        date: precise_date(&head6)
            .or_else(|| year.map(|y| format!("{}-01-01", y)))
            .or_else(|| ins.pub_date.clone()),
    }
}

fn extract_bill_em(ins: &RuleInputs) -> DerivedMetadata {
    let em_title = compose_from_em_front_matter(ins);
    let h2 = ins.h2();
    let h1 = ins.h1();
    let source = if re_bill_year().is_match(&h2) { h2 } else { h1 };
    let mut bill_title = collapse_ws(&source);
    if !re_bill_year().is_match(&bill_title) && !re_act_title().is_match(&bill_title) {
        let head8: String = ins.body_head.chars().take(800).collect();
        let bold_re = regex::Regex::new(r"\*\*([^*]+?)\*\*").unwrap();
        for cap in bold_re.captures_iter(&head8) {
            let line = collapse_ws(&cap[1]);
            if re_bill_year().is_match(&line) || re_act_title().is_match(&line) {
                bill_title = line;
                break;
            }
        }
    }
    let year = re_bill_year()
        .captures(&bill_title)
        .or_else(|| re_act_year().captures(&bill_title))
        .and_then(|c| c["year"].parse::<u32>().ok());
    let mut title = em_title;
    if title.is_none() && !bill_title.is_empty() {
        title = if !bill_title.contains("Explanatory") && year.is_some() {
            Some(format!("EM to {}", bill_title))
        } else {
            Some(bill_title.clone())
        };
    }
    let needs_compose = title
        .as_deref()
        .map(|t| type_phrase_shape(shape_of(t)))
        .unwrap_or(true);
    if needs_compose {
        if let Some(c) = compose_from_leading_headings(ins)
            .or_else(|| compose_from_body_h2(ins))
            .or_else(|| compose_from_first_ref(ins))
        {
            title = Some(c);
        }
    }
    let head6: String = ins.body_head.chars().take(600).collect();
    DerivedMetadata {
        title,
        date: precise_date(&head6)
            .or_else(|| year.map(|y| format!("{}-01-01", y)))
            .or_else(|| ins.pub_date.clone()),
    }
}

fn extract_smsfrb(ins: &RuleInputs) -> DerivedMetadata {
    for h in ins.headings.iter().take(4) {
        if let Some(c) = re_smsfrb_token().captures(h) {
            let year: u32 = c["year"].parse().unwrap_or(0);
            return DerivedMetadata {
                title: compose_title(Some(&format!("SMSFRB {}/{}", &c["year"], &c["num"])), ins),
                date: Some(format!("{}-01-01", year)),
            };
        }
    }
    extract_other(ins)
}

fn re_docid_year4() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(&format!(
            r"^({})(?P<year>(?:19|20)\d{{2}})(?P<draft>D?)(?P<num>\d+)$",
            ruling_series_alt()
        ))
        .unwrap()
    })
}

fn re_docid_year2() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(&format!(
            r"^({})(?P<year>[89]\d)(?P<num>\d+)$",
            ruling_series_alt()
        ))
        .unwrap()
    })
}

fn re_docid_psla() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^PSLA(?P<year>\d{4})(?P<num>\d+)$").unwrap())
}

fn re_docid_psla_draft() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^PSD(?P<year>\d{4})D?(?P<num>\d+)$").unwrap())
}

fn re_docid_atoid() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^(?:ATOID|AID)(?P<year>\d{4})(?P<num>\d+)$").unwrap())
}

fn extract_from_docid(ins: &RuleInputs) -> (Option<String>, Option<u32>) {
    let body = ins.inner_body();
    if let Some(c) = re_docid_year4().captures(&body) {
        let series = &c[1];
        let y: u32 = c["year"].parse().unwrap_or(0);
        let draft = &c["draft"];
        return (
            Some(format!("{} {}/{}{}", series, &c["year"], draft, &c["num"])),
            Some(y),
        );
    }
    if let Some(c) = re_docid_psla().captures(&body) {
        let y: u32 = c["year"].parse().unwrap_or(0);
        return (Some(format!("PS LA {}/{}", &c["year"], &c["num"])), Some(y));
    }
    if let Some(c) = re_docid_psla_draft().captures(&body) {
        let y: u32 = c["year"].parse().unwrap_or(0);
        return (
            Some(format!("PS LA {}/D{}", &c["year"], &c["num"])),
            Some(y),
        );
    }
    if let Some(c) = re_docid_atoid().captures(&body) {
        let y: u32 = c["year"].parse().unwrap_or(0);
        return (
            Some(format!("ATO ID {}/{}", &c["year"], &c["num"])),
            Some(y),
        );
    }
    if let Some(c) = re_docid_year2().captures(&body) {
        let series = &c[1];
        let y2: u32 = c["year"].parse().unwrap_or(0);
        return (
            Some(format!("{} {}/{}", series, &c["year"], &c["num"])),
            Some(1900 + y2),
        );
    }
    (None, None)
}

fn re_date_of_advice() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r"(?i)\bDate\s+of\s+(?:advice|ruling|issue)\s*[:\-]?\s*(?P<day>\d{1,2})\s+(?P<mon>January|February|March|April|May|June|July|August|September|October|November|December)\s+(?P<year>\d{4})").unwrap()
    })
}

fn extract_epa(ins: &RuleInputs) -> DerivedMetadata {
    let auth = ins.inner_body();
    let auth = auth.trim().to_string();
    let outer = ins.outer_prefix();
    let code = if !auth.is_empty() {
        Some(format!("{} {}", outer, auth))
    } else {
        None
    };
    let head: String = ins.body_head.chars().take(1500).collect();
    let mut precise: Option<String> = None;
    if let Some(c) = re_date_of_advice().captures(&head) {
        let month = month_index(&c["mon"]);
        let day: u32 = c["day"].parse().unwrap_or(0);
        let year: u32 = c["year"].parse().unwrap_or(0);
        precise = Some(format!("{:04}-{:02}-{:02}", year, month, day));
    }
    let date = precise.or_else(|| ins.pub_date.clone());
    DerivedMetadata { title: code, date }
}

fn extract_other(ins: &RuleInputs) -> DerivedMetadata {
    let (code, year) = extract_from_docid(ins);
    let mut year = year;
    if year.is_none() {
        if let Some(pd) = ins.pub_date.as_deref() {
            if pd.len() >= 4 {
                let prefix = &pd[..4];
                if prefix.chars().all(|c| c.is_ascii_digit()) {
                    year = prefix.parse().ok();
                }
            }
        }
    }
    let head6: String = ins.body_head.chars().take(600).collect();
    let pd = precise_date(&head6);
    let title = compose_from_em_front_matter(ins)
        .or_else(|| compose_from_leading_headings(ins))
        .or_else(|| compose_from_body_h2(ins))
        .or_else(|| compose_from_first_ref(ins))
        .or(code);
    DerivedMetadata {
        title,
        date: pd
            .or_else(|| ins.pub_date.clone())
            .or_else(|| year.map(|y| format!("{}-01-01", y))),
    }
}

fn universal_fallback_title(ins: &RuleInputs) -> Option<String> {
    let outer = ins.outer_prefix();
    let inner = ins.inner_body();
    if !outer.is_empty() && !inner.is_empty() {
        return Some(format!("{} {}", outer, inner));
    }
    if !outer.is_empty() {
        return Some(outer);
    }
    None
}

fn year_from_docid_fallback(ins: &RuleInputs) -> Option<u32> {
    let body = ins.inner_body();
    if let Some(c) = re_docid_jud_star().captures(&body) {
        return c[1].parse().ok();
    }
    if let Some(c) = re_docid_act_section().captures(&body) {
        return c[1].parse().ok();
    }
    let r = regex::Regex::new(r"^((?:19|20)\d{2})").unwrap();
    if let Some(c) = r.captures(&body) {
        return c[1].parse().ok();
    }
    None
}

pub fn derive_metadata(ins: &RuleInputs) -> DerivedMetadata {
    let template = classify(ins);
    let mut result = match template {
        Template::OfficialPub => extract_official_pub(ins),
        Template::CaseH1 => extract_case_h1(ins),
        Template::CaseH2 => extract_case_h2(ins),
        Template::HistCase => extract_historical_case(ins),
        Template::Dis => extract_dis(ins),
        Template::Act => extract_act(ins),
        Template::LegislationSection => extract_legislation_section(ins),
        Template::BillEm => extract_bill_em(ins),
        Template::Smsfrb => extract_smsfrb(ins),
        Template::Epa => extract_epa(ins),
        Template::Other => extract_other(ins),
    };
    if result.title.is_none() {
        let (fb_code, fb_year) = extract_from_docid(ins);
        if let Some(c) = fb_code {
            result.title = Some(c);
            if result.date.is_none() {
                if let Some(y) = fb_year {
                    result.date = Some(format!("{}-01-01", y));
                }
            }
        }
    }
    if result.title.is_none() {
        result.title = universal_fallback_title(ins);
    }
    if result.date.is_none() {
        if let Some(pd) = ins.pub_date.clone() {
            result.date = Some(pd);
        } else if let Some(y) = year_from_docid_fallback(ins) {
            result.date = Some(format!("{}-01-01", y));
        }
    }
    result
}

#[cfg(test)]
mod rules_tests {
    use super::*;

    fn ins(doc_id: &str, headings: &[&str]) -> RuleInputs {
        RuleInputs {
            doc_id: doc_id.to_string(),
            headings: headings.iter().map(|s| s.to_string()).collect(),
            heading_levels: (1..=headings.len() as u32).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn shape_classifies_taxation_ruling_phrase() {
        assert_eq!(shape_of("Taxation Ruling"), Shape::RulingTypePhrase);
    }

    #[test]
    fn shape_classifies_ruling_citation() {
        assert_eq!(shape_of("TR 2024/3"), Shape::RulingCitation);
    }

    #[test]
    fn shape_classifies_neutral_citation() {
        assert_eq!(shape_of("[2024] HCA 41"), Shape::NeutralCitation);
    }

    #[test]
    fn classify_ruling_routes_to_official_pub() {
        let i = ins(
            "TXR/TR20243/NAT/ATO/00001",
            &["Taxation Ruling", "TR 2024/3", "Subtitle"],
        );
        assert_eq!(classify(&i), Template::OfficialPub);
    }

    #[test]
    fn derive_metadata_official_pub_title_with_citation() {
        let i = ins(
            "TXR/TR20243/NAT/ATO/00001",
            &[
                "Taxation Ruling",
                "TR 2024/3",
                "R&D tax incentive eligibility",
            ],
        );
        let d = derive_metadata(&i);
        assert_eq!(
            d.title.as_deref(),
            Some("TR 2024/3 — R&D tax incentive eligibility")
        );
        assert_eq!(d.date.as_deref(), Some("2024-01-01"));
    }

    #[test]
    fn derive_metadata_dis() {
        let mut i = ins(
            "DIS/DIS2024_PEPSICO/NAT/ATO",
            &[
                "Decision impact statement",
                "Pepsico Inc v Commissioner of Taxation",
            ],
        );
        i.body_head = String::new();
        let d = derive_metadata(&i);
        assert_eq!(
            d.title.as_deref(),
            Some("DIS: Pepsico Inc v Commissioner of Taxation")
        );
    }

    #[test]
    fn derive_metadata_act_year() {
        // PAC/<8digit>/<section> → LegislationSection extractor (not Act).
        // The Act name comes from h1 and gets " s <section>" appended.
        let i = ins("PAC/19970038/995-1", &["Income Tax Assessment Act 1997"]);
        let d = derive_metadata(&i);
        assert_eq!(
            d.title.as_deref(),
            Some("Income Tax Assessment Act 1997 s 995-1")
        );
        assert_eq!(d.date.as_deref(), Some("1997-01-01"));
    }

    #[test]
    fn derive_metadata_act_template_no_section() {
        // Pure Act title with no PAC docid → Act extractor.
        let i = ins(
            "ACT/INCOME_TAX_ASSESSMENT_1997",
            &["Income Tax Assessment Act 1997"],
        );
        let d = derive_metadata(&i);
        assert_eq!(d.title.as_deref(), Some("Income Tax Assessment Act 1997"));
        assert_eq!(d.date.as_deref(), Some("1997-01-01"));
    }

    #[test]
    fn derive_metadata_epa_uses_docid() {
        let mut i = ins("EV/1012101718232/00001", &[]);
        i.category = Some("Edited_private_advice".to_string());
        let d = derive_metadata(&i);
        assert_eq!(d.title.as_deref(), Some("EV 1012101718232"));
    }

    #[test]
    fn derive_metadata_universal_fallback_when_nothing_matches() {
        let i = ins("XYZ/abc/def", &[]);
        let d = derive_metadata(&i);
        assert_eq!(d.title.as_deref(), Some("XYZ abc"));
    }

    #[test]
    fn precise_date_parses_real_date() {
        assert_eq!(
            precise_date("issued on 12 March 2024 by ..."),
            Some("2024-03-12".to_string())
        );
    }

    #[test]
    fn clean_citation_drops_withdrawn_marker() {
        assert_eq!(clean_citation("LCR 2019/2EC (Withdrawn)"), "LCR 2019/2EC");
    }
}

// ----- Build orchestrator (port of src/ato_mcp/indexer/build.py) -----
//
// Walks pages_dir/index.jsonl, runs each doc through the cleaning + chunker
// + rules-engine metadata classifier + embedder pipeline in-process, writes
// documents + chunks + chunk_embeddings + chunks_fts + title_fts +
// doc_anchors + definitions + citations rows, then writes pack files,
// asset blobs, manifest.json, and update.json to --out-dir. Missing vs
// build.py: release seeding, checkpoint resume, parallelism.

struct PendingBuildEmbedding {
    chunk_id: i64,
    doc_idx: usize,
    chunk_idx: usize,
    text: String,
}

#[derive(Default)]
struct BuildProfile {
    enabled: bool,
    started_at: Option<std::time::Instant>,
    docs: usize,
    chunks: usize,
    html_bytes: u64,
    read: Duration,
    clean: Duration,
    metadata: Duration,
    chunking: Duration,
    references: Duration,
    sqlite: Duration,
    assets: Duration,
    embedding: Duration,
    embedding_tokenize: Duration,
    embedding_prepare: Duration,
    embedding_run: Duration,
    embedding_postprocess: Duration,
    embedding_write: Duration,
    embedding_batches: usize,
    embedding_inputs: usize,
    embedding_active_tokens: usize,
    embedding_padded_tokens: usize,
    embedding_max_batch: usize,
    embedding_max_seq_len: usize,
    pack: Duration,
    checkpoint: Duration,
    finalise: Duration,
}

impl BuildProfile {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            started_at: enabled.then(std::time::Instant::now),
            ..Self::default()
        }
    }

    fn elapsed(&self) -> Duration {
        self.started_at
            .map(|started| started.elapsed())
            .unwrap_or_default()
    }

    fn print(&self) {
        if !self.enabled {
            return;
        }
        // [IB-19] --profile reports stage timing plus embedding batch,
        // token, padding, and model-throughput counters for build tuning.
        let total = self.elapsed().as_secs_f64().max(0.000_001);
        eprintln!(
            "ato-mcp build profile: docs={} chunks={} html_mb={:.1} total_s={:.2} docs_per_s={:.2}",
            self.docs,
            self.chunks,
            self.html_bytes as f64 / (1024.0 * 1024.0),
            total,
            self.docs as f64 / total
        );
        let rows = [
            ("read", self.read),
            ("clean", self.clean),
            ("metadata", self.metadata),
            ("chunking", self.chunking),
            ("references", self.references),
            ("sqlite", self.sqlite),
            ("assets", self.assets),
            ("embedding", self.embedding),
            ("pack", self.pack),
            ("checkpoint", self.checkpoint),
            ("finalise", self.finalise),
        ];
        for (name, duration) in rows {
            let secs = duration.as_secs_f64();
            eprintln!("  {name:>10}: {secs:>8.2}s {:>5.1}%", secs * 100.0 / total);
        }
        if self.embedding_batches > 0 {
            let model_secs = self.embedding_run.as_secs_f64().max(0.000_001);
            let padding_ratio = if self.embedding_padded_tokens == 0 {
                0.0
            } else {
                self.embedding_active_tokens as f64 / self.embedding_padded_tokens as f64
            };
            eprintln!(
                "  embedding batches={} inputs={} active_tokens={} padded_tokens={} padding_efficiency={:.1}% max_batch={} max_seq_len={} model_tokens_per_s={:.0}",
                self.embedding_batches,
                self.embedding_inputs,
                self.embedding_active_tokens,
                self.embedding_padded_tokens,
                padding_ratio * 100.0,
                self.embedding_max_batch,
                self.embedding_max_seq_len,
                self.embedding_padded_tokens as f64 / model_secs,
            );
            let rows = [
                ("embed_tok", self.embedding_tokenize),
                ("embed_prep", self.embedding_prepare),
                ("embed_run", self.embedding_run),
                ("embed_post", self.embedding_postprocess),
                ("embed_write", self.embedding_write),
            ];
            for (name, duration) in rows {
                let secs = duration.as_secs_f64();
                eprintln!("  {name:>10}: {secs:>8.2}s {:>5.1}%", secs * 100.0 / total);
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
struct SemanticEncodeStats {
    tokenize: Duration,
    prepare: Duration,
    run: Duration,
    postprocess: Duration,
    batches: usize,
    inputs: usize,
    active_tokens: usize,
    padded_tokens: usize,
    max_batch: usize,
    max_seq_len: usize,
}

impl SemanticEncodeStats {
    fn record_batch(&mut self, batch: usize, seq_len: usize, active_tokens: usize) {
        self.batches += 1;
        self.inputs += batch;
        self.active_tokens += active_tokens;
        self.padded_tokens += batch * seq_len;
        self.max_batch = self.max_batch.max(batch);
        self.max_seq_len = self.max_seq_len.max(seq_len);
    }

    fn merge(&mut self, other: Self) {
        self.tokenize += other.tokenize;
        self.prepare += other.prepare;
        self.run += other.run;
        self.postprocess += other.postprocess;
        self.batches += other.batches;
        self.inputs += other.inputs;
        self.active_tokens += other.active_tokens;
        self.padded_tokens += other.padded_tokens;
        self.max_batch = self.max_batch.max(other.max_batch);
        self.max_seq_len = self.max_seq_len.max(other.max_seq_len);
    }
}

fn is_batch_allocation_failure(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    msg.contains("failed to allocate memory") || msg.contains("out of memory")
}

fn encode_build_embeddings_adaptive(
    state: &ServerState,
    inputs: &[String],
) -> Result<(Vec<[i8; EMBEDDING_DIM]>, SemanticEncodeStats)> {
    if inputs.is_empty() {
        return Ok((Vec::new(), SemanticEncodeStats::default()));
    }
    match state.encode_query_embeddings_with_stats(inputs) {
        Ok((embeddings, stats)) => Ok((embeddings, stats)),
        Err(err) if inputs.len() > 1 && is_batch_allocation_failure(&err) => {
            let mid = inputs.len() / 2;
            eprintln!(
                "ato-mcp build: embedding batch of {} exceeded GPU memory; retrying as {} + {}",
                inputs.len(),
                mid,
                inputs.len() - mid
            );
            let (mut embeddings, mut stats) =
                encode_build_embeddings_adaptive(state, &inputs[..mid])?;
            let (tail_embeddings, tail_stats) =
                encode_build_embeddings_adaptive(state, &inputs[mid..])?;
            embeddings.extend(tail_embeddings);
            stats.merge(tail_stats);
            Ok((embeddings, stats))
        }
        Err(err) => Err(err).context(format!("encoding {} chunk embeddings", inputs.len())),
    }
}

fn flush_pending_build_embeddings(
    state: &ServerState,
    conn: &Connection,
    pending: &mut Vec<PendingBuildEmbedding>,
    pack_records: &mut [(String, JsonValue)],
    profile: &mut BuildProfile,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let mut order: Vec<usize> = (0..pending.len()).collect();
    order.sort_by_key(|&idx| pending[idx].text.len());
    for batch in order.chunks(BUILD_EMBED_BATCH_SIZE) {
        let inputs: Vec<String> = batch.iter().map(|&idx| pending[idx].text.clone()).collect();
        let (embeddings, stats) = encode_build_embeddings_adaptive(state, &inputs)?;
        profile.embedding_tokenize += stats.tokenize;
        profile.embedding_prepare += stats.prepare;
        profile.embedding_run += stats.run;
        profile.embedding_postprocess += stats.postprocess;
        profile.embedding_batches += stats.batches;
        profile.embedding_inputs += stats.inputs;
        profile.embedding_active_tokens += stats.active_tokens;
        profile.embedding_padded_tokens += stats.padded_tokens;
        profile.embedding_max_batch = profile.embedding_max_batch.max(stats.max_batch);
        profile.embedding_max_seq_len = profile.embedding_max_seq_len.max(stats.max_seq_len);
        if embeddings.len() != batch.len() {
            bail!(
                "embedding batch returned {} vectors for {} chunks",
                embeddings.len(),
                batch.len()
            );
        }
        let write_started = std::time::Instant::now();
        for (&idx, emb) in batch.iter().zip(embeddings.iter()) {
            let item = &pending[idx];
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(emb.as_ptr() as *const u8, emb.len()) };
            conn.execute(
                "INSERT INTO chunk_embeddings (chunk_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![item.chunk_id, bytes],
            )
            .context("INSERT chunk_embeddings")?;
            // [IB-11] Pack records carry base64 raw int8 embeddings; install
            // decode length-checks them against EMBEDDING_DIM.
            let emb_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            let chunk_record = pack_records
                .get_mut(item.doc_idx)
                .and_then(|(_doc_id, record)| record.get_mut("chunks"))
                .and_then(|chunks| chunks.as_array_mut())
                .and_then(|chunks| chunks.get_mut(item.chunk_idx))
                .ok_or_else(|| anyhow!("missing pack chunk record for embedded chunk"))?;
            chunk_record["embedding_b64"] = JsonValue::String(emb_b64);
        }
        profile.embedding_write += write_started.elapsed();
    }
    pending.clear();
    profile.embedding += started.elapsed();
    Ok(())
}

struct BuildPackShardContext<'a> {
    out_dir: &'a Path,
    zstd_level: i32,
    doc_hashes: &'a HashMap<String, String>,
    documents: &'a mut Vec<DocRef>,
    packs: &'a mut Vec<PackInfo>,
    profile: &'a mut BuildProfile,
}

fn write_build_pack_shard(
    shard_idx: usize,
    pack_records: &mut Vec<(String, JsonValue)>,
    ctx: &mut BuildPackShardContext<'_>,
) -> Result<()> {
    if pack_records.is_empty() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    eprintln!(
        "ato-mcp build: writing pack shard {} ({} docs)",
        shard_idx + 1,
        pack_records.len()
    );
    let tmp_pack = ctx
        .out_dir
        .join("packs")
        .join(format!(".pack-{shard_idx:04}-writing.bin.zst.tmp"));
    let pack_meta = write_pack(&tmp_pack, ctx.zstd_level, pack_records.drain(..).map(Ok))?;
    let sha8 = pack_meta
        .get("sha8")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("write_pack returned no sha8"))?
        .to_string();
    let final_pack = ctx
        .out_dir
        .join("packs")
        .join(format!("pack-{sha8}.bin.zst"));
    fs::rename(&tmp_pack, &final_pack)?;

    let refs = pack_meta
        .get("refs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("write_pack returned no refs"))?;
    for r in refs {
        let doc_id = r
            .get("doc_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("pack ref missing doc_id"))?
            .to_string();
        let content_hash = ctx
            .doc_hashes
            .get(&doc_id)
            .cloned()
            .ok_or_else(|| anyhow!("missing content hash for packed doc {doc_id}"))?;
        ctx.documents.push(DocRef {
            doc_id,
            content_hash,
            pack_sha8: sha8.clone(),
            offset: r
                .get("offset")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow!("pack ref missing offset"))?,
            length: r
                .get("length")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow!("pack ref missing length"))?,
        });
    }

    let pack_size = pack_meta
        .get("size")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("write_pack returned no size"))?;
    let pack_sha256 = pack_meta
        .get("sha256")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("write_pack returned no sha256"))?
        .to_string();
    ctx.packs.push(PackInfo {
        sha8: sha8.clone(),
        sha256: pack_sha256,
        size: pack_size,
        url: format!("packs/pack-{sha8}.bin.zst"),
    });
    ctx.profile.pack += started.elapsed();
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BuildCheckpoint {
    // [IB-13] Checkpoints persist source/model/chunker gates plus committed
    // doc refs, packs, and base-release verification state.
    schema_version: u32,
    source_index_sha256: String,
    zstd_level: i32,
    embedding_model_id: String,
    embedding_model_fingerprint: String,
    embedding_dim: usize,
    embedding_input_max_tokens: usize,
    chunker_format_version: u32,
    documents: Vec<DocRef>,
    packs: Vec<PackInfo>,
    #[serde(default)]
    base_documents: Vec<DocRef>,
    #[serde(default)]
    base_source_hash_by_doc_id: HashMap<String, String>,
    #[serde(default)]
    verified_source_doc_ids: Vec<String>,
}

fn build_checkpoint_path(out_dir: &Path) -> PathBuf {
    out_dir.join("build-state.json")
}

fn load_build_checkpoint(
    out_dir: &Path,
    source_index_sha256: &str,
    zstd_level: i32,
) -> Result<Option<BuildCheckpoint>> {
    let path = build_checkpoint_path(out_dir);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let checkpoint: BuildCheckpoint =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if checkpoint.schema_version != BUILD_CHECKPOINT_SCHEMA_VERSION {
        bail!(
            "unsupported build checkpoint schema {} in {}",
            checkpoint.schema_version,
            path.display()
        );
    }
    if checkpoint.source_index_sha256 != source_index_sha256 {
        bail!(
            "build checkpoint source index hash differs from {}; remove {} to start a fresh build",
            source_index_sha256,
            path.display()
        );
    }
    if checkpoint.zstd_level != zstd_level {
        bail!(
            "build checkpoint zstd level {} differs from requested {}; remove {} to start a fresh build",
            checkpoint.zstd_level,
            zstd_level,
            path.display()
        );
    }
    if checkpoint.embedding_model_id != EMBEDDING_MODEL_ID {
        bail!(
            "build checkpoint embedding model `{}` differs from requested `{}`; remove {} to start a fresh build",
            checkpoint.embedding_model_id,
            EMBEDDING_MODEL_ID,
            path.display()
        );
    }
    if checkpoint.embedding_model_fingerprint != EMBEDDING_MODEL_FINGERPRINT {
        bail!(
            "build checkpoint embedding model fingerprint differs from requested model; remove {} to start a fresh build",
            path.display()
        );
    }
    if checkpoint.embedding_dim != EMBEDDING_DIM {
        bail!(
            "build checkpoint embedding dim {} differs from requested {}; remove {} to start a fresh build",
            checkpoint.embedding_dim,
            EMBEDDING_DIM,
            path.display()
        );
    }
    if checkpoint.embedding_input_max_tokens != EMBEDDING_INPUT_MAX_TOKENS {
        bail!(
            "build checkpoint embedding input max {} differs from requested {}; remove {} to start a fresh build",
            checkpoint.embedding_input_max_tokens,
            EMBEDDING_INPUT_MAX_TOKENS,
            path.display()
        );
    }
    if checkpoint.chunker_format_version != CHUNKER_FORMAT_VERSION {
        bail!(
            "build checkpoint chunker format {} differs from requested {}; remove {} to start a fresh build",
            checkpoint.chunker_format_version,
            CHUNKER_FORMAT_VERSION,
            path.display()
        );
    }
    Ok(Some(checkpoint))
}

struct SaveBuildCheckpointArgs<'a> {
    out_dir: &'a Path,
    source_index_sha256: &'a str,
    zstd_level: i32,
    documents: &'a [DocRef],
    packs: &'a [PackInfo],
    base_documents: &'a [DocRef],
    base_source_hash_by_doc_id: &'a HashMap<String, String>,
    verified_source_doc_ids: &'a HashSet<String>,
}

fn save_build_checkpoint(args: SaveBuildCheckpointArgs<'_>) -> Result<()> {
    let mut verified_source_doc_ids: Vec<String> =
        args.verified_source_doc_ids.iter().cloned().collect();
    verified_source_doc_ids.sort();
    let checkpoint = BuildCheckpoint {
        schema_version: BUILD_CHECKPOINT_SCHEMA_VERSION,
        source_index_sha256: args.source_index_sha256.to_string(),
        zstd_level: args.zstd_level,
        embedding_model_id: EMBEDDING_MODEL_ID.to_string(),
        embedding_model_fingerprint: EMBEDDING_MODEL_FINGERPRINT.to_string(),
        embedding_dim: EMBEDDING_DIM,
        embedding_input_max_tokens: EMBEDDING_INPUT_MAX_TOKENS,
        chunker_format_version: CHUNKER_FORMAT_VERSION,
        documents: args.documents.to_vec(),
        packs: args.packs.to_vec(),
        base_documents: args.base_documents.to_vec(),
        base_source_hash_by_doc_id: args.base_source_hash_by_doc_id.clone(),
        verified_source_doc_ids,
    };
    let path = build_checkpoint_path(args.out_dir);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&checkpoint)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

fn clean_stale_build_packs(out_dir: &Path, packs: &[PackInfo]) -> Result<()> {
    let packs_dir = out_dir.join("packs");
    if !packs_dir.exists() {
        return Ok(());
    }
    let keep: HashSet<String> = packs
        .iter()
        .map(|pack| format!("pack-{}.bin.zst", pack.sha8))
        .collect();
    for entry in fs::read_dir(&packs_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let is_pack = name.starts_with("pack-") && name.ends_with(".bin.zst");
        let is_tmp = name.starts_with(".pack-") && name.ends_with(".tmp");
        if is_tmp || (is_pack && !keep.contains(name)) {
            fs::remove_file(&path).with_context(|| format!("removing stale {}", path.display()))?;
        }
    }
    Ok(())
}

fn committed_build_doc_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM documents WHERE pack_sha8 <> 'PENDING'",
        [],
        |row| row.get(0),
    )?;
    Ok(count as usize)
}

fn pending_build_doc_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM documents WHERE pack_sha8 = 'PENDING'",
        [],
        |row| row.get(0),
    )?;
    Ok(count as usize)
}

fn update_pack_sha8_for_docs(conn: &Connection, docs: &[DocRef]) -> Result<()> {
    let mut update = conn.prepare("UPDATE documents SET pack_sha8 = ?1 WHERE doc_id = ?2")?;
    for doc in docs {
        update.execute(rusqlite::params![&doc.pack_sha8, &doc.doc_id])?;
    }
    Ok(())
}

fn remove_build_doc(conn: &Connection, doc_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM chunks_fts WHERE rowid IN (SELECT chunk_id FROM chunks WHERE doc_id = ?1)",
        [doc_id],
    )?;
    conn.execute("DELETE FROM title_fts WHERE doc_id = ?1", [doc_id])?;
    conn.execute("DELETE FROM citations WHERE target_doc_id = ?1", [doc_id])?;
    conn.execute("DELETE FROM documents WHERE doc_id = ?1", [doc_id])?;
    Ok(())
}

fn copy_or_hard_link(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if dst.exists() {
        return Ok(());
    }
    match fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(src, dst)
                .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
            Ok(())
        }
    }
}

struct BaseReleaseSeed {
    documents: Vec<DocRef>,
    packs: Vec<PackInfo>,
    by_doc_id: HashMap<String, DocRef>,
    source_hash_by_doc_id: HashMap<String, String>,
}

struct BuildSourceFingerprint<'a> {
    doc_id: &'a str,
    doc_type: &'a str,
    title: &'a str,
    date: &'a Option<String>,
    html: &'a str,
    currency: &'a CurrencyInfo,
    has_in_doc_links: bool,
    has_related_docs: bool,
    has_history: bool,
    anchor_refs: &'a [AnchorRef],
    definitions: &'a [JsonValue],
    chunks: &'a [Chunk],
    assets: &'a [ExtractedAsset],
}

fn source_fingerprint_hash(value: &JsonValue) -> Result<String> {
    let mut h = Sha256::new();
    h.update(serde_json::to_vec(value)?);
    let digest = h.finalize();
    let hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Ok(format!("sha256:{hex}"))
}

fn build_source_fingerprint_value(input: BuildSourceFingerprint<'_>) -> JsonValue {
    json!({
        "doc_id": input.doc_id,
        "type": input.doc_type,
        "title": input.title,
        "date": input.date,
        "html": input.html,
        "withdrawn_date": &input.currency.withdrawn_date,
        "superseded_by": &input.currency.superseded_by,
        "replaces": &input.currency.replaces,
        "has_in_doc_links": input.has_in_doc_links as i64,
        "has_related_docs": input.has_related_docs as i64,
        "has_history": input.has_history as i64,
        "anchors": input.anchor_refs.iter().map(|r| json!({
            "kind": &r.kind,
            "label": &r.label,
            "target_doc_id": &r.target_doc_id,
            "target_pit": &r.target_pit,
        })).collect::<Vec<_>>(),
        "definitions": input.definitions,
        "chunks": input.chunks.iter().map(|chunk| json!({
            "ord": chunk.ord,
            "anchor": &chunk.anchor,
            "text": &chunk.text,
        })).collect::<Vec<_>>(),
        "assets": input.assets.iter().map(|asset| json!({
            "asset_ref": &asset.asset_ref,
            "source_path": &asset.source_path,
            "relative_path": &asset.relative_path,
            "media_type": &asset.media_type,
            "alt": &asset.alt,
            "title": &asset.title,
            "sha256": &asset.sha256,
            "size": asset.size,
        })).collect::<Vec<_>>(),
    })
}

fn pack_record_source_fingerprint_value(record: &PackRecord) -> JsonValue {
    json!({
        "doc_id": &record.doc_id,
        "type": &record.doc_type,
        "title": &record.title,
        "date": &record.date,
        "html": &record.html,
        "withdrawn_date": &record.withdrawn_date,
        "superseded_by": &record.superseded_by,
        "replaces": &record.replaces,
        "has_in_doc_links": record.has_in_doc_links,
        "has_related_docs": record.has_related_docs,
        "has_history": record.has_history,
        "anchors": record.anchors.iter().map(|anchor| json!({
            "kind": &anchor.kind,
            "label": &anchor.label,
            "target_doc_id": &anchor.target_doc_id,
            "target_pit": &anchor.target_pit,
        })).collect::<Vec<_>>(),
        "definitions": record.definitions.iter().map(|definition| json!({
            "definition_id": &definition.definition_id,
            "term": &definition.term,
            "norm_term": &definition.norm_term,
            "doc_id": &definition.doc_id,
            "source_title": &definition.source_title,
            "source_type": &definition.source_type,
            "scope": &definition.scope,
            "anchor": &definition.anchor,
            "ord": definition.ord,
            "body": &definition.body,
        })).collect::<Vec<_>>(),
        "chunks": record.chunks.iter().map(|chunk| json!({
            "ord": chunk.ord,
            "anchor": &chunk.anchor,
            "text": &chunk.text,
        })).collect::<Vec<_>>(),
        "assets": record.assets.iter().map(|asset| json!({
            "asset_ref": &asset.asset_ref,
            "source_path": &asset.source_path,
            "relative_path": &asset.relative_path,
            "media_type": &asset.media_type,
            "alt": &asset.alt,
            "title": &asset.title,
            "sha256": &asset.sha256,
            "size": asset.size,
        })).collect::<Vec<_>>(),
    })
}

fn pack_filename(url: &str) -> Result<String> {
    Path::new(url)
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("pack URL has no filename: {url}"))
}

fn seed_build_from_base_release(
    base_dir: &Path,
    out_dir: &Path,
    db_path: &Path,
) -> Result<BaseReleaseSeed> {
    if out_dir.exists()
        && base_dir.exists()
        && out_dir.canonicalize().ok() == base_dir.canonicalize().ok()
    {
        bail!("--base-release-dir must not point at --out-dir");
    }
    if db_path.exists() {
        bail!(
            "--base-release-dir requires an absent --db-path; remove {} before seeding",
            db_path.display()
        );
    }
    let manifest_path = base_dir.join("manifest.json");
    let db_src = base_dir.join("ato.db");
    if !manifest_path.exists() {
        bail!("base release missing {}", manifest_path.display());
    }
    if !db_src.exists() {
        bail!("base release missing {}", db_src.display());
    }
    let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    if manifest.model.id != EMBEDDING_MODEL_ID {
        bail!(
            "base release uses embedding model `{}`; expected `{EMBEDDING_MODEL_ID}`",
            manifest.model.id
        );
    }
    if parse_hf_model_url(&manifest.model.url).is_some()
        && manifest.model.sha256 != EMBEDDING_MODEL_FINGERPRINT
    {
        bail!("base release embedding fingerprint differs from the current pinned model");
    }
    let base_packs_dir = base_dir.join("packs");
    let mut source_hash_by_doc_id = HashMap::new();
    let pack_index: HashMap<String, PackInfo> = manifest
        .packs
        .iter()
        .map(|pack| (pack.sha8.clone(), pack.clone()))
        .collect();
    let mut docs_by_pack: HashMap<String, Vec<&DocRef>> = HashMap::new();
    for doc in &manifest.documents {
        docs_by_pack
            .entry(doc.pack_sha8.clone())
            .or_default()
            .push(doc);
    }
    for (sha8, docs) in docs_by_pack {
        let pack = pack_index
            .get(&sha8)
            .ok_or_else(|| anyhow!("base manifest missing pack info for {sha8}"))?;
        let filename = pack_filename(&pack.url)?;
        let pack_path = base_packs_dir.join(filename);
        let pack_bytes =
            fs::read(&pack_path).with_context(|| format!("reading {}", pack_path.display()))?;
        if !pack.sha256.is_empty() {
            verify_sha256_bytes(&pack_bytes, &pack.sha256)
                .with_context(|| format!("verifying {}", pack_path.display()))?;
        }
        if pack.size != 0 && pack_bytes.len() as u64 != pack.size {
            bail!(
                "base pack size mismatch for {}: got {}, expected {}",
                pack_path.display(),
                pack_bytes.len(),
                pack.size
            );
        }
        for doc in docs {
            let record = read_record_from_pack_bytes(&pack_bytes, doc.offset, doc.length)
                .with_context(|| format!("reading base pack record {}", doc.doc_id))?;
            // [IB-12] Previous-release reuse is keyed by a full
            // source-derived fingerprint, not just cleaned body text.
            let source_hash =
                source_fingerprint_hash(&pack_record_source_fingerprint_value(&record))
                    .with_context(|| format!("hashing base source record {}", doc.doc_id))?;
            source_hash_by_doc_id.insert(doc.doc_id.clone(), source_hash);
        }
    }
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&db_src, db_path).with_context(|| {
        format!(
            "copying base DB {} to {}",
            db_src.display(),
            db_path.display()
        )
    })?;
    let out_packs_dir = out_dir.join("packs");
    fs::create_dir_all(&out_packs_dir)?;
    for pack in &manifest.packs {
        let filename = pack_filename(&pack.url)?;
        copy_or_hard_link(
            &base_packs_dir.join(&filename),
            &out_packs_dir.join(&filename),
        )?;
    }
    let by_doc_id = manifest
        .documents
        .iter()
        .map(|doc| (doc.doc_id.clone(), doc.clone()))
        .collect();
    Ok(BaseReleaseSeed {
        documents: manifest.documents,
        packs: manifest.packs,
        by_doc_id,
        source_hash_by_doc_id,
    })
}

struct BuildCorpusArgs<'a> {
    pages_dir: &'a Path,
    db_path: &'a Path,
    model_dir: &'a Path,
    base_release_dir: Option<&'a Path>,
    out_dir: &'a Path,
    zstd_level: i32,
    limit: Option<usize>,
    use_gpu: bool,
    profile_enabled: bool,
}

fn build_corpus(args: BuildCorpusArgs<'_>) -> Result<()> {
    use std::io::BufRead as _;

    let BuildCorpusArgs {
        pages_dir,
        db_path,
        model_dir,
        base_release_dir,
        out_dir,
        zstd_level,
        limit,
        use_gpu,
        profile_enabled,
    } = args;

    // [IB-17] Maintainer builds require a local pinned Granite model
    // checkout; hosted model metadata is owned by publish/release.
    let semantic_model_paths = SemanticModelPaths::from_model_dir(model_dir)?;
    let index_path = pages_dir.join("index.jsonl");
    let source_index_sha256 = sha256_file(&index_path)?;
    let index_file =
        File::open(&index_path).with_context(|| format!("opening {}", index_path.display()))?;
    let reader = std::io::BufReader::new(index_file);

    fs::create_dir_all(out_dir)
        .with_context(|| format!("creating out_dir {}", out_dir.display()))?;
    fs::create_dir_all(out_dir.join("packs"))?;
    fs::create_dir_all(out_dir.join("assets"))?;

    let checkpoint = load_build_checkpoint(out_dir, &source_index_sha256, zstd_level)?;
    let checkpoint_loaded = checkpoint.is_some();
    let mut base_doc_refs: HashMap<String, DocRef> = HashMap::new();
    let mut base_documents: Vec<DocRef> = Vec::new();
    let mut base_source_hash_by_doc_id: HashMap<String, String> = HashMap::new();
    let mut source_doc_ids: HashSet<String> = HashSet::new();
    let mut base_seeded = false;
    let (mut documents, mut packs) = match checkpoint {
        Some(checkpoint) => {
            eprintln!(
                "ato-mcp build: resuming from checkpoint ({} docs, {} packs)",
                checkpoint.documents.len(),
                checkpoint.packs.len()
            );
            if !checkpoint.base_documents.is_empty() {
                base_documents = checkpoint.base_documents;
                base_doc_refs = base_documents
                    .iter()
                    .map(|doc| (doc.doc_id.clone(), doc.clone()))
                    .collect();
                base_source_hash_by_doc_id = checkpoint.base_source_hash_by_doc_id;
                source_doc_ids = checkpoint.verified_source_doc_ids.into_iter().collect();
                base_seeded = true;
            }
            (checkpoint.documents, checkpoint.packs)
        }
        None => {
            fs::remove_file(out_dir.join("manifest.json")).ok();
            fs::remove_file(out_dir.join("update.json")).ok();
            if let Some(base_dir) = base_release_dir {
                let seed = seed_build_from_base_release(base_dir, out_dir, db_path)?;
                eprintln!(
                    "ato-mcp build: seeded from base release {} ({} docs, {} packs)",
                    base_dir.display(),
                    seed.documents.len(),
                    seed.packs.len()
                );
                base_doc_refs = seed.by_doc_id;
                base_documents = seed.documents.clone();
                base_source_hash_by_doc_id = seed.source_hash_by_doc_id;
                base_seeded = true;
                (seed.documents, seed.packs)
            } else {
                (Vec::new(), Vec::new())
            }
        }
    };
    let conn = open_write_at(db_path)
        .with_context(|| format!("opening sqlite at {}", db_path.display()))?;
    init_db(&conn)?;
    clean_stale_build_packs(out_dir, &packs)?;
    let committed_docs = committed_build_doc_count(&conn)?;
    let pending_docs = pending_build_doc_count(&conn)?;
    if pending_docs > 0 {
        bail!(
            "build DB has {pending_docs} uncheckpointed PENDING documents at {}; remove the release dir to start fresh",
            db_path.display()
        );
    }
    if committed_docs != documents.len() {
        bail!(
            "build checkpoint has {} documents but DB has {committed_docs}; remove {} to start fresh",
            documents.len(),
            build_checkpoint_path(out_dir).display()
        );
    }
    // [IB-14] Resume skips only checkpoint-committed docs (or verified source
    // doc_ids for a base-seeded checkpoint); PENDING rows abort above.
    let checkpoint_doc_ids: HashSet<String> = if checkpoint_loaded && base_seeded {
        source_doc_ids.clone()
    } else if checkpoint_loaded {
        documents.iter().map(|doc| doc.doc_id.clone()).collect()
    } else {
        HashSet::new()
    };

    let mut profile = BuildProfile::new(profile_enabled);
    // [IB-16] Corpus build runs as a single Rust process with adaptive
    // embedding batches and no separate worker-pool build path.
    let state = ServerState::with_model_paths(use_gpu, semantic_model_paths);
    let mut processed: usize = if checkpoint_loaded && base_seeded {
        source_doc_ids.len()
    } else {
        checkpoint_doc_ids.len()
    };
    let mut skipped_no_payload: usize = 0;
    let mut skipped_duplicate_doc_ids: usize = 0;
    let mut reused_base_docs: usize = 0;
    let mut changed_base_docs: usize = 0;
    let mut removed_base_docs: usize = 0;
    let mut tx = conn.unchecked_transaction()?;

    // Pack records collected for this build. Each record is the full doc
    // payload the Rust updater can ingest from `pack-<sha8>.bin.zst`.
    let mut pack_records: Vec<(String, JsonValue)> = Vec::new();
    let mut pending_embeddings: Vec<PendingBuildEmbedding> =
        Vec::with_capacity(BUILD_EMBED_BATCH_SIZE);
    let mut doc_hashes: HashMap<String, String> = HashMap::new();
    let mut pack_shard_idx: usize = packs.len();

    for line_res in reader.lines() {
        if let Some(n) = limit {
            if processed >= n {
                break;
            }
        }
        let line = line_res?;
        if line.trim().is_empty() {
            continue;
        }
        let record: JsonValue = serde_json::from_str(&line).context("parsing index.jsonl line")?;
        let canonical_id = record
            .get("canonical_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("index record missing canonical_id"))?;
        let Some(payload_path_raw) = record.get("payload_path").and_then(|v| v.as_str()) else {
            skipped_no_payload += 1;
            continue;
        };
        if payload_path_raw.is_empty() {
            skipped_no_payload += 1;
            continue;
        }
        let payload_path = pages_dir.join(payload_path_raw);
        let doc_id = metadata_doc_id_for(canonical_id);
        let checkpoint_verified = checkpoint_doc_ids.contains(&doc_id);
        if !source_doc_ids.insert(doc_id.clone()) {
            if checkpoint_verified {
                processed += 1;
                continue;
            }
            skipped_duplicate_doc_ids += 1;
            continue;
        }
        if checkpoint_verified {
            processed += 1;
            continue;
        }

        let started = std::time::Instant::now();
        let html = fs::read_to_string(&payload_path)
            .with_context(|| format!("reading payload {}", payload_path.display()))?;
        profile.read += started.elapsed();
        profile.html_bytes += html.len() as u64;
        let doc_type = metadata_parse_docid(canonical_id).unwrap_or_default();

        // Cleaning pipeline.
        let started = std::time::Instant::now();
        let cleaned = clean_ato_html(&html);
        let (rewritten_html, assets) =
            rewrite_images_html(&cleaned.html, Some(&doc_id), Some(payload_path.as_path()));
        let normalised = normalise_named_anchors(&rewritten_html);
        let with_links = rewrite_links_html(&normalised);
        let final_html = strip_attributes(&with_links);
        profile.clean += started.elapsed();

        // Currency / supersession from raw page HTML (alert + body scan).
        let started = std::time::Instant::now();
        let currency = extract_currency(&html);

        // Initial title from leading-headings composer (always present).
        let leading = extract_leading_headings(&cleaned.html);
        let composed_title = extract_compose_title(&leading);
        let raw_title = composed_title
            .clone()
            .or(cleaned.title.clone())
            .unwrap_or_else(|| canonical_id.to_string());

        // Headings + levels for the rule engine.
        let mut headings: Vec<String> = Vec::new();
        let mut heading_levels: Vec<u32> = Vec::new();
        {
            let frag = scraper::Html::parse_fragment(&final_html);
            let h_sel = scraper::Selector::parse("h1, h2, h3, h4, h5, h6").unwrap();
            for h in frag.select(&h_sel) {
                let text = anchors_node_text(h);
                if text.is_empty() {
                    continue;
                }
                let level: u32 = match h.value().name() {
                    "h1" => 1,
                    "h2" => 2,
                    "h3" => 3,
                    "h4" => 4,
                    "h5" => 5,
                    "h6" => 6,
                    _ => 0,
                };
                headings.push(text);
                heading_levels.push(level);
            }
        }

        // Body head (first 3000 chars of cleaned text) for date / case-name pulls.
        let body_head: String = cleaned.text.chars().take(3000).collect();
        let pub_date = metadata_extract_pub_date(&body_head);

        // EM front-matter signals (parliamentary EM / regulation ES).
        let (fm_refs, fm_phrase) = extract_em_front_matter(&cleaned.html);

        let rule_inputs = RuleInputs {
            doc_id: doc_id.clone(),
            title: Some(raw_title.clone()),
            headings,
            heading_levels,
            body_head,
            category: Some(doc_type.clone()),
            pub_date,
            front_matter_refs: fm_refs,
            front_matter_phrase: fm_phrase,
        };
        let derived = derive_metadata(&rule_inputs);
        let title = derived.title.clone().unwrap_or(raw_title);
        let derived_date = derived.date.clone();
        profile.metadata += started.elapsed();

        // Chunker.
        let started = std::time::Instant::now();
        let chunks = chunk_html(&final_html, Some(&title), EMBED_MAX_TOKENS);
        profile.chunking += started.elapsed();
        profile.chunks += chunks.len();

        // Anchor refs (used for navigation flags + doc_anchors table).
        let started = std::time::Instant::now();
        let anchor_refs = extract_anchors(&final_html, &doc_id);
        let has_in_doc_links = anchor_refs.iter().any(|r| r.kind == "in_doc");
        let has_related_docs = anchor_refs.iter().any(|r| r.kind == "sister");
        let has_history = anchor_refs.iter().any(|r| r.kind == "history");
        profile.references += started.elapsed();

        // Definitions are source-derived and needed before the base-release
        // reuse decision, but their DB rows are inserted after the document row.
        let started = std::time::Instant::now();
        let def_chunks: Vec<DefinitionChunk> = chunks
            .iter()
            .map(|c| DefinitionChunk {
                ord: c.ord,
                anchor: c.anchor.clone(),
                text: c.text.clone(),
            })
            .collect();
        let defs = extract_definitions(&doc_id, &title, &doc_type, &def_chunks);
        let definition_records: Vec<JsonValue> = defs
            .iter()
            .map(|d| {
                json!({
                    "definition_id": d.definition_id.clone(),
                    "term": d.term.clone(),
                    "norm_term": d.norm_term.clone(),
                    "doc_id": d.doc_id.clone(),
                    "source_title": d.source_title.clone(),
                    "source_type": d.source_type.clone(),
                    "scope": d.scope.clone(),
                    "anchor": d.anchor.clone(),
                    "ord": d.ord,
                    "body": d.body.clone(),
                })
            })
            .collect();
        profile.references += started.elapsed();

        let source_hash =
            source_fingerprint_hash(&build_source_fingerprint_value(BuildSourceFingerprint {
                doc_id: &doc_id,
                doc_type: &doc_type,
                title: &title,
                date: &derived_date,
                html: &final_html,
                currency: &currency,
                has_in_doc_links,
                has_related_docs,
                has_history,
                anchor_refs: &anchor_refs,
                definitions: &definition_records,
                chunks: &chunks,
                assets: &assets,
            }))?;
        let content_hash = metadata_content_hash(&cleaned.text);
        if base_doc_refs.contains_key(&doc_id) {
            let base_source_hash = base_source_hash_by_doc_id
                .get(&doc_id)
                .ok_or_else(|| anyhow!("base release missing source fingerprint for {doc_id}"))?;
            if base_source_hash == &source_hash {
                reused_base_docs += 1;
                processed += 1;
                continue;
            }
            remove_build_doc(&tx, &doc_id)
                .with_context(|| format!("removing changed base doc {doc_id}"))?;
            documents.retain(|doc| doc.doc_id != doc_id);
            changed_base_docs += 1;
        }

        let now = chrono::Utc::now().to_rfc3339();
        doc_hashes.insert(doc_id.clone(), content_hash.clone());

        // Pack sha8 placeholder; finalised after all docs processed.
        let pack_placeholder = "PENDING".to_string();

        let started = std::time::Instant::now();
        tx.execute(
            "INSERT INTO documents
                (doc_id, type, title, date, downloaded_at, content_hash, pack_sha8,
                 html, withdrawn_date, superseded_by, replaces,
                 has_in_doc_links, has_related_docs, has_history)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                doc_id,
                doc_type,
                title,
                derived_date,
                now,
                content_hash,
                pack_placeholder,
                compress_text(&final_html)?,
                currency.withdrawn_date.clone(),
                currency.superseded_by.clone(),
                currency.replaces.clone(),
                has_in_doc_links as i64,
                has_related_docs as i64,
                has_history as i64,
            ],
        )
        .context("INSERT documents")?;

        // Insert chunks + embeddings + chunks_fts; also collect a record
        // entry for pack writing.
        let mut chunk_ids: Vec<(i64, String, Option<String>)> = Vec::new();
        let mut chunk_records: Vec<JsonValue> = Vec::new();
        let mut doc_pending_embeddings: Vec<(i64, usize, String)> = Vec::new();
        for chunk in &chunks {
            let zstd_text =
                zstd::stream::encode_all(std::io::Cursor::new(chunk.text.as_bytes()), zstd_level)
                    .context("zstd-compressing chunk text")?;
            let chunk_id: i64 = tx
                .query_row(
                    "INSERT INTO chunks (doc_id, ord, anchor, text)
                 VALUES (?1, ?2, ?3, ?4)
                 RETURNING chunk_id",
                    rusqlite::params![doc_id, chunk.ord, chunk.anchor, zstd_text],
                    |row| row.get(0),
                )
                .context("INSERT chunks")?;
            chunk_ids.push((chunk_id, chunk.text.clone(), chunk.anchor.clone()));

            tx.execute(
                "INSERT INTO chunks_fts (rowid, text) VALUES (?1, ?2)",
                rusqlite::params![chunk_id, chunk.text],
            )
            .with_context(|| {
                format!(
                    "INSERT chunks_fts doc_id={} chunk_id={} ord={}",
                    doc_id, chunk_id, chunk.ord
                )
            })?;

            let chunk_idx = chunk_records.len();
            chunk_records.push(json!({
                "ord": chunk.ord,
                "anchor": chunk.anchor.clone(),
                "text": chunk.text.clone(),
                "embedding_b64": JsonValue::Null,
            }));
            doc_pending_embeddings.push((chunk_id, chunk_idx, chunk.text.clone()));
        }

        // title_fts: concat headings into a searchable per-doc row.
        let frag = scraper::Html::parse_fragment(&final_html);
        let h_sel = scraper::Selector::parse("h1, h2, h3, h4, h5, h6").unwrap();
        let headings_concat: Vec<String> = frag
            .select(&h_sel)
            .map(anchors_node_text)
            .filter(|s| !s.is_empty())
            .collect();
        let headings_text = headings_concat.join(" ");
        tx.execute(
            "INSERT INTO title_fts (doc_id, title, headings) VALUES (?1, ?2, ?3)",
            rusqlite::params![doc_id, title, headings_text],
        )
        .context("INSERT title_fts")?;

        // doc_anchors.
        let mut anchor_records: Vec<JsonValue> = Vec::new();
        for (anchor_ord, r) in (0_i64..).zip(anchor_refs.iter()) {
            let target_chunk_id: Option<i64> = if r.kind == "in_doc" {
                if let Some(name) = r.target_anchor.as_deref() {
                    let marker = format!("[anchor:{name}]");
                    chunk_ids
                        .iter()
                        .find(|(_id, text, _anchor)| text.contains(&marker))
                        .map(|(id, _, _)| *id)
                } else {
                    None
                }
            } else {
                None
            };
            anchor_records.push(json!({
                "ord": anchor_ord,
                "kind": r.kind.clone(),
                "label": r.label.clone(),
                "target_chunk_id": target_chunk_id,
                "target_doc_id": r.target_doc_id.clone(),
                "target_pit": r.target_pit.clone(),
            }));
            tx.execute(
                "INSERT INTO doc_anchors
                    (doc_id, ord, kind, label, target_chunk_id, target_doc_id, target_pit)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    doc_id,
                    anchor_ord,
                    r.kind,
                    r.label,
                    target_chunk_id,
                    r.target_doc_id,
                    r.target_pit,
                ],
            )
            .context("INSERT doc_anchors")?;
        }
        profile.sqlite += started.elapsed();

        // Definitions.
        let started = std::time::Instant::now();
        for d in &defs {
            tx.execute(
                "INSERT OR REPLACE INTO definitions
                    (definition_id, term, norm_term, doc_id, source_title,
                     source_type, scope, anchor, ord, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    d.definition_id,
                    d.term,
                    d.norm_term,
                    d.doc_id,
                    d.source_title,
                    d.source_type,
                    d.scope,
                    d.anchor,
                    d.ord,
                    d.body,
                ],
            )
            .context("INSERT definitions")?;
        }
        profile.sqlite += started.elapsed();

        // Asset persistence: write each image to <out_dir>/assets/<sha[:2]>/<sha>.bin.
        let started = std::time::Instant::now();
        for asset in &assets {
            let target = out_dir.join(&asset.relative_path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            if !target.exists() || fs::metadata(&target)?.len() != asset.size {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(asset.data_b64.as_bytes())
                    .context("decoding asset b64")?;
                fs::write(&target, &bytes)?;
            }
        }
        profile.assets += started.elapsed();

        // Pack record (in-memory; written at end of build).
        let started = std::time::Instant::now();
        let doc_idx = pack_records.len();
        pack_records.push((
            doc_id.clone(),
            json!({
                "doc_id": doc_id,
                "type": doc_type,
                "title": title,
                "date": derived_date,
                "downloaded_at": now,
                "content_hash": content_hash,
                "html": final_html,
                "withdrawn_date": currency.withdrawn_date,
                "superseded_by": currency.superseded_by,
                "replaces": currency.replaces,
                "has_in_doc_links": has_in_doc_links as i64,
                "has_related_docs": has_related_docs as i64,
                "has_history": has_history as i64,
                "anchors": anchor_records,
                "definitions": definition_records,
                "chunks": chunk_records,
                "assets": assets.iter().map(|a| json!({
                    "asset_ref": a.asset_ref.clone(),
                    "source_path": a.source_path.clone(),
                    "relative_path": a.relative_path.clone(),
                    "media_type": a.media_type.clone(),
                    "alt": a.alt.clone(),
                    "title": a.title.clone(),
                    "sha256": a.sha256.clone(),
                    "size": a.size,
                    "data_b64": a.data_b64.clone(),
                })).collect::<Vec<_>>(),
            }),
        ));
        profile.pack += started.elapsed();
        for (chunk_id, chunk_idx, text) in doc_pending_embeddings {
            pending_embeddings.push(PendingBuildEmbedding {
                chunk_id,
                doc_idx,
                chunk_idx,
                text,
            });
        }
        if pending_embeddings.len() >= BUILD_EMBED_PENDING_FLUSH_CHUNKS {
            flush_pending_build_embeddings(
                &state,
                &tx,
                &mut pending_embeddings,
                &mut pack_records,
                &mut profile,
            )?;
        }
        if pack_records.len() >= BUILD_PACK_RECORDS_PER_SHARD {
            flush_pending_build_embeddings(
                &state,
                &tx,
                &mut pending_embeddings,
                &mut pack_records,
                &mut profile,
            )?;
            let first_new_doc = documents.len();
            write_build_pack_shard(
                pack_shard_idx,
                &mut pack_records,
                &mut BuildPackShardContext {
                    out_dir,
                    zstd_level,
                    doc_hashes: &doc_hashes,
                    documents: &mut documents,
                    packs: &mut packs,
                    profile: &mut profile,
                },
            )?;
            let started = std::time::Instant::now();
            update_pack_sha8_for_docs(&tx, &documents[first_new_doc..])?;
            tx.commit()?;
            save_build_checkpoint(SaveBuildCheckpointArgs {
                out_dir,
                source_index_sha256: &source_index_sha256,
                zstd_level,
                documents: &documents,
                packs: &packs,
                base_documents: &base_documents,
                base_source_hash_by_doc_id: &base_source_hash_by_doc_id,
                verified_source_doc_ids: &source_doc_ids,
            })?;
            profile.checkpoint += started.elapsed();
            doc_hashes.clear();
            tx = conn.unchecked_transaction()?;
            pack_shard_idx += 1;
        }

        processed += 1;
        profile.docs += 1;
        if processed.is_multiple_of(50) {
            eprintln!("ato-mcp build: processed {processed} docs");
        }
    }

    if base_seeded {
        let removed_doc_ids: Vec<String> = documents
            .iter()
            .filter(|doc| {
                base_doc_refs.contains_key(&doc.doc_id) && !source_doc_ids.contains(&doc.doc_id)
            })
            .map(|doc| doc.doc_id.clone())
            .collect();
        for doc_id in &removed_doc_ids {
            remove_build_doc(&tx, doc_id)
                .with_context(|| format!("removing doc absent from source index {doc_id}"))?;
        }
        removed_base_docs = removed_doc_ids.len();
        if removed_base_docs > 0 {
            documents.retain(|doc| !removed_doc_ids.contains(&doc.doc_id));
        }
    }

    flush_pending_build_embeddings(
        &state,
        &tx,
        &mut pending_embeddings,
        &mut pack_records,
        &mut profile,
    )?;
    let first_new_doc = documents.len();
    write_build_pack_shard(
        pack_shard_idx,
        &mut pack_records,
        &mut BuildPackShardContext {
            out_dir,
            zstd_level,
            doc_hashes: &doc_hashes,
            documents: &mut documents,
            packs: &mut packs,
            profile: &mut profile,
        },
    )?;
    let started = std::time::Instant::now();
    update_pack_sha8_for_docs(&tx, &documents[first_new_doc..])?;
    tx.commit()?;
    let used_pack_sha8: HashSet<String> =
        documents.iter().map(|doc| doc.pack_sha8.clone()).collect();
    packs.retain(|pack| used_pack_sha8.contains(&pack.sha8));
    clean_stale_build_packs(out_dir, &packs)?;
    if documents.len() > first_new_doc || base_seeded || removed_base_docs > 0 {
        save_build_checkpoint(SaveBuildCheckpointArgs {
            out_dir,
            source_index_sha256: &source_index_sha256,
            zstd_level,
            documents: &documents,
            packs: &packs,
            base_documents: &base_documents,
            base_source_hash_by_doc_id: &base_source_hash_by_doc_id,
            verified_source_doc_ids: &source_doc_ids,
        })?;
    }
    profile.checkpoint += started.elapsed();
    if skipped_no_payload > 0 {
        eprintln!("ato-mcp build: skipped {skipped_no_payload} index records without payload_path");
    }
    if skipped_duplicate_doc_ids > 0 {
        eprintln!(
            "ato-mcp build: skipped {skipped_duplicate_doc_ids} duplicate doc_id index records"
        );
    }
    if reused_base_docs > 0 {
        eprintln!("ato-mcp build: reused {reused_base_docs} unchanged docs from base release");
    }
    if changed_base_docs > 0 {
        eprintln!("ato-mcp build: rebuilt {changed_base_docs} changed docs from base release");
    }
    if removed_base_docs > 0 {
        eprintln!("ato-mcp build: removed {removed_base_docs} base docs absent from source index");
    }

    let started = std::time::Instant::now();
    let created_at = chrono::Utc::now().to_rfc3339();
    let manifest = Manifest {
        schema_version: SUPPORTED_MANIFEST_VERSION as i64,
        index_version: chrono::Utc::now().format("%Y.%m.%d").to_string(),
        created_at,
        min_client_version: env!("CARGO_PKG_VERSION").to_string(),
        model: ModelInfo {
            id: EMBEDDING_MODEL_ID.to_string(),
            sha256: EMBEDDING_MODEL_FINGERPRINT.to_string(),
            size: EMBEDDING_MODEL_HF_SIZE,
            url: EMBEDDING_MODEL_HF_URL.to_string(),
        },
        documents,
        packs,
    };

    let final_tx = conn.unchecked_transaction()?;
    set_meta(&final_tx, "index_version", &manifest.index_version)?;
    set_meta(&final_tx, "embedding_model_id", &manifest.model.id)?;
    set_meta(&final_tx, "last_update_at", &manifest.created_at)?;
    eprintln!("ato-mcp build: deriving citations…");
    // [IB-20] Build finalisation derives citations from stored [doc:X]
    // markers before manifest/update metadata is written.
    derive_citations(&final_tx)?;
    verify_semantic_install(&final_tx, &manifest)?;
    final_tx.commit()?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

    let manifest_path = out_dir.join("manifest.json");
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    let summary = UpdateSummary {
        schema_version: manifest.schema_version,
        index_version: manifest.index_version.clone(),
        min_client_version: manifest.min_client_version.clone(),
        model: manifest.model.clone(),
        document_count: manifest.documents.len(),
        pack_count: manifest.packs.len(),
        manifest_fingerprint: Some(manifest_fingerprint(&manifest)?),
    };
    let summary_path = out_dir.join("update.json");
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)?;
    eprintln!(
        "ato-mcp build: wrote {} + {}",
        manifest_path.display(),
        summary_path.display()
    );
    profile.finalise += started.elapsed();
    profile.print();

    eprintln!(
        "ato-mcp build: done - {processed} docs written to {}",
        db_path.display()
    );
    Ok(())
}

// ----- What's New scraper (port of src/ato_mcp/scraper/whats_new.py) -----

#[derive(Debug, Clone, Serialize)]
struct WhatsNewEntry {
    href: String,
    title: String,
    heading: Option<String>,
}

fn normalize_doc_href(href: &str) -> String {
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

fn parse_whats_new(html: &str, base_url: &str) -> Result<Vec<WhatsNewEntry>> {
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

fn fetch_external_doc(doc_id: &str, pit: Option<&str>, view: Option<&str>) -> Result<String> {
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
    let chunk_json: Vec<JsonValue> = chunks
        .iter()
        .map(|c| {
            json!({
                "ord": c.ord,
                "anchor": c.anchor,
                "text": c.text,
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&json!({
        "doc_id": doc_id,
        "canonical_url": url,
        "title": cleaned.title,
        "chunks": chunk_json,
    }))?)
}

struct CleanedAtoDoc {
    html: String,
    text: String,
    title: Option<String>,
}

fn clean_ato_html(html: &str) -> CleanedAtoDoc {
    use scraper::{Html, Selector};

    let doc = Html::parse_document(html);

    // Browser tab title (for hint / display).
    let title_selector = Selector::parse("title").unwrap();
    let raw_title = doc
        .select(&title_selector)
        .next()
        .map(|n| n.text().collect::<String>());
    let title = raw_title
        .map(|t| normalise_ws(&t))
        .filter(|t| !t.is_empty());

    // Pick container — first match wins; fallback to <main> then <body>.
    let container_html = pick_container_html(&doc);
    let Some(container_html) = container_html else {
        return CleanedAtoDoc {
            html: String::new(),
            text: String::new(),
            title,
        };
    };

    // Re-parse the picked container so we can strip noise within just that subtree.
    let mut subdoc = Html::parse_fragment(&container_html);
    strip_noise(&mut subdoc);
    strip_history_ui_controls(&mut subdoc);
    let referenced_anchors = collect_referenced_anchors(&subdoc);

    let cleaned_html = subdoc.root_element().html();
    let cleaned_text = subtree_text(&subdoc, &referenced_anchors);
    CleanedAtoDoc {
        html: cleaned_html,
        text: cleaned_text,
        title,
    }
}

fn pick_container_html(doc: &scraper::Html) -> Option<String> {
    use scraper::Selector;
    for sel_str in ATO_CONTAINER_SELECTORS {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(node) = doc.select(&sel).next() {
                return Some(node.html());
            }
        }
    }
    for sel_str in &["main", "body"] {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(node) = doc.select(&sel).next() {
                return Some(node.html());
            }
        }
    }
    None
}

fn strip_noise(doc: &mut scraper::Html) {
    use ego_tree::NodeId;
    use scraper::Selector;
    let mut to_remove: Vec<NodeId> = Vec::new();
    for sel_str in ATO_NOISE_SELECTORS {
        if let Ok(sel) = Selector::parse(sel_str) {
            for el in doc.select(&sel) {
                to_remove.push(el.id());
            }
        }
    }
    for id in to_remove {
        if let Some(mut node) = doc.tree.get_mut(id) {
            node.detach();
        }
    }
}

fn strip_history_ui_controls(doc: &mut scraper::Html) {
    use ego_tree::NodeId;
    use scraper::{Node as ScraperNode, Selector};

    // Pass 1: strip <img> whose title or alt matches a history-UI label.
    let img_sel = Selector::parse("img").unwrap();
    let mut img_remove: Vec<NodeId> = Vec::new();
    for el in doc.select(&img_sel) {
        let val = el.value();
        let title = val.attr("title").unwrap_or("").trim().to_lowercase();
        let alt = val.attr("alt").unwrap_or("").trim().to_lowercase();
        if ATO_HISTORY_UI_LABELS
            .iter()
            .any(|l| *l == title || *l == alt)
        {
            img_remove.push(el.id());
        }
    }
    for id in img_remove {
        if let Some(mut node) = doc.tree.get_mut(id) {
            node.detach();
        }
    }

    // Pass 2: strip text nodes whose content is exactly a history-UI label.
    let mut text_remove: Vec<NodeId> = Vec::new();
    for node_ref in doc.tree.nodes() {
        if let ScraperNode::Text(text) = node_ref.value() {
            let trimmed = text.trim().to_lowercase();
            if ATO_HISTORY_UI_LABELS.iter().any(|l| *l == trimmed) {
                text_remove.push(node_ref.id());
            }
        }
    }
    for id in text_remove {
        if let Some(mut node) = doc.tree.get_mut(id) {
            node.detach();
        }
    }
}

fn collect_referenced_anchors(doc: &scraper::Html) -> std::collections::HashSet<String> {
    use scraper::Selector;
    let sel = Selector::parse("a[href]").unwrap();
    let mut refs = std::collections::HashSet::new();
    for el in doc.select(&sel) {
        let href = el.value().attr("href").unwrap_or("");
        if let Some(name) = href.strip_prefix('#') {
            if !name.is_empty() {
                refs.insert(name.to_string());
            }
        }
    }
    refs
}

fn has_descendant_with_tag(node: ego_tree::NodeRef<scraper::Node>, tags: &[&str]) -> bool {
    use scraper::Node as ScraperNode;
    for n in node.descendants() {
        if let ScraperNode::Element(el) = n.value() {
            if tags.contains(&el.name()) {
                return true;
            }
        }
    }
    false
}

/// Walk the cleaned tree and emit text with inline markdown markers, ported
/// from src/ato_mcp/indexer/chunk.py:_inline_text + html_to_text. Block-level
/// tags introduce paragraph breaks. Inline tags emit:
///   <a> with an ATO docid in href: "text [doc:X]" (with @PiT / view= when
///     present) — ported from chunk.py:_inline_text and
///     extract.py:_doc_id_from_ato_link.
///   <a name="X"> where X is referenced: "text [anchor:X]"
///   any element with id="X" referenced (fallback): "text [anchor:X]"
///   <span data-asset-ref="X">: "[asset:X]"
///   <img alt="...">: "[image: alt]" when alt is non-empty, else dropped
///   <strong>/<b> containing <em>/<i> (or vice versa): "***term***"
///   <strong>/<b>: **text**, <em>/<i>: *text*
///   <h1>-<h6>:    "# text" / "## text" / ... on their own line
///   <br>:         newline
fn subtree_text(
    doc: &scraper::Html,
    referenced_anchors: &std::collections::HashSet<String>,
) -> String {
    let mut buf = String::new();
    for root_child in doc.tree.root().children() {
        render_node(root_child, &mut buf, referenced_anchors);
    }
    normalise_paragraph_breaks(&buf)
}

fn render_node(
    node: ego_tree::NodeRef<scraper::Node>,
    buf: &mut String,
    referenced: &std::collections::HashSet<String>,
) {
    use scraper::Node as ScraperNode;

    const BLOCK_TAGS: &[&str] = &[
        "p",
        "div",
        "section",
        "article",
        "header",
        "footer",
        "main",
        "aside",
        "table",
        "tr",
        "thead",
        "tbody",
        "tfoot",
        "td",
        "th",
        "caption",
        "ul",
        "ol",
        "li",
        "dl",
        "dt",
        "dd",
        "hr",
        "pre",
        "blockquote",
    ];

    match node.value() {
        ScraperNode::Text(t) => {
            // Collapse internal whitespace; preserve content.
            let raw: &str = &t.text;
            let mut last_ws = buf.chars().last().is_none_or(|c| c == '\n');
            for c in raw.chars() {
                if c.is_whitespace() {
                    if !last_ws {
                        buf.push(' ');
                        last_ws = true;
                    }
                } else {
                    buf.push(c);
                    last_ws = false;
                }
            }
        }
        ScraperNode::Element(el) => {
            let tag = el.name();

            // Self-contained markers that fully consume the element first.
            match tag {
                "br" => {
                    buf.push('\n');
                    return;
                }
                "img" => {
                    let alt = el.attr("alt").unwrap_or("").trim();
                    if !alt.is_empty() {
                        buf.push_str("[image: ");
                        buf.push_str(alt);
                        buf.push(']');
                    }
                    return;
                }
                "span" => {
                    if let Some(asset_ref) = el.attr("data-asset-ref") {
                        buf.push_str("[asset:");
                        buf.push_str(asset_ref);
                        buf.push(']');
                        return;
                    }
                }
                _ => {}
            }

            // Cross-doc <a> link rewriting and in-doc anchor target.
            if tag == "a" {
                let href = el.attr("href").unwrap_or("");
                let data_doc_id = el.attr("data-doc-id");
                let resolved = if let Some(id) = data_doc_id {
                    Some((
                        id.to_string(),
                        el.attr("data-pit").map(|s| s.to_string()),
                        el.attr("data-view").map(|s| s.to_string()),
                    ))
                } else if !href.is_empty() {
                    doc_id_from_ato_link(href)
                } else {
                    None
                };
                let inner = render_inner_string(node, referenced).trim().to_string();
                if let Some((doc_id, pit, view)) = resolved {
                    let mut marker = format!("[doc:{doc_id}");
                    if let Some(p) = pit.as_ref().filter(|s| !s.is_empty()) {
                        marker.push('@');
                        marker.push_str(p);
                    }
                    if let Some(v) = view.as_ref().filter(|s| !s.is_empty()) {
                        marker.push_str(" view=");
                        marker.push_str(v);
                    }
                    marker.push(']');
                    if !inner.is_empty() {
                        buf.push_str(&inner);
                        buf.push(' ');
                    }
                    buf.push_str(&marker);
                    return;
                }
                // In-doc anchor target via name=
                if let Some(name) = el.attr("name") {
                    if referenced.contains(name) {
                        if !inner.is_empty() {
                            buf.push_str(&inner);
                            buf.push(' ');
                        }
                        buf.push_str("[anchor:");
                        buf.push_str(name);
                        buf.push(']');
                        return;
                    }
                }
                // Plain <a> with no recognised doc/anchor: emit the inner text.
                if !inner.is_empty() {
                    buf.push_str(&inner);
                }
                return;
            }

            // Definition term: <strong>/<b> containing <em>/<i> or vice versa.
            let is_def_term = match tag {
                "strong" | "b" => has_descendant_with_tag(node, &["em", "i"]),
                "em" | "i" => has_descendant_with_tag(node, &["strong", "b"]),
                _ => false,
            };
            if is_def_term {
                let term = render_inner_string(node, referenced).trim().to_string();
                if !term.is_empty() {
                    buf.push_str("***");
                    buf.push_str(&term);
                    buf.push_str("***");
                }
                // Fall through to id-anchor check below for completeness.
                if let Some(id) = el.attr("id") {
                    if referenced.contains(id) {
                        buf.push_str(" [anchor:");
                        buf.push_str(id);
                        buf.push(']');
                    }
                }
                return;
            }

            match tag {
                "strong" | "b" => {
                    let inner = render_inner_string(node, referenced).trim().to_string();
                    if !inner.is_empty() {
                        buf.push_str("**");
                        buf.push_str(&inner);
                        buf.push_str("**");
                    }
                    if let Some(id) = el.attr("id") {
                        if referenced.contains(id) {
                            buf.push_str(" [anchor:");
                            buf.push_str(id);
                            buf.push(']');
                        }
                    }
                    return;
                }
                "em" | "i" => {
                    let inner = render_inner_string(node, referenced).trim().to_string();
                    if !inner.is_empty() {
                        buf.push('*');
                        buf.push_str(&inner);
                        buf.push('*');
                    }
                    if let Some(id) = el.attr("id") {
                        if referenced.contains(id) {
                            buf.push_str(" [anchor:");
                            buf.push_str(id);
                            buf.push(']');
                        }
                    }
                    return;
                }
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    if !buf.ends_with('\n') && !buf.is_empty() {
                        buf.push('\n');
                    }
                    let level = tag[1..].parse::<usize>().unwrap_or(1).clamp(1, 6);
                    let inner = render_inner_string(node, referenced).trim().to_string();
                    if !inner.is_empty() {
                        for _ in 0..level {
                            buf.push('#');
                        }
                        buf.push(' ');
                        buf.push_str(&inner);
                        if let Some(id) = el.attr("id") {
                            if referenced.contains(id) {
                                buf.push_str(" [anchor:");
                                buf.push_str(id);
                                buf.push(']');
                            }
                        }
                        buf.push('\n');
                    }
                    return;
                }
                _ if BLOCK_TAGS.contains(&tag) => {
                    if !buf.ends_with('\n') && !buf.is_empty() {
                        buf.push('\n');
                    }
                    for child in node.children() {
                        render_node(child, buf, referenced);
                    }
                    if let Some(id) = el.attr("id") {
                        if referenced.contains(id) {
                            // Append after content but before final newline.
                            if buf.ends_with('\n') {
                                buf.pop();
                            }
                            buf.push_str(" [anchor:");
                            buf.push_str(id);
                            buf.push(']');
                            buf.push('\n');
                        } else if !buf.ends_with('\n') {
                            buf.push('\n');
                        }
                    } else if !buf.ends_with('\n') {
                        buf.push('\n');
                    }
                    return;
                }
                _ => {}
            }

            // Fallback: id-as-anchor for inline elements that didn't match a special case.
            if let Some(id) = el.attr("id") {
                if referenced.contains(id) {
                    let inner = render_inner_string(node, referenced).trim().to_string();
                    if !inner.is_empty() {
                        buf.push_str(&inner);
                        buf.push(' ');
                    }
                    buf.push_str("[anchor:");
                    buf.push_str(id);
                    buf.push(']');
                    return;
                }
            }

            // Default: just recurse.
            for child in node.children() {
                render_node(child, buf, referenced);
            }
        }
        _ => {
            for child in node.children() {
                render_node(child, buf, referenced);
            }
        }
    }
}

fn render_inner_string(
    node: ego_tree::NodeRef<scraper::Node>,
    referenced: &std::collections::HashSet<String>,
) -> String {
    let mut inner = String::new();
    for child in node.children() {
        render_node(child, &mut inner, referenced);
    }
    inner
}

// ATO URL parsing — port of extract.py:_doc_id_from_ato_link and helpers.
// We accept either ato.gov.au hosts or any URL whose path contains one of the
// ATO doc path hints. Recognised query params (case-insensitive): docid, locid,
// PiT, db. Recognised db values: HISTFT (amendment-history view).
const ATO_DOC_PATH_HINTS: &[&str] = &[
    "/law/view/document",
    "/law/view/view.htm",
    "/law/view.htm",
    "/atolaw/view.htm",
    "/view.htm",
];
const ATO_KNOWN_VIEWS: &[&str] = &["HISTFT"];

fn doc_id_from_ato_link(target: &str) -> Option<(String, Option<String>, Option<String>)> {
    let mut t = target.trim();
    if t.starts_with('<') && t.ends_with('>') && t.len() >= 2 {
        t = &t[1..t.len() - 1];
    }
    if let Some(idx) = t.find(' ') {
        t = &t[..idx];
    }
    // ATO often serves relative URLs in its own pages (e.g. href="/law/view/document?LocID=...").
    // url::Url::parse rejects those, so prepend the public origin to make
    // parsing succeed; the host check below still treats both absolute and
    // relative ATO paths consistently.
    let parsed = if t.starts_with('/') {
        let base = url::Url::parse("https://www.ato.gov.au").ok()?;
        base.join(t).ok()?
    } else {
        url::Url::parse(t).ok()?
    };
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let path_lower = parsed.path().to_ascii_lowercase();
    let is_ato_host = host.ends_with("ato.gov.au");
    let has_ato_path = ATO_DOC_PATH_HINTS
        .iter()
        .any(|hint| path_lower.contains(hint));
    if !(is_ato_host || has_ato_path) {
        return None;
    }
    let (mut raw, mut pit, mut view) = (None, None, None);
    for (k, v) in parsed.query_pairs() {
        let key_lc = k.to_ascii_lowercase();
        match key_lc.as_str() {
            "docid" | "locid" if raw.is_none() => {
                raw = Some(v.into_owned());
            }
            "pit" if pit.is_none() => {
                let s = v.trim().to_string();
                if !s.is_empty() {
                    pit = Some(s);
                }
            }
            "db" if view.is_none() => {
                let s = v.trim().to_ascii_uppercase();
                if ATO_KNOWN_VIEWS.iter().any(|kv| *kv == s) {
                    view = Some(s);
                }
            }
            _ => {}
        }
    }
    // SPA-style URLs hide the doc id in the fragment after a `?`.
    if raw.is_none() {
        if let Some(frag) = parsed.fragment() {
            if let Some(qpos) = frag.find('?') {
                let frag_query = &frag[qpos + 1..];
                for (k, v) in url::form_urlencoded::parse(frag_query.as_bytes()) {
                    let key_lc = k.to_ascii_lowercase();
                    match key_lc.as_str() {
                        "docid" | "locid" if raw.is_none() => {
                            raw = Some(v.into_owned());
                        }
                        "pit" if pit.is_none() => {
                            let s = v.trim().to_string();
                            if !s.is_empty() {
                                pit = Some(s);
                            }
                        }
                        "db" if view.is_none() => {
                            let s = v.trim().to_ascii_uppercase();
                            if ATO_KNOWN_VIEWS.iter().any(|kv| *kv == s) {
                                view = Some(s);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    let raw = raw?;
    // SPA category links carry a trailing `?` flag — drop those entirely.
    if raw.ends_with('?') {
        return None;
    }
    let doc_id = raw.trim().trim_matches('"').to_string();
    if doc_id.is_empty() || !doc_id.contains('/') {
        return None;
    }
    Some((doc_id, pit, view))
}

fn normalise_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

fn normalise_paragraph_breaks(s: &str) -> String {
    // Within each line, collapse runs of whitespace to one space.
    // Between lines, allow at most one blank line.
    let mut out_lines: Vec<String> = Vec::new();
    let mut last_blank = false;
    for line in s.split('\n') {
        let collapsed = normalise_ws(line);
        if collapsed.is_empty() {
            if !last_blank && !out_lines.is_empty() {
                out_lines.push(String::new());
            }
            last_blank = true;
        } else {
            out_lines.push(collapsed);
            last_blank = false;
        }
    }
    while out_lines.last().is_some_and(|l| l.is_empty()) {
        out_lines.pop();
    }
    out_lines.join("\n")
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
    if model_id == EMBEDDING_MODEL_ID {
        ensure_vector_search_ready(&conn)?;
        let embeddings: i64 =
            conn.query_row("SELECT COUNT(*) FROM chunk_embeddings", [], |r| r.get(0))?;
        println!("chunk_embeddings: {embeddings}");
        println!("semantic_search: ready");
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: i64,
    index_version: String,
    created_at: String,
    min_client_version: String,
    model: ModelInfo,
    documents: Vec<DocRef>,
    packs: Vec<PackInfo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelInfo {
    id: String,
    sha256: String,
    size: u64,
    url: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UpdateSummary {
    schema_version: i64,
    index_version: String,
    #[serde(default)]
    min_client_version: String,
    model: ModelInfo,
    document_count: usize,
    pack_count: usize,
    #[serde(default)]
    manifest_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DocRef {
    doc_id: String,
    content_hash: String,
    pack_sha8: String,
    offset: u64,
    length: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

const EMBEDDING_MODEL_HF_FILES: &[HfModelFile] = &[
    HfModelFile {
        path: "onnx/model_fp16.onnx",
        output_name: "model_fp16.onnx",
        sha256: "ee200de55cb2f94e858aabca54be7697a9c0805a14c858ee26ad0922b05f57d7",
        size: 200_792,
    },
    HfModelFile {
        path: "onnx/model_fp16.onnx_data",
        output_name: "model_fp16.onnx_data",
        sha256: "28d16e29cd623f25cc6fa0968700c5bc31036466091a5fa06d1353c1777f050e",
        size: 97_402_880,
    },
    HfModelFile {
        path: "tokenizer.json",
        output_name: "tokenizer.json",
        sha256: "feeb83348dcb033bc6b9d2e1f7906ca9eb2d122845000c9416d894d7c2927149",
        size: 2_128_614,
    },
];

#[derive(Clone, Copy, Debug, Default)]
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

/// Reject a manifest whose `schema_version` is not the current release
/// format, or whose `min_client_version` is newer than the currently-running
/// binary.
fn enforce_manifest_compatibility(manifest: &Manifest) -> Result<()> {
    // [CC-03] `ato-mcp update` and the `serve` startup availability probe
    // both gate on this. There is one current manifest schema; old or new
    // schemas are incompatible.
    let schema_version = manifest.schema_version;
    if schema_version < 0 {
        bail!("manifest schema_version is negative ({schema_version}); manifest is malformed");
    }
    let schema_version = schema_version as u32;
    if schema_version != SUPPORTED_MANIFEST_VERSION {
        bail!(
            "manifest schema_version {schema_version} is not supported by this binary (expects {SUPPORTED_MANIFEST_VERSION}); install a matching ato-mcp release"
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
    // [UM-05] Update flow: short-circuit via update.json summary when the
    // installed manifest still matches; otherwise rebuild the live DB
    // wholesale through a staging file and atomic rename. There is no
    // in-place delete+insert path — full rebuild on a fresh SQLite file
    // is faster than mutating the live multi-GB DB and avoids FK cascades
    // wiping the citations table mid-update.
    let manifest_context = UrlContext::from_manifest_url(manifest_url);
    if let Some(stats) = try_skip_update_from_summary(manifest_url, &manifest_context)? {
        return Ok(stats);
    }
    let staging = staging_dir()?;
    let manifest_bytes = fetch_bytes(manifest_url, &manifest_context)
        .with_context(|| format!("fetching manifest from {manifest_url}"))?;
    let new_manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
    enforce_manifest_compatibility(&new_manifest)?;

    // Cheap stats-only diff for the human-readable "+a ~c -r" CLI line.
    // No code path branches on the result — the rebuild always replaces
    // the live DB wholesale.
    let old_manifest = load_installed_manifest()?;
    let (added, changed, removed) = diff_manifests(old_manifest.as_ref(), &new_manifest);
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
    let staged_corpus = stage_live_db_from_manifest_at(
        &new_manifest,
        &manifest_context,
        manifest_bytes.len() as u64,
        added.len(),
        changed.len(),
        removed.len(),
        &update_root.join("corpus-rebuild"),
    )?;
    let stats = staged_corpus.stats;
    promote_staged_update(staged_model.as_ref(), staged_corpus, &new_manifest)?;
    let _ = fs::remove_dir_all(&update_root);
    Ok(stats)
}

#[derive(Debug)]
struct StagedModel {
    dir: PathBuf,
    marker_value: String,
}

struct ModelPromotionGuard {
    backup_dir: PathBuf,
    marker_value: String,
    active: bool,
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

struct PathPromotionGuard {
    live_path: PathBuf,
    backup_path: PathBuf,
    had_live: bool,
    active: bool,
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

    fn backup_path(&self) -> &Path {
        &self.backup_path
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

fn remove_path_if_exists(path: &Path) -> Result<()> {
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

struct StagedCorpusUpdate {
    staging_root: PathBuf,
    staged_db: PathBuf,
    staged_asset_root: PathBuf,
    stats: UpdateStats,
}

fn promote_staged_update(
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
    let assets_guard = promote_live_assets(
        &staged_corpus.staged_asset_root,
        &staged_corpus.staging_root.join("assets.backup"),
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
    persist_doctor_db_backup(db_guard.backup_path())?;
    if let Some(guard) = model_guard {
        guard.commit();
    }
    manifest_guard.commit();
    assets_guard.commit();
    db_guard.commit();
    let _ = fs::remove_dir_all(&staged_corpus.staging_root);
    Ok(())
}

fn live_model_file_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = EMBEDDING_MODEL_HF_FILES
        .iter()
        .map(|file| file.output_name)
        .collect();
    names.push(".model.sha256");
    names
}

fn backup_live_model_files(backup_dir: &Path) -> Result<()> {
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

fn restore_model_backup(backup_dir: &Path) -> Result<()> {
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

fn promote_staged_model_files(
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

fn stage_live_db_from_manifest_at(
    manifest: &Manifest,
    context: &UrlContext,
    manifest_bytes: u64,
    added: usize,
    changed: usize,
    removed: usize,
    staging_root: &Path,
) -> Result<StagedCorpusUpdate> {
    if staging_root.exists() {
        fs::remove_dir_all(staging_root)?;
    }
    fs::create_dir_all(staging_root)?;
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
        set_meta(&tx, "last_update_at", &Utc::now().to_rfc3339())?;
        // [UM-07] citations is a derived index of `[doc:X]` markers in
        // chunks.text. Newly-inserted chunks carry no citation rows; derive
        // them at the tail so the live DB always ships a populated table.
        derive_citations(&tx)?;
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

    Ok(StagedCorpusUpdate {
        staging_root: staging_root.to_path_buf(),
        staged_db,
        staged_asset_root,
        stats: UpdateStats {
            added,
            changed,
            removed,
            bytes_downloaded,
        },
    })
}

fn promote_live_db(staged_db: &Path, backup: &Path) -> Result<PathPromotionGuard> {
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

fn persist_doctor_db_backup(transient_backup: &Path) -> Result<()> {
    if !transient_backup.exists() {
        return Ok(());
    }
    let persistent_backup = backups_dir()?.join("ato.db.prev");
    let tmp = persistent_backup.with_extension("prev.tmp");
    let old = persistent_backup.with_extension("prev.old");
    remove_path_if_exists(&tmp)?;
    remove_path_if_exists(&old)?;
    fs::copy(transient_backup, &tmp).with_context(|| {
        format!(
            "writing persistent rollback backup {}",
            persistent_backup.display()
        )
    })?;
    replace_file_cross_platform(&tmp, &persistent_backup, &old)?;
    Ok(())
}

fn replace_file_cross_platform(src: &Path, dest: &Path, old: &Path) -> Result<()> {
    if dest.exists() {
        fs::rename(dest, old)
            .with_context(|| format!("moving {} to {}", dest.display(), old.display()))?;
    }
    if let Err(err) = fs::rename(src, dest) {
        if old.exists() {
            let _ = fs::rename(old, dest);
        }
        return Err(err)
            .with_context(|| format!("renaming {} to {}", src.display(), dest.display()));
    }
    let _ = remove_path_if_exists(old);
    Ok(())
}

fn promote_live_assets(staged_asset_root: &Path, backup: &Path) -> Result<PathPromotionGuard> {
    let live_assets = live_dir()?.join("assets");
    let guard = PathPromotionGuard::backup(live_assets.clone(), backup.to_path_buf())?;
    let staged_assets = staged_asset_root.join("assets");
    if staged_assets.exists() {
        if !staged_assets.is_dir() {
            bail!(
                "staged assets path is not a directory: {}",
                staged_assets.display()
            );
        }
        fs::rename(staged_assets, &live_assets)
            .with_context(|| format!("promoting assets to {}", live_assets.display()))?;
    } else {
        fs::create_dir_all(&live_assets)?;
    }
    if !live_assets.is_dir() {
        bail!(
            "promoted assets path is not a directory: {}",
            live_assets.display()
        );
    }
    Ok(guard)
}

fn promote_installed_manifest(manifest: &Manifest, backup: &Path) -> Result<PathPromotionGuard> {
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

fn semantic_backfill_required_for_model(conn: &Connection, model_id: &str) -> Result<bool> {
    if model_id != EMBEDDING_MODEL_ID {
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

/// Notice surfaced to the agent via `server_instructions` when the
/// published corpus is newer than the installed one. Constructed only
/// when an update is genuinely available; the field carries the newly
/// published `index_version` so the agent can mention it to the user.
struct UpdateAvailability {
    available_index_version: String,
}

fn http_probe_client() -> Result<Client> {
    // Tight budget: this client runs synchronously inside `serve` startup.
    // A slow network must not stall the MCP stdio loop — `serve` falls
    // through to no-notice if the probe doesn't complete in time.
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(5))
        .build()?)
}

fn fetch_bytes_probe(url_or_path: &str, context: &UrlContext) -> Result<Vec<u8>> {
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
    let client = http_probe_client()?;
    let mut resp = client.get(url_or_path).send()?.error_for_status()?;
    let mut out = Vec::new();
    resp.copy_to(&mut out)?;
    Ok(out)
}

/// Non-mutating availability probe. Returns `Some(UpdateAvailability)` only
/// when (a) an installed manifest is present, (b) the published `update.json`
/// is reachable inside the probe timeout, (c) it parses, (d) this binary can
/// still ingest it, and (e) its fingerprint does not match the installed
/// corpus. Every other case collapses to `Ok(None)` — no error path that
/// could stall serve startup. A published index that requires a newer binary
/// also returns `None` rather than emitting an "update available" notice the
/// user could not act on; the next manual `ato-mcp update` will surface the
/// real upgrade-the-binary error.
fn check_for_update_availability(manifest_url: &str) -> Result<Option<UpdateAvailability>> {
    // [SW-06] Synchronous startup probe with a tight 5s budget; every failure
    // path collapses to Ok(None) so a slow network cannot stall serve.
    if env_truthy("ATO_MCP_OFFLINE") {
        return Ok(None);
    }
    let Some(installed) = load_installed_manifest()? else {
        return Ok(None);
    };
    let context = UrlContext::from_manifest_url(manifest_url);
    let summary_url = update_summary_url_for_manifest(manifest_url);
    let summary_bytes = match fetch_bytes_probe(&summary_url, &context) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let summary: UpdateSummary = match serde_json::from_slice(&summary_bytes) {
        Ok(summary) => summary,
        Err(_) => return Ok(None),
    };
    if enforce_update_summary_compatibility(&summary).is_err() {
        return Ok(None);
    }
    if installed_matches_update_summary(&installed, &summary).unwrap_or(false) {
        return Ok(None);
    }
    Ok(Some(UpdateAvailability {
        available_index_version: summary.index_version,
    }))
}

fn enforce_update_summary_compatibility(summary: &UpdateSummary) -> Result<()> {
    let manifest = Manifest {
        schema_version: summary.schema_version,
        index_version: summary.index_version.clone(),
        created_at: String::new(),
        min_client_version: summary.min_client_version.clone(),
        model: summary.model.clone(),
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

// ----- tree-crawl (port of src/ato_mcp/scraper/tree_crawler.py + snapshot.py) -----

const SCRAPER_EXCLUDED_TITLES: &[&str] = &[
    "Archived document types",
    "Amending legislation",
    "Amending regulations",
    "Archived",
    "Full document",
    "View list of provisions",
    "Draft",
    "Draft amendments",
];

fn scraper_normalise_title(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
        .to_lowercase()
}

fn scraper_is_excluded(title: &str) -> bool {
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
struct SnapshotNode {
    uid: u64,
    parent_uid: Option<u64>,
    title: String,
    level: u32,
    node_type: String,
    data_url: Option<String>,
    href: Option<String>,
    canonical_id: Option<String>,
    path: Vec<String>,
    payload: JsonValue,
}

fn canonical_id_from(data_url: Option<&str>, href: Option<&str>) -> Option<String> {
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

fn fetch_nodes_blocking(
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

fn tree_crawl(
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
struct CanonicalEntry {
    canonical_id: String,
    title: Option<String>,
    href: Option<String>,
    representative_path: Vec<String>,
    occurrences: u64,
    folder_occurrences: std::collections::HashSet<String>,
    owner_folder: Option<String>,
}

#[derive(Debug, Default)]
struct FolderRecord {
    data_url: String,
    title: Option<String>,
    path: Vec<String>,
    parent_data_url: Option<String>,
    canonical_ids: std::collections::HashSet<String>,
    owned_ids: std::collections::HashSet<String>,
    redundant: bool,
}

fn is_better_path(candidate: &[String], incumbent: &[String]) -> bool {
    if incumbent.is_empty() {
        return true;
    }
    (candidate.len(), candidate) < (incumbent.len(), incumbent)
}

fn snapshot_reduce(nodes_path: &Path, output_dir: Option<&Path>) -> Result<()> {
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

struct LinkDownloadArgs {
    deduped_links: PathBuf,
    out_dir: PathBuf,
    base_url: String,
    request_delay_seconds: f64,
    max_workers: usize,
    timeout_seconds: f64,
    force: bool,
}

fn slug_for(text: &str, fallback: &str) -> String {
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

fn build_payload_path(out_dir: &Path, link: &JsonValue) -> PathBuf {
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

fn extract_law_contents(html: &str) -> Option<String> {
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

fn link_download(args: LinkDownloadArgs) -> Result<()> {
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

fn representative_path_from_docid(
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

fn doc_id_top_category(canonical_id: &str) -> Option<&'static str> {
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

fn load_canonical_ids(index_path: &Path) -> Result<std::collections::HashSet<String>> {
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

fn scrape_diff(
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

fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read as _;
    let mut hasher = Sha256::new();
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn bundle_localize_manifest(
    manifest_path: &Path,
    packs_dir: &Path,
    model_bundle: &Path,
) -> Result<()> {
    let mut manifest: JsonValue = serde_json::from_str(&fs::read_to_string(manifest_path)?)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;

    if let Some(packs) = manifest.get_mut("packs").and_then(|v| v.as_array_mut()) {
        for pack in packs {
            let url = pack
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let filename = std::path::Path::new(&url)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&url)
                .to_string();
            let pack_path = packs_dir.join(&filename);
            if !pack_path.exists() {
                bail!("manifest references missing pack: {}", filename);
            }
            pack["url"] = JsonValue::String(format!("packs/{filename}"));
            pack["sha256"] = JsonValue::String(sha256_file(&pack_path)?);
            pack["size"] =
                JsonValue::Number(serde_json::Number::from(fs::metadata(&pack_path)?.len()));
        }
    }

    let model_filename = model_bundle
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("model bundle has no filename"))?;
    if let Some(model) = manifest.get_mut("model") {
        model["url"] = JsonValue::String(model_filename.to_string());
        model["sha256"] = JsonValue::String(sha256_file(model_bundle)?);
        model["size"] =
            JsonValue::Number(serde_json::Number::from(fs::metadata(model_bundle)?.len()));
    }

    let manifest_typed: Manifest = serde_json::from_value(manifest.clone())
        .with_context(|| format!("validating {}", manifest_path.display()))?;
    let manifest_fingerprint = manifest_fingerprint(&manifest_typed)?;
    fs::write(manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

    let summary = json!({
        "schema_version": manifest.get("schema_version").cloned().unwrap_or(JsonValue::Null),
        "index_version": manifest.get("index_version").cloned().unwrap_or(JsonValue::Null),
        "min_client_version": manifest.get("min_client_version").cloned().unwrap_or(JsonValue::String(String::new())),
        "model": manifest.get("model").cloned().unwrap_or(JsonValue::Null),
        "document_count": manifest
            .get("documents")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        "pack_count": manifest
            .get("packs")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        "manifest_fingerprint": manifest_fingerprint,
    });
    let summary_path = manifest_path
        .parent()
        .map(|p| p.join("update.json"))
        .ok_or_else(|| anyhow!("manifest has no parent dir"))?;
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)?;

    eprintln!(
        "bundle-localize-manifest: rewrote {} + {}",
        manifest_path.display(),
        summary_path.display(),
    );
    Ok(())
}

// ----- publish-release (port of src/ato_mcp/indexer/release.py:publish) -----

const EMBEDDING_MODEL_HF_URL: &str =
    "hf://onnx-community/granite-embedding-small-english-r2-ONNX@1dc7835ba0cb9c76a3618d0bf0c427c97671b3c8";
const EMBEDDING_MODEL_HF_SIZE: u64 = 99_732_286;

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
    if !packs_dir.exists() {
        bail!("no packs/ dir at {}", packs_dir.display());
    }

    let mut pack_files: Vec<PathBuf> = fs::read_dir(&packs_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with("pack-") && s.ends_with(".bin.zst"))
        })
        .collect();
    pack_files.sort();
    if pack_files.is_empty() {
        bail!("no pack files found to upload");
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

    // Rewrite packs[].url in-place to GitHub release URLs.
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

    // Save updated manifest (sorted keys, indented).
    let pretty = serde_json::to_vec_pretty(&manifest)?;
    fs::write(&manifest_path, &pretty)?;

    // Generate update.json (UpdateSummary) so end-users can probe quickly.
    let summary = UpdateSummary {
        schema_version: manifest.schema_version,
        index_version: manifest.index_version.clone(),
        min_client_version: manifest.min_client_version.clone(),
        model: manifest.model.clone(),
        document_count: manifest.documents.len(),
        pack_count: manifest.packs.len(),
        manifest_fingerprint: Some(manifest_fingerprint(&manifest)?),
    };
    let summary_path = args.out_dir.join("update.json");
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)?;

    // [SL-07] Optional release signing shells out to the maintainer minisign
    // CLI and uploads the generated manifest.json.minisig artifact.
    // Sign manifest if --sign-key provided.
    let mut artifacts: Vec<PathBuf> = vec![manifest_path.clone(), summary_path.clone()];
    artifacts.extend(pack_files.iter().cloned());

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

fn model_info_matches(left: &ModelInfo, right: &ModelInfo) -> bool {
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

fn embedding_model_installed_matches(info: &ModelInfo) -> Result<bool> {
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
        if !is_http_url(url_or_path) {
            bail!("release asset not found: {url_or_path}");
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

fn validate_manifest_model_source(model: &ModelInfo) -> Result<()> {
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

fn parse_hf_model_url(value: &str) -> Option<(&str, &str)> {
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

fn stage_model(
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct PackChunk {
    ord: i64,
    #[serde(default)]
    anchor: Option<String>,
    text: String,
    #[serde(default)]
    embedding_b64: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
        let rowid: i64 = conn.query_row(
            "INSERT INTO chunks (doc_id, ord, anchor, text)
             VALUES (?, ?, ?, ?)
             RETURNING chunk_id",
            params![record.doc_id, chunk.ord, chunk.anchor, blob],
            |row| row.get(0),
        )?;
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
        )
        .with_context(|| {
            format!(
                "INSERT chunks_fts doc_id={} chunk_id={} ord={}",
                record.doc_id, rowid, chunk.ord
            )
        })?;
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

fn spawn_lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("spawn.lock"))
}

fn daemon_log_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("daemon.log"))
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
/// Called at the tail of `rebuild_live_db_from_manifest`. The rebuild path
/// bulk-inserts chunks into a fresh staging DB and then atomic-renames it
/// over the live file; freshly-inserted chunks carry no citation rows, so
/// every row must be derived here before the swap.
fn derive_citations(conn: &Connection) -> Result<()> {
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

fn load_cited_by(conn: &Connection, doc_id: &str) -> Result<(Vec<JsonValue>, i64)> {
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

const ATO_MCP_USE_INSTRUCTIONS: &str = r##"## ATO MCP Use Instructions

Use the ATO MCP as the primary retrieval layer for Australian tax research involving ATO-administered law, rulings, determinations, ATO IDs, practice statements, guidance, and related interpretive material. Treat it as an authority-finding and context-retrieval tool, not as a calculator or final legal reasoner.

The MCP is strongest for:
- Australian income tax, GST, FBT, PAYG, superannuation, consolidation, losses, CGT, R&D tax incentive, and ATO administrative guidance.
- Locating legislation, rulings, determinations, ATO IDs, practice statements, tax guidance, related documents, cited-by material, and document history links.
- Finding relevant law by section number, phrase, ruling identifier, or tax concept.

The MCP may need external supplementation for:
- Tax treaty existence and treaty rates.
- Annual thresholds, rates, and indexed amounts where the relevant year matters.
- Treasury explanatory memoranda, bills, second reading speeches, non-ATO regulator materials, TPB/TASA material, court judgments, or state taxes.
- Historical point-in-time versions where the MCP only exposes links to older versions rather than storing the old text itself.

### Tool Surface

1. `search`
- Use this first. Returns two parallel arrays: `hits` (chunk-level) and `title_hits` (document-level, up to 10).
- `hits` are slim chunk pointers — `chunk_id`, `doc_id`, anchor/title metadata, snippets, and flags showing whether related navigation may be useful. Fetch the actual text via `get_chunks`.
- `title_hits` is a sidebar of strongly-matching whole documents (ranked by title, plus exact `doc_id` / ATO-document-link lookups). Use it to recognise when a single authoritative document IS the answer, then `search` again with `doc_scope=<doc_id>` or read it chunk-by-chunk.
- Search works best with section numbers, defined terms, ruling IDs, and exact tax phrases.
- Default search is current-focused and excludes some older, withdrawn, private, or non-current material.
- Use `doc_scope` to restrict search to one document ID, or a prefix pattern like `<PREFIX>/%` for a document family.
- Use `current_only=false` and `include_old=true` when old, withdrawn, transitional, repealed, or historical material may matter.
- Use `similar_to_chunk_id` after finding a good chunk to locate semantically similar chunks without writing a new query (returns no `title_hits`).
- Use `seed_text` to do the same with arbitrary text rather than a corpus `chunk_id` — for example, paste a chunk returned by `fetch_external_doc` to pull related corpus material. The text is runtime-embedded as the query vector (returns no `title_hits`).

2. `get_chunks`
- Use this after `search` to retrieve the actual text of promising hits.
- Fetch by `chunk_id`.
- Request neighbouring chunks before and after the hit when statutory context, exceptions, calculation steps, examples, or notes may matter.
- Do not rely on snippets alone for final conclusions.

3. `get_doc_anchors`
- Use this when a hit indicates related navigation is available, or when the issue depends on document structure.
- Retrieves useful document anchors and navigation entries.
- Also surfaces related documents, history links, and `cited_by` documents where available.
- Use this to move from a single chunk to surrounding sections, related rulings, amendments, explanatory links, or later materials citing the document.

4. `fetch_external_doc`
- Use this when a `[doc:X]` marker in chunk text points at an id the local corpus does not index — subdivisions, paragraph-level references, footnote pointers, or historical point-in-time pointers (the `url` in `get_doc_anchors` historical_versions).
- Returns the live ATO document as the same `{ord, anchor, text}` chunk shape `search`/`get_chunks` use, so it reads like a corpus document.
- To pivot from an external chunk back into the corpus, pass its text to `search` as `seed_text`.
- Network-dependent and slower than the local corpus tools — prefer `search` for anything indexed.

### Standard Research Workflow

1. Start with targeted searches.
Prefer searches like `s 25-90 foreign branch income deduction`, `Subdivision 768-G active foreign business asset percentage`, `TR 2008/7 royalty withholding tax`, or `GIC deduction incurred after 1 July 2025`. Avoid relying only on broad natural-language searches like "is this deductible?" Scan both `hits` and `title_hits` — a strong `title_hits` entry often names the controlling document directly.

2. Open the best chunks.
Use `get_chunks` on multiple promising hits. Include neighbouring chunks where the answer may depend on nearby exceptions, formulas, definitions, or notes.

3. Follow the document structure.
If the result has document anchors, history, related docs, or cited-by flags, call `get_doc_anchors`. Tax answers often depend on adjacent provisions or related interpretive material. When a `[doc:X]` marker or a historical-version URL points outside the corpus, follow it with `fetch_external_doc`.

4. Confirm the full rule, not just the headline rule.
For each issue, check the operative rule, exceptions, definitions, timing rule, calculation method, rounding rule, transitional or commencement rule, interaction with other regimes, and administrative guidance where relevant.

5. For quantitative issues, retrieve the calculation provision.
Do not calculate from memory after finding only a general explanation. Confirm the statutory formula, rounding convention, caps, thresholds, rate year, and ordering rules.

6. For historical or date-sensitive issues, deliberately widen the search.
Use `current_only=false` and `include_old=true` where transitional provisions, commencement dates, historical thresholds or rates, repealed law, old ATO IDs, withdrawn guidance, acquisition dates, joining times, loss years, income years, or FBT years matter.

7. For multi-step regimes, search each step separately.
This is especially important for consolidation, tax losses, R&D tax incentive, CGT cost base modifications, foreign income / NANE rules, franking, debt/equity rules, withholding tax, GST input tax credit limits, and FBT exemptions and taxable value calculations.

8. For multiple-choice or issue-spotting tasks, test each plausible answer.
Search the concepts behind each suspicious option. Do not stop after proving one option sounds right; there may be a more specific rule.

### Reliability Rules

- Never cite a search snippet as authority. Retrieve the chunk text first.
- Never assume the first relevant hit is the controlling rule.
- If a provision has a formula, retrieve the formula.
- If a rule has a date, retrieve the commencement or transitional material.
- If an amount depends on a year, verify the threshold or rate for that exact year.
- If the MCP result is current law but the facts are historical, explicitly check whether historical law or transitional treatment matters.
- If the MCP does not clearly answer an issue, say so and use official external sources where appropriate.

### External Source Fallback

Use official sources first when the MCP is insufficient:
- ATO website and ATO legal database.
- Treasury, Federal Register of Legislation, explanatory memoranda, and bills.
- TPB for tax agent services / Code of Professional Conduct material.
- Official treaty databases or Treasury treaty pages for double tax agreements.
- Court or tribunal databases for case law.

When using external sources, prefer primary or official materials over commentary.

### Good Search Habits

Prefer searches like:
- `s 8-1 incurred presently existing liability`
- `s 110-45 Division 43 cost base reduction`
- `s 110-37 indexation cost base reduction`
- `s 23AH deduction foreign branch income`
- `s 25-90 debt deduction NANE income`
- `Subdivision 768-G active foreign business asset percentage rounding`
- `s 355-480 associate R&D payment`
- `s 355-315 balancing adjustment R&D`
- `s 701-30 non-membership period`
- `s 716-850 threshold gross up`
- `CGT event L4 allocable cost amount excess`
- `franking benchmark rule underfranking debit`

After finding a strong chunk, use `similar_to_chunk_id` to find neighbouring interpretive materials or related provisions that may not share the same wording.

### Output Discipline

When answering from MCP research:
- State the rule and the source type relied on.
- Apply the rule to the facts.
- Identify unresolved assumptions, especially dates, thresholds, rates, or calculation conventions.
- Separate legal conclusion from arithmetic.
- For material calculations, show the formula and inputs.
- Do not overstate certainty where the MCP produced nearby but not decisive authority."##;

fn server_instructions(update_notice: Option<&UpdateAvailability>) -> String {
    // [SW-02] Instructions are generated from live corpus stats.
    // [SW-03] Missing/unreadable stats fall back to static install guidance.
    let body = match stats()
        .ok()
        .and_then(|s| serde_json::from_str::<JsonValue>(&s).ok())
    {
        Some(s) => format!(
            "ATO legal corpus. Documents: {}, chunks: {}. Index: {}. Default search excludes Edited_private_advice, withdrawn rulings, and content dated before {} except legislation; override with current_only=false and include_old=true.\n\n{}",
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
            "description": "Hybrid semantic+lexical search over the ATO corpus. Returns two parallel result arrays: `hits` — chunk-level pointer hits (chunk_id, doc_id, anchor, optional snippet); fetch bodies via get_chunks. `title_hits` — up to 10 document-level hits ranked by title (no chunk_id), including exact doc_id / ATO-document-link lookups; treat as a sidebar of strongly-matching whole documents. doc_scope filters by full doc_id (in-doc search) or \"<PREFIX>/%\" (family). mode=keyword forces lexical-only; hybrid/vector require the semantic index. Set include_snippet=false when the caller will follow up with get_chunks. Pass similar_to_chunk_id to find chunks semantically close to one the agent already has (skips query encoding, ignores `query`, forces vector-only mode, filters the seed chunk out of results, and returns no title_hits). Pass seed_text to do the same with arbitrary text rather than a corpus chunk_id — e.g. a chunk returned by fetch_external_doc — to pull related corpus material; it is runtime-embedded as the query vector, forces vector-only mode, and returns no title_hits. similar_to_chunk_id wins if both are set. Hits in both arrays include navigation hints: has_in_doc_links (doc has paragraph anchors / contents entries — call get_doc_anchors to navigate), has_related_docs (doc has companion documents like errata / addenda), has_history (doc has earlier point-in-time versions — get_doc_anchors lists their URLs).",
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
                    "similar_to_chunk_id": {"type": "integer", "description": "When set, use this chunk's stored embedding as the query vector (skips encoding `query`, forces mode=vector, excludes the seed chunk from results, returns no title_hits)."},
                    "seed_text": {"type": "string", "description": "When set, runtime-embed this text as the query vector instead of `query` (e.g. a chunk from fetch_external_doc). Forces mode=vector, returns no title_hits. Ignored when similar_to_chunk_id is also set."},
                    "include_snippet": {"type": "boolean", "description": "When true (default), each hit carries a BM25-windowed snippet. Set false to omit the snippet field entirely — useful when the caller will fetch full text via get_chunks."},
                    "format": {"type": "string", "enum": ["json"], "default": "json"}
                },
                "required": ["query"]
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
            "description": "Return the navigation map for a document: in-doc anchors (paragraph references, contents-table entries), sister documents (errata, addenda, withdrawal notices), historical versions (earlier point-in-time publications), and reverse citations (other documents whose chunks carry a [doc:X] marker pointing AT this doc). Slim search hits surface `has_in_doc_links`, `has_related_docs`, or `has_history` when this tool would return useful entries — call it then to navigate. `in_doc` entries carry chunk_id (pass to get_chunks); `related_docs` carry doc_id (pass to search/get_chunks); `historical_versions` carry {doc_id, pit, date, url}; `cited_by` carries [{doc_id, title, type, date}] ordered by source date DESC and capped at 100 — when more citers exist, `cited_by_total` reports the full count. The corpus does not store historical content; use the historical-version `url` field with fetch_external_doc to retrieve an older version when needed.",
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
            "description": "Fetch chunk bodies by chunk_id. before/after expand the response with ordinal neighbour chunks within the same doc (0-20 each). Plaintext carries [doc:X] cross-reference markers (resolve via search, or fetch_external_doc when not indexed) and [asset:X] image markers (resolve via get_asset). On max_chars truncation, truncated_at + next_call point at the next chunk_id to continue scrolling.",
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
        },
        {
            "name": "fetch_external_doc",
            "description": "Fetch a document FROM ATO's live website by doc_id and return it as deterministic chunks — the same {ord, anchor, text} chunk shape `search`/`get_chunks` work with, so an external doc reads like a corpus doc. Use this when a [doc:X] marker in chunk text points at an id the local corpus doesn't index — typically subdivisions (PAC/<act>/SDiv*), paragraph-level references (PAC/<act>/<section>(N)), footnote pointers (.../fpN), or historical PiT-qualified pointers. URL is https://www.ato.gov.au/law/view/document?docid=<doc_id>[&PiT=<pit>][&db=<view>]. Stateless: the chunks are not persisted and carry no chunk_id; all of them are returned inline. Network-dependent and slower than local corpus tools — prefer search for anything indexed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "doc_id": {"type": "string"},
                    "pit": {"type": "string", "description": "Optional PiT timestamp (e.g. 99991231235958 for current view)."},
                    "view": {"type": "string", "description": "Optional db= view qualifier (e.g. HISTFT for amendment-history view)."}
                },
                "required": ["doc_id"]
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

    #[test]
    fn quantize_embedding_rejects_non_finite_values() {
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
        };
        let summary = UpdateSummary {
            schema_version: (SUPPORTED_MANIFEST_VERSION + 1) as i64,
            index_version: "test-future".to_string(),
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
            model: installed.model.clone(),
            document_count: 0,
            pack_count: 0,
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
        };
        let summary = UpdateSummary {
            schema_version: manifest.schema_version,
            index_version: manifest.index_version.clone(),
            min_client_version: manifest.min_client_version.clone(),
            model: manifest.model.clone(),
            document_count: 0,
            pack_count: 0,
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
        };
        let summary = UpdateSummary {
            schema_version: manifest.schema_version,
            index_version: manifest.index_version.clone(),
            min_client_version: manifest.min_client_version.clone(),
            model: manifest.model.clone(),
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
