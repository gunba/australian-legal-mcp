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
  links/assets/definitions, deterministic FTS/vector ranking, and exact flat
  int8 top-50 equality against SQLite. Schema 11 uses digest-bound
  contentless-delete chunk FTS.
- Pinned mdbr-leaf-ir FP32 graph, exact tokenizer, TensorRT FP16/CUDA local build,
  and CPU serving path.
- Validated/active local chunker-format-6 v22
  `937683b86190ea9bc51f1607c8d517d4848a6f4db413fcc41d8116995e61d939`:
  409,528 documents, 6,986,040 chunks/embeddings, 20,169 definitions, a
  19,758,231,552-byte schema-11 DB, exact model/ten-flat-sidecar bindings,
  preserved typed FRL formula assets, all-source retrieval, rollback, pruning,
  and graceful bounded Streamable HTTP. The HarbourGrid evaluation passes
  authority, formula, `get_asset`, latency, and loaded-readiness gates. The
  schema-10 v19 parent and matching v0.18.1 binary/image remain the local
  disaster-recovery fallback; the schema-11 binary deliberately rejects it.
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
  V0.19.8 transactionally upgrades the historical launcher, detects the live
  rules through verbose UFW status, removes the exact commented web rules, and
  adds the configured-dark state required for a coordinated
  Arroy-image/generation to flat-int8-image/generation cutover. V0.19.10 proves
  all live process capability sets through Podman and provides an exact bridge
  that leaves v0.19.8 code immutable while that code retires its journal, retaining
  recovery after every modelled SIGKILL phase. V0.19.11 narrows exact
  document-scoped FTS work without changing wildcard/case semantics and makes
  the HarbourGrid evaluator enforce private readiness versus public route
  hiding explicitly.
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

Local evidence includes full v22 activation/verification, a zero-failure
HarbourGrid evaluation, all-ten-source retrieval, valid/invalid signed-token and API-key tests,
resource metadata and 401/403 challenges, exact seven descriptors, official
Microsoft v2.4 schema validation; bridged non-root/read-only container probes; zero fixed
HIGH/CRITICAL image findings; Caddy validation; strict disk/mount guards;
packaged ONNX loading; and a clean offline Linode provider plan. The live
instance/volume boundary now also proves Ubuntu 24.04, XFS/reflink volume
adoption and restricted SSH. Exact v0.19.11 host tools and runtime image serve v22;
Arroy v20 is the sole hosted rollback and every cutover/recovery journal is
retired. Exact routes, private/public HarbourGrid, all seven tools, all ten
sources, live empty capability sets, API-key revocation, and reboot recovery
passed after cutover.

## Phase 2 — disposable Linode infrastructure

1. **Completed 2026-07-16:** applied `infra/linode` in Sydney with
   `public_mcp_enabled=false`, distinct administrator/publisher keys, and an
   encrypted 128-GiB volume; verified the attached device is signature-free and
   public 80/443/51235 are closed.
2. **Completed:** bootstrapped the host, cut it over to the v0.19.0 empty-host
   contract, and fully staged v20.
3. **Completed:** recovered the legacy authentication journal, upgraded to V2
   v0.19.5 host tools, configured API-key authentication, and opened only exact
   Caddy 80/443 ingress.
4. **Completed:** proved all seven tools, all ten source partitions, reboot
   recovery, API-key overlap and revocation, exact listeners/UFW, TLS, and a
   transaction-free final state.
5. **Completed:** published and independently verified immutable v0.19.10
   assets, OCI digest, runtime, labels, and attestation; recovered and retired
   the exact v0.19.8 transaction without changing old bytes.
6. **Completed:** upgraded exact host tools, committed the v22/image pair,
   retained v20 as sole rollback, removed all transaction residue, and proved
   private/public HarbourGrid plus all tools and sources.
7. **Completed:** issued `enterprise-laptop`, revoked `second-client`, restored
   only exact authenticated routes, and passed a full host reboot proof.
8. **Completed:** rotated client-specific access to `local-pi` and `work-laptop`,
   revoked `enterprise-laptop`, and proved both new keys plus the current Pi MCP
   connection without placing credentials in the repository or Obsidian vault.
9. **Optional DR proof:** test changed/unchanged generation deltas, image
   rollback, volume detach/reattach, and VPS replacement without another full
   upload.
10. Record ongoing compute/volume cost, p50/p95 latency, CPU, RSS, page cache,
   queue rejection, and disk extent growth.

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
  is insufficient; never put SQLite or mmap vector sidecars on network/FUSE storage.
- Move the same attested OCI digest and volume contract to an Azure VM only for
  a real production decision; then re-enable the preserved managed-identity,
  Blob, monitoring, and DR adapters.
- Preserve application-level authentication even when a gateway is introduced.

## Cleanup gate

The post-v22 reviewed cleanup removed superseded packaged build output,
disposable caches, Cargo debug/cross/package artifacts, completed acquisition
attempts, provider downloads, and one-off scratch work. Allocated project usage
fell from about 300 GiB to about 203 GiB; Btrfs reflink sharing yielded about 23
GiB of additional filesystem free space. Retain the active v22, v21 rollback
parent, hosted Arroy v20 rollback, v19 DR corpus, prepared flat-v20 recovery
copy, canonical sources/models, deployment state, logs, and validation evidence
until the corresponding replacement and rollback gates pass. Delete
no cloud bootstrap/rollback artifact or sole source of source truth or
validation evidence before those gates pass.
