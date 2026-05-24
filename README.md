# ato-mcp

Local search and retrieval over the Australian Taxation Office legal corpus.
Ships as a local HTTP MCP server with plugin metadata, a Rust binary, and a
one-shot corpus download.

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

## Install For An Agent

Agent flow from a fresh checkout:

```bash
git clone https://github.com/gunba/ato-mcp.git
claude plugin install ./ato-mcp
# start in the background from the agent's shell/tool
ato-mcp serve
```

The first `serve` picks a free local port, prints
`ato-mcp listening on http://127.0.0.1:<port>/mcp`, and rewrites `.mcp.json`
from `http://127.0.0.1:0/mcp` to the real URL. The agent tells the user to
exit and resume the agent session once so the MCP host reloads the generated
URL. The user should not have to choose a port, edit config, or run terminal
commands.

After the session resumes, the agent verifies:

```bash
ato-mcp stats
ato-mcp search "research and development tax incentive eligibility" --k 1
```

If the corpus is missing, the agent explains the large one-time download,
runs the update, restarts `ato-mcp serve`, and verifies again:

```bash
ato-mcp update
```

The plugin includes two agent skills:

- `ato-mcp-server`: small research skill loaded for ordinary ATO/tax queries.
- `setup-ato-mcp`: detailed install, timeout, port, and corpus-update recovery
  skill loaded only when setup or repair is needed.

For manual MCP clients, register the HTTP endpoint directly. Do not configure
`ato-mcp serve` as a stdio MCP command; it is an HTTP server.

## Updates

```bash
ato-mcp update
```

Full corpus replacement: the binary finds the newest release that includes a
corpus `manifest.json`, downloads `ato.db.zst`, verifies its sha256, and
atomic-renames it into the live data dir. The MCP server reads its corpus
once at startup, so restart the MCP client (or the `ato-mcp serve` process)
for a new corpus to take effect.

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
./target/release/ato-mcp publish-release --out-dir ./release --tag vX.Y.Z --repo gunba/ato-mcp --overwrite
```

`scripts/publish-release.sh <tag>` wraps the `package-corpus` +
`publish-release` steps. Binary archives publish on tag push. Corpus releases
attach `manifest.json` and `ato.db.zst`; `ato-mcp update` finds the newest
release that includes those corpus assets.

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
