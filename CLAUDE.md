# Australian Legal MCP guidelines

Australian Legal MCP is source-grounded legal retrieval over prebuilt immutable
generations. The package is `australian-legal-mcp`, executable `legal-mcp`, MCP
key `australian-legal`, and environment prefix `LEGAL_MCP_*`.

Acquisition, OCR, embedding, ANN construction, and builds run on the local RTX
maintainer host. Validated generations are activated locally and transferred
directly over SSH to a CPU serving VM. The runtime never downloads, scrapes,
embeds, packages, or publishes corpus/model artifacts. GitHub Releases contain
software binaries only.

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
authoritative reranking.

## Build and hosting workflow

```bash
cargo build --release --features cuda
scripts/maintainer-sync.sh             # or --full
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp verify
scripts/deploy-generation.sh deploy@example-vps
```

The unpacked model is under `data/models/mdbr-leaf-ir-standard`. Maintainer
builds use deterministic FP32 ONNX, TensorRT FP16, CUDA fallback, lossless
512-token splitting, and normalized first-256-dimension int8 vectors. Serving is
CPU-safe.

Lifecycle commands are `activate`, `verify`, `rollback`, and
`prune-generations`. There is no runtime updater, remote model assumption,
corpus package/publication, or offline bundle.

Production `legal-mcp.service` binds loopback behind private Tailscale HTTPS.
Local stdio development may use `legal-mcp mcp`. See [DEPLOYMENT.md](DEPLOYMENT.md).

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
