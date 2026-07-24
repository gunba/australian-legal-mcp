# Focused QA remediation plan

This plan addresses the maintainable, source-grounded defects demonstrated by
the 21 July 2026 ten-source QA run.

## Scope

1. **Shared heading/body chunking**
   - A heading opens a pending structural section instead of becoming a
     standalone chunk.
   - The first substantive block carries the heading.
   - Oversized continuations retain the heading in their search and embedding
     input.
   - The rule is shared by every source after normalization to structural HTML.

2. **South Australian extraction**
   - Preserve official RTF headings, paragraphs, lists and hyperlinks before
     the shared chunker runs.
   - Reject malformed placeholder output through source fixtures and acquisition
     validation.

3. **Federal Court extraction**
   - Validate official HTML structurally, prefer an official Word rendition when
     HTML is degraded, and use PDF/OCR only as the final fallback.
   - Preserve numbered paragraphs and attached footnotes.

4. **High Court discovery**
   - Discover judgment categories from the official index rather than a fixed
     category list.
   - Fail closed when a published judgment listing is malformed or silently
     loses records, while distinguishing other official resource collections.
   - Keep the source official-only. The current Court index has no reported
     collection for 1960–1997 and labels its 1906–1994 unreported collection
     incomplete, so Mabo `[1992] HCA 23` and Kable `[1996] HCA 24` are explicit
     external coverage limitations rather than impossible inventory gates.

5. **Lexical performance**
   - Remove the any-two-terms fallback. Keyword search remains strict; hybrid
     recall continues to come from embeddings.
   - Build one immutable generation-bound lexical sidecar per source containing
     postings and compact filtering metadata, never document or chunk payloads.
   - Query the sidecar and hydrate only final winners from `legal.db`.
   - Select the simplest exact sidecar format that demonstrates a maintainer
     lexical-stage p95 below 100 ms for the unscoped Federal Court QA query.
     Measure cold reads separately; do not use result caching to satisfy the
     target.

6. **Internal observability**
   - Log queue, lexical, embedding, vector scan, fusion/hydration and total
     durations under a request identifier.
   - Keep scores, timings, model details and candidate counts out of MCP
     responses.

## Preserved contracts

- Exactly seven tools and one explicit source per search.
- Semantic search and flat-int8 sidecars remain.
- Deterministic ordering, filters, typed references and continuations remain.
- Builds and acquisition stay off production.
- Generations and all sidecars are immutable, hash-bound and fail closed.

## Explicit non-goals

No abbreviation or synonym system, neutral-citation/provision parser,
automatic title-to-document scoping, generic legal-content scoring, manual title
aliases, new anchor/version model, indexed fallback in `fetch`, public scores or
timings, new MCP tool, result-cache dependency, or vector-only fallback.

## Release gates

- No standalone heading chunk where adjacent substantive body text exists.
- NSW s 18, Queensland s 302 and Tasmania s 157 retain their heading context.
- South Australian structure/link fixtures and Federal Court
  paragraph/footnote fixtures pass.
- Official High Court discovery traverses every published judgment category,
  rejects malformed non-empty listings, and reports the official 1960–1997 gap
  without guessed URLs, manual records or third-party substitution.
- The strict source-scoped lexical path is exact under all current filters and
  tie rules and meets the measured performance target.
- Internal timing is correlated and public response contracts remain clean.
- Full Rust, Python, shell, security, packaging, source-isolation, flat-int8,
  HarbourGrid and host-contract gates pass before release or deployment.
