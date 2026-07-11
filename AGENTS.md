# AGENTS.md

Instructions for agents developing, installing or operating Australian Legal
MCP. Read [README.md](./README.md) for the user surface,
[MAINTENANCE.md](./MAINTENANCE.md) for release operations,
[CURRENT_STATE.md](./CURRENT_STATE.md) for the implementation snapshot and
[PLAN.md](./PLAN.md) for planned source and deployment work.

## Canonical product contract

| Concern | Canonical value |
|---|---|
| Product and package | `australian-legal-mcp` |
| Executable | `legal-mcp` |
| MCP key | `australian-legal` |
| Environment variables | `LEGAL_MCP_*` |
| Repository | `gunba/australian-legal-mcp` |
| Data directory name | `australian-legal-mcp` |
| Corpus database | `legal.db` |
| ANN sidecars | `ann/<source>.ann` |
| Registered sources | `ato`, `frl` |
| Live document URI | `legal://<source>/<encoded-native-id>` |

The current implementation is one `legal-mcp` binary. It owns local MCP
transport, corpus installation, retrieval and maintainer source/build commands.
Public remote serving, deployment-role separation and Azure are planned work.

## Design principles

Australian Legal MCP exposes compact, source-grounded retrieval primitives and
lets agents perform legal reasoning. Prefer deterministic features derived from
stable official source structure, few parameters and minimal response context.

Every public concern has one canonical path: one source-qualified identity
model, one corpus schema, one generation layout, one URI syntax and one MCP
surface. Runtime logic follows typed source adapters and structured upstream
data rather than guessed aliases, hand-maintained Act maps or prose heuristics.

The document surface is cleaned structural HTML. Preserve useful tags and
attributes so agents can navigate source structure directly. Internal links
become deterministic source-qualified document references, and retained images
become compact source-qualified asset references. Search text is plain text
derived from that HTML; headings remain metadata, while links and images
contribute useful visible text.

## Public MCP surface

Expose exactly these seven tools:

- `search`
- `get_chunks`
- `get_asset`
- `get_doc_anchors`
- `get_definition`
- `stats`
- `fetch`

A search resolves exactly one registered source; omission selects `ato`. Public
document, chunk and asset references carry their source, and chunk references
also carry their corpus generation. Continuations preserve every explicit
source and filter.

`fetch` accepts a canonical `legal://` URI. The host is a registered source and
the native ID is one percent-encoded path segment. Reject whitespace, fragments,
credentials, ports, noncanonical escapes, extra path segments and unrecognised
query keys. ATO `pit` and `view` query values are preserved through typed URI
parsing.

Tool responses contain information a legal-research agent can navigate, quote or
cite. Keep ranking internals, model identifiers, candidate counts, chunk
ordinals, query echoes and diagnostic counters out of public payloads. Optional
empty fields are omitted rather than serialised as `null`.

## Source and update invariants

Each source owns its descriptor, official upstreams, discovery cursor,
inventory,
rate policy, retry policy, normalization hooks and fixtures. Source discovery
jobs run concurrently under independent limits. A failed source retains its last
publishable state; successful sources continue to validation and publication.

Incremental acquisition is the routine path: discover an overlap window, dedupe
by stable native ID, fetch changed records, normalize, and commit the cursor
only
after the source result is durable. A full inventory is authoritative. Reconcile
it inside one source transaction and directly delete source rows absent from
that
inventory.

Every publication builds a fresh `legal.db`. Rechunk changed documents, reuse
embeddings by approved model and chunk-text hash, rebuild the affected
`ann/<source>.ann`, run source and corpus checks, then assemble one immutable
generation. `active-generation` is the only activation point.

### ATO (`ato`)

Use `/home/jordan/Desktop/Projects/ato_pages` as the integrity-pinned ATO source
workspace. Routine runs use its `index.jsonl`, payload tree and What's New
discovery, then fetch discovered changed links. A full ATO acquisition is an
explicitly authorised repair operation.

Preserve the proven ATO request policy: one shared 50 ms issue interval, four
fetch workers and a 30-second request timeout. Validate declared payload sizes
and SHA-256 values before normalization.

Preserve current-guidance search policy:

- `EV` is selected only through an explicit `types` request;
- `include_old=true` includes non-legislation dated before `2000-01-01`;
- legislation is exempt from the date cutoff;
- `current_only=true` filters withdrawn and superseded rulings.

### Federal Register of Legislation (`frl`)

Use the official API at `https://api.prod.legislation.gov.au/v1/`. Initial
reconciliation pages `Titles` by stable `id` with `$top` at most 100.
Incremental
discovery pages `Versions` from a seven-day overlap boundary, ordered by
`registeredAt`, `titleId`, `start` and `retrospectiveStart`. Persist the full
cursor tuple and deduplicate the overlap.

Use `titleId` as the stable native document ID, the version tuple as version
identity and `registerId` as registration provenance. Enumerate `Documents` for
the selected version and prefer official authorised EPUB, then DOCX, then
official extracted PDF text. The initial source policy is two concurrent
operations, a 250 ms issue interval, a 30-second request timeout and bounded
exponential backoff with jitter.

Periodic authoritative `Titles` reconciliation directly deletes records that
are absent or outside the selected current corpus.

## Clean-room source development

OALCC is behavioural research evidence for endpoint discovery, pagination, rate
limits, format selection and browser requirements. Implement adapters from
official upstream contracts and independently captured fixtures. Record the
official URL, native identity, pagination, provenance, format selection and
measured rate policy for every adapter.

Register a source when acquisition, normalization, source-qualified indexing,
search, assets, links, quality fixtures and update reconciliation work end to
end. New sources use the shared cleaned-HTML and retrieval syntax rather than
source-specific public tools.

## Install and operate

Use the release executable and the canonical MCP entry:

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

`legal-mcp mcp` starts or reuses one loopback backend for the selected data
directory. Installer agents choose a stable data directory once and supply the
same `LEGAL_MCP_DATA_DIR` to every command when overriding the platform default.
The first corpus install is a large verified download; explain it and obtain
approval before running `legal-mcp update`.

Verify an installation with:

```bash
legal-mcp stats
legal-mcp search "research and development tax incentive eligibility" \
  --source ato --k 5
legal-mcp search "income tax assessment act" --source frl --k 5
```

Inside the MCP host, call `stats` and a source-specific `search`; confirm
results
contain exact `canonical_url` values and resolvable source-qualified references.

## Maintainer boundary

Corpus acquisition, builds and publication run from the maintainer checkout.
Build release binaries and corpus tooling with:

```bash
cargo build --release --features cuda
```

The CUDA feature selects the GPU execution provider for maintainer embedding
work. End-user `update`, `stats`, `search`, `fetch`, `mcp` and `serve` use the
CPU-safe runtime.

Treat installed generation directories as immutable. Serialize `legal-mcp
update` with its data-directory lock and serialize source updates with each
workspace lock. Keep the ATO source workspace, FRL workspace, run output and
published data directory as distinct stable paths.

## Validation

Before publication, run:

```bash
cargo fmt --all -- --check
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
bash -n scripts/*.sh
git diff --check
scripts/smoke.sh
```

Validate both sources, strict source isolation, canonical URI rejection cases,
authoritative deletion, source failure isolation, embedding reuse, per-source
ANN recall, manifest integrity and atomic activation.

## Troubleshooting

- **`legal-mcp` is not found:** Install the release executable in a stable
  directory on `PATH`, then verify `legal-mcp --version`.
- **MCP stdio startup fails:** Confirm `mcpServers.australian-legal` runs
  `legal-mcp mcp` and inherits the intended `LEGAL_MCP_DATA_DIR`.
- **Local backend bind fails:** Stop the process holding the port or run
  `legal-mcp serve --port <port>` for a deliberate local test.
- **Corpus is unavailable:** Run `legal-mcp update` with approval, restart the
  MCP host/backend and inspect `legal-mcp stats`.
- **A source search is empty:** Confirm the source appears in `stats`, inspect
  its type/date policy and retry with an explicit source.
