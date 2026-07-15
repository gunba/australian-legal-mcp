---
name: setup-australian-legal-mcp
description: "Install or repair Australian Legal MCP endpoint connectivity, hosted service readiness, immutable generation activation/rollback, or local stdio development setup."
---

# Set up or repair Australian Legal MCP

Use this skill only for missing tools, endpoint/service failure, inactive or
invalid generation, direct-deployment recovery, or repeated startup failure.
The runtime has no corpus downloader or updater.

## Identify the mode

Production mode is a private HTTPS `/mcp` endpoint (normally Tailscale HTTPS) in
front of `legal-mcp.service`. The service runs:

```bash
legal-mcp serve --bind 127.0.0.1 --port 51235
```

Local development mode may register:

```json
{
  "mcpServers": {
    "australian-legal": {
      "command": "legal-mcp",
      "args": ["mcp"]
    }
  }
}
```

Do not silently replace one mode with the other.

## Production endpoint checks

1. Confirm the configured private HTTPS URL ends in `/mcp` and the client can
   reach the tailnet/private network.
2. On the serving VM, inspect:

   ```bash
   curl -fsS http://127.0.0.1:51235/livez
   curl -fsS http://127.0.0.1:51235/readyz
   sudo systemctl status legal-mcp.service
   sudo journalctl -u legal-mcp.service -n 100 --no-pager
   sudo -u legal-mcp env \
     LEGAL_MCP_DATA_DIR=/var/lib/australian-legal-mcp \
     /usr/local/bin/legal-mcp verify
   ```

3. `/readyz` must report `status: ok` and the expected 64-character generation.
   A live-but-not-ready service is not healthy.
4. Confirm `/etc/australian-legal-mcp/legal-mcp.env` points to
   `/var/lib/australian-legal-mcp` and the CPU ONNX Runtime library.
5. Confirm port 51235 is loopback-only. Do not open it publicly.

If a deployment was interrupted, inspect
`/var/lib/australian-legal-mcp/.deploy-lock` and `incoming/`. Remove a stale lock
only after proving no deployment process is running. Re-run from the RTX host:

```bash
scripts/deploy-generation.sh deploy@example-vps
```

The script resumes/converges incoming files, strictly verifies before pruning,
activates atomically, restarts, checks the exact generation, and rolls back on
failure.

## Missing or invalid generation

Never run or suggest `legal-mcp update`; it does not exist. On the maintainer
checkout:

```bash
cargo build --release --features cuda
scripts/maintainer-sync.sh             # or --full for a fresh repair
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp verify
scripts/deploy-generation.sh deploy@example-vps
```

Manual local lifecycle:

```bash
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp activate \
  --generation-dir "$PWD/data/builds/<generation-directory>"
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp verify
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp rollback \
  --generation <generation-id>
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" legal-mcp prune-generations \
  --keep-inactive 1
```

Do not prune until the active generation passes `verify`. Do not edit files
inside `generations/` or manually rewrite `active-generation`.

## Local stdio checks

1. Verify `legal-mcp --version` and the CPU ONNX Runtime library are discoverable.
2. Ensure every local command inherits the same runtime root, normally:

   ```bash
   export LEGAL_MCP_DATA_DIR=/path/to/australian-legal-mcp/data/runtime
   legal-mcp verify
   legal-mcp mcp
   ```

3. Inspect `http.json` and `SERVER_LOCK` only in local stdio/backend mode. A
   stale endpoint is safe to remove only after confirming no `legal-mcp serve`
   process owns it.
4. For a deliberate foreground test:

   ```bash
   legal-mcp serve --bind 127.0.0.1 --port 51235
   ```

## Verification after recovery

Call the MCP `stats` tool and one explicit-source `search` for each relevant
source. Confirm exact HTTPS `canonical_url` values and resolvable typed
references. For a full production check run:

```bash
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" scripts/smoke.sh
```

If recovery cannot proceed because no validated generation exists, report that
maintainer build/direct deployment is required. Do not substitute a runtime
download, GitHub corpus release, offline bundle, or shared public token.
