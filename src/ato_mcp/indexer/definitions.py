"""Definition extraction via the Rust ato-mcp binary.

Thin Python subprocess wrapper. The ***term*** marker scanner with body
cue detection lives in src/main.rs (extract_definitions).

Public API preserved for tests/test_definitions.py and build.py:
- DefinitionChunk dataclass (input chunks)
- Definition dataclass (output records)
- extract_definitions(*, doc_id, source_title, source_type, chunks)
- normalize_term helper
"""
from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path


DEFINITIONS_FORMAT_VERSION = 2

_WS_RE = re.compile(r"\s+")


@dataclass(frozen=True)
class Definition:
    definition_id: str
    term: str
    norm_term: str
    doc_id: str
    source_title: str
    source_type: str
    scope: str | None
    anchor: str | None
    ord: int
    body: str


@dataclass(frozen=True)
class DefinitionChunk:
    ord: int
    anchor: str | None
    text: str


def normalize_term(term: str) -> str:
    term = term.replace("\\*", "*").replace("\\&", "&")
    term = term.strip(" \t\r\n:*")
    term = _WS_RE.sub(" ", term)
    return term.casefold()


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


def extract_definitions(
    *,
    doc_id: str,
    source_title: str,
    source_type: str,
    chunks: list[DefinitionChunk],
) -> list[Definition]:
    payload = {
        "doc_id": doc_id,
        "source_title": source_title,
        "source_type": source_type,
        "chunks": [
            {"ord": c.ord, "anchor": c.anchor, "text": c.text} for c in chunks
        ],
    }
    proc = subprocess.run(
        [_ato_mcp_bin(), "extract-definitions"],
        input=json.dumps(payload),
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"ato-mcp extract-definitions failed (exit {proc.returncode}): {proc.stderr.strip()}"
        )
    raw = json.loads(proc.stdout)
    return [Definition(**entry) for entry in raw]
