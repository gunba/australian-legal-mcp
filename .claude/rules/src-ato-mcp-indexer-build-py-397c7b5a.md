---
paths:
  - "src/ato_mcp/indexer/build.py"
---

# src/ato_mcp/indexer/build.py

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Index Builder
Build orchestration, heading-aware chunking with overlap, token estimation, packing, manifest, release.

- [IB-13 L73] Windowed processing groups records into args.window_docs (default 20,000); CHECKPOINT_EVERY=20000 commits the in-progress SQLite transaction and flushes the in-flight pack so a kill mid-run loses at most the current build window.
- [IB-17 L97] Production build-index accepts only the EmbeddingGemma maintainer embedder; lexical/hash-vector experiments are not exposed as release corpus builders, and query-time keyword mode is not an alternative corpus embedder.
  - BuildArgs.embedder is Literal['embeddinggemma'], the CLI rejects any other embedder value, and both fresh and incremental build paths raise unless args.embedder is embeddinggemma.
- [IB-19 L184] Fresh EmbeddingGemma builds emit per-window and final embedding telemetry for batch tuning: encode calls, tokens, token and chunk throughput, maximum batch size, maximum approximate padded tokens, total approximate padded tokens, encode_batch_size, and max_batch_tokens.
  - The telemetry is produced by _build_fresh_windowed from EncodedWindow returned by _encode_length_bucketed. It intentionally uses the existing approx_tokens + 16 length estimate so the metric tracks batching pressure without invoking the tokenizer twice.
- [IB-12 L205] Two build paths exist: _build_fresh_windowed performs a full EmbeddingGemma re-embed when previous_manifest is absent, while build() with previous_manifest reuses pack slots only when content_hash is unchanged and the previous pack record is compatible with the current extracted fields.
  - The incremental path now reads the previous pack record before reuse and requires a current definitions_format_version so content-stable documents are repacked when extracted pack-side fields change.
- [IB-14 L438] Resume support: on incremental restart, doc_ids already in documents with a sealed pack_sha8 (not 'PENDING') are skipped — the prior commit landed rows + pack atomically, so the on-disk state is safe to keep.
- [IB-16 L810] Window preparation parallelises HTML extract + chunking via ProcessPoolExecutor with workers = max(1, cpu_count - 1); only the embed + DB-write phases stay single-threaded since they hold the SQLite transaction.
