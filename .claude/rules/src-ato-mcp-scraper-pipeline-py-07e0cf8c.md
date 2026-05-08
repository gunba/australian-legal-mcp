---
paths:
  - "src/ato_mcp/scraper/pipeline.py"
---

# src/ato_mcp/scraper/pipeline.py

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Source Scraper
Incremental/full/catch-up scraping from ato.gov.au, threadpool, snapshot, tree crawler.

- [SS-01 L44] Three scrape modes from refresh_source(): 'incremental' (What's New feed, ~2-3 week rolling window), 'full' (whole crawl + reduce + download, hours), 'catch_up' (diff missing canonical_ids and download only those — for use after long gaps).
- [SS-04 L91] Default refresh_source pacing: request_interval=0.5s, max_workers=1. Concurrency is intentionally restrained — the rate lock would serialise anyway, and anything faster risks tripping ATO's rate guard.
- [SS-06 L213] catch_up mode inherits each new doc's category from the reducer's representative_path so the new payloads land in payloads/<Category>/... matching the existing tree shape — no manual category assignment needed.
