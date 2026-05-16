---
paths:
  - "src/chunker.rs"
---

# src/chunker.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust Index Builder
Rust corpus build orchestration: source-derived cleaning/metadata/chunking, adaptive Granite embedding batches, base-release seeding, checkpoint resume, packs, manifest/update output, and citation derivation.

- [IB-21 L17] CHUNKER_FORMAT_VERSION is part of the build checkpoint gate; output-shape changes must bump it so stale checkpoints fail instead of silently resuming incompatible chunk records.
- [IB-22 L677] chunker_pack projects chunk size from accumulated raw word counts, not summed per-block integer token estimates, so per-block truncation drift cannot push packed chunks over the max-token budget.
