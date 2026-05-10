from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional
from urllib.parse import parse_qs, unquote, urljoin, urlparse

import requests
from bs4 import BeautifulSoup


def normalize_doc_href(href: str) -> str:
	"""
	Normalize a law/view/document href to the relative canonical form used by deduped_links.jsonl.
	Examples:
		https://www.ato.gov.au/law/view/document?docid=ABC -> /law/view/document?docid=ABC
		/law/view/document?docid=ABC -> /law/view/document?docid=ABC
		/law/view/document?docid=ABC&PiT=20120418000001
		    -> /law/view/document?docid=ABC@20120418000001

	The ``@<PiT>`` suffix on the docid value matches the historical-version
	encoding used by ``anchors.py`` and parsed by
	``build._parse_historical_doc_id``. Downstream ``metadata.doc_id_for``
	extracts the docid value verbatim, yielding the ``<base>@<PiT>`` doc_id.
	"""
	if not href:
		return ""
	parsed = urlparse(href)
	path = parsed.path or ""
	if path and not path.startswith("/"):
		path = f"/{path}"

	query_params = parse_qs(parsed.query)
	docid_values = query_params.get("docid")
	docid = docid_values[0] if docid_values else ""
	docid = unquote(docid).strip("'\" ")
	pit = ""
	for key, values in query_params.items():
		if key.lower() == "pit" and values:
			candidate = unquote(values[0]).strip("'\" ")
			if candidate:
				pit = candidate
				break
	if docid:
		if pit:
			return f"/law/view/document?docid={docid}@{pit}"
		return f"/law/view/document?docid={docid}"

	query = f"?{parsed.query}" if parsed.query else ""
	return f"{path}{query}"


@dataclass
class WhatsNewEntry:
	href: str
	title: str
	heading: Optional[str]


class WhatsNewFetcher:
	def __init__(
		self,
		whats_new_url: str = "https://www.ato.gov.au/law/view/whatsnew.htm?fid=whatsnew",
		*,
		base_url: str = "https://www.ato.gov.au",
		fetcher: Optional[Callable[[str], str]] = None,
	) -> None:
		self.whats_new_url = whats_new_url
		self.base_url = base_url.rstrip("/")
		self.fetcher = fetcher or self._default_fetcher

	def fetch_entries(self) -> List[WhatsNewEntry]:
		html = self.fetcher(self.whats_new_url)
		return self._parse_html(html)

	def _default_fetcher(self, url: str) -> str:  # pragma: no cover - exercised in live runs
		response = requests.get(url, timeout=30)
		response.raise_for_status()
		return response.text

	def _parse_html(self, html: str) -> List[WhatsNewEntry]:
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
			absolute = urljoin(self.base_url + "/", raw_href)
			canonical = normalize_doc_href(absolute)
			if not canonical.startswith("/law/view/document"):
				continue
			if canonical in seen:
				continue
			seen.add(canonical)
			title = anchor.get_text(" ", strip=True) or canonical
			heading = self._find_heading(anchor)
			entries.append(WhatsNewEntry(href=canonical, title=title, heading=heading))
		return entries

	def _find_heading(self, anchor) -> Optional[str]:
		heading = anchor.find_previous(["h1", "h2", "h3", "h4", "h5"])
		if heading is None:
			return None
		text = heading.get_text(" ", strip=True) if hasattr(heading, "get_text") else ""
		return text or None


class DedupedLinkIndex:
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
	"""Produce a deduped-link record for a new What's New entry.

	The representative_path is derived from the docid prefix via
	``ato_mcp.indexer.metadata.representative_path_from_docid`` so the file
	lands under the correct ``payloads/<category>/`` bucket. The legacy
	``payloads/whats_new`` bucket is intentionally not used.
	"""
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
