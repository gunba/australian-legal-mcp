"""Orchestrator for the ATO scrape pipeline.

Collapses taxiv's ``download_pages`` + ``reduce_snapshot`` + ``run_pipeline`` +
``whatsnew_update`` into a single ``refresh_source(mode, output_dir)`` entry
point. Produces (or updates) the ``output_dir/index.jsonl`` +
``output_dir/payloads/`` layout that the indexer consumes.

Three modes:

- ``incremental`` — pulls the ATO ``What's new`` feed, refreshes matching
  payloads, and writes any new documents under their classified
  ``payloads/<Category>/`` path. Covers the rolling 2-3 week window the
  feed exposes.
- ``full`` — runs the whole crawl + reduce + download pipeline. Takes hours;
  intended for monthly full rebuilds.
- ``catch_up`` — runs a fresh tree crawl, diffs the resulting canonical IDs
  against the existing ``output_dir/index.jsonl``, and downloads **only the
  missing** docs. Each new doc inherits its category from the reducer's
  ``representative_path``, so they land in the correct
  ``payloads/<Category>/...`` subfolder automatically. Use this after long
  gaps where the What's New feed has rolled past the last scrape.
"""
from __future__ import annotations

import json
import logging
import tempfile
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Literal, Optional


from .client import AtoBrowseClient
from .downloader import LinkDownloader
from .reducer import SnapshotReducer
from .snapshot import SnapshotWriter
from .tree_crawler import AtoTreeCrawler
from .whats_new import DedupedLinkIndex, WhatsNewFetcher, build_pending_record, normalize_doc_href

LOGGER = logging.getLogger(__name__)

Mode = Literal["incremental", "full", "catch_up"]
# [SS-01] Three scrape modes: incremental (What's New feed, ~2-3 week
# window), full (whole crawl, hours), catch_up (diff missing canonical_ids
# — for use after long gaps where What's New rolled past).


@dataclass
class RefreshResult:
    mode: Mode
    output_dir: Path
    whats_new_summary: dict[str, Any] | None = None
    snapshot_dir: Path | None = None
    catch_up_summary: "CatchUpSummary | None" = None


@dataclass
class CatchUpSummary:
    """Outcome of a catch-up run."""

    total_current_links: int
    existing_canonical_ids: int
    missing: int
    downloaded: int
    snapshot_dir: Path
    diff_file: Path
    by_category: dict[str, int]

    def as_dict(self) -> dict[str, Any]:
        return {
            "total_current_links": self.total_current_links,
            "existing_canonical_ids": self.existing_canonical_ids,
            "missing": self.missing,
            "downloaded": self.downloaded,
            "snapshot_dir": str(self.snapshot_dir),
            "diff_file": str(self.diff_file),
            "by_category": self.by_category,
        }


def refresh_source(
    *,
    mode: Mode = "incremental",
    output_dir: Path | str,
    links_file: Path | str | None = None,
    snapshot_dir: Path | str | None = None,
    base_url: str = "https://www.ato.gov.au",
    whats_new_url: str = "https://www.ato.gov.au/law/view/whatsnew.htm?fid=whatsnew",
    parser_run_date: str | None = None,
    max_workers: int = 1,
    request_interval: float = 0.5,
    # [SS-04] Default pacing: request_interval=0.5s, max_workers=1 — concurrency restrained because the rate lock would serialise anyway and faster risks ATO's rate guard.
    verbose_progress: bool = False,
    force: bool = True,
    root_query: str = "Mode=type&Action=initialise",
    max_nodes: int | None = None,
    path_prefix: list[str] | None = None,
) -> RefreshResult:
    output_dir = Path(output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    parser_run_date = parser_run_date or datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    if mode == "incremental":
        if links_file is None:
            links_file = output_dir.parent / "ato_snapshots" / "deduped_links.jsonl"
        links_file = Path(links_file)
        if not links_file.exists():
            raise FileNotFoundError(
                f"deduped_links.jsonl not found at {links_file}. Run a full crawl first."
            )
        summary = _run_whats_new(
            links_file=links_file,
            output_dir=output_dir,
            whats_new_url=whats_new_url,
            base_url=base_url,
            parser_run_date=parser_run_date,
            max_workers=max_workers,
            request_interval=request_interval,
            verbose_progress=verbose_progress,
            force=force,
        )
        return RefreshResult(mode="incremental", output_dir=output_dir, whats_new_summary=summary)

    if mode == "catch_up":
        snapshot_base = Path(snapshot_dir) if snapshot_dir else output_dir.parent / "ato_snapshots"
        snapshot_base.mkdir(parents=True, exist_ok=True)
        summary = _run_catch_up(
            output_dir=output_dir,
            snapshot_base=snapshot_base,
            base_url=base_url,
            parser_run_date=parser_run_date,
            max_workers=max_workers,
            request_interval=request_interval,
            verbose_progress=verbose_progress,
            root_query=root_query,
            max_nodes=max_nodes,
            path_prefix=path_prefix,
        )
        return RefreshResult(
            mode="catch_up",
            output_dir=output_dir,
            snapshot_dir=summary.snapshot_dir,
            catch_up_summary=summary,
        )

    # full mode
    snapshot_base = Path(snapshot_dir) if snapshot_dir else output_dir.parent / "ato_snapshots"
    snapshot_base.mkdir(parents=True, exist_ok=True)

    client = AtoBrowseClient(request_interval=request_interval)
    crawler = AtoTreeCrawler(client)
    nodes = crawler.crawl(root_query=root_query, max_nodes=max_nodes)
    writer = SnapshotWriter(base_dir=snapshot_base)
    snap_dir, meta = writer.write(nodes, root_query=root_query, output_dir=snapshot_base)
    LOGGER.info("Crawl complete: %s nodes (%s links)", meta.node_count, meta.link_count)

    reducer = SnapshotReducer(snap_dir / "nodes.jsonl")
    outputs = reducer.run(output_dir=snap_dir)
    links_path = outputs["deduped_links"]

    downloader = LinkDownloader(
        deduped_links_path=links_path,
        output_dir=output_dir,
        base_url=base_url,
        parser_run_date=parser_run_date,
        request_delay=request_interval,
        verbose_progress=verbose_progress,
    )
    downloader.download_all(force=force, max_workers=max_workers)

    summary = _run_whats_new(
        links_file=links_path,
        output_dir=output_dir,
        whats_new_url=whats_new_url,
        base_url=base_url,
        parser_run_date=parser_run_date,
        max_workers=max_workers,
        request_interval=request_interval,
        verbose_progress=verbose_progress,
        force=True,
    )
    return RefreshResult(
        mode="full",
        output_dir=output_dir,
        whats_new_summary=summary,
        snapshot_dir=snap_dir,
    )


def _run_catch_up(
    *,
    output_dir: Path,
    snapshot_base: Path,
    base_url: str,
    parser_run_date: str,
    max_workers: int,
    request_interval: float,
    verbose_progress: bool,
    root_query: str,
    max_nodes: int | None,
    path_prefix: list[str] | None = None,
) -> CatchUpSummary:
    """Crawl the tree, diff against the existing index, download just the new docs.

    ``representative_path`` from the reducer is relative to the crawl root. For
    a full-tree crawl that starts with the correct top-level category
    ("Public rulings", "Cases", etc.). For a scoped crawl the caller must pass
    ``path_prefix`` — the ancestor folders from the absolute root down to the
    scope — so the downloader writes files to the same locations a full crawl
    would.

    Without ``path_prefix``, we refuse to run a scoped crawl and raise.
    """
    # [SS-06] catch_up inherits each new doc's category from the reducer's representative_path so payloads land in payloads/<Category>/... matching the existing tree shape.
    existing = _load_existing_canonical_ids(output_dir / "index.jsonl")
    if root_query != "Mode=type&Action=initialise" and not path_prefix:
        raise ValueError(
            "scoped catch_up requires path_prefix "
            "(e.g. ['Public_rulings','Rulings','Class']) so new payloads land "
            "under the correct ato_pages/payloads/<category>/... folder"
        )

    client = AtoBrowseClient(request_interval=request_interval)
    crawler = AtoTreeCrawler(client)
    LOGGER.info("crawling browse-content tree (root=%s, max_nodes=%s)", root_query, max_nodes)
    nodes = crawler.crawl(root_query=root_query, max_nodes=max_nodes)
    writer = SnapshotWriter(base_dir=snapshot_base)
    snap_dir, meta = writer.write(nodes, root_query=root_query, output_dir=snapshot_base)
    LOGGER.info("crawl: %s nodes (%s links) -> %s", meta.node_count, meta.link_count, snap_dir)

    reducer = SnapshotReducer(snap_dir / "nodes.jsonl")
    outputs = reducer.run(output_dir=snap_dir)
    links_path = outputs["deduped_links"]

    # Build the filtered "missing" links file, prepending path_prefix if needed.
    missing_path = snap_dir / "missing_links.jsonl"
    total_current = 0
    missing_count = 0
    by_category: dict[str, int] = {}
    with open(links_path, "r", encoding="utf-8") as src, open(missing_path, "w", encoding="utf-8") as dst:
        for line in src:
            total_current += 1
            rec = json.loads(line)
            cid = normalize_doc_href(rec.get("canonical_id", ""))
            if not cid or cid in existing:
                continue
            if path_prefix:
                rep = list(path_prefix) + list(rec.get("representative_path") or [])
                rec["representative_path"] = rep
            else:
                rep = rec.get("representative_path") or []
            missing_count += 1
            category = rep[0] if rep else "(uncategorized)"
            by_category[category] = by_category.get(category, 0) + 1
            dst.write(json.dumps(rec) + "\n")
    LOGGER.info(
        "diff: %d current, %d existing, %d missing (%d categories)",
        total_current, len(existing), missing_count, len(by_category),
    )

    downloaded = 0
    if missing_count:
        downloader = LinkDownloader(
            deduped_links_path=missing_path,
            output_dir=output_dir,
            base_url=base_url,
            parser_run_date=parser_run_date,
            request_delay=request_interval,
            verbose_progress=verbose_progress,
        )
        # Force=False: respect existing payloads if somehow present (cheap retries).
        downloader.download_all(force=False, max_workers=max_workers)
        downloaded = _count_success_since(output_dir / "index.jsonl", parser_run_date)

    return CatchUpSummary(
        total_current_links=total_current,
        existing_canonical_ids=len(existing),
        missing=missing_count,
        downloaded=downloaded,
        snapshot_dir=snap_dir,
        diff_file=missing_path,
        by_category=dict(sorted(by_category.items(), key=lambda kv: -kv[1])),
    )


def _load_existing_canonical_ids(index_path: Path) -> set[str]:
    out: set[str] = set()
    if not index_path.exists():
        return out
    with open(index_path, "r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            rec = json.loads(line)
            cid = normalize_doc_href(rec.get("canonical_id", ""))
            if cid:
                out.add(cid)
    return out


def _count_success_since(index_path: Path, parser_run_date: str) -> int:
    """Count how many index.jsonl rows have status=success and downloaded_at >= parser_run_date."""
    if not index_path.exists():
        return 0
    n = 0
    with open(index_path, "r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            rec = json.loads(line)
            if rec.get("status") != "success":
                continue
            ts = rec.get("downloaded_at", "")
            if ts and ts >= parser_run_date:
                n += 1
    return n


def _run_whats_new(
    *,
    links_file: Path,
    output_dir: Path,
    whats_new_url: str,
    base_url: str,
    parser_run_date: str,
    max_workers: int,
    request_interval: float,
    verbose_progress: bool,
    force: bool,
    html_fetcher: Optional[Callable[[str], str]] = None,
    page_fetcher: Optional[Callable[[str], tuple[int, str]]] = None,
    asset_fetcher: Optional[Callable[[str], bytes]] = None,
) -> dict[str, Any]:
    fetcher = WhatsNewFetcher(whats_new_url, base_url=base_url, fetcher=html_fetcher)
    entries = fetcher.fetch_entries()
    dedup_index = DedupedLinkIndex(links_file)

    known, pending = [], []
    for entry in entries:
        match = dedup_index.find(entry.href)
        if match:
            known.append(match)
        else:
            pending.append(build_pending_record(entry))

    summary = {
        "whats_new_url": whats_new_url,
        "total_links": len(entries),
        "refreshed_links": len(known),
        "pending_links": len(pending),
        "run_started_at": datetime.now(timezone.utc).isoformat(),
    }

    if known:
        LOGGER.info("Refreshing %s existing payload(s)", len(known))
        _download_records(
            records=known,
            output_dir=output_dir,
            base_url=base_url,
            parser_run_date=parser_run_date,
            max_workers=max_workers,
            request_interval=request_interval,
            verbose_progress=verbose_progress,
            force=force,
            page_fetcher=page_fetcher,
            asset_fetcher=asset_fetcher,
        )
    if pending:
        LOGGER.info("Writing %s pending What's New document(s)", len(pending))
        _download_records(
            records=pending,
            output_dir=output_dir,
            base_url=base_url,
            parser_run_date=parser_run_date,
            max_workers=max_workers,
            request_interval=request_interval,
            verbose_progress=verbose_progress,
            force=True,
            page_fetcher=page_fetcher,
            asset_fetcher=asset_fetcher,
        )

    summary["run_completed_at"] = datetime.now(timezone.utc).isoformat()
    summary["processed_ids"] = sorted(
        {record["canonical_id"] for record in (*known, *pending) if record.get("canonical_id")}
    )
    (output_dir / "whats_new_summary.json").write_text(
        json.dumps(summary, indent=2), encoding="utf-8"
    )
    return summary


def _download_records(
    *,
    records: list[dict[str, Any]],
    output_dir: Path,
    base_url: str,
    parser_run_date: str,
    max_workers: int,
    request_interval: float,
    verbose_progress: bool,
    force: bool,
    page_fetcher: Optional[Callable[[str], tuple[int, str]]],
    asset_fetcher: Optional[Callable[[str], bytes]],
) -> None:
    if not records:
        return
    with tempfile.NamedTemporaryFile("w", delete=False, encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record) + "\n")
        temp_path = Path(handle.name)
    try:
        downloader = LinkDownloader(
            deduped_links_path=temp_path,
            output_dir=output_dir,
            base_url=base_url,
            parser_run_date=parser_run_date,
            request_delay=request_interval,
            verbose_progress=verbose_progress,
            fetcher=page_fetcher,
            asset_fetcher=asset_fetcher,
        )
        downloader.download_all(force=force, max_workers=max_workers)
    finally:
        temp_path.unlink(missing_ok=True)
