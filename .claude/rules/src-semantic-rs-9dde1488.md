---
paths:
  - "src/semantic.rs"
---

# src/semantic.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Granite Embedding Model
Granite ONNX semantic runtime: CPU by default, optional CUDA for maintainer builds, 1024-token dynamic padding, sentence_embedding or mean-pooling, 256-d int8 vectors.

- [EM-01 L215] SemanticRuntime loads Granite ONNX on CPU by default; maintainer --gpu builds require the cuda feature and CUDA execution-provider registration with error_on_failure.
- [EM-04 L324] If the ONNX graph exposes sentence_embedding it is used directly; otherwise pooled_embeddings mean-pools 3D token embeddings with the attention mask and clamps the denominator to avoid div-by-zero on all-padding rows.
- [EM-06 L439] Quantization rejects non-finite values, L2-normalises the first 256 dimensions, clips to [-1, 1], multiplies by 127, rounds, and stores raw int8 bytes.
