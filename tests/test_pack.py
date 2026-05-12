"""Pack writer + reader round-trip."""
from __future__ import annotations

from pathlib import Path

from ato_mcp.indexer import definitions as definition_mod
from ato_mcp.indexer.pack import (
    PackBuilder,
    PackWriter,
    encode_embedding,
    read_record,
    read_record_from_bytes,
)
from ato_mcp.indexer.build import (
    _materialize_reused_pack,
    _previous_pack_record_has_current_definitions,
)
from ato_mcp.store.manifest import DocRef, PackInfo


def _record(doc_id: str) -> dict:
    return {
        "doc_id": doc_id,
        "href": f"/law/view/document?docid={doc_id}",
        "category": "Cases",
        "doc_type": "JUD",
        "human_code": None,
        "title": f"Document {doc_id}",
        "human_title": None,
        "pub_date": None,
        "first_published_date": None,
        "effective_date": None,
        "status": "active",
        "has_content": True,
        "downloaded_at": "2026-04-18T00:00:00Z",
        "content_hash": "sha256:" + "0" * 64,
        "html": f"<div><p>Hello world chunk for {doc_id}</p></div>",
        "assets": [],
        "chunks": [
            {
                "ord": 0,
                "anchor": "s1",
                "text": "Hello world chunk for " + doc_id,
                "embedding_b64": encode_embedding(b"\x00" * 256),
            }
        ],
    }


def test_pack_round_trip(tmp_path: Path) -> None:
    path = tmp_path / "pack.bin.zst"
    with PackWriter(path=path) as writer:
        writer.add("a", _record("a"))
        writer.add("b", _record("b"))
        writer.add("c", _record("c"))
        refs = list(writer.refs)
    for r in refs:
        record = read_record(path, r.offset, r.length)
        assert record["doc_id"] == r.doc_id


def test_pack_builder_splits_on_size(tmp_path: Path) -> None:
    builder = PackBuilder(out_dir=tmp_path, target_size=512)
    for i in range(20):
        builder.add(f"doc-{i}", _record(f"doc-{i}"))
    packs = builder.close()
    assert len(packs) > 1
    # every doc resolvable from its pack + range
    for path, _sha8, _sha256, _size, refs in packs:
        for r in refs:
            rec = read_record(path, r.offset, r.length)
            assert rec["doc_id"] == r.doc_id


def test_read_record_from_bytes_matches_disk(tmp_path: Path) -> None:
    path = tmp_path / "pack.bin.zst"
    with PackWriter(path=path) as writer:
        writer.add("only", _record("only"))
        refs = list(writer.refs)
    r = refs[0]
    disk = read_record(path, r.offset, r.length)
    with open(path, "rb") as fh:
        fh.seek(r.offset)
        blob = fh.read(r.length)
    assert read_record_from_bytes(blob) == disk


def test_materialize_reused_pack_hardlinks_or_copies_previous_pack(tmp_path: Path) -> None:
    prev_packs = tmp_path / "previous" / "packs"
    new_packs = tmp_path / "new" / "packs"
    prev_packs.mkdir(parents=True)
    new_packs.mkdir(parents=True)

    src = prev_packs / "pack-deadbeef.bin.zst"
    src.write_bytes(b"previous pack bytes")

    dest = _materialize_reused_pack(
        new_packs,
        prev_packs,
        PackInfo(
            sha8="deadbeef",
            sha256="0" * 64,
            size=src.stat().st_size,
            url="https://github.com/gunba/ato-mcp/releases/download/v0.6.5/pack-deadbeef.bin.zst",
        ),
    )

    assert dest == new_packs / "pack-deadbeef.bin.zst"
    assert dest.read_bytes() == src.read_bytes()


def test_previous_pack_reuse_requires_current_definition_format(tmp_path: Path) -> None:
    packs_dir = tmp_path / "packs"
    packs_dir.mkdir()
    manifest_path = tmp_path / "manifest.json"
    manifest_path.write_text("{}", encoding="utf-8")

    old_path = packs_dir / "pack-old.bin.zst"
    with PackWriter(path=old_path) as writer:
        writer.add("old", _record("old"))
        old_ref = list(writer.refs)[0]

    old_format_record = _record("old-format")
    old_format_record["definitions"] = [
        {
            "definition_id": "old-id",
            "term": "test term",
            "norm_term": "test term",
            "doc_id": "old-format",
            "source_title": "Document old-format",
            "source_type": "Cases",
            "scope": "Document old-format",
            "heading_path": "Root",
            "anchor": None,
            "ord": 0,
            "body": "means an old-format definition.",
        }
    ]
    old_format_path = packs_dir / "pack-old-format.bin.zst"
    with PackWriter(path=old_format_path) as writer:
        writer.add("old-format", old_format_record)
        old_format_ref = list(writer.refs)[0]

    new_record = _record("new")
    new_record["definitions_format_version"] = definition_mod.DEFINITIONS_FORMAT_VERSION
    new_record["definitions"] = []
    new_path = packs_dir / "pack-new.bin.zst"
    with PackWriter(path=new_path) as writer:
        writer.add("new", new_record)
        new_ref = list(writer.refs)[0]

    assert not _previous_pack_record_has_current_definitions(
        DocRef(
            doc_id="old",
            content_hash="same",
            pack_sha8="old",
            offset=old_ref.offset,
            length=old_ref.length,
        ),
        manifest_path,
        {
            "old": PackInfo(
                sha8="old",
                sha256="0" * 64,
                size=old_path.stat().st_size,
                url="packs/pack-old.bin.zst",
            )
        },
        "Cases",
    )
    assert not _previous_pack_record_has_current_definitions(
        DocRef(
            doc_id="old-format",
            content_hash="same",
            pack_sha8="old-format",
            offset=old_format_ref.offset,
            length=old_format_ref.length,
        ),
        manifest_path,
        {
            "old-format": PackInfo(
                sha8="old-format",
                sha256="0" * 64,
                size=old_format_path.stat().st_size,
                url="packs/pack-old-format.bin.zst",
            )
        },
        "Cases",
    )
    assert _previous_pack_record_has_current_definitions(
        DocRef(
            doc_id="new",
            content_hash="same",
            pack_sha8="new",
            offset=new_ref.offset,
            length=new_ref.length,
        ),
        manifest_path,
        {
            "new": PackInfo(
                sha8="new",
                sha256="0" * 64,
                size=new_path.stat().st_size,
                url="packs/pack-new.bin.zst",
            )
        },
        "Cases",
    )
