# ato-mcp

Standalone MCP server for local search and retrieval over the Australian
Taxation Office legal corpus.

`ato-mcp` is retrieval infrastructure, not tax advice. Always verify cited
ATO material and apply professional judgment before relying on an answer.

The installed server is a Rust binary. End users do not need Python, pip,
pipx, uv, a compiler, `gh`, or an API key. The corpus is shipped as GitHub
release assets, while the Granite embedding query encoder is downloaded from the
external URL recorded in the release manifest.

## Tools

| Tool | Purpose |
|---|---|
| `search` | Hybrid semantic-plus-lexical search over the GPU-built corpus. Defaults exclude Edited Private Advice and very old non-legislation content. |
| `get_doc_anchors` | Return in-document anchors, related documents, historical-version URLs, and reverse citations for a corpus document. |
| `get_chunks` | Fetch exact chunks returned by `search`, with optional neighbor context. |
| `get_definition` | Fetch compact statutory definitions for a term, with labelled ordinary-meaning fallback when no statutory definition is found. |
| `get_asset` | Resolve a retained image `data-asset-ref` to a local file path and source metadata. |
| `fetch_external_doc` | Fetch a live ATO document by `doc_id` and optional point-in-time parameters when a referenced document is outside the local corpus. |
| `stats` | Index version, counts, and default search policy. |

JSON results include the ATO `canonical_url`. Document bodies are exposed as
cleaned HTML fragments so agents can navigate source structure directly without
Markdown escaping or rendered-host assumptions. Search chunks are plain
semantic text derived from the cleaned HTML; heading paths are metadata, and
internal links/images contribute only useful visible text to search.

## Install

`ato-mcp` is shipped as a Rust binary — there is no `pip install ato-mcp`
and no Python is needed for end users. The flow is:

1. **Download the release binary** for your platform from the GitHub
   releases page at `https://github.com/gunba/ato-mcp/releases/latest`.

   - Linux x64: `ato-mcp-x86_64-unknown-linux-gnu.tar.gz`
   - macOS Apple Silicon: `ato-mcp-aarch64-apple-darwin.tar.gz`
   - Windows x64: `ato-mcp-x86_64-pc-windows-msvc.zip`

2. **Extract and put on `PATH`.**

   Linux/macOS:

   ```bash
   mkdir -p ~/.local/bin
   tar -xzf ato-mcp-*.tar.gz -C ~/.local/bin ato-mcp
   ```

   Windows: unzip `ato-mcp.exe` (and the bundled `onnxruntime.dll`) into a
   directory on `%PATH%`.

3. **Wire `ato-mcp serve` into your MCP client** (Claude Code / Claude
   Desktop / Cursor / Codex / Continue / any stdio MCP host — see next
   section). `ato-mcp serve` is a thin stdio shim: on first launch it
   auto-spawns the persistent HTTP daemon in the background, then proxies
   stdin/stdout to it. Every subsequent MCP session reuses the same
   daemon, so the embedding model and corpus index only load once across
   the whole machine. There is no daemon to start manually and no port
   to remember. On first use, the MCP server tells the assistant that
   the corpus has not been installed yet and asks the user to run
   `ato-mcp update` in their terminal. The download is ~4 GB and takes
   1–10 minutes on a typical home connection (longer behind a corporate
   proxy — see
   [Enterprise / corporate environments](#enterprise--corporate-environments)).
   After it completes, restart the MCP client so it picks up the new
   corpus.

You can also run `ato-mcp update` manually before wiring the server in;
`ato-mcp doctor` and `ato-mcp stats` will verify the install.

### Enterprise / corporate environments

The published binaries are unsigned today (no Authenticode / no notarised
macOS bundle). On a managed endpoint expect one or more of the following
to delay or block first run:

- **EDR / endpoint protection** (Defender, CrowdStrike, SentinelOne,
  Carbon Black, Sophos, …) may hold the binary in a cloud-sandbox queue
  for minutes-to-hours before allowing execution, or quarantine it
  outright. If `ato-mcp` disappears after extraction or fails to launch
  with no error, check your endpoint console. IT typically resolves this
  by either waiting out the analysis, adding a per-hash allow rule, or
  flagging the binary as known-good.

- **Windows SmartScreen / macOS Gatekeeper** will show an "unidentified
  developer" or "unrecognized program" warning on the first launch.
  Users without override privilege need IT to whitelist the executable
  hash, or build from source on-prem.

- **TLS-inspecting proxies** terminate and re-sign HTTPS. The Windows
  binary uses `native-tls`/SChannel so it trusts the corporate root CA
  in the OS certificate store automatically. The Linux/macOS binaries
  use `rustls` with the built-in Mozilla bundle; if your proxy re-signs
  with a private CA, set `SSL_CERT_FILE` to a bundle that includes that
  CA before running `ato-mcp update`.

- **Egress allow-list.** `ato-mcp update` fetches from two hosts:
  `github.com` (release manifest + pack assets) and `huggingface.co`
  (Granite embedding model). Both need to be reachable. The release URL
  base can be overridden with `ATO_MCP_RELEASES_URL` to point at an
  internal mirror; the model URL is recorded in `manifest.model.url` and
  can be redirected at release time with `--model-url`.

- **No GitHub token needed.** `ato-mcp` deliberately does not read
  `GITHUB_TOKEN` or shell out to `gh`. Public releases are fetched
  anonymously. For private mirrors, expose them through a plain HTTPS
  URL that doesn't require auth, or pre-stage the install via the
  offline-bundle path documented under [Development](#development).

If your team can't run unsigned binaries at all, build from source on a
maintainer machine (`cargo build --release` from this repo) and
distribute the resulting binary internally. Building from source is
straightforward; signing the resulting artifact is the
organisation-specific bit. Maintainer corpus rebuilds are a separate
maintainer flow and not required for fresh installs — end users always
consume pre-built corpus releases from GitHub.

## Wire Into MCP Clients

`ato-mcp serve` is a stdio MCP entry point — the same shape Claude
Code, Cursor, Codex, and Continue expect. Internally it's a thin shim
that auto-launches a persistent HTTP daemon in the background and
proxies stdin/stdout to it. The first MCP session on the machine spawns
the daemon; every subsequent session reuses it. There is nothing to
start manually and no port to configure.

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

Codex (`~/.codex/config.toml`):

```toml
[mcp_servers.ato]
command = "ato-mcp"
args = ["serve"]
```

Cursor, Continue, and other stdio MCP clients use the same command.

The shim handles daemon lifecycle automatically:

- First `ato-mcp serve` call picks a free port in the ephemeral range,
  persists it to `~/.local/share/ato-mcp/http.json` (macOS:
  `~/Library/Application Support/ato-mcp/http.json`; Windows:
  `%APPDATA%\ato-mcp\http.json`), and spawns the HTTP daemon detached
  from the shim process.
- Subsequent shims detect the live daemon and proxy to it directly.
- If the daemon dies, the next request transparently re-spawns it.
- Concurrent shim launches are serialised through
  `<data_dir>/spawn.lock` so two parallel sessions don't race the bind.

For the always-on systemd / launchd shape, see `systemd/README.md` —
that lets the daemon come up at login so the first MCP request never
pays spawn cost. It's optional; the shim works the same without it.

### Advanced: connect directly over HTTP

Power users who want to bypass the shim (for example, an MCP client
that natively speaks Streamable HTTP, or a remote tunnel) can talk to
the daemon directly. Pick or read the port and point the client at it:

```bash
ato-mcp install-http                    # prints the URL and config block
```

```json
{
  "mcpServers": {
    "ato": {
      "type": "http",
      "url": "http://127.0.0.1:51234/mcp"
    }
  }
}
```

In this mode you also need the daemon running independently — either
`ato-mcp daemon` in a terminal or the systemd unit.

`serve` starts immediately from whatever local corpus is present and never
downloads on the MCP hot path, so it cannot trip stdio client spawn timeouts
on slow or TLS-inspecting corporate networks. When a newer corpus index has
been published, the server tells the assistant via `initialize` instructions
and the assistant asks the user to run `ato-mcp update`. `ATO_MCP_OFFLINE=1`
disables the update-availability probe entirely.

## Search Defaults

Default search is tuned for current public tax-law work:

- `search` defaults to `mode=hybrid`, combining Granite vector retrieval
  with lexical ranking. `mode=vector` and explicit `mode=keyword` are available;
  hybrid/vector fail rather than silently downgrading when semantic search is
  unavailable.
- Edited private advice (`EV`) is excluded unless `types` explicitly includes it.
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
ato-mcp search "TR 2024 3" --k 5
ato-mcp search "PAC/19970038/203-50" --k 5
ato-mcp fetch-external-doc PAC/19970038/203-50
ato-mcp get-definition "corporate tax gross-up rate" --context-doc-id PAC/19970038/203-50
ato-mcp search "section 8-1 repairs" --mode keyword
ato-mcp search "royalties withholding old cases" --include-old --types Cases
```

## Updates

`ato-mcp update` is the one command that both installs the corpus on a fresh
machine and refreshes it later:

```bash
ato-mcp update
ato-mcp doctor
```

Update first fetches the small `update.json` release summary. If the installed
corpus, schema, and model still match, it exits without downloading the
manifest. When anything has changed, it downloads `manifest.json`, fetches
the pack assets, rebuilds the live SQLite database in a staging file, and
atomically renames it over the live DB — leaving the previous database in
`backups/ato.db.prev` for rollback:

```bash
ato-mcp doctor --rollback
```

While a server is already running, a newer index does not take effect until
the MCP client is restarted, since `serve` loads the corpus once at startup.

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
│   ├── model_fp16.onnx
│   ├── model_fp16.onnx_data
│   └── tokenizer.json
├── installed_manifest.json
├── backups/ato.db.prev
├── staging/
└── LOCK
```

## Maintainer Workflow

The Rust binary is the end-user product **and** the maintainer tool. The
Python pipeline that used to do scraping, metadata extraction, vector
generation, pack building, and release publication has been retired —
all of it now ships as `ato-mcp` subcommands.

Local GPU release build:

```bash
cargo build --release --features cuda

./target/release/ato-mcp scrape-diff \
  --index         /path/to/ato_pages/index.jsonl \
  --whats-new-url https://www.ato.gov.au/law/view/whatsnew.htm?fid=whatsnew \
  --out           /tmp/whats_new_pending.jsonl

./target/release/ato-mcp link-download \
  --deduped-links /tmp/whats_new_pending.jsonl \
  --out-dir       /path/to/ato_pages

./target/release/ato-mcp build \
  --pages-dir /path/to/ato_pages \
  --db-path   ./release/ato.db \
  --model-dir /path/to/granite-embedding-small-r2 \
  --base-release-dir ./release/.latest \
  --out-dir   ./release \
  --gpu \
  --profile

./target/release/ato-mcp publish-release \
  --out-dir ./release \
  --tag     v0.8.0 \
  --repo    gunba/ato-mcp \
  --overwrite
```

For full crawls (rare, hours):

```bash
./target/release/ato-mcp tree-crawl --out-dir snapshots/$(date -u +%Y%m%dT%H%M%SZ)
./target/release/ato-mcp snapshot-reduce --nodes-path snapshots/.../nodes.jsonl
./target/release/ato-mcp link-download --deduped-links snapshots/.../deduped_links.jsonl --out-dir /path/to/ato_pages
```

`scripts/maintainer-sync.sh` wraps all three modes (`incremental`,
`catch_up`, `full`) and handles release publication. Drive it from the
provided `systemd/ato-mcp-maintainer-weekly.service` unit on a GPU host.
The script validates `release/.latest` before using it, restores the pointer
from another local compatible corpus release when possible, and can
materialize a compatible published corpus release locally from manifest and
pack assets without running the embedding model. If a build was interrupted,
the script can also resume a matching checkpointed output directory instead
of starting a new full embedding pass after the date tag changes.

Release builds use Granite embedding vectors and should run on the
maintainer GPU. The Rust end-user runtime does not require a GPU; query
embedding must continue to work on ordinary CPU-only laptops. The model is not
uploaded to GitHub Releases; by default the
manifest points at pinned Hugging Face Granite embedding files, and the
Rust client downloads and verifies them during `ato-mcp update`. The
model URL can be redirected at release time via `--model-url` /
`--model-sha256` / `--model-size`; non-Hugging Face mirrors must supply
both hash and size.

Corpus releases must come from `ato-mcp build`; DB-derived repack
scripts are not a supported release path. The optional
`corpus release (gpu)` workflow targets a self-hosted runner labelled
`gpu` and fails if `nvidia-smi` is unavailable. It is not scheduled by
default, so it does not spend hosted GPU minutes.

## Development

```bash
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
```

Offline bundles are materialized through the Rust installer:

```bash
ATO_MCP_RELEASE_DIR=./release/index-2026.05.02 \
ATO_MCP_MODEL_BUNDLE=/path/to/semantic-model-bundle.tar.zst \
scripts/make-offline-bundle.sh ./release/ato-mcp-offline-bundle.tar.zst
```

CI runs the Rust binary checks. Release binary assets are produced by
`.github/workflows/release-binaries.yml`.

## Corporate Windows Builds from Source

For from-source Windows builds (the binary release already bundles
everything needed), a few extras matter on managed networks.

Microsoft's `onnxruntime.dll` is not vendored into the Cargo build. Put
the DLL next to your built `ato-mcp.exe`, or set `ORT_DYLIB_PATH` to its
path before running. The published Windows release zip already includes
this DLL.

If building from source behind a TLS-inspecting proxy, Cargo may fail
revocation checks before it can fetch dependencies. Put this in
`%USERPROFILE%\.cargo\config.toml` when your corporate proxy blocks CRL
access:

```toml
[http]
check-revoke = false
```

End-user concerns (Authenticode signing, SmartScreen, EDR holds, proxy
allow-listing) are covered in the [Enterprise / corporate
environments](#enterprise--corporate-environments) subsection above.

## License

MIT. ATO content remains subject to the ATO's publication terms.
Granite Embedding Small English R2 is distributed under Apache-2.0.
