# ato-mcp

Local search and retrieval over the Australian Taxation Office legal corpus.
Ships as a local MCP command with plugin metadata, a Rust binary, and a
one-shot corpus download. The MCP command starts or reuses one local HTTP
backend so the SQLite corpus and semantic model are shared per user.

> Retrieval infrastructure, not tax advice. Verify cited ATO material and
> apply professional judgment before relying on an answer.

## What you get

- A pre-built local corpus of ~158k ATO documents and ~467k chunks, queryable
  with hybrid BM25 + Granite vector search.
- Live retrieval for ATO documents the corpus doesn't carry.
- Statutory-definition lookup with an ordinary-meaning fallback.
- All of the above as MCP tools the agent can call directly.

## Tools

- `search`: hybrid semantic-plus-lexical search over the corpus, with current
  guidance defaults.
- `get_chunks`: fetch chunk bodies and optional neighbours by `chunk_id`.
  `[doc:X]` markers stay in-corpus; `[fetch:URI]` markers use `fetch`.
- `get_doc_anchors`: navigate in-document anchors, related/history links, and
  reverse citations.
- `get_definition`: statutory definitions with a labelled ordinary-meaning
  fallback.
- `get_asset`: resolve a retained `data-asset-ref` image and caption.
- `fetch`: live-fetch `ato:<doc_id>[?pit=...&view=...]` outside the corpus.
- `stats`: index version, counts, type codes, and default search policy.

Document bodies are exposed as cleaned HTML fragments so agents navigate the
source structure directly. Search chunks are plain text derived from that
HTML; heading paths live in metadata, links and images contribute only their
visible text.

## Install For An Agent

Install the platform archive from the matching `vX.Y.Z` release. Download the
archive and `SHA256SUMS`, verify the archive entry before extraction, then install
the executable and its packaged runtime library together in a stable per-user
directory on `PATH`:

```bash
grep ' ato-mcp-x86_64-unknown-linux-gnu.tar.gz$' SHA256SUMS | sha256sum -c -
mkdir -p "$HOME/.local/bin"
tmp=$(mktemp -d)
tar -C "$tmp" -xzf ato-mcp-x86_64-unknown-linux-gnu.tar.gz
install -m 0755 "$tmp/ato-mcp" "$HOME/.local/bin/ato-mcp"
install -m 0644 "$tmp/libonnxruntime.so" "$HOME/.local/bin/libonnxruntime.so"
test "$(command -v ato-mcp)" = "$HOME/.local/bin/ato-mcp"
ato-mcp --version
```

Use the macOS archive on Apple silicon. On Windows, verify with
`Get-FileHash`, install `ato-mcp.exe` and `onnxruntime.dll` together under
`%LOCALAPPDATA%\Programs\ato-mcp`, add that exact directory to the user `PATH`,
then verify `Get-Command ato-mcp` and `ato-mcp --version`. Never execute the
binary from a temporary extraction directory.

For Claude Code, register this checkout as a marketplace and install its named
plugin:

```bash
git clone https://github.com/gunba/ato-mcp.git
claude plugin marketplace add ./ato-mcp
claude plugin install ato-mcp@ato-mcp
```

Pi uses the same MCP command through `pi-mcp-adapter`. Install the adapter and
this checkout as a Pi package:

```bash
pi install npm:pi-mcp-adapter
pi install ./ato-mcp
```

The repository `.mcp.json` registers `ato-mcp mcp` for project-local clients.
For user-global Pi access from any project, copy that `mcpServers.ato` entry to
`~/.config/mcp/mcp.json` or `~/.pi/agent/mcp.json`. The plugin/package supplies
skills; it does not place the Rust executable on `PATH`.

`ato-mcp mcp` starts or reuses a local loopback backend, records its endpoint in
the user data directory, and proxies stdio MCP messages. There is no generated
port to configure. The binary and corpus locations are independent. Leave
`ATO_MCP_DATA_DIR` unset for the platform default, or set it to one stable
portable directory for `update`, `mcp`, `serve`, `stats`, and every verification
call.

After install, verify:

```bash
ato-mcp stats
ato-mcp search "research and development tax incentive eligibility" --k 1
```

If the corpus is missing, explain the large one-time download, obtain approval,
run `ato-mcp update`, restart the MCP host/backend, and verify again.

The plugin includes two agent skills:

- `ato-mcp-server`: small research skill loaded for ordinary ATO/tax queries.
- `setup-ato-mcp`: detailed install, timeout, port, and corpus-update recovery
  skill loaded only when setup or repair is needed.

For manual MCP clients, register `ato-mcp mcp` as the stdio MCP command. Do
not configure `ato-mcp serve` as a stdio MCP command; it is the backend HTTP
server.

```json
{
  "mcpServers": {
    "ato": {
      "command": "ato-mcp",
      "args": ["mcp"]
    }
  }
}
```

## Updates

```bash
ato-mcp update
```

Corpus discovery has one stable contract: query GitHub releases in API order,
ignore drafts and prereleases, and use the first release containing an asset
named exactly `manifest.json`. That manifest must use the current
`manifest.db` shape and reference `ato.db.zst` with its exact SHA-256 and size.
Manifest format 2 requires a versioned `ann` descriptor for `ato.ann`,
including its format, model, dimensions, corpus identity, vector-set digest,
size, and SHA-256. Legacy format-1 manifests without `ann` remain readable
through the exact vector scan. Format 2 makes ANN mandatory: semantic readiness
fails rather than silently using a stale, missing, or corrupt sidecar.
Publishers upload and verify `ato.db.zst`, then `ato.ann`, before uploading
`manifest.json`; the manifest is always the final discoverable asset.

The updater downloads and verifies `ato.db.zst` first and `ato.ann` second,
checks their shared corpus/vector identity, assembles an immutable generation,
then atomically replaces `active-generation` as the sole commit point. The
legacy `live/` layout remains readable until the first format-2 update. The server opens the corpus at startup,
so restart the MCP client/backend after an update. When initialize reports a
newer index, the agent surfaces the suggestion and updates only with approval.

## Search defaults

- `mode=hybrid` (default) combines Granite vector retrieval with BM25 ranking
  and suits natural-language queries. `mode=vector` uses semantic retrieval;
  `mode=keyword` uses BM25 bag-of-words matching. Hybrid and vector searches fail
  rather than silently downgrade when the semantic runtime cannot load.
- `types` accepts exact corpus codes or `*` globs. Discover current codes with
  `ato-mcp stats` or MCP `stats.types`; judgments and cases use `JUD`. Edited
  private advice (`EV`) is excluded unless `types` includes it.
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

Override with `ATO_MCP_DATA_DIR` for a deliberate portable or offline install
where every later `ato-mcp` command and server start will receive the same
environment variable. Do not point it at a temporary extraction directory; use
a stable install directory. If the corpus is installed with a non-default data
dir and `mcp` or `serve` is later started without that setting, the server
will look in the default user data directory and report a missing corpus.
Layout:

```text
ato-mcp/
├── live/
│   ├── ato.db
│   ├── model_fp16.onnx
│   ├── model_fp16.onnx_data
│   └── tokenizer.json
├── installed_manifest.json
├── http.json              # current local backend endpoint
├── server.log             # backend stderr when started by `ato-mcp mcp`
└── staging/               # transient during update
```

### Offline data bundle

`scripts/make-offline-bundle.sh` accepts the current `manifest.json`,
`ato.db.zst`, and required `ato.ann` release artifacts. Extract its archive directly into a stable data
directory, then keep using that path:

```bash
mkdir -p "$HOME/.local/share/ato-mcp"
zstd -dc ato-mcp-offline-bundle.tar.zst \
  | tar -C "$HOME/.local/share/ato-mcp" -xf -
ATO_MCP_DATA_DIR="$HOME/.local/share/ato-mcp" ato-mcp stats
```

When extracting into the platform default shown above, leave
`ATO_MCP_DATA_DIR` unset afterward. For any other directory, persist the same
value in the MCP host environment.

## Maintainer workflow

The Rust binary ships both the end-user product and the maintainer pipeline.
A maintainer build runs on a GPU box with the `cuda` Cargo feature:

```bash
cargo build --release --features cuda

./target/release/ato-mcp tree-crawl \
  --out-dir snapshots/$(date -u +%Y%m%dT%H%M%SZ)
./target/release/ato-mcp snapshot-reduce \
  --nodes-path snapshots/.../nodes.jsonl
./target/release/ato-mcp link-download \
  --deduped-links snapshots/.../deduped_links.jsonl \
  --out-dir /path/to/ato_pages
./target/release/ato-mcp build \
  --pages-dir /path/to/ato_pages --db-path ./release/ato.db \
  --model-dir /path/to/granite-embedding-small-r2 --out-dir ./release --profile
ATO_MCP_RELEASE_DIR="$PWD/release" \
  scripts/publish-release.sh vX.Y.Z gunba/ato-mcp
```

`scripts/publish-release.sh <tag>` packages `ato.db.zst`, uploads the database
and ANN sidecar first, and uploads `manifest.json` last. The manual binary workflow
checks out the supplied tag and requires it to equal `v<Cargo.toml version>`. The
Linux executable targets glibc 2.17 with `cargo-zigbuild`; the bundled ONNX Runtime
sets the complete Linux archive baseline to glibc 2.27. All release archives are
verified before upload and published with `SHA256SUMS`.

The self-hosted GPU workflow adds the corpus contract to the current latest
binary release. End-user discovery paginates GitHub releases until it finds the
newest non-prerelease `manifest.json`, so thousands of newer binary-only
releases cannot hide the current corpus. See [MAINTENANCE.md](MAINTENANCE.md)
for the publication runbook.

## Development

```bash
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo audit
cargo deny check advisories
scripts/smoke.sh
```

CI compiles and tests on Linux, macOS, and Windows; checks shell/package/plugin
contracts; exercises a packaged binary over stdio; and gates advisories.

## License

MIT. ATO content remains subject to the ATO's publication terms. Granite
Embedding Small English R2 is distributed under Apache-2.0.
