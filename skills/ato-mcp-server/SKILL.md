---
name: ato-mcp-server
description: "Use the local ato-mcp tools for Australian tax-law research. Run for ATO/tax questions, ATO document references, rulings, definitions, or when local ATO source retrieval would improve the answer."
disable-model-invocation: true
---

# ATO MCP Research

Use the `ato` MCP tools before answering Australian tax-law questions where
ATO guidance, rulings, determinations, legislation, definitions, or document
citations would improve the answer.

Normal flow:

1. Use `search` first.
2. Use `get_chunks` for source text from search hits.
3. Use `get_doc_anchors` for in-document navigation, related/history links,
   and cited-by material.
4. Use `fetch` for `[fetch:ato:...]` markers or live ATO documents outside
   the corpus.
5. Cite the ATO source details returned by the tools.

If the `ato` tools are missing or a call returns a connection error:

1. Tell the user:

   > This is best answered against the local ATO corpus. I am going to check
   > the local ATO tool setup and then continue.

2. Load the `setup-ato-mcp` skill and repair the MCP entry. The MCP host
   should run:

   ```bash
   ato-mcp mcp
   ```

3. After the MCP host reconnects, retry the original tool call.

Use the `setup-ato-mcp` skill only for install, first-run handoff, timeout
diagnosis, missing corpus, corpus update, or repeated startup failures.

Do not silently substitute web search when the local ATO corpus should be
used. If ATO MCP cannot be started, tell the user what failed and what you
will try next.
