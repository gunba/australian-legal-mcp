# systemd user units for Australian Legal MCP

These are optional Linux user units. They install under
`~/.config/systemd/user/` and run as the current user; they are not part of
the normal agent install path.

## Optional backend prewarm

Normal MCP clients should launch:

```bash
legal-mcp mcp
```

That stdio command starts or reuses one local loopback HTTP backend and writes
the active endpoint to
`~/.local/share/australian-legal-mcp/http.json`.

To have the backend running before the first MCP request, install the optional
user service:

```bash
mkdir -p ~/.config/systemd/user
cp legal-mcp-serve.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now legal-mcp-serve.service
systemctl --user status legal-mcp-serve.service
```

## Optional corpus update timer

Install the update and serve units together so a successful update refreshes
the running backend before verification:

```bash
mkdir -p ~/.config/systemd/user
cp legal-mcp-serve.service legal-mcp-update.service legal-mcp-update.timer \
  ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now legal-mcp-serve.service legal-mcp-update.timer
systemctl --user list-timers --all | grep legal-mcp
```

Both services load the optional
`~/.config/australian-legal-mcp/environment` file. For a portable corpus, create
it once:

```bash
mkdir -p ~/.config/australian-legal-mcp
printf 'LEGAL_MCP_DATA_DIR=%s\n' \
  "$HOME/path/to/stable/australian-legal-mcp-data" \
  > ~/.config/australian-legal-mcp/environment
systemctl --user daemon-reload
systemctl --user restart legal-mcp-serve.service
```

The update service runs `legal-mcp update`, tries to restart
`legal-mcp-serve.service`, then runs `legal-mcp stats` in the same data
directory.

## Maintainer timer

Only install these units on the machine that publishes corpus releases.

```bash
cp legal-mcp-maintainer-*.service legal-mcp-maintainer-*.timer \
  ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now legal-mcp-maintainer-weekly.timer
```

Set the maintainer service's `Environment=` and `ExecStart=` paths for the
repository checkout, `ato_pages`, mandatory FRL workspace, model directory,
and optional model mirror URL before enabling the timer.

## Manual checks

```bash
systemctl --user start legal-mcp-serve.service
systemctl --user start legal-mcp-update.service
systemctl --user start legal-mcp-maintainer-weekly.service

systemctl --user status legal-mcp-serve.service
journalctl --user -u legal-mcp-serve.service -n 50 --no-pager
systemctl --user status legal-mcp-update.timer
journalctl --user -u legal-mcp-update.service -n 50 --no-pager
journalctl --user -u legal-mcp-maintainer-weekly.service -n 100 --no-pager
```
