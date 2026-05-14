"""HTML extraction via the Rust ato-mcp binary.

This module is a thin Python subprocess wrapper around the Rust scraper. The
heavy lifting — container picking, noise stripping, link rewriting, image
asset extraction, attribute scrubbing, currency detection, anchor / heading
extraction — all lives in src/main.rs (functions clean_ato_html,
rewrite_images_html, normalise_named_anchors, strip_attributes,
extract_currency, doc_id_from_ato_link, extract_collect_anchors,
extract_em_front_matter, extract_leading_headings, extract_compose_title).

The wrapper preserves the legacy Python API surface so existing callers
(build.py, tests/test_extract.py) keep working unmodified. Each public
function shells out to the Rust ``ato-mcp`` CLI, parses the JSON
response, and returns the corresponding dataclass.

The Rust binary is located via ``ATO_MCP_BIN`` env var, then PATH, then
the in-repo ``target/release/ato-mcp`` build artefact.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path


# ---------------------------------------------------------------------------
# Dataclasses (kept stable for callers / tests that import them).

@dataclass(frozen=True)
class ExtractedAsset:
    asset_ref: str
    source_path: str
    relative_path: str
    media_type: str | None
    alt: str | None
    title: str | None
    sha256: str
    size: int
    data_b64: str


@dataclass
class ExtractedDoc:
    html: str
    text: str
    title: str | None
    html_title: str | None = None
    headings: list[str] = field(default_factory=list)
    heading_levels: list[int] = field(default_factory=list)
    anchors: list[tuple[str, str]] = field(default_factory=list)
    assets: list[ExtractedAsset] = field(default_factory=list)
    front_matter_refs: list[str] = field(default_factory=list)
    front_matter_phrase: str | None = None


@dataclass(frozen=True)
class CurrencyInfo:
    withdrawn_date: str | None = None
    superseded_by: str | None = None
    replaces: str | None = None


# ---------------------------------------------------------------------------
# Binary lookup.

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


def _run(args: list[str], *, stdin: str | None = None) -> str:
    cmd = [_ato_mcp_bin(), *args]
    proc = subprocess.run(
        cmd,
        input=stdin,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"ato-mcp {' '.join(args)} failed (exit {proc.returncode}): {proc.stderr.strip()}"
        )
    return proc.stdout


# ---------------------------------------------------------------------------
# Public API — preserves the legacy Python signatures.

def extract(
    html: str,
    *,
    doc_id: str | None = None,
    source_path: Path | None = None,
) -> ExtractedDoc:
    if not html or not html.strip():
        return ExtractedDoc(html="", text="", title=None, html_title=None)
    args = ["extract"]
    if doc_id:
        args += ["--doc-id", doc_id]
    if source_path:
        args += ["--source-path", str(source_path)]
    raw = _run(args, stdin=html)
    payload = json.loads(raw)
    assets = [ExtractedAsset(**a) for a in payload.get("assets", [])]
    anchors_pairs = [tuple(p) for p in payload.get("anchors", [])]
    return ExtractedDoc(
        html=payload.get("html") or "",
        text=payload.get("text") or "",
        title=payload.get("title"),
        html_title=payload.get("html_title"),
        headings=payload.get("headings") or [],
        heading_levels=payload.get("heading_levels") or [],
        anchors=anchors_pairs,
        assets=assets,
        front_matter_refs=payload.get("front_matter_refs") or [],
        front_matter_phrase=payload.get("front_matter_phrase"),
    )


def extract_currency(html: str) -> CurrencyInfo:
    if not html or not html.strip():
        return CurrencyInfo()
    raw = _run(["extract-currency"], stdin=html)
    payload = json.loads(raw)
    return CurrencyInfo(
        withdrawn_date=payload.get("withdrawn_date"),
        superseded_by=payload.get("superseded_by"),
        replaces=payload.get("replaces"),
    )


def _doc_id_from_ato_link(target: str) -> tuple[str, str | None, str | None] | None:
    raw = _run(["doc-id-from-link", target])
    payload = json.loads(raw)
    if payload is None:
        return None
    return payload["doc_id"], payload.get("pit"), payload.get("view")
