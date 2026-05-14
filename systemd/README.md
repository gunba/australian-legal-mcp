# systemd user units for ato-mcp

This folder ships systemd **user** timers. They install under
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

## End-user install (pulls the latest release weekly)

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

## Maintainer Install (Weekly Incremental Publish)

Only install these on the machine you publish releases from. They
refresh the ATO What's New feed, incrementally rebuild the corpus when the
source changed, and push a new GitHub release.

```bash
cp ato-mcp-maintainer-*.service ato-mcp-maintainer-*.timer \
   ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ato-mcp-maintainer-weekly.timer
```

Edit the `Environment=` and `ExecStart=` lines in the maintainer service if
your repo path, ato_pages path, model path, or optional model mirror URL
differs from the defaults.

## Triggering manually

```bash
systemctl --user start ato-mcp-serve.service
systemctl --user start ato-mcp-update.service
systemctl --user start ato-mcp-maintainer-weekly.service
```

## Checking results

```bash
systemctl --user status ato-mcp-serve.service
journalctl --user -u ato-mcp-serve.service -n 50 --no-pager
systemctl --user status ato-mcp-update.timer
journalctl --user -u ato-mcp-update.service -n 50 --no-pager
journalctl --user -u ato-mcp-maintainer-weekly.service -n 100 --no-pager
```

