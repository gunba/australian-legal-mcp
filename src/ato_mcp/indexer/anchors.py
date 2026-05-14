"""Doc-navigation extraction via the Rust ato-mcp binary.

Thin Python subprocess wrapper. The classification logic for in_doc /
sister / history anchor refs lives in src/main.rs (extract_anchors),
including label resolution priority (sibling row cells, anchor's own
text, default-date suffix), sentinel PiT handling, and dedup.

Public API preserved for tests/test_doc_anchors.py and build.py:
- AnchorRef dataclass
- extract_anchors(html, source_doc_id) -> list[AnchorRef]
- anchor_target_to_chunk(anchor, chunk_texts) -> chunk_id | None
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


@dataclass(frozen=True)
class AnchorRef:
    kind: str  # 'in_doc' | 'sister' | 'history'
    label: str
    target_anchor: str | None = None
    target_doc_id: str | None = None
    target_pit: str | None = None


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


def extract_anchors(html: str, *, source_doc_id: str) -> list[AnchorRef]:
    if not html.strip():
        return []
    proc = subprocess.run(
        [_ato_mcp_bin(), "extract-anchors", "--source-doc-id", source_doc_id],
        input=html,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"ato-mcp extract-anchors failed (exit {proc.returncode}): {proc.stderr.strip()}"
        )
    payload = json.loads(proc.stdout)
    out: list[AnchorRef] = []
    for entry in payload:
        out.append(
            AnchorRef(
                kind=entry["kind"],
                label=entry["label"],
                target_anchor=entry.get("target_anchor"),
                target_doc_id=entry.get("target_doc_id"),
                target_pit=entry.get("target_pit"),
            )
        )
    return out


def anchor_target_to_chunk(
    anchor: str, chunk_texts: Iterable[tuple[int, str]]
) -> int | None:
    """Find the chunk_id whose text contains [anchor:<anchor>]. Pure Python
    helper — no subprocess needed because callers pass chunk text in-process."""
    marker = f"[anchor:{anchor}]"
    for chunk_id, text in chunk_texts:
        if marker in text:
            return chunk_id
    return None
