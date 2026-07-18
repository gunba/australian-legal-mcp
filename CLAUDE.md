# Australian Legal MCP guidelines

Australian Legal MCP is source-grounded legal retrieval over prebuilt immutable
generations. The package is `australian-legal-mcp`, executable `legal-mcp`, MCP
key `australian-legal`, and environment prefix `LEGAL_MCP_*`.

Acquisition, OCR, embedding, ANN construction, and builds run on the local RTX
maintainer host. Validated generations are activated locally and can be
CoW-seeded and rsynced by changed blocks to an external XFS/reflink volume on
the Akamai/Linode host. A corpus-free OCI image serves and validates them. The
runtime never scrapes, embeds, builds, packages, or publishes corpus/model
artifacts. GitHub Releases contain software binaries only. V20 is active on the
current Linode, but authentication is disabled, `legal-mcp.service` is
inactive, Caddy is disabled/inactive, UFW 80/443 are closed, and there is no
deployment, auth, or image transaction or upload authorization. The host is not
serving.

Persistent project data is
`data/{sources,source-snapshots,models,builds,runtime,cache,runs,logs,validation,archive}`.
`LEGAL_MCP_DATA_DIR` selects a runtime root only.

## Public contract

Expose exactly seven tools: `search`, `get_chunks`, `get_asset`,
`get_doc_anchors`, `get_definition`, `stats`, and `fetch`. Every source-scoped
request requires exactly one registered source. Public references are typed and
source-qualified; chunk references include the generation. Live fetch accepts
only canonical `legal://<source>/<percent-encoded-native-id>` URIs.

Tool responses contain only actionable legal-research data: identity, title,
type, date/currency metadata, exact stored canonical URL, heading path, snippet,
and navigable typed references. Keep scores, model identifiers, candidate
counts, ordinals, echoed queries, diagnostics, and debug counters internal.
Omit empty optional values.

Preserve cleaned structural HTML and useful attributes. Internal links/assets
become deterministic typed references. Derive FTS/embedding text from visible
source content, retaining headings as metadata.

## Engineering choices

Prefer typed deterministic logic from official structure. Keep source volatility
inside acquisition/normalization adapters. Use one path for identities, storage,
ANN, continuations, generation validation, activation, rollback, and pruning.
Do not add compatibility surfaces or guessed mappings.

Use deterministic completion signals, durable journals, locks, and atomic
same-filesystem operations. Bound queues, bodies, decompression, retries,
timeouts, concurrency, and blocking work. Installed generations are immutable;
`active-generation` is the only switch.

## Source and corpus rules

Current authoritative workspaces are `data/sources/<source>`. ATO retains the
pinned What's New path, 50 ms issue interval, adaptive ten-request ceiling, and
30-second timeout. FRL uses the official API, authoritative title/version
ordering, and EPUB → DOCX → PDF rendition preference. Other adapters use only
official publisher surfaces. Federal Court protected document hosts alone use
bounded Chrome CDP.

Run source jobs independently. A broad source failure aborts that source and
preserves its last committed state. Full repairs build a fresh complete source
set and atomically exchange the whole set. Builds consume committed workspaces
only and always create a fresh `legal.db` generation. Reuse vectors solely by
exact model ID and chunk-text hash.

Every generation binds SQLite, the pinned model/tokenizer, and one deterministic
ANN sidecar per source. ANN finds candidates; SQLite int8 vectors provide exact
authoritative reranking. Schema 11 uses contentless-delete `chunks_fts`, keeps
authoritative text in `chunks`, and digest-binds FTS postings/BM25 metadata.

The one supported schema migration is `derive-schema11-from-schema10`. It uses
SQLite FTS tokenization over existing chunk text to rebuild only chunk FTS
storage and creates a fresh validated generation. It performs no acquisition,
OCR, rechunking, model tokenization, model execution, re-embedding, or ANN
rebuild; model, tokenizer, and ANN artifacts remain identical.

## Build and hosting workflow

```bash
cargo build --release --features cuda
scripts/maintainer-sync.sh             # or --full
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp verify
scripts/deploy-generation.sh \
  --host legal-mcp-publisher@HOST
```

Software is 0.19.4. V20
`a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3` is active
locally and on the Linode after the v0.19.2 publisher-tool repair and
activation succeeded. Retain local v19 with the matching v0.18.1 binary/image
as its schema-10 fallback; the schema-11 binary cannot roll back to schema 10.

V0.19.4 implements one hard-cut V2 host-tools transaction for either a
prepared-bootstrap or activated-dark host. It atomically covers the publisher
helper/wrapper/sudoers, `configure-auth`, `update-image`, installed Quadlet
template, and V2 marker/hashes; exact version, `SOURCE_COMMIT`, release bytes,
and the shared host lock are mandatory, and recovery uses the same bundle.
Generated-Quadlet auth state is handled without enabling or disabling the unit.
The operation leaves service and ingress off. Once the v0.19.4 release bundle
exists, verify it, upgrade with `--upgrade-host-tools --version 0.19.4`,
configure auth, then move the image by verified digest.

The unpacked model is under `data/models/mdbr-leaf-ir-standard`. Maintainer
builds use deterministic FP32 ONNX, TensorRT FP16, CUDA fallback, lossless
512-token splitting, and normalized first-256-dimension int8 vectors. Serving is
CPU-safe.

Lifecycle commands are `activate`, `verify`, `rollback`, and
`prune-generations`. There is no runtime updater, remote model assumption,
corpus package/publication, or offline bundle.

The digest-pinned non-root container publishes only host loopback behind Caddy.
Public requests require individual digest-backed API keys, single-tenant
delegated Entra tokens, or both; Caddy stays disabled until auth is proved. The
live corpus stays on external XFS and never enters the image. Local stdio may
use `legal-mcp mcp`. See
[DEPLOYMENT.md](DEPLOYMENT.md) and [MICROSOFT_COPILOT.md](MICROSOFT_COPILOT.md).

## Documentation and proofd

[README.md](README.md) defines the user surface, [AGENTS.md](AGENTS.md) agent
rules, [MAINTENANCE.md](MAINTENANCE.md) maintainer operations,
[CURRENT_STATE.md](CURRENT_STATE.md) the implementation snapshot, and
[PLAN.md](PLAN.md) remaining work.

Canonical tagged guidance is managed by `proofd`; `proofd sync` generates
`.claude/rules/*.md`:

```bash
"$HOME/.claude/agent-proofs/bin/proofd.py" sync
"$HOME/.claude/agent-proofs/bin/proofd.py" lint
"$HOME/.claude/agent-proofs/bin/proofd.py" entry-files --tag <TAG>
"$HOME/.claude/agent-proofs/bin/proofd.py" select-matching <paths...>
"$HOME/.claude/agent-proofs/bin/proofd.py" context <paths...>
```
