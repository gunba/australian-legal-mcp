---
name: setup-australian-legal-mcp
description: "Install or repair Australian Legal MCP hosted-container connectivity, Linode volume/Quadlet/Caddy operation, API-key or Entra authentication, immutable generation activation/rollback, or local stdio setup. Use for missing tools, endpoint failures, auth failures, interrupted deployment, invalid generations, or restart/volume recovery."
---

# Set up or repair Australian Legal MCP

Read the repository [DEPLOYMENT.md](../../DEPLOYMENT.md) before changing a hosted
system. The runtime has no corpus downloader or updater.

## Identify the mode

Hosted mode is a digest-pinned OCI container behind native Caddy. Podman maps the
container bridge only to host `127.0.0.1:51235`; Caddy exposes exact `/mcp` and,
for Entra, OAuth protected-resource metadata. Hosted startup requires
`api-key`, `entra`, or `entra+api-key`.

Local development may use `legal-mcp mcp` or default loopback `legal-mcp serve`.
Do not substitute local and hosted modes or expose 51235.

## Check a hosted endpoint

1. Confirm canonical `https://HOST/mcp` and run the no-token boundary probe:

   ```bash
   python3 scripts/test-remote-mcp.py 'https://HOST/mcp'
   ```

2. On the VPS inspect private readiness, container, Caddy, and firewalls:

   ```bash
   curl --fail http://127.0.0.1:51235/livez
   curl --fail http://127.0.0.1:51235/readyz
   sudo systemctl status legal-mcp.service caddy.service
   sudo podman inspect australian-legal-mcp
   sudo journalctl -u legal-mcp.service -n 100 --no-pager
   sudo ufw status verbose
   ```

3. Require `status: ok` and one 64-hex generation. Confirm host 51235 is
   loopback-only.
4. Confirm `/srv/legal-mcp` is the exact XFS/reflink mount and its marker UUID
   matches the block device:

   ```bash
   findmnt -o SOURCE,FSTYPE,UUID,TARGET /srv/legal-mcp
   sudo xfs_info /srv/legal-mcp | grep reflink=1
   sudo cat /srv/legal-mcp/.legal-mcp-volume
   ```

5. Confirm the running image is referenced by a GHCR digest, container user is
   `971:971`, root is read-only, all capabilities are dropped, generations and
   lifecycle are read-only, and only state is writable.
6. Inspect `/etc/legal-mcp/runtime.env` without printing credentials. Entra IDs
   are non-secret; never print bearer tokens or plaintext API keys.

If public TLS or challenges are wrong, disable Caddy while repairing. Do not
weaken authentication.

## Repair authentication

API-key verifier files contain only `{id, sha256}` entries, are owned by UID
971, mode `0400`, and are loaded only at container start. Generate/rotate keys
with `scripts/manage-api-keys.py`; never place plaintext keys in arguments,
environment, images, Caddy, logs, or chat. Stream the one-time probe key to
`legal-mcp-configure-auth` on standard input.

For Entra, require the exact tenant, server application/audiences, delegated
scope URI, and caller client IDs. Hosted startup must prewarm JWKS before
listening. Copilot always uses Entra, never an API key.

Use `/usr/local/sbin/legal-mcp-configure-auth` for changes. It journals and
rolls back the runtime/verifier files, service, UFW 80/443, and Caddy state if
private or public probes fail.

## Repair a missing or invalid hosted generation

On the RTX maintainer checkout:

```bash
cargo build --release --features cuda
scripts/maintainer-sync.sh             # or --full
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp verify
scripts/deploy-generation.sh \
  --host legal-mcp-publisher@HOST
```

The deployment revalidates local bytes/model execution, CoW-seeds restricted
remote upload staging, rsyncs changed blocks, and uses a one-shot copy of the
exact image for strict activation. The publisher cannot write lifecycle state
or installed generations. Rerun the same command after interruption; do not
manually edit upload state, the root transaction journal, locks, installed
generations, or `lifecycle/active-generation`.

Initial activation intentionally remains stopped pending auth cutover. Later
activation requires exact readiness and rolls back automatically.

## Local lifecycle

```bash
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp activate \
  --generation-dir "$PWD/data/builds/<generation-directory>"
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp verify
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp rollback \
  --generation <generation-id>
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp prune-generations \
  --keep-inactive 1
```

Never suggest `legal-mcp update`; it does not exist.

## Local stdio checks

1. Verify `legal-mcp --version`, `ORT_DYLIB_PATH`, and `LEGAL_MCP_DATA_DIR`.
2. Run `legal-mcp verify`, then `legal-mcp mcp`.
3. Local endpoint state is under `state/http.json` and `state/SERVER_LOCK`.
4. Foreground default `legal-mcp serve --port 51235` remains loopback-only.

## Prove recovery

Run `stats`, explicit-source searches, and:

```bash
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" scripts/smoke.sh
```

For a real Entra token, keep it only in `LEGAL_MCP_TEST_ACCESS_TOKEN` and run
`scripts/test-remote-mcp.py --require-token --tools PATH/TO/mcp-tools.json`.
If no valid generation exists, report that maintainer build/activation and
restricted hosted deployment are required. Never substitute a GitHub corpus
release, offline bundle, anonymous endpoint, object/FUSE live filesystem, or
unidentified shared token.
