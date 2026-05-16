---
paths:
  - "src/build.rs"
---

# src/build.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust Index Builder
Rust corpus build orchestration: source-derived cleaning/metadata/chunking, adaptive Granite embedding batches, base-release seeding, checkpoint resume, packs, manifest/update output, and citation derivation.

- [IB-10 L52] Build pack shards seal after BUILD_PACK_RECORDS_PER_SHARD=4096 documents rather than a byte target, keeping pack downloads tractable while preserving stable offsets within each written pack.
- [IB-19 L123] Build --profile emits cumulative stage timings plus embedding telemetry: batch count, inputs, active/padded tokens, padding efficiency, max batch, max sequence length, model tokens/sec, and tokenize/prepare/run/postprocess/write timings.
  - BuildProfile collects these counters during adaptive Granite encoding and prints them once at finalisation.
- [IB-11 L276] Chunk embeddings travel through pack records as base64-encoded raw int8 bytes; install-side decode checks the decoded length against EMBEDDING_DIM before writing chunk_embeddings.
- [IB-13 L385] Build checkpoints persist source index hash, zstd level, embedding model id/fingerprint/dim/max tokens, chunker format, committed doc refs, packs, base docs, base source hashes, and verified source doc_ids.
- [IB-12 L1001] Previous-release seeding copies the base DB and packs, computes a full source-derived fingerprint from each base pack record, and reuses a document only when the current fingerprint matches.
  - The fingerprint covers html, title/date/type, currency fields, navigation flags, anchors, definitions, chunks, and assets, so non-body extraction changes rebuild the document.
- [IB-17 L1068] Maintainer build accepts only the pinned local Granite model files via --model-dir; lexical/hash-vector experiments are not exposed as release corpus builders, and keyword mode is only a query-time path.
  - SemanticModelPaths::from_model_dir validates tokenizer.json, onnx/model_fp16.onnx, and onnx/model_fp16.onnx_data against pinned size and sha256 before build_corpus starts.
- [IB-14 L1148] Resume support skips only checkpoint-committed documents, or verified source doc_ids for a base-seeded checkpoint; any PENDING document rows abort the build and require a fresh release directory.
- [IB-16 L1159] The release corpus builder is a single Rust process with adaptive embedding batches; HTML extraction/chunking and DB writes are in-process, and no separate worker-pool build path is exposed.
- [IB-20 L1763] derive_citations runs during build finalisation and live-DB rebuild, clears and repopulates citations by streaming chunks.text, zstd-decompressing, regex-extracting [doc:X] markers, collapsing qualifiers to base doc_id, and skipping self-citations.
