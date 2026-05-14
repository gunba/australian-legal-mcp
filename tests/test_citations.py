"""Tests for the citations derivation pass.

Inline `[doc:X]` markers in chunk text are pulled into a `citations` table
at the tail of every build. This file exercises the regex + write path on a
fixture connection so the derivation invariants stay covered without a real
corpus build.
"""
from __future__ import annotations

import sqlite3

import zstandard as zstd

from ato_mcp.indexer.build import _derive_citations


def _setup_corpus(rows: list[tuple[int, str, str]]) -> sqlite3.Connection:
    """rows := [(chunk_id, source_doc_id, text), ...]"""
    conn = sqlite3.connect(":memory:")
    conn.executescript(
        """
        CREATE TABLE documents (
            doc_id TEXT PRIMARY KEY,
            type TEXT NOT NULL DEFAULT 'x',
            title TEXT NOT NULL DEFAULT 'x',
            date TEXT,
            downloaded_at TEXT NOT NULL DEFAULT 'x',
            content_hash TEXT NOT NULL DEFAULT 'x',
            pack_sha8 TEXT NOT NULL DEFAULT 'x',
            html BLOB NOT NULL DEFAULT x'00'
        );
        CREATE TABLE chunks (
            chunk_id INTEGER PRIMARY KEY,
            doc_id TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
            text BLOB NOT NULL
        );
        CREATE TABLE citations (
            source_chunk_id INTEGER NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
            source_doc_id TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
            target_doc_id TEXT NOT NULL,
            PRIMARY KEY (source_chunk_id, target_doc_id)
        );
        """
    )
    cctx = zstd.ZstdCompressor(level=3)
    distinct_doc_ids = {doc_id for _, doc_id, _ in rows}
    conn.executemany(
        "INSERT INTO documents(doc_id) VALUES (?)",
        [(d,) for d in distinct_doc_ids],
    )
    for chunk_id, doc_id, text in rows:
        conn.execute(
            "INSERT INTO chunks(chunk_id, doc_id, text) VALUES (?, ?, ?)",
            (chunk_id, doc_id, cctx.compress(text.encode("utf-8"))),
        )
    conn.commit()
    return conn


def test_derive_citations_pulls_doc_markers() -> None:
    conn = _setup_corpus(
        [
            (1, "TXR/TR/00001", "See [doc:PAC/19970038/8-1] for context."),
            (2, "TXR/TR/00001", "Also [doc:PAC/19970038/995-1] applies."),
            (3, "PSR/PS/00001", "Refer [doc:PAC/19970038/8-1]."),
        ]
    )
    _derive_citations(conn)
    rows = sorted(conn.execute(
        "SELECT source_chunk_id, source_doc_id, target_doc_id FROM citations"
    ))
    assert rows == [
        (1, "TXR/TR/00001", "PAC/19970038/8-1"),
        (2, "TXR/TR/00001", "PAC/19970038/995-1"),
        (3, "PSR/PS/00001", "PAC/19970038/8-1"),
    ]


def test_derive_citations_strips_pit_and_view_qualifiers() -> None:
    """`[doc:X@PIT]` and `[doc:X view=HISTFT]` markers collapse to the base
    doc_id — reverse-citation lookup is at the doc-level, not the version
    level."""
    conn = _setup_corpus(
        [
            (
                1,
                "TXR/TR/00001",
                "see [doc:TXR/OLD/00001@19960320000001] and"
                " [doc:PAC/19970038/Pt3-6 view=HISTFT].",
            ),
        ]
    )
    _derive_citations(conn)
    targets = sorted(
        row[0] for row in conn.execute("SELECT target_doc_id FROM citations")
    )
    assert targets == ["PAC/19970038/Pt3-6", "TXR/OLD/00001"]


def test_derive_citations_skips_self_references() -> None:
    """A chunk that mentions its own doc shouldn't write a self-citation —
    reverse-citation lookup gets noisy otherwise."""
    conn = _setup_corpus(
        [
            (1, "TXR/TR/00001", "see [doc:TXR/TR/00001] (self) and [doc:OTHER/X]."),
        ]
    )
    _derive_citations(conn)
    rows = sorted(conn.execute(
        "SELECT source_doc_id, target_doc_id FROM citations"
    ))
    assert rows == [("TXR/TR/00001", "OTHER/X")]


def test_derive_citations_deduplicates_within_chunk() -> None:
    """A chunk that repeats the same `[doc:X]` marker only writes ONE row.
    Reverse-citation lookup cares whether a doc references another, not how
    many times within a single chunk."""
    conn = _setup_corpus(
        [
            (1, "TXR/TR/00001", "[doc:OTHER/X] foo [doc:OTHER/X] bar [doc:OTHER/X]"),
        ]
    )
    _derive_citations(conn)
    rows = list(conn.execute(
        "SELECT source_chunk_id, target_doc_id FROM citations"
    ))
    assert rows == [(1, "OTHER/X")]


def test_derive_citations_idempotent() -> None:
    """Running derivation twice yields the same rows — the function clears
    the table before repopulating so a second pass doesn't double-insert."""
    conn = _setup_corpus(
        [
            (1, "TXR/TR/00001", "[doc:OTHER/X] and [doc:OTHER/Y]"),
        ]
    )
    _derive_citations(conn)
    first = sorted(conn.execute("SELECT * FROM citations"))
    _derive_citations(conn)
    second = sorted(conn.execute("SELECT * FROM citations"))
    assert first == second
    assert len(first) == 2
