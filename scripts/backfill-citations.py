#!/usr/bin/env python3
"""One-shot backfill of the citations table on an existing ato-mcp install.

Derives the table from inline `[doc:X]` markers in chunks.text. Idempotent
(clears and repopulates). Used to retrofit reverse-citation lookup onto a
local DB built before the citations table existed; future maintainer
builds derive citations as the build-index tail step.

Usage:
    python scripts/backfill-citations.py [path/to/ato.db]

When the path is omitted, defaults to the installed corpus at
~/.local/share/ato-mcp/live/ato.db.
"""
from __future__ import annotations

import sqlite3
import sys
import time
from pathlib import Path

from ato_mcp.indexer.build import _derive_citations


def main() -> None:
    if len(sys.argv) > 1:
        db_path = Path(sys.argv[1])
    else:
        db_path = Path.home() / ".local/share/ato-mcp/live/ato.db"
    if not db_path.exists():
        print(f"db not found: {db_path}", file=sys.stderr)
        sys.exit(2)

    conn = sqlite3.connect(db_path)
    try:
        conn.execute("PRAGMA foreign_keys = ON")
        conn.execute(
            """
            CREATE TABLE IF NOT EXISTS citations (
                source_chunk_id  INTEGER NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
                source_doc_id    TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
                target_doc_id    TEXT NOT NULL,
                PRIMARY KEY (source_chunk_id, target_doc_id)
            )
            """
        )
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_citations_target ON citations(target_doc_id)"
        )
        conn.commit()
        t0 = time.monotonic()
        _derive_citations(conn)
        conn.commit()
        print(f"backfill complete in {time.monotonic() - t0:.1f}s")
    finally:
        conn.close()


if __name__ == "__main__":
    main()
