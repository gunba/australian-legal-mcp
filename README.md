# ato-mcp

Standalone MCP server for local search and retrieval over the Australian
Taxation Office legal corpus.

`ato-mcp` is retrieval infrastructure, not tax advice. Always verify cited
ATO material and apply professional judgment before relying on an answer.

The installed server is a Rust binary. End users do not need Python, pip,
pipx, uv, a compiler, `gh`, or an API key. The corpus is shipped as GitHub
release assets, while the EmbeddingGemma query encoder is downloaded from the
external URL recorded in the release manifest.

## Tools

| Tool | Purpose |
|---|---|
| `search` | Hybrid semantic-plus-lexical search over the GPU-built corpus. Defaults exclude Edited Private Advice and very old non-legislation content. |
| `search_titles` | Fast title lookup, plus exact `doc_id` and ATO document-link lookup. |
| `get_document` | Fetch cleaned source HTML for a document. Internal ATO document links are `data-doc-id` attributes. |
| `get_asset` | Resolve a retained image `data-asset-ref` to a local file path and source metadata. |
| `get_chunks` | Fetch exact chunks returned by `search`, with optional neighbor context. |
| `get_definition` | Fetch compact statutory definitions for a term, with labelled ordinary-meaning fallback when no statutory definition is found. |
| `stats` | Index version, counts, and default search policy. |

JSON results include the ATO `canonical_url`. Document bodies are exposed as
cleaned HTML fragments so agents can navigate source structure directly without
Markdown escaping or rendered-host assumptions. Search chunks are plain
semantic text derived from the cleaned HTML; heading paths are metadata, and
internal links/images contribute only useful visible text to search.

## Install

Download the binary for your platform from the latest release:

- Linux x64: `ato-mcp-x86_64-unknown-linux-gnu.tar.gz`
- macOS Apple Silicon: `ato-mcp-aarch64-apple-darwin.tar.gz`
- Windows x64: `ato-mcp-x86_64-pc-windows-msvc.zip`

Linux example:

```bash
mkdir -p ~/.local/bin
tar -xzf ato-mcp-x86_64-unknown-linux-gnu.tar.gz -C ~/.local/bin ato-mcp
ato-mcp init
ato-mcp doctor
ato-mcp stats
```

Windows: unzip `ato-mcp.exe` into a directory on `%PATH%`, then run:

```powershell
ato-mcp.exe init
ato-mcp.exe doctor
ato-mcp.exe stats
```

`init` downloads `manifest.json` and document packs from the configured
release URL. By default that is:

```text
https://github.com/gunba/ato-mcp/releases/latest/download
```

Override with `ATO_MCP_RELEASES_URL` for staging or an internal corporate
mirror. The Rust client intentionally does not read GitHub token
environment variables and does not shell out to `gh`. The embedding model
source is resolved from `manifest.model.url` and verified before use.

## Wire Into MCP Clients

Claude Code:

```bash
claude mcp add --scope user ato -- ato-mcp serve
claude mcp list
```

Claude Desktop:

```json
{
  "mcpServers": {
    "ato": {
      "command": "ato-mcp",
      "args": ["serve"]
    }
  }
}
```

Cursor, Continue, and other stdio MCP clients use the same command:

```text
ato-mcp serve
```

`serve` starts from the installed local corpus and does not check for updates on
the MCP hot path. This avoids stdio client spawn timeouts on slow or
TLS-inspecting corporate networks. Use `ato-mcp update` explicitly, or opt in to
a startup check with `ato-mcp serve --check-update` or `ATO_MCP_AUTO_UPDATE=1`.
`ATO_MCP_OFFLINE=1` always disables startup update checks.

## Search Defaults

Default search is tuned for current public tax-law work:

- `search` defaults to `mode=hybrid`, combining EmbeddingGemma vector retrieval
  with lexical ranking. `mode=vector` and explicit `mode=keyword` are available;
  hybrid/vector fail rather than silently downgrading when semantic search is
  unavailable.
- `Edited_private_advice` is excluded unless `types` explicitly includes it.
- Non-legislation documents dated before `2000-01-01` are excluded unless
  `include_old=true`.
- Legislation is not excluded by the old-content rule because current Acts
  often have old commencement dates.
- `get_definition` returns statutory definitions from the corpus definition
  index. If no statutory definition is found, it falls back to a labelled
  non-statutory ordinary meaning. By default the Rust client downloads and
  indexes Open English WordNet 2024 (CC-BY 4.0) into the local data directory
  on first use. Set `ATO_MCP_DICTIONARY_PATH` to a licensed JSON/JSONL/TSV
  dictionary export to use that source instead.

Examples:

```bash
ato-mcp search "R&D tax incentive eligibility" --k 5
ato-mcp search-titles "TR 2024 3"
ato-mcp search-titles "PAC/19970038/203-50"
ato-mcp get-document PAC/19970038/203-50 --format html --max-chars 20000
ato-mcp get-definition "corporate tax gross-up rate" --context-doc-id PAC/19970038/203-50
ato-mcp search "section 8-1 repairs" --mode keyword
ato-mcp search "royalties withholding old cases" --include-old --types Cases
```

## Updates

Run updates explicitly whenever you want to prefetch the latest published corpus
or verify the install:

```bash
ato-mcp update
ato-mcp doctor
```

The update path first checks the small `update.json` release summary. If the
installed corpus, schema, model, and reranker already match, it exits without
downloading the full manifest. When an update is needed, it downloads
`manifest.json`, diffs the installed manifest against the new manifest,
downloads only changed pack assets, mutates SQLite in one transaction, and
writes `installed_manifest.json` last. If an update fails, the previous
database snapshot is retained:

```bash
ato-mcp doctor --rollback
```

## Data Directory

Override the install location with `ATO_MCP_DATA_DIR`.

```text
Linux:   ~/.local/share/ato-mcp
macOS:   ~/Library/Application Support/ato-mcp
Windows: %APPDATA%\ato-mcp or the platform data directory
```

Layout:

```text
ato-mcp/
├── live/
│   ├── ato.db
│   ├── assets/
│   ├── model_quantized.onnx
│   ├── model_quantized.onnx_data
│   └── tokenizer.json
├── installed_manifest.json
├── backups/ato.db.prev
├── staging/
└── LOCK
```

## Maintainer Workflow

The Rust binary is the end-user product. Python remains maintainer tooling
for scraping, metadata extraction, vector generation, pack building, and
release publication.

Local GPU release build:

```bash
python -m venv .venv
.venv/bin/pip install -e '.[dev,gpu]'

.venv/bin/ato-mcp refresh-source \
  --mode incremental \
  --output-dir /path/to/ato_pages

LD_LIBRARY_PATH="$(find .venv/lib*/python3.*/site-packages/nvidia/ -maxdepth 2 -name lib -type d | tr '\n' ':')$LD_LIBRARY_PATH" \
  .venv/bin/ato-mcp build-index \
  --pages-dir /path/to/ato_pages \
  --out-dir ./release \
  --db-path ./release/ato.db \
  --model-path ./models/embeddinggemma/onnx/model_quantized.onnx \
  --tokenizer-path ./models/embeddinggemma/tokenizer.json \
  --gpu

.venv/bin/ato-mcp release \
  --out-dir ./release \
  --tag v0.3.0 \
  --repo gunba/ato-mcp \
  --model-dir ./models/embeddinggemma \
  --overwrite
```

Run the incremental What's New refresh immediately before every release build.
If it changes `ato_pages/index.jsonl`, rebuild from that refreshed source before
publishing.

`build-index` consumes local model files only to embed the corpus. Hosted model
and reranker distribution metadata is resolved in the `release` step, not in the
corpus build step.

For faster local rebuilds while tuning extraction/chunking, use a smaller
`--limit` smoke corpus first. `scripts/maintainer-sync.sh` also accepts
`ATO_MCP_BUILD_WORKERS`, `ATO_MCP_WINDOW_DOCS`, `ATO_MCP_ENCODE_BATCH_SIZE`,
`ATO_MCP_MAX_BATCH_TOKENS`, `ATO_MCP_CHECKPOINT_EVERY`,
`ATO_MCP_PACK_TARGET_MB`, and
`ATO_MCP_UNSAFE_FAST_SQLITE=1` for maintainer scratch builds. `--gpu` defaults
to larger embedding batches than CPU, with a conservative padded-token cap for
12 GB CUDA cards. Both fresh and previous-manifest builds
use windowed, length-bucketed embedding batches; unchanged documents still reuse
prior pack records. By default, full builds commit a resumable checkpoint every
20,000 prepared records, so a later extractor or embed failure loses only the
current window and a rerun skips already sealed documents. The build log prints
per-window prepare/embed/write timing, token throughput, batch size, and
approximate padded-token pressure so you can tune those values from evidence.

Long maintainer runs automatically inhibit system sleep when the host provides
`systemd-inhibit` or macOS `caffeinate`. `build-index` protects direct corpus
rebuilds, and `scripts/maintainer-sync.sh` protects the full scrape, rebuild,
and release flow. Set `ATO_MCP_ALLOW_SLEEP=1` only for short local checks where
sleep prevention is unwanted.

Release builds use EmbeddingGemma vectors and should run on the maintainer GPU.
The Rust end-user runtime does not require a GPU; query embedding and reranking
must continue to work on ordinary CPU-only laptops. The model is not uploaded to
GitHub Releases; by default the manifest points at pinned Hugging Face
EmbeddingGemma files, and the Rust client downloads and verifies them during
`init`, `update`, or an opted-in `serve --check-update` startup check. Pass
`--model-url` only for an approved mirror.
Corpus releases must come from `build-index`; DB-derived repack scripts are not
a supported release path. A full current corpus should use the current 64 MB
pack target, which is about a dozen pack assets rather than dozens of small
packs.
Explicit `mode=keyword` is a query-time FTS mode, not an alternative corpus
embedder. The optional `corpus release (gpu)` workflow targets a self-hosted
runner labelled `gpu` and fails if `nvidia-smi` or ONNX Runtime's
`CUDAExecutionProvider` is unavailable. It is not scheduled by default, so it
does not spend hosted GPU minutes.

## Development

```bash
cargo test --locked
.venv/bin/pytest -q
```

Published corpus/install smoke test:

```bash
ATO_MCP_MANIFEST_URL=https://.../manifest.json scripts/smoke-rust-install.sh
```

Offline bundles are materialized through the Rust installer:

```bash
ATO_MCP_RELEASE_DIR=./release/index-2026.05.02 \
ATO_MCP_MODEL_BUNDLE=/path/to/embeddinggemma-bundle.tar.zst \
scripts/make-offline-bundle.sh ./release/ato-mcp-offline-bundle.tar.zst
```

CI runs both the Rust binary checks and the Python maintainer test suite.
Release binary assets are produced by `.github/workflows/release-binaries.yml`.

## Corporate Windows Builds

Windows release binaries built from this repo use the Windows system TLS stack
(`native-tls`/SChannel), so HTTPS downloads trust corporate root CAs installed
in the OS certificate store. The Windows release zip includes `onnxruntime.dll`
next to `ato-mcp.exe`; the Windows build uses ORT dynamic loading to reduce the
executable footprint and avoid requiring MSVC for local source builds.

For local Windows source builds, put Microsoft's `onnxruntime.dll` next to the
built `ato-mcp.exe`, or set `ORT_DYLIB_PATH` to the DLL path before running
`ato-mcp`.

If building from source behind a TLS-inspecting proxy, Cargo may fail revocation
checks before it can fetch dependencies. Put this in `%USERPROFILE%\.cargo\config.toml`
when your corporate proxy blocks CRL access:

```toml
[http]
check-revoke = false
```

Aggressive endpoint protection can still block unsigned binaries based on local
policy. Building from source produces local-prevalence bytes, but a durable
fleet-wide fix for published Windows artifacts requires Authenticode signing.

## License

MIT. ATO content remains subject to the ATO's publication terms.
EmbeddingGemma remains subject to the Gemma Terms of Use.
