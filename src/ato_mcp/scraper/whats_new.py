"""ATO 'What's New' scraper via the Rust ato-mcp binary.

Thin Python subprocess wrapper. The HTTP fetch + HTML parsing + canonical
href normalisation live in src/main.rs (parse_whats_new,
normalize_doc_href). Exposes the legacy public API used by build.py and
test_whats_new.py.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional


def _ato_mcp_bin() -> str:
    env = os.environ.get("ATO_MCP_BIN")
    if env:
        return env
    on_path = shutil.which("ato-mcp")
    if on_path:
        return on_path
    repo_release = Path(__file__).resolve().parents[3] / "target" / "release" / "ato-mcp"
    if repo_release.is_file():
        return str(repo_release)
    raise RuntimeError(
        "ato-mcp binary not found: set ATO_MCP_BIN, put it on PATH, or "
        "build target/release/ato-mcp"
    )


def normalize_doc_href(href: str) -> str:
    """Canonicalise an ATO law/view/document href. Mirrors the Rust impl."""
    if not href:
        return ""
    proc = subprocess.run(
        [_ato_mcp_bin(), "normalize-doc-href", href],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"ato-mcp normalize-doc-href failed (exit {proc.returncode}): {proc.stderr.strip()}"
        )
    return proc.stdout.strip()


@dataclass
class WhatsNewEntry:
    href: str
    title: str
    heading: Optional[str]


class WhatsNewFetcher:
    """Tiny Python shim that invokes 'ato-mcp whats-new' for the actual work."""

    def __init__(
        self,
        whats_new_url: str = "https://www.ato.gov.au/law/view/whatsnew.htm?fid=whatsnew",
        *,
        base_url: str = "https://www.ato.gov.au",
        fetcher: Optional[Callable[[str], str]] = None,
    ) -> None:
        self.whats_new_url = whats_new_url
        self.base_url = base_url.rstrip("/")
        self.fetcher = fetcher  # accepted for API compat; unused (Rust does its own GET)

    def fetch_entries(self) -> List[WhatsNewEntry]:
        if self.fetcher is not None:
            # Caller injected a custom fetcher (used by tests with offline
            # HTML fixtures). Run it and pipe the HTML through the Rust
            # parser via a temp file.
            html = self.fetcher(self.whats_new_url)
            import tempfile

            with tempfile.NamedTemporaryFile(mode="w", suffix=".html", delete=False) as f:
                f.write(html)
                tmp_path = f.name
            try:
                # No CLI for "parse fixed HTML" yet — fall back to the live
                # path with the temp html served via file:// won't work
                # because reqwest blocks file://. Embed the parser via a
                # wrapper that reads stdin... not yet supported. For now,
                # tests using fetcher injection should run the Python
                # parsing locally below as a fallback.
                Path(tmp_path).unlink()
                return _parse_html_fallback(html, self.base_url)
            finally:
                pass
        proc = subprocess.run(
            [_ato_mcp_bin(), "whats-new", "--url", self.whats_new_url],
            capture_output=True,
            text=True,
            check=False,
        )
        if proc.returncode != 0:
            raise RuntimeError(
                f"ato-mcp whats-new failed (exit {proc.returncode}): {proc.stderr.strip()}"
            )
        raw = json.loads(proc.stdout)
        return [
            WhatsNewEntry(href=e["href"], title=e["title"], heading=e.get("heading"))
            for e in raw
        ]


def _parse_html_fallback(html: str, base_url: str) -> List[WhatsNewEntry]:
    """Pure-Python HTML parser used only when callers inject a custom
    fetcher (test fixtures). Production path uses the Rust binary."""
    from urllib.parse import urljoin

    from bs4 import BeautifulSoup

    soup = BeautifulSoup(html, "html.parser")
    article = soup.find("article")
    if article is None:
        raise ValueError("whatsnew article block not found")

    entries: List[WhatsNewEntry] = []
    seen: set[str] = set()
    for anchor in article.find_all("a"):
        raw_href = anchor.get("href")
        if not raw_href:
            continue
        absolute = urljoin(base_url + "/", raw_href)
        canonical = normalize_doc_href(absolute)
        if not canonical.startswith("/law/view/document"):
            continue
        if canonical in seen:
            continue
        seen.add(canonical)
        title = anchor.get_text(" ", strip=True) or canonical
        heading_node = anchor.find_previous(["h1", "h2", "h3", "h4", "h5"])
        heading = (
            heading_node.get_text(" ", strip=True) if heading_node is not None else None
        )
        if not heading:
            heading = None
        entries.append(WhatsNewEntry(href=canonical, title=title, heading=heading))
    return entries


class DedupedLinkIndex:
    """Pure-Python — small in-memory JSONL index, no Rust counterpart needed."""

    def __init__(self, links_path: Path) -> None:
        self.links_path = Path(links_path)
        self._by_canonical: Dict[str, Dict[str, Any]] = {}
        self._load()

    def _load(self) -> None:
        if not self.links_path.exists():
            raise FileNotFoundError(f"deduped links file not found: {self.links_path}")
        with self.links_path.open("r", encoding="utf-8") as fh:
            for line in fh:
                text = line.strip()
                if not text:
                    continue
                record = json.loads(text)
                canonical = normalize_doc_href(record.get("canonical_id", ""))
                if canonical:
                    self._by_canonical[canonical] = record

    def find(self, href: str) -> Optional[Dict[str, Any]]:
        return self._by_canonical.get(normalize_doc_href(href))

    def __len__(self) -> int:
        return len(self._by_canonical)


def build_pending_record(entry: WhatsNewEntry) -> Dict[str, Any]:
    from ..indexer.metadata import representative_path_from_docid

    segments = representative_path_from_docid(
        entry.href, title=entry.title, heading=entry.heading,
    )
    return {
        "canonical_id": entry.href,
        "href": entry.href,
        "title": entry.title,
        "representative_path": segments,
        "occurrences": 1,
        "folder_count": 1,
    }
