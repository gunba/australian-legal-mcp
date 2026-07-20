# Future Azure deployment

> Secondary portability reference. The selected hosted path is the attested OCI
> image on Akamai/Linode documented in [../DEPLOYMENT.md](../DEPLOYMENT.md).
> Revalidate this adapter before any Azure return; it is preserved to retain
> the hardened Bicep, Blob, identity, and recovery work.

The current Linode generation is schema-11 v20, while local v22 is ready for
the pending coordinated cutover. The Linode is configured-dark during recovery;
Azure has not received either generation and no Azure resource exists.

This preserved adapter would use Azure when the enterprise deployment decision
requires it; Microsoft 365 Copilot itself does not require Azure hosting.
Acquisition, OCR, corpus construction, and embeddings would remain on the local
RTX maintainer PC, while Azure would serve only a validated immutable CPU
generation.

The non-production design deliberately minimizes recurring cost:

```text
local RTX maintainer
  └─ strict local verify
     └─ zstd content-addressed 64 MiB chunks
        └─ private Azure Blob container (upload missing chunks only)
           └─ VM managed identity restores changed chunks
              └─ XFS/reflink Standard SSD managed disk
                 └─ legal-mcp on 127.0.0.1:51235
                    └─ Caddy HTTPS + application-level Entra validation
                       └─ Copilot Studio / Microsoft 365 Copilot
```

The checked-in Bicep creates one deallocatable `Standard_B2s_v2` VM (2 vCPU,
8 GiB), a 32-GiB OS disk, a detachable 128-GiB Standard SSD corpus disk, a
private Blob container, persistent user-assigned managed identity, NSG, static public
IP/DNS name, auto-shutdown, and an optional resource-group budget. Confirm the
SKU and current prices in the target subscription before deploying.

Official references:

- [Bsv2 sizes and CPU-credit behaviour](https://learn.microsoft.com/azure/virtual-machines/sizes/general-purpose/bsv2-series)
- [Managed disk types](https://learn.microsoft.com/azure/virtual-machines/disks-types)
- [VM states and billing](https://learn.microsoft.com/azure/virtual-machines/states-billing)
- [Azure budgets are alerts, not hard caps](https://learn.microsoft.com/azure/cost-management-billing/costs/tutorial-acm-create-budgets)
- [Managed identity for Azure resources](https://learn.microsoft.com/entra/identity/managed-identities-azure-resources/overview)

## Why this storage design

The active local v22 generation is approximately 21 GiB and includes a
19,758,231,552-byte SQLite database, ten flat-int8 sidecars totalling
1,816,430,592 bytes, the model, and tokenizer. SQLite and mmap flat-vector
sidecars require local reads, locking, and same-filesystem atomic renames.
BlobFuse, Azure Files, App Service storage, and Container Apps network storage
are not the live database.

Blob is a distribution and disaster-recovery store only. The transport:

- splits canonical files at fixed 64-MiB boundaries aligned to SQLite pages;
- hashes uncompressed bytes with SHA-256;
- zstd-compresses each chunk independently;
- stores each chunk once by its uncompressed content hash;
- uploads no chunk already present in the container;
- CoW-clones the active generation on XFS and downloads only changed chunks;
- re-hashes every reconstructed file before the Rust lifecycle validates it.

Repeating an unchanged deployment uploads zero chunks. A slightly changed
corpus uploads only changed chunks, not another complete generation. No
automatic Blob GC is performed because deleting a chunk still referenced by a
retained transport manifest would make rollback unrecoverable.

## Prerequisites

On the maintainer machine:

- Azure CLI and Bicep;
- an Azure subscription and permission to deploy resources and role assignments;
- distinct break-glass administrator and restricted publisher SSH keys;
- Python 3, `zstd`, and the CPU `legal-mcp` release package;
- enough time/network quota for the one initial corpus bootstrap upload.

Authenticate and select the subscription:

```bash
az login
az account set --subscription '<subscription-id-or-name>'
az account show --query '{name:name,id:id,tenantId:tenantId}' -o json
```

Check the test VM SKU in Australia East:

```bash
az vm list-skus --location australiaeast \
  --resource-type virtualMachines --all \
  --query "[?name=='Standard_B2s_v2'].{name:name,restrictions:restrictions}" \
  -o json
```

## 1. Provision the private test stack

Copy and edit the example. Do not commit the resulting parameter file:

```bash
cp infra/azure/test.parameters.example.json \
  "$HOME/.config/australian-legal-mcp-azure.json"
chmod 0600 "$HOME/.config/australian-legal-mcp-azure.json"
```

Fill in:

- a globally unique lowercase `namePrefix`;
- the two distinct SSH public keys;
- the operator's current public IPv4 address (Bicep adds `/32`);
- the signed-in user's Entra object ID for `uploaderPrincipalId`:

```bash
az ad signed-in-user show --query id -o tsv
```

Configure the two private keys with `IdentitiesOnly yes` in `~/.ssh/config`,
keyed by user (`azureadmin` versus `legal-mcp-publisher`), or load only the
required key into the SSH agent for each step.

Leave `publicMcpEnabled=false`. The same operator IPv4 is the only public Blob
firewall rule; the VM uses its subnet service endpoint. Redeploy the parameter
when the maintainer's public IP changes, before SSH or upload. Public Caddy
ingress remains disabled until Entra auth is configured. Review and deploy:

```bash
az bicep build --file infra/azure/main.bicep
az deployment sub what-if \
  --name australian-legal-mcp-test \
  --location australiaeast \
  --template-file infra/azure/main.bicep \
  --parameters @"$HOME/.config/australian-legal-mcp-azure.json"

az deployment sub create \
  --name australian-legal-mcp-test \
  --location australiaeast \
  --template-file infra/azure/main.bicep \
  --parameters @"$HOME/.config/australian-legal-mcp-azure.json"

az deployment sub show --name australian-legal-mcp-test \
  --query properties.outputs -o json
```

Cloud-init formats Azure LUN 0 as XFS with reflinks, records its UUID in
`/etc/fstab`, writes a root-owned volume marker, and creates separate service
and deployment identities. A persistent user-assigned managed identity retains
Blob access across VM replacement. The data disk uses `deleteOption=Detach`, so
VM replacement does not imply corpus loss. Existing XFS disks without the exact
UUID-bound marker are rejected rather than formatted or adopted. Deleting the entire resource group
still deletes resources in that group.

## 2. Install the CPU runtime

Use a verified Linux release archive. It must contain `legal-mcp` and
`libonnxruntime.so`; do not copy the local CUDA binary to the VM. Extract it,
then run:

```bash
scripts/configure-azure-host.sh \
  --host azureadmin@'<public-ip-or-fqdn>' \
  --public-host '<name>.australiaeast.cloudapp.azure.com' \
  --blob-base-url 'https://<account>.blob.core.windows.net/corpus' \
  --binary /path/to/extracted/legal-mcp \
  --onnx-runtime /path/to/extracted/libonnxruntime.so
```

This one-time host configuration uses the break-glass administrator. Routine
generation publication must use `legal-mcp-publisher`, never `azureadmin`.
The script installs the binary/runtime, forced-command deployment helper,
transport, systemd unit, and checksum-pinned Caddy 2.11.4 package/configuration. It enables
`legal-mcp.service` for reboot persistence
but does not start it without a generation. Caddy is explicitly disabled, so an
accidentally open NSG does not expose an unauthenticated MCP endpoint.
This Azure adapter installs a native unit at
`/etc/systemd/system/legal-mcp.service`; it rejects a Linode Quadlet instead of
treating systemd's `generated` state as native enablement, and proves the final
unit state is exactly `enabled/inactive`.

The systemd service:

- requires the managed disk mount and volume marker;
- reads the corpus root but writes only `state/`;
- runs as `legal-mcp`, separate from `legal-mcp-deploy`;
- binds only to `127.0.0.1:51235`;
- performs bounded installed-state checks before start, then exits unless model
  execution succeeds while the server initializes.

## 3. Upload once and activate the local generation

Build the CPU release locally if necessary, ensure `ORT_DYLIB_PATH` points to a
compatible local ONNX Runtime, then deploy:

```bash
cargo build --release --locked
export LEGAL_MCP_DATA_DIR="$PWD/data/runtime"
export ORT_DYLIB_PATH=/path/to/local/libonnxruntime.so

scripts/deploy-generation-azure.sh \
  --host legal-mcp-publisher@'<public-ip-or-fqdn>' \
  --blob-base-url 'https://<account>.blob.core.windows.net/corpus'
```

The first invocation strictly verifies the selected active local generation,
compresses/uploads its unique chunks, restores it with the VM managed identity,
validates every reconstructed file, activates it atomically, starts the service,
checks exact-generation readiness, and rolls back on failure. A root-owned durable transaction journal
also recovers interruption, reboot, or SSH loss after pointer activation;
bootstrap failure restores the no-active-generation state. Interrupted
upload/restore is resumable. The publisher SSH key is forced to the one strict
deployment command and has no administrator shell.

Later wrapper invocations first run the full canonical generation verifier,
then reuse `data/cache/azure-transport/<generation>.json`, list Blob content,
and upload only absent chunks. Every absent chunk is re-read and hash-checked
before upload. Direct transport-script callers should omit `--cache-dir` unless
they have just completed the same verification. Remote restore reflink-clones
the active generation and overwrites only chunks whose hashes differ.

Azure stores no source workspaces and performs no scrape, OCR, embedding, ANN
build, or corpus update. Blob versioning plus 30-day blob/container soft delete
protects the upload-once cache from accidental replacement or deletion; those
retained versions remain billable.

V20 was projected locally from retained schema-10 v19. SQLite FTS tokenization
rebuilt only the chunk index as contentless-delete; acquisition, OCR,
rechunking, model tokenization/execution, re-embedding, and ANN reconstruction
did not run. The v19 parent remains a local fallback and is not an Azure
bootstrap prerequisite.

## 4. Test privately before opening HTTPS

With Caddy and public ports still disabled:

```bash
ssh -L 51235:127.0.0.1:51235 azureadmin@'<public-ip-or-fqdn>'
curl -fsS http://127.0.0.1:51235/readyz | jq
```

Use an MCP client against `http://127.0.0.1:51235/mcp` and run all-source smoke
queries through the SSH tunnel. This tests the exact Azure CPU runtime and
managed disk without any public application surface.

## 5. Add Entra and Copilot

Do not open TCP 443 until the resource app, connector app, delegated scope, and
caller allowlist exist. Create those tenant objects as described in
[MICROSOFT_COPILOT.md](../MICROSOFT_COPILOT.md), but use the native Azure
transaction rather than the Linode Quadlet command:

```bash
scripts/configure-azure-entra.sh \
  --host azureadmin@'<public-ip-or-fqdn>' \
  --public-host '<name>.australiaeast.cloudapp.azure.com' \
  --tenant-id '<tenant-id>' \
  --server-app-id '<server-app-id>' \
  --allowed-client-id '<connector-app-id>'
```

That transaction first enables Entra validation locally, then starts Caddy and
proves public OAuth metadata/TLS. The VM never stores the connector client
secret.

## Routine operation and cost control

Start only for a test session and deallocate immediately afterward:

```bash
scripts/azure-vm.sh start rg-australian-legal-mcp-test <vm-name>
scripts/azure-vm.sh status rg-australian-legal-mcp-test <vm-name>
# test
scripts/azure-vm.sh deallocate rg-australian-legal-mcp-test <vm-name>
```

Azure auto-shutdown is a backstop. A guest `shutdown` is not the cost-control
contract: verify the management-plane state is `PowerState/deallocated`.
Deallocation stops VM compute charges, but managed disks, Blob capacity, public
IPv4, DNS/monitoring, and operations remain billable. Budget alerts do not stop
resources automatically.

The expected test cost is therefore:

```text
B2s_v2 hourly rate × allocated test hours
+ 32-GiB Standard SSD OS disk
+ 128-GiB Standard SSD data disk
+ compressed unique Blob chunks
+ public IPv4 and small storage/monitoring transaction costs
```

A 2026-07-16 Azure Retail Prices API snapshot (USD retail, before GST or
agreement discounts) showed: Linux B2s_v2 `$0.106/hour`, E4 32-GiB Standard SSD
`$3.264/month`, E10 128-GiB Standard SSD `$13.056/month`, Standard static IPv4
`$0.005/hour`, and Cool LRS Blob capacity `$0.011/GB-month`, plus disk/Blob
operations, retrieval, and any applicable mount meters. At 20 allocated VM hours
per month, compute is about `$2.12`; persistent test infrastructure is roughly
`$20–25/month` before operations and tax. A continuously allocated VM would add
about `$77/month`, which is why deallocation is mandatory.

Use the [Azure Pricing Calculator](https://azure.microsoft.com/en-au/pricing/calculator/)
and [Retail Prices API](https://learn.microsoft.com/rest/api/cost-management/retail-prices/azure-retail-prices)
for current subscription-specific estimates. The B-series CPU can throttle
after credits are exhausted; move to a D-series VM only when measured sustained-
query latency requires it.

To remove the disposable test completely:

```bash
az group delete --name rg-australian-legal-mcp-test --yes --no-wait
```

Confirm whether the Blob bootstrap and data disk should be retained before that
command.

## VM replacement without corpus upload

The data disk is the durable runtime root. To replace the VM:

1. stop and deallocate the old VM;
2. detach/preserve the managed disk;
3. attach it as LUN 0 to the replacement VM;
4. run the checked-in disk preparation script/cloud-init contract;
5. confirm the filesystem UUID and `.legal-mcp-data-volume` marker;
6. reinstall the pinned binary, ONNX Runtime, helper, config, and unit;
7. run `legal-mcp verify`, then start the service.

The existing `active-generation` pointer and installed generations are reused;
no Blob download is required.

## Enterprise evolution

The test VM is intentionally single-node. A production design should retain the
same application auth and immutable generation contract, then add:

- Azure Compute Gallery/Image Builder or a controlled data-disk image pipeline;
- at least two zonal read-only replicas, each with local corpus storage;
- Application Gateway WAF_v2 with response buffering disabled if SSE is added;
- Key Vault certificates/secrets, private backend networking, Azure Monitor,
  Conditional Access, Defender/Agent 365 governance, and tested regional DR;
- canary/green image deployment and exact-generation rollback.

Do not put SQLite or ANN files on shared network storage. Azure Front Door
currently documents no Server-Sent Events support, so it must not become a
future Streamable HTTP constraint. Application Gateway's SSE support is
 documented at <https://learn.microsoft.com/azure/application-gateway/use-server-sent-events>.
