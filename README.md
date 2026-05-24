# ato-mcp

Local search and retrieval over the Australian Taxation Office legal corpus.
Ships as a Claude Code plugin that bundles an MCP server, a Rust binary, and
a one-shot corpus download.

> Retrieval infrastructure, not tax advice. Verify cited ATO material and
> apply professional judgment before relying on an answer.

## What you get

- A pre-built local corpus of ~158k ATO documents and ~467k chunks, queryable
  with hybrid BM25 + Granite vector search.
- Live retrieval for ATO documents the corpus doesn't carry.
- Statutory-definition lookup with an ordinary-meaning fallback.
- All of the above as MCP tools the agent can call directly.

## Tools

| Tool | Purpose |
|---|---|
| `search` | Hybrid semantic-plus-lexical search over the corpus. Defaults exclude edited private advice and pre-2000 non-legislation content. |
| `get_chunks` | Fetch chunk bodies by `chunk_id`, with optional neighbour context. `[doc:X]` markers point into the corpus and resolve via `get_chunks` / `get_doc_anchors`; `[fetch:URI]` markers point outside the corpus and resolve via `fetch`. |
| `get_doc_anchors` | In-document anchors, related documents, historical-version URLs, and reverse citations for a corpus document. |
| `get_definition` | Statutory definitions with a labelled ordinary-meaning fallback. |
| `get_asset` | Resolve a retained image `data-asset-ref` to an MCP image content item plus caption. |
| `fetch` | Live-fetch an ATO document by URI: `ato:<doc_id>[?pit=...&view=...]`. Returns chunks of the same shape as `get_chunks`. |
| `stats` | Index version, counts, and default search policy. |

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

The plugin's `.mcp.json` ships with `http://127.0.0.1:0/mcp` as a sentinel
— `:0` means "the server will pick a port on first run." Start the server
from a terminal:

```bash
ato-mcp serve
```

On first run, `serve` picks a free port, binds it, and writes the actual
URL back into the plugin's `.mcp.json` so Claude Code can find it. **Exit
and resume the Claude Code session** so it re-reads the updated config.
Subsequent runs reuse the same port from `.mcp.json`; pass `--port <N>`
to force a different binding.

If you ever see "ATO MCP tools unavailable" the agent will offer to start
the server for you via the plugin's skill. Same flow — start the server,
exit and resume the session.

For Codex, register the HTTP endpoint directly. Use a fixed port so the
Codex config remains stable across restarts:

```bash
ato-mcp serve --port 34893
codex mcp add ato --url http://127.0.0.1:34893/mcp
```

Do not configure Codex with `command = "ato-mcp"` and `args = ["serve"]`.
`serve` is an HTTP MCP server; launching it as a stdio MCP server causes
Codex to wait for stdio initialization until it times out.

On first start the server tells the agent the corpus isn't installed yet;
the agent will offer to run `ato-mcp update`, which downloads `ato.db.zst`
(~1.5 GB, 5–10 min) from the latest GitHub release and atomic-swaps it into
place. After the download completes, restart `ato-mcp serve` so it picks
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
│   ├── model_fp16.onnx
│   ├── model_fp16.onnx_data
│   └── tokenizer.json
├── installed_manifest.json
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
./target/release/ato-mcp publish-release --out-dir ./release --tag v0.15.1 --repo gunba/ato-mcp --overwrite
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
