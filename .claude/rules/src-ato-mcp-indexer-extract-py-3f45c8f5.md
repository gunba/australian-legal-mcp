---
paths:
  - "src/ato_mcp/indexer/extract.py"
---

# src/ato_mcp/indexer/extract.py

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Index Extraction And Chunking

- [IB-07 L155] Document title is composed from a small number of leading headings (h1=doc_type, h2=code, h3=subject on rulings) via _compose_title with prefix-overlap suppression; falls back to the raw <title> when no leading headings present.
- [IB-06 L178] HTML container is picked from a fallback chain (#LawContent → #lawContents → #contents → #content → article → main → body), absorbing the various wrapper IDs ATO has used over the years.
- [IB-08 L204] extract injects heading id attributes back into the markdown as ' {#anchor}' suffixes so chunks can reference sections directly; markdownify runs with heading_style=ATX, bullets='-', and script/style/iframe stripped.
- [IE-03 L333] Plain prose blocks are unwrapped after markdownify so source line breaks inside inline spans, quoted fragments, and hyphenated amendment ranges stay inline in emitted markdown.
  - Structural markdown blocks such as headings, lists, blockquotes, tables, fences, and thematic breaks are left unchanged; tests cover the S 355-210 amendment-note pattern.
