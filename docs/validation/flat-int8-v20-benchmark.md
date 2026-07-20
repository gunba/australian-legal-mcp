# V20 source-scoped flat int8 sidecar prototype

## Verdict

A flat exact sidecar is a strong replacement candidate for the current Arroy
candidate path.

- The three tested flat layouts total **0.808 GiB**, versus **9.072 GiB** for
  their Arroy files: **11.23x smaller**.
- Projected over all 6,968,250 vectors, `u32 ids + 256-byte vectors` is
  **1.687 GiB** (about **1.688 GiB** with one bit/vector of eligibility), versus
  **18.943 GiB** of current Arroy sidecars, saving **17.256 GiB**.
- Warm, default-eligible, exact top-50 scans on four physical P-cores took
  **2.65 ms (FRL), 3.99 ms (ATO), and 10.70 ms (Federal Court)**.
- Nine deterministic sampled-query comparisons were **9/9 exactly equal** to a
  SQLite scalar-dot `ORDER BY score DESC, chunk_id ASC LIMIT 50` reference,
  including integer scores and tie order.
- The closest current-path comparison—`similar_to_chunk`, public `k=10`, which
  asks `vector_search` for a 50-hit frontier (from 1,000 Arroy candidates) while
  skipping model, BM25, title search, and snippets—took **383-1,681 ms warm**. Flat scan wall
  speedups were **145x-249x** when the default eligibility bitmap was already
  available.

The important integration caveat is eligibility. Rebuilding the current default
eligible set from SQLite took **195 ms (FRL), 579 ms (ATO), and 792 ms (Federal
Court)** warm. A production replacement must cache the default bitmap by active
generation/source (and optionally cache repeated explicit filters); otherwise
SQLite eligibility materialisation, not vector scoring, remains the dominant
cost.

## Target and method

- Corpus: active schema-11 v20 generation
  `a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3`.
- DB: its read-only `legal.db`; stored vectors are exactly 256 bytes.
- Current comparison server: pre-existing local `target/release/legal-mcp
  0.19.7`, not restarted, using the same v20 generation and Arroy 0.6.4 files.
- CPU: Intel i9-12900KF; scanner pinned to logical CPUs `0,2,4,6`, four distinct
  P-cores, performance governor.
- Scanner: GCC 15.2, C++20, `-O3 -march=native -flto`; AVX-VNNI signed-int8 exact
  dot with checked file arithmetic/bounds, strict file/range validation,
  per-worker bounded heaps, dynamic 16K-vector blocks, and deterministic merge.
- A separately exercised AVX2 widening/madd kernel was only 4-9% slower.
- `/tmp` is a 32-GiB tmpfs. Therefore the “cold-ish” run is deliberately
  **page-table cold but page-cache hot**, not a storage-cold measurement.

Default eligibility matched runtime policy:

- all three sources: `withdrawn_date IS NULL`;
- ATO additionally: exclude `EV`, and exclude pre-2000 material except types
  `PAC`, `REG`, `RPC`, and `RRG`.

The prototype uses three deterministic raw planes per source:

```text
<source>.ids.u32le       N little-endian u32 chunk IDs
<source>.vectors.i8      N x 256 two's-complement int8, row-major
<source>.default.bits    ceil(N/8) eligibility bits (benchmark fixture)
```

Input SQL was ordered by `chunk_id`; IDs were required to be strictly increasing
and dense in the observed source range. Every extracted source stream reproduced
its existing `source_meta.embedding_set_sha256`. A second independent FRL
extraction was byte-identical for IDs, vectors, eligibility bits, and metadata.

## Extraction and size

Times include SQLite reading and tmpfs writes. `vector extract` includes writing
IDs/vectors and sync; `mask` is the exact default-policy SQL; `hash` is SHA-256
of the three output planes. Process elapsed is `/usr/bin/time` end-to-end.
Page-cache state was not forcibly controlled, explaining Federal Court's first
mask build I/O wait.

| source | vectors | default eligible | flat size | Arroy size | ratio | vector extract | mask | hash | process elapsed |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ATO | 1,123,777 | 628,465 (55.92%) | 278.780 MiB | 3.051 GiB | 11.21x | 0.649 s | 0.664 s | 0.158 s | 1.71 s |
| FRL | 441,910 | 439,046 (99.35%) | 109.627 MiB | 1.193 GiB | 11.15x | 0.255 s | 0.490 s | 0.059 s | 0.92 s |
| Federal Court | 1,769,363 | 1,755,820 (99.23%) | 438.934 MiB | 4.828 GiB | 11.26x | 1.055 s | 4.398 s | 0.242 s | 6.22 s |

Vector extraction sustained 409-423 MiB/s of vector payload. Federal Court's
first mask build performed about 920 MiB of filesystem input; its later warm
mask-build median was 0.792 s.

## Exact scan results

Warm results aggregate 27 scans/source: three deterministic eligible query
vectors (eligible ranks 1/7, 1/2, and 6/7), nine runs each. Throughput counts
only vectors actually scored, which matters for ATO.

| source | wall median | p10-p90 | CPU median | CPU/wall | scored throughput | warm RSS |
|---|---:|---:|---:|---:|---:|---:|
| ATO | 3.987 ms | 3.634-4.939 ms | 14.524 ms | 3.58 cores | 37.58 GiB/s | 286.3 MiB |
| FRL | 2.648 ms | 2.411-3.261 ms | 9.455 ms | 3.59 cores | 39.54 GiB/s | 117.1 MiB |
| Federal Court | 10.704 ms | 9.384-11.963 ms | 41.086 ms | 3.85 cores | 39.11 GiB/s | 446.5 MiB |

The production search's public `k=50` widens the vector frontier to 250 before
document diversity/hydration. Exact top-250 remained essentially unchanged:
**4.188 ms ATO, 2.791 ms FRL, 11.033 ms Federal Court**.

Paired 15-run portable-kernel medians:

| source | AVX-VNNI | AVX2 |
|---|---:|---:|
| ATO | 3.990 ms | 4.331 ms |
| FRL | 2.817 ms | 3.030 ms |
| Federal Court | 11.302 ms | 11.744 ms |

## Cold-ish, RSS, and page-cache behaviour

Before each run the benchmark issued `posix_fadvise(DONTNEED)` and
`madvise(DONTNEED)` on all three mappings. On tmpfs this removed process PTEs/RSS
but could not evict the file's backing pages: `mincore` was 100% resident before
and after, and every run had zero major faults.

| source | wall median | CPU median | minor faults | RSS before -> after |
|---|---:|---:|---:|---:|
| ATO | 4.374 ms | 15.567 ms | 2,529 | 7.6 -> 182.8 MiB |
| FRL | 3.275 ms | 10.902 ms | 883 | 7.6 -> 116.6 MiB |
| Federal Court | 12.017 ms | 45.665 ms | 6,569 | 7.6 -> 445.8 MiB |

ATO's post-scan RSS is below its full payload because default eligibility is
clustered and only 55.9% of vectors are scored. FRL/Federal Court touch almost
every vector. Linux fault-around explains why minor-fault counts are much lower
than 4-KiB page counts.

The current Arroy server's warm seed-vector calls still incurred median minor
faults of **23,103 ATO, 14,551 FRL, and 62,036 Federal Court** per request, while
physical `read_bytes` was zero at the median. Logical read volume (`rchar`) was
about **637, 210, and 845 MiB/request**, respectively. The first measured seed
call per source took **1.549 s ATO, 0.606 s FRL, and 1.994 s Federal Court** and
reported about **295, 125, and 329 MiB** of physical reads; later new query
vectors sometimes pulled additional pages. The server's observed high-water RSS
rose from about 3.78 GiB to 4.07 GiB during the benchmark; steady RSS after
mappings closed was about 259 MiB. That HWM also includes the semantic runtime, so it is an observed
whole-process figure, not an Arroy-only allocation.

## Exactness evidence

The SQLite reference registered a scalar `dot_i8_ref(blob)` UDF and ran the
exact default document filter followed by:

```sql
ORDER BY score DESC, e.chunk_id ASC LIMIT 50
```

Sample query IDs:

- ATO: `353087`, `787084`, `1018270`
- FRL: `5786841`, `5946364`, `6103290`
- Federal Court: `4207082`, `4836448`, `5469857`

All nine flat results exactly matched all 50 IDs and raw i32 scores. Reference
SQLite wall medians were 571 ms ATO, 206 ms FRL, and 793 ms Federal Court. The
prototype also checked AVX-VNNI and AVX2 against scalar dot for 90,000
deterministic vector pairs, with no mismatch. A `_GLIBCXX_ASSERTIONS`, fortify,
and stack-protector build also passed FRL inspection and scan.

## Current Arroy/vector/hybrid comparison

Medians are six steady-state loopback HTTP calls/source. Server CPU was read from
`/proc/<pid>/stat` around each call.

| source | flat exact top-50, cached eligibility | current seed vector k=10 | current text vector k=50 | current hybrid k=50 |
|---|---:|---:|---:|---:|
| ATO | 3.987 ms wall / 14.5 ms CPU | 993.8 / 970 ms | 1,122.0 / 6,320 ms | 1,330.6 / 6,515 ms |
| FRL | 2.648 ms wall / 9.5 ms CPU | 383.4 / 375 ms | 504.7 / 5,720 ms | 784.3 / 5,980 ms |
| Federal Court | 10.704 ms wall / 41.1 ms CPU | 1,680.6 / 1,665 ms | 1,845.7 / 7,010 ms | 1,955.8 / 7,095 ms |

Notes:

- Seed vector is the closest current-path comparator to raw top-50, but still
  includes SQLite eligibility, Arroy, SQLite exact candidate rerank, document
  diversity, and hydration; it excludes the seed from output.
- Text vector/hybrid include ONNX query embedding and title/hydration work;
  hybrid also includes FTS. The existing server was allowed all 24 logical CPUs,
  which explains model CPU time far above wall time. These rows are end-to-end,
  not an isolated Arroy microbenchmark.
- Adding uncached warm default-mask construction to the flat scan gives about
  **583 ms ATO, 197 ms FRL, and 803 ms Federal Court**. This is still roughly
  1.7x-2.1x faster than the current seed path, but forfeits most of the flat
  layout's potential. Cache the default bitmap.
- Once Arroy is removed, query embedding/FTS becomes the next dominant cost for
  normal text searches.

## Is a local root SSD needed?

**Not for steady-state operation, assuming startup/lazy prewarming.** The full
ten-source flat vector+ID set is only 1.687 GiB and should fit comfortably in the
documented 8-GiB serving host's page cache alongside the model and a bounded
SQLite working set. Sequentially prefetching each source from the mounted corpus volume avoids
the random-access sensitivity that motivated a root-SSD copy.

This workstation's root/home device is a Lexar NM790 NVMe behind LUKS+Btrfs. A
read-only `O_DIRECT` 4-GiB scan of the existing fragmented Federal Court Arroy
file took 0.66 s (about 6.06 GiB/s), but that is not evidence for Linode Block
Storage. Because the prototype files had to remain in tmpfs, a genuinely
storage-cold flat scan was not measured.

Cold transfer lower bounds at representative attached-volume bandwidths are:

| source flat payload | 250 MiB/s | 500 MiB/s | 1,000 MiB/s |
|---|---:|---:|---:|
| ATO, 278.8 MiB | 1.12 s | 0.56 s | 0.28 s |
| FRL, 109.6 MiB | 0.44 s | 0.22 s | 0.11 s |
| Federal Court, 438.9 MiB | 1.76 s | 0.88 s | 0.44 s |

Therefore keep the sidecars on the canonical XFS corpus volume and prewarm them.
Only consider an ephemeral root-SSD cache if a read-only benchmark on the actual
host proves cold sequential throughput violates the first-query SLO and startup
prefetch cannot hide it. A root copy otherwise creates an unnecessary second
activation/verification path.

## Minimal production format and verification contract

Use one source file to reduce lifecycle surface while retaining the prototype's
separate planes:

```text
[4-KiB fixed little-endian header]
[u32 little-endian ID plane: N * 4]
[zero alignment padding]
[int8 vector plane: N * 256, 4-KiB aligned]
```

Retaining IDs costs only 1.5% and avoids making dense IDs a permanent hidden
assumption. Required header/manifest fields:

- magic, format version, header length, source ID;
- embedding dimension `256`, vector count, first/last chunk ID;
- checked offsets and lengths for ID/vector planes;
- model ID/fingerprint and the existing `embedding_set_sha256`;
- metric `signed-int8-dot-exact`, ID encoding `sqlite-chunk-id-u32`;
- whole-file size and SHA-256 in `generation.json`.

Activation/startup verification should:

1. require a regular, non-symlink, single-link file of exact manifest size;
2. validate magic/version/source/model and all offset/length arithmetic before
   mapping or slicing; reject overlap, truncation, trailing bytes, and nonzero
   reserved header fields;
3. require nonzero `N`, 256 dimensions, strictly increasing unique u32 IDs, and
   source isolation/count/range equality with SQLite;
4. recompute the existing domain-separated embedding-set digest from
   `(chunk_id as i64 little-endian, 256 vector bytes)` and match both SQLite and
   manifest metadata;
5. verify the whole-file SHA-256 once at activation/startup, not on every search;
6. use exact i32 accumulation (the maximum absolute 256-dimensional int8 dot is
   safely inside i32), rank by score descending then chunk ID ascending, and
   runtime-dispatch AVX-VNNI/AVX2/scalar implementations with equivalence tests;
7. build/cache default eligibility from authoritative SQLite by
   `(generation, source, exact filter tuple)`; do not bake mutable search policy
   into the immutable vector artifact;
8. run per-source sampled exact top-50 equality against SQLite as a generation
   validation gate.

A global bounded scan pool should own roughly four physical cores and dynamically
schedule blocks; allowing every concurrent HTTP worker to independently consume
four cores would trade single-query latency for uncontrolled contention.

## Artifacts and repository state

The prototype's disposable working set was created under
`/tmp/legal-flat-int8-v20-bench` (about 938 MiB), including source, optimized
binary, raw sidecars, metadata, repeat-extraction evidence, exact-reference
logs, and current-server timings. That scratch directory was removed after this
report became the retained durable evidence.

No tracked repository file was edited during the benchmark. The immutable DB
and Arroy files remained read-only with their original timestamps.
