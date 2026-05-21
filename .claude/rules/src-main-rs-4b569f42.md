---
paths:
  - "src/main.rs"
---

# src/main.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Granite Embedding Model
Granite ONNX semantic runtime: CPU by default, optional CUDA for maintainer builds, 1024-token dynamic padding, sentence_embedding or mean-pooling, 256-d int8 vectors.

- [EM-05 L71] Stored semantic vectors use the first EMBEDDING_DIM=256 dimensions of the Granite output before normalisation and int8 quantisation.
- [EM-03 L74] The tokenizer truncates semantic inputs at EMBEDDING_INPUT_MAX_TOKENS=1024 and pads dynamically to the batch max sequence length.
- [EM-02 L77] Granite embedding inputs use source-derived text directly; EMBEDDING_TEXT_PREFIX is empty, so neither stored chunk bodies nor runtime queries get query/passage prompt prefixes.

## Rust CLI Commands
Closed clap command surface covering end-user MCP/update/doctor/search commands plus maintainer source, build, and release commands in the Rust binary.

- [CC-01 L108] One Rust binary owns both end-user commands (serve, install-http, update, doctor, stats, search, retrieval helpers) and maintainer-only source, build, and release commands; AGENTS.md documents which maintainer commands require checkout/source/model/GPU.
- [CC-06 L111] The CLI surface is a closed clap enum: every external command is declared in Command, with no dynamic plugin subcommands or generated shell-completion surface.
- [CC-05 L192] Source acquisition and corpus building are separate commands: source commands populate ato_pages/index.jsonl, while build requires pages-dir/model-dir/db-path/out-dir and can reuse a base release; the same pages tree can feed repeated builds.

## Rust Server Wiring
MCP tool registration, shared ServerState, runtime statistics instructions, install/update notices, and the small explicit tool surface.

- [SW-04 L680] ServerState lazily loads SemanticRuntime on the first semantic query and reuses that runtime for the rest of the process.
  - There is no reranker state in the MCP surface; non-semantic tools do not load the semantic runtime.
- [SW-02 L1811] Server instructions are built dynamically at start time from corpus stats (doc count, chunk count, type breakdown, meta keys), so the agent sees up-to-date corpus shape without restart-time configuration.
- [SW-03 L1812] server_instructions is built from stats(OutputFormat::Json); if stats cannot be read (corpus not yet installed) it returns a static install message telling the agent to ask the user to run ato-mcp update. When the serve-startup probe has stashed an UpdateAvailability on ServerState, both branches append a newer-index-available notice carrying the published index_version.
- [SW-01 L1840] Seven MCP tools are exposed by tool_descriptors/call_tool: search, get_chunks, get_definition, get_asset, get_doc_anchors, fetch, and stats.
  - The surface stays small and explicit; unsupported tools fail through the normal tools/call error path.

## Rust Source Scraper
Maintainer source acquisition commands for What's New incremental pulls, tree crawl snapshots, snapshot reduction, deduped catch-up, and paced link download.

- [SS-04 L265] Default maintainer source-download pacing is 0.05s, and link-download defaults to max_workers=4; workers parse/write concurrently while the shared delay lock serializes HTTP issuance.

## Rust Storage Layer
SQLite schema, compressed chunk/html storage, FTS5, WAL write handles, pack/assets install, optional minisign release signatures, doc anchors, and derived citations.

- [SL-07 L916] publish-release optionally signs manifest.json by shelling out to the maintainer minisign CLI, then uploads manifest.json.minisig with the release artifacts.

## Rust Update Mechanism
End-user update flow: update.json fast-path when local DB/model match, otherwise staged model/corpus rebuild and guarded promotion, with single-writer LOCK and doctor rollback backup.

- [UM-04 L1080] fetch helpers intentionally don't read GitHub token env vars and don't shell out to gh — private release assets must be exposed through an approved mirror or installed from a local/offline bundle. This keeps the end-user runtime credential-free.
