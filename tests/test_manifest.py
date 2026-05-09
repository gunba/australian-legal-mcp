"""Manifest + diff round-trip."""
from __future__ import annotations

import json
from pathlib import Path

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


def test_manifest_schema_version_bumped_to_4() -> None:
    """Cleaned HTML/assets bump the manifest schema version to 4 so older
    Rust binaries refuse to ingest packs with the new required body surface.

    The Rust side's `MAX_SUPPORTED_MANIFEST_VERSION` advances in lockstep;
    this constant is the gate the build pipeline writes into freshly-built
    manifests.
    """
    assert MANIFEST_SCHEMA_VERSION == 4
    fresh = _m([])
    assert fresh.schema_version == 4


def test_min_client_version_pins_to_0_6_9() -> None:
    """The HTML/assets corpus requires the matching Rust binary.
    """
    assert DEFAULT_MIN_CLIENT_VERSION == "0.6.9"
    fresh = _m([])
    assert fresh.min_client_version == "0.6.9"


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
