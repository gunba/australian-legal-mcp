from __future__ import annotations

import logging
from collections import deque
from dataclasses import dataclass, field
from typing import Any, Deque, Dict, Iterable, List, Optional, Set
from urllib.parse import parse_qs

from .progress import progress_bar, progress_write
from .constants import EXCLUDED_TITLES

LOGGER = logging.getLogger(__name__)


@dataclass
class SnapshotNode:
	uid: int
	parent_uid: Optional[int]
	title: str
	level: int
	node_type: str
	data_url: Optional[str]
	href: Optional[str]
	canonical_id: Optional[str]
	path: List[str] = field(default_factory=list)
	payload: Dict[str, Any] = field(default_factory=dict)

	def to_dict(self) -> Dict[str, Any]:
		return {
			"uid": self.uid,
			"parent_uid": self.parent_uid,
			"title": self.title,
			"level": self.level,
			"node_type": self.node_type,
			"data_url": self.data_url,
			"href": self.href,
			"canonical_id": self.canonical_id,
			"path": self.path,
			"payload": self.payload,
		}


class AtoTreeCrawler:
	"""Walks the browse-content tree and records every node."""

	def __init__(
		self,
		client,
		logger: Optional[logging.Logger] = None,
		skip_data_urls: Optional[Set[str]] = None,
		excluded_titles: Optional[Iterable[str]] = None,
	) -> None:
		self.client = client
		self.logger = logger or LOGGER
		self.skip_data_urls = skip_data_urls or set()
		self.excluded_titles_lookup = {title.strip().lower() for title in (excluded_titles or EXCLUDED_TITLES)}

	def crawl(
		self,
		root_query: str = "Mode=type&Action=initialise",
		max_nodes: Optional[int] = None,
	) -> List[SnapshotNode]:
		nodes: List[SnapshotNode] = []
		queue: Deque[Dict[str, Any]] = deque()
		visited_data_urls: set[str] = set()

		initial_payload = list(self.client.fetch_nodes(root_query))
		queue.extend(
			{
				"parent_uid": None,
				"path": [],
				"payload": payload,
				"level": 0,
			}
			for payload in initial_payload
		)

		uid_counter = 0
		progress = progress_bar(total=None, desc="ATO nodes", unit="node")

		while queue:
			item = queue.popleft()
			payload = item["payload"]
			uid_counter += 1
			node = self._build_node(
				uid=uid_counter,
				parent_uid=item["parent_uid"],
				payload=payload,
				level=item["level"],
				path=item["path"],
			)

			if self._is_excluded_title(node.title):
				progress_write(f"[crawl] skipping title '{node.title}'")
				if node.data_url:
					visited_data_urls.add(node.data_url)
				continue

			nodes.append(node)
			progress.update(1)
			if uid_counter % 1000 == 0:
				progress.set_postfix_str(f"crawl_frontier={len(queue)}")

			if max_nodes and len(nodes) >= max_nodes:
				self.logger.warning("Reached max_nodes=%s before finishing crawl", max_nodes)
				break

			child_url = self._child_query(node)
			if child_url is None:
				continue

			if child_url in self.skip_data_urls:
				progress_write(f"[crawl] skipping {child_url} (skip list)")
				visited_data_urls.add(child_url)
				continue

			if child_url in visited_data_urls:
				continue
			visited_data_urls.add(child_url)

			try:
				child_payloads = list(self.client.fetch_nodes(child_url))
			except Exception as exc:  # pragma: no cover - logged for live runs
				progress_write(f"[crawl] failed to fetch {child_url}: {exc}")
				continue

			queue.extend(
				{
					"parent_uid": node.uid,
					"path": node.path,
					"payload": child_payload,
					"level": node.level + 1,
				}
				for child_payload in child_payloads
			)

		progress.close()
		folder_count = sum(1 for n in nodes if "folder" in n.node_type)
		link_count = sum(1 for n in nodes if "link" in n.node_type)
		self.logger.info(
			"Captured %s nodes (folders=%s, links=%s)",
			len(nodes),
			folder_count,
			link_count,
		)
		return nodes

	def _build_node(
		self,
		uid: int,
		parent_uid: Optional[int],
		payload: Dict[str, Any],
		level: int,
		path: List[str],
	) -> SnapshotNode:
		title = payload.get("title") or "(untitled)"
		data_url = (payload.get("data") or {}).get("url")
		href = (payload.get("a_attr") or {}).get("href")
		if data_url and href:
			node_type = "folder+link"
		elif data_url:
			node_type = "folder"
		elif href:
			node_type = "link"
		else:
			node_type = "unknown"
		canonical_id = self._canonical_id(data_url, href)
		new_path = [*path, title]

		return SnapshotNode(
			uid=uid,
			parent_uid=parent_uid,
			title=title,
			level=level,
			node_type=node_type,
			data_url=data_url,
			href=href,
			canonical_id=canonical_id,
			path=new_path,
			payload=payload,
		)

	def _child_query(self, node: SnapshotNode) -> Optional[str]:
		if not node.data_url:
			return None
		return node.data_url

	def _canonical_id(self, data_url: Optional[str], href: Optional[str]) -> Optional[str]:
		if href:
			return href
		if not data_url:
			return None
		params = parse_qs(data_url)
		toc_values = params.get("TOC")
		if toc_values:
			return toc_values[0]
		return data_url

	def _is_excluded_title(self, title: str) -> bool:
		return title.strip().lower() in self.excluded_titles_lookup
