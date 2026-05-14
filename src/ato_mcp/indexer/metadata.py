"""Doc-metadata helpers via the Rust ato-mcp binary.

Thin Python wrapper. URL parsing, human-citation derivation and date
extraction live in src/main.rs (metadata_doc_id_for, metadata_parse_docid,
metadata_year_for_docid, metadata_human_code_for_doc_id,
metadata_extract_pub_date) — exposed via 'ato-mcp doc-meta'.

Pure-Python helpers that don't need a subprocess (path categorisation,
content hashing, metadata-signature) stay inline.
"""
from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
from pathlib import Path
from typing import Any


# ---------------------------------------------------------------------------
# Constants matching the Rust impl.

OTHER_CATEGORY = "Other_ATO_documents"
PACK_FORMAT_VERSION = 2

_METADATA_SIGNATURE_KEYS = (
    "title",
    "type",
    "date",
    "withdrawn_date",
    "superseded_by",
    "replaces",
    "pack_format_version",
)


# ---------------------------------------------------------------------------
# Binary lookup + subprocess.

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


def _doc_meta(canonical_id: str) -> dict[str, Any]:
    proc = subprocess.run(
        [_ato_mcp_bin(), "doc-meta", canonical_id],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"ato-mcp doc-meta failed (exit {proc.returncode}): {proc.stderr.strip()}"
        )
    return json.loads(proc.stdout)


# ---------------------------------------------------------------------------
# Public API delegating to Rust.

def doc_id_for(canonical_id: str) -> str:
    return _doc_meta(canonical_id)["doc_id"]


def parse_docid(canonical_id: str) -> str | None:
    return _doc_meta(canonical_id).get("type_prefix")


def year_for_docid(canonical_id: str) -> str | None:
    return _doc_meta(canonical_id).get("year")


def human_code_for_doc_id(doc_id: str) -> str | None:
    """Caller passes the bare doc_id, not a canonical URL. Build a synthetic
    canonical to feed the CLI."""
    canonical = f"https://www.ato.gov.au/law/view/document?docid={doc_id}"
    return _doc_meta(canonical).get("human_code")


# ---------------------------------------------------------------------------
# Pure-Python helpers (no subprocess overhead).

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
        return OTHER_CATEGORY
    return category


def representative_path_from_docid(
    canonical_id: str,
    *,
    title: str | None = None,
    heading: str | None = None,
) -> list[str]:
    category = OTHER_CATEGORY
    year = year_for_docid(canonical_id)
    segments = [category]
    if heading:
        segments.append(heading)
    if year:
        segments.append(year)
    segments.append(title or canonical_id)
    return segments


def extract_pub_date(text: str) -> str | None:
    """Mirror of the Rust impl in pure Python (small helper, called per-doc)."""
    import re

    pattern = re.compile(
        r"\b(\d{1,2})\s+(January|February|March|April|May|June|July|August|September|October|November|December)\s+(\d{4})\b",
        re.IGNORECASE,
    )
    m = pattern.search(text[:2000])
    if not m:
        return None
    day, month_name, year = m.groups()
    month = {
        "january": 1, "february": 2, "march": 3, "april": 4, "may": 5, "june": 6,
        "july": 7, "august": 8, "september": 9, "october": 10, "november": 11, "december": 12,
    }[month_name.lower()]
    return f"{int(year):04d}-{month:02d}-{int(day):02d}"


def content_hash(text: str) -> str:
    h = hashlib.sha256()
    h.update(text.encode("utf-8", errors="replace"))
    return "sha256:" + h.hexdigest()


def metadata_signature(meta_fields: dict[str, Any]) -> str:
    h = hashlib.sha256()
    for key in _METADATA_SIGNATURE_KEYS:
        value = meta_fields.get(key)
        h.update(b"\0")
        h.update(key.encode("ascii"))
        h.update(b"=")
        if value is not None:
            h.update(str(value).encode("utf-8"))
    return h.hexdigest()
