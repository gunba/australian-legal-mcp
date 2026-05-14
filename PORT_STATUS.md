# Python -> Rust Port Status

Tracking the progress of the multi-session port of the Python maintainer
pipeline to Rust, per the "no Python left" goal. Each item lists the file,
line count before, what's in Rust now, and the wrapper status.

## Done — Rust impl + Python wrapper subprocesses to it

| File | LOC before | Wrapper LOC | Rust impl | pytest |
|---|---|---|---|---|
| `src/ato_mcp/indexer/extract.py` | 972 | ~150 | clean_ato_html, doc_id_from_ato_link, rewrite_links_html, rewrite_images_html, normalise_named_anchors, strip_attributes, extract_currency, extract_collect_anchors, extract_em_front_matter, extract_leading_headings, extract_compose_title, chunker_html_to_text | 48/48 ✓ |
| `src/ato_mcp/indexer/anchors.py` | 227 | ~85 | extract_anchors (in_doc / sister / history classification, sentinel PiT, label resolution) | 11/11 ✓ |
| `src/ato_mcp/indexer/definitions.py` | 130 | ~95 | extract_definitions | 2/2 ✓ |
| `src/ato_mcp/indexer/chunk.py` | 711 | ~90 | chunk_html (block walker, atomic block classifier, render_block, dt/dd pairing, table render, oversize split via row/sentence/word fallback) | 25/25 ✓ |
| `src/ato_mcp/indexer/metadata.py` | 261 | ~89 | doc_id_for, parse_docid, year_for_docid, human_code_for_doc_id, extract_pub_date | 41/41 ✓ |

CLI subcommands added: `extract`, `extract-anchors`, `extract-definitions`,
`extract-currency`, `chunk-html`, `doc-meta`, `doc-id-from-link`, `pack-write`,
`manifest-rewrite-urls`, `bundle-model`, `ato-fetch-nodes`, `embed`.

## Done — Rust impl, no Python wrapper yet

| Function | Rust | Notes |
|---|---|---|
| `pack.py:PackWriter` | `write_pack` + `pack-write` CLI | Wrapper deferred — stateful API exposes refs/sha8 inside the with-block, doesn't subprocess cleanly. Lands with build.py rewrite. |
| `release.py:rewrite_manifest_urls` | `manifest-rewrite-urls` CLI | One-shot — once publish-release.sh stops shelling into Python, this CLI replaces the call. |
| `release.py:bundle_model` | `bundle-model` CLI | Same as above. |
| `scraper/client.py:AtoBrowseClient.fetch_nodes` | `ato-fetch-nodes` CLI | First scraper primitive in Rust. Tree crawler / what's-new use this. |
| `embed.model:EmbeddingModel.encode_query` (build path) | `embed` CLI | Batch text-to-int8 encoder. Build pipeline will pipe chunk text through this. |

## Pending — Python still unmodified

| File | LOC | Why deferred |
|---|---|---|
| `src/ato_mcp/indexer/build.py` | 1977 | Orchestrator; depends on every other module. Rewrite as `ato-mcp build` CLI is the end goal. |
| `src/ato_mcp/indexer/rules.py` | 1217 | Classifier rule engine. Big port, low priority. |
| `src/ato_mcp/scraper/*.py` | 1849 | HTTP downloader, tree crawler, what's-new. Significant async-style logic. |
| `src/ato_mcp/indexer/release.py` | 411 | gh + minisign + tar/zstd orchestration. Could become a shell script. |
| `src/ato_mcp/store/manifest.py` | 235 | Rust runtime has Manifest struct already; wrapper needs care because build.py builds the Manifest in-memory. |
| `src/ato_mcp/store/*.py` (db, queries) | 129 | Used by build.py for DB writes. Will subsume into Rust build orchestrator. |
| `scripts/maintainer-sync.sh` | 285 | Calls into the deleted Python pipeline. |
| `scripts/publish-release.sh` | 87 | Same. |
| `scripts/make-offline-bundle.sh` | 141 | Embeds Python via heredoc. |
| `scripts/backfill-citations.py` | 49 | Standalone Python utility. |
| `pyproject.toml` | — | Goes when src/ato_mcp/ is fully gone. |

## Stats

- Python deleted (replaced with subprocess wrappers): **~2200 lines**
- Wrapper code added: **~510 lines**
- Net Python reduction so far: **~1700 lines**
- Rust ports added: **~3200 lines**
- pytest: **212 passed, 1 skipped** across all wrappered files
- cargo test: **83 passed** (Rust unit + http_smoke + stdio_shim)
- CI: green on every commit

## End-state checklist (goal completion)

- [x] Runtime fetch_external_doc with full inline marker parity
- [x] All extract.py logic in Rust (text, currency, anchors, assets, links, attrs, headings, EM)
- [x] All anchors.py logic in Rust
- [x] All definitions.py logic in Rust
- [x] All chunk.py logic in Rust (chunker + html_to_text + oversize splits)
- [x] All metadata.py public helpers in Rust
- [x] Rust pack-write CLI
- [ ] Python build.py rewritten as `ato-mcp build` CLI (or subprocess composition)
- [ ] Python rules.py ported
- [ ] Python scraper/* ported
- [ ] Python release.py replaced (gh + tar/zstd shell?)
- [ ] Python wrappers deleted (extract.py, anchors.py, definitions.py, chunk.py, metadata.py)
- [ ] All Python under src/ato_mcp/ deleted
- [ ] tests/ migrated or deleted
- [ ] pyproject.toml deleted
- [ ] CI python-maintainer job dropped
- [ ] Version bumped to 0.8.0
- [ ] v0.8.0 tag pushed
- [ ] release-binaries workflow ships binaries
