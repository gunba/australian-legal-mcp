---
description: "Run scripts/smoke.sh end-to-end against the installed ato-mcp binary + corpus. Use after a release, after pulling main, or when verifying the local install."
---

End-to-end smoke verification for `ato-mcp`. Use when:

- You just published a new release tag and want to confirm the binary on
  `$PATH` works against the live corpus.
- You pulled `main` and rebuilt the binary; you want to verify nothing regressed
  before restarting your MCP clients.
- A user reports something broken and you want a baseline.

## Run

```bash
scripts/smoke.sh
```

The script is the source of truth — it documents which surfaces are tested and
which behaviours are checked. Read the top-of-file comment block before
adding new tests.

## Configuration

- `ATO_MCP_BIN` — path to the binary under test. Defaults to
  `$HOME/.local/bin/ato-mcp`.
- `ATO_MCP_SKIP_NETWORK=1` — skip the two `fetch_external_doc` tests when the
  ATO website is unreachable or you're on a metered link.

## What gets checked

The script prints `PASS`/`FAIL` per assertion grouped into five sections:

1. **Binary identity** — `--version` shape, the full list of expected
   subcommands is present, and every subcommand deleted in v0.10.0 is absent.
2. **Corpus health** — `stats` JSON shape (documents, chunks, embeddings,
   definitions, prefix breakdown, model id, index version) and `doctor`
   reports `semantic_search: ready`.
3. **CLI search** — hybrid / vector / keyword modes, `--types` filter,
   `--doc-scope` glob, `--sort-by recency` ordering, `--seed-text` vector-only
   fast path, `--include-old` policy relaxation, direct doc_id title hit, and
   the `meta.next_call` continuation hint.
4. **CLI retrieval** — `get-definition` returns statutory hits with `[doc:...]`
   citation markers; the regression check exercises the unified definition
   term normaliser fixed in v0.10.0; `fetch-external-doc` round-trips against
   `ato.gov.au`.
5. **MCP HTTP transport** — spawns a fresh daemon in a tempdir (symlinking the
   user's live corpus in read-only), then issues JSON-RPC `initialize`,
   `tools/list` (must be exactly the 7 supported tools), and `tools/call`
   for each of `stats`, `search`, `get_chunks`, `get_definition`,
   `get_doc_anchors`, `get_asset`, and `fetch_external_doc`, plus an
   `unknown_tool` request to confirm structured JSON-RPC errors.

## Interpreting failures

- Any `FAIL` line includes the assertion name and a short snippet of the
  actual output (truncated to 120 chars).
- A summary at the bottom lists every failed assertion by name.
- The script exits non-zero on any failure.

When a section fails, run that part by hand against the binary to see the full
output — every test is a single `ato-mcp ...` invocation or `curl` to the
daemon, copy-pasteable from the script.

## When to extend the script

Add a new test whenever:

- A new MCP tool is added (must appear in the `tools/list` expected set and
  get its own `tools/call` round trip).
- A new CLI subcommand is added or an old one is removed (update the
  present/absent lists in Section 1).
- A bug fix lands that's easy to regress silently (the v0.10.0 definition
  term normaliser fix is the prototype).

Keep individual assertions cheap: total runtime should stay under ~30 s with
the network tests enabled. Use `--k 3` for search and `--max-defs 2/3` for
definitions to keep work small.
