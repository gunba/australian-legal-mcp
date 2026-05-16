---
paths:
  - "src/extract.rs"
---

# src/extract.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust Extraction And Chunking
Source HTML cleaning, container selection, leading-heading title composition, doc_id extraction, and block-aware chunking all live in the Rust binary.

- [IB-07 L469] Document title composition starts from leading headings, suppresses adjacent prefix-overlap duplicates, and falls back to the cleaned source title or canonical_id in the build path.
- [IB-18 L1114] doc_id is the ATO docid query path verbatim with prefix/case/slashes preserved; missing or malformed canonical URLs fall back to canonical_id so every source record has a stable key.
