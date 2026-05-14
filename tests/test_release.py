"""Release helpers — URL rewrite only (gh CLI calls not exercised)."""
from __future__ import annotations

import json
from pathlib import Path
import subprocess

import pytest

from ato_mcp.indexer.release import (
    EMBEDDINGGEMMA_HF_FINGERPRINT,
    EMBEDDINGGEMMA_HF_SIZE,
    EMBEDDINGGEMMA_HF_URL,
    RERANKER_MODEL_PATH,
    ReleaseArgs,
    ReleaseError,
    _resolve_reranker_info,
    publish,
    rewrite_manifest_urls,
)
from ato_mcp.store.manifest import (
    DEFAULT_MIN_CLIENT_VERSION,
    DocRef,
    MANIFEST_SCHEMA_VERSION,
    Manifest,
    ModelInfo,
    PackInfo,
    load_manifest,
    save_manifest,
)


def test_freshly_built_manifest_pins_min_client_version() -> None:
    """A freshly-constructed Manifest defaults ``min_client_version`` to
    the Cargo.toml version, so older binaries refuse to ingest this corpus.

    The Rust enforce_manifest_compatibility check compares this field to
    ``CARGO_PKG_VERSION``; without this default the gate is dormant and a
    pre-HTML-surface binary would silently download a v4 corpus and only fail
    AFTER the install via the schema-version DB check.
    """
    import re
    cargo = (Path(__file__).resolve().parents[1] / "Cargo.toml").read_text()
    cargo_version = re.search(r'^version\s*=\s*"([^"]+)"', cargo, re.MULTILINE).group(1)
    manifest = Manifest(
        index_version="2026.05.03",
        created_at="2026-05-03T00:00:00+00:00",
        model=ModelInfo(id="m", sha256="0" * 64, size=1, url="model/m.onnx.zst"),
    )
    assert manifest.min_client_version == cargo_version
    assert DEFAULT_MIN_CLIENT_VERSION == cargo_version
    # And the manifest format version is bumped to v4 (HTML/assets boundary).
    assert manifest.schema_version == MANIFEST_SCHEMA_VERSION
    assert MANIFEST_SCHEMA_VERSION == 4


def test_rewrite_manifest_urls_flattens_asset_names(tmp_path: Path) -> None:
    manifest = Manifest(
        index_version="2026.04.18",
        created_at="2026-04-18T00:00:00+00:00",
        model=ModelInfo(id="m", sha256="0" * 64, size=1,
                        url="model/placeholder.onnx.zst"),
        documents=[DocRef(doc_id="d", content_hash="sha256:abc", pack_sha8="deadbeef",
                          offset=0, length=1, category="c", title="T")],
        packs=[PackInfo(sha8="deadbeef", sha256="0" * 64, size=1,
                       url="packs/pack-deadbeef.bin.zst")],
    )
    path = tmp_path / "manifest.json"
    save_manifest(manifest, path)

    rewrite_manifest_urls(path, repo="gunba/ato-mcp", tag="index-2026.04.18")

    out = load_manifest(path)
    # Model URL is managed by publish(), not by this helper.
    assert out.model.url == "model/placeholder.onnx.zst"
    assert out.packs[0].url == (
        "https://github.com/gunba/ato-mcp/releases/download/"
        "index-2026.04.18/pack-deadbeef.bin.zst"
    )


def test_bundle_model_round_trip(tmp_path: Path) -> None:
    """bundle_model + tarfile extract restores the original files byte-for-byte."""
    import tarfile
    import zstandard as zstd

    from ato_mcp.indexer.release import bundle_model

    model_dir = tmp_path / "model"
    (model_dir / "onnx").mkdir(parents=True)
    (model_dir / "onnx" / "model_quantized.onnx").write_bytes(b"ONNX\x00" * 50)
    (model_dir / "onnx" / "model_quantized.onnx_data").write_bytes(b"\x01\x02\x03" * 200)
    (model_dir / "tokenizer.json").write_text('{"tok":"json"}')

    bundle = tmp_path / "bundle.tar.zst"
    sha256, size = bundle_model(model_dir, bundle)
    assert size == bundle.stat().st_size
    assert len(sha256) == 64

    extract_dir = tmp_path / "extract"
    extract_dir.mkdir()
    with open(bundle, "rb") as fh:
        dctx = zstd.ZstdDecompressor()
        with dctx.stream_reader(fh) as reader, tarfile.open(fileobj=reader, mode="r|") as tar:
            tar.extractall(extract_dir, filter="data")

    expected_sources = {
        "model_quantized.onnx": model_dir / "onnx" / "model_quantized.onnx",
        "model_quantized.onnx_data": model_dir / "onnx" / "model_quantized.onnx_data",
        "tokenizer.json": model_dir / "tokenizer.json",
    }
    for name, source in expected_sources.items():
        assert (extract_dir / name).read_bytes() == source.read_bytes()


def test_publish_uses_external_model_url_without_uploading_bundle(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    out_dir = tmp_path / "release"
    packs_dir = out_dir / "packs"
    packs_dir.mkdir(parents=True)
    pack = packs_dir / "pack-deadbeef.bin.zst"
    pack.write_bytes(b"pack")
    save_manifest(
        Manifest(
            index_version="2026.04.18",
            created_at="2026-04-18T00:00:00+00:00",
            model=ModelInfo(
                id="embeddinggemma-300m-int8-256d",
                sha256="",
                size=0,
                url="model/embeddinggemma-300m-int8-256d.onnx.zst",
            ),
            documents=[],
            packs=[PackInfo(sha8="deadbeef", sha256="0" * 64, size=4, url=str(pack))],
        ),
        out_dir / "manifest.json",
    )
    model_dir = tmp_path / "model"
    (model_dir / "onnx").mkdir(parents=True)
    (model_dir / "onnx" / "model_quantized.onnx").write_bytes(b"ONNX\x00" * 50)
    (model_dir / "onnx" / "model_quantized.onnx_data").write_bytes(b"\x01\x02\x03" * 200)
    (model_dir / "tokenizer.json").write_text('{"tok":"json"}')

    commands: list[list[str]] = []

    def fake_run(cmd, **kwargs):  # type: ignore[no-untyped-def]
        commands.append([str(part) for part in cmd])
        return subprocess.CompletedProcess(cmd, 0, "", "")

    monkeypatch.setattr(subprocess, "run", fake_run)

    publish(
        ReleaseArgs(
            out_dir=out_dir,
            tag="index-2026.04.18",
            repo="gunba/ato-mcp",
            model_dir=model_dir,
            model_url="https://models.example.internal/ato-mcp/embeddinggemma-bundle.tar.zst",
            overwrite=True,
        )
    )

    out = load_manifest(out_dir / "manifest.json")
    assert out.model.url == "https://models.example.internal/ato-mcp/embeddinggemma-bundle.tar.zst"
    assert len(out.model.sha256) == 64
    assert out.model.size > 0
    summary = json.loads((out_dir / "update.json").read_text())
    assert summary["model"]["url"] == out.model.url
    assert summary["document_count"] == 0
    assert "documents" not in summary

    upload = next(cmd for cmd in commands if "upload" in cmd)
    assert "manifest.json" in " ".join(upload)
    assert "update.json" in " ".join(upload)
    assert "pack-deadbeef.bin.zst" in " ".join(upload)
    assert "embeddinggemma-bundle.tar.zst" not in " ".join(upload)


def test_publish_defaults_to_pinned_huggingface_model_source(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    out_dir = tmp_path / "release"
    packs_dir = out_dir / "packs"
    packs_dir.mkdir(parents=True)
    pack = packs_dir / "pack-deadbeef.bin.zst"
    pack.write_bytes(b"pack")
    save_manifest(
        Manifest(
            index_version="2026.04.18",
            created_at="2026-04-18T00:00:00+00:00",
            model=ModelInfo(
                id="embeddinggemma-300m-int8-256d",
                sha256="",
                size=0,
                url="model/embeddinggemma-300m-int8-256d.onnx.zst",
            ),
            documents=[],
            packs=[PackInfo(sha8="deadbeef", sha256="0" * 64, size=4, url=str(pack))],
        ),
        out_dir / "manifest.json",
    )

    commands: list[list[str]] = []

    def fake_run(cmd, **kwargs):  # type: ignore[no-untyped-def]
        commands.append([str(part) for part in cmd])
        return subprocess.CompletedProcess(cmd, 0, "", "")

    monkeypatch.setattr(subprocess, "run", fake_run)

    publish(
        ReleaseArgs(
            out_dir=out_dir,
            tag="index-2026.04.18",
            repo="gunba/ato-mcp",
            overwrite=True,
        )
    )

    out = load_manifest(out_dir / "manifest.json")
    assert out.model.url == EMBEDDINGGEMMA_HF_URL
    assert out.model.sha256 == EMBEDDINGGEMMA_HF_FINGERPRINT
    assert out.model.size == EMBEDDINGGEMMA_HF_SIZE

    upload = next(cmd for cmd in commands if "upload" in cmd)
    assert "manifest.json" in " ".join(upload)
    assert "update.json" in " ".join(upload)
    assert "pack-deadbeef.bin.zst" in " ".join(upload)
    assert "embeddinggemma-bundle.tar.zst" not in " ".join(upload)


def test_publish_rejects_github_model_url(tmp_path: Path) -> None:
    out_dir = tmp_path / "release"
    packs_dir = out_dir / "packs"
    packs_dir.mkdir(parents=True)
    pack = packs_dir / "pack-deadbeef.bin.zst"
    pack.write_bytes(b"pack")
    save_manifest(
        Manifest(
            index_version="2026.04.18",
            created_at="2026-04-18T00:00:00+00:00",
            model=ModelInfo(
                id="embeddinggemma-300m-int8-256d",
                sha256="0" * 64,
                size=1,
                url="model/embeddinggemma-300m-int8-256d.onnx.zst",
            ),
            documents=[],
            packs=[PackInfo(sha8="deadbeef", sha256="0" * 64, size=4, url=str(pack))],
        ),
        out_dir / "manifest.json",
    )

    with pytest.raises(ReleaseError, match="must not be hosted on GitHub"):
        publish(
            ReleaseArgs(
                out_dir=out_dir,
                tag="index-2026.04.18",
                repo="gunba/ato-mcp",
                model_url="https://github.com/gunba/ato-mcp/releases/download/index-2026.04.18/embeddinggemma-bundle.tar.zst",
            )
        )


def _seed_release_dir(tmp_path: Path) -> tuple[Path, Path]:
    """Set up an `out_dir` containing the minimum needed to call publish().

    Returns (out_dir, manifest_path).
    """
    out_dir = tmp_path / "release"
    packs_dir = out_dir / "packs"
    packs_dir.mkdir(parents=True)
    pack = packs_dir / "pack-deadbeef.bin.zst"
    pack.write_bytes(b"pack")
    manifest_path = out_dir / "manifest.json"
    save_manifest(
        Manifest(
            index_version="2026.05.03",
            created_at="2026-05-03T00:00:00+00:00",
            model=ModelInfo(
                id="embeddinggemma-300m-int8-256d",
                sha256="",
                size=0,
                url="model/embeddinggemma-300m-int8-256d.onnx.zst",
            ),
            documents=[],
            packs=[PackInfo(sha8="deadbeef", sha256="0" * 64, size=4, url=str(pack))],
        ),
        manifest_path,
    )
    return out_dir, manifest_path


def test_release_cli_accepts_reranker_bundle(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Passing `--reranker-bundle` populates the manifest's `reranker` field
    with a sha256/size derived from the bundle's `onnx/model_quantized.onnx`.

    The bundle itself is NOT uploaded to GitHub (only its fingerprint goes
    into the manifest), and the reranker URL records the external HF mirror
    the Rust runtime fetches from.
    """
    out_dir, manifest_path = _seed_release_dir(tmp_path)

    bundle = tmp_path / "reranker_bundle"
    (bundle / Path(RERANKER_MODEL_PATH).parent).mkdir(parents=True)
    onnx_bytes = b"ONNX" + b"\x00" * 1024
    (bundle / RERANKER_MODEL_PATH).write_bytes(onnx_bytes)
    (bundle / "tokenizer.json").write_text('{"tok":"json"}')
    (bundle / "config.json").write_text('{"model_type":"bert"}')

    commands: list[list[str]] = []

    def fake_run(cmd, **kwargs):  # type: ignore[no-untyped-def]
        commands.append([str(part) for part in cmd])
        return subprocess.CompletedProcess(cmd, 0, "", "")

    monkeypatch.setattr(subprocess, "run", fake_run)

    publish(
        ReleaseArgs(
            out_dir=out_dir,
            tag="index-2026.05.03",
            repo="gunba/ato-mcp",
            reranker_bundle=bundle,
            reranker_url="hf://Alibaba-NLP/gte-reranker-modernbert-base@deadbeef",
            overwrite=True,
        )
    )

    out = load_manifest(manifest_path)
    assert out.reranker is not None
    # Bundle sha256 + size were computed from onnx/model_quantized.onnx.
    import hashlib as _hashlib
    expected_sha = _hashlib.sha256(onnx_bytes).hexdigest()
    assert out.reranker.sha256 == expected_sha
    assert out.reranker.size == len(onnx_bytes)
    assert out.reranker.url == "hf://Alibaba-NLP/gte-reranker-modernbert-base@deadbeef"
    assert out.reranker.id == "gte-reranker-modernbert-base-quantized"

    # Bundle bytes never make it to gh upload — the Rust runtime fetches
    # them from the HF URL on first use.
    upload = next(cmd for cmd in commands if "upload" in cmd)
    joined = " ".join(upload)
    assert "update.json" in joined
    assert "model_quantized.onnx" not in joined
    assert "reranker_bundle" not in joined


def test_release_cli_without_reranker_leaves_field_none(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A release without any --reranker-* flag publishes a manifest whose
    `reranker` is None — the runtime falls back to the un-reranked hybrid
    score in that case."""
    out_dir, manifest_path = _seed_release_dir(tmp_path)

    def fake_run(cmd, **kwargs):  # type: ignore[no-untyped-def]
        return subprocess.CompletedProcess(cmd, 0, "", "")

    monkeypatch.setattr(subprocess, "run", fake_run)

    publish(
        ReleaseArgs(
            out_dir=out_dir,
            tag="index-2026.05.03",
            repo="gunba/ato-mcp",
            overwrite=True,
        )
    )

    out = load_manifest(manifest_path)
    assert out.reranker is None


def test_release_cli_rejects_github_reranker_url(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Reranker URLs hosted on GitHub Releases are rejected — Wave 3 pins
    the reranker source to Hugging Face the same way EmbeddingGemma is
    pinned."""
    out_dir, _ = _seed_release_dir(tmp_path)

    bundle = tmp_path / "reranker_bundle"
    bundle.mkdir()
    (bundle / Path(RERANKER_MODEL_PATH).parent).mkdir(parents=True)
    (bundle / RERANKER_MODEL_PATH).write_bytes(b"ONNX")

    def fake_run(cmd, **kwargs):  # type: ignore[no-untyped-def]
        return subprocess.CompletedProcess(cmd, 0, "", "")

    monkeypatch.setattr(subprocess, "run", fake_run)

    with pytest.raises(ReleaseError, match="must not be hosted on GitHub"):
        publish(
            ReleaseArgs(
                out_dir=out_dir,
                tag="index-2026.05.03",
                repo="gunba/ato-mcp",
                reranker_bundle=bundle,
                reranker_url="https://github.com/gunba/ato-mcp/releases/download/x/reranker.onnx",
            )
        )


def test_resolve_reranker_info_uses_canonical_model_filename(
    tmp_path: Path,
) -> None:
    """The release helper hashes the same canonical ONNX file that Rust downloads."""
    import hashlib as _hashlib

    bundle = tmp_path / "bundle"
    (bundle / Path(RERANKER_MODEL_PATH).parent).mkdir(parents=True, exist_ok=True)
    onnx_bytes = b"ONNX-canonical-payload"
    onnx_file = bundle / RERANKER_MODEL_PATH
    onnx_file.write_bytes(onnx_bytes)
    # Tokenizer is part of the C4 path; populate it so the auto-derived
    # sha lights up in the same call.
    tokenizer_bytes = b'{"tok":"json"}'
    (bundle / "tokenizer.json").write_bytes(tokenizer_bytes)

    args = ReleaseArgs(
        out_dir=tmp_path / "out",
        tag="index-test",
        reranker_bundle=bundle,
        reranker_url="hf://test/test@abc",
    )
    info = _resolve_reranker_info(args, current_reranker=None)
    assert info is not None
    expected_sha = _hashlib.sha256(onnx_bytes).hexdigest()
    assert info.sha256 == expected_sha
    assert info.size == len(onnx_bytes)
    # C4: tokenizer sha auto-populates from the bundle.
    assert info.tokenizer_sha256 == _hashlib.sha256(tokenizer_bytes).hexdigest()


def test_resolve_reranker_info_rejects_bundle_without_recognised_onnx(
    tmp_path: Path,
) -> None:
    """A bundle without the canonical ONNX path surfaces a clear error."""
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    (bundle / "tokenizer.json").write_bytes(b"{}")
    (bundle / "config.json").write_bytes(b"{}")

    args = ReleaseArgs(
        out_dir=tmp_path / "out",
        tag="index-test",
        reranker_bundle=bundle,
        reranker_url="hf://test/test@abc",
    )
    with pytest.raises(ReleaseError, match="missing onnx/model_quantized.onnx"):
        _resolve_reranker_info(args, current_reranker=None)


def test_resolve_reranker_info_threads_explicit_tokenizer_sha(
    tmp_path: Path,
) -> None:
    """`--reranker-tokenizer-sha256` overrides the auto-derived value."""
    import hashlib as _hashlib

    bundle = tmp_path / "bundle"
    (bundle / Path(RERANKER_MODEL_PATH).parent).mkdir(parents=True)
    (bundle / RERANKER_MODEL_PATH).write_bytes(b"ONNX")
    (bundle / "tokenizer.json").write_bytes(b'{"tok":"json"}')

    explicit = "f" * 64
    args = ReleaseArgs(
        out_dir=tmp_path / "out",
        tag="index-test",
        reranker_bundle=bundle,
        reranker_url="hf://test/test@abc",
        reranker_tokenizer_sha256=explicit,
    )
    info = _resolve_reranker_info(args, current_reranker=None)
    assert info is not None
    # Explicit override wins over the bundle's tokenizer.json.
    assert info.tokenizer_sha256 == explicit
    # And the model sha still hashes the bundle's onnx.
    assert info.sha256 == _hashlib.sha256(b"ONNX").hexdigest()
