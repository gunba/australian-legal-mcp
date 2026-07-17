---
name: australian-legal-mcp
description: "Use Australian Legal MCP for source-grounded Australian legislation, cases, ATO guidance, rulings, definitions, and citations."
---

# Australian Legal MCP research

Use the `australian-legal` MCP tools before answering Australian legal questions
when primary sources or exact citations would improve the answer.

1. Call `stats` when source/type availability is unclear.
2. Call `search` first. Always pass exactly one registered `source`; omission is
   an error. Prefer hybrid mode for natural-language or mixed title/body queries.
3. Call `get_chunks` for exact source text and bounded neighbouring context.
4. Call `get_doc_anchors` for in-document navigation, history/related links,
   and cited-by material.
5. Call `get_asset` when a retained image marker is material.
6. Call `get_definition` for statutory definitions and clearly labelled
   ordinary-meaning fallback.
7. Call `fetch` only for canonical `legal://...` references, including
   `[fetch:legal://...]` markers.

Cite the returned source title, exact stored canonical URL, date/currency
metadata, and typed document/chunk references. Do not infer federation or use an
ATO default.

If the tools are missing or connection fails, tell the user that you are
checking the Australian legal research service, then load
`setup-australian-legal-mcp`. Diagnose the hosted Linode OCI endpoint,
API-key/Entra challenge, Caddy, and serving-host readiness first. `legal-mcp mcp` is
valid only for deliberately configured local stdio development.

The runtime cannot download or update a corpus. A missing active generation
requires maintainer build/validation, immutable activation or rollback, and—on
the hosted service—restricted CoW/rsync generation deployment. Never recommend
`legal-mcp update`.

Do not silently substitute general web search when the source-grounded service
should be used. If recovery is impossible, state the exact failure and the
bounded next step.
