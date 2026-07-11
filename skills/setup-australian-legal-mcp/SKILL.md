---
name: setup-australian-legal-mcp
description: "Detailed setup and recovery for australian-legal-mcp. Use only when installing the Australian Legal MCP plugin, repairing MCP startup, diagnosing a 30-second MCP timeout, fixing a missing or stale corpus, or recovering repeated legal-mcp startup failures."
metadata:
  parent_skill: australian-legal-mcp
---

# Setup Australian Legal MCP

This skill is for setup and failure recovery only. Do not load it for ordinary
legal research when the `australian-legal` MCP tools are already working.

## User Communication

Do the technical work yourself and keep the user informed. Do not ask legal
practitioners to choose ports, edit config, or run terminal commands.

Use concise status messages:

> I am checking the local Australian legal research tool setup. The MCP host
> should start the local service automatically; I will report anything that needs
> a session restart.

## Server Startup

`legal-mcp mcp` is the MCP host entry point. It is a stdio command that starts
or reuses one local loopback HTTP backend and proxies MCP messages to that
backend. `legal-mcp serve` is the backend HTTP server for advanced or manual
use; do not configure `serve` directly as a stdio MCP command.

Before configuring MCP, resolve one installed executable path and verify it.
Release archives named `legal-mcp-<version>-<target>` must be checked against
the release `SHA256SUMS` before extraction. Install the executable into a stable
per-user `PATH` directory (`~/.local/bin` on Linux/macOS, or a fixed Programs
directory under local app data on Windows), then confirm `command -v legal-mcp` /
`Get-Command legal-mcp` resolves that exact file and run `legal-mcp --version`.
On Linux keep the verified `libonnxruntime.so` beside `legal-mcp`; on Windows
keep `onnxruntime.dll` beside `legal-mcp.exe`. Never configure an executable
inside a temporary extraction directory.

The binary path and corpus data directory are separate. Enterprise policy may
require the binary to live under local app data. That is fine, but the installer
must choose one corpus data directory and use it consistently.

The default data directories are `%APPDATA%\australian-legal-mcp` on Windows,
`~/.local/share/australian-legal-mcp` on Linux, and
`~/Library/Application Support/australian-legal-mcp` on macOS.

Two valid modes:

- Default data dir: leave `LEGAL_MCP_DATA_DIR` unset for `legal-mcp update`,
  `legal-mcp mcp`, `stats`, and MCP calls.
- Portable/co-located data dir: set `LEGAL_MCP_DATA_DIR` to a stable data
  directory next to the binary for every `legal-mcp update`, `legal-mcp mcp`,
  `stats`, and verification call.

Do not install the corpus under a temporary extraction directory. Do not run
`legal-mcp update` with a non-default `LEGAL_MCP_DATA_DIR` and later start the
MCP entry point without that same setting.

MCP startup behavior:

- `.mcp.json` must contain an `australian-legal` server entry with
  `command: legal-mcp` and `args: ["mcp"]`.
- `legal-mcp mcp` checks `<data_dir>/http.json` for an existing backend.
- If that backend answers MCP `initialize` with the same binary version, the
  stdio command reuses it.
- Otherwise, `legal-mcp mcp` takes `<data_dir>/SERVER_LOCK`, starts
  `legal-mcp serve --port <free-port>` in the background, waits for the
  deterministic readiness line, writes `<data_dir>/http.json`, and proxies the
  MCP call.
- `legal-mcp serve --port <N>` is only for manual HTTP testing.

Manual stdio smoke:

```bash
request='{"jsonrpc":"2.0","id":1,"method":"initialize",'
request+='"params":{"protocolVersion":"2025-06-18","capabilities":{},'
request+='"clientInfo":{"name":"smoke","version":"1"}}}'
printf '%s\n' "$request" | legal-mcp mcp
```

If MCP host config changed, ask the user to exit and resume the agent session
so the host reloads the MCP command. The stdio entry remains stable when the
backend selects a new loopback port.

## 30-Second Timeout

Interpret the timeout from the config state:

- If `.mcp.json` points at HTTP or `legal-mcp serve`, replace it with the stdio
  entry running `legal-mcp mcp`, then ask the user to exit and resume the agent
  session.
- If `.mcp.json` already runs `legal-mcp mcp`, check `legal-mcp --version`,
  then run the manual stdio smoke above.
- If the stdio smoke starts but tool calls fail, check `LEGAL_MCP_DATA_DIR`,
  `stats`, `<data_dir>/server.log`, and whether the corpus exists in that data
  directory.
- For a timeout during update, run `legal-mcp update` separately so progress
  is visible; do not perform corpus update inside MCP startup.

Do not use fixed sleeps. Use command output, process exit, or MCP host health.

## Missing Corpus

If `stats` or MCP initialize says the corpus is missing, tell the user:

> The Australian legal research tool is installed, but its local corpus has not
> been downloaded yet. It is a large one-time download. I can run it now and
> then verify the tool before we continue.

On approval:

```bash
legal-mcp update
```

After update, verify `legal-mcp stats`. If the MCP host was already running, ask
the user to exit and resume after update so it reconnects to a backend using the
refreshed corpus.

If an earlier setup downloaded the corpus under a temporary or one-shot
`LEGAL_MCP_DATA_DIR`, fix the install by choosing the intended stable data dir:
either rerun `legal-mcp update` with `LEGAL_MCP_DATA_DIR` unset, or set
`LEGAL_MCP_DATA_DIR` to the stable co-located data dir for both update and MCP
startup. Then restart the MCP host and verify `legal-mcp stats` reports the
intended `data_dir`.

## Newer Corpus Available

If a newer corpus is available, ask whether to update now or after the current
answer. If updating:

```bash
legal-mcp update
```

Then restart the MCP host or backend and verify `legal-mcp stats`.

## Verification

Before saying setup is ready:

```bash
legal-mcp --version
legal-mcp stats
legal-mcp search "research and development tax incentive eligibility" --source ato --k 1
legal-mcp search "income tax assessment act" --source frl --k 1
```

If available, run the MCP host health check and confirm the `australian-legal`
MCP server is connected.

## Do Not

- Do not make the user run commands or edit config.
- Do not configure `legal-mcp serve` as a stdio MCP command.
- Do not run `legal-mcp update` as a hidden background startup task.
