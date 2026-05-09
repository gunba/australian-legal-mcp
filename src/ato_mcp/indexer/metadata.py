"""Metadata extraction from ATO canonical IDs + payload HTML.

The canonical_id for every ATO document is a URL fragment of the form
``/law/view/document?docid=<PREFIX>/<CODE>/.../<VERSION>`` where PREFIX is one
of ~40 known document-type codes (TR, GSTR, ATOID, PCG, TA, LCR, PS LA, ...).

We use the prefix as the primary doc_type signal; ``doc_id`` in the v4 schema
is the entire docid path verbatim (prefix included), which is unique per
document. A short human citation like ``"TR 2024/3"`` lives in ``human_code``
and is populated by the main-PC corpus parser rather than derived from the
URL here.
"""
from __future__ import annotations

import hashlib
import re
from importlib import resources
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, unquote, urlparse

import yaml

_DATE_RE = re.compile(
    r"\b(\d{1,2})\s+(January|February|March|April|May|June|July|August|September|October|November|December)\s+(\d{4})\b",
    re.IGNORECASE,
)

_DOC_TYPE_MAP: dict[str, dict[str, str]] | None = None
_WARNED_UNMAPPED_PREFIXES: set[str] = set()


def _warn_unmapped_prefix(prefix: str) -> None:
    """Warn once per unknown docid prefix so the maintainer can update the map."""
    if prefix in _WARNED_UNMAPPED_PREFIXES:
        return
    _WARNED_UNMAPPED_PREFIXES.add(prefix)
    # Imported here to avoid a module-load dependency for scripts that only
    # use the pure parsing helpers.
    from ..util.log import get_logger
    get_logger(__name__).warning(
        "unmapped docid prefix %r — add it to data/doc_type_map.yaml", prefix
    )


def _load_doc_type_map() -> dict[str, dict[str, str]]:
    global _DOC_TYPE_MAP
    if _DOC_TYPE_MAP is not None:
        return _DOC_TYPE_MAP
    # Try package-resource first; fall back to repo-root data/ during dev.
    data_text: str | None = None
    try:
        files = resources.files("ato_mcp").joinpath("_data/doc_type_map.yaml")
        if files.is_file():
            data_text = files.read_text(encoding="utf-8")
    except (FileNotFoundError, ModuleNotFoundError, AttributeError):
        pass
    if data_text is None:
        repo_root = Path(__file__).resolve().parents[3]
        candidate = repo_root / "data" / "doc_type_map.yaml"
        if candidate.exists():
            data_text = candidate.read_text(encoding="utf-8")
    if data_text is None:
        _DOC_TYPE_MAP = {}
        return _DOC_TYPE_MAP
    loaded = yaml.safe_load(data_text) or {}
    _DOC_TYPE_MAP = {str(k).upper(): v for k, v in loaded.items()}
    return _DOC_TYPE_MAP


def _extract_docid_path(canonical_id: str) -> str | None:
    """Pull the ``docid=<path>`` query value out of an ATO canonical URL.

    The returned string is the verbatim docid path (prefix included), e.g.
    ``"TXR/TR20133/NAT/ATO/00001"``. This is the ``doc_id`` primary key in
    the v4 schema — no slugification, case preserved.
    """
    parsed = urlparse(canonical_id)
    docid_values = parse_qs(parsed.query).get("docid")
    if not docid_values:
        return None
    return unquote(docid_values[0]) or None


def doc_id_for(canonical_id: str) -> str:
    """Return the primary-key ``doc_id`` for this URL.

    ``doc_id`` is the ATO's docid path verbatim — prefix, case, and slashes
    preserved — because the path form is both unique and human-inspectable.
    Falls back to the raw URL if the ``docid=`` query parameter is missing,
    so we always have *some* unique key even for malformed inputs.
    """
    # [IB-18] doc_id is the verbatim docid= path (prefix, case, slashes preserved); raw-URL fallback ensures every record has a unique key even when the canonical URL is malformed.
    return _extract_docid_path(canonical_id) or canonical_id


def category_from_path(payload_path: str | None) -> str:
    if not payload_path:
        return "Unknown"
    parts = Path(payload_path).parts
    if parts and parts[0].lower() in ("payloads",):
        parts = parts[1:]
    return parts[0] if parts else "Unknown"


def category_for_record(canonical_id: str, payload_path: str | None) -> str:
    category = category_from_path(payload_path)
    if category in {"Unknown", "whats_new"}:
        return category_for_docid(canonical_id)
    return category


def parse_docid(canonical_id: str) -> tuple[str | None, str | None]:
    """Return ``(prefix, doc_type_name)``. Prefix is uppercased first segment
    of the docid, e.g. ``TR`` from ``TR/TR20243/NAT/ATO/00001``.
    """
    docid = _extract_docid_path(canonical_id)
    if not docid:
        return None, None
    segments = [s for s in docid.split("/") if s]
    if not segments:
        return None, None
    prefix = segments[0].upper()
    name = _load_doc_type_map().get(prefix, {}).get("name")
    if name is None:
        _warn_unmapped_prefix(prefix)
    return prefix, name


def category_for_docid(canonical_id: str) -> str:
    """Return the ``ato_pages/payloads/<category>/`` bucket for this docid.

    Driven by ``data/doc_type_map.yaml``'s ``category`` hint. Falls back to
    ``Other_ATO_documents`` for unknown prefixes so new docs still land in a
    valid bucket rather than ``whats_new``.
    """
    prefix, _ = parse_docid(canonical_id)
    if prefix:
        entry = _load_doc_type_map().get(prefix)
        if entry and entry.get("category"):
            return entry["category"]
    return "Other_ATO_documents"


_YEAR_RE = re.compile(r"(?:19|20)\d{2}")


def year_for_docid(canonical_id: str) -> str | None:
    """Best-effort year extraction from the docid body. E.g. ``CR202612`` → ``2026``."""
    docid = _extract_docid_path(canonical_id)
    if not docid:
        return None
    segments = [s for s in docid.split("/") if s]
    for seg in segments[:2]:
        m = _YEAR_RE.search(seg)
        if m:
            return m.group(0)
    return None


def representative_path_from_docid(
    canonical_id: str,
    *,
    title: str | None = None,
    heading: str | None = None,
) -> list[str]:
    """Derive a ``representative_path`` for the downloader using the docid alone.

    Shape: ``[category, doc_type_name, year, title]``. Segments that can't be
    resolved are omitted. This is used for What's New entries where we don't
    have the tree-crawl-derived path — the first segment (category) is what
    the indexer uses, so even partial classification is correct downstream.
    """
    category = category_for_docid(canonical_id)
    prefix, doc_type_name = parse_docid(canonical_id)
    year = year_for_docid(canonical_id)
    segments = [category]
    if doc_type_name:
        segments.append(doc_type_name)
    elif heading:
        segments.append(heading)
    if year:
        segments.append(year)
    segments.append(title or canonical_id)
    return segments


def extract_pub_date(text: str) -> str | None:
    """Best-effort publication-date scrape. Returns ISO yyyy-mm-dd or None."""
    match = _DATE_RE.search(text[:2000])
    if not match:
        return None
    day, month_name, year = match.groups()
    month = {
        "january": 1, "february": 2, "march": 3, "april": 4, "may": 5, "june": 6,
        "july": 7, "august": 8, "september": 9, "october": 10, "november": 11, "december": 12,
    }[month_name.lower()]
    return f"{int(year):04d}-{month:02d}-{int(day):02d}"


# Series codes that use the <SERIES><YEAR><NUMBER> format with an optional
# 'D' draft marker before the number. Listed longest-first because Python's
# regex alternation is left-to-right, not longest-match — SMSFRB must beat
# SMSFR, GSTR must beat GST, FBTR must beat FBT.
#
# IT is deliberately excluded: the Income Tax Ruling series predates
# year-based numbering and is always cited by sequence number alone (IT 117,
# IT 131). Including it would mis-parse "IT117" as "IT 11/7". Legacy
# un-yeared series are an iteration target for later rule additions.
_YEAR_SERIES = sorted([
    "SMSFRB", "SMSFR", "SMSFD",
    "GSTR", "GSTD", "FBTR", "WETR", "WETD",
    "LCR", "SGR", "FTR", "PCG", "LCG", "PRR", "CLR", "COG", "TXD", "TPA", "FBT",
    "CR", "PR", "TR", "TD", "MT", "TA", "LI", "LG", "WT",
], key=len, reverse=True)
_YEAR_SERIES_ALT = "|".join(_YEAR_SERIES)

# 4-digit-year form: TR20243 -> TR 2024/3, PCG2025D6 -> PCG 2025/D6.
_RE_YEAR4 = re.compile(rf"^({_YEAR_SERIES_ALT})(\d{{4}})(D?)(\d+)$")
# Pre-2000 legacy 2-digit-year form: TR9725 -> TR 97/25. Year must start
# with 8 or 9 (1980s/1990s); otherwise "MT2005" (a legacy un-yeared MT
# ruling number 2005) would mis-parse as "MT 20/05".
_RE_YEAR2 = re.compile(rf"^({_YEAR_SERIES_ALT})([89]\d)(\d+)$")
# PS LA — final.
_RE_PSLA = re.compile(r"^PSLA(\d{4})(\d+)$")
# PS LA — draft (the PSD inner prefix itself marks it as draft; render with D).
_RE_PSLA_DRAFT = re.compile(r"^PSD(\d{4})D?(\d+)$")
# ATO ID: ATOID or AID inner prefix -> "ATO ID YYYY/NN".
_RE_ATOID = re.compile(r"^(?:ATOID|AID)(\d{4})(\d+)$")


def human_code_for_doc_id(doc_id: str) -> str | None:
    """Derive the short human citation (e.g. ``"TR 2024/3"``) from ``doc_id``.

    Operates on the *second* path segment of the v4 docid — the series code.
    For ``"TXR/TR20243/NAT/ATO/00001"`` that's ``"TR20243"`` → ``"TR 2024/3"``.

    Returns ``None`` for formats the rule set doesn't recognise (legacy
    un-yeared IT/TD, consolidated EC/addendum suffixes, malformed paths);
    callers must tolerate nulls by leaving ``documents.human_code`` unset.
    Grow this rule set as new ATO document ID formats are found in the corpus.
    """
    segments = [s for s in doc_id.split("/") if s]
    if len(segments) < 2:
        return None
    body = segments[1]
    # Try modern 4-digit year first so e.g. TR20081 is "TR 2008/1", not
    # "TR 20/081" as the 2-digit-year rule would emit.
    m = _RE_YEAR4.match(body)
    if m:
        series, year, draft, number = m.groups()
        return f"{series} {year}/{draft}{number}"
    m = _RE_PSLA.match(body)
    if m:
        year, number = m.groups()
        return f"PS LA {year}/{number}"
    m = _RE_PSLA_DRAFT.match(body)
    if m:
        year, number = m.groups()
        return f"PS LA {year}/D{number}"
    m = _RE_ATOID.match(body)
    if m:
        year, number = m.groups()
        return f"ATO ID {year}/{number}"
    # Legacy 2-digit year applies only when the 4-digit rule didn't match.
    m = _RE_YEAR2.match(body)
    if m:
        series, year, number = m.groups()
        return f"{series} {year}/{number}"
    return None


def content_hash(text: str, metadata: dict[str, Any]) -> str:
    """Stable hash of cleaned source-derived text plus key metadata."""
    h = hashlib.sha256()
    h.update(text.encode("utf-8", errors="replace"))
    for key in ("title", "doc_type", "pub_date", "status"):
        value = metadata.get(key)
        if value:
            h.update(b"\0")
            h.update(key.encode("ascii"))
            h.update(b"=")
            h.update(str(value).encode("utf-8"))
    return "sha256:" + h.hexdigest()
