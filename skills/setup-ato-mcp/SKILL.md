---
description: "Detailed setup and recovery for ato-mcp. Use only when installing the ATO MCP plugin, repairing MCP startup, diagnosing a 30-second MCP timeout, fixing missing/stale corpus, or recovering repeated ato-mcp startup failures."
---

# Setup ATO MCP

This skill is for setup and failure recovery only. Do not load it for ordinary
tax research when the `ato` MCP tools are already working.

## User Communication

Do the technical work yourself and keep the user informed. Do not ask tax
practitioners to choose ports, edit config, or run terminal commands.

Use concise status messages:

> I am checking the local ATO research tool setup. The MCP host should start
> the local service automatically; I will report anything that needs a session
> restart.

## Server Startup

`ato-mcp mcp` is the MCP host entry point. It is a stdio command that starts
or reuses one local loopback HTTP backend and proxies MCP messages to that
backend. `ato-mcp serve` is the backend HTTP server for advanced/manual use;
do not configure `serve` directly as a stdio MCP command.

The binary path and corpus data directory are separate. Enterprise policy may
require the binary to live under local app data. That is fine, but the
installer must choose one corpus data directory and use it consistently.

Two valid modes:

- Default data dir: leave `ATO_MCP_DATA_DIR` unset for `ato-mcp update`,
  `ato-mcp mcp`, `stats`, and MCP calls.
- Portable/co-located data dir: set `ATO_MCP_DATA_DIR` to a stable data
  directory next to the binary for every `ato-mcp update`, `ato-mcp mcp`,
  `stats`, and verification call.

Do not install the corpus under a temporary extraction directory. Do not run
`ato-mcp update` with a non-default `ATO_MCP_DATA_DIR` and later start the MCP
entry point without that same setting.

MCP startup behavior:

- `.mcp.json` should contain an `mcpServers.ato` entry with
  `command: ato-mcp` and `args: ["mcp"]`.
- `ato-mcp mcp` checks `<data_dir>/http.json` for an existing backend.
- If that backend answers MCP `initialize` with the same binary version, the
  stdio command reuses it.
- If not, `ato-mcp mcp` takes `<data_dir>/SERVER_LOCK`, starts
  `ato-mcp serve --port <free-port>` in the background, waits for the
  deterministic readiness line, writes `<data_dir>/http.json`, and proxies the
  MCP call.
- `ato-mcp serve --port <N>` is only for manual HTTP testing.

Manual stdio smoke:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}' | ato-mcp mcp
```

If MCP host config changed, ask the user to exit/resume the agent session so
the host reloads the MCP command. There is no generated-port restart.

## 30-Second Timeout

Interpret the timeout from the config state:

- `.mcp.json` points at HTTP or `ato-mcp serve`: this is legacy config.
  Replace it with a stdio entry running `ato-mcp mcp`, then ask the user to
  exit/resume the agent session.
- `.mcp.json` already runs `ato-mcp mcp`: check `ato-mcp --version`, then run
  the manual stdio smoke above.
- Stdio smoke starts but tool calls fail: check `ATO_MCP_DATA_DIR`, `stats`,
  `<data_dir>/server.log`, and whether the corpus exists in the same data dir.
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

After update, verify `ato-mcp stats`. If the MCP host was already running, ask
the user to exit/resume after update so it reconnects to a backend using the
refreshed corpus.

If an earlier setup downloaded the corpus under a temporary or one-shot
`ATO_MCP_DATA_DIR`, fix the install by choosing the intended stable data dir:
either rerun `ato-mcp update` with `ATO_MCP_DATA_DIR` unset, or set
`ATO_MCP_DATA_DIR` to the stable co-located data dir for both update and
MCP startup. Then restart the MCP host and verify `ato-mcp stats` reports the
intended `data_dir`.

## Newer Corpus Available

If a newer corpus is available, ask whether to update now or after the current
answer. If updating:

```bash
ato-mcp update
```

Then restart the MCP host/backend and verify `ato-mcp stats`.

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
- Do not run `ato-mcp update` as a hidden background startup task.
