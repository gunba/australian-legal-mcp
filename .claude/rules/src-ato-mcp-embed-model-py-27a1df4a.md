---
paths:
  - "src/ato_mcp/embed/model.py"
---

# src/ato_mcp/embed/model.py

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Embedding Model
EmbeddingGemma ONNX, Matryoshka 256-dim, int8 quantization, query/passage prefixes, lexical-hash fallback.

- [EM-02 L26] Query and passage embeddings get distinct prefixes per EmbeddingGemma docs: 'task: search result | query: ' for is_query=True, 'title: none | text: ' for passages. Applied at encode time, not at storage.
- [EM-01 L70] ONNX Runtime providers: CUDAExecutionProvider preferred when available in ort.get_available_providers(), otherwise CPUExecutionProvider — same code runs on enterprise laptops without GPU.
- [EM-03 L99] Tokenizer is configured with truncation at MAX_TOKENS=1024 and dynamic batch padding (length=None pads to the batch max), so a small batch isn't penalised by a fixed sequence length.
- [EM-04 L141] If the ONNX graph exposes a 'sentence_embedding' output it is used directly; otherwise the loader falls back to mean-pooling 3D token embeddings with the attention mask, dividing by clipped mask sum to avoid div-by-zero on all-padding rows.
- [EM-05 L148] Matryoshka representation: the model returns full-dim embeddings; encode() truncates to the first EMBEDDING_DIM=256 dimensions before normalize+quantize, so smaller indices stay compatible with the same model file.
- [EM-06 L151] Quantization: after L2 normalize, vectors are clipped to [-1, 1], multiplied by 127, rounded, and cast to int8 — saturating outliers rather than scaling globally so a single rogue dimension can't squash the rest.
