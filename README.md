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

A validated generation is activated atomically and deployed to an external
XFS/reflink corpus volume on an Akamai Cloud (Linode) VPS. Restricted rsync
seeds from a CoW clone and transmits only changed blocks. The serving runtime
never scrapes, embeds, builds, or publishes corpus/model artifacts. GitHub
Releases remain binary-only; a separately attested GHCR image contains runtime
software but no corpus or model artifacts.

Semantic search exactly scans a source-scoped mmap flat int8 sidecar through one
bounded four-thread pool, then reranks selected candidates from authoritative
normalized int8 vectors in SQLite. Bodies remain cleaned
structural HTML; FTS and embeddings use source-derived plain text. Schema 11
stores chunk keyword postings in a contentless-delete FTS5 table while
authoritative chunk text remains in `chunks`.

For hosted operation, a non-root, read-only OCI container publishes only
`127.0.0.1:51235` behind host Caddy. Public `/mcp` requires individually
revocable API keys, exact single-tenant Entra bearer validation, or both; hosted
startup fails closed without authentication. The same image/volume contract can
later move to Azure. Local development may run `legal-mcp mcp`. See
[DEPLOYMENT.md](DEPLOYMENT.md) and [MICROSOFT_COPILOT.md](MICROSOFT_COPILOT.md).

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

Binary releases contain the executable, checksums, CPU ONNX Runtime library,
licenses, and the version-matched hosting bundle. Verify the archive against
`SHA256SUMS`, then keep
`legal-mcp` and its ONNX Runtime library together in a stable directory on
`PATH`. Release archives target Linux x86-64 (glibc 2.27+), macOS arm64, and
Windows x86-64. Windows requires the current Microsoft Visual C++ 2015–2022
Redistributable because the official ONNX Runtime DLL imports `MSVCP140` and
`VCRUNTIME140`; this dependency is not bundled. Run `legal-mcp verify-runtime`
from the extracted directory before installation to prove the packaged shared
library loads on that host.

The hosted Streamable HTTP service is the default client path; it does not
require the Australian Legal MCP plugin or a local corpus. See
[CLIENT_SETUP.md](CLIENT_SETUP.md) for tested setup instructions for Pi, Claude
Code, Codex, ChatGPT, Claude custom connectors, the OpenAI Responses API, and
Microsoft Copilot. The live validation endpoint currently uses individually
revocable API keys; cloud connectors and end-user enterprise integrations must
use the documented OAuth/Entra path.

Local maintainers may still register the binary directly over stdio:

```json
{
  "mcpServers": {
    "australian-legal-local": {
      "command": "legal-mcp",
      "args": ["mcp"]
    }
  }
}
```

Local stdio requires the complete local runtime generation selected by
`LEGAL_MCP_DATA_DIR`. Never expose container port 51235 publicly.

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
├── lifecycle/
│   ├── active-generation
│   ├── LOCK
│   └── LIFECYCLE_LOCK
└── state/
    ├── SERVER_LOCK
    ├── http.json          # local stdio/backend mode only
    └── server.log         # local background backend only
```

Generation directories are real, non-symlink, read-only directories.
`lifecycle/active-generation` is the atomic switch. Lifecycle commands are:

```bash
legal-mcp activate --generation-dir /same/filesystem/complete-generation
legal-mcp verify
legal-mcp rollback --generation <generation-id>
legal-mcp prune-generations --keep-inactive 1
```

There is no runtime `update`, corpus downloader, offline bundle, corpus package,
or GitHub corpus-release path.

Software is version 0.19.11. Exact document-scoped lexical searches narrow FTS
work to that document while glob, percent-wildcard, case-insensitive, and
missing scopes preserve their established semantics. The active local generation is chunker-format-6
v22 `937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939`.
It contains 409,528 documents, 6,986,040 chunks/embeddings, and 20,169
definitions in schema 11. Its 19,758,231,552-byte `legal.db` has SHA-256
`c8e77a7dbf61a8b185592c07bb47b0cc324bfc2cce2b9e2663f5c4716483b851`;
ten source-scoped flat-int8 sidecars total 1,816,430,592 bytes. Strict CPU
verification and the HarbourGrid retrieval/latency evaluation pass. The rebuild
preserves typed FRL formula images through internal-link rewriting, and exact
chunk-text hashes reuse authoritative vectors from the previous generation.

V22 is the current hosted corpus; v20 Arroy is its sole hosted rollback. The v19
schema-10 parent remains installed locally with its matching v0.18.1
binary/image as a disaster-recovery fallback; the schema-11 binary deliberately
rejects schema 10.

Maintainers may project an exact immutable schema-10 generation without a
source or model rebuild:

```bash
target/release/legal-mcp derive-schema11-from-schema10 \
  --source-generation-dir "$PWD/data/runtime/generations/<schema-10-generation>" \
  --expected-source-generation <schema-10-generation> \
  --out-dir "$PWD/data/builds/<fresh-schema-11-candidate>"
```

This command uses SQLite FTS tokenization to rebuild only chunk FTS storage. It
does not acquire sources, run OCR, rechunk, tokenize for the model, execute the
model, re-embed, or rebuild sidecars. It accepts only generations already using
the current flat format. The separate maintainer-only
`derive-flat-int8-from-schema11-arroy-v20` command is the one-shot, strictly
validated conversion path for the immutable schema-11 v20 Arroy generation; it
derives the flat sidecars exclusively from authoritative SQLite int8 vectors.

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

CoW/delta deployment of the locally active generation:

```bash
scripts/deploy-generation.sh \
  --host legal-mcp-publisher@HOST
```

Flat-int8 v22
`937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939`
is active on the Linode with exact v0.19.11 host tools and independently
verified image
`ghcr.io/gunba/australian-legal-mcp@sha256:43be03afbdd78c509053200d0f61b35a1519e9d95f303b917f8023f4ae2a7470`.
The v0.19.10 bridge retired the exact v0.19.8 recovery
transaction while keeping `/run` `noexec`; the corrected paired cutover then
retired its own journal. Arroy v20 remains as the sole hosted rollback
generation.

Private and public HarbourGrid, all-seven-tool/all-ten-source retrieval, exact
Caddy routes, live empty capability sets, API-key revocation, and reboot
recovery passed on v22. Current client key IDs are `local-pi` and
`work-laptop`; `enterprise-laptop` and `second-client` are revoked. The current
Pi install passed an authenticated v22 stats call. Plaintext credentials are
not stored in this repository or the Obsidian vault.

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
python3 -m unittest \
  tests/test_azure_generation_transport.py \
  tests/test_configure_azure_host.py \
  tests/test_manage_api_keys.py \
  tests/test_remote_mcp.py \
  tests/test_render_microsoft_integrations.py
# Validate infra/linode with the lock-pinned Linode provider.
tofu -chdir=infra/linode init -backend=false -lockfile=readonly
tofu -chdir=infra/linode validate
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" scripts/smoke.sh
```

See [CLIENT_SETUP.md](CLIENT_SETUP.md) for client and enterprise Obsidian/Pi
setup, [docs/validation](docs/validation/README.md) for benchmark and HarbourGrid
validation evidence, [MAINTENANCE.md](MAINTENANCE.md) for maintainer operations,
[CURRENT_STATE.md](CURRENT_STATE.md) for the implementation snapshot,
[DEPLOYMENT.md](DEPLOYMENT.md) for Akamai/Linode OCI hosting, and
[MICROSOFT_COPILOT.md](MICROSOFT_COPILOT.md) for Entra/Copilot onboarding.

## License

The software is MIT licensed. Source material remains subject to each official
publishing authority's terms. MongoDB mdbr-leaf-ir is Apache-2.0 licensed.
