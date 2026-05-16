---
paths:
  - "src/source.rs"
---

# src/source.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust CLI Commands
Closed clap command surface covering end-user MCP/update/doctor/search commands plus maintainer source, build, and release commands in the Rust binary.

- [CC-03 L403] ato-mcp update and the serve startup availability probe both gate on enforce_manifest_compatibility / enforce_update_summary_compatibility. Incompatible manifests surface as an upgrade-the-binary error from update; the probe silently suppresses incompatible-summary cases so the agent never points the user at an action that could not succeed.
  - ATO_MCP_OFFLINE=1 disables the startup probe entirely; the server still starts and serves whatever local corpus is present.

## Rust Output Formatters
JSON output for hits, document outline + section + full renderers.

- [OF-06 L202] JSON outputs use serde_json::to_string_pretty or to_vec_pretty before returning/writing, so CLI/MCP JSON responses and installed manifests are deterministic human-readable JSON strings/files.

## Rust Server Wiring
MCP tool registration, shared ServerState, runtime statistics instructions, install/update notices, and the small explicit tool surface.

- [SW-05 L176] prefix_breakdown is corpus-derived: per-prefix doc counts plus a sample title used as the description. Replaces the hand-maintained prefix-to-doc-type map; surfaced via stats() so agents discover the canonical `doc_scope="<PREFIX>/%"` filter idiom for every prefix in the corpus.
- [SW-06 L1024] Serve startup runs a synchronous non-mutating availability probe (check_for_update_availability + http_probe_client + fetch_bytes_probe) with a tight 5s budget. It reuses the same fingerprint/compat helpers as the update fast-path skip; every error / timeout / missing installed manifest / incompatible summary collapses to None, so a slow network cannot stall the MCP stdio loop. The Option<UpdateAvailability> is stashed on ServerState and read by server_instructions to surface the update-available notice to the agent.
  - ATO_MCP_OFFLINE=1 short-circuits the probe before any I/O.

## Rust Source Scraper
Maintainer source acquisition commands for What's New incremental pulls, tree crawl snapshots, snapshot reduction, deduped catch-up, and paced link download.

- [SS-03 L1252] Maintainer ATO API pacing uses a mutex-protected Instant before outgoing tree-crawl/link-download requests, serializing issuance across workers for the configured interval.
- [SS-07 L1460] snapshot_reduce dedupes canonical IDs across the tree, chooses a representative_path, records redundant folders, and filters excluded titles plus descendants before writing deduped_links and skip lists.
- [SS-06 L1772] link-download builds payload paths under payloads/ from each link record representative_path, so catch-up records inherit the reducer source path without manual category assignment.
- [SS-08 L1868] link_download uses up to max_workers threads with a shared queue, reqwest client, index map/writer, progress counters, and request-delay lock.

## Rust Update Mechanism
End-user update flow: update.json fast-path when local DB/model match, otherwise staged model/corpus rebuild and guarded promotion, with single-writer LOCK and doctor rollback backup.

- [UM-06 L290] doctor --rollback restores backups/ato.db.prev over the live DB; successful update promotion persists the previous DB as that rollback backup only after model, DB, assets, and manifest promotion have reached the commit point.
  - Transient promotion guards restore DB/assets/manifest/model on failed promotion before commit, and failed promotions do not replace the existing doctor rollback backup.
- [UM-01 L381] Single-writer guard: apply_update takes the app LOCK file before apply_update_locked and releases it afterwards; serve/search paths open read-only DB connections and do not take the writer lock.
- [UM-05 L451] Update flow short-circuits via update.json only when the installed corpus and local Granite model files match the published summary; otherwise it fetches manifest.json, stages model files, rebuilds DB/assets in staging, then promotes model, DB, assets, and installed_manifest with rollback guards.
  - There is no in-place delete+insert path; full rebuild on a fresh SQLite file is faster than mutating the live multi-GB DB and avoids FK cascades wiping derived tables mid-update.
- [UM-07 L753] rebuild_live_db_from_manifest calls derive_citations between the bulk pack insert and verify_semantic_install. Freshly-inserted chunks carry no citation rows, so every row must be derived in the staging DB before the atomic swap; skipping it ships an install with an empty citations table. Idempotent: clears + repopulates by streaming chunks.text once and regex-extracting [doc:X] markers.
- [UM-03 L876] Fetch helpers resolve local paths, file://, manifest-relative assets, HTTP(S), and hf:// Granite model file URLs; downloaded model bundle/file and pack bytes are sha256-verified when the manifest or pinned model metadata provides a hash.
  - HF model installs verify each pinned Granite file and non-HF model bundles require explicit sha256 and positive size metadata.
