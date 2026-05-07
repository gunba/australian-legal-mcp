"""SQLite connection helpers.

Loads sqlite-vec on every connection, applies schema.sql on first open,
creates the vec0 virtual table once the extension is available.
"""
from __future__ import annotations

import sqlite3
from pathlib import Path
from typing import Literal

import sqlite_vec

from ..util import paths

SCHEMA_VERSION = "6"
EMBEDDING_DIM = 256
EMBEDDING_DTYPE = "int8"

_SCHEMA_PATH = Path(__file__).parent / "schema.sql"

_VEC_TABLE_DDL = f"""
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(
    chunk_id INTEGER PRIMARY KEY,
    embedding {EMBEDDING_DTYPE}[{EMBEDDING_DIM}] distance_metric=cosine
);
"""


def _load_vec(conn: sqlite3.Connection) -> None:
    # [SL-06] Toggle extension loading only for sqlite_vec.load — re-disable so application queries can't smuggle other extensions through.
    conn.enable_load_extension(True)
    sqlite_vec.load(conn)
    conn.enable_load_extension(False)


def connect(
    path: Path | None = None,
    *,
    mode: Literal["ro", "rw", "rwc"] = "rwc",
    mmap_bytes: int = 256 * 1024 * 1024,
) -> sqlite3.Connection:
    """Open an ato.db connection with sqlite-vec loaded.

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
    _load_vec(conn)
    conn.execute("PRAGMA foreign_keys = ON")
    conn.execute(f"PRAGMA mmap_size = {mmap_bytes}")
    conn.execute("PRAGMA temp_store = MEMORY")
    if mode != "ro":
        # [SL-05] WAL+synchronous=NORMAL for writers only; read-only handles skip these pragmas (writes would fail anyway).
        conn.execute("PRAGMA journal_mode = WAL")
        conn.execute("PRAGMA synchronous = NORMAL")
    return conn


def init_db(path: Path | None = None) -> sqlite3.Connection:
    """Create the DB file (if missing), apply schema, and create the vec0 table.

    Runs the migration check BEFORE executing schema.sql so a pre-v6 DB
    bails with a clear rebuild prompt rather than tripping over the new
    ``CREATE INDEX ... withdrawn_date`` DDL.
    """
    conn = connect(path, mode="rwc")
    _migrate(conn)
    conn.executescript(_SCHEMA_PATH.read_text(encoding="utf-8"))
    conn.execute(_VEC_TABLE_DDL)
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?, ?) "
        "ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        ("schema_version", SCHEMA_VERSION),
    )
    return conn


def _migrate(conn: sqlite3.Connection) -> None:
    """Reject pre-v6 databases; additively patch v6 DBs missing later tables.

    v6 adds three currency columns (``withdrawn_date``, ``superseded_by``,
    ``replaces``) to ``documents``. Wave 2 forces a re-embed because of the
    heading-aware embedder change, so additive ALTER buys nothing for end
    users; pre-v6 DBs are rejected with a rebuild prompt mirroring the
    pre-v5 rejection pattern.

    v5 collapsed the schema (human_code/human_title/category/doc_type/
    pub_date/first_published_date/effective_date/status/has_content/href
    all dropped or merged into ``type``/``title``/``date``). In-place
    column migrations are no longer supported; pre-v5 DBs should be rebuilt
    from source.

    Additive tables introduced without a schema bump are created here if
    absent so older v6 DBs that pre-date the additive table land safely.
    """
    cols = {row["name"] for row in conn.execute("PRAGMA table_info(documents)").fetchall()}
    if not cols:
        return  # fresh DB; schema.sql just created it
    if "human_code" in cols or "category" in cols or "href" in cols:
        raise RuntimeError(
            "This database is pre-v5 (it still has human_code/category/href columns).\n"
            "v5 replaced those with type/title/date. Rebuild from ato_pages/ with\n"
            "  ato-mcp build-index ..."
        )
    if "canonical_id" in cols or "docid_code" in cols:
        raise RuntimeError(
            "This database is pre-v4. Rebuild from ato_pages/ with\n"
            "  ato-mcp build-index ..."
        )
    if "withdrawn_date" not in cols:
        raise RuntimeError(
            "This database is v5 (missing currency columns).\n"
            "v6 adds withdrawn_date / superseded_by / replaces to documents,\n"
            "and Wave 2 also forces a re-embed under the heading-aware input\n"
            "format, so an in-place migration would not be sound.\n"
            "v5 corpus needs rebuild for v6 schema; run `ato-mcp init` to\n"
            "fetch the new release, or rebuild locally with `ato-mcp build-index`."
        )
    # Additive: empty_shells table landed after the initial v5 schema.
    conn.executescript(
        """
        CREATE TABLE IF NOT EXISTS empty_shells (
            doc_id          TEXT PRIMARY KEY,
            first_seen_at   TEXT NOT NULL,
            last_checked_at TEXT NOT NULL,
            check_count     INTEGER NOT NULL DEFAULT 1,
            source          TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_shells_last_checked
          ON empty_shells(last_checked_at);
        CREATE TABLE IF NOT EXISTS definitions (
            definition_id TEXT PRIMARY KEY,
            term          TEXT NOT NULL,
            norm_term     TEXT NOT NULL,
            doc_id        TEXT NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
            source_title  TEXT NOT NULL,
            source_type   TEXT NOT NULL,
            scope         TEXT,
            heading_path  TEXT,
            anchor        TEXT,
            ord           INTEGER NOT NULL,
            body          TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_definitions_norm_term ON definitions(norm_term);
        CREATE INDEX IF NOT EXISTS idx_definitions_doc ON definitions(doc_id);
        """
    )


def get_meta(conn: sqlite3.Connection, key: str) -> str | None:
    row = conn.execute("SELECT value FROM meta WHERE key = ?", (key,)).fetchone()
    return row["value"] if row else None


def set_meta(conn: sqlite3.Connection, key: str, value: str) -> None:
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?, ?) "
        "ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )
