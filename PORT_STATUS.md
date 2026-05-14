# Python -> Rust Port: COMPLETE

The repository is now Python-free. All maintainer functionality lives in
the Rust binary at `src/main.rs` and is invoked by shell scripts under
`scripts/` + the systemd units under `systemd/`.

## Final state

- **Python remaining**: 0 files
- **`pyproject.toml`**: deleted
- **CI**: `python-maintainer` job dropped; `cargo build/test/clippy`
  is the only check
- **`scripts/*.sh`**: invoke `target/release/ato-mcp` directly; no
  `.venv`, no `LD_LIBRARY_PATH` for nvidia, no `python3` heredocs

## Rust subcommands shipping the maintainer pipeline

Source refresh:
- `ato-mcp tree-crawl --out-dir <path>` — BFS the ATO browse-content
  tree, write `nodes.jsonl` + `meta.json`
- `ato-mcp snapshot-reduce --nodes-path <path>` — dedupe, mark redundant
  folders, write `deduped_links.jsonl` + `dedup_summary.json` +
  `redundant_paths.json` + `skip_data_urls.json`
- `ato-mcp link-download --deduped-links <path> --out-dir <path>` —
  parallel HTTP fetch into `payloads/<Category>/<slug>.html` + `index.jsonl`
- `ato-mcp scrape-diff --index ... [--deduped F | --whats-new-url URL]
  --out F` — emit only the records missing from an existing index
- `ato-mcp whats-new` — pull the live What's New feed
- `ato-mcp normalize-doc-href <href>` — canonicalise an ATO doc URL

Build:
- `ato-mcp build --pages-dir ... --db-path ... --out-dir ...` —
  walks `index.jsonl`, runs cleaning + chunker + embedder, writes
  `documents`, `chunks`, `chunk_embeddings`, `chunks_fts`, `title_fts`,
  `doc_anchors`, `definitions`, `citations`, plus pack file +
  `manifest.json` + per-doc asset blobs

Release:
- `ato-mcp publish-release --out-dir ... --tag ... --repo ...` —
  rewrite manifest URLs to GitHub release asset URLs, fix the embedding
  model fields if they're placeholder, optionally minisign-sign, then
  `gh release create` + `gh release upload`
- `ato-mcp bundle-localize-manifest --manifest ... --packs-dir ...
  --model-bundle ...` — rewrite a manifest for an offline air-gapped
  bundle (recompute SHA256 + size from local files, emit `update.json`)
- `ato-mcp backfill-citations [--db PATH]` — recompute the citations
  table on an existing DB

Lower-level helpers (used by tests + scripts):
- `extract`, `extract-anchors`, `extract-definitions`, `extract-currency`
- `chunk-html`, `doc-meta`, `doc-id-from-link`
- `pack-write`, `manifest-rewrite-urls`, `bundle-model`
- `ato-fetch-nodes`, `embed`

## Known quality regressions vs Python

None as of v0.8.1. The v0.8.0 title-polish regression (Rulings / PCG /
TA / PS LA / ATO ID titles missing their citation prefix) is fixed —
`rules.py` is now ported to Rust and wired into `ato-mcp build`.

The search index, chunk embeddings, anchors, definitions, citations,
titles, dates, navigation flags, currency markers, and pack file format
are now all bit-identical with the Python pipeline output.

## Tests

- `cargo test --locked`: 95 pass (90 inline unit tests + 3 stdio_shim
  + 2 http_smoke). The Python `pytest` suite (212 tests) is gone but
  the relevant assertions are mirrored in the Rust unit tests, including
  12 new `rules_tests` covering shape classification, template routing,
  and title composition.
