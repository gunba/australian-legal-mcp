# Australian Legal MCP

Australian Legal MCP provides source-grounded search and retrieval across ten
official Australian legal sources. One Rust executable, `legal-mcp`, serves MCP,
searches a prebuilt immutable corpus, and contains the maintainer-only source and
corpus pipeline.

> Legal research infrastructure, not legal or tax advice. Verify source
> material, status, and point-in-time applicability before relying on an answer.

## Architecture

The production catalogue contains:

- `ato`, `frl`, `federal-court`, `high-court`, and `nsw-caselaw`;
- `nsw-legislation`, `qld-legislation`, `wa-legislation`,
  `sa-legislation`, and `tas-legislation`.

Every request selects exactly one registered source. Omission is an error; the
server never silently federates or defaults to ATO.

The local RTX maintainer PC owns acquisition, OCR, normalization, embedding,
ANN construction, validation, and rebuilds. It produces a complete generation
containing:

- one source-qualified SQLite `legal.db`;
- one deterministic `ann/<source>.ann` per source;
- the pinned ONNX model and tokenizer;
- `generation.json`, binding every file, source, vector set, schema, and model.

A validated generation is activated atomically and transferred directly over
SSH to a CPU-only serving VM. The serving runtime never downloads, scrapes,
embeds, packages, or publishes corpus/model artifacts. GitHub Releases are
binary-only.

Semantic search uses mdbr-leaf-ir ANN candidates followed by exact reranking
from authoritative normalized int8 vectors in SQLite. Bodies remain cleaned
structural HTML; FTS and embeddings use source-derived plain text.

For private production hosting, `legal-mcp.service` binds
`127.0.0.1:51235` behind Tailscale HTTPS. Local development and compatible MCP
hosts may instead run `legal-mcp mcp`, which starts or reuses a loopback backend.
See [DEPLOYMENT.md](DEPLOYMENT.md).

## Exactly seven MCP tools

1. `search` — source-scoped keyword, vector, or hybrid search.
2. `get_chunks` — read typed chunk references and bounded continuations.
3. `get_asset` — resolve retained source-qualified image assets.
4. `get_doc_anchors` — return in-document anchors, history/related links, and
   reverse citations.
5. `get_definition` — find statutory definitions, with a labelled ordinary
   meaning fallback.
6. `stats` — report generation, source, count, type, and search-policy status.
7. `fetch` — retrieve a live official document from a canonical legal URI.

Responses preserve typed `DocumentId`, `ChunkRef`, and `AssetRef` identities.
Internal links remain deterministic references. `fetch` accepts only:

```text
legal://<source>/<percent-encoded-native-id>[?pit=<value>&view=<value>]
```

The native ID is one percent-encoded path segment with uppercase hexadecimal
escapes, for example:

```text
legal://ato/PAC%2F1?pit=20200101000000&view=HISTFT
```

## Client installation

Binary releases contain only the executable, checksums, and the CPU ONNX Runtime
software library. Verify the archive against `SHA256SUMS`, then keep
`legal-mcp` and its ONNX Runtime library together in a stable directory on
`PATH`.

For local stdio development, clone and install the package metadata:

```bash
git clone https://github.com/gunba/australian-legal-mcp.git
claude plugin install ./australian-legal-mcp
```

Pi can use the same local adapter:

```bash
pi install npm:pi-mcp-adapter
pi install ./australian-legal-mcp
```

The local registration is:

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

A production client should instead use the private HTTPS `/mcp` endpoint
provisioned by the serving host. Do not publicly expose the loopback service or
replace OAuth/device identity with a shared static token.

## Search policy

`mode=hybrid` combines BM25 and vector retrieval; `mode=keyword` uses BM25;
`mode=vector` uses the semantic index. Semantic modes fail clearly when model or
ANN validation fails.

The ATO policy is current-guidance-first:

- edited private advice (`EV`) is included only when explicitly requested;
- non-legislation material before `2000-01-01` requires `include_old=true`;
- legislation is exempt from that date cutoff;
- `current_only=true` excludes withdrawn and superseded rulings.

`types` accepts exact source type codes or `*` globs. Use `stats` to discover
available source/type values. The ordinary-meaning fallback uses Open English
WordNet 2024 under CC-BY 4.0; `LEGAL_MCP_DICTIONARY_PATH` may select another
approved JSON, JSONL, or TSV dictionary.

## Immutable runtime generations

`LEGAL_MCP_DATA_DIR` selects only a runtime root. A serving root has this shape:

```text
runtime/
├── generations/
│   └── <generation-id>/
│       ├── generation.json
│       ├── legal.db
│       ├── ann/<source>.ann
│       ├── model.onnx
│       └── tokenizer.json
├── active-generation
├── LOCK
├── LIFECYCLE_LOCK
├── SERVER_LOCK
└── http.json              # local stdio/backend mode only
```

Generation directories are real, non-symlink, read-only directories.
`active-generation` is the atomic switch. Lifecycle commands are:

```bash
legal-mcp activate --generation-dir /same/filesystem/complete-generation
legal-mcp verify
legal-mcp rollback --generation <generation-id>
legal-mcp prune-generations --keep-inactive 1
```

There is no runtime `update`, corpus downloader, offline bundle, corpus package,
or GitHub corpus-release path.

## Maintainer data and builds

All persistent project data is ignored by Git and consolidated beneath
`data/`:

```text
data/
├── sources/             # ten current official-source workspaces
├── source-snapshots/    # rollback, failed refresh, and legacy stores
├── models/              # pinned unpacked model inputs
├── builds/              # resumable/inactive generation builds
├── runtime/             # locally active immutable generations
├── cache/               # disposable embedding/TensorRT acceleration
├── runs/                # acquisition state and durable pending work
├── logs/                # build, activation, and service evidence
├── validation/          # retained validation-only layouts
└── archive/             # non-canonical historical diagnostics
```

Each authoritative source workspace remains flat:
`state.json`, `documents/`, `assets/`, and temporary `staging/`. Full repairs
build a fresh complete source set, atomically exchange the whole set, and retain
the prior set under `source-snapshots/`. Corpus builds never scrape; they consume
only committed source workspaces.

Routine local build and activation:

```bash
cargo build --release --features cuda
scripts/maintainer-sync.sh
```

Fresh full repair:

```bash
scripts/maintainer-sync.sh --full
```

Direct deployment of the locally active generation:

```bash
scripts/deploy-generation.sh deploy@example-vps
```

The maintainer pipeline requires the pinned model in
`data/models/mdbr-leaf-ir-standard`, CUDA/TensorRT ONNX Runtime, Google
Chrome/Chromium for Federal Court, and `unrtf`, `antiword`, `soffice`,
`pdftotext`, `pdftoppm`, and `tesseract`. Serving is CPU-safe and requires none
of the acquisition or GPU tools.

## Development gates

```bash
cargo fmt --all -- --check
cargo test --locked --workspace --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo audit
cargo deny check advisories
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" scripts/smoke.sh
```

See [MAINTENANCE.md](MAINTENANCE.md) for maintainer operations,
[CURRENT_STATE.md](CURRENT_STATE.md) for the implementation snapshot, and
[DEPLOYMENT.md](DEPLOYMENT.md) for low-cost private hosting.

## License

The software is MIT licensed. Source material remains subject to each official
publishing authority's terms. MongoDB mdbr-leaf-ir is Apache-2.0 licensed.
