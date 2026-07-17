# Native systemd installation

This unit is retained for local/native binary installs and the future Azure VM
adapter. The active Akamai/Linode deployment uses the root-managed Podman
Quadlet in `infra/hosting/legal-mcp.container.template`, not this unit.

The native service:

- runs as the unprivileged `legal-mcp` identity;
- binds only `127.0.0.1:51235`;
- requires a ready immutable generation;
- may write only the runtime `state/` directory;
- loads the CPU ONNX Runtime library from the adjacent stable host path.

The example environment defaults to unauthenticated loopback. Any native public
proxy must add `--require-http-auth`, configure `api-key`, `entra`, or
`entra+api-key`, and prove auth before opening ingress. Prefer the documented
container contract for hosted operation.

Useful operations:

```bash
sudo systemctl status legal-mcp.service
sudo journalctl -u legal-mcp.service -n 200 --no-pager
curl --fail http://127.0.0.1:51235/livez
curl --fail http://127.0.0.1:51235/readyz
```

Do not run lifecycle commands as the serving identity. The service exits unless
its active generation remains consistent and semantic-model prewarm succeeds.
Maintainer timers remain on the local RTX workstation; a serving host never
acquires official sources or builds a corpus.
