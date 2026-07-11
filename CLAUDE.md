# Australian Legal MCP Guidelines

Australian Legal MCP is an on-device legal retrieval service backed by
pre-built, integrity-checked corpus generations. The package is
`australian-legal-mcp`, the executable is `legal-mcp`, the MCP key is
`australian-legal`, and runtime configuration uses `LEGAL_MCP_*`.

The current service is one Rust binary. It provides the local MCP stdio proxy,
the shared loopback backend, end-user corpus commands and maintainer acquisition,
build and publication commands. Remote serving and deployment-role separation
belong to the roadmap in [PLAN.md](PLAN.md).

## Public contract

The MCP server exposes exactly seven tools: `search`, `get_chunks`, `get_asset`,
`get_doc_anchors`, `get_definition`, `stats` and `fetch`. Search selects one
registered source, `ato` or `frl`, and defaults to `ato`. Live fetch identifiers
use only canonical `legal://<source>/<percent-encoded-native-id>` URIs.

The canonical installed corpus is `legal.db` plus one required
`ann/<source>.ann` sidecar for every indexed source. Public references are
source-qualified and chunk references include the corpus generation.

## Context budget

Tool responses contain only information a legal-research agent can act on:
source identity, title, type, date or currency metadata, exact canonical URL,
heading path, snippet, and navigable document/chunk/asset references.

Keep internal ranking scores, model identifiers, candidate counts, chunk
ordinals, echoed queries, policy diagnostics and debug counters inside the
runtime. Omit empty optional fields with `skip_serializing_if =
"Option::is_none"`. Every response byte competes with primary source text.

The document surface is cleaned structural HTML. Preserve stable tags and useful
attributes. Represent internal legal links and retained assets with deterministic
source-qualified references. Derive FTS and embedding text directly from cleaned
HTML, with headings in metadata and only useful visible link/image text.

## Engineering choices

Prefer typed, deterministic logic derived from official source structure. Parse
official URLs and API records into exact native IDs; derive titles and headings
from source markup; and keep source-specific volatility inside acquisition and
normalization adapters.

Use one canonical implementation path for identities, storage, ANN lookup,
continuations, manifests and activation. Extend the source adapter or shared
pipeline when required functionality is missing.

Use deterministic completion signals, locks and atomic filesystem operations for
control flow. Bound request queues, bodies, decompression, retries, timeouts and
blocking work.

Date-sensitive or historical-law resolution belongs on the public surface when
broad, source-derived version and effective-date data supports it. Preserve raw
provenance so agents can verify point-in-time conclusions themselves.

## Source and corpus rules

Routine ATO acquisition reuses the integrity-pinned
`/home/jordan/Desktop/Projects/ato_pages` workspace and runs the proven What's
New changed-link path. Preserve its 50 ms shared issue interval, four workers and
30-second request timeout.

FRL acquisition uses the official `https://api.prod.legislation.gov.au/v1/`
contract: authoritative `Titles`, overlapping `Versions`, per-version
`Documents`, stable title/register identities and authorised rendition
selection. Prefer EPUB, then DOCX, then official extracted PDF text.

Run independent source jobs concurrently under their own rate policies. Commit a
source cursor after durable success. A source failure retains its publishable
state while unrelated sources continue.

Build every release into a fresh `legal.db`. Reconcile authoritative inventories
inside source transactions, directly delete absent source records, reuse
embeddings by approved model and chunk-text hash, and rebuild the changed
source's `ann/<source>.ann`. Activate only a fully validated immutable generation.

Implement new source adapters as clean-room Rust code against official upstreams
and independent fixtures. OALCC behaviour is research evidence for discovery,
pagination, rate limits, format choice and browser requirements.

## Build and workflow

End-user machines are expected to be CPU-only and may be low-performance
enterprise laptops. Keep install, update, search, fetch and serving CPU-safe.
Maintainer embedding builds run on an approved GPU host:

```bash
cargo build --release --features cuda
```

`build` consumes local embedding model files. `publish-release` owns model
distribution URLs, digests, sizes, signatures and the final manifest.

Run long builds and test suites with background execution where supported, wait
for their completion signal, and act on dependent results afterward. Use `/r`,
`/j`, `/b`, `/c` and `/rj` from the repository root where those project commands
are available.

## Documentation and proofd

[README.md](README.md) defines the user surface, [AGENTS.md](AGENTS.md) defines
agent operating rules, [MAINTENANCE.md](MAINTENANCE.md) defines release
operations, [CURRENT_STATE.md](CURRENT_STATE.md) records the implementation
snapshot and [PLAN.md](PLAN.md) contains planned work.

Tagged implementation guidance is managed by `proofd`. Canonical rule data lives
in its knowledge base; `proofd sync` generates `.claude/rules/*.md`. Use the
proofd CLI to add or update rules and regenerate snapshots:

```bash
"$HOME/.claude/agent-proofs/bin/proofd.py" sync
"$HOME/.claude/agent-proofs/bin/proofd.py" lint
"$HOME/.claude/agent-proofs/bin/proofd.py" entry-files --tag <TAG>
"$HOME/.claude/agent-proofs/bin/proofd.py" select-matching <paths...>
"$HOME/.claude/agent-proofs/bin/proofd.py" context <paths...>
```

Allocate implementation tags through `proofd` and place `[TAG]` comments near
the governed code. Generated rule Markdown is refreshed through `proofd sync`.
