# Low-cost hosting

The corpus is built on the maintainer PC and copied directly to the serving
machine. The server never scrapes, embeds, downloads, or publishes corpus/model
artifacts. GitHub releases remain binary-only.

## Recommended first deployment

Use one ordinary 64-bit Linux VPS with local SSD storage:

- **4 vCPU, 8 GiB RAM, 160 GiB SSD/NVMe** for a low-traffic private pilot;
- two MCP workers (`LEGAL_MCP_HTTP_WORKERS=2`);
- the Rust service bound only to `127.0.0.1:51235`;
- private HTTPS through Tailscale Serve;
- only SSH/Tailscale management access; never expose port 51235.

Akamai/Linode's published APAC plans include a Sydney 4-vCPU, 8-GiB,
160-GB shared instance at about **USD 48/month**:
<https://www.akamai.com/cloud/pricing/asia-pacific>. That is the best initial
fit under the USD 100/month ceiling. Memory mapping the 56 GiB generation does
not require 56 GiB of RAM; Linux pages in only accessed SQLite/Arroy blocks.

The deployment script keeps only the active remote generation before upload.
During activation the server therefore holds about 112 GiB (active + incoming),
then retains the former active generation as one rollback copy. A 160-GB plan
is workable but intentionally tight. Prefer 200–256 GB when the budget allows,
or add provider block storage before the corpus grows materially.

### Why not Azure initially?

An Azure Linux `Standard_D2as_v5` in Australia East was approximately
USD 0.108/hour (~USD 79/month) through the Azure Retail Prices API, before a
256-GiB managed disk, OS disk, backups, and traffic:
<https://learn.microsoft.com/rest/api/cost-management/retail-prices/azure-retail-prices>.
A VM plus managed block disk is the correct Azure architecture, but it is
unlikely to remain below USD 100/month. Azure disk choices are documented at
<https://learn.microsoft.com/azure/virtual-machines/disks-types>.

Do not use Azure Container Apps, App Service, Azure Files, or AKS for the first
version. The corpus requires predictable mmap/random reads and same-filesystem
atomic renames. Container Apps' ephemeral/persistent storage model is described
at <https://learn.microsoft.com/azure/container-apps/storage-mounts>; network
file shares are a poor fit. AKS adds cost and operations without changing the
single-node disk requirement.

Move to an Azure VM when Entra governance, procurement, private networking, or
Microsoft 365 integration is worth the extra cost. The same binary, generation,
and systemd unit work there.

## Security boundary

This initial design is **private**, not a public OAuth service.

1. `legal-mcp` accepts only loopback TCP peers.
2. Tailscale provides device/user authentication and HTTPS at no application
   port exposure.
3. The MCP endpoint validates Streamable HTTP headers, protocol version, and
   browser `Origin` when present.
4. `/livez` and `/readyz` are ordinary HTTP probes and do not add MCP tools.
5. The MCP surface remains exactly seven tools.

Install Tailscale, join the VM to the private tailnet, and publish the loopback
backend:

```bash
sudo tailscale serve --bg --https=443 http://127.0.0.1:51235
```

Use `https://HOST.TAILNET.ts.net/mcp`. Browser-based clients also require that
exact origin in `LEGAL_MCP_ALLOWED_ORIGINS`.

Do **not** expose this endpoint publicly with a shared static token. A public or
cross-tenant service needs MCP-compatible OAuth resource-server behavior,
including protected-resource metadata, audience/expiry validation, and
`WWW-Authenticate`. Put that in the application or a trusted Entra/OIDC gateway
before public exposure.

## VM installation

Create a dedicated account and runtime directories:

```bash
sudo useradd --system --home /var/lib/australian-legal-mcp \
  --shell /usr/sbin/nologin legal-mcp
sudo install -d -o legal-mcp -g legal-mcp -m 0750 \
  /var/lib/australian-legal-mcp \
  /etc/australian-legal-mcp
sudo install -m 0755 target/release/legal-mcp /usr/local/bin/legal-mcp
sudo install -D -m 0644 libonnxruntime.so \
  /usr/local/lib/australian-legal-mcp/libonnxruntime.so
sudo cp systemd/legal-mcp.env.example \
  /etc/australian-legal-mcp/legal-mcp.env
sudo cp systemd/legal-mcp.service /etc/systemd/system/legal-mcp.service
sudo systemctl daemon-reload
```

Set the CPU ONNX Runtime library's exact location in the environment file. Do
not start the service until the first generation is transferred;
`ExecStartPre=legal-mcp verify` intentionally rejects an empty or semantically
incomplete host.

The service is hardened, runs without privileges, drains on SIGTERM, and writes
only runtime locks/state. The immutable generation itself is read-only from the
service's systemd sandbox.

Install `curl`, `python3`, and `rsync` on the VM. The SSH deployment identity
must be key-authenticated over the private management path and able to run the
script's noninteractive `sudo` operations. Prefer Tailscale SSH or a dedicated
deployment account; do not expose password-authenticated SSH to the internet.

## Build and deploy from this PC

All persistent local data is under `data/`:

```text
data/
  sources/             ten current official-source workspaces
  source-snapshots/    rollback, discarded, and legacy source stores
  models/              pinned model inputs and archived model artifacts
  builds/              incomplete or not-yet-activated builds
  runtime/             locally active immutable generations
  cache/               disposable build/TensorRT acceleration
  runs/ and logs/      acquisition and build evidence
  validation/          retained validation-only runtime layouts
  archive/             non-canonical historical diagnostics
```

Run an incremental refresh/build/activation locally:

```bash
cargo build --release --features cuda
scripts/maintainer-sync.sh
```

A full repair uses fresh source workspaces and retains the prior stores:

```bash
scripts/maintainer-sync.sh --full
```

Deploy the locally active generation directly over SSH:

```bash
scripts/deploy-generation.sh deploy@example-vps
```

The deploy script:

1. prunes only inactive remote generations before transfer;
2. resumes an `rsync` into remote `incoming/`;
3. validates every DB/model/ANN hash and binding on the VM;
4. atomically switches `active-generation`;
5. restarts the service and waits for `/readyz`;
6. rolls back the pointer if readiness fails.

It also holds a durable remote deployment lock and uses SSH keepalives during
long validation. If the client is killed, inspect
`/var/lib/australian-legal-mcp/.deploy-lock` and confirm no deployment is alive
before removing a stale lock and rerunning the same command.

No GitHub corpus release, object-storage bucket, remote build worker, or GPU is
involved.

## Scaling

Measure p95 latency, queue rejections, RSS, and page-cache pressure first.
Scale vertically to 16 GiB RAM and four or more serving workers when concurrent
searches justify it. Beyond one VM, run complete read-only replicas, each with
its own local generation, behind an OAuth-capable gateway/load balancer. Do not
put SQLite or Arroy on a shared network filesystem.
