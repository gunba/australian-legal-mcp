---
paths:
  - "src/html.rs"
---

# src/html.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust Extraction And Chunking
Source HTML cleaning, container selection, leading-heading title composition, doc_id extraction, and block-aware chunking all live in the Rust binary.

- [IB-06 L8] HTML container selection is deterministic: try #LawContent, #lawContents, #LawContents, then #contents, falling back to main/body if none match.
