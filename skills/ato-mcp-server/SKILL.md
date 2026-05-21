---
description: "Ensure the local ato-mcp HTTP server is reachable before using ato-mcp tools. Run this when you encounter an Australian tax-law question, an ATO ruling reference, or an unreachable-MCP-server error from the ato plugin."
---

# Starting the ato-mcp server

The `ato` MCP plugin connects to a local HTTP server (`ato-mcp serve`) that the
user starts manually from a terminal. If that server isn't running, every `ato`
tool call will fail with a connection error.

When you need an `ato` tool and aren't sure the server is up:

## 1. Check reachability

The plugin's URL resolves from the user's `ATO_MCP_PORT` env var. Probe:

```bash
curl -sf -o /dev/null -X POST -H 'content-type: application/json' \
    -d '{}' "http://127.0.0.1:${ATO_MCP_PORT:-51234}/mcp"
```

The server replies with a JSON-RPC parse error (HTTP 200) when it's up.
`curl` exits 7 (could not connect) when it's down. Treat any other status as
a transient and ask the user to investigate.

## 2. If unreachable, ask the user

Tell the user the server isn't running and ask whether to start it in the
background:

> The local ATO MCP server isn't reachable. Should I start it for you?
> It'll run in the background on port `${ATO_MCP_PORT:-51234}`; you can stop
> it from the terminal it spawns in or by killing the process.

On approval:

```bash
ato-mcp serve
```

Run via the **Bash tool with `run_in_background: true`** so the agent's
foreground continues. Wait ~2 seconds, then re-probe with the curl command
above to confirm readiness. If still down, surface the background process's
stderr to the user.

## 3. If `ATO_MCP_PORT` isn't set

The plugin's `.mcp.json` uses `${env:ATO_MCP_PORT}`. If the variable isn't
in the user's environment, the plugin URL can't be resolved. Detect this by:

```bash
test -n "${ATO_MCP_PORT:-}" && echo "set" || echo "unset"
```

If unset, ask the user to run the one-shot setup:

> The plugin needs an `ATO_MCP_PORT` env var to know which local port the
> server listens on. Should I run `ato-mcp install` to pick a free port and
> tell you the `export` line to add to your shell rc? After you set it,
> restart your shell (and Claude Code) so the plugin can read it.

On approval:

```bash
ato-mcp install
```

The command prints the `export ATO_MCP_PORT=<n>` line and the resolved URL.
Tell the user to add that line to `~/.bashrc` / `~/.zshrc` / their shell's
rc, then restart their terminal and Claude Code so the plugin picks up the
new URL.

## 4. If the corpus isn't installed yet

`ato-mcp stats` returns "corpus is not yet installed" guidance on a fresh
machine. The first MCP `initialize` response carries the same message.
Offer to run `ato-mcp update` for the user (~4 GB download, 5-10 minutes):

```bash
ato-mcp update
```

After the download completes, restart `ato-mcp serve` so it picks up the new
corpus, then retry the user's original question.

## What NOT to do

- Don't poll the server with `sleep` loops. If it's down, ask the user once
  and stop.
- Don't run `ato-mcp serve` in the foreground (it blocks the Bash tool).
  Always use `run_in_background: true`.
- Don't try to write the user's shell rc for them; print the `export` line
  and let them paste it. Modifying shell rcs without explicit per-line
  consent is rude.
- Don't fall back to web-search for ATO content when the local server is
  down without telling the user — the corpus is the authority; degrading
  silently hides the actual problem.
