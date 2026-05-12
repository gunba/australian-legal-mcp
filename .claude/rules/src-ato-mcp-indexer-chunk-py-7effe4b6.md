---
paths:
  - "src/ato_mcp/indexer/chunk.py"
---

# src/ato_mcp/indexer/chunk.py

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Index Builder
Build orchestration, heading-aware chunking with overlap, token estimation, packing, manifest, release.

- [IB-21 L87] CHUNKER_FORMAT_VERSION (chunk.py) bumps when chunker output shape changes; pack records emit it at write time and the build fast-path equality check compares manifest content_hashes that already encode the chunker output, so a version bump that genuinely changes chunks invalidates the hash and routes affected docs through Branch 3 (full re-extract + re-embed) naturally.
- [IB-22 L568] _pack_chunks tracks raw word count, not sum-of-int-truncated approx_tokens(block.text), so the projected token total matches approx_tokens of the joined chunk exactly. Per-block int(words * 1.3) truncation otherwise accumulates ~1 token of drift per block, letting chunks land 5–30 tokens over max_tokens when packed from many small blocks.
