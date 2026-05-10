from __future__ import annotations

from typing import Iterable, Optional


EXCLUDED_TITLES = {
	"Archived document types",
	"Amending legislation",
	"Amending regulations",
	"Archived",
	"Full document",
	"View list of provisions",
	"Draft",
	"Draft amendments",
}


def _normalise_title(value: str) -> str:
	return " ".join(value.split()).strip().lower()


_EXCLUDED_TITLES_NORMALISED = frozenset(_normalise_title(title) for title in EXCLUDED_TITLES)


def is_excluded_title(value: Optional[str], lookup: Optional[Iterable[str]] = None) -> bool:
	"""True if the (whitespace-normalised, lowercased) title appears in EXCLUDED_TITLES."""
	if not value:
		return False
	normalised = _normalise_title(value)
	if lookup is None:
		return normalised in _EXCLUDED_TITLES_NORMALISED
	return normalised in lookup


def build_excluded_titles_lookup(excluded_titles: Optional[Iterable[str]]) -> frozenset[str]:
	if excluded_titles is None:
		return _EXCLUDED_TITLES_NORMALISED
	return frozenset(_normalise_title(title) for title in excluded_titles)
