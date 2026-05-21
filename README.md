# ato-mcp

Local search and retrieval over the Australian Taxation Office legal corpus.
Ships as a Claude Code plugin that bundles an MCP server, a Rust binary, and
a one-shot corpus download.

> Retrieval infrastructure, not tax advice. Verify cited ATO material and
> apply professional judgment before relying on an answer.

## What you get

- A pre-built local corpus of ~158k ATO documents and ~467k chunks, queryable
  with hybrid BM25 + Granite vector search.
- Live retrieval for ATO documents the corpus doesn't carry, plus AustLII
  case law and legislation via the `fetch` and `search_austlii` tools.
- Statutory-definition lookup with an ordinary-meaning fallback.
- All of the above as MCP tools the agent can call directly.

## Tools

| Tool | Purpose |
|---|---|
| `search` | Hybrid semantic-plus-lexical search over the corpus. Defaults exclude edited private advice and pre-2000 non-legislation content. |
| `get_chunks` | Fetch chunk bodies by `chunk_id`, with optional neighbour context. `[doc:X]` markers point into the corpus and resolve via `get_chunks` / `get_doc_anchors`; `[fetch:URI]` markers point outside the corpus and resolve via `fetch`. |
| `get_doc_anchors` | In-document anchors, related documents, historical-version URLs, and reverse citations for a corpus document. |
| `get_definition` | Statutory definitions with a labelled ordinary-meaning fallback. |
| `get_asset` | Resolve a retained image `data-asset-ref` to a local file path. |
| `fetch` | Live-fetch a document by URI. `ato:<doc_id>[?pit=...&view=...]` for ATO live retrieval; `austlii:<path>` for AustLII via `classic.austlii.edu.au`. Returns chunks of the same shape as `get_chunks`. Pass `allow_ocr=true` for scanned PDFs (Tesseract on `$PATH`, allow ~120s). |
| `search_austlii` | Live search of AustLII via SINO. Returns hits with `fetch_uri` ready to pass to `fetch`. Uses the `cf_clearance` session acquired by `ato-mcp austlii setup`. |
| `stats` | Index version, counts, default search policy, AustLII session state. |

Document bodies are exposed as cleaned HTML fragments so agents navigate the
source structure directly. Search chunks are plain text derived from that
HTML; heading paths live in metadata, links and images contribute only their
visible text.

## Install

The plugin is installed through Claude Code:

```bash
git clone https://github.com/gunba/ato-mcp.git
claude plugin install ./ato-mcp
```

The plugin's `.mcp.json` points at `http://127.0.0.1:51234/mcp`, so the agent
needs the local HTTP server running. Start it from a terminal:

```bash
ato-mcp serve              # default port 51234
ato-mcp serve --port 51235 # if 51234 is in use
```

On first start the server tells the agent the corpus isn't installed yet; the
agent will offer to run `ato-mcp update`, which downloads `ato.db.zst` (~4 GB,
5–10 min) from the latest GitHub release and atomic-swaps it into place. Once
the download finishes, restart the MCP client (or just reconnect) so it picks
up the new corpus.

## Updates

```bash
ato-mcp update
```

Full corpus replacement: the binary fetches the published `manifest.json`,
downloads the new `ato.db.zst`, verifies its sha256, and atomic-renames it
into the live data dir. The MCP server reads its corpus once at startup, so
restart the MCP client (or the `ato-mcp serve` process) for a new corpus to
take effect.

When a newer corpus is published, the server's `initialize` instructions tell
the agent — the agent surfaces the suggestion to the user and runs the update
when the user agrees.

## AustLII

ATO commentary cites AustLII material that lives outside the local corpus.
The `fetch` tool reaches `classic.austlii.edu.au` directly for case and
legislation URLs. `search_austlii` reaches SINO, which Cloudflare gates with
a JS challenge — that needs a clearance cookie from your real browser:

```bash
ato-mcp austlii setup                       # opens AustLII in your browser, reads cf_clearance
ato-mcp austlii setup --cookie "<value>"    # manual paste fallback (Safari, EDR-locked endpoints)
ato-mcp austlii clear                       # delete the persisted session
```

`stats` reports the persisted session (browser, cookie age, cf_clearance
presence). Override the auto-detected default browser with
`ATO_MCP_BROWSER=chrome|edge|firefox` if the registry / xdg-mime lookup
returns the wrong one.

Scanned pre-digital judgments return `error: scanned_pdf` from `fetch` by
default. Pass `allow_ocr=true` to opt into Tesseract OCR (results are cached
at `<data_dir>/ocr_cache/<sha256>.txt`). The response carries `ocr_used:
true` and an `ocr_warning` so the agent can tell the user to verify against
the canonical source.

## Search defaults

- `mode=hybrid` (default) combines Granite vector retrieval with BM25 ranking.
  `mode=vector` and `mode=keyword` are also available; both fail rather than
  silently downgrade when the semantic runtime can't load.
- Edited private advice (`EV`) is excluded unless `types` includes it.
- Non-legislation documents dated before 2000-01-01 are excluded unless
  `include_old=true`. Legislation is exempt from the cutoff because current
  Acts often have old commencement dates.
- `get_definition` returns statutory definitions from the corpus index, with
  a labelled non-statutory ordinary-meaning fallback. The fallback uses Open
  English WordNet 2024 (CC-BY 4.0), downloaded on first use. Point
  `ATO_MCP_DICTIONARY_PATH` at a JSON/JSONL/TSV file to use a different
  source.

## Data directory

```text
Linux:   ~/.local/share/ato-mcp
macOS:   ~/Library/Application Support/ato-mcp
Windows: %APPDATA%\ato-mcp
```

Override with `ATO_MCP_DATA_DIR`. Layout:

```text
ato-mcp/
├── live/
│   ├── ato.db
│   ├── assets/
│   ├── model_fp16.onnx
│   ├── model_fp16.onnx_data
│   └── tokenizer.json
├── installed_manifest.json
├── austlii_session.json   # only when `austlii setup` has run
└── staging/               # transient during update
```

## Maintainer workflow

The Rust binary ships both the end-user product and the maintainer pipeline.
A maintainer build runs on a GPU box with the `cuda` Cargo feature:

```bash
cargo build --release --features cuda

./target/release/ato-mcp tree-crawl   --out-dir snapshots/$(date -u +%Y%m%dT%H%M%SZ)
./target/release/ato-mcp snapshot-reduce --nodes-path snapshots/.../nodes.jsonl
./target/release/ato-mcp link-download   --deduped-links snapshots/.../deduped_links.jsonl --out-dir /path/to/ato_pages
./target/release/ato-mcp build           --pages-dir /path/to/ato_pages --db-path ./release/ato.db --model-dir /path/to/granite-embedding-small-r2 --out-dir ./release --profile
./target/release/ato-mcp package-corpus  --db-path ./release/ato.db --out ./release/ato.db.zst --manifest ./release/manifest.json
./target/release/ato-mcp publish-release --out-dir ./release --tag v0.13.0 --repo gunba/ato-mcp --overwrite
```

`scripts/publish-release.sh <tag>` wraps the `package-corpus` +
`publish-release` steps. Releases live on a single rolling tag: binary
archives publish on tag push, and the maintainer GPU host attaches
`manifest.json` and `ato.db.zst` to the same tag. End users hit
`releases/latest/download/manifest.json`.

## Development

```bash
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
scripts/smoke.sh
```

CI runs build, clippy, and tests on Linux. Release binaries are built by
`.github/workflows/release-binaries.yml`.

## License

MIT. ATO content remains subject to the ATO's publication terms. Granite
Embedding Small English R2 is distributed under Apache-2.0.
