# Hosted deployment

The hosted target is one Akamai Cloud (Linode) VPS in Sydney. The host is
disposable; the corpus lives on a detachable, encrypted Block Storage volume
and is never baked into an image.

The host is currently fail-closed. Its v0.19.0 empty-host contract is installed
and the complete v20 candidate is preserved at
`/srv/legal-mcp/uploads/a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3`
in a publisher-owned `prepared` transaction. Activation failed before
validation because the capability-free one-shot container could not traverse
the UID/GID 973 mode-`0700` upload parent; recovery restored publisher ownership
without aborting or deleting staging. There is no active remote generation,
authentication, application service, Caddy, DNS record, or ingress. Public
ports 80, 443, and 51235 remain closed. Software 0.19.1 is the unreleased patch
for this host-side permission boundary.

The same image and mounted-generation contract can later run on an Azure VM.
Azure-specific work is retained in [docs/AZURE_FUTURE.md](docs/AZURE_FUTURE.md),
but it is not the current deployment path.

## Runtime boundary

```text
Akamai Cloud Firewall
  └─ Ubuntu 24.04 VPS
     ├─ native Caddy :80/:443
     │    └─ 127.0.0.1:51235 only
     ├─ root-managed Podman Quadlet
     │    └─ legal-mcp as numeric UID/GID 971, no capabilities
     │         ├─ generations/ + lifecycle/ read-only
     │         ├─ state/ read-write
     │         └─ API-key verifier file read-only
     ├─ /srv/legal-mcp on a detachable XFS/reflink volume
     └─ forced-command publisher account
          └─ CoW seed → restricted rsync delta → verify → activate/rollback
```

The image contains:

- the `legal-mcp` server, search, tokenisation, exact reranking, and all seven MCP
  tools;
- bundled SQLite (via `rusqlite`), Arroy/heed vector-search code, and normalized
  int8 exact reranking support;
- ONNX Runtime 1.25.0, `libgomp`, C/C++ runtime libraries, and CA certificates;
- a fixed unprivileged `971:971` runtime identity.

`model.onnx`, `tokenizer.json`, `legal.db`, and the ten ANN sidecars remain part
of each complete immutable generation on the corpus volume. They are data/model
artifacts, not image dependencies. `data/`, `release/`, `target/`, and `Temp/`
are excluded from both Docker and OCI build contexts.

The long-running service container uses a read-only root filesystem, drops
every capability, sets `no-new-privileges`, bounds memory/PIDs/files, has no
engine socket, and publishes its bridge port only on host loopback. Container
network scope cannot start without `--require-http-auth`. One separate,
networkless activation container receives only `CAP_DAC_OVERRIDE` for the exact
prepared-upload `activate` invocation; all other one-shot lifecycle commands
also drop every capability.

## Authentication

Hosted mode supports these exact values of `LEGAL_MCP_HTTP_AUTH`:

- `api-key`;
- `entra`;
- `entra+api-key`.

Entra OAuth validation remains in the Rust resource server and works identically
when the image is hosted on Linode or Azure. It validates the exact tenant,
issuer, audience, delegated scope, caller application, token time bounds, and
RS256 signing key. JWKS startup is prewarmed, refresh is serialized/rate-limited,
and stale signing keys have a hard 24-hour limit.

API keys are for individually identified automation clients, not delegated user
identity or Microsoft 365 Copilot. Each key is a 256-bit random secret with a
revocable ID. The server stores only SHA-256 verifiers in a strict, owner-only
file and compares fixed-size digests in constant time. Send a key only as:

```http
X-API-Key: KEY_ID.BASE64URL_SECRET
```

Requests containing both a bearer token and an API key are rejected. Plaintext
keys never belong in environment variables, command arguments, image layers,
Caddy, Terraform/OpenTofu state, logs, or chat.

## 1. Build and release gate

Every release builds `linux/amd64`, macOS arm64, and Windows x64 archives plus an
immutable `linux/amd64` GHCR image. The container build uses digest-pinned Rust
and Debian bases, verifies the ONNX Runtime archive hash, runs `verify-runtime`,
scans for fixed HIGH/CRITICAL vulnerabilities before publication, emits an SBOM
and maximum BuildKit provenance, and creates a GitHub/Sigstore build attestation.
No mutable `latest` tag is published. The GHCR software package contains no
corpus and should be configured public with the repository; prove an anonymous
digest pull before host installation. If policy requires a private package, use
a host-scoped read-only `packages:read` credential and never place it in chat,
OpenTofu, the image, or the corpus volume.

Deploy only a digest:

```text
ghcr.io/gunba/australian-legal-mcp@sha256:...
```

Download release bundles and verify them before extracting or copying any host
tooling. The Linux bundle carries the exact Caddy package and its committed
SHA-512 manifest; the installer verifies both before package, firewall, or
volume mutation:

```bash
gh release download v0.19.1 --repo gunba/australian-legal-mcp \
  --pattern 'legal-mcp-*' --pattern SHA256SUMS
sha256sum --check SHA256SUMS
```

This v0.19.1 command is valid only after that immutable patch release exists.
The existing host was bootstrapped from the separately verified v0.18.1 bundle
and later cut over with v0.19.0; do not relabel either historical evidence set
as v0.19.1.

Verify the attestation before deployment:

```bash
gh attestation verify \
  oci://ghcr.io/gunba/australian-legal-mcp@sha256:DIGEST \
  --repo gunba/australian-legal-mcp
```

## 2. Provision the Linode and persistent volume

Do not place an Akamai API token in a tfvars file. Export it only in the operator
shell expected by the Linode provider:

```bash
export LINODE_TOKEN=...   # do not paste into chat or commit
cp infra/linode/test.tfvars.example infra/linode/terraform.tfvars
# Fill one break-glass admin public key and one /32 or /128 source CIDR.
```

The validated defaults are Sydney (`ap-southeast`), Ubuntu 24.04,
`g6-standard-4` (8 GiB), and a 128-GiB encrypted Block Storage volume. The
smaller volume deliberately relies on mandatory XFS reflink deltas. Before any
operation needing full-copy headroom, increase `volume_size_gib` to 256, apply
the reviewed plan, and then grow the mounted filesystem (Block Storage growth
does not itself grow XFS):

```bash
sudo xfs_growfs /srv/legal-mcp
findmnt /srv/legal-mcp
df -h /srv/legal-mcp
```

Never attempt to shrink XFS or the Block Storage volume. The volume has
`prevent_destroy`; remove that guard only for intentional corpus destruction.

At the 2026-07-16 public price snapshot, `g6-standard-4` was USD 0.072/hour
capped at USD 48/month and Block Storage was USD 0.10/GiB-month, so the baseline
was about USD 60.80/month before tax, transfer overages, backups, or DNS. Query
the live plan API and pricing page before applying.

Use OpenTofu 1.12.4 or a compatible Terraform implementation:

```bash
cd infra/linode
tofu init -lockfile=readonly
tofu plan -out legal-mcp.tfplan
tofu apply legal-mcp.tfplan
```

`public_mcp_enabled` must remain `false`. The Cloud Firewall is attached while
the instance is created, initially permits SSH only from the exact
administrator CIDR plus essential ICMPv6, and never admits 51235. DNS records
are optional and use an existing Akamai DNS Manager domain.

Review the plan, current Akamai pricing, region capacity, tax, and Block Storage
charges before applying it. After creation, retain the encrypted local state and
remember that a powered-off instance and a detached volume remain billable until
deleted.

## 3. Install the host contract

Create a second, restricted publisher SSH key. It must not be the administrator
key:

```bash
ssh-keygen -t ed25519 -f ~/.ssh/legal-mcp-publisher
```

Transfer only its `.pub` file and the version-matched Linux release bundle to
the VPS. The bundle's `SOURCE_COMMIT` must exactly match the OCI revision label.
On a brand-new, signature-free volume, run from the unpacked bundle:

```bash
sudo infra/linode/install-host.sh \
  --image ghcr.io/gunba/australian-legal-mcp@sha256:DIGEST \
  --public-host legal.example.com \
  --volume-device /dev/disk/by-id/scsi-0Linode_Volume_LABEL \
  --publisher-key-file /root/legal-mcp-publisher.pub \
  --admin-source-ip YOUR_SINGLE_IP \
  --initialize-empty-volume
```

`--initialize-empty-volume` is deliberately destructive but refuses any
partition, filesystem, or signature and never uses `mkfs.xfs -f`. Record the
reported UUID. On a replacement VPS, attach the existing volume and use:

```bash
sudo infra/linode/install-host.sh \
  ... \
  --expected-volume-uuid RECORDED_UUID
```

An existing volume is accepted only when its XFS filesystem, reflink/file-type
features, UUID-bound marker, exact `noatime,nodev,noexec,nosuid` mount contract,
ownership/ACLs, and pre-created lock all match. Unknown volumes are neither
formatted nor adopted, and an unvalidated temporary mount never writes fstab.

The installer:

- creates fixed service UID/GID 971, publisher UID/GID 973, and break-glass
  administrator UID/GID 974;
- installs rootful Podman/Quadlet but runs the application as container UID 971;
- installs the release-bundled, checksum-pinned Caddy 2.11.4 package but leaves
  it disabled;
- configures UFW for SSH only from the supplied address;
- pulls and tests the exact image digest;
- installs the forced publisher command and narrow sudo policy;
- copies the provisioned administrator key to `legal-mcp-admin`, disables root
  and password SSH, and leaves both `legal-mcp.service` and Caddy disabled.

Before closing the initial root session, open a second SSH session as
`legal-mcp-admin` and confirm `sudo -n true`. Thereafter root SSH is disabled.
Also retain the Akamai Cloud Firewall. UFW is defence in depth, not a substitute.

The current host completed its initial install with v0.18.1 and its empty-host
software cutover with v0.19.0. Do not rerun the initial installer against it;
use the staged-activation repair below.

## 4. Repair the preserved v20 activation

Configure SSH identities locally so only the publisher key is offered:

```sshconfig
Host legal-mcp-publisher
  HostName legal.example.com
  User legal-mcp-publisher
  IdentityFile ~/.ssh/legal-mcp-publisher
  IdentitiesOnly yes
```

The complete schema-11 generation
`a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3`
is already staged. It is approximately 37.42 GiB complete, with a
19,746,840,576-byte `legal.db`. The failed v0.19.0 activation restored the
journal to `prepared` and recursively restored UID/GID 973 ownership with
mode `0700` directories and `0600` files. Preserve that transaction exactly.
Do **not** run `prepare`, rsync, or `abort`.

From the verified, unpacked v0.19.1 Linux bundle, first transactionally replace
only the digest-pinned host publisher tools and sudo policy:

```bash
sudo infra/linode/install-host.sh --upgrade-host-tools --version 0.19.1
```

If interrupted, recover from that same release bundle before continuing:

```bash
sudo infra/linode/install-host.sh --recover-host-tools --version 0.19.1
```

The host-tool preflight accepts upload authorization only when it is safely
absent (the closed post-activation state) or is the exact matching protected
file, and preserves that state with the prepared corpus transaction. After the
upgrade succeeds, retry only its exact forced activation command:

```bash
ssh -T legal-mcp-publisher \
  'activate a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3'
```

A successful empty-host retry prints `activated-pending-auth`. If the SSH
response is lost, issue the same exact command again; do not infer an abort or
restart the upload. The durable journal and active pointer make the retry
idempotently reconcile whichever of these states completed.

The local v19 parent remains available only with its paired v0.18.1 schema-10
binary/image fallback. The schema-11 binary cannot directly roll back to it.

For future generation deployments, `scripts/deploy-generation.sh` first
performs strict local hashing and semantic-model execution. The remote root
helper then creates a CoW clone of the active generation, and restricted rsync
uses checksums, block deltas, and in-place writes. An unchanged redeploy
transmits no file data; interrupted uploads resume. Zstd is negotiated for
transport because the ANN tree and SQLite bytes compress materially on the
maintainer uplink; the immutable files remain uncompressed on XFS. The
publisher can write only `/srv/legal-mcp/uploads` and can invoke only `prepare`,
`activate`, or explicit `abort`. It cannot otherwise mutate lifecycle state,
installed generations, service configuration, or image/auth secrets.

`prepare` creates one root-owned, generation-specific upload authorization under
`/run`. The forced rsync command accepts only that 64-character destination and
holds the same host transaction lock used by activation, auth, and image
changes. Activation revokes the authorization before normalizing or validating
staging, so rsync can never race the immutable cutover.

Remote activation normalizes only the candidate tree to `root:legal-mcp`,
strictly revalidates every generation artifact inside a one-shot copy of the
same image, atomically switches the pointer, starts the service, checks the
exact generation, and rolls back on readiness failure. The upload parent
intentionally remains UID/GID 973 mode `0700`; changing it would widen publisher
visibility and could leave that wider access behind after SIGKILL.

Container UID 0 with all capabilities dropped cannot search that parent or
rename its child. `CAP_DAC_READ_SEARCH` is also insufficient because the atomic
rename needs write/search permission on the source parent. The host helper
therefore has two closed capability profiles:

- `capability-free` for verify, rollback, deactivate, prune, and every other
  lifecycle operation;
- `prepared-upload-activation` for exactly
  `activate --generation-dir /var/lib/legal-mcp/uploads/<generation>
  --expected-generation <generation>`, with only `CAP_DAC_OVERRIDE` added.

The activation container remains networkless, read-only-root,
`no-new-privileges`, resource-bounded, and digest-pinned. The capability is not
path-scoped, so exact arguments, the immutable image, the host transaction
lock, and the one-command profile are material controls. It is never added to
the Quadlet or long-running service.

The `activating` journal is durable before normalization or container launch.
If validation fails before rename, the candidate is restored to publisher
ownership and `prepared`. If SIGKILL leaves the candidate in uploads, retry
normalizes and reuses it; if rename completed, retry reconciles the installed
directory and pointer. The activation child inherits the locked transaction
file descriptor, so a surviving child prevents a concurrent retry until it
exits. The exact `activate` retry neither reauthorizes rsync nor aborts staging.
Initial activation intentionally reports `activated-pending-auth` and does not
start a network listener. Configure authentication only after this succeeds.

## 5. Configure an API key and open ingress

Generate the verifier locally. Capture stdout directly into a password manager
or a mode-`0600` temporary file; it is the only copy of the plaintext key:

```bash
umask 077
scripts/manage-api-keys.py generate \
  --file "$PWD/Temp/legal-mcp-api-verifiers.json" \
  --id first-client \
  > "$PWD/Temp/first-client.key"
```

Copy only `legal-mcp-api-verifiers.json` to a root-owned staging path on the VPS.
Set `public_mcp_enabled=true` in OpenTofu immediately before cutover so the
Akamai firewall admits ACME/HTTPS. UFW remains closed until private checks pass.
Run the host transaction while streaming the probe key over SSH standard input; the plaintext is not placed in a remote argument, environment,
or file:

```bash
ssh -T legal-mcp-admin@legal.example.com \
  'sudo /usr/local/sbin/legal-mcp-configure-auth \
    --mode api-key \
    --public-host legal.example.com \
    --api-key-file /root/legal-mcp-api-verifiers.json' \
  < "$PWD/Temp/first-client.key"
```

The transaction restarts the private container, proves readiness, 401
challenges, and a valid API-key call, opens host ports 80/443, enables Caddy,
then repeats the checks through public TLS without following redirects. Any
failure disables public Caddy, closes UFW 80/443, restores auth files, and
restarts the prior service configuration. If the newly supplied key cannot
also prove a prior API-key configuration during rollback, ingress stays closed
and the explicit recovery command below must receive a valid prior key.

Auth, image, and corpus mutations share one host transaction lock. If power
loss or SIGKILL leaves an auth transaction pending, keep ingress closed and
recover the prior configuration before retrying:

```bash
sudo /usr/local/sbin/legal-mcp-configure-auth --recover
```

If the saved prior mode contains API-key authentication, redirect a still-valid
prior key to the recovery command on standard input. Recovery keeps Caddy and
UFW ingress closed unless exact readiness, expected challenges/metadata, and
positive API-key authentication all pass for the restored configuration.

Delete the temporary plaintext key file after it is secured by the intended
client. Keep the verifier file; it is not plaintext but remains protected.

## 6. Entra OAuth or combined mode

Linode hosting does not change the Microsoft identity design. Create the Entra
resource/caller applications as described in
[MICROSOFT_COPILOT.md](MICROSOFT_COPILOT.md), then use `entra` or
`entra+api-key`:

```bash
ssh -T legal-mcp-admin@legal.example.com \
  'sudo /usr/local/sbin/legal-mcp-configure-auth \
    --mode entra \
    --public-host legal.example.com \
    --tenant-id TENANT_UUID \
    --server-app-id SERVER_APP_UUID \
    --audiences SERVER_APP_UUID,api://SERVER_APP_UUID \
    --scope legal.read \
    --scope-uri api://SERVER_APP_UUID/legal.read \
    --allowed-client-ids CALLER_APP_UUID'
```

There is no server client secret. Hosted startup fetches and prewarms Entra
public signing keys before listening. Copilot must use OAuth; do not put an API
key in a connector definition.

## 7. Rotation and revocation

To rotate an API key with a brief fail-closed cutover and no anonymous exposure:

1. Add a new ID with `manage-api-keys.py generate` to the current verifier file.
2. Rerun `legal-mcp-configure-auth`, probing with the new key.
3. Move clients to the new key.
4. Revoke the old ID with `manage-api-keys.py revoke` and rerun the transaction.

The helper refuses to revoke the final API key. Switch to Entra first if API-key
authentication is being removed entirely.

For an image update, verify a new attested digest and run from its unpacked
Linux release bundle:

```bash
sudo infra/hosting/update-image.sh \
  --image ghcr.io/gunba/australian-legal-mcp@sha256:NEW_DIGEST \
  --version 0.19.1 \
  --template infra/hosting/legal-mcp.container.template
```

For `api-key` or `entra+api-key` mode, redirect a valid probe key to this command
on standard input just as for auth cutover. The helper validates OCI source,
release, revision, binary, runtime, and the complete mounted generation before
closing UFW/Caddy. It then atomically replaces the digest-pinned Quadlet,
restarts against the same generation, proves exact readiness, 401 challenges,
metadata, the running image ID, and (when configured) a valid API-key request
before restoring public ingress. Any failure verifies the prior image and auth
boundary before reopening ingress. Recover a hard-interrupted transaction with:

```bash
sudo /path/to/the-same-release-bundle/infra/hosting/update-image.sh --recover \
  < /root/one-time-probe-key  # omit redirection for Entra-only mode
```

Retain that version-matched release bundle until the transaction has completed;
its recovery code owns the transaction format. Never deploy by tag, mutate a running container, or install native libraries
into it.

## 8. Verification and operations

On the host:

```bash
systemctl status legal-mcp.service caddy.service
podman inspect australian-legal-mcp
curl --fail http://127.0.0.1:51235/readyz
ss -lntp | grep -E ':(80|443|51235)\b'
findmnt /srv/legal-mcp
xfs_info /srv/legal-mcp | grep reflink=1
ufw status verbose
```

Run the redirect-safe authenticated API-key contract probe from the operator
workstation when API-key mode is enabled:

```bash
python3 scripts/test-remote-mcp.py --require-api-key \
  --tools data/cache/microsoft-integration/tools.json \
  'https://legal.example.com/mcp' < "$PWD/Temp/first-client.key"
```

Required observations:

- 51235 is bound only to `127.0.0.1` on the host;
- Caddy exposes only exact `/mcp` and
  `/.well-known/oauth-protected-resource/mcp`;
- `/mcp/`, origin-level OAuth metadata, redirects, and all other paths are not
  aliases;
- unauthenticated MCP calls return 401 with the appropriate challenge;
- readiness names the expected generation;
- container root is read-only, user is `971:971`, all capabilities are dropped,
  and corpus/lifecycle mounts are read-only;
- restart and host reboot preserve service readiness;
- detach/reattach or VPS replacement preserves the volume and requires the same
  marker UUID;
- a changed fixture generation activates and rolls back; an unchanged redeploy
  sends no file content.

The local RTX workstation remains the source of truth for official workspaces
and complete generations. Block Storage is the live VPS filesystem, not a build
source. Akamai Object Storage is optional future DR/transport work; do not mount
it through FUSE for SQLite, ANN sidecars, lifecycle pointers, or locks.

## 9. Cost and shutdown

Linode compute continues billing while the instance exists; unlike Azure,
there is no assumed deallocate-without-delete workflow here. Destroy/recreate
the disposable VPS when idle only after proving volume reattachment and keeping
the OpenTofu volume `prevent_destroy` guard. The encrypted volume and any DNS or
Object Storage remain billable.

## Azure portability

The image, authentication, Caddy routes, mounted generation layout, fixed UID,
publisher protocol, and one-shot deployment container are provider-neutral. A
future Azure VM should consume the same digest and volume contract. Azure Blob
and managed identity remain optional provider adapters; they must not become
the live filesystem. See [docs/AZURE_FUTURE.md](docs/AZURE_FUTURE.md).
