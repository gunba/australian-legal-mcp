"""Manifest + diff round-trip."""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from ato_mcp.indexer.build import BuildArgs, _reranker_model_info
from ato_mcp.store.manifest import (
    DEFAULT_MIN_CLIENT_VERSION,
    DocRef,
    MANIFEST_SCHEMA_VERSION,
    Manifest,
    ModelInfo,
    PackInfo,
    diff_manifests,
    load_manifest,
    manifest_fingerprint,
    save_manifest,
    save_update_summary,
    update_summary_from_manifest,
)


def _m(docs: list[DocRef]) -> Manifest:
    return Manifest(
        index_version="2026.04.18",
        created_at="2026-04-18T00:00:00+00:00",
        model=ModelInfo(id="x", sha256="0" * 64, size=1, url="model"),
        documents=docs,
        packs=[PackInfo(sha8="deadbeef", sha256="0" * 64, size=1, url="p")],
    )


def _doc(doc_id: str, content_hash: str) -> DocRef:
    return DocRef(
        doc_id=doc_id,
        content_hash=content_hash,
        pack_sha8="deadbeef",
        offset=0,
        length=10,
        category="Cases",
        title=doc_id,
    )


def test_roundtrip(tmp_path: Path) -> None:
    m = _m([_doc("a", "h1"), _doc("b", "h2")])
    path = tmp_path / "manifest.json"
    save_manifest(m, path)
    again = load_manifest(path)
    assert again.documents[0].doc_id == "a"
    assert again.packs[0].sha8 == "deadbeef"


def test_diff_added_changed_removed() -> None:
    old = _m([_doc("a", "h1"), _doc("b", "h2")])
    new = _m([_doc("a", "h1"), _doc("b", "h2b"), _doc("c", "h3")])
    added, changed, removed = diff_manifests(old, new)
    assert [r.doc_id for r in added] == ["c"]
    assert [r.doc_id for r in changed] == ["b"]
    assert removed == []


def test_diff_marks_same_content_repack_changed() -> None:
    old_doc = _doc("a", "same")
    new_doc = _doc("a", "same")
    new_doc.pack_sha8 = "feedface"
    new_doc.length = 12

    added, changed, removed = diff_manifests(_m([old_doc]), _m([new_doc]))

    assert added == []
    assert [r.doc_id for r in changed] == ["a"]
    assert removed == []


def test_manifest_schema_version_bumped_to_3() -> None:
    """Wave 3 (0.6.0) bumps the manifest schema version to 3 so older Rust
    binaries refuse to ingest a v3 corpus.

    The Rust side's `MAX_SUPPORTED_MANIFEST_VERSION` advances in lockstep;
    this constant is the gate the build pipeline writes into freshly-built
    manifests.
    """
    assert MANIFEST_SCHEMA_VERSION == 3
    fresh = _m([])
    assert fresh.schema_version == 3


def test_min_client_version_pins_to_0_6_1() -> None:
    """Wave 3 bumps the minimum client version to 0.6.1. Older binaries
    decoding the v3 manifest would parse fine (the new `reranker` field is
    optional), but the runtime's `min_client_version > CARGO_PKG_VERSION`
    check rejects them earlier with a friendlier "upgrade required" error.
    """
    assert DEFAULT_MIN_CLIENT_VERSION == "0.6.1"
    fresh = _m([])
    assert fresh.min_client_version == "0.6.1"


def test_manifest_with_reranker_serializes_and_deserializes(tmp_path: Path) -> None:
    """A manifest with a populated `reranker: ModelInfo` round-trips
    losslessly through JSON serialization."""
    rer = ModelInfo(
        id="gte-reranker-modernbert-base-quantized",
        sha256="b" * 64,
        size=150_871_837,
        url="hf://Alibaba-NLP/gte-reranker-modernbert-base@abc123",
    )
    m = Manifest(
        index_version="2026.05.03",
        created_at="2026-05-03T00:00:00+00:00",
        model=ModelInfo(id="x", sha256="0" * 64, size=1, url="model"),
        reranker=rer,
        documents=[_doc("a", "h1")],
        packs=[PackInfo(sha8="deadbeef", sha256="0" * 64, size=1, url="p")],
    )
    path = tmp_path / "manifest.json"
    save_manifest(m, path)
    loaded = load_manifest(path)
    assert loaded.reranker is not None
    assert loaded.reranker.id == "gte-reranker-modernbert-base-quantized"
    assert loaded.reranker.sha256 == "b" * 64
    assert loaded.reranker.size == 150_871_837
    assert loaded.reranker.url == "hf://Alibaba-NLP/gte-reranker-modernbert-base@abc123"

    # The on-disk JSON must include the reranker field so older Rust binaries
    # can detect it (and the new ones can deserialize it).
    raw = json.loads(path.read_text())
    assert raw["reranker"]["id"] == "gte-reranker-modernbert-base-quantized"


def test_manifest_without_reranker_omits_field_or_defaults_none(tmp_path: Path) -> None:
    """A manifest built without a reranker round-trips with `reranker: None`.

    The JSON serialization must still emit the key (Pydantic default), so the
    Rust side can distinguish "no reranker" from "missing field" reliably.
    """
    m = _m([_doc("a", "h1")])
    assert m.reranker is None
    path = tmp_path / "manifest.json"
    save_manifest(m, path)
    loaded = load_manifest(path)
    assert loaded.reranker is None
    raw = json.loads(path.read_text())
    # Pydantic emits null when the field is None; the Rust side decodes both
    # null and absent as no-reranker.
    assert raw.get("reranker") is None


def test_update_summary_keeps_fast_check_fields(tmp_path: Path) -> None:
    rer = ModelInfo(
        id="gte-reranker-modernbert-base-quantized",
        sha256="b" * 64,
        size=150_871_837,
        url="hf://Alibaba-NLP/gte-reranker-modernbert-base@abc123",
        tokenizer_sha256="c" * 64,
    )
    m = Manifest(
        index_version="2026.05.03",
        created_at="2026-05-03T00:00:00+00:00",
        model=ModelInfo(id="embeddinggemma", sha256="a" * 64, size=1, url="model"),
        reranker=rer,
        documents=[_doc("a", "h1"), _doc("b", "h2")],
        packs=[PackInfo(sha8="deadbeef", sha256="0" * 64, size=1, url="p")],
    )

    summary = update_summary_from_manifest(m)
    assert summary.index_version == "2026.05.03"
    assert summary.document_count == 2
    assert summary.pack_count == 1
    assert summary.manifest_fingerprint == manifest_fingerprint(m)
    assert summary.reranker is not None
    assert summary.reranker.tokenizer_sha256 == "c" * 64

    path = tmp_path / "update.json"
    save_update_summary(m, path)
    raw = json.loads(path.read_text())
    assert "documents" not in raw
    assert raw["document_count"] == 2
    assert raw["manifest_fingerprint"] == manifest_fingerprint(m)
    assert raw["reranker"]["id"] == "gte-reranker-modernbert-base-quantized"


def test_build_reranker_manifest_requires_integrity_fields(tmp_path: Path) -> None:
    args = BuildArgs(
        pages_dir=tmp_path,
        out_dir=tmp_path,
        db_path=tmp_path / "ato.db",
        model_id="embeddinggemma-300m-int8-256d",
        model_path=tmp_path / "model.onnx",
        tokenizer_path=tmp_path / "tokenizer.json",
        reranker_id="ms-marco-minilm-l6-v2-int8",
        reranker_url="hf://example/repo@abc",
    )

    with pytest.raises(ValueError, match="sha256.*size"):
        _reranker_model_info(args)


def test_build_reranker_manifest_accepts_complete_metadata(tmp_path: Path) -> None:
    args = BuildArgs(
        pages_dir=tmp_path,
        out_dir=tmp_path,
        db_path=tmp_path / "ato.db",
        model_id="embeddinggemma-300m-int8-256d",
        model_path=tmp_path / "model.onnx",
        tokenizer_path=tmp_path / "tokenizer.json",
        reranker_id="ms-marco-minilm-l6-v2-int8",
        reranker_url="hf://example/repo@abc",
        reranker_sha256="a" * 64,
        reranker_size=123,
        reranker_tokenizer_sha256="b" * 64,
    )

    info = _reranker_model_info(args)
    assert info is not None
    assert info.sha256 == "a" * 64
    assert info.size == 123
    assert info.tokenizer_sha256 == "b" * 64
