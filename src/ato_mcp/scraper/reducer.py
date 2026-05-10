from __future__ import annotations

import json
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Set

from .progress import progress_bar
from .constants import build_excluded_titles_lookup, is_excluded_title


@dataclass
class CanonicalEntry:
	canonical_id: str
	title: Optional[str] = None
	href: Optional[str] = None
	representative_path: List[str] = field(default_factory=list)
	occurrences: int = 0
	folder_occurrences: set[str] = field(default_factory=set)
	owner_folder: Optional[str] = None


@dataclass
class FolderRecord:
	data_url: str
	title: Optional[str]
	path: List[str]
	parent_data_url: Optional[str]
	canonical_ids: set[str] = field(default_factory=set)
	owned_ids: set[str] = field(default_factory=set)
	redundant: bool = False


class SnapshotReducer:
	"""Processes a snapshot to deduplicate links and flag redundant paths."""
	# [SS-07] Dedupes canonical_ids across folders, picks a representative_path per id, flags redundant folder paths; titles in EXCLUDED_TITLES (and their descendants) filtered out before reduction.

	def __init__(self, nodes_path: Path | str, excluded_titles: Optional[Iterable[str]] = None) -> None:
		self.nodes_path = Path(nodes_path)
		self.excluded_titles_lookup = build_excluded_titles_lookup(excluded_titles)
		self.excluded_counts: Dict[str, int] = defaultdict(int)
		self.excluded_folder_urls: Set[str] = set()

	def run(self, output_dir: Optional[Path | str] = None) -> Dict[str, Path]:
		result = self._process()
		out_dir = Path(output_dir) if output_dir else self.nodes_path.parent
		out_dir.mkdir(parents=True, exist_ok=True)

		dedup_path = self._write_dedup(out_dir, result["canonical_entries"])
		redundant_path, skip_path = self._write_redundant(out_dir, result["folder_records"])

		return {
			"deduped_links": dedup_path,
			"summary": out_dir / "dedup_summary.json",
			"redundant_paths": redundant_path,
			"skip_file": skip_path,
		}

	def _process(self) -> Dict[str, Any]:
		node_meta: Dict[int, Dict[str, Optional[str]]] = {}
		folder_records: Dict[str, FolderRecord] = {}
		folder_children: Dict[Optional[str], set[str]] = defaultdict(set)
		canonical_entries: Dict[str, CanonicalEntry] = {}

		excluded_uids: Set[int] = set()

		for record in self._iter_nodes():
			uid = record["uid"]
			parent_uid = record.get("parent_uid")
			data_url = record.get("data_url")
			node_meta[uid] = {"parent": parent_uid, "data_url": data_url}

			title = (record.get("title") or "").strip()
			is_excluded = is_excluded_title(title, self.excluded_titles_lookup)
			parent_excluded = parent_uid in excluded_uids if parent_uid is not None else False
			if is_excluded or parent_excluded:
				excluded_uids.add(uid)
				self.excluded_counts[title or "(untitled)"] += 1
				if data_url:
					self.excluded_folder_urls.add(data_url)
				continue

			if data_url:
				parent_folder = self._find_parent_folder(parent_uid, node_meta)
				folder_record = folder_records.setdefault(
					data_url,
					FolderRecord(
						data_url=data_url,
						title=record.get("title"),
						path=record.get("path") or [],
						parent_data_url=parent_folder,
					),
				)
				folder_record.parent_data_url = parent_folder
				folder_children[parent_folder].add(data_url)

			if "link" in (record.get("node_type") or "") and record.get("canonical_id"):
				folder_url = record.get("data_url") or self._find_parent_folder(parent_uid, node_meta)
				if not folder_url:
					continue
				folder_records.setdefault(
					folder_url,
					FolderRecord(data_url=folder_url, title=None, path=[], parent_data_url=None),
				)

				entry = canonical_entries.setdefault(
					record["canonical_id"],
					CanonicalEntry(canonical_id=record["canonical_id"]),
				)
				self._update_canonical_entry(entry, record, folder_url)
				folder_records[folder_url].canonical_ids.add(entry.canonical_id)

		self._assign_folder_ownership(canonical_entries, folder_records)
		self._mark_redundant_folders(folder_records, folder_children)

		return {
			"canonical_entries": canonical_entries,
			"folder_records": folder_records,
			"excluded_counts": dict(self.excluded_counts),
		}

	def _iter_nodes(self) -> Iterable[Dict[str, Any]]:
		with self.nodes_path.open("r", encoding="utf-8") as fh:
			for line in progress_bar(fh, desc="Reducing snapshot", unit="node"):
				text = line.strip()
				if not text:
					continue
				yield json.loads(text)

	def _find_parent_folder(self, uid: Optional[int], node_meta: Dict[int, Dict[str, Optional[str]]]) -> Optional[str]:
		while uid is not None:
			meta = node_meta.get(uid)
			if not meta:
				return None
			data_url = meta.get("data_url")
			if data_url:
				return data_url
			uid = meta.get("parent")
		return None

	def _update_canonical_entry(self, entry: CanonicalEntry, record: Dict[str, Any], folder_url: str) -> None:
		entry.occurrences += 1
		entry.folder_occurrences.add(folder_url)
		entry.href = entry.href or record.get("href")
		entry.title = entry.title or record.get("title")
		path = record.get("path") or []
		if not entry.representative_path or self._is_better_path(path, entry.representative_path):
			entry.representative_path = path
			entry.title = record.get("title")
			entry.owner_folder = folder_url

	def _is_better_path(self, candidate: List[str], incumbent: List[str]) -> bool:
		if not incumbent:
			return True
		return (len(candidate), candidate) < (len(incumbent), incumbent)

	def _assign_folder_ownership(self, canonical_entries: Dict[str, CanonicalEntry], folder_records: Dict[str, FolderRecord]) -> None:
		for entry in canonical_entries.values():
			if entry.owner_folder and entry.owner_folder in folder_records:
				folder_records[entry.owner_folder].owned_ids.add(entry.canonical_id)

	def _mark_redundant_folders(self, folder_records: Dict[str, FolderRecord], folder_children: Dict[Optional[str], set[str]]) -> None:
		def dfs(folder_url: str) -> bool:
			record = folder_records.get(folder_url)
			has_owned = bool(record and record.owned_ids)
			for child in folder_children.get(folder_url, []):
				if dfs(child):
					has_owned = True
			if record:
				record.redundant = not has_owned
			return has_owned

		for root in folder_children.get(None, []):
			dfs(root)

	def _write_dedup(self, output_dir: Path, canonical_entries: Dict[str, CanonicalEntry]) -> Path:
		dedup_path = output_dir / "deduped_links.jsonl"
		with dedup_path.open("w", encoding="utf-8") as fh:
			for canonical_id in sorted(canonical_entries.keys()):
				entry = canonical_entries[canonical_id]
				json.dump(
					{
						"canonical_id": entry.canonical_id,
						"href": entry.href,
						"title": entry.title,
						"representative_path": entry.representative_path,
						"occurrences": entry.occurrences,
						"folder_count": len(entry.folder_occurrences),
					},
					fh,
					ensure_ascii=False,
				)
				fh.write("\n")

		summary = {
			"unique_links": len(canonical_entries),
			"total_occurrences": sum(entry.occurrences for entry in canonical_entries.values()),
			"excluded_titles": self.excluded_counts,
			"excluded_folder_urls": sorted(self.excluded_folder_urls),
		}
		(output_dir / "dedup_summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
		return dedup_path

	def _write_redundant(self, output_dir: Path, folder_records: Dict[str, FolderRecord]) -> tuple[Path, Path]:
		redundant = [rec for rec in folder_records.values() if rec.redundant]
		redundant.sort(key=lambda rec: (len(rec.path), rec.data_url))
		payload = [
			{
				"data_url": rec.data_url,
				"title": rec.title,
				"path": rec.path,
				"parent_data_url": rec.parent_data_url,
				"canonical_id_count": len(rec.canonical_ids),
				"owned_canonical_ids": len(rec.owned_ids),
			}
			for rec in redundant
		]

		redundant_path = output_dir / "redundant_paths.json"
		redundant_path.write_text(json.dumps(payload, indent=2), encoding="utf-8")

		skip_path = output_dir / "skip_data_urls.json"
		all_skip_urls = {rec["data_url"] for rec in payload} | self.excluded_folder_urls
		skip_path.write_text(json.dumps(sorted(all_skip_urls), indent=2), encoding="utf-8")

		return redundant_path, skip_path
