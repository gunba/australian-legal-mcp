---
paths:
  - "src/source.rs"
---

# src/source.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust CLI Commands
Closed clap command surface covering end-user MCP/update/doctor/search commands plus maintainer source, build, and release commands in the Rust binary.

- [CC-03 L326] ato-mcp update and the serve startup availability probe both gate on enforce_manifest_compatibility. Incompatible manifests surface as an upgrade-the-binary error from update; the probe silently suppresses incompatible-manifest cases so the agent never points the user at an action that could not succeed.
  - ATO_MCP_OFFLINE=1 disables the startup probe entirely; the server still starts and serves whatever local corpus is present.

## Rust Output Formatters
JSON output for hits, document outline + section + full renderers.

- [OF-06 L196] JSON outputs use serde_json::to_string_pretty or to_vec_pretty before returning/writing, so CLI/MCP JSON responses and installed manifests are deterministic human-readable JSON strings/files.

## Rust Server Wiring
MCP tool registration, shared ServerState, runtime statistics instructions, install/update notices, and the small explicit tool surface.

- [SW-05 L168] prefix_breakdown is corpus-derived: per-prefix doc counts plus a sample title used as the description. Replaces the hand-maintained prefix-to-doc-type map; surfaced via stats() so agents discover the canonical `doc_scope="<PREFIX>/%"` filter idiom for every prefix in the corpus.
- [SW-06 L794] Serve startup runs a synchronous non-mutating availability probe (check_for_update_availability + http_probe_client + fetch_bytes_probe) with a tight 5s budget. It reuses the same fingerprint/compat helpers as the update flow; every error / timeout / missing installed manifest / incompatible manifest collapses to None, so a slow network cannot stall the MCP HTTP server. The Option<UpdateAvailability> is stashed on ServerState and read by server_instructions to surface the update-available notice to the agent.
  - ATO_MCP_OFFLINE=1 short-circuits the probe before any I/O.

## Rust Source Scraper
Maintainer source acquisition commands for What's New incremental pulls, tree crawl snapshots, snapshot reduction, deduped catch-up, and paced link download.

- [SS-02 L962] fetch_nodes_blocking calls the ATO browse-content API through a reqwest blocking client and expects the response payload to be a JSON list.
- [SS-03 L1014] Maintainer ATO API pacing uses a mutex-protected Instant before outgoing tree-crawl/link-download requests, serializing issuance across workers for the configured interval.
- [SS-07 L1222] snapshot_reduce dedupes canonical IDs across the tree, chooses a representative_path, records redundant folders, and filters excluded titles plus descendants before writing deduped_links and skip lists.
- [SS-06 L1534] link-download builds payload paths under payloads/ from each link record representative_path, so catch-up records inherit the reducer source path without manual category assignment.
- [SS-08 L1630] link_download uses up to max_workers threads with a shared queue, reqwest client, index map/writer, progress counters, and request-delay lock.

## Rust Update Mechanism
End-user update flow: update.json fast-path when local DB/model match, otherwise staged model/corpus rebuild and guarded promotion, with single-writer LOCK and doctor rollback backup.

- [UM-01 L316] Single-writer guard: apply_update takes the app LOCK file before apply_update_locked and releases it afterwards; serve/search paths open read-only DB connections and do not take the writer lock.
