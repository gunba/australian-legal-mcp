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
  transactional workspaces, strict source quality, schema 10, cleaned HTML,
  links/assets/definitions, deterministic FTS/vector ranking, and ANN recall at
  least 0.99 at 50.
- Pinned mdbr-leaf-ir FP32 graph, exact tokenizer, TensorRT FP16/CUDA local build,
  and CPU serving path.
- Validated/active v19: 409,528 documents, 6,968,250 chunks/embeddings, 20,170
  definitions, exact DB/model/ten-ANN bindings, all-source retrieval, rollback,
  pruning, and graceful bounded Streamable HTTP.
- Removed runtime corpus download/publication/offline-bundle paths.
- Added immutable activation, strict verification, lifecycle locks, durable
  maintainer resumption, exact-generation readiness, and hardened systemd.
- Added a hardened non-root OCI image, lock-pinned Linode OpenTofu, strict
  XFS-volume adoption, CoW/rsync delta deployment, narrow publisher/root
  transactions, Caddy, API-key plus Entra auth, signed image provenance/SBOM
  policy, RFC 9728 metadata/challenges, and Copilot templates.
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

Local evidence includes valid/invalid signed-token and API-key tests; resource
metadata and 401/403 challenges; exact seven descriptors; official Microsoft
v2.4 schema validation; bridged non-root/read-only container probes; zero fixed
HIGH/CRITICAL image findings; Caddy validation; strict disk/mount guards;
packaged ONNX loading; and a clean offline Linode provider plan. The live
instance/volume boundary now also proves Ubuntu 24.04, a signature-free attached
volume, restricted SSH, and closed public 80/443/51235. Volume initialization,
host installation, and Microsoft tenant behavior remain in the cloud phases.

## Phase 2 — disposable Linode infrastructure

1. **Completed 2026-07-16:** applied `infra/linode` in Sydney with
   `public_mcp_enabled=false`, distinct administrator/publisher keys, and an
   encrypted 128-GiB volume; verified the attached device is signature-free and
   public 80/443/51235 are closed.
2. Run the host installer and prove signature-free initialization, exact
   UUID/marker adoption, XFS reflink, fixed identities, UFW, Quadlet, and
   digest-pinned runtime verification.
3. Perform one v19 CoW/rsync upload and bootstrap activation.
4. Configure API-key and/or Entra auth privately; only then open Cloud Firewall
   80/443 and run the transactional Caddy cutover.
5. Test reboot, changed/unchanged generation deltas, readiness rollback,
   API-key rotation/revocation, image rollback, volume detach/reattach, and VPS
   replacement without another full upload.
6. Record compute/volume cost, p50/p95 latency, CPU, RSS, page cache, queue
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

Delete historical local builds/snapshots or cloud bootstrap/rollback artifacts
only after local and Linode activation, VPS replacement, exact readiness,
Copilot token validation, and rollback are proven, and no surviving item is the
sole source of source truth or validation evidence.
