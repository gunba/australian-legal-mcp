---
description: "Detailed setup and recovery for ato-mcp. Use only when installing the ATO MCP plugin, handling first-run port setup, diagnosing a 30-second MCP timeout, fixing missing/stale corpus, or recovering repeated ato-mcp startup failures."
---

# Setup ATO MCP

This skill is for setup and failure recovery only. Do not load it for ordinary
tax research when the `ato` MCP tools are already working.

## User Communication

Do the technical work yourself and keep the user informed. Do not ask tax
practitioners to choose ports, edit config, or run terminal commands.

Use concise status messages:

> I am starting the local ATO research service in the background. If this
> agent session needs to reload the generated local port, I will ask you to
> restart the session once.

For the required first-run restart:

> The local ATO service is running and I have updated the MCP plugin config
> with its generated port. This agent session needs one restart to load that
> local URL. Please exit and resume this session now; after it reopens, I will
> verify the ATO tools. You do not need to run any commands.

## Server Startup

`ato-mcp serve` is an HTTP MCP server, not stdio.

The binary path and corpus data directory are separate. Enterprise policy may
require the binary to live under local app data. That is fine, but the
installer must choose one corpus data directory and use it consistently.

Two valid modes:

- Default data dir: leave `ATO_MCP_DATA_DIR` unset for `ato-mcp update`,
  `ato-mcp serve`, `stats`, and MCP calls.
- Portable/co-located data dir: set `ATO_MCP_DATA_DIR` to a stable data
  directory next to the binary for every `ato-mcp update`, `ato-mcp serve`,
  `stats`, and verification call.

Do not install the corpus under a temporary extraction directory. Do not run
`ato-mcp update` with a non-default `ATO_MCP_DATA_DIR` and later start
`ato-mcp serve` without that same setting.

Port behavior:

- `.mcp.json` initially contains `http://127.0.0.1:0/mcp`; `:0` is a sentinel.
- First `ato-mcp serve` picks a free loopback port and rewrites `.mcp.json`.
- Later starts reuse the stored port if available.
- If the stored port is occupied, `serve` picks a new free port and rewrites
  `.mcp.json`.
- `ato-mcp serve --port <N>` uses the explicit port and does not rewrite.

Start in the background:

```bash
ato-mcp serve
```

Wait for the deterministic readiness line:

```text
ato-mcp listening on http://127.0.0.1:<port>/mcp
```

If the output says it wrote the new URL to `.mcp.json`, stop and ask the user
to exit/resume the agent session. The current session may still have the old
URL.

## 30-Second Timeout

Interpret the timeout from the config state:

- `.mcp.json` still has `:0`: first-run server startup did not complete.
  Start `ato-mcp serve`, wait for the rewrite line, then ask for exit/resume.
- `.mcp.json` has a real port: server is not running or the port was taken.
  Start `ato-mcp serve`; it will reuse or rewrite the port.
- Timeout during update: do not perform corpus update inside MCP startup.
  Run `ato-mcp update` separately so progress is visible.

Do not use fixed sleeps. Use command output, process exit, or MCP host health.

## Missing Corpus

If `stats` or MCP initialize says the corpus is missing, tell the user:

> The ATO tool is installed, but the local ATO corpus has not been downloaded
> yet. It is a large one-time download, about 1.5 GB and usually 5-10 minutes.
> I can run it now and then verify the tool before we continue.

On approval:

```bash
ato-mcp update
```

After update, restart `ato-mcp serve`, verify `ato-mcp stats`, then continue.
If restart rewrites the port, ask the user to exit/resume first.

If an earlier setup downloaded the corpus under a temporary or one-shot
`ATO_MCP_DATA_DIR`, fix the install by choosing the intended stable data dir:
either rerun `ato-mcp update` with `ATO_MCP_DATA_DIR` unset, or set
`ATO_MCP_DATA_DIR` to the stable co-located data dir for both update and
serve. Then restart `ato-mcp serve` and verify `ato-mcp stats` reports the
intended `data_dir`.

## Newer Corpus Available

If a newer corpus is available, ask whether to update now or after the current
answer. If updating:

```bash
ato-mcp update
```

Then restart `ato-mcp serve` and verify `ato-mcp stats`.

## Verification

Before saying setup is ready:

```bash
ato-mcp --version
ato-mcp stats
ato-mcp search "research and development tax incentive eligibility" --k 1
```

If available, run the MCP host health check and confirm the `ato` MCP is
connected.

## Do Not

- Do not make the user run commands or edit config.
- Do not configure `ato-mcp serve` as a stdio MCP.
- Do not call ATO MCP tools after a first-run port rewrite until the agent
  session has been exited and resumed.
- Do not run `ato-mcp update` as a hidden background startup task.
