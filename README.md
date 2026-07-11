# Australian Legal MCP

Australian Legal MCP provides local, source-grounded search and retrieval over
Australian legal materials. The `australian-legal-mcp` package ships one Rust
executable, `legal-mcp`, which serves the MCP protocol, manages the local
corpus,
and contains the maintainer corpus pipeline.

> Legal research infrastructure, not legal or tax advice. Verify source
> material,
status and point-in-time applicability before relying on an answer.

## Current architecture

The installed service is local-first:

- source `ato` contains Australian Taxation Office legal and guidance material;
- source `frl` contains official Commonwealth legislation from the Federal
  Register of Legislation;
- one source-qualified SQLite corpus is stored as `legal.db`;
- semantic candidates come from `ann/<source>.ann`, then authoritative SQLite
  embeddings provide exact reranking;
- document bodies are cleaned structural HTML, while FTS and embeddings use
  plain source-derived text;
- `legal-mcp mcp` proxies stdio MCP to one shared loopback backend so each user
  data directory loads SQLite and the embedding model once.

`legal-mcp serve` runs that loopback backend directly for local testing. The
remote Streamable HTTP service and Azure deployment are roadmap work described
in [PLAN.md](PLAN.md).

## MCP tools

The MCP surface contains exactly seven tools:

1. `search` — source-scoped keyword, vector or hybrid search. Omitted `source`
   resolves to `ato`.
2. `get_chunks` — read source-qualified chunk references with optional
   neighbouring chunks and bounded continuations.
3. `get_asset` — resolve a retained source-qualified asset reference to image
   bytes and a caption.
4. `get_doc_anchors` — return in-document anchors, related/history links and
   reverse citations for a source-qualified document.
5. `get_definition` — find statutory definitions, with a labelled
   ordinary-meaning fallback when the corpus has none.
6. `stats` — report generations, source availability, counts, type codes and
   search policy.
7. `fetch` — retrieve a live document from a canonical `legal://` URI.

Search and retrieval responses preserve exact document, chunk, asset and corpus
generation references. Internal document links remain deterministic references;
external or uncatalogued ATO links become `[fetch:legal://…]` markers.

`fetch` accepts only the canonical form:

```text
legal://<source>/<percent-encoded-native-id>[?pit=<value>&view=<value>]
```

The native ID is one percent-encoded path segment and canonical percent escapes
use uppercase hexadecimal. ATO point-in-time and history requests can therefore
be expressed as:

```text
legal://ato/PAC%2F1?pit=20200101000000&view=HISTFT
```

## Install

Download the archive for the required platform from the
[latest release](https://github.com/gunba/australian-legal-mcp/releases/latest),
verify its entry in `SHA256SUMS`, and install `legal-mcp` together with the
packaged ONNX Runtime library in one stable directory on `PATH`. A Linux archive
is named like `legal-mcp-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz`; macOS and
Windows releases use their corresponding target triples.

For a Linux per-user install:

```bash
archive=legal-mcp-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz
grep " $archive$" SHA256SUMS | sha256sum -c -
tmp=$(mktemp -d)
tar -C "$tmp" -xzf "$archive"
install -Dm0755 "$tmp/legal-mcp" "$HOME/.local/bin/legal-mcp"
install -Dm0644 "$tmp/libonnxruntime.so" "$HOME/.local/bin/libonnxruntime.so"
legal-mcp --version
```

On Windows, keep `legal-mcp.exe` and `onnxruntime.dll` together in a stable
directory on the user `PATH`.

Clone the package when installing its MCP/plugin metadata:

```bash
git clone https://github.com/gunba/australian-legal-mcp.git
claude plugin install ./australian-legal-mcp
```

Pi uses the same MCP command through `pi-mcp-adapter`:

```bash
pi install npm:pi-mcp-adapter
pi install ./australian-legal-mcp
```

The canonical MCP registration is:

```json
{
  "mcpServers": {
    "australian-legal": {
      "command": "legal-mcp",
      "args": ["mcp"]
    }
  }
}
```

For a project-local client, the repository `.mcp.json` supplies this entry. A
user-global Pi installation can place the same `mcpServers.australian-legal`
entry in `~/.config/mcp/mcp.json` or `~/.pi/agent/mcp.json`.

## Install and verify the corpus

The corpus is a separate, integrity-checked download. Obtain approval before a
large first install, then run:

```bash
legal-mcp update
legal-mcp stats
legal-mcp search "research and development tax incentive eligibility" \
  --source ato --k 5
legal-mcp search "income tax assessment act" --source frl --k 5
```

Restart the MCP host or local backend after an update so it opens the activated
generation. The binary location and corpus location are independent; every
command must resolve the same data directory.

## Search policy

Search resolves exactly one source per request. `mode=hybrid` combines BM25 and
Granite vector retrieval, `mode=keyword` uses BM25, and `mode=vector` uses the
semantic index. Semantic modes fail clearly when the model or selected source
ANN sidecar cannot be validated.

ATO defaults remain current-guidance-first:

- edited private advice (`EV`) is selected only when `types` explicitly includes
  it;
- non-legislation material dated before `2000-01-01` is selected when
  `include_old=true`;
- legislation is exempt from that date cutoff;
- `current_only=true` filters withdrawn and superseded rulings.

`types` accepts exact source type codes or `*` globs. Discover available codes
and source status with `legal-mcp stats`. Judgments and cases use `JUD` where
that code is present in the selected source.

The ordinary-meaning fallback uses Open English WordNet 2024 under CC-BY 4.0.
Set `LEGAL_MCP_DICTIONARY_PATH` to a JSON, JSONL or TSV dictionary when a
different approved source is required.

## Data directory

Platform defaults are:

```text
Linux:   ~/.local/share/australian-legal-mcp
macOS:   ~/Library/Application Support/australian-legal-mcp
Windows: %APPDATA%\australian-legal-mcp
```

`LEGAL_MCP_DATA_DIR` selects a stable portable or offline location. Supply the
same value to `update`, `mcp`, `serve`, `stats`, searches and verification runs.
An installed generation has this canonical shape:

```text
australian-legal-mcp/
├── generations/
│   └── <generation-id>/
│       ├── legal.db
│       ├── ann/
│       │   ├── ato.ann
│       │   └── frl.ann
│       ├── model_fp16.onnx
│       ├── model_fp16.onnx_data
│       ├── tokenizer.json
│       └── installed_manifest.json
├── active-generation
├── http.json
├── server.log
└── staging/
```

Generation directories are immutable. `active-generation` is the atomic
activation point; `http.json` records the local backend endpoint used by
`legal-mcp mcp`.

## Corpus updates

`legal-mcp update` discovers the newest release containing `manifest.json`,
ignores draft and prerelease publications, verifies every declared digest and
size, assembles a complete immutable generation, and atomically activates it.
The manifest binds `legal.db`, the embedding model and every required
`ann/<source>.ann` sidecar to one corpus identity. Publishers make the manifest
discoverable only after every referenced artifact has been uploaded and
verified.

Maintainer acquisition is incremental, but every published database is a fresh
`legal.db`. Unchanged embeddings are reused by model and chunk-text hash;
changed source inventories are reconciled directly, and an authoritative
inventory deletes absent source documents within that source transaction.
Independent source failures retain the last publishable state for that source
while other source jobs complete.

See [MAINTENANCE.md](MAINTENANCE.md) for build and release operations,
[CURRENT_STATE.md](CURRENT_STATE.md) for the implementation snapshot, and
[PLAN.md](PLAN.md) for planned source and deployment work.

## Development

```bash
cargo fmt --all -- --check
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo audit
cargo deny check advisories
scripts/smoke.sh
```

Release corpus builds use `cargo build --release --features cuda` on a
maintainer machine with the approved local embedding model and a CUDA-enabled
ONNX Runtime. End-user install, update, search and serving remain CPU-safe.

## License

The software is MIT licensed. ATO and Federal Register source material remains
subject to the publishing authority's terms. Granite Embedding Small English R2
is distributed under Apache-2.0.
Embedding Small English R2 is distributed under Apache-2.0.
