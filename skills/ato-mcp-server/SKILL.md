---
description: "Use the local ato-mcp tools for Australian tax-law research. Run for ATO/tax questions, ATO document references, rulings, definitions, or when local ATO source retrieval would improve the answer."
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

   > This is best answered against the local ATO corpus. I am going to start
   > the local ATO research service and then continue.

2. Start the server in the background:

   ```bash
   ato-mcp serve
   ```

3. Wait for the readiness line:

   ```text
   ato-mcp listening on http://127.0.0.1:<port>/mcp
   ```

4. If the output says it wrote a new URL to `.mcp.json`, tell the user:

   > The local ATO service is running. This agent session needs one restart to
   > load its generated local port. Please exit and resume this session; after
   > it reopens, I will verify the ATO tools. You do not need to run any
   > commands.

   Stop there. Do not call the `ato` tools again in this pre-restart session.

5. If no URL rewrite occurred, retry the original tool call.

Use the `setup-ato-mcp` skill only for install, first-run handoff, timeout
diagnosis, missing corpus, corpus update, or repeated startup failures.

Do not silently substitute web search when the local ATO corpus should be
used. If ATO MCP cannot be started, tell the user what failed and what you
will try next.
