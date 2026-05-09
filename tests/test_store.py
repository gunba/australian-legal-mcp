"""DB schema + basic insert / search sanity tests.

These exercise the schema without the embedding model; vector rows are stubbed
with all-zero int8 embeddings and never queried.
"""
from __future__ import annotations

from pathlib import Path

import zstandard as zstd

from ato_mcp.store import db as store_db
from ato_mcp.store.queries import (
    INSERT_CHUNK,
    INSERT_CHUNK_FTS,
    INSERT_DOCUMENT,
    INSERT_TITLE_FTS,
    INSERT_VEC,
)


def _seed_doc(
    conn,
    doc_id: str,
    title: str,
    text: str,
    *,
    withdrawn_date: str | None = None,
    superseded_by: str | None = None,
    replaces: str | None = None,
) -> int:
    conn.execute(
        INSERT_DOCUMENT,
        (
            doc_id, "Public_rulings", title, "2024-07-01",
            "2026-04-18T00:00:00Z",
            "sha256:" + "0" * 64, "deadbeef",
            zstd.ZstdCompressor(level=3).compress(b"<div></div>"),
            withdrawn_date, superseded_by, replaces,
        ),
    )
    conn.execute(INSERT_TITLE_FTS, (doc_id, title, ""))
    compressed = zstd.ZstdCompressor(level=3).compress(text.encode("utf-8"))
    cur = conn.execute(INSERT_CHUNK, (doc_id, 0, "Root", None, compressed))
    rowid = cur.lastrowid
    conn.execute(INSERT_CHUNK_FTS, (rowid, text, "Root"))
    conn.execute(INSERT_VEC, (rowid, b"\x00" * store_db.EMBEDDING_DIM))
    return rowid


def test_schema_inserts_and_queries(tmp_path: Path) -> None:
    conn = store_db.init_db(tmp_path / "ato.db")
    _seed_doc(conn, "TXR/TR20243/NAT/ATO/00001", "TR 2024/3 — R&D tax incentive ruling",
              "Research and development activities definition.")
    _seed_doc(conn, "TXR/TR9725/NAT/ATO/00001", "TR 97/25 — Capital works deductions",
              "Division 43 applies to buildings.")
    # W2.2 currency columns should accept values and round-trip.
    _seed_doc(
        conn,
        "TXR/TR20221/NAT/ATO/00001",
        "TR 2022/1 — withdrawn",
        "Effective life of depreciating assets.",
        withdrawn_date="2025-10-31",
        superseded_by="TR 2025/1",
    )

    docs = conn.execute("SELECT COUNT(*) AS n FROM documents").fetchone()["n"]
    assert docs == 3

    rows = conn.execute(
        "SELECT doc_id FROM title_fts WHERE title_fts MATCH ?",
        ("incentive",),
    ).fetchall()
    assert [r["doc_id"] for r in rows] == ["TXR/TR20243/NAT/ATO/00001"]

    rows = conn.execute(
        "SELECT rowid FROM chunks_fts WHERE chunks_fts MATCH ?",
        ("buildings",),
    ).fetchall()
    assert len(rows) == 1

    # Currency columns round-trip.
    row = conn.execute(
        "SELECT withdrawn_date, superseded_by, replaces FROM documents WHERE doc_id = ?",
        ("TXR/TR20221/NAT/ATO/00001",),
    ).fetchone()
    assert row["withdrawn_date"] == "2025-10-31"
    assert row["superseded_by"] == "TR 2025/1"
    assert row["replaces"] is None

    # And rulings without markers stay NULL.
    row = conn.execute(
        "SELECT withdrawn_date FROM documents WHERE doc_id = ?",
        ("TXR/TR20243/NAT/ATO/00001",),
    ).fetchone()
    assert row["withdrawn_date"] is None

    assert store_db.get_meta(conn, "schema_version") == store_db.SCHEMA_VERSION
    assert store_db.SCHEMA_VERSION == "7"
    conn.close()
