# systemd user units for ato-mcp

These are optional Linux user units. They install under
`~/.config/systemd/user/` and run as the current user; they are not part of
the normal agent install path.

## Optional backend prewarm

Normal MCP clients should launch:

```bash
ato-mcp mcp
```

That stdio command starts or reuses one local loopback HTTP backend and writes
the active endpoint to `~/.local/share/ato-mcp/http.json`.

If you want the backend running before the first MCP request, install the
optional user service:

```bash
mkdir -p ~/.config/systemd/user
cp ato-mcp-serve.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ato-mcp-serve.service
systemctl --user status ato-mcp-serve.service
```

## Optional corpus update timer

```bash
mkdir -p ~/.config/systemd/user
cp ato-mcp-update.service ato-mcp-update.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ato-mcp-update.timer
systemctl --user list-timers --all | grep ato-mcp
```

Restart the MCP host/backend after an update so it uses the refreshed corpus.

## Maintainer timer

Only install these on the machine that publishes corpus releases.

```bash
cp ato-mcp-maintainer-*.service ato-mcp-maintainer-*.timer \
   ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ato-mcp-maintainer-weekly.timer
```

Edit the `Environment=` and `ExecStart=` lines in the maintainer service if
your repo path, `ato_pages` path, model path, or optional model mirror URL
differs from the defaults.

## Manual checks

```bash
systemctl --user start ato-mcp-serve.service
systemctl --user start ato-mcp-update.service
systemctl --user start ato-mcp-maintainer-weekly.service

systemctl --user status ato-mcp-serve.service
journalctl --user -u ato-mcp-serve.service -n 50 --no-pager
systemctl --user status ato-mcp-update.timer
journalctl --user -u ato-mcp-update.service -n 50 --no-pager
journalctl --user -u ato-mcp-maintainer-weekly.service -n 100 --no-pager
```
