---
paths:
  - "src/pack.rs"
---

# src/pack.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust Index Builder
Rust corpus build orchestration: source-derived cleaning/metadata/chunking, adaptive Granite embedding batches, base-release seeding, checkpoint resume, packs, manifest/update output, and citation derivation.

- [IB-09 L5,25] Pack record format is length:uint32 little-endian followed by zstd(JSON record); the trailer stores MAGIC, record count, index offset/length, and a zstd(JSON) reverse index of doc_id, offset, and length.
