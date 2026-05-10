"""Content-addressable pack files for release artifacts.

A pack is a single ``.bin.zst`` blob that stores many document records back
to back. Each record is ``length:uint32 | zstd(orjson(record))``. A trailer at
the end records ``(pack_sha8, records_count, index_offset)`` plus a compact
index ``[(doc_id, offset, length), ...]`` also orjson+zstd encoded.

Clients never need the trailer to fetch a specific document: the manifest
carries ``(pack_sha8, offset, length)`` directly. The trailer exists mostly
for offline verification (``ato-mcp doctor``).

Records are the fundamental delta unit. A document record contains:

    {
        "doc_id": str,                    # full docid path incl. prefix
        "type": str,                      # top-level bucket
        "title": str,                     # human-readable, citation inlined
        "date": str | None,               # best-guess publication date
        "downloaded_at": str,
        "content_hash": str,
        "chunks": [
            {"ord": int, "anchor": str | None,
             "text": str, "embedding_b64": str},
            ...
        ],
    }

``embedding_b64`` is base64-encoded raw int8 bytes.
"""
from __future__ import annotations

import base64
import hashlib
import struct
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import orjson
import zstandard as zstd

from ..store.db import EMBEDDING_DIM

TRAILER_MAGIC = b"ATOPK\x01"
RECORD_LEN = struct.Struct("<I")  # uint32 little-endian
PACK_TARGET_SIZE = 64 * 1024 * 1024  # 64 MB uncompressed record payload before zstd
# [IB-10] Pack target 64 MB uncompressed before zstd level 3 — keeps individual pack downloads tractable on slow links; PackBuilder seals + opens next writer when offset crosses target.


def encode_embedding(raw_bytes: bytes) -> str:
    # [IB-11] Embeddings travel as base64-encoded raw int8 bytes; both encode + decode length-check against EMBEDDING_DIM so a wrong-shape embedding can't slip through.
    if len(raw_bytes) != EMBEDDING_DIM:
        raise ValueError(f"embedding must be {EMBEDDING_DIM} bytes, got {len(raw_bytes)}")
    return base64.b64encode(raw_bytes).decode("ascii")


def decode_embedding(b64: str) -> bytes:
    data = base64.b64decode(b64.encode("ascii"))
    if len(data) != EMBEDDING_DIM:
        raise ValueError(f"decoded embedding must be {EMBEDDING_DIM} bytes, got {len(data)}")
    return data


@dataclass
class PackedDocRef:
    doc_id: str
    offset: int
    length: int


@dataclass
class PackWriter:
    """Stream-writes document records to a pack file.

    Usage:
        with PackWriter(path) as writer:
            writer.add(doc_id, record_dict)
        pack_sha8 = writer.sha8
    """
    # [IB-09] Pack record format: length:uint32 (LE) | zstd(orjson(record)); content-addressable via sha256[:8]; trailer at end has MAGIC + count + index_offset + index_blob (reverse index of (doc_id, offset, length)) for offline verification.

    path: Path
    level: int = 3
    _fh: Any = None
    _cctx: zstd.ZstdCompressor | None = None
    _hasher: "hashlib._Hash" = field(default_factory=hashlib.sha256)  # type: ignore[assignment]
    _offset: int = 0
    _refs: list[PackedDocRef] = field(default_factory=list)

    def __enter__(self) -> "PackWriter":
        self.path = Path(self.path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._fh = open(self.path, "wb")
        self._cctx = zstd.ZstdCompressor(level=self.level)
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        if exc_type is None:
            self._write_trailer()
        if self._fh is not None:
            self._fh.close()

    def add(self, doc_id: str, record: dict[str, Any]) -> None:
        assert self._fh is not None and self._cctx is not None
        payload = self._cctx.compress(orjson.dumps(record))
        header = RECORD_LEN.pack(len(payload))
        start = self._offset
        self._fh.write(header)
        self._fh.write(payload)
        self._hasher.update(header)
        self._hasher.update(payload)
        length = RECORD_LEN.size + len(payload)
        self._offset += length
        self._refs.append(PackedDocRef(doc_id=doc_id, offset=start, length=length))

    def _write_trailer(self) -> None:
        assert self._fh is not None and self._cctx is not None
        index_offset = self._offset
        index = [
            {"doc_id": r.doc_id, "offset": r.offset, "length": r.length}
            for r in self._refs
        ]
        index_blob = self._cctx.compress(orjson.dumps(index))
        # Trailer layout: MAGIC(6) | count(uint32) | index_offset(uint64) | index_len(uint32) | index_blob
        self._fh.write(index_blob)
        trailer = struct.pack(
            "<6sIQI",
            TRAILER_MAGIC,
            len(self._refs),
            index_offset,
            len(index_blob),
        )
        self._fh.write(trailer)
        self._hasher.update(index_blob)
        self._hasher.update(trailer)

    @property
    def sha256(self) -> str:
        return self._hasher.hexdigest()

    @property
    def sha8(self) -> str:
        return self.sha256[:8]

    @property
    def refs(self) -> list[PackedDocRef]:
        return list(self._refs)


def read_record(pack_path: Path, offset: int, length: int) -> dict[str, Any]:
    """Read a single record given its pack byte range.

    ``length`` covers the 4-byte header + compressed payload together, matching
    what the writer records.
    """
    with open(pack_path, "rb") as fh:
        fh.seek(offset)
        blob = fh.read(length)
    if len(blob) < RECORD_LEN.size:
        raise ValueError(f"short read at offset {offset}: got {len(blob)} bytes")
    (payload_len,) = RECORD_LEN.unpack_from(blob, 0)
    if payload_len + RECORD_LEN.size != length:
        raise ValueError(
            f"pack record length mismatch: header says {payload_len}+4, manifest says {length}"
        )
    payload = blob[RECORD_LEN.size :]
    dctx = zstd.ZstdDecompressor()
    return orjson.loads(dctx.decompress(payload))


def read_record_from_bytes(blob: bytes) -> dict[str, Any]:
    """Decode a record from the raw bytes matching the manifest range."""
    if len(blob) < RECORD_LEN.size:
        raise ValueError("record bytes too short")
    (payload_len,) = RECORD_LEN.unpack_from(blob, 0)
    if payload_len + RECORD_LEN.size != len(blob):
        raise ValueError("record bytes length mismatch with header")
    dctx = zstd.ZstdDecompressor()
    return orjson.loads(dctx.decompress(blob[RECORD_LEN.size :]))


@dataclass
class PackBuilder:
    """Groups records into ~target_size packs and writes them to ``out_dir``.

    Pack filenames are ``pack-<sha8>.bin.zst``. Returns a list of ``(path, refs)``
    as packs are finalized so the orchestrator can record them in the manifest.
    """

    out_dir: Path
    target_size: int = PACK_TARGET_SIZE
    _writer: PackWriter | None = None
    _packs: list[tuple[Path, str, str, int, list[PackedDocRef]]] = field(default_factory=list)

    def add(self, doc_id: str, record: dict[str, Any]) -> None:
        if self._writer is None:
            self._writer = self._new_writer()
        self._writer.add(doc_id, record)
        if self._writer._offset >= self.target_size:
            self._finalize()

    def _new_writer(self) -> PackWriter:
        tmp = Path(self.out_dir) / f".pack-writing-{len(self._packs):04d}.bin.zst.tmp"
        writer = PackWriter(path=tmp)
        writer.__enter__()
        return writer

    def _finalize(self) -> None:
        if self._writer is None:
            return
        self._writer.__exit__(None, None, None)
        tmp = self._writer.path
        sha8 = self._writer.sha8
        sha256 = self._writer.sha256
        final = Path(self.out_dir) / f"pack-{sha8}.bin.zst"
        tmp.rename(final)
        size = final.stat().st_size
        self._packs.append((final, sha8, sha256, size, self._writer.refs))
        self._writer = None

    def close(self) -> list[tuple[Path, str, str, int, list[PackedDocRef]]]:
        self._finalize()
        return list(self._packs)

    def flush(self) -> None:
        """Seal the in-flight pack right now so it hits disk.

        Use at checkpoint boundaries to make partial progress durable. Safe
        to call repeatedly; a no-op when no writer is open.
        """
        self._finalize()

    @property
    def finalized_packs(self) -> list[tuple[Path, str, str, int, list[PackedDocRef]]]:
        return list(self._packs)
