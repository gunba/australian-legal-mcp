# HarbourGrid v22 validation results

- **Validated:** 19–21 July 2026
- **Build validation software:** `legal-mcp 0.19.7`
- **Hosted serving software:** `legal-mcp 0.19.11`
- **Generation:** `937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939`
- **Status:** active locally and on the authenticated Linode service; Arroy v20 is the sole hosted rollback.

## Corpus build and identity

The chunker-format-6 build service completed successfully in 2,850.2 seconds and wrote:

| Measure | Result |
|---|---:|
| Documents | 409,528 |
| Chunks | 6,986,040 |
| Embeddings | 6,986,040 |
| Definitions | 20,169 |
| SQLite size | 19,758,231,552 bytes |
| Flat-int8 sidecars | 1,816,430,592 bytes across ten sources |

Strict activation and CPU verification passed with `semantic_search_ready=true`. The database SHA-256 is `c8e77a7dbf61a8b185592c07bb47b0cc324bfc2cce2b9e2663f5c4716483b851`.

The build reconstructed exact `(model_id, chunk_text_sha256)` cache keys from the prior active generation's authoritative `chunk_embeddings`. It reused 6,882,481 of 6,986,040 vectors (98.518%) and encoded only 103,559 changed chunk texts. FRL accounted for 97,996 newly encoded chunks because preserving typed formula images changed its normalised/chunked content.

## Formula and asset regression

V21 failed three deterministic checks:

1. section `355-25` was absent from retrieved R&D core-activity context;
2. section `355-450` was absent from clawback/feedstock context; and
3. the formula context exposed no typed asset.

The cause was structural serialization in `rewrite_internal_document_links()`: it removed every `<img>`, including already-typed formula images. Chunker format 6 preserves safe `data-asset-ref`, `data-media-type`, `alt`, and `title` attributes while continuing to remove source URLs.

V22 passed all three checks. Retrieval contained the provision text, exposed a typed formula asset, and `get_asset` returned a non-empty image payload with an image media type.

## HarbourGrid evaluator

`scripts/run-harbourgrid-eval.py` passed with zero failures. It exercised exactly seven tools, ten source partitions, expected legislation/guidance/cases, typed chunk retrieval, formula assets, definitions, canonical fetch, warm latency, and `/readyz` while four concurrent hybrid requests occupied the research workers.

| Warm metric | V21 | V22 | V22 result |
|---|---:|---:|---:|
| Keyword p95 | 349.391 ms | 363.234 ms | Pass |
| Hybrid p95 | 570.583 ms | 516.406 ms | Pass |
| Retrieval p95 | 12.125 ms | 11.636 ms | Pass |
| Formula/asset failures | 3 | 0 | Pass |
| Readiness under load | 0.495 ms | 0.618 ms | Pass |

These are end-to-end evaluator measurements and should not be compared directly with the isolated flat-sidecar microbenchmark.

After the v0.19.10 deployment, the same evaluator passed with zero failures
against the live service. A three-repetition host-loopback run measured keyword p95
720.831 ms, hybrid p95 1,133.475 ms, retrieval p95 12.203 ms, and private
readiness under load. The final v0.19.11 public-TLS run again had zero failures:
keyword p95 was 584.989 ms, hybrid p95 910.471 ms, and retrieval p95 757.606 ms.
Caddy returned the required 404 for the deliberately unexposed `/readyz` route;
private readiness returned 200 on host loopback. Three post-reboot exact
Moreton document-scoped keyword requests completed end to end in 561.877–628.965
ms, replacing the roughly 11-second full-source scan path. All seven tools, all
ten sources, formulas, typed assets, and expected authorities passed before and
after the v0.19.11 reboot.

## Search architecture evidence

Every request requires one explicit source. Keyword and title FTS execute inside that source's validated row-ID partition; vector search scans only that source's deterministic mmap flat-int8 sidecar. Ranking and tie ordering remain deterministic. Source partitions are validated against interleaving and fail closed.

The prototype benchmark established:

- 1.687 GiB projected ten-source flat storage versus 18.943 GiB of Arroy sidecars;
- exact top-50 scans of 3.99 ms for ATO, 2.65 ms for FRL, and 10.70 ms for Federal Court with cached eligibility;
- 9/9 sampled queries exactly matching the SQLite scalar reference, including scores and `score DESC, chunk_id ASC` tie order; and
- 145×–249× raw scan-wall improvement over the closest measured Arroy request path.

See [the full benchmark](flat-int8-v20-benchmark.md) for methodology and caveats.

## Software and recovery validation

Before merge:

- all 85 installed-corpus smoke checks passed;
- 287 Rust workspace/HTTP/SDK tests passed, with 11 hardware/live tests explicitly ignored;
- rustfmt, strict Clippy, `cargo audit`, and `cargo deny` passed;
- 18 Python tests passed;
- Bash syntax, ShellCheck, actionlint, Caddy validation, packaging, npm allowlisting, and diff checks passed;
- launcher upgrade, authentication, deployment, activation, abort, publisher, bootstrap, and SIGKILL recovery fixtures passed; and
- GitHub `main` CI completed successfully after merge.

- Pull request: <https://github.com/gunba/australian-legal-mcp/pull/16>
- Merge commit: `054797d60f579315a095e615cf90c00c5e9cf59b`
- Successful main CI: <https://github.com/gunba/australian-legal-mcp/actions/runs/29710261737>

## Remaining boundaries

- FRL historical-compilation/provision navigation remains less complete than ATO point-in-time breadcrumbs.
- AAT/ART coverage is not present.
- The temporary hosted hostname is not permanent production DNS.
- Enterprise testing must use a separately issued revocable credential and synthetic/public facts unless organisational approval permits otherwise.
