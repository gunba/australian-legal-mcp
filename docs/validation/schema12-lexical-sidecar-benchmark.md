# Schema-12 lexical sidecar benchmark

Date: 2026-07-22

This gate selected one deterministic SQLite FTS5 sidecar per source for the
schema-12 lexical path. It measures the demonstrated Federal Court failure case
without a result cache and verifies the exact expected result IDs on every run.

## Corpus and artefact

- Input: flat-int8 v22 `legal.db`
  (`937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939`)
- Source: `federal-court`
- Documents: 72,981
- Chunks: 1,769,379
- Sidecar: `lexical/federal-court.db`
- Size: 1,203,605,504 bytes
- SHA-256: `9c246f6bbe9f07ef3e961b719a163e9312f89f279ec734478a6713a4cb0864a2`
- SQLite: bundled 3.50.2
- Query: `moreton resources innovation science australia activities`
- Policy: strict all-term matching, current-only, score descending then chunk ID
  ascending. The ATO-only old-content and private-advice defaults are covered by
  exact filter-parity tests, not this Federal Court run.

The sidecar contains compact document/chunk filter rows and contentless FTS5
postings only. Full chunk and document payloads remain in `legal.db` and are
loaded only for final winners.

## Reproducible gate

The initial sidecar gate is
`lexical::tests::benchmark_installed_source_sidecar`. On Linux it:

1. rebuilds and strictly verifies the source sidecar;
2. runs 50 warm lexical-stage repetitions;
3. closes the connection;
4. calls `POSIX_FADV_DONTNEED` on the sidecar and opens a new read-only SQLite
   connection before each of 30 cold repetitions;
5. checks the exact eight expected chunk IDs on every repetition; and
6. fails if either warm or cold p95 is at least 100 ms.

```bash
LEGAL_MCP_BENCH_DB=data/runtime/generations/937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939/legal.db \
LEGAL_MCP_BENCH_OUTPUT_ROOT=Temp/lexical-benchmark-v22/rust-cold-gate-schema12 \
LEGAL_MCP_BENCH_SOURCE=federal-court \
LEGAL_MCP_BENCH_RUNS=50 \
LEGAL_MCP_BENCH_COLD_RUNS=30 \
cargo test --locked lexical::tests::benchmark_installed_source_sidecar \
  -- --ignored --exact --nocapture
```

## Result

Test host:

- Linux 6.19.12, Btrfs on NVMe SSD
- Intel Core i9-12900KF, 24 logical CPUs
- Rust 1.95.0
- unoptimised Rust test profile (conservative for query latency)

| Condition | Runs | Median | p95 | Maximum | Gate |
|---|---:|---:|---:|---:|---:|
| warm | 50 | 18.88 ms | 21.05 ms | 24.74 ms | <100 ms |
| advised-cold, reopen each run | 30 | 37.40 ms | 51.05 ms | 53.22 ms | <100 ms |

The exact ordered chunk IDs were:

```text
5433720, 5578447, 5433795, 5494336,
5433665, 5668951, 5326814, 4135052
```

The test used no query-result cache. This result established that the SQLite
sidecar itself has enough latency margin to avoid a more complex inverted-index
dependency.

## Full production-phase gate

Release also requires
`search::tests::benchmark_installed_production_lexical_phase` against the fresh
schema-12 generation. That test prewarms the same strict startup validation as
the hosted service, then advises the selected sidecar out of cache before every
run and measures the emitted `duration_us.lexical_index`. The measured phase
includes active-manifest loading, per-request read-only sidecar open and schema
checks, document-scope bounds, strict body and title FTS work, and candidate
metadata loading. It verifies exact final typed chunk IDs and fails unless cold
p95 is below 100 ms.

```bash
LEGAL_MCP_BENCH_DATA_DIR=data/runtime-schema12 \
LEGAL_MCP_BENCH_SOURCE=federal-court \
LEGAL_MCP_BENCH_EXPECTED_IDS=<comma-separated-final-chunk-ids> \
LEGAL_MCP_BENCH_COLD_RUNS=30 \
LEGAL_MCP_BENCH_COLD_P95_LIMIT_MS=100 \
cargo test --locked search::tests::benchmark_installed_production_lexical_phase \
  -- --ignored --exact --nocapture
```

Record that result here after the fresh immutable generation is built. The
production-phase result, not the core-query result alone, is the release gate.
