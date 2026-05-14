"""Utility helpers for managing progress reporting in ingestion pipelines."""
import os
import sys
from typing import Any, Iterable, Iterator, Optional, TypeVar

T = TypeVar('T')

try:
	from tqdm import tqdm as _tqdm
except ImportError:
	_tqdm = None

_DISABLE_TOKENS = {"0", "false", "off", "no", "disable", "disabled"}
_PROGRESS_ENABLED: Optional[bool] = None


def _coerce_bool(value: str) -> bool:
	if value is None:
		return True
	normalized = value.strip().lower()
	if not normalized:
		return True
	return normalized not in _DISABLE_TOKENS


def set_progress_enabled(enabled: bool) -> None:
	"""Override the progress flag (primarily for testing)."""
	global _PROGRESS_ENABLED
	_PROGRESS_ENABLED = bool(enabled)


def progress_enabled() -> bool:
	"""Return whether progress output should be emitted."""
	global _PROGRESS_ENABLED
	if _PROGRESS_ENABLED is None:
		_PROGRESS_ENABLED = _coerce_bool(os.getenv("INGEST_PROGRESS", "1"))
	return _PROGRESS_ENABLED


class _DummyProgress(Iterator[T]):
	"""Fallback iterator used when tqdm is unavailable."""

	def __init__(self, iterable: Optional[Iterable[T]] = None, **_: Any) -> None:
		self._iterable = iterable
		self._iterator: Optional[Iterator[T]] = None

	def __iter__(self) -> Iterator[T]:
		if self._iterator is None:
			if self._iterable is None:
				self._iterator = iter(())
			else:
				self._iterator = iter(self._iterable)
		return self._iterator

	def __next__(self) -> T:
		return next(iter(self))

	def update(self, *_: Any, **__: Any) -> None:
		return None

	def close(self) -> None:
		return None

	def set_description(self, *_: Any, **__: Any) -> None:
		return None

	def set_postfix_str(self, *_: Any, **__: Any) -> None:
		return None


def progress_bar(iterable: Optional[Iterable[T]] = None, **kwargs: Any):
	"""Return a configured progress bar respecting the global toggle."""
	if _tqdm is not None:
		disable = kwargs.pop("disable", None)
		if disable is None:
			disable = not progress_enabled()
		return _tqdm(iterable, disable=disable, **kwargs)
	return _DummyProgress(iterable, **kwargs)


def progress_write(message: str, *, file: Any = None, end: str = "\n", nolock: bool = False) -> None:
	"""Thread-safe printing compatible with tqdm.write."""
	if _tqdm is not None:
		_tqdm.write(message, file=file, end=end, nolock=nolock)
	else:
		stream = file if file is not None else sys.stdout
		print(message, file=stream, end=end)


__all__ = [
	"progress_bar",
	"progress_enabled",
	"progress_write",
	"set_progress_enabled",
]
