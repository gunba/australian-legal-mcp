# Australian Legal MCP Roadmap

## Purpose

Australian Legal MCP provides one compact, source-grounded retrieval service for
Australian legal material. The current local product indexes ATO material and
official Commonwealth legislation, then exposes both through one source-qualified
corpus and seven MCP tools. The roadmap expands source coverage and validates a
vendor-neutral remote deployment while preserving the local product contract.

## Current baseline

The implemented product baseline is:

- package `australian-legal-mcp`;
- one executable, `legal-mcp`;
- MCP key `australian-legal`;
- local stdio MCP through `legal-mcp mcp`, backed by one shared loopback server;
- registered source IDs `ato` and `frl`;
- one fresh source-qualified `legal.db` per published generation;
- one required `ann/<source>.ann` sidecar per indexed source;
- strict canonical `legal://` live-fetch URIs;
- exactly `search`, `get_chunks`, `get_asset`, `get_doc_anchors`,
  `get_definition`, `stats` and `fetch`;
- source adapters, corpus construction, release operations and serving in the
  same Rust binary.

Remote Streamable HTTP, public authorization, deployment-role separation and
Azure hosting are planned phases. The single `legal-mcp` binary remains the
current executable throughout local validation.

## Architecture invariants

1. **One source per search.** Every search resolves one registered source.
   Omitted `source` selects `ato`; retrieval evaluates only that source's
   database rows and ANN sidecar.
2. **Modular acquisition, simple serving.** Each adapter owns upstream discovery,
   inventory, cursor, rate policy, retries, normalization and fixtures. Serving
   reads one SQLite corpus and the selected source's ANN sidecar.
3. **One content contract.** Every source emits cleaned structural HTML,
   source-qualified document and asset references, plain source-derived search
   text, exact canonical URLs and provenance.
4. **Minimal public payloads.** Source-specific details stay internal unless they
   improve retrieval, citation, currency or navigation.
5. **Fresh generation builds.** Every publication constructs a fresh `legal.db`,
   validates every required `ann/<source>.ann`, and atomically activates one
   immutable generation.
6. **Incremental source work.** Changed records are fetched, normalized and
   rechunked; embeddings are reused by approved model and chunk-text hash.
7. **Authoritative reconciliation.** A completed full source inventory directly
   deletes absent records inside that source transaction.
8. **Failure isolation.** A source failure preserves its last publishable state
   while independent source jobs continue.
9. **Clean-room adapters.** Official upstream contracts and independent fixtures
   govern Rust implementations; OALCC behaviour supplies research evidence for
   discovery and pacing.
10. **Local-first delivery.** Fixture, corpus, protocol, container and load
    validation precede cloud provisioning.

## Current source contracts

### ATO (`ato`)

The initial ATO source workspace is the integrity-pinned
`/home/jordan/Desktop/Projects/ato_pages` tree. The adapter consumes its
`index.jsonl` and payloads directly, runs the proven What's New overlap path and
fetches changed links. Routine policy remains a shared 50 ms issue interval, four
workers and a 30-second request timeout. Selected payloads must match their
declared size and SHA-256.

ATO search defaults remain current-guidance-first: explicit selection for `EV`,
`include_old=true` for pre-2000 non-legislation, a legislation exemption from
that cutoff, and `current_only=true` for withdrawn or superseded rulings.

### Federal Register of Legislation (`frl`)

The FRL adapter uses `https://api.prod.legislation.gov.au/v1/` and its official
OData/OpenAPI contracts:

- authoritative `Titles` pages ordered by `id`, with `$top` at most 100;
- overlapping `Versions` pages ordered by `registeredAt`, `titleId`, `start` and
  `retrospectiveStart`;
- a seven-day overlap and cursor containing that full ordering tuple;
- stable `titleId`, version tuple and `registerId` identities;
- per-version `Documents` enumeration;
- authorised EPUB preference, followed by DOCX and official extracted PDF text;
- two concurrent operations, a 250 ms issue interval, a 30-second timeout and
  bounded exponential backoff with jitter.

Periodic authoritative `Titles` reconciliation supplies direct deletion. Cursor
and inventory advancement follow successful rendition acquisition and durable
state publication.

## Shared source adapter contract

Every adapter supplies:

- a stable `SourceId` and `SourceDescriptor`;
- an official-source inventory with stable native IDs and canonical URLs;
- a cursor and overlap strategy for recent or changed records;
- an independent concurrency, request interval, timeout, retry and transport
  policy;
- deterministic raw payload and normalized-document hashes;
- cleaned HTML, retained assets and source-qualified links;
- fixtures for discovery, pagination, normalization, deletion and failure paths.

Direct HTTP is preferred for structured upstreams. A bounded browser worker is a
planned transport for official portals whose listings require client-side
execution. Source jobs share an outer coordinator but retain independent
limiters and failure state.

A normalized document contains only the shared retrieval fields: source and
native ID, upstream version provenance, title, document type, relevant dates,
canonical URL, cleaned HTML, retained assets and content hashes.

## Corpus update and publication

For each scheduled run:

1. discover changed records through the source overlap window;
2. deduplicate by stable native identity and version signal;
3. fetch new or changed official payloads;
4. normalize through the shared content contract;
5. complete periodic authoritative inventory reconciliation;
6. open a builder-owned fresh `legal.db`;
7. directly delete records absent from each authoritative source inventory;
8. rechunk changed documents and reuse matching embeddings;
9. write source-qualified FTS, definitions, anchors, citations and assets;
10. build the changed `ann/<source>.ann`;
11. validate integrity, content quality, source isolation, retrieval and ANN
    recall;
12. assemble and atomically activate one immutable generation;
13. commit source cursors associated with the published result.

Acquisition and normalization can run concurrently across sources. SQLite
publication is a controlled transactional stage. The published generation is
self-contained and tied to one manifest identity.

## Clean-room expansion

For each additional source:

1. record official endpoints, identities, pagination, formats, rate behaviour and
   browser requirements;
2. capture independent fixtures from official pages, APIs and downloads;
3. implement the adapter in Rust against those official contracts;
4. compare discovery coverage and representative normalized output with verified
   behavioural evidence;
5. run an official-source backfill;
6. prove incremental discovery, correction and deletion paths;
7. register the source with its ANN sidecar and quality gates.

Planned source order:

1. High Court of Australia decisions;
2. NSW Caselaw;
3. Federal Court of Australia decisions;
4. NSW, Queensland, South Australia, Tasmania and Western Australia legislation;
5. Victoria, ACT and Northern Territory legislation;
6. additional superior courts, federal family/circuit courts and tribunals;
7. bills, explanatory material, gazettes, law reform reports and treaties.

This sequence proves structured court and browser-backed acquisition before
repeating similar jurisdictional adapters.

## Local-first validation

Before remote deployment work:

- run adapter discovery, pagination and normalization against bounded fixtures;
- run ATO and FRL incremental updates from stable workspaces;
- build a fresh combined generation locally;
- verify source isolation and direct authoritative deletion;
- benchmark keyword, vector and hybrid search by source;
- verify at least 0.99 ANN recall@50 and exact reranking;
- exercise update interruption and atomic activation;
- run the server in a local container with persistent corpus storage;
- run MCP conformance and client tests;
- use a local OAuth test issuer for the planned remote authorization flow;
- run concurrent search, cancellation and bounded-resource load tests;
- verify container startup, health, update, rollback and persistence.

These results determine CPU, memory, disk and concurrency requirements for the
remote pilot.

## Planned remote service

The remote phase will add one latest-stable, vendor-neutral MCP Streamable HTTP
endpoint around the existing protocol-independent tool dispatch. Planned controls
include:

- protocol-version negotiation and origin validation;
- OAuth protected-resource metadata and standards-based bearer validation;
- request deadlines, cancellation and bounded request/decompression sizes;
- read-only SQLite pooling and shared read-only ANN readers;
- a small bounded pool of semantic model sessions;
- global and per-principal concurrency limits;
- bounded blocking pools for SQLite, ANN and ONNX work;
- structured health, readiness and update status.

Local stdio and remote HTTP will use the same tool schemas and response DTOs. A
deployment-role split between read-only serving and corpus jobs is planned for
evaluation after local container benchmarks. The split will be operational:
`legal-mcp` remains the canonical product executable until a measured deployment
need justifies dedicated service roles.

## Planned Azure pilot

Azure provisioning follows successful local validation. The initial measured
shape will select between a small Linux VM and one Container Apps revision. The
pilot design includes:

- one read-only service container;
- managed disk or an immutable image layer for the active corpus;
- Blob Storage for release artifacts and backups;
- Azure Container Registry;
- Key Vault and managed identity;
- Entra ID for workplace OAuth;
- enterprise ingress only when required by the client environment;
- scheduled, scale-to-zero corpus jobs;
- on-demand GPU compute for deliberate full embedding rebuilds.

Test resources are provisioned for deployment validation and deallocated after
the test window. Production sizing and high availability follow measured latency,
throughput, memory and availability requirements.

## Delivery phases

### Phase 1 — Combined local corpus

- complete one fresh ATO and FRL generation;
- validate official provenance and content quality;
- prove source-qualified search, references and per-source ANN isolation;
- exercise installer update and atomic activation.

### Phase 2 — Source expansion

- implement High Court and NSW Caselaw adapters;
- prove direct HTTP and browser-backed acquisition;
- add Federal Court and state legislation in independently releasable source
  batches.

### Phase 3 — Remote protocol validation

- add Streamable HTTP and authorization around shared tool dispatch;
- pass MCP conformance and supported-client tests;
- run local container, OAuth and load validation;
- decide deployment roles from measured results.

### Phase 4 — Azure pilot

- provision the smallest measured Azure shape;
- deploy a staging revision;
- run conformance, load, update, rollback and persistence tests;
- promote the validated service configuration.

## Acceptance criteria

- Exactly seven tools are exposed on every transport.
- Every search resolves exactly one registered source.
- Public identities and continuations remain source-qualified and generation-safe.
- A source search opens only that source's ANN sidecar.
- A changed document alone is fetched, normalized, rechunked and re-embedded.
- An authoritative inventory directly removes absent records for its source.
- A failed source job leaves its publishable state intact and cannot damage other
  sources.
- Every publication creates and atomically activates a complete fresh generation.
- ATO search quality and content structure remain stable.
- FRL results derive from official API records and authorised renditions.
- Additional adapters satisfy the clean-room evidence and fixture contract.
- Local and planned remote transports produce the same tool results.
- Azure sizing follows local measurements and explicit service objectives.
