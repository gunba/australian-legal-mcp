# systemd user units for ato-mcp

This folder ships systemd **user** units. They install under
`~/.config/systemd/user/` and run as your user account — no sudo
required.

## Run the MCP daemon

`ato-mcp` ships as a one-process design: the daemon runs the HTTP MCP
server, and the `ato-mcp serve` stdio shim that MCP clients launch
auto-spawns it on first use. You don't have to run anything yourself
unless you want the daemon up before any client connects.

Most users can skip this section entirely — Claude Code / Cursor /
Codex launch `ato-mcp serve` themselves and the shim handles
everything.

If you want the daemon up at login (faster first-request latency, no
spawn during MCP startup), install the service unit:

```bash
cp ato-mcp-serve.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ato-mcp-serve.service
systemctl --user status ato-mcp-serve.service
```

The daemon picks a free port the first time it (or the shim) runs and
persists it to `~/.local/share/ato-mcp/http.json`.

## Auto-update the corpus weekly

```bash
mkdir -p ~/.config/systemd/user
cp ato-mcp-update.service ato-mcp-update.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ato-mcp-update.timer
systemctl --user list-timers --all | grep ato-mcp
```

Keep the user manager alive between logins:

```bash
loginctl enable-linger "$USER"
```

## Triggering manually

```bash
systemctl --user start ato-mcp-serve.service
systemctl --user start ato-mcp-update.service
```

## Checking results

```bash
systemctl --user status ato-mcp-serve.service
journalctl --user -u ato-mcp-serve.service -n 50 --no-pager
systemctl --user status ato-mcp-update.timer
journalctl --user -u ato-mcp-update.service -n 50 --no-pager
```
