"""End-to-end smoke test of the indexer against a sample of real ato_pages/.

Skipped when ``ato_pages/index.jsonl`` or the embedding model is absent.
Only exercises the non-embedding path: extract, chunk, manifest, pack, and
DB inserts via a monkeypatched ``EmbeddingModel``. Gives us one signal that
the pipeline wires together against real HTML.
"""
from __future__ import annotations

import json
import logging
import os
from pathlib import Path

import numpy as np
import pytest

ATO_PAGES = Path(os.environ.get("ATO_MCP_TEST_PAGES_DIR", "ato_pages"))


@pytest.fixture()
def tiny_pages_dir(tmp_path: Path) -> Path:
    pages_dir = tmp_path / "ato_pages_tiny"
    payload = pages_dir / "payloads" / "tiny.html"
    payload.parent.mkdir(parents=True)
    payload.write_text(
        """
        <html>
          <head><title>Example ruling</title></head>
          <body>
            <main>
              <h1>Taxation Ruling</h1>
              <h2>TR 2026/1</h2>
              <h3>Example ruling</h3>
              <p>This is body text for the small telemetry build smoke.</p>
            </main>
          </body>
        </html>
        """,
        encoding="utf-8",
    )
    record = {
        "canonical_id": (
            "https://www.ato.gov.au/law/view/document?"
            "docid=TR/TR20261/NAT/ATO/00001"
        ),
        "payload_path": "payloads/tiny.html",
        "status": "success",
        "downloaded_at": "2026-05-03T00:00:00+00:00",
    }
    (pages_dir / "index.jsonl").write_text(json.dumps(record) + "\n", encoding="utf-8")
    return pages_dir


def test_embedding_input_includes_heading_between_title_and_text() -> None:
    from ato_mcp.indexer.build import _embedding_input

    assert (
        _embedding_input("Example title", "Section 8-1 > Reasons", "Body text")
        == "Example title\nSection 8-1 > Reasons\nBody text"
    )
    assert _embedding_input("", "", "Body text") == "Body text"


def test_length_bucketed_encoder_reports_batch_telemetry(monkeypatch) -> None:
    import ato_mcp.indexer.build as build_module
    from ato_mcp.embed.model import EncodedBatch
    from ato_mcp.store import db as store_db

    token_counts = {"a": 10, "b": 20, "c": 30, "d": 200}
    monkeypatch.setattr(
        build_module.chunk_mod,
        "approx_tokens",
        lambda text: token_counts[text],
    )

    class StubModel:
        def __init__(self) -> None:
            self.batches: list[list[str]] = []

        def encode(self, texts, *, is_query, batch_size: int):
            batch = list(texts)
            self.batches.append(batch)
            return EncodedBatch(
                vectors_int8=np.zeros((len(batch), store_db.EMBEDDING_DIM), dtype=np.int8),
                tokens_seen=sum(token_counts[text] for text in batch),
            )

    model = StubModel()
    encoded = build_module._encode_length_bucketed(
        model,
        ["d", "a", "c", "b"],
        batch_size=4,
        max_batch_tokens=100,
    )

    assert model.batches == [["a", "b"], ["c"], ["d"]]
    assert encoded.vectors_int8.shape == (4, store_db.EMBEDDING_DIM)
    assert encoded.tokens_seen == 260
    assert encoded.encode_calls == 3
    assert encoded.max_batch_size == 2
    assert encoded.max_padded_tokens == 216
    assert encoded.approx_padded_tokens == 334


def test_length_bucketed_encoder_logs_batch_shape_on_failure(monkeypatch, caplog) -> None:
    import ato_mcp.indexer.build as build_module

    token_counts = {"a": 10, "b": 20, "c": 30}
    monkeypatch.setattr(
        build_module.chunk_mod,
        "approx_tokens",
        lambda text: token_counts[text],
    )
    logger = logging.getLogger("tests.embedding.batch_failure")
    logger.handlers.clear()
    logger.propagate = True
    monkeypatch.setattr(build_module, "LOGGER", logger)
    caplog.set_level(logging.WARNING, logger=logger.name)

    class FailingModel:
        def encode(self, texts, *, is_query, batch_size: int):
            raise RuntimeError("encode failed")

    with pytest.raises(RuntimeError, match="encode failed"):
        build_module._encode_length_bucketed(
            FailingModel(),
            ["a", "b", "c"],
            batch_size=3,
            max_batch_tokens=100,
        )

    assert (
        "embedding batch failed batch_size=2 max_len=36 "
        "approx_padded_tokens=72 max_batch_tokens=100 pos=0 remaining=3"
    ) in caplog.text


def test_length_bucketed_encoder_splits_failed_batch(monkeypatch, caplog) -> None:
    import ato_mcp.indexer.build as build_module
    from ato_mcp.embed.model import EncodedBatch
    from ato_mcp.store import db as store_db

    token_counts = {text: 10 for text in ["a", "b", "c", "d"]}
    monkeypatch.setattr(
        build_module.chunk_mod,
        "approx_tokens",
        lambda text: token_counts[text],
    )
    logger = logging.getLogger("tests.embedding.batch_split")
    logger.handlers.clear()
    logger.propagate = True
    monkeypatch.setattr(build_module, "LOGGER", logger)
    caplog.set_level(logging.WARNING, logger=logger.name)

    class SplittingModel:
        def __init__(self) -> None:
            self.batches: list[list[str]] = []

        def encode(self, texts, *, is_query, batch_size: int):
            batch = list(texts)
            self.batches.append(batch)
            if len(batch) > 2:
                raise RuntimeError("simulated cuda oom")
            return EncodedBatch(
                vectors_int8=np.zeros((len(batch), store_db.EMBEDDING_DIM), dtype=np.int8),
                tokens_seen=sum(token_counts[text] for text in batch),
            )

    model = SplittingModel()
    encoded = build_module._encode_length_bucketed(
        model,
        ["a", "b", "c", "d"],
        batch_size=4,
        max_batch_tokens=200,
    )

    assert model.batches == [["a", "b", "c", "d"], ["a", "b"], ["c", "d"]]
    assert encoded.vectors_int8.shape == (4, store_db.EMBEDDING_DIM)
    assert encoded.tokens_seen == 40
    assert encoded.encode_calls == 2
    assert encoded.max_batch_size == 2
    assert "splitting failed embedding batch into 2 and 2 rows" in caplog.text


def test_fresh_build_logs_embedding_window_telemetry(
    tiny_pages_dir: Path,
    tmp_path: Path,
    monkeypatch,
    caplog,
) -> None:
    import ato_mcp.indexer.build as build_module
    from ato_mcp.embed.model import EncodedBatch
    from ato_mcp.store import db as store_db

    logger = logging.getLogger("tests.embedding.window_telemetry")
    logger.handlers.clear()
    logger.propagate = True
    monkeypatch.setattr(build_module, "LOGGER", logger)
    caplog.set_level(logging.INFO, logger=logger.name)

    class StubModel:
        def __init__(self, *a, **kw) -> None:
            pass

        def encode(self, texts, *, is_query, batch_size: int = 16):
            texts_list = list(texts)
            return EncodedBatch(
                vectors_int8=np.zeros(
                    (len(texts_list), store_db.EMBEDDING_DIM),
                    dtype=np.int8,
                ),
                tokens_seen=123,
            )

    monkeypatch.setattr(build_module, "EmbeddingModel", StubModel)

    out_dir = tmp_path / "release"
    manifest = build_module.build(
        build_module.BuildArgs(
            pages_dir=tiny_pages_dir,
            out_dir=out_dir,
            db_path=out_dir / "ato.db",
            model_id="stub",
            model_path=Path("/dev/null"),
            tokenizer_path=Path("/dev/null"),
            model_sha256="0" * 64,
            model_size=0,
            workers=1,
            window_docs=1,
        )
    )

    assert len(manifest.documents) == 1
    assert "window=1" in caplog.text
    assert "embed_calls=1" in caplog.text
    assert "tokens=123" in caplog.text
    assert "tokens/s=" in caplog.text
    assert "chunks/s=" in caplog.text
    assert "max_batch=" in caplog.text
    assert "max_padded_tokens=" in caplog.text
    assert "approx_padded_tokens=" in caplog.text
    assert "encode_batch_size=64 max_batch_tokens=8192" in caplog.text


def test_previous_manifest_changed_build_uses_windowed_embedding(
    tiny_pages_dir: Path,
    tmp_path: Path,
    monkeypatch,
    caplog,
) -> None:
    import ato_mcp.indexer.build as build_module
    from ato_mcp.embed.model import EncodedBatch
    from ato_mcp.store import db as store_db

    logger = logging.getLogger("tests.embedding.incremental_window_telemetry")
    logger.handlers.clear()
    logger.propagate = True
    monkeypatch.setattr(build_module, "LOGGER", logger)
    caplog.set_level(logging.INFO, logger=logger.name)

    class StubModel:
        def __init__(self, *a, **kw) -> None:
            pass

        def encode(self, texts, *, is_query, batch_size: int = 16):
            texts_list = list(texts)
            return EncodedBatch(
                vectors_int8=np.zeros((len(texts_list), store_db.EMBEDDING_DIM), dtype=np.int8),
                tokens_seen=456,
            )

    monkeypatch.setattr(build_module, "EmbeddingModel", StubModel)

    prev_out = tmp_path / "previous"
    build_module.build(
        build_module.BuildArgs(
            pages_dir=tiny_pages_dir,
            out_dir=prev_out,
            db_path=prev_out / "ato.db",
            model_id="stub",
            model_path=Path("/dev/null"),
            tokenizer_path=Path("/dev/null"),
            workers=1,
            window_docs=1,
        )
    )

    previous_manifest = tmp_path / "previous_changed_manifest.json"
    raw_manifest = json.loads((prev_out / "manifest.json").read_text(encoding="utf-8"))
    raw_manifest["documents"][0]["content_hash"] = "force-reembed"
    previous_manifest.write_text(json.dumps(raw_manifest), encoding="utf-8")

    caplog.clear()
    out_dir = tmp_path / "changed"
    build_module.build(
        build_module.BuildArgs(
            pages_dir=tiny_pages_dir,
            out_dir=out_dir,
            db_path=out_dir / "ato.db",
            model_id="stub",
            model_path=Path("/dev/null"),
            tokenizer_path=Path("/dev/null"),
            previous_manifest=previous_manifest,
            workers=1,
            window_docs=1,
        )
    )

    assert "changed=1" in caplog.text
    assert "reused=0" in caplog.text
    assert "embed_calls=1" in caplog.text
    assert "tokens=456" in caplog.text
    assert "encode_batch_size=64 max_batch_tokens=8192" in caplog.text


def test_metadata_only_change_skips_re_embed(
    tiny_pages_dir: Path,
    tmp_path: Path,
    monkeypatch,
    caplog,
) -> None:
    """A doc whose chunks are unchanged but whose row metadata (title, status,
    withdrawn_date, ...) differs goes through the metadata-refresh branch:
    chunks + embeddings are carried byte-identically from the prior pack
    record, the embedding model is not invoked for this doc, and the new pack
    record carries the new title.
    """
    import ato_mcp.indexer.build as build_module
    import ato_mcp.indexer.rules as rules_module
    from ato_mcp.embed.model import EncodedBatch
    from ato_mcp.indexer.pack import read_record
    from ato_mcp.store import db as store_db
    from ato_mcp.store.manifest import load_manifest

    encode_calls: list[int] = []

    class StubModel:
        def __init__(self, *a, **kw) -> None:
            pass

        def encode(self, texts, *, is_query, batch_size: int = 16):
            texts_list = list(texts)
            encode_calls.append(len(texts_list))
            return EncodedBatch(
                vectors_int8=np.zeros(
                    (len(texts_list), store_db.EMBEDDING_DIM), dtype=np.int8,
                ),
                tokens_seen=len(texts_list),
            )

    monkeypatch.setattr(build_module, "EmbeddingModel", StubModel)

    prev_out = tmp_path / "previous"
    prev_manifest_path = prev_out / "manifest.json"
    build_module.build(
        build_module.BuildArgs(
            pages_dir=tiny_pages_dir,
            out_dir=prev_out,
            db_path=prev_out / "ato.db",
            model_id="stub",
            model_path=Path("/dev/null"),
            tokenizer_path=Path("/dev/null"),
            workers=1,
            window_docs=1,
        )
    )
    prev_manifest = load_manifest(prev_manifest_path)
    assert len(prev_manifest.documents) == 1
    prev_doc_ref = prev_manifest.documents[0]
    prev_pack_path = prev_out / "packs" / f"pack-{prev_doc_ref.pack_sha8}.bin.zst"
    prev_record = read_record(prev_pack_path, prev_doc_ref.offset, prev_doc_ref.length)
    prev_chunks = prev_record["chunks"]
    assert prev_chunks, "fixture must produce at least one chunk"
    prev_title = prev_record["title"]
    prev_embeddings = [c["embedding_b64"] for c in prev_chunks]

    encode_calls.clear()
    real_derive = rules_module.derive_metadata
    new_title = prev_title + " (renamed)"

    from dataclasses import replace as dc_replace

    def fake_derive_metadata(inputs):
        return dc_replace(real_derive(inputs), title=new_title)

    monkeypatch.setattr(build_module.rules_mod, "derive_metadata", fake_derive_metadata)

    logger = logging.getLogger("tests.embedding.metadata_refresh")
    logger.handlers.clear()
    logger.propagate = True
    monkeypatch.setattr(build_module, "LOGGER", logger)
    caplog.set_level(logging.INFO, logger=logger.name)

    out_dir = tmp_path / "refreshed"
    new_manifest = build_module.build(
        build_module.BuildArgs(
            pages_dir=tiny_pages_dir,
            out_dir=out_dir,
            db_path=out_dir / "ato.db",
            model_id="stub",
            model_path=Path("/dev/null"),
            tokenizer_path=Path("/dev/null"),
            previous_manifest=prev_manifest_path,
            workers=1,
            window_docs=1,
        )
    )

    assert encode_calls == [], (
        f"metadata-only change must not invoke the embedding model; got {encode_calls}"
    )
    assert "metadata_refreshed=1" in caplog.text
    assert "reused=0" in caplog.text
    assert "changed=0" in caplog.text

    assert len(new_manifest.documents) == 1
    new_ref = new_manifest.documents[0]
    new_pack_path = out_dir / "packs" / f"pack-{new_ref.pack_sha8}.bin.zst"
    new_record = read_record(new_pack_path, new_ref.offset, new_ref.length)

    assert new_record["title"] == new_title
    assert new_record["title"] != prev_title
    assert new_record["content_hash"] == prev_record["content_hash"]
    assert [c["embedding_b64"] for c in new_record["chunks"]] == prev_embeddings, (
        "metadata-refresh must carry chunk embeddings byte-identically from "
        "the previous pack record"
    )
    assert [c["text"] for c in new_record["chunks"]] == [c["text"] for c in prev_chunks]


def test_chunk_text_change_re_embeds(
    tiny_pages_dir: Path,
    tmp_path: Path,
    monkeypatch,
    caplog,
) -> None:
    """A doc whose chunk text changes (because its source HTML changed)
    falls into the full re-extract + re-embed branch — the existing
    incremental contract.
    """
    import ato_mcp.indexer.build as build_module
    from ato_mcp.embed.model import EncodedBatch
    from ato_mcp.store import db as store_db

    encode_calls: list[int] = []

    class StubModel:
        def __init__(self, *a, **kw) -> None:
            pass

        def encode(self, texts, *, is_query, batch_size: int = 16):
            texts_list = list(texts)
            encode_calls.append(len(texts_list))
            return EncodedBatch(
                vectors_int8=np.zeros(
                    (len(texts_list), store_db.EMBEDDING_DIM), dtype=np.int8,
                ),
                tokens_seen=len(texts_list),
            )

    monkeypatch.setattr(build_module, "EmbeddingModel", StubModel)

    prev_out = tmp_path / "previous"
    build_module.build(
        build_module.BuildArgs(
            pages_dir=tiny_pages_dir,
            out_dir=prev_out,
            db_path=prev_out / "ato.db",
            model_id="stub",
            model_path=Path("/dev/null"),
            tokenizer_path=Path("/dev/null"),
            workers=1,
            window_docs=1,
        )
    )

    encode_calls.clear()
    payload_path = next((tiny_pages_dir / "payloads").iterdir())
    payload_path.write_text(
        payload_path.read_text(encoding="utf-8").replace(
            "This is body text for the small telemetry build smoke.",
            "Replacement body text changes the chunk fingerprint.",
        ),
        encoding="utf-8",
    )

    logger = logging.getLogger("tests.embedding.body_change")
    logger.handlers.clear()
    logger.propagate = True
    monkeypatch.setattr(build_module, "LOGGER", logger)
    caplog.set_level(logging.INFO, logger=logger.name)

    out_dir = tmp_path / "rebuilt"
    build_module.build(
        build_module.BuildArgs(
            pages_dir=tiny_pages_dir,
            out_dir=out_dir,
            db_path=out_dir / "ato.db",
            model_id="stub",
            model_path=Path("/dev/null"),
            tokenizer_path=Path("/dev/null"),
            previous_manifest=prev_out / "manifest.json",
            workers=1,
            window_docs=1,
        )
    )

    assert encode_calls, "body change must trigger re-embedding"
    assert "changed=1" in caplog.text
    assert "metadata_refreshed=0" in caplog.text
    assert "reused=0" in caplog.text


def test_legacy_recipe_manifest_promotes_to_fast_path(
    tiny_pages_dir: Path,
    tmp_path: Path,
    monkeypatch,
    caplog,
) -> None:
    """A previous manifest whose stored content_hash was computed with the
    legacy recipe (which mixed title / doc_type / pub_date / status into the
    hash) still triggers Branches 1/2 when the actual chunks are unchanged.

    The bridge logic in build() recomputes the prev pack record's hash under
    the current recipe instead of trusting the stored manifest value, so a
    corpus built before the recipe change can still be used as a fast-path
    previous_manifest.
    """
    import hashlib

    import ato_mcp.indexer.build as build_module
    from ato_mcp.embed.model import EncodedBatch
    from ato_mcp.store import db as store_db
    from ato_mcp.store.manifest import load_manifest

    encode_calls: list[int] = []

    class StubModel:
        def __init__(self, *a, **kw) -> None:
            pass

        def encode(self, texts, *, is_query, batch_size: int = 16):
            texts_list = list(texts)
            encode_calls.append(len(texts_list))
            return EncodedBatch(
                vectors_int8=np.zeros(
                    (len(texts_list), store_db.EMBEDDING_DIM), dtype=np.int8,
                ),
                tokens_seen=len(texts_list),
            )

    monkeypatch.setattr(build_module, "EmbeddingModel", StubModel)

    prev_out = tmp_path / "previous"
    prev_manifest_path = prev_out / "manifest.json"
    build_module.build(
        build_module.BuildArgs(
            pages_dir=tiny_pages_dir,
            out_dir=prev_out,
            db_path=prev_out / "ato.db",
            model_id="stub",
            model_path=Path("/dev/null"),
            tokenizer_path=Path("/dev/null"),
            workers=1,
            window_docs=1,
        )
    )

    # Rewrite each DocRef's content_hash to a deliberately-different value
    # to simulate a manifest produced by the legacy recipe. We use a sha256
    # of (prev_hash + "legacy-marker") so the value differs from any hash
    # the current recipe could produce on these chunks.
    prev_manifest_data = json.loads(prev_manifest_path.read_text(encoding="utf-8"))
    assert prev_manifest_data["documents"], "fixture must produce at least one document"
    for doc in prev_manifest_data["documents"]:
        legacy_marker = (doc["content_hash"] + "legacy-marker").encode("utf-8")
        doc["content_hash"] = "sha256:" + hashlib.sha256(legacy_marker).hexdigest()
    prev_manifest_path.write_text(json.dumps(prev_manifest_data), encoding="utf-8")

    # Sanity: the rewrite stuck.
    rewritten = load_manifest(prev_manifest_path)
    assert all(
        d.content_hash.startswith("sha256:") for d in rewritten.documents
    )

    encode_calls.clear()
    logger = logging.getLogger("tests.embedding.legacy_bridge")
    logger.handlers.clear()
    logger.propagate = True
    monkeypatch.setattr(build_module, "LOGGER", logger)
    caplog.set_level(logging.INFO, logger=logger.name)

    out_dir = tmp_path / "rebuilt"
    new_manifest = build_module.build(
        build_module.BuildArgs(
            pages_dir=tiny_pages_dir,
            out_dir=out_dir,
            db_path=out_dir / "ato.db",
            model_id="stub",
            model_path=Path("/dev/null"),
            tokenizer_path=Path("/dev/null"),
            previous_manifest=prev_manifest_path,
            workers=1,
            window_docs=1,
        )
    )

    # Source HTML is unchanged, so no doc should fall into the re-embed path.
    assert encode_calls == [], (
        "legacy-recipe bridge must let unchanged chunks skip the embedding "
        f"model; got {encode_calls}"
    )
    assert "changed=0" in caplog.text
    # Either branch is acceptable: reused (full slot match) or
    # metadata_refreshed (chunks reused, only metadata re-derived).
    assert ("reused=1" in caplog.text) or ("metadata_refreshed=1" in caplog.text)

    # The freshly-emitted manifest must carry the new-recipe hash, not the
    # legacy marker we wrote above. Future incremental builds should then hit
    # the optimised fast-path (manifest hash already matches item hash).
    assert len(new_manifest.documents) == len(prev_manifest_data["documents"])
    for new_ref, legacy_doc in zip(new_manifest.documents, prev_manifest_data["documents"]):
        assert new_ref.content_hash != legacy_doc["content_hash"], (
            "new manifest must be promoted off the legacy recipe hash"
        )


@pytest.fixture()
def sample_pages_dir(tmp_path: Path) -> Path:
    if not (ATO_PAGES / "index.jsonl").exists():
        pytest.skip("ato_pages/ not present")

    sample_dir = tmp_path / "ato_pages_sample"
    sample_dir.mkdir()
    index_lines: list[str] = []
    count = 0
    with open(ATO_PAGES / "index.jsonl", "r", encoding="utf-8") as fh:
        for line in fh:
            rec = json.loads(line)
            if rec.get("status") != "success":
                continue
            payload_rel = rec.get("payload_path")
            if not payload_rel:
                continue
            src = ATO_PAGES / payload_rel
            if not src.exists():
                continue
            dest = sample_dir / payload_rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(src.read_text(encoding="utf-8", errors="replace"), encoding="utf-8")
            index_lines.append(json.dumps(rec))
            count += 1
            if count >= 5:
                break
    (sample_dir / "index.jsonl").write_text("\n".join(index_lines) + "\n", encoding="utf-8")
    return sample_dir


def test_build_small_index(sample_pages_dir: Path, tmp_path: Path, monkeypatch) -> None:
    from ato_mcp.indexer import build as build_mod  # noqa: F401 — keep module imported
    import ato_mcp.indexer.build as build_module
    from ato_mcp.store import db as store_db

    captured_texts: list[str] = []

    class StubModel:
        def __init__(self, *a, **kw) -> None:
            pass

        def encode(self, texts, *, is_query, batch_size: int = 16):
            from ato_mcp.embed.model import EncodedBatch
            texts_list = list(texts)
            captured_texts.extend(texts_list)
            n = len(texts_list)
            return EncodedBatch(
                vectors_int8=np.zeros((n, store_db.EMBEDDING_DIM), dtype=np.int8),
                tokens_seen=0,
            )

    monkeypatch.setattr(build_module, "EmbeddingModel", StubModel)

    out_dir = tmp_path / "release"
    db_path = out_dir / "ato.db"
    args = build_module.BuildArgs(
        pages_dir=sample_pages_dir,
        out_dir=out_dir,
        db_path=db_path,
        model_id="stub",
        model_path=Path("/dev/null"),
        tokenizer_path=Path("/dev/null"),
        model_sha256="0" * 64,
        model_size=0,
    )
    manifest = build_module.build(args)

    assert (out_dir / "manifest.json").exists()
    assert (out_dir / "update.json").exists()
    assert len(manifest.documents) >= 1
    assert len(manifest.packs) >= 1
    assert db_path.exists()

    conn = store_db.connect(db_path, mode="ro")
    try:
        row = conn.execute("SELECT COUNT(*) AS n FROM documents").fetchone()
        assert row["n"] == len(manifest.documents)
        row = conn.execute("SELECT COUNT(*) AS n FROM chunks").fetchone()
        assert row["n"] >= 0

        # W2.1: the embedder must receive title + heading_path + text, not
        # bare chunk text. We inspect at least one captured input string and
        # check that it carries the document title (which is also stored in
        # documents.title) somewhere in its prefix.
        if captured_texts:
            titles = {row[0] for row in conn.execute("SELECT title FROM documents").fetchall()}
            hits = sum(
                1
                for txt in captured_texts
                for title in titles
                if title and txt.startswith(title)
            )
            assert hits > 0, (
                "expected at least one embedder input to start with a stored document title; "
                f"first 3 captured: {captured_texts[:3]!r}"
            )
            headed_rows = conn.execute(
                """
                SELECT d.title, c.heading_path, c.text
                FROM chunks c
                JOIN documents d ON d.doc_id = c.doc_id
                WHERE c.heading_path <> ''
                LIMIT 20
                """
            ).fetchall()
            if headed_rows:
                expected = {
                    build_module._embedding_input(row["title"], row["heading_path"], row["text"])
                    for row in headed_rows
                }
                assert expected.intersection(captured_texts), (
                    "expected at least one embedder input to exactly preserve "
                    "title\\nheading_path\\ntext; "
                    f"expected sample: {list(expected)[:1]!r}, first 3 captured: {captured_texts[:3]!r}"
                )
    finally:
        conn.close()
