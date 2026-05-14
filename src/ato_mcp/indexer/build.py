"""Maintainer entry point: read ato_pages/, write ato.db + packs + manifest.

Orchestrates:
1. Enumerate ``index.jsonl`` records from a scraped ``ato_pages/`` directory.
2. Load each payload HTML.
3. Extract cleaned HTML, metadata, status.
4. Chunk cleaned HTML into plain semantic search text.
5. Embed chunks (int8 via EmbeddingGemma ONNX).
6. Write document into a sqlite ``ato.db`` and a release pack.
7. Emit a ``manifest.json`` suitable for clients.

Supports ``incremental`` rebuilds: if a prior manifest is supplied, a
document's ``content_hash`` is unchanged, and its previous pack record is
compatible with the current extracted fields, the existing pack slot is reused.
"""
from __future__ import annotations

import base64
import json
import logging
import os
import re
import shutil
import struct
import time
from concurrent.futures import ProcessPoolExecutor
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterator, Literal

import numpy as np
import zstandard as zstd

from ..embed.model import EmbeddingModel, vec_to_bytes
from ..store import db as store_db
from ..store.manifest import (
    DocRef,
    Manifest,
    ModelInfo,
    PackInfo,
    load_manifest,
    save_update_summary,
)
from ..store.queries import (
    DELETE_ALL_CITATIONS,
    INSERT_ASSET,
    INSERT_CHUNK,
    INSERT_CHUNK_FTS,
    INSERT_CITATION,
    INSERT_DEFINITION,
    INSERT_DOC_ANCHOR,
    INSERT_DOCUMENT,
    INSERT_TITLE_FTS,
    INSERT_VEC,
    UPDATE_DOC_NAVIGATION_FLAGS,
)
from ..util.log import get_logger
from . import anchors as anchors_mod
from . import chunk as chunk_mod
from . import definitions as definition_mod
from . import extract as extract_mod
from . import metadata as meta_mod
from . import rules as rules_mod
from .pack import (
    PACK_TARGET_SIZE,
    TRAILER_MAGIC,
    PackBuilder,
    PackWriter,
    PackedDocRef,
    encode_embedding,
    read_record,
)

LOGGER = get_logger(__name__)

BASE_URL = "https://www.ato.gov.au"

# [IB-13] CHECKPOINT_EVERY=20000 docs commits the in-progress SQLite txn AND flushes the in-flight pack — a failure loses at most the current build window.
# Commit the in-progress transaction every N newly-processed docs so a kill
# mid-run only loses at most this many docs of work. Packs sealed at each
# checkpoint become immutable on disk and are picked up on restart.
CHECKPOINT_EVERY = 20_000

INSERT_CHUNK_WITH_ID = """
INSERT INTO chunks (chunk_id, doc_id, ord, anchor, text)
VALUES (?, ?, ?, ?, ?)
"""


@dataclass
class BuildArgs:
    pages_dir: Path
    out_dir: Path  # receives manifest.json + packs/
    db_path: Path  # new ato.db
    model_id: str
    model_path: Path | None
    tokenizer_path: Path | None
    model_sha256: str | None = None
    model_size: int | None = None
    previous_manifest: Path | None = None
    limit: int | None = None  # optional cap for testing
    # [IB-17] Production build-index exposes only EmbeddingGemma.
    # Lexical/hash-vector experiments are not release corpus embedders, and
    # query-time keyword mode is separate.
    embedder: Literal["embeddinggemma"] = "embeddinggemma"
    encode_batch_size: int = 64
    max_batch_tokens: int = 8192
    providers: tuple[str, ...] | None = None  # ORT execution providers override
    workers: int = max(1, (os.cpu_count() or 2) - 1)
    window_docs: int = 20_000
    checkpoint_every: int = CHECKPOINT_EVERY
    unsafe_fast_sqlite: bool = False
    zstd_level: int = 3
    pack_target_size: int = PACK_TARGET_SIZE


@dataclass
class PreparedChunk:
    ord: int
    anchor: str | None
    text: str
    definition_text: str | None = None


@dataclass
class PreparedAsset:
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
class PreparedDoc:
    doc_id: str
    category: str
    title: str
    date: str | None
    downloaded_at: str
    content_hash: str
    metadata_signature: str
    headings_text: str
    html: str
    assets: list[PreparedAsset]
    chunks: list[PreparedChunk]
    definitions: list[definition_mod.Definition]
    # W2.2 currency markers — None when the source page carries no marker.
    withdrawn_date: str | None = None
    superseded_by: str | None = None
    replaces: str | None = None
    doc_anchors: list[anchors_mod.AnchorRef] = field(default_factory=list)


Prepared = PreparedDoc | None


def _embedding_input(title: str, text: str) -> str:
    """Compose the passage sent to EmbeddingGemma for a chunk."""
    return f"{title}\n{text}".strip()


@dataclass
class FastPackBuilder(PackBuilder):
    zstd_level: int = 3

    def _new_writer(self) -> PackWriter:
        tmp = Path(self.out_dir) / f".pack-writing-{len(self._packs):04d}.bin.zst.tmp"
        writer = PackWriter(path=tmp, level=self.zstd_level)
        writer.__enter__()
        return writer


@dataclass
class WindowTimings:
    prepare: float = 0.0
    embed: float = 0.0
    write: float = 0.0
    manifest: float = 0.0


@dataclass
class EncodedWindow:
    # [IB-19] Fresh-build telemetry reports batch shape and throughput from
    # this wrapper: encode calls, real tokens_seen, approximate padded-token
    # pressure, and max observed batch size.
    vectors_int8: np.ndarray
    tokens_seen: int
    encode_calls: int
    max_batch_size: int
    max_padded_tokens: int
    approx_padded_tokens: int


@dataclass
class BatchEncodeStats:
    tokens_seen: int = 0
    encode_calls: int = 0
    max_batch_size: int = 0
    max_padded_tokens: int = 0
    approx_padded_tokens: int = 0


def _build_fresh_windowed(args: BuildArgs) -> Manifest:
    # [IB-12] Fresh builds fully re-embed; incremental builds reuse pack slots only when content_hash is unchanged and the prior pack record is compatible with current extracted fields.
    if args.embedder != "embeddinggemma":
        raise ValueError("build-index requires --embedder embeddinggemma")
    _reset_fresh_outputs(args.out_dir, args.db_path)
    asset_root = args.db_path.parent
    packs_dir = args.out_dir / "packs"
    packs_dir.mkdir(parents=True, exist_ok=True)

    conn = store_db.init_db(args.db_path)
    if args.unsafe_fast_sqlite:
        _apply_unsafe_fast_sqlite_pragmas(conn)

    model_id = args.model_id
    store_db.set_meta(conn, "embedding_model_id", model_id)
    store_db.set_meta(conn, "index_version", _today_version())

    model: EmbeddingModel | None = None
    if args.embedder == "embeddinggemma":
        if args.model_path is None or args.tokenizer_path is None:
            raise ValueError("--model-path and --tokenizer-path are required for embeddinggemma builds")
        model = EmbeddingModel(
            model_path=args.model_path,
            tokenizer_path=args.tokenizer_path,
            providers=args.providers,
        )

    pack_builder = FastPackBuilder(
        out_dir=packs_dir,
        target_size=args.pack_target_size,
        zstd_level=args.zstd_level,
    )
    doc_refs: list[DocRef] = []
    timings = WindowTimings()
    seen = docs_count = empty_shells = chunks_count = tokens_seen = windows = 0
    encode_calls = approx_padded_tokens = 0
    since_checkpoint = 0
    t0 = time.monotonic()

    index_records = _iter_index(args.pages_dir)
    if args.limit is not None:
        index_records = _take(index_records, args.limit)

    conn.execute("BEGIN")
    try:
        for window in _windowed(index_records, args.window_docs):
            windows += 1
            seen += len(window)
            phase = time.monotonic()
            prepared = _prepare_window(args.pages_dir, window, args.workers)
            prepare_s = time.monotonic() - phase
            timings.prepare += prepare_s

            docs = [item for item in prepared if isinstance(item, PreparedDoc)]
            empty_count = sum(1 for item in prepared if item is None)
            docs_count += len(docs)
            empty_shells += empty_count

            texts: list[str] = []
            doc_chunk_ranges: list[tuple[PreparedDoc, int, int]] = []
            for doc in docs:
                start = len(texts)
                # [W2.1] Embedder input prefixes title. Lifts recall on legal
                # text by giving the model the doc context EmbeddingGemma was
                # pretrained against. Heading hierarchy is rendered inline in
                # chunk.text already (# / ## / **) so it flows through here.
                texts.extend(_embedding_input(doc.title, c.text) for c in doc.chunks)
                doc_chunk_ranges.append((doc, start, len(texts)))
            chunks_count += len(texts)

            phase = time.monotonic()
            window_tokens = 0
            window_encode_calls = 0
            window_max_batch_size = 0
            window_max_padded_tokens = 0
            window_approx_padded_tokens = 0
            if texts:
                assert model is not None
                encoded = _encode_length_bucketed(
                    model,
                    texts,
                    batch_size=args.encode_batch_size,
                    max_batch_tokens=args.max_batch_tokens,
                )
                vectors_i8 = encoded.vectors_int8
                window_tokens = encoded.tokens_seen
                window_encode_calls = encoded.encode_calls
                window_max_batch_size = encoded.max_batch_size
                window_max_padded_tokens = encoded.max_padded_tokens
                window_approx_padded_tokens = encoded.approx_padded_tokens
                tokens_seen += window_tokens
                encode_calls += window_encode_calls
                approx_padded_tokens += window_approx_padded_tokens
            else:
                vectors_i8 = np.empty((0, store_db.EMBEDDING_DIM), dtype=np.int8)
            embed_s = time.monotonic() - phase
            timings.embed += embed_s

            phase = time.monotonic()
            _write_window(
                conn=conn,
                pack_builder=pack_builder,
                doc_refs=doc_refs,
                doc_chunk_ranges=doc_chunk_ranges,
                vectors_i8=vectors_i8,
                zstd_level=args.zstd_level,
                asset_root=asset_root,
            )
            write_s = time.monotonic() - phase
            timings.write += write_s

            since_checkpoint += len(prepared)
            if since_checkpoint >= args.checkpoint_every:
                _checkpoint(conn, pack_builder, doc_refs)
                since_checkpoint = 0

            elapsed = time.monotonic() - t0
            LOGGER.info(
                "window=%d seen=%d docs=%d empty_shells=%d chunks=%d elapsed=%.1fs "
                "docs/s=%.1f prepare=%.1fs embed=%.1fs write=%.1fs "
                "embed_calls=%d tokens=%d tokens/s=%.1f chunks/s=%.1f "
                "max_batch=%d max_padded_tokens=%d approx_padded_tokens=%d "
                "encode_batch_size=%d max_batch_tokens=%d",
                windows,
                seen,
                docs_count,
                empty_shells,
                chunks_count,
                elapsed,
                seen / elapsed if elapsed else 0.0,
                prepare_s,
                embed_s,
                write_s,
                window_encode_calls,
                window_tokens,
                window_tokens / embed_s if embed_s else 0.0,
                len(texts) / embed_s if embed_s else 0.0,
                window_max_batch_size,
                window_max_padded_tokens,
                window_approx_padded_tokens,
                args.encode_batch_size,
                args.max_batch_tokens,
            )

        _checkpoint(conn, pack_builder, doc_refs)
    except Exception:
        conn.execute("ROLLBACK")
        raise

    phase = time.monotonic()
    doc_refs_final = _load_doc_refs_from_db(conn, [packs_dir])
    packs = _scan_packs_dir(packs_dir)
    manifest = Manifest(
        index_version=_today_version(),
        created_at=datetime.now(timezone.utc).isoformat(),
        model=ModelInfo(
            id=model_id,
            sha256=args.model_sha256 or "",
            size=args.model_size or 0,
            url=f"model/{model_id}.onnx.zst",
        ),
        documents=doc_refs_final,
        packs=packs,
    )
    (args.out_dir / "manifest.json").write_bytes(manifest.to_bytes())
    save_update_summary(manifest, args.out_dir / "update.json")
    timings.manifest += time.monotonic() - phase

    total = time.monotonic() - t0
    LOGGER.info(
        "Indexed %d docs, %d chunks, %d empty shells in %.1fs "
        "(prepare=%.1fs embed=%.1fs write=%.1fs manifest=%.1fs "
        "tokens=%d embed_calls=%d tokens/s=%.1f chunks/s=%.1f approx_padded_tokens=%d "
        "encode_batch_size=%d max_batch_tokens=%d)",
        len(doc_refs_final),
        chunks_count,
        empty_shells,
        total,
        timings.prepare,
        timings.embed,
        timings.write,
        timings.manifest,
        tokens_seen,
        encode_calls,
        tokens_seen / timings.embed if timings.embed else 0.0,
        chunks_count / timings.embed if timings.embed else 0.0,
        approx_padded_tokens,
        args.encode_batch_size,
        args.max_batch_tokens,
    )
    _derive_citations(conn)
    # The transaction opened by the trailing _checkpoint() still needs to
    # commit so citation rows persist; the connection is in autocommit
    # mode (isolation_level=None) so a COMMIT here closes the implicit
    # transaction started after the last checkpoint.
    conn.execute("COMMIT")
    _log_currency_summary(conn)
    return manifest


def build(args: BuildArgs) -> Manifest:
    LOGGER.info(
        "Embed batch config: encode_batch_size=%d max_batch_tokens=%d",
        args.encode_batch_size,
        args.max_batch_tokens,
    )
    if args.previous_manifest is None:
        return _build_fresh_windowed(args)

    if args.embedder != "embeddinggemma":
        raise ValueError("incremental previous-manifest builds currently require --embedder embeddinggemma")
    if args.model_path is None or args.tokenizer_path is None:
        raise ValueError("--model-path and --tokenizer-path are required for embeddinggemma builds")

    args.out_dir.mkdir(parents=True, exist_ok=True)
    asset_root = args.db_path.parent
    packs_dir = args.out_dir / "packs"
    packs_dir.mkdir(parents=True, exist_ok=True)

    # Clean stale tmp pack files left by a prior crashed run.
    for stale in packs_dir.glob(".pack-writing-*.bin.zst.tmp"):
        stale.unlink()

    prev_manifest: Manifest | None = None
    prev_docs: dict[str, DocRef] = {}
    prev_pack_info: dict[str, PackInfo] = {}
    prev_packs_dir: Path | None = None
    if args.previous_manifest and args.previous_manifest.exists():
        prev_manifest = load_manifest(args.previous_manifest)
        prev_docs = prev_manifest.doc_index()
        prev_pack_info = prev_manifest.pack_index()
        prev_packs_dir = Path(args.previous_manifest).parent / "packs"
        LOGGER.info("Loaded previous manifest with %d documents", len(prev_docs))

    conn = store_db.init_db(args.db_path)
    if args.unsafe_fast_sqlite:
        _apply_unsafe_fast_sqlite_pragmas(conn)
    store_db.set_meta(conn, "embedding_model_id", args.model_id)
    store_db.set_meta(conn, "index_version", _today_version())

    # [IB-14] Resume support: doc_ids already in documents with sealed pack_sha8 (not 'PENDING') are skipped — prior commit landed rows + pack atomically.
    # Resume support: any doc_id already in documents with a sealed pack
    # (pack_sha8 != PENDING) is skipped this run. The prior commit landed its
    # rows + pack bytes atomically, so the state is safe to keep.
    resume_done = _load_resume_state(conn)
    if resume_done:
        LOGGER.info("Resuming: %d documents already committed; will skip them", len(resume_done))

    index_records = _iter_index(args.pages_dir)
    if args.limit is not None:
        index_records = _take(index_records, args.limit)

    model = EmbeddingModel(
        model_path=args.model_path,
        tokenizer_path=args.tokenizer_path,
        providers=args.providers,
    )
    pack_builder = FastPackBuilder(
        out_dir=packs_dir,
        target_size=args.pack_target_size,
        zstd_level=args.zstd_level,
    )

    doc_refs: list[DocRef] = []
    timings = WindowTimings()
    seen = processed = reused = metadata_refreshed = changed = empty_shells = chunks_count = windows = 0
    tokens_seen = encode_calls = approx_padded_tokens = 0
    since_checkpoint = 0
    t0 = time.monotonic()

    conn.execute("BEGIN")
    try:
        for raw_window in _windowed(index_records, args.window_docs):
            active_records = [
                rec
                for rec in raw_window
                if meta_mod.doc_id_for(rec["canonical_id"]) not in resume_done
            ]
            if not active_records:
                continue

            windows += 1
            seen += len(active_records)
            phase = time.monotonic()
            prepared = _prepare_window(args.pages_dir, active_records, args.workers)
            prepare_s = time.monotonic() - phase
            timings.prepare += prepare_s

            empty_count = 0
            texts: list[str] = []
            doc_chunk_ranges: list[tuple[PreparedDoc, int, int]] = []
            metadata_refresh_jobs: list[tuple[dict, PreparedDoc, DocRef, Path]] = []
            window_reused = 0
            window_metadata_refreshed = 0

            phase = time.monotonic()
            for rec, item in zip(active_records, prepared, strict=True):
                if item is None:
                    empty_count += 1
                    continue

                prev_ref = prev_docs.get(item.doc_id)
                prev_pack_path: Path | None = None
                prev_record: dict | None = None
                chunks_unchanged = False
                if prev_ref is not None and prev_ref.pack_sha8 in prev_pack_info:
                    prev_pack_path = _locate_previous_pack_path(
                        prev_ref, args.previous_manifest, prev_pack_info
                    )
                    if prev_pack_path is not None and prev_ref.content_hash == item.content_hash:
                        # Manifest hash matches the freshly-computed item hash:
                        # the previous build emitted the same chunks under the
                        # same recipe. Load the pack record so branches 1/2 can
                        # reuse its bytes.
                        chunks_unchanged = True
                        prev_record = read_record(
                            prev_pack_path, prev_ref.offset, prev_ref.length
                        )

                # Branch 1: wholesale slot reuse — content + metadata both unchanged.
                if (
                    chunks_unchanged
                    and prev_record is not None
                    and _record_has_current_definitions(prev_record, item.category)
                    and _record_metadata_signature(prev_record) == item.metadata_signature
                ):
                    # Carry the new-recipe content_hash on the new manifest so
                    # the next incremental build doesn't have to bridge again.
                    new_ref = prev_ref.model_copy(update={"content_hash": item.content_hash})
                    doc_refs.append(new_ref)
                    _insert_from_previous_record(
                        conn,
                        new_ref,
                        prev_record,
                        asset_root,
                    )
                    window_reused += 1
                    continue

                # Branch 2: metadata-refresh — chunks+embeddings reusable, but
                # row-level metadata (title / status / withdrawn_date / ...)
                # changed. Repack the record header + DB row without touching
                # the embedding model. [TODO-IB metadata-refresh path: chunks
                # and embedding_b64 carried byte-identically from prev pack
                # record; only top-level metadata fields and definitions are
                # rewritten.]
                if (
                    chunks_unchanged
                    and prev_record is not None
                    and _record_has_current_definitions(prev_record, item.category)
                ):
                    assert prev_pack_path is not None
                    metadata_refresh_jobs.append((rec, item, prev_ref, prev_pack_path))
                    window_metadata_refreshed += 1
                    continue

                # Branch 3: full re-extract + re-embed.
                start = len(texts)
                texts.extend(
                    _embedding_input(item.title, c.text)
                    for c in item.chunks
                )
                doc_chunk_ranges.append((item, start, len(texts)))
            reuse_write_s = time.monotonic() - phase

            phase = time.monotonic()
            window_tokens = 0
            window_encode_calls = 0
            window_max_batch_size = 0
            window_max_padded_tokens = 0
            window_approx_padded_tokens = 0
            if texts:
                encoded = _encode_length_bucketed(
                    model,
                    texts,
                    batch_size=args.encode_batch_size,
                    max_batch_tokens=args.max_batch_tokens,
                )
                vectors_i8 = encoded.vectors_int8
                window_tokens = encoded.tokens_seen
                window_encode_calls = encoded.encode_calls
                window_max_batch_size = encoded.max_batch_size
                window_max_padded_tokens = encoded.max_padded_tokens
                window_approx_padded_tokens = encoded.approx_padded_tokens
                tokens_seen += window_tokens
                encode_calls += window_encode_calls
                approx_padded_tokens += window_approx_padded_tokens
            else:
                vectors_i8 = np.empty((0, store_db.EMBEDDING_DIM), dtype=np.int8)
            embed_s = time.monotonic() - phase
            timings.embed += embed_s

            phase = time.monotonic()
            _write_window(
                conn=conn,
                pack_builder=pack_builder,
                doc_refs=doc_refs,
                doc_chunk_ranges=doc_chunk_ranges,
                vectors_i8=vectors_i8,
                zstd_level=args.zstd_level,
                asset_root=asset_root,
            )
            _write_metadata_refresh(
                conn=conn,
                pack_builder=pack_builder,
                doc_refs=doc_refs,
                jobs=metadata_refresh_jobs,
                zstd_level=args.zstd_level,
                asset_root=asset_root,
            )
            write_s = reuse_write_s + (time.monotonic() - phase)
            timings.write += write_s

            window_changed = len(doc_chunk_ranges)
            window_empty = empty_count
            reused += window_reused
            metadata_refreshed += window_metadata_refreshed
            changed += window_changed
            empty_shells += window_empty
            chunks_count += len(texts)
            processed += (
                window_reused
                + window_metadata_refreshed
                + window_changed
                + window_empty
            )
            since_checkpoint += (
                window_reused
                + window_metadata_refreshed
                + window_changed
                + window_empty
            )
            if since_checkpoint >= args.checkpoint_every:
                _checkpoint(conn, pack_builder, doc_refs)
                since_checkpoint = 0

            elapsed = time.monotonic() - t0
            LOGGER.info(
                "window=%d seen=%d processed=%d reused=%d metadata_refreshed=%d "
                "changed=%d empty_shells=%d "
                "chunks=%d elapsed=%.1fs prepare=%.1fs embed=%.1fs write=%.1fs "
                "embed_calls=%d tokens=%d tokens/s=%.1f chunks/s=%.1f "
                "max_batch=%d max_padded_tokens=%d approx_padded_tokens=%d "
                "encode_batch_size=%d max_batch_tokens=%d",
                windows,
                seen,
                processed,
                reused,
                metadata_refreshed,
                changed,
                empty_shells,
                chunks_count,
                elapsed,
                prepare_s,
                embed_s,
                write_s,
                window_encode_calls,
                window_tokens,
                window_tokens / embed_s if embed_s else 0.0,
                len(texts) / embed_s if embed_s else 0.0,
                window_max_batch_size,
                window_max_padded_tokens,
                window_approx_padded_tokens,
                args.encode_batch_size,
                args.max_batch_tokens,
            )

        # Final checkpoint seals the last pack and commits leftover docs.
        _checkpoint(conn, pack_builder, doc_refs)
    except Exception:
        conn.execute("ROLLBACK")
        raise

    # Build the manifest from DB state so resumed runs pick up work committed
    # in prior sessions as well as this one.
    pack_search_dirs = [packs_dir]
    if prev_packs_dir is not None:
        pack_search_dirs.append(prev_packs_dir)
    doc_refs_final = _load_doc_refs_from_db(conn, pack_search_dirs)
    new_packs = _scan_packs_dir(packs_dir)
    have = {p.sha8 for p in new_packs}
    referenced = {r.pack_sha8 for r in doc_refs_final}
    for sha8 in referenced - have:
        if sha8 not in prev_pack_info:
            raise RuntimeError(
                f"doc references pack {sha8} but it's neither in {packs_dir} "
                f"nor in the previous manifest"
            )
        _materialize_reused_pack(packs_dir, prev_packs_dir, prev_pack_info[sha8])
    new_packs = _scan_packs_dir(packs_dir)

    manifest = Manifest(
        index_version=_today_version(),
        created_at=datetime.now(timezone.utc).isoformat(),
        model=ModelInfo(
            id=args.model_id,
            sha256=args.model_sha256 or "",
            size=args.model_size or 0,
            url=f"model/{args.model_id}.onnx.zst",
        ),
        documents=doc_refs_final,
        packs=new_packs,
    )
    (args.out_dir / "manifest.json").write_bytes(manifest.to_bytes())
    save_update_summary(manifest, args.out_dir / "update.json")
    dt = time.monotonic() - t0
    LOGGER.info(
        "Indexed %d records this session "
        "(%d changed, %d reused, %d metadata_refreshed, %d empty shells); "
        "manifest has %d docs total in %.1fs "
        "(prepare=%.1fs embed=%.1fs write=%.1fs tokens=%d embed_calls=%d "
        "tokens/s=%.1f chunks/s=%.1f approx_padded_tokens=%d "
        "encode_batch_size=%d max_batch_tokens=%d)",
        processed,
        changed,
        reused,
        metadata_refreshed,
        empty_shells,
        len(doc_refs_final),
        dt,
        timings.prepare,
        timings.embed,
        timings.write,
        tokens_seen,
        encode_calls,
        tokens_seen / timings.embed if timings.embed else 0.0,
        chunks_count / timings.embed if timings.embed else 0.0,
        approx_padded_tokens,
        args.encode_batch_size,
        args.max_batch_tokens,
    )
    _derive_citations(conn)
    conn.execute("COMMIT")
    _log_currency_summary(conn)
    return manifest


def _backfill_pack_slots(
    doc_refs: list[DocRef],
    packs_written: list[tuple[Path, str, str, int, list[PackedDocRef]]],
    conn,
) -> None:
    # Build a lookup of (doc_id -> pack_sha8, offset, length)
    slot: dict[str, tuple[str, int, int]] = {}
    for _path, sha8, _sha256, _size, refs in packs_written:
        for r in refs:
            slot[r.doc_id] = (sha8, r.offset, r.length)
    for ref in doc_refs:
        if ref.pack_sha8 != "PENDING":
            continue  # reused from previous manifest
        found = slot.get(ref.doc_id)
        if not found:
            raise RuntimeError(f"pack slot not found for doc {ref.doc_id}")
        ref.pack_sha8, ref.offset, ref.length = found
        conn.execute(
            "UPDATE documents SET pack_sha8 = ? WHERE doc_id = ?",
            (ref.pack_sha8, ref.doc_id),
        )


def _reset_fresh_outputs(out_dir: Path, db_path: Path) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    if db_path.exists():
        db_path.unlink()
    manifest = out_dir / "manifest.json"
    if manifest.exists():
        manifest.unlink()
    packs_dir = out_dir / "packs"
    if packs_dir.exists():
        shutil.rmtree(packs_dir)
    build_dir = out_dir / ".build"
    if build_dir.exists():
        shutil.rmtree(build_dir)
    asset_dir = db_path.parent / "assets"
    if asset_dir.exists():
        shutil.rmtree(asset_dir)


def _apply_unsafe_fast_sqlite_pragmas(conn) -> None:
    conn.execute("PRAGMA journal_mode = OFF")
    conn.execute("PRAGMA synchronous = OFF")
    conn.execute("PRAGMA locking_mode = EXCLUSIVE")
    conn.execute("PRAGMA cache_size = -1048576")
    conn.execute("PRAGMA temp_store = MEMORY")


def _prepare_window(pages_dir: Path, records: list[dict], workers: int) -> list[Prepared]:
    items = ((pages_dir, rec) for rec in records)
    if workers <= 1:
        return [_prepare_one(item) for item in items]
    # [IB-16] Window-prepare phase parallelises HTML extract + chunking via ProcessPoolExecutor (workers = cpu_count - 1); embed + DB-write phases stay single-threaded since they hold the SQLite transaction.
    with ProcessPoolExecutor(max_workers=workers, initializer=_prepare_worker_init) as pool:
        return list(pool.map(_prepare_one, items, chunksize=32))


def _prepare_worker_init() -> None:
    logging.getLogger("ato_mcp.indexer.metadata").setLevel(logging.ERROR)


def _prepare_one(item: tuple[Path, dict]) -> Prepared:
    pages_dir, rec = item
    canonical_id = rec["canonical_id"]
    doc_id = meta_mod.doc_id_for(canonical_id)
    category = meta_mod.category_for_record(canonical_id, rec.get("payload_path"))

    status = rec.get("status")
    has_content = status == "success"
    headings: list[str] = []
    heading_levels: list[int] = []
    title: str | None = None
    html: str | None = None
    clean_html = ""
    assets: list[PreparedAsset] = []
    front_matter_refs: tuple[str, ...] = ()
    front_matter_phrase: str | None = None

    if has_content and rec.get("payload_path"):
        payload_path = pages_dir / rec["payload_path"]
        if payload_path.exists():
            html = payload_path.read_text(encoding="utf-8", errors="replace")
            extracted = extract_mod.extract(html, doc_id=doc_id, source_path=payload_path)
            clean_html = extracted.html
            headings = extracted.headings
            heading_levels = extracted.heading_levels
            front_matter_refs = tuple(extracted.front_matter_refs)
            front_matter_phrase = extracted.front_matter_phrase
            assets = [
                PreparedAsset(
                    asset_ref=a.asset_ref,
                    source_path=a.source_path,
                    relative_path=a.relative_path,
                    media_type=a.media_type,
                    alt=a.alt,
                    title=a.title,
                    sha256=a.sha256,
                    size=a.size,
                    data_b64=a.data_b64,
                )
                for a in extracted.assets
            ]
            title = extracted.title
        else:
            has_content = False

    if not title:
        title = (rec.get("title") or canonical_id).strip() or canonical_id

    chunks = [
        PreparedChunk(c.ord, c.anchor, c.text, c.definition_text)
        for c in chunk_mod.chunk_html(clean_html, root_title=title)
    ] if has_content and clean_html else []

    if has_content and not chunks:
        has_content = False

    if not has_content:
        return None

    body_text = "\n\n".join(c.text for c in chunks)
    pub_date = meta_mod.extract_pub_date(body_text) if body_text else None
    derived = rules_mod.derive_metadata(
        rules_mod.RuleInputs(
            doc_id=doc_id,
            title=title,
            headings=tuple(headings),
            heading_levels=tuple(heading_levels),
            body_head=body_text[:3000] if body_text else "",
            category=category,
            pub_date=pub_date,
            front_matter_refs=front_matter_refs,
            front_matter_phrase=front_matter_phrase,
        )
    )
    derived_title = derived.title or title
    downloaded_at = rec.get("downloaded_at") or datetime.now(timezone.utc).isoformat()
    asset_hashes = "\n".join(f"{a.asset_ref}:{a.sha256}" for a in assets)
    # chunk_fingerprint covers chunk-shape changes that aren't already captured
    # by clean_html. We do NOT include c.definition_text: definition markers
    # are a deterministic function of the cleaned HTML, so any change there
    # already perturbs clean_html in the hash. Including definition_text here
    # also blocks the bridge that recomputes content_hash from a stored pack
    # record — the on-disk record carries text but not definition_text.
    chunk_fingerprint = "\n".join(
        "\t".join(
            (
                str(c.ord),
                c.anchor or "",
                c.text,
            )
        )
        for c in chunks
    )
    content_hash = meta_mod.content_hash(
        f"{clean_html}\n{asset_hashes}\n{chunk_fingerprint}",
    )
    # W2.2: extract currency markers from the source HTML (alert panels +
    # body prose + history table). Cheap relative to the extract+chunk pass
    # already done; runs from the same parse if selectolax cached anything.
    currency = extract_mod.extract_currency(html or "")
    definitions = _extract_definitions_for_doc(
        doc_id=doc_id,
        title=derived_title,
        category=category,
        chunks=chunks,
    )
    metadata_signature = meta_mod.metadata_signature(
        {
            "title": derived_title,
            "type": category,
            "date": derived.date,
            "withdrawn_date": currency.withdrawn_date,
            "superseded_by": currency.superseded_by,
            "replaces": currency.replaces,
            "pack_format_version": meta_mod.PACK_FORMAT_VERSION,
            "chunker_format_version": chunk_mod.CHUNKER_FORMAT_VERSION,
        }
    )
    doc_anchors = anchors_mod.extract_anchors(clean_html, source_doc_id=doc_id)
    return PreparedDoc(
        doc_id=doc_id,
        category=category,
        title=derived_title,
        date=derived.date,
        downloaded_at=downloaded_at,
        content_hash=content_hash,
        metadata_signature=metadata_signature,
        headings_text=" ".join(headings),
        html=clean_html,
        assets=assets,
        chunks=chunks,
        definitions=definitions,
        withdrawn_date=currency.withdrawn_date,
        superseded_by=currency.superseded_by,
        replaces=currency.replaces,
        doc_anchors=doc_anchors,
    )


def _extract_definitions_for_doc(
    *,
    doc_id: str,
    title: str,
    category: str,
    chunks: list[PreparedChunk],
) -> list[definition_mod.Definition]:
    if category != "Legislation_and_supporting_material":
        return []
    return definition_mod.extract_definitions(
        doc_id=doc_id,
        source_title=title,
        source_type=category,
        chunks=[
            definition_mod.DefinitionChunk(
                c.ord,
                c.anchor,
                c.definition_text or c.text,
            )
            for c in chunks
        ],
    )


def _definition_rows(definitions: list[definition_mod.Definition]) -> list[tuple]:
    return [
        (
            d.definition_id,
            d.term,
            d.norm_term,
            d.doc_id,
            d.source_title,
            d.source_type,
            d.scope,
            d.anchor,
            d.ord,
            d.body,
        )
        for d in definitions
    ]


def _asset_rows(doc_id: str, assets: list[PreparedAsset]) -> list[tuple]:
    return [
        (
            a.asset_ref,
            doc_id,
            a.source_path,
            a.relative_path,
            a.media_type,
            a.alt,
            a.title,
            a.sha256,
            a.size,
        )
        for a in assets
    ]


def _write_asset_files(asset_root: Path, assets: list[PreparedAsset]) -> None:
    for asset in assets:
        target = asset_root / asset.relative_path
        if target.exists() and target.stat().st_size == asset.size:
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(base64.b64decode(asset.data_b64))


def _locate_previous_pack_path(
    prev_ref: DocRef,
    prev_manifest_path: Path | None,
    prev_packs: dict[str, PackInfo],
) -> Path | None:
    if prev_manifest_path is None:
        return None
    prev_root = Path(prev_manifest_path).parent
    pack_path = prev_root / "packs" / f"pack-{prev_ref.pack_sha8}.bin.zst"
    if not pack_path.exists():
        info = prev_packs.get(prev_ref.pack_sha8)
        if info:
            pack_path = prev_root / info.url
    return pack_path if pack_path.exists() else None


def _record_has_current_definitions(record: dict, category: str) -> bool:
    return (
        (record.get("type") or record.get("category") or "") == category
        and record.get("definitions_format_version") == definition_mod.DEFINITIONS_FORMAT_VERSION
        and "definitions" in record
        and "html" in record
        and "assets" in record
    )


def _previous_pack_record_has_current_definitions(
    prev_ref: DocRef,
    prev_manifest_path: Path | None,
    prev_packs: dict[str, PackInfo],
    category: str,
) -> bool:
    """Return true only when a reusable old pack carries current definitions.

    Reusing a content-stable pack from an older extractor would make the new
    manifest point at records that cannot hydrate current definition IDs on
    user installs.
    """

    pack_path = _locate_previous_pack_path(prev_ref, prev_manifest_path, prev_packs)
    if pack_path is None:
        return False
    record = read_record(pack_path, prev_ref.offset, prev_ref.length)
    return _record_has_current_definitions(record, category)


def _record_metadata_signature(record: dict) -> str:
    return meta_mod.metadata_signature(
        {
            "title": record.get("title"),
            "type": record.get("type") or record.get("category"),
            "date": record.get("date") or record.get("first_published_date"),
            "withdrawn_date": record.get("withdrawn_date"),
            "superseded_by": record.get("superseded_by"),
            "replaces": record.get("replaces"),
            "pack_format_version": record.get("pack_format_version"),
        }
    )


def _encode_length_bucketed(
    model: EmbeddingModel,
    texts: list[str],
    *,
    batch_size: int,
    max_batch_tokens: int,
) -> EncodedWindow:
    if not texts:
        return EncodedWindow(
            vectors_int8=np.empty((0, store_db.EMBEDDING_DIM), dtype=np.int8),
            tokens_seen=0,
            encode_calls=0,
            max_batch_size=0,
            max_padded_tokens=0,
            approx_padded_tokens=0,
        )

    lengths = [min(1024, chunk_mod.approx_tokens(t) + 16) for t in texts]
    order = sorted(range(len(texts)), key=lengths.__getitem__)
    vectors = np.empty((len(texts), store_db.EMBEDDING_DIM), dtype=np.int8)
    tokens_seen = 0
    encode_calls = 0
    max_seen_batch_size = 0
    max_seen_padded_tokens = 0
    approx_padded_tokens = 0

    pos = 0
    while pos < len(order):
        first_idx = order[pos]
        max_len = lengths[first_idx]
        end = pos + 1
        while end < len(order) and end - pos < batch_size:
            next_idx = order[end]
            next_max = max(max_len, lengths[next_idx])
            if next_max * (end - pos + 1) > max_batch_tokens:
                break
            max_len = next_max
            end += 1

        batch_indices = order[pos:end]
        stats = _encode_batch_with_split(
            model,
            texts,
            lengths,
            batch_indices,
            vectors,
            max_batch_tokens=max_batch_tokens,
            pos=pos,
            remaining=len(order) - pos,
        )
        tokens_seen += stats.tokens_seen
        encode_calls += stats.encode_calls
        approx_padded_tokens += stats.approx_padded_tokens
        max_seen_batch_size = max(max_seen_batch_size, stats.max_batch_size)
        max_seen_padded_tokens = max(max_seen_padded_tokens, stats.max_padded_tokens)
        pos = end

    return EncodedWindow(
        vectors_int8=vectors,
        tokens_seen=tokens_seen,
        encode_calls=encode_calls,
        max_batch_size=max_seen_batch_size,
        max_padded_tokens=max_seen_padded_tokens,
        approx_padded_tokens=approx_padded_tokens,
    )


def _encode_batch_with_split(
    model: EmbeddingModel,
    texts: list[str],
    lengths: list[int],
    batch_indices: list[int],
    vectors: np.ndarray,
    *,
    max_batch_tokens: int,
    pos: int,
    remaining: int,
) -> BatchEncodeStats:
    stats = BatchEncodeStats()
    pending = [batch_indices]
    while pending:
        indices = pending.pop()
        max_len = max(lengths[i] for i in indices)
        padded_tokens = max_len * len(indices)
        batch = [texts[i] for i in indices]
        try:
            encoded = model.encode(batch, is_query=False, batch_size=len(batch))
        except Exception:
            LOGGER.warning(
                "embedding batch failed batch_size=%d max_len=%d "
                "approx_padded_tokens=%d max_batch_tokens=%d pos=%d remaining=%d",
                len(indices),
                max_len,
                padded_tokens,
                max_batch_tokens,
                pos,
                remaining,
                exc_info=True,
            )
            if len(indices) == 1:
                raise
            mid = len(indices) // 2
            LOGGER.warning(
                "splitting failed embedding batch into %d and %d rows",
                mid,
                len(indices) - mid,
            )
            pending.append(indices[mid:])
            pending.append(indices[:mid])
            continue

        vectors[indices, :] = encoded.vectors_int8
        stats.tokens_seen += encoded.tokens_seen
        stats.encode_calls += 1
        stats.approx_padded_tokens += padded_tokens
        stats.max_batch_size = max(stats.max_batch_size, len(indices))
        stats.max_padded_tokens = max(stats.max_padded_tokens, padded_tokens)
    return stats


def _write_window(
    *,
    conn,
    pack_builder: PackBuilder,
    doc_refs: list[DocRef],
    doc_chunk_ranges: list[tuple[PreparedDoc, int, int]],
    vectors_i8: np.ndarray,
    zstd_level: int,
    asset_root: Path,
) -> None:
    if not doc_chunk_ranges:
        return

    conn.executemany(
        INSERT_DOCUMENT,
        [
            _document_row(doc, "PENDING", zstd_level)
            for doc, _start, _end in doc_chunk_ranges
        ],
    )
    conn.executemany(
        INSERT_TITLE_FTS,
        [(doc.doc_id, doc.title, doc.headings_text) for doc, _start, _end in doc_chunk_ranges],
    )

    next_chunk_id = _next_chunk_id(conn)
    chunk_rows = []
    chunk_fts_rows = []
    vec_rows = []
    definition_rows = []
    asset_rows = []
    zstd_compressor = zstd.ZstdCompressor(level=zstd_level)
    # (PreparedDoc, [(chunk_id, chunk_text), ...]) so we can resolve in_doc
    # anchor targets after chunks land in the DB.
    doc_chunk_pairs: list[tuple[PreparedDoc, list[tuple[int, str]]]] = []
    # Resolved doc_anchors + per-doc navigation flag updates collected as we
    # build each pack record so the pack carries the same anchor rows that
    # land in SQLite.
    doc_anchor_rows_all: list[tuple] = []
    nav_flag_updates: list[tuple] = []

    for doc, start, end in doc_chunk_ranges:
        _write_asset_files(asset_root, doc.assets)
        asset_rows.extend(_asset_rows(doc.doc_id, doc.assets))
        vectors = vectors_i8[start:end]
        record_chunks = []
        chunk_id_text_pairs: list[tuple[int, str]] = []
        for local_idx, chunk in enumerate(doc.chunks):
            chunk_id = next_chunk_id
            next_chunk_id += 1
            compressed_text = zstd_compressor.compress(chunk.text.encode("utf-8"))
            chunk_rows.append(
                (
                    chunk_id,
                    doc.doc_id,
                    chunk.ord,
                    chunk.anchor,
                    compressed_text,
                )
            )
            chunk_fts_rows.append((chunk_id, chunk.text))
            vec_bytes = vec_to_bytes(vectors[local_idx])
            vec_rows.append((chunk_id, vec_bytes))
            record_chunks.append(
                {
                    "ord": chunk.ord,
                    "anchor": chunk.anchor,
                    "text": chunk.text,
                    "embedding_b64": encode_embedding(vec_bytes),
                }
            )
            chunk_id_text_pairs.append((chunk_id, chunk.text))

        anchor_rows, nav_flags = _build_doc_anchor_rows(doc, chunk_id_text_pairs)
        pack_anchors = [
            {
                "ord": row[1],
                "kind": row[2],
                "label": row[3],
                "target_chunk_id": row[4],
                "target_doc_id": row[5],
                "target_pit": row[6],
            }
            for row in anchor_rows
        ]

        record = {
            "doc_id": doc.doc_id,
            "type": doc.category,
            "title": doc.title,
            "date": doc.date,
            "downloaded_at": doc.downloaded_at,
            "content_hash": doc.content_hash,
            "html": doc.html,
            "anchors": pack_anchors,
            "assets": [a.__dict__ for a in doc.assets],
            # W2.2 currency markers persist into the pack record so a future
            # incremental build that reuses this doc can replay the state.
            "withdrawn_date": doc.withdrawn_date,
            "superseded_by": doc.superseded_by,
            "replaces": doc.replaces,
            "has_in_doc_links": nav_flags[0],
            "has_related_docs": nav_flags[1],
            "has_history": nav_flags[2],
            "pack_format_version": meta_mod.PACK_FORMAT_VERSION,
            "chunker_format_version": chunk_mod.CHUNKER_FORMAT_VERSION,
            "definitions_format_version": definition_mod.DEFINITIONS_FORMAT_VERSION,
            "definitions": [d.__dict__ for d in doc.definitions],
            "chunks": record_chunks,
        }
        pack_builder.add(doc.doc_id, record)
        doc_anchor_rows_all.extend(anchor_rows)
        nav_flag_updates.append((*nav_flags, doc.doc_id))
        doc_refs.append(
            DocRef(
                doc_id=doc.doc_id,
                content_hash=doc.content_hash,
                pack_sha8="PENDING",
                offset=0,
                length=0,
                type=doc.category,
                title=doc.title,
                has_content=True,
            )
        )
        definition_rows.extend(_definition_rows(doc.definitions))
        doc_chunk_pairs.append((doc, chunk_id_text_pairs))

    if chunk_rows:
        conn.executemany(INSERT_CHUNK_WITH_ID, chunk_rows)
        conn.executemany(INSERT_CHUNK_FTS, chunk_fts_rows)
        conn.executemany(INSERT_VEC, vec_rows)
    if asset_rows:
        conn.executemany(INSERT_ASSET, asset_rows)
    if definition_rows:
        conn.executemany(INSERT_DEFINITION, definition_rows)

    if doc_anchor_rows_all:
        conn.executemany(INSERT_DOC_ANCHOR, doc_anchor_rows_all)
    if nav_flag_updates:
        conn.executemany(UPDATE_DOC_NAVIGATION_FLAGS, nav_flag_updates)


def _next_chunk_id(conn) -> int:
    row = conn.execute("SELECT COALESCE(MAX(chunk_id), 0) + 1 AS next_id FROM chunks").fetchone()
    return int(row["next_id"])


def _document_row(doc: PreparedDoc, pack_sha8: str, zstd_level: int) -> tuple:
    """Build the INSERT_DOCUMENT row for a freshly-extracted PreparedDoc.

    Navigation flags are written as 0; ``_persist_doc_anchors`` later updates
    them in a single statement once the anchor rows are known.
    """
    return (
        doc.doc_id,
        doc.category,
        doc.title,
        doc.date,
        doc.downloaded_at,
        doc.content_hash,
        pack_sha8,
        zstd.ZstdCompressor(level=zstd_level).compress(doc.html.encode("utf-8")),
        doc.withdrawn_date,
        doc.superseded_by,
        doc.replaces,
        0,
        0,
        0,
    )


def _persist_doc_anchors(
    conn,
    doc_chunk_pairs: list[tuple[PreparedDoc, list[tuple[int, str]]]],
) -> None:
    """Insert doc_anchor rows and update the per-doc navigation flag triple.

    ``doc_chunk_pairs`` carries each doc alongside its (chunk_id, text) pairs
    so in_doc anchor labels can be resolved to chunk_ids via the
    ``[anchor:<name>]`` markers emitted by the chunker.
    """
    if not doc_chunk_pairs:
        return
    anchor_rows: list[tuple] = []
    flag_updates: list[tuple] = []
    for doc, chunk_id_text_pairs in doc_chunk_pairs:
        rows, flags = _build_doc_anchor_rows(doc, chunk_id_text_pairs)
        anchor_rows.extend(rows)
        flag_updates.append((*flags, doc.doc_id))
    if anchor_rows:
        conn.executemany(INSERT_DOC_ANCHOR, anchor_rows)
    conn.executemany(UPDATE_DOC_NAVIGATION_FLAGS, flag_updates)


def _build_doc_anchor_rows(
    doc: PreparedDoc,
    chunk_id_text_pairs: list[tuple[int, str]],
) -> tuple[list[tuple], tuple[int, int, int]]:
    """Return (anchor_rows, (has_in_doc, has_related, has_history)) for a doc.

    ``in_doc`` rows are dropped when their target anchor cannot be resolved to
    a chunk — the anchor would dangle and provide no useful navigation.
    """
    rows: list[tuple] = []
    has_in_doc = has_related = has_history = 0
    ord_counter = 0
    for ref in doc.doc_anchors:
        target_chunk_id: int | None = None
        target_doc_id: str | None = None
        target_pit: str | None = None
        if ref.kind == "in_doc":
            assert ref.target_anchor is not None
            target_chunk_id = anchors_mod.anchor_target_to_chunk(
                ref.target_anchor, chunk_id_text_pairs
            )
            if target_chunk_id is None:
                continue
            has_in_doc = 1
        elif ref.kind == "sister":
            target_doc_id = ref.target_doc_id
            has_related = 1
        elif ref.kind == "history":
            target_doc_id = ref.target_doc_id
            target_pit = ref.target_pit
            has_history = 1
        else:
            continue
        rows.append(
            (
                doc.doc_id,
                ord_counter,
                ref.kind,
                ref.label,
                target_chunk_id,
                target_doc_id,
                target_pit,
            )
        )
        ord_counter += 1
    return rows, (has_in_doc, has_related, has_history)


def _windowed(items: Iterator[dict], size: int) -> Iterator[list[dict]]:
    window: list[dict] = []
    for item in items:
        window.append(item)
        if len(window) >= size:
            yield window
            window = []
    if window:
        yield window


def _insert_from_previous_record(
    conn,
    prev_ref: DocRef,
    record: dict,
    asset_root: Path,
) -> None:
    """Replay DB inserts for a doc whose pack record is reused verbatim.

    Caller supplies the already-loaded ``record`` so we don't read the
    previous pack twice in the dispatch loop.
    """
    doc_id = record["doc_id"]
    conn.execute(
        INSERT_DOCUMENT,
        (
            doc_id,
            record.get("type") or record.get("category") or "",
            record["title"],
            record.get("date") or record.get("first_published_date"),
            record["downloaded_at"],
            # Persist the DocRef's content_hash (already migrated to the
            # current recipe by the dispatch loop) rather than the legacy
            # value stored in the pack record itself.
            prev_ref.content_hash,
            prev_ref.pack_sha8,
            zstd.ZstdCompressor(level=3).compress(record["html"].encode("utf-8")),
            # Carry forward currency markers from the prior pack record. We
            # do NOT re-read the HTML for content-hash-stable docs, so a
            # withdrawal-status change without a body edit will be missed
            # until the next full rebuild. The ATO almost always changes
            # the alert-panel HTML on withdrawal, which changes content_hash
            # and forces a re-extract — but a body-stable withdrawal flip
            # is theoretically possible and would silently miss here.
            record.get("withdrawn_date"),
            record.get("superseded_by"),
            record.get("replaces"),
            0,
            0,
            0,
        ),
    )
    assets = [PreparedAsset(**a) for a in record.get("assets", [])]
    _write_asset_files(asset_root, assets)
    if assets:
        conn.executemany(INSERT_ASSET, _asset_rows(doc_id, assets))
    conn.execute(
        INSERT_TITLE_FTS,
        (doc_id, record["title"], ""),
    )
    chunk_id_text_pairs: list[tuple[int, str]] = []
    for c in record.get("chunks", []):
        compressed_text = zstd.ZstdCompressor(level=3).compress(c["text"].encode("utf-8"))
        cur = conn.execute(
            INSERT_CHUNK,
            (doc_id, c["ord"], c.get("anchor"), compressed_text),
        )
        chunk_rowid = cur.lastrowid
        conn.execute(INSERT_CHUNK_FTS, (chunk_rowid, c["text"]))
        from .pack import decode_embedding as _dec
        conn.execute(INSERT_VEC, (chunk_rowid, _dec(c["embedding_b64"])))
        chunk_id_text_pairs.append((chunk_rowid, c["text"]))
    if record.get("definitions_format_version") == definition_mod.DEFINITIONS_FORMAT_VERSION:
        definitions = record.get("definitions", [])
    else:
        definitions = None
    if definitions is None:
        chunks = [
            PreparedChunk(
                int(c["ord"]),
                c.get("anchor"),
                c["text"],
            )
            for c in record.get("chunks", [])
        ]
        definitions = [
            d.__dict__
            for d in _extract_definitions_for_doc(
                doc_id=doc_id,
                title=record["title"],
                category=record.get("type") or record.get("category") or "",
                chunks=chunks,
            )
        ]
    if definitions:
        conn.executemany(
            INSERT_DEFINITION,
            [
                (
                    d["definition_id"],
                    d["term"],
                    d["norm_term"],
                    d["doc_id"],
                    d["source_title"],
                    d["source_type"],
                    d.get("scope"),
                    d.get("anchor"),
                    d["ord"],
                    d["body"],
                )
                for d in definitions
            ],
        )
    # Re-extract anchors from the stored clean HTML rather than persisting
    # them in the pack record. The pack record contract stays unchanged and
    # the walk is cheap relative to a real rebuild.
    refs = anchors_mod.extract_anchors(record["html"], source_doc_id=doc_id)
    if refs:
        synthetic_doc = _synthetic_anchor_doc(doc_id, refs)
        _persist_doc_anchors(conn, [(synthetic_doc, chunk_id_text_pairs)])
    else:
        conn.execute(UPDATE_DOC_NAVIGATION_FLAGS, (0, 0, 0, doc_id))


def _synthetic_anchor_doc(doc_id: str, refs: list[anchors_mod.AnchorRef]) -> PreparedDoc:
    """Minimal PreparedDoc carrying just the fields ``_persist_doc_anchors`` reads.

    The reuse paths re-extract anchors from stored HTML and don't need the
    full prepared-doc state to insert them.
    """
    return PreparedDoc(
        doc_id=doc_id,
        category="",
        title="",
        date=None,
        downloaded_at="",
        content_hash="",
        metadata_signature="",
        headings_text="",
        html="",
        assets=[],
        chunks=[],
        definitions=[],
        doc_anchors=refs,
    )


def _write_metadata_refresh(
    *,
    conn,
    pack_builder: PackBuilder,
    doc_refs: list[DocRef],
    jobs: list[tuple[dict, "PreparedDoc", DocRef, Path]],
    zstd_level: int,
    asset_root: Path,
) -> None:
    """Repack chunks+embeddings from the previous pack with new metadata.

    For each job, we read the previous pack record (already located by the
    dispatch loop), replace its top-level metadata fields with values from
    the freshly extracted ``PreparedDoc``, and add the result to the active
    PackBuilder. The ``chunks`` list — including ``embedding_b64`` — is
    carried over byte-identically so no embedding-model work is needed.
    The DB documents/chunks/embeddings rows are inserted as a normal new
    document.
    """
    if not jobs:
        return
    zstd_compressor = zstd.ZstdCompressor(level=zstd_level)
    next_chunk_id = _next_chunk_id(conn)

    document_rows = []
    title_fts_rows = []
    asset_rows = []
    chunk_rows = []
    chunk_fts_rows = []
    vec_rows = []
    definition_rows = []
    meta_anchor_rows: list[tuple] = []
    meta_nav_flag_updates: list[tuple] = []

    from .pack import decode_embedding as _dec

    for _rec, item, prev_ref, prev_pack_path in jobs:
        record = read_record(prev_pack_path, prev_ref.offset, prev_ref.length)

        document_rows.append(_document_row(item, "PENDING", zstd_level))
        title_fts_rows.append((item.doc_id, item.title, item.headings_text))
        _write_asset_files(asset_root, item.assets)
        asset_rows.extend(_asset_rows(item.doc_id, item.assets))

        prev_chunks = record.get("chunks", [])
        chunk_id_text_pairs: list[tuple[int, str]] = []
        for c in prev_chunks:
            chunk_id = next_chunk_id
            next_chunk_id += 1
            compressed_text = zstd_compressor.compress(c["text"].encode("utf-8"))
            chunk_rows.append(
                (
                    chunk_id,
                    item.doc_id,
                    c["ord"],
                    c.get("anchor"),
                    compressed_text,
                )
            )
            chunk_fts_rows.append((chunk_id, c["text"]))
            vec_rows.append((chunk_id, _dec(c["embedding_b64"])))
            chunk_id_text_pairs.append((chunk_id, c["text"]))

        anchor_rows, nav_flags = _build_doc_anchor_rows(item, chunk_id_text_pairs)
        pack_anchors = [
            {
                "ord": row[1],
                "kind": row[2],
                "label": row[3],
                "target_chunk_id": row[4],
                "target_doc_id": row[5],
                "target_pit": row[6],
            }
            for row in anchor_rows
        ]

        new_record = {
            "doc_id": item.doc_id,
            "type": item.category,
            "title": item.title,
            "date": item.date,
            "downloaded_at": item.downloaded_at,
            "content_hash": item.content_hash,
            "html": item.html,
            "anchors": pack_anchors,
            "assets": [a.__dict__ for a in item.assets],
            "withdrawn_date": item.withdrawn_date,
            "superseded_by": item.superseded_by,
            "replaces": item.replaces,
            "has_in_doc_links": nav_flags[0],
            "has_related_docs": nav_flags[1],
            "has_history": nav_flags[2],
            "pack_format_version": meta_mod.PACK_FORMAT_VERSION,
            "chunker_format_version": chunk_mod.CHUNKER_FORMAT_VERSION,
            "definitions_format_version": definition_mod.DEFINITIONS_FORMAT_VERSION,
            "definitions": [d.__dict__ for d in item.definitions],
            "chunks": prev_chunks,
        }
        pack_builder.add(item.doc_id, new_record)
        doc_refs.append(
            DocRef(
                doc_id=item.doc_id,
                content_hash=item.content_hash,
                pack_sha8="PENDING",
                offset=0,
                length=0,
                type=item.category,
                title=item.title,
                has_content=True,
            )
        )
        definition_rows.extend(_definition_rows(item.definitions))
        meta_anchor_rows.extend(anchor_rows)
        meta_nav_flag_updates.append((*nav_flags, item.doc_id))

    if document_rows:
        conn.executemany(INSERT_DOCUMENT, document_rows)
    if title_fts_rows:
        conn.executemany(INSERT_TITLE_FTS, title_fts_rows)
    if asset_rows:
        conn.executemany(INSERT_ASSET, asset_rows)
    if chunk_rows:
        conn.executemany(INSERT_CHUNK_WITH_ID, chunk_rows)
        conn.executemany(INSERT_CHUNK_FTS, chunk_fts_rows)
        conn.executemany(INSERT_VEC, vec_rows)
    if definition_rows:
        conn.executemany(INSERT_DEFINITION, definition_rows)

    if meta_anchor_rows:
        conn.executemany(INSERT_DOC_ANCHOR, meta_anchor_rows)
    if meta_nav_flag_updates:
        conn.executemany(UPDATE_DOC_NAVIGATION_FLAGS, meta_nav_flag_updates)


def _iter_index(pages_dir: Path) -> Iterator[dict]:
    index_path = pages_dir / "index.jsonl"
    with index_path.open("r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            yield json.loads(line)


def _take(it: Iterator[dict], n: int) -> Iterator[dict]:
    count = 0
    for rec in it:
        if count >= n:
            return
        count += 1
        yield rec


def _today_version() -> str:
    return datetime.now().strftime("%Y.%m.%d")


def _checkpoint(conn, pack_builder: PackBuilder, doc_refs: list[DocRef]) -> None:
    """Seal the current pack, backfill pack_sha8 for this session's new docs,
    and commit + reopen the transaction. Safe to call when nothing is pending.
    """
    pack_builder.flush()
    sealed = pack_builder.finalized_packs
    pending = [r for r in doc_refs if r.pack_sha8 == "PENDING"]
    if pending:
        _backfill_pack_slots(pending, sealed, conn)
    store_db.set_meta(conn, "last_update_at", datetime.now(timezone.utc).isoformat())
    conn.execute("COMMIT")
    conn.execute("BEGIN")


def _load_resume_state(conn) -> set[str]:
    """Doc IDs already sealed in a prior checkpoint of this DB.

    A row is resumable only if its pack has been sealed (pack_sha8 != PENDING).
    PENDING rows live in an uncommitted transaction anyway; a crash rolled
    them back. Returns the set of doc_ids to skip on this session.
    """
    try:
        rows = conn.execute(
            "SELECT doc_id FROM documents WHERE pack_sha8 IS NOT NULL AND pack_sha8 != 'PENDING'"
        ).fetchall()
    except Exception:
        return set()
    return {r["doc_id"] for r in rows}


def _load_doc_refs_from_db(conn, pack_search_dirs: list[Path]) -> list[DocRef]:
    """Reconstruct the manifest's document list from committed DB state."""
    rows = conn.execute(
        "SELECT doc_id, content_hash, pack_sha8, type, title "
        "FROM documents WHERE pack_sha8 != 'PENDING' ORDER BY doc_id"
    ).fetchall()
    refs: list[DocRef] = [
        DocRef(
            doc_id=row["doc_id"],
            content_hash=row["content_hash"],
            pack_sha8=row["pack_sha8"],
            offset=0,
            length=0,
            type=row["type"] or "",
            title=row["title"],
            has_content=True,
        )
        for row in rows
    ]
    # offset/length aren't stored in the DB; read them from each pack's trailer.
    _populate_offsets_from_packs(refs, pack_search_dirs)
    return refs


def _populate_offsets_from_packs(refs: list[DocRef], pack_search_dirs: list[Path]) -> None:
    """Fill offset/length on doc_refs by reading each referenced pack's trailer.

    Searches the supplied directories in order so incremental builds can find
    reused packs in the previous release directory.
    """
    import orjson

    by_pack: dict[str, list[DocRef]] = {}
    for ref in refs:
        by_pack.setdefault(ref.pack_sha8, []).append(ref)

    trailer_struct = struct.Struct("<6sIQI")
    for sha8, group in by_pack.items():
        pack_path: Path | None = None
        for search_dir in pack_search_dirs:
            candidate = search_dir / f"pack-{sha8}.bin.zst"
            if candidate.exists():
                pack_path = candidate
                break
        if pack_path is None:
            raise RuntimeError(
                f"pack {sha8} not found in any of {pack_search_dirs}"
            )
        with open(pack_path, "rb") as fh:
            fh.seek(0, 2)
            size = fh.tell()
            fh.seek(size - trailer_struct.size)
            magic, _count, index_offset, index_len = trailer_struct.unpack(fh.read(trailer_struct.size))
            if magic != TRAILER_MAGIC:
                raise RuntimeError(f"pack {pack_path} has bad trailer magic")
            fh.seek(index_offset)
            index_blob = fh.read(index_len)
        entries = orjson.loads(zstd.ZstdDecompressor().decompress(index_blob))
        lut = {e["doc_id"]: (e["offset"], e["length"]) for e in entries}
        for ref in group:
            hit = lut.get(ref.doc_id)
            if hit is None:
                raise RuntimeError(f"doc {ref.doc_id} missing from pack {sha8}")
            ref.offset, ref.length = hit


def _materialize_reused_pack(
    packs_dir: Path,
    prev_packs_dir: Path | None,
    pack: PackInfo,
) -> Path:
    """Make a reused pack part of the new release output directory."""
    if prev_packs_dir is None:
        raise RuntimeError(f"cannot materialize reused pack {pack.sha8} without previous packs dir")

    filename = f"pack-{pack.sha8}.bin.zst"
    src = prev_packs_dir / filename
    if not src.exists():
        fallback = prev_packs_dir / Path(pack.url).name
        if fallback.exists():
            src = fallback
    if not src.exists():
        raise RuntimeError(f"reused pack {pack.sha8} not found under {prev_packs_dir}")

    dest = packs_dir / filename
    if dest.exists():
        if dest.stat().st_size != src.stat().st_size:
            raise RuntimeError(f"existing pack {dest} does not match reused pack {src}")
        return dest

    try:
        os.link(src, dest)
    except OSError:
        shutil.copy2(src, dest)
    return dest


def _scan_packs_dir(packs_dir: Path) -> list[PackInfo]:
    """List every sealed pack present in the release packs dir."""
    out: list[PackInfo] = []
    for p in sorted(packs_dir.glob("pack-*.bin.zst")):
        sha8 = p.stem.split("-", 1)[1].split(".", 1)[0]
        out.append(
            PackInfo(
                sha8=sha8,
                sha256="",  # filled in by the release step when needed
                size=p.stat().st_size,
                url=f"packs/pack-{sha8}.bin.zst",
            )
        )
    return out


# Pulls the base doc_id out of inline [doc:X] markers. Stops at the first
# whitespace, `]`, or `@` so PiT-qualified (`[doc:X@PIT]`) and view-qualified
# (`[doc:X view=HISTFT]`) markers all collapse to the base reference.
_DOC_MARKER_RE = re.compile(r"\[doc:([^\s\]@]+)")


def _derive_citations(conn) -> None:
    # [IB-20] Streams chunks.text in batches, regex-extracts [doc:X] markers,
    # batch-inserts into citations (PiT/view qualifiers collapse to base
    # doc_id; self-citations skipped). Idempotent: clears + repopulates.
    """Rebuild the citations table from inline `[doc:X]` markers in chunk text.

    Cheap relative to a corpus build (single pass over chunks, zstd
    decompress + regex). Idempotent: clears and repopulates so a second
    call doesn't double-insert. Skips self-citations so a chunk pointing
    back at its own doc doesn't pollute the reverse-citation lookup.
    """
    try:
        conn.execute(DELETE_ALL_CITATIONS)
    except Exception:
        # Older DB without the citations table; nothing to do. The runtime
        # creates the table on next read; the next build will populate it.
        return
    dctx = zstd.ZstdDecompressor()
    insert_batch: list[tuple[int, str, str]] = []
    flush_size = 5000
    total = 0
    cur = conn.execute("SELECT chunk_id, doc_id, text FROM chunks")
    while True:
        rows = cur.fetchmany(2000)
        if not rows:
            break
        for chunk_id, doc_id, raw in rows:
            text = _zstd_decompress_text(raw, dctx)
            targets = set(_DOC_MARKER_RE.findall(text))
            targets.discard(doc_id)
            for target in targets:
                insert_batch.append((chunk_id, doc_id, target))
        if len(insert_batch) >= flush_size:
            conn.executemany(INSERT_CITATION, insert_batch)
            total += len(insert_batch)
            insert_batch.clear()
    if insert_batch:
        conn.executemany(INSERT_CITATION, insert_batch)
        total += len(insert_batch)
    distinct_targets = conn.execute(
        "SELECT COUNT(DISTINCT target_doc_id) FROM citations"
    ).fetchone()[0]
    LOGGER.info(
        "citations: derived %d rows (%d distinct cited docs)",
        total,
        distinct_targets,
    )


def _zstd_decompress_text(blob: bytes, dctx: zstd.ZstdDecompressor) -> str:
    """Decompress a chunk-text BLOB regardless of frame header.

    Maintainer Python builds use frames with content size; the Rust runtime
    writes streaming frames (no content size) when it materialises chunks
    from a pack on `update`. Use the streaming reader so both shapes load.
    """
    import io
    return dctx.stream_reader(io.BytesIO(blob)).read().decode("utf-8")


def _log_currency_summary(conn) -> None:
    """Emit a single-line smoke test of W2.2 currency extraction.

    Reports the count of documents with a non-NULL ``withdrawn_date``. Zero is
    a red flag — either the corpus genuinely has no withdrawn rulings (likely
    only on tiny test corpora) or the extractor selectors broke. Either way,
    the maintainer wants to see this before publishing.
    """
    try:
        withdrawn, superseded, replaces = conn.execute(
            "SELECT "
            "COUNT(*) FILTER (WHERE withdrawn_date IS NOT NULL), "
            "COUNT(*) FILTER (WHERE superseded_by IS NOT NULL), "
            "COUNT(*) FILTER (WHERE replaces IS NOT NULL) "
            "FROM documents"
        ).fetchone()
    except Exception:
        # Older DB or fresh-build race — the docs table always exists by now,
        # but defensively no-op rather than failing the build for telemetry.
        return
    LOGGER.info(
        "currency metadata: %d documents have withdrawn_date set "
        "(superseded_by=%d, replaces=%d)",
        withdrawn,
        superseded,
        replaces,
    )
