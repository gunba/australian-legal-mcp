# systemd units

## Hosted runtime

`legal-mcp.service` is a system service for a dedicated `legal-mcp` account. It
serves only a locally activated immutable generation on
`127.0.0.1:51235`; it never downloads corpus or model artifacts.

```bash
sudo install -d -o legal-mcp -g legal-mcp -m 0750 \
  /var/lib/australian-legal-mcp /etc/australian-legal-mcp
sudo cp legal-mcp.env.example /etc/australian-legal-mcp/legal-mcp.env
sudo cp legal-mcp.service /etc/systemd/system/
sudo systemctl daemon-reload
# Transfer and activate a generation before the first start.
sudo systemctl enable --now legal-mcp.service
curl -fsS http://127.0.0.1:51235/readyz
```

`ExecStartPre=legal-mcp verify` deliberately blocks a start when the generation,
model, database, source set, or ANN sidecars are incomplete. SIGTERM marks the
service not ready, stops acceptance, and drains bounded workers for the
configured grace period.

Keep port 51235 private. See [../DEPLOYMENT.md](../DEPLOYMENT.md) for the
low-cost Tailscale/VPS design and direct local-PC deployment flow.

## Maintainer PC

`legal-mcp-maintainer-weekly.service` and its timer are optional **user** units
for the CUDA-capable machine that owns `data/sources` and `data/models`. They
refresh, build, validate, and activate locally. They do not publish releases.

Adjust `LEGAL_MCP_REPO_DIR` if the checkout is not under
`%h/src/australian-legal-mcp`, then install:

```bash
mkdir -p ~/.config/systemd/user
cp legal-mcp-maintainer-weekly.service legal-mcp-maintainer-weekly.timer \
  ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now legal-mcp-maintainer-weekly.timer
```

Inspect it with:

```bash
systemctl --user status legal-mcp-maintainer-weekly.timer
journalctl --user -u legal-mcp-maintainer-weekly.service -n 100 --no-pager
```
