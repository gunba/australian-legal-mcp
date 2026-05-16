---
paths:
  - "src/build.rs"
---

# src/build.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust Index Builder
Rust corpus build orchestration: source-derived cleaning/metadata/chunking, adaptive Granite embedding batches, base-release seeding, checkpoint resume, packs, manifest/update output, and citation derivation.

- [IB-19 L141] Build --profile emits cumulative stage timings plus embedding telemetry: batch count, inputs, active/padded tokens, padding efficiency, max batch, max sequence length, model tokens/sec, and tokenize/prepare/run/postprocess/write timings.
  - BuildProfile collects these counters during adaptive Granite encoding and prints them once at finalisation.
- [IB-13 L306] Build checkpoints persist source index hash, zstd level, embedding model id/fingerprint/dim/max tokens, chunker format, committed doc refs, packs, base docs, base source hashes, and verified source doc_ids.
- [IB-17 L617] Maintainer build accepts only the pinned local Granite model files via --model-dir; lexical/hash-vector experiments are not exposed as release corpus builders, and keyword mode is only a query-time path.
  - SemanticModelPaths::from_model_dir validates tokenizer.json, onnx/model_fp16.onnx, and onnx/model_fp16.onnx_data against pinned size and sha256 before build_corpus starts.
- [IB-14 L681] Resume support skips only checkpoint-committed documents, or verified source doc_ids for a base-seeded checkpoint; any PENDING document rows abort the build and require a fresh release directory.
- [IB-16 L692] The release corpus builder is a single Rust process with adaptive embedding batches; HTML extraction/chunking and DB writes are in-process, and no separate worker-pool build path is exposed.
- [IB-20 L1205] derive_citations runs during build finalisation and live-DB rebuild, clears and repopulates citations by streaming chunks.text, zstd-decompressing, regex-extracting [doc:X] markers, collapsing qualifiers to base doc_id, and skipping self-citations.
