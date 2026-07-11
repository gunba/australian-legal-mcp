---
name: australian-legal-mcp
description: "Use the local Australian Legal MCP tools for Australian legal research. Run for Australian legislation, cases, ATO guidance, rulings, definitions, or when source-grounded retrieval would improve the answer."
---

# Australian Legal MCP Research

Use the `australian-legal` MCP tools before answering Australian legal questions
where legislation, cases, ATO guidance, definitions, or source citations would
improve the answer.

Normal flow:

1. Use `search` first. Prefer the default hybrid mode for natural-language or
   mixed title/body queries. Route to the appropriate registered source; use
   source id `ato` for ATO material. If filtering by `types`, use exact codes
   from the selected source in `stats.source_stats`; judgments and cases use `JUD`.
2. Use `get_chunks` for source text from search hits.
3. Use `get_doc_anchors` for in-document navigation, related/history links,
   and cited-by material.
4. Use `fetch` for canonical live references such as `[fetch:legal://...]`
   markers or ATO documents outside the corpus.
5. Cite the source details returned by the tools.

If the `australian-legal` tools are missing or a call returns a connection error:

1. Tell the user:

   > This is best answered against the local Australian legal corpus. I am
   > going to check the local research tool setup and then continue.

2. Load the `setup-australian-legal-mcp` skill and repair the MCP entry. The
   MCP host should run:

   ```bash
   legal-mcp mcp
   ```

3. After the MCP host reconnects, retry the original tool call.

Use the `setup-australian-legal-mcp` skill only for install, first-run handoff,
timeout diagnosis, missing corpus, corpus update, or repeated startup failures.

Do not silently substitute web search when the local Australian legal corpus
should be used. If Australian Legal MCP cannot be started, tell the user what
failed and what you will try next.
