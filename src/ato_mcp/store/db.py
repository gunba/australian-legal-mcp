"""SQLite connection helpers."""
from __future__ import annotations

import sqlite3
from pathlib import Path
from typing import Literal

from ..util import paths

SCHEMA_VERSION = "8"
EMBEDDING_DIM = 256
EMBEDDING_DTYPE = "int8"

_SCHEMA_PATH = Path(__file__).parent / "schema.sql"


def connect(
    path: Path | None = None,
    *,
    mode: Literal["ro", "rw", "rwc"] = "rwc",
    mmap_bytes: int = 256 * 1024 * 1024,
) -> sqlite3.Connection:
    """Open an ato.db connection.

    mode=ro gives a read-only handle (safe for serve). mmap reduces disk reads.
    """
    if path is None:
        path = paths.db_path()
    path = Path(path)
    if mode == "rwc":
        path.parent.mkdir(parents=True, exist_ok=True)
    uri = f"file:{path}?mode={mode}"
    conn = sqlite3.connect(uri, uri=True, isolation_level=None, timeout=30.0)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA foreign_keys = ON")
    conn.execute(f"PRAGMA mmap_size = {mmap_bytes}")
    conn.execute("PRAGMA temp_store = MEMORY")
    if mode != "ro":
        # [SL-05] WAL+synchronous=NORMAL for writers only; read-only handles skip these pragmas (writes would fail anyway).
        conn.execute("PRAGMA journal_mode = WAL")
        conn.execute("PRAGMA synchronous = NORMAL")
    return conn


def init_db(path: Path | None = None) -> sqlite3.Connection:
    """Create the DB file (if missing) and apply the current schema."""
    conn = connect(path, mode="rwc")
    conn.executescript(_SCHEMA_PATH.read_text(encoding="utf-8"))
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?, ?) "
        "ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        ("schema_version", SCHEMA_VERSION),
    )
    return conn


def get_meta(conn: sqlite3.Connection, key: str) -> str | None:
    row = conn.execute("SELECT value FROM meta WHERE key = ?", (key,)).fetchone()
    return row["value"] if row else None


def set_meta(conn: sqlite3.Connection, key: str, value: str) -> None:
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?, ?) "
        "ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )
