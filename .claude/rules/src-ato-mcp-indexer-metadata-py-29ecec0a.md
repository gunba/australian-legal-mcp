---
paths:
  - "src/ato_mcp/indexer/metadata.py"
---

# src/ato_mcp/indexer/metadata.py

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Index Extraction And Chunking

- [IB-18 L49] doc_id is the ATO's docid path verbatim — prefix, case, slashes preserved — extracted from the canonical URL's docid= query parameter; falls back to the raw URL when that's missing so we always have a unique key.
