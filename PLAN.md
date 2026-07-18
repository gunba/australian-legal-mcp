# Delivery plan

## Mission

Operate one source-grounded Australian Legal MCP over ten official sources with
exactly seven tools, explicit source selection, deterministic citations/ranking,
locally built immutable generations, portable Akamai/Linode OCI hosting, and an
Entra-governed path into Microsoft 365 Copilot.

## Fixed architecture

- Official source truth, acquisition, OCR, embedding, ANN construction, and
  builds remain on the local RTX PC beneath project `data/`.
- Builds consume committed source stores and never scrape.
- Complete generations are strictly validated, sealed read-only, and atomically
  activated under `data/runtime`.
- The current hosted target is a corpus-free, digest-pinned OCI image on an
  Akamai/Linode VPS. A detachable encrypted XFS/reflink volume is the live
  SQLite/ANN filesystem.
- A restricted publisher CoW-seeds and rsyncs changed blocks; a one-shot copy of
  the same image validates and activates each generation.
- Podman publishes the application bridge only on host loopback. Native Caddy
  exposes exact `/mcp` and OAuth resource metadata after auth checks pass.
- Public access uses exact Entra delegated identity, individually revocable
  digest-backed API keys, or both. Copilot always uses Entra.
- GitHub Releases remain binary-only.

Every search requires one registered source. Public JSON uses typed
source-qualified identities. `fetch` accepts only canonical `legal://` URIs. The
MCP surface remains exactly `search`, `get_chunks`, `get_asset`,
`get_doc_anchors`, `get_definition`, `stats`, and `fetch`.

## Completed foundation

- Ten official adapters, adaptive acquisition, bounded Federal Chrome CDP,
  transactional workspaces, strict source quality, schema 11, cleaned HTML,
  links/assets/definitions, deterministic FTS/vector ranking, and ANN recall at
  least 0.99 at 50. Schema 11 uses digest-bound contentless-delete chunk FTS.
- Pinned mdbr-leaf-ir FP32 graph, exact tokenizer, TensorRT FP16/CUDA local build,
  and CPU serving path.
- Validated/active local v20
  `a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3`:
  409,528 documents, 6,968,250 chunks/embeddings, 20,170 definitions, a
  19,746,840,576-byte schema-11 DB, exact model/ten-ANN bindings, all-source
  hybrid retrieval, rollback, pruning, and graceful bounded Streamable HTTP.
  The schema-10 v19 parent and matching v0.18.1 binary/image remain the local
  fallback; the schema-11 binary deliberately rejects it.
- Added deterministic schema-10 projection. SQLite tokenizes existing text only
  to rebuild FTS; acquisition, OCR, rechunking, model tokenization/execution,
  re-embedding, and ANN reconstruction do not run.
- Removed runtime corpus download/publication/offline-bundle paths.
- Added immutable activation, strict verification, lifecycle locks, durable
  maintainer resumption, exact-generation readiness, and hardened systemd.
- Added a hardened non-root OCI image, lock-pinned Linode OpenTofu, strict
  XFS-volume adoption, CoW/rsync delta deployment, narrow publisher/root
  transactions, Caddy, API-key plus Entra auth, signed image provenance/SBOM
  policy, RFC 9728 metadata/challenges, and Copilot templates.
- Added version-matched host-tool upgrade, explicit publisher abort, and
  fail-closed empty-host image cutover operations for the v20 transition.
  V0.19.6 hard-cuts the upgrade to one exact, recoverable V2 transaction for
  prepared-bootstrap or activated-dark state, including auth/image helpers and
  the installed Quadlet template.
- Restricted the locked-parent activation exception to one exact networkless
  `activate` invocation with `CAP_DAC_OVERRIDE`; the hosted service and every
  other lifecycle command remain capability-free. Disposable fixtures prove
  the real DAC boundary and SIGKILL/retry reconciliation.
- Preserved Azure Bicep, managed-disk, private Blob, and content-addressed
  transport as a future provider adapter rather than the active deployment.

## Phase 1 — local hosting/identity gates

The branch currently passes:

```bash
cargo fmt --all -- --check
cargo test --workspace --locked --all-features
cargo clippy --workspace --locked --all-targets --all-features -- -D warnings
cargo audit
cargo deny check advisories
bash -n scripts/*.sh
python3 -m unittest \
  tests/test_azure_generation_transport.py \
  tests/test_configure_azure_host.py \
  tests/test_manage_api_keys.py \
  tests/test_remote_mcp.py \
  tests/test_render_microsoft_integrations.py
tofu -chdir=infra/linode init -backend=false -lockfile=readonly
tofu -chdir=infra/linode validate
LINODE_TOKEN=0000000000000000000000000000000000000000000000000000000000000000 \
  tofu -chdir=infra/linode plan -refresh=false -input=false -lock=false
git diff --check
LEGAL_MCP_DATA_DIR="$PWD/data/runtime" scripts/smoke.sh
cargo package --workspace --locked --allow-dirty
```

Local evidence includes full v20 activation/verification, 76 smoke checks,
all-ten-source hybrid retrieval, valid/invalid signed-token and API-key tests,
resource metadata and 401/403 challenges, exact seven descriptors, official
Microsoft v2.4 schema validation; bridged non-root/read-only container probes; zero fixed
HIGH/CRITICAL image findings; Caddy validation; strict disk/mount guards;
packaged ONNX loading; and a clean offline Linode provider plan. The live
instance/volume boundary now also proves Ubuntu 24.04, XFS/reflink volume
adoption and restricted SSH. V20 is active on the host after the v0.19.2
publisher-tool repair and activation, but authentication is disabled,
`legal-mcp.service` is inactive, Caddy is disabled/inactive, and UFW 80/443 are
closed. One known v0.19.2 authentication transaction remains for explicit
one-shot recovery; no deployment or image transaction or upload authorization
exists.

## Phase 2 — disposable Linode infrastructure

1. **Completed 2026-07-16:** applied `infra/linode` in Sydney with
   `public_mcp_enabled=false`, distinct administrator/publisher keys, and an
   encrypted 128-GiB volume; verified the attached device is signature-free and
   public 80/443/51235 are closed.
2. **Completed:** bootstrapped the host, cut it over to the v0.19.0 empty-host
   contract, and fully staged v20.
3. **Completed:** the v0.19.2 publisher-tool repair and activation succeeded;
   v20 is active with authentication, application service, Caddy, and UFW web
   ingress still off and no transaction or upload authorization remaining.
4. Once v0.19.6 artifacts exist, independently verify the release bundle,
   checksums, `SOURCE_COMMIT`, and OCI digest. Run
   `--upgrade-host-tools --version 0.19.6` from those exact bytes; the V2
   transaction must leave the activated host dark.
5. Configure API-key and/or Entra auth, then move the running image to the
   verified v0.19.6 digest through the normal authenticated image transaction.
   Do not claim release or publication before the immutable artifacts exist.
6. Test reboot, changed/unchanged generation deltas, readiness rollback,
   API-key rotation/revocation, image rollback, volume detach/reattach, and VPS
   replacement without another full upload.
7. Record compute/volume cost, p50/p95 latency, CPU, RSS, page cache, queue
   rejection, and disk extent growth.

Exit criterion: the disposable VPS can be recreated from OpenTofu + an attested
image digest + the retained volume, while 51235 never becomes public.

## Phase 3 — Copilot Studio OBO

1. Create a single-tenant resource app and delegated `legal.read` scope.
2. Create the connector app, delegated permission/admin consent, short-lived
   secret, `access_as_user`, and Azure API Connections preauthorization.
3. Keep public ingress closed until app IDs exist; run
   `legal-mcp-configure-auth --mode entra` so auth is proved before Caddy stays
   enabled.
4. Import the rendered Streamable MCP Swagger custom connector with OBO enabled.
5. Test consent, valid invocation, expiry, revoked consent, disabled user,
   Conditional Access, wrong tenant/audience/client, missing scope, DLP, and
   publication to a controlled Microsoft 365 Copilot test audience.

Exit criterion: every cloud request is a validated delegated user call, all
seven tools remain read-only, and no bearer token/query content is leaked into
infrastructure logs.

## Phase 4 — direct Microsoft 365 declarative agent

- Register Teams Developer Portal Entra SSO for the exact MCP base URL.
- Add the generated Application ID URI and Microsoft enterprise token-store
  client to exact server allowlists.
- Render plugin manifest v2.4 with static seven-tool definitions.
- Provision/sideload with Microsoft 365 Agents Toolkit, then test admin consent,
  assignment, revocation, and tenant policy.

Treat Agent 365 BYO registry and dynamic tenant tool discovery as optional
preview paths, not production dependencies.

## Phase 5 — scale only from evidence

- Resize or move to dedicated CPU only for sustained CPU/latency evidence.
- Add read-only replicas and a suitable managed edge/gateway only when one VPS
  is insufficient; never put SQLite/Arroy on network/FUSE storage.
- Move the same attested OCI digest and volume contract to an Azure VM only for
  a real production decision; then re-enable the preserved managed-identity,
  Blob, monitoring, and DR adapters.
- Preserve application-level authentication even when a gateway is introduced.

## Cleanup gate

Superseded local build/cache cleanup reduced project usage from 298 GiB to 197
GiB and increased free disk from 76 GiB to 153 GiB. Retain v19 until Linode
VPS replacement, exact readiness, Copilot token validation, and rollback are
proven. Delete no cloud bootstrap/rollback artifact or sole source
of source truth or validation evidence before those gates pass.
