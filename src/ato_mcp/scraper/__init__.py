"""ATO browse-content scraper (ported from taxiv).

Public entry point: :func:`refresh_source` — runs an incremental ``What's New``
refresh or a full crawl + reduce + download, producing ``index.jsonl`` and
``payloads/**/*.html`` under the supplied output directory.

The Postgres writer from taxiv is intentionally not ported. The indexer
package consumes the raw ``payloads/`` + ``index.jsonl`` layout directly.
"""
from __future__ import annotations

from .client import AtoBrowseClient, AtoBrowseClientError
from .constants import EXCLUDED_TITLES
from .downloader import LinkDownloader
from .pipeline import refresh_source
from .reducer import SnapshotReducer
from .snapshot import SnapshotMeta, SnapshotWriter
from .tree_crawler import AtoTreeCrawler, SnapshotNode
from .whats_new import DedupedLinkIndex, WhatsNewFetcher, build_pending_record

__all__ = [
    "AtoBrowseClient",
    "AtoBrowseClientError",
    "AtoTreeCrawler",
    "DedupedLinkIndex",
    "EXCLUDED_TITLES",
    "LinkDownloader",
    "SnapshotMeta",
    "SnapshotNode",
    "SnapshotReducer",
    "SnapshotWriter",
    "WhatsNewFetcher",
    "build_pending_record",
    "refresh_source",
]
