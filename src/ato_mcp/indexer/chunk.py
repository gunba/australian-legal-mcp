"""Block-aware chunker via the Rust ato-mcp binary.

Thin Python subprocess wrapper. The DOM walker, block renderer, atomic-block
classifier, packer, and inline marker emitter all live in src/main.rs
(chunker_walk, chunker_render_block, chunker_pack, chunk_html,
chunker_html_to_text). This wrapper preserves chunk.py's public API so
build.py and tests/test_chunk.py keep importing unchanged.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path


CHUNKER_FORMAT_VERSION = 3
EMBED_MAX_TOKENS = 1024
DEFAULT_MAX_TOKENS = EMBED_MAX_TOKENS


@dataclass
class Chunk:
    ord: int
    anchor: str | None
    text: str
    definition_text: str | None = None


def approx_tokens(text: str) -> int:
    """Whitespace split * 1.3 factor — matches the Rust impl."""
    return max(1, int(len(text.split()) * 1.3))


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


def chunk_html(
    html: str,
    *,
    root_title: str | None = None,
    max_tokens: int = DEFAULT_MAX_TOKENS,
) -> list[Chunk]:
    if not html.strip():
        return []
    args = ["chunk-html", "--max-tokens", str(max_tokens)]
    if root_title is not None:
        args += ["--root-title", root_title]
    proc = subprocess.run(
        [_ato_mcp_bin(), *args],
        input=html,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"ato-mcp chunk-html failed (exit {proc.returncode}): {proc.stderr.strip()}"
        )
    raw = json.loads(proc.stdout)
    return [
        Chunk(
            ord=entry["ord"],
            anchor=entry.get("anchor"),
            text=entry["text"],
            definition_text=entry.get("definition_text"),
        )
        for entry in raw
    ]


def html_to_text(html: str) -> str:
    """Convenience: run chunk_html and join chunk texts. Mirrors the Python
    chunk.py html_to_text behaviour at the API level (the underlying Rust
    chunker_html_to_text does the same join)."""
    if not html.strip():
        return ""
    chunks = chunk_html(html, max_tokens=EMBED_MAX_TOKENS)
    return "\n\n".join(c.text for c in chunks if c.text)
