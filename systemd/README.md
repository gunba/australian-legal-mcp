# systemd user units for ato-mcp

This folder ships systemd **user** timers. They install under
`~/.config/systemd/user/` and run as your user account — no sudo
required.

## Run the MCP daemon

`ato-mcp` is an HTTP MCP server: one long-lived process, every Claude
session connects over localhost. Pick a port and start the daemon:

```bash
ato-mcp install-http              # picks a free port, prints the MCP config
cp ato-mcp-serve.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ato-mcp-serve.service
systemctl --user status ato-mcp-serve.service
```

`install-http` writes `~/.local/share/ato-mcp/http.json` and prints the
exact URL to paste into your Claude Code / Claude Desktop MCP config.

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

