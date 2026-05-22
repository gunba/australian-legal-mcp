---
description: "Ensure the local ato-mcp HTTP server is reachable before using ato-mcp tools. Run this when you encounter an Australian tax-law question, an ATO ruling reference, or an unreachable-MCP-server error from the ato plugin."
---

# Starting the ato-mcp server

The `ato` plugin connects to a local HTTP server started by the user
(`ato-mcp serve`). If that server isn't running, the `ato` tools won't be
available in this MCP session.

## When the `ato` tools aren't in your tool list

That means the MCP host couldn't connect to the URL in the plugin's
`.mcp.json`. The first run hasn't happened, so the URL still has the `:0`
sentinel. The user needs to start the server, which auto-picks a free port
and writes the real URL back into `.mcp.json`. After that they exit and
resume the session and the tools show up.

Walk the user through it in one turn:

1. Ask:
   > The ATO plugin's local server isn't running. Should I start it for you
   > in the background? It'll pick a free port and update the plugin
   > config; you'll need to exit and resume this Claude Code session
   > afterwards so the new URL takes effect.

2. On approval, run via the **Bash tool with `run_in_background: true`**:
   ```bash
   ato-mcp serve
   ```
   `serve` prints the chosen URL on stderr and rewrites the plugin's
   `.mcp.json`. The background process keeps running.

3. Tell the user:
   > Server is up. Exit and resume this Claude Code session, then ask me
   > your question again — the ATO tools will be loaded.

Don't wait for them; that's the end of this turn.

## When the `ato` tools ARE in the list but a call returns a connection error

The server was running and stopped (machine restart, terminal closed,
process killed). Same flow as above — `ato-mcp serve` will reuse the port
that's already in `.mcp.json`, so no restart is needed unless the port has
been taken by something else. Run it in the background and retry the user's
last query.

## When the corpus isn't installed yet

`stats` (or the MCP `initialize` instructions) reports "corpus is not yet
installed" on a fresh machine. Offer to run the download for the user
(~1.5 GB, 5-10 min):

```bash
ato-mcp update
```

After the download completes, restart `ato-mcp serve` so it picks up the
new corpus, then retry the user's original question.

## AustLII search

`search_austlii` uses native AustLII SINO full-text search when a validated
AustLII session exists. Results are AustLII URLs normalised to
`austlii:<path>` fetch URIs and should be fetched and verified before use. If
native SINO is not configured or fails, the tool falls back to AustLII title
indexes.

If `stats` reports `austlii.native_search_available=false`, run
`ato-mcp austlii setup`. Setup first tries local browser cookies. If no valid
Cloudflare session is available, it opens the AustLII SINO validation URL in
the user's browser, waits for the user to complete Cloudflare verification,
then re-reads the browser cookie store and validates the session before saving
metadata. Manual fallback for locked-down environments:
`ato-mcp austlii setup --cookie '<cf_clearance>' --user-agent '<matching UA>'`
or `ato-mcp austlii setup --cookie-header '<Cookie header>' --user-agent '<matching UA>'`.

## What not to do

- Don't run `ato-mcp serve` in the foreground (it blocks the Bash tool).
  Always use `run_in_background: true`.
- Don't poll the server with `sleep` loops; if it's down, run `serve` once
  and tell the user to resume the session.
- Don't fall back to web-search for ATO content silently. The corpus is the
  authority; if it's unreachable, tell the user.
