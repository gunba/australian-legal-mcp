from __future__ import annotations

import json
import logging
import re
import threading
import time
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from threading import Event, Lock, Thread, current_thread
from typing import Any, Callable, Dict, Iterable, List, Optional
from urllib.parse import urljoin, urlparse

import requests
from bs4 import BeautifulSoup

from .progress import progress_bar, progress_write
from .threadpool import thread_pool

LOGGER = logging.getLogger(__name__)


_SLUG_RE = re.compile(r"[^A-Za-z0-9]+")
DEFAULT_RUN_DATE = "2025-11-15T00:00:00Z"


@dataclass
class DownloadResult:
	canonical_id: str
	href: str
	status: str
	payload_path: Optional[str] = None
	assets: List[str] = field(default_factory=list)
	error: Optional[str] = None
	http_status: Optional[int] = None
	downloaded_at: str = ""


class _RunStats:
	def __init__(self, total: int, completed: int = 0) -> None:
		self.total = total
		self.completed = completed
		self.errors = 0
		self.skipped = 0
		self._lock = Lock()

	def increment_success(self) -> None:
		with self._lock:
			self.completed += 1

	def increment_error(self) -> None:
		with self._lock:
			self.errors += 1

	def increment_skipped(self) -> None:
		with self._lock:
			self.skipped += 1

	def snapshot(self) -> Dict[str, int]:
		with self._lock:
			return {
				"total": self.total,
				"completed": self.completed,
				"errors": self.errors,
				"skipped": self.skipped,
			}


class LinkDownloader:
	"""Downloads deduped ATO links and stores their lawContents HTML."""

	def __init__(
		self,
		deduped_links_path: Path | str,
		output_dir: Path | str,
		*,
		base_url: str = "https://www.ato.gov.au",
		parser_run_date: str = DEFAULT_RUN_DATE,
		fetcher: Optional[Callable[[str], tuple[int, str]]] = None,
		asset_fetcher: Optional[Callable[[str], bytes]] = None,
		request_delay: float = 0.0,
		retry_attempts: int = 3,
		verbose_progress: bool = False,
	) -> None:
		self.deduped_links_path = Path(deduped_links_path)
		self.output_dir = Path(output_dir)
		self.base_url = base_url.rstrip("/")
		self.parser_run_date = parser_run_date
		self.fetcher = fetcher or self._default_fetcher
		self.asset_fetcher = asset_fetcher or self._default_asset_fetcher
		self.payload_dir = self.output_dir / "payloads"
		self.index_path = self.output_dir / "index.jsonl"
		self.metadata_path = self.output_dir / "metadata.json"
		self.session = requests.Session()
		self.request_delay = request_delay
		self._rate_lock: Lock = Lock()
		self._last_request_started_at: float = 0.0
		self.verbose_progress = verbose_progress
		self._active_lock: Lock = Lock()
		self._active_by_thread: Dict[str, str] = {}
		self.retry_attempts = retry_attempts
		self._index_lock: Lock = Lock()
		self._metadata_lock: Lock = Lock()
		self._run_started_at: Optional[str] = None
		self._last_metadata_write: float = 0.0
		self._status_interval: float = 5.0
		self._status_stop_event: Optional[Event] = None
		self._status_thread: Optional[Thread] = None
		self._session_local = threading.local()
		self._asset_session_local = threading.local()

	def download_all(self, *, force: bool = False, max_workers: int = 2) -> None:
		self._prepare_output()
		links = list(self._iter_links())
		index = self._load_index()
		self._active_by_thread.clear()

		if self._is_live_http_fetcher():  # pragma: no cover - only runs in live environments
			self._log_outbound_ip()

		total_links = len(links)
		initial_completed = sum(1 for record in index.values() if record.get("status") == "success")
		if initial_completed:
			LOGGER.info("Resuming download with %s completed links already persisted", initial_completed)
		stats = _RunStats(total=total_links, completed=initial_completed)
		self._run_started_at = self._now()
		self._last_metadata_write = 0.0
		self._update_metadata_snapshot(total=total_links, completed=stats.completed, finished=False, force=True)

		progress = progress_bar(total=total_links, desc="Downloading links", unit="link")
		self._start_status_logger(stats)

		def process(link: Dict[str, any]) -> None:
			result = self._download_link(link, index.get(link["canonical_id"]), force=force)
			if result is None:
				stats.increment_skipped()
				progress.update(1)
				return
			self._record_result(result, index, stats)
			progress.update(1)

		try:
			with thread_pool(max_workers=max_workers) as executor:
				for link in links:
					executor.submit(process, link)
		finally:
			self._stop_status_logger()

		progress.close()
		self._log_status_snapshot(stats)

		self._write_index(index)
		self._update_metadata_snapshot(total=total_links, completed=stats.completed, finished=True, force=True)

	def _prepare_output(self) -> None:
		self.output_dir.mkdir(parents=True, exist_ok=True)
		self.payload_dir.mkdir(parents=True, exist_ok=True)

	def _iter_links(self) -> Iterable[Dict[str, any]]:
		with self.deduped_links_path.open("r", encoding="utf-8") as fh:
			for line in fh:
				text = line.strip()
				if not text:
					continue
				yield json.loads(text)

	def _thread_session(self, store: threading.local) -> requests.Session:
		session = getattr(store, "session", None)
		if session is None:
			session = requests.Session()
			setattr(store, "session", session)
		return session

	def _default_fetcher(self, href: str) -> tuple[int, str]:  # pragma: no cover
		url = href
		if href.startswith("/"):
			url = f"{self.base_url}{href}"

		self._acquire_request_slot()
		session = self._thread_session(self._session_local)
		response = session.get(url, timeout=30)
		response.raise_for_status()
		return response.status_code, response.text

	def _download_link(self, link: Dict[str, any], existing: Optional[Dict[str, any]], *, force: bool) -> Optional[DownloadResult]:
		canonical_id = link["canonical_id"]
		if not self._should_download(link, existing, force):
			# Orphan payload: file on disk but no success row in the index. Emit a
			# synthetic success so the builder sees the doc, without refetching.
			if not force and (existing is None or existing.get("status") != "success") and self._payload_exists(link):
				payload_path = self._build_payload_path(link, ensure_dirs=False)
				return DownloadResult(
					canonical_id=canonical_id,
					href=link.get("href"),
					status="success",
					payload_path=str(payload_path.relative_to(self.output_dir)),
					assets=[],
					error=None,
					http_status=None,
					downloaded_at=self._now(),
				)
			return None

		href = link.get("href")
		thread_name = current_thread().name
		self._set_active(thread_name, canonical_id)
		try:
			http_status, html = self.fetcher(href)
		except Exception as exc:  # pragma: no cover - network failure path logged for user runs
			progress_write(f"[download] failed {canonical_id}: {exc}")
			self._clear_active(thread_name)
			return DownloadResult(
				canonical_id=canonical_id,
				href=href,
				status="failed",
				payload_path=None,
				error=str(exc),
				http_status=None,
				downloaded_at=self._now(),
			)

		result = self._build_result(link, href, html, http_status)
		self._clear_active(thread_name)
		return result

	def _record_result(self, result: DownloadResult, index: Dict[str, Dict[str, Any]], stats: _RunStats) -> None:
		record = asdict(result)
		with self._index_lock:
			index[result.canonical_id] = record
			self._append_index_record(record)
		if result.status == "success":
			stats.increment_success()
		else:
			stats.increment_error()
		self._update_metadata_snapshot(total=stats.total, completed=stats.completed)

	def _should_download(self, link: Dict[str, any], existing: Optional[Dict[str, any]], force: bool) -> bool:
		if force:
			return True
		if existing and existing.get("status") == "success":
			return False
		if self._payload_exists(link):
			return False
		return True

	def _build_result(self, link: Dict[str, any], href: str, html: str, http_status: int) -> DownloadResult:
		canonical_id = link["canonical_id"]
		snippet, assets = self._extract_law_contents(html, link)
		if snippet is None:
			progress_write(f"[download] missing lawContents for {canonical_id}")
			return DownloadResult(
				canonical_id=canonical_id,
				href=href,
				status="missing_content",
				payload_path=None,
				assets=[],
				error="lawContents div not found",
				http_status=http_status,
				downloaded_at=self._now(),
			)

		payload_path = self._write_payload(link, snippet)
		return DownloadResult(
			canonical_id=canonical_id,
			href=href,
			status="success",
			payload_path=str(payload_path.relative_to(self.output_dir)),
			assets=[str(Path(asset).as_posix()) for asset in assets],
			error=None,
			http_status=http_status,
			downloaded_at=self._now(),
		)

	def _extract_law_contents(self, html: str, link: Dict[str, any]) -> tuple[Optional[str], List[str]]:
		soup = BeautifulSoup(html, "html.parser")
		container = self._extract_article_container(soup)
		if container is None:
			return None, []

		rep_path = link.get("representative_path") or []
		payload_dir = self._ensure_payload_dir(rep_path)
		assets = self._download_assets(container, payload_dir, link)

		return container.decode(), assets

	def _extract_article_container(self, soup: BeautifulSoup) -> Optional[BeautifulSoup]:
		article = soup.find("article")
		if article is None:
			return None

		clone = BeautifulSoup("", "html.parser")
		container = clone.new_tag("div", id="lawContents")
		for child in list(article.contents):
			container.append(child)
		return container

	def _ensure_payload_dir(self, rep_path: List[str]) -> Path:
		dir_path = self.payload_dir
		for segment in rep_path:
			dir_path = dir_path / self._slug(segment)
		dir_path.mkdir(parents=True, exist_ok=True)
		return dir_path

	def _write_payload(self, link: Dict[str, any], snippet: str) -> Path:
		payload_path = self._build_payload_path(link, ensure_dirs=True)
		payload_path.write_text(snippet, encoding="utf-8")
		return payload_path

	def _build_payload_path(self, link: Dict[str, any], ensure_dirs: bool = False) -> Path:
		rep_path = link.get("representative_path") or []
		dir_path = self.payload_dir
		for segment in rep_path:
			dir_path = dir_path / self._slug(segment)
		if ensure_dirs:
			dir_path.mkdir(parents=True, exist_ok=True)
		filename = f"{self._slug(link['canonical_id'], fallback='link')}.html"
		return dir_path / filename

	def _payload_exists(self, link: Dict[str, any]) -> bool:
		return self._build_payload_path(link, ensure_dirs=False).exists()

	def _download_assets(self, container: BeautifulSoup, payload_dir: Path, link: Dict[str, any]) -> List[str]:
		assets: List[str] = []
		asset_dir = payload_dir / "assets"
		for idx, img in enumerate(container.find_all("img")):
			src = img.get("src")
			if not src:
				continue
			asset_url = self._resolve_asset_url(src)
			if asset_url is None:
				continue
			try:
				data = self.asset_fetcher(asset_url)
			except Exception as exc:  # pragma: no cover - network failure
				progress_write(f"[download] failed image {asset_url}: {exc}")
				continue
			asset_dir.mkdir(parents=True, exist_ok=True)
			ext = Path(urlparse(asset_url).path).suffix or ".bin"
			filename = f"{self._slug(link['canonical_id'], fallback='asset')}_{idx}{ext}"
			asset_path = asset_dir / filename
			asset_path.write_bytes(data)
			img["src"] = f"assets/{filename}"
			assets.append(str(asset_path.relative_to(self.output_dir)))
		return assets

	def _resolve_asset_url(self, src: str) -> Optional[str]:
		src = src.strip()
		if not src:
			return None
		if src.startswith("http://") or src.startswith("https://"):
			return src
		return urljoin(f"{self.base_url}/", src)

	def _load_index(self) -> Dict[str, Dict[str, any]]:
		if not self.index_path.exists():
			return {}
		entries: Dict[str, Dict[str, any]] = {}
		with self.index_path.open("r", encoding="utf-8") as fh:
			for line in fh:
				text = line.strip()
				if not text:
					continue
				record = json.loads(text)
				entries[record["canonical_id"]] = record
		return entries

	def _append_index_record(self, record: Dict[str, Any]) -> None:
		with self.index_path.open("a", encoding="utf-8") as fh:
			json.dump(record, fh, ensure_ascii=False)
			fh.write("\n")

	def _write_index(self, index: Dict[str, Dict[str, any]]) -> None:
		tmp_path = self.index_path.with_name(f"{self.index_path.name}.tmp")
		with tmp_path.open("w", encoding="utf-8") as fh:
			for canonical_id in sorted(index.keys()):
				json.dump(index[canonical_id], fh, ensure_ascii=False)
				fh.write("\n")
		tmp_path.replace(self.index_path)

	def _update_metadata_snapshot(self, *, total: int, completed: int, finished: bool = False, force: bool = False) -> None:
		if self._run_started_at is None:
			self._run_started_at = self._now()
		now = time.monotonic()
		if not force and now - self._last_metadata_write < 1.0:
			return
		self._last_metadata_write = now
		with self._metadata_lock:
			run_meta = {
				"links_file": str(self.deduped_links_path),
				"parser_run_date": self.parser_run_date,
				"download_started_at": self._run_started_at,
				"download_completed_at": self._now() if finished else None,
				"total_links": total,
				"completed_links": completed,
			}
			payload = json.dumps(run_meta, indent=2)
			tmp_path = self.metadata_path.with_name(f"{self.metadata_path.name}.tmp")
			try:
				tmp_path.write_text(payload, encoding="utf-8")
				tmp_path.replace(self.metadata_path)
			except PermissionError:
				# Some environments mount outputs with read-only directories but writable files.
				# Fall back to writing metadata in-place so the downloader can continue.
				self.metadata_path.write_text(payload, encoding="utf-8")

	def _slug(self, text: str, fallback: str = "node") -> str:
		sanitized = _SLUG_RE.sub("_", text.strip())
		sanitized = sanitized.strip("_")
		if not sanitized:
			sanitized = fallback
		return sanitized[:80]

	def _now(self) -> str:
		return datetime.now(timezone.utc).isoformat()

	def _default_asset_fetcher(self, url: str) -> bytes:  # pragma: no cover - production path
		session = self._thread_session(self._asset_session_local)
		self._acquire_request_slot()
		response = session.get(url, timeout=30)
		response.raise_for_status()
		return response.content

	def _acquire_request_slot(self) -> None:
		"""Globally pace all HTTP requests across threads.

		When ``request_delay`` is > 0, we ensure that each new request
		starts at least ``request_delay`` seconds after the previous one
		(regardless of which thread is issuing it).
		"""
		if not self.request_delay:
			return
		with self._rate_lock:
			now = time.monotonic()
			earliest = self._last_request_started_at + self.request_delay
			if earliest > now:
				time.sleep(earliest - now)
				now = earliest
			self._last_request_started_at = now

	def _is_live_http_fetcher(self) -> bool:
		"""Return True when using the built-in HTTP fetcher (not a stub)."""
		# Compare underlying functions so bound-method identity doesn't matter.
		fetcher_func = getattr(self.fetcher, "__func__", self.fetcher)
		default_func = getattr(self._default_fetcher, "__func__", self._default_fetcher)
		return fetcher_func is default_func

	def _log_outbound_ip(self) -> None:  # pragma: no cover - network check only for live runs
		try:
			resp = self.session.get("https://api.ipify.org?format=text", timeout=10)
			resp.raise_for_status()
			ip = (resp.text or "").strip()
			if ip:
				LOGGER.info("Detected outbound IP for ATO downloader: %s", ip)
		except Exception as exc:
			LOGGER.info("Could not determine outbound IP for ATO downloader: %s", exc)

	def _start_status_logger(self, stats: _RunStats) -> None:
		if not self.verbose_progress or self._status_thread is not None:
			return
		self._status_stop_event = Event()
		self._status_thread = Thread(target=self._status_loop, args=(stats,), daemon=True, name="downloader-status")
		self._status_thread.start()

	def _status_loop(self, stats: _RunStats) -> None:
		stop_event = self._status_stop_event
		if stop_event is None:
			return
		while not stop_event.wait(self._status_interval):
			self._log_status_snapshot(stats)

	def _stop_status_logger(self) -> None:
		if not self.verbose_progress:
			return
		if self._status_stop_event is None:
			return
		self._status_stop_event.set()
		if self._status_thread is not None:
			self._status_thread.join(timeout=1.0)
		self._status_stop_event = None
		self._status_thread = None

	def _log_status_snapshot(self, stats: _RunStats) -> None:
		if not self.verbose_progress:
			return
		snapshot = stats.snapshot()
		with self._active_lock:
			active = dict(self._active_by_thread)
		lines = [
			f"[status] {snapshot['completed']}/{snapshot['total']} links (errors={snapshot['errors']}, skipped={snapshot['skipped']})"
		]
		for name, cid in sorted(active.items()):
			lines.append(f"  {name}: {cid}")
		progress_write("\n".join(lines))

	def _set_active(self, thread_name: str, canonical_id: str) -> None:
		if not self.verbose_progress:
			return
		with self._active_lock:
			self._active_by_thread[thread_name] = canonical_id

	def _clear_active(self, thread_name: str) -> None:
		if not self.verbose_progress:
			return
		with self._active_lock:
			self._active_by_thread.pop(thread_name, None)
