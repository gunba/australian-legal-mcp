"""Manifest schema + signature verification (minisign).

A manifest enumerates every document in a release and the pack-file byte range
it lives in. Clients diff the document content hash and pack byte range to
produce the delta work list.
"""
from __future__ import annotations

import hashlib
import shutil
import subprocess
from pathlib import Path
from typing import Optional

import orjson
from pydantic import BaseModel, Field

# Manifest format version. Bump alongside any binary release that adds new
# required fields the older binary doesn't tolerate.
MANIFEST_SCHEMA_VERSION = 4


def _binary_version_from_cargo_toml() -> str:
    """Read the runtime binary version from Cargo.toml.

    Couples corpus releases to the binary that built them: every
    build-index run stamps the manifest with the maintainer's current
    Cargo.toml version, and the Rust runtime rejects updates from older
    binaries. Falls back to a hardcoded constant if Cargo.toml can't be
    found (e.g. installed-from-wheel scenarios).
    """
    candidates = [
        Path(__file__).resolve().parents[3] / "Cargo.toml",
        Path.cwd() / "Cargo.toml",
    ]
    for cargo_toml in candidates:
        if not cargo_toml.is_file():
            continue
        for line in cargo_toml.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if not stripped.startswith("version"):
                continue
            _, _, value = stripped.partition("=")
            value = value.strip().strip('"')
            if value:
                return value
    return "0.7.0"


# `min_client_version` is read from Cargo.toml at build time so corpus
# releases are always pinned to the binary that built them. The Rust
# runtime rejects any manifest whose `min_client_version` exceeds the
# binary's `CARGO_PKG_VERSION` — agents setting up the MCP cannot pair
# an old binary with a newer corpus by accident.
DEFAULT_MIN_CLIENT_VERSION = _binary_version_from_cargo_toml()


class ModelInfo(BaseModel):
    id: str
    sha256: str
    size: int
    url: str
    # Optional sha256 of the companion `tokenizer.json`. Reranker manifests
    # must set this so the Rust HF download path verifies tokenizer.json to
    # the same checksum-pinned standard as the model file.
    tokenizer_sha256: str | None = None


class DocRef(BaseModel):
    # The five fields the Rust installer needs for diffing + fetching.
    doc_id: str
    content_hash: str
    pack_sha8: str
    offset: int
    length: int
    # Client-unused metadata. Kept on the model for build-side debugging
    # but excluded from serialization so produced manifests stay lean.
    type: str = Field(default="", exclude=True)
    title: str = Field(default="", exclude=True)


class PackInfo(BaseModel):
    sha8: str
    sha256: str
    size: int
    url: str


class Manifest(BaseModel):
    schema_version: int = MANIFEST_SCHEMA_VERSION
    index_version: str
    created_at: str
    min_client_version: str = DEFAULT_MIN_CLIENT_VERSION
    model: ModelInfo
    # Optional cross-encoder reranker bundle. Wave 3 (0.6.x) introduces a
    # local ONNX reranker that the Rust runtime applies to top-N hybrid
    # candidates. The field is optional: a release built without
    # ``--reranker-bundle`` leaves it as ``None`` and the runtime falls back
    # to the un-reranked hybrid score.
    reranker: Optional[ModelInfo] = None
    documents: list[DocRef] = Field(default_factory=list)
    packs: list[PackInfo] = Field(default_factory=list)

    def doc_index(self) -> dict[str, DocRef]:
        return {d.doc_id: d for d in self.documents}

    def pack_index(self) -> dict[str, PackInfo]:
        return {p.sha8: p for p in self.packs}

    def to_bytes(self) -> bytes:
        return orjson.dumps(self.model_dump(), option=orjson.OPT_SORT_KEYS | orjson.OPT_INDENT_2)


class UpdateSummary(BaseModel):
    schema_version: int
    index_version: str
    min_client_version: str
    model: ModelInfo
    reranker: Optional[ModelInfo] = None
    document_count: int
    pack_count: int
    manifest_fingerprint: str

    def to_bytes(self) -> bytes:
        return orjson.dumps(self.model_dump(), option=orjson.OPT_SORT_KEYS | orjson.OPT_INDENT_2)


def manifest_fingerprint(manifest: Manifest) -> str:
    payload = {
        "documents": [
            {
                "doc_id": d.doc_id,
                "content_hash": d.content_hash,
                "pack_sha8": d.pack_sha8,
                "offset": d.offset,
                "length": d.length,
            }
            for d in sorted(manifest.documents, key=lambda d: d.doc_id)
        ],
        "packs": [
            {
                "sha8": p.sha8,
                "sha256": p.sha256,
                "size": p.size,
                "url": p.url,
            }
            for p in sorted(manifest.packs, key=lambda p: p.sha8)
        ],
    }
    return hashlib.sha256(orjson.dumps(payload, option=orjson.OPT_SORT_KEYS)).hexdigest()


def update_summary_from_manifest(manifest: Manifest) -> UpdateSummary:
    return UpdateSummary(
        schema_version=manifest.schema_version,
        index_version=manifest.index_version,
        min_client_version=manifest.min_client_version,
        model=manifest.model,
        reranker=manifest.reranker,
        document_count=len(manifest.documents),
        pack_count=len(manifest.packs),
        manifest_fingerprint=manifest_fingerprint(manifest),
    )


def load_manifest(path: Path) -> Manifest:
    return Manifest.model_validate_json(Path(path).read_bytes())


def save_manifest(manifest: Manifest, path: Path) -> None:
    Path(path).write_bytes(manifest.to_bytes())


def save_update_summary(manifest: Manifest, path: Path) -> None:
    Path(path).write_bytes(update_summary_from_manifest(manifest).to_bytes())


def sha256_file(path: Path, chunk_size: int = 1 << 20) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        while chunk := fh.read(chunk_size):
            h.update(chunk)
    return h.hexdigest()


def verify_signature(manifest_path: Path, sig_path: Path, pubkey_path: Path) -> bool:
    """Verify the manifest.minisig signature.

    Returns True on success. Signature verification uses the ``minisign`` CLI
    so it exercises the same verifier maintainers use outside Python.
    """
    # [SL-07] Use the minisign CLI via subprocess (not a Python library) so the offline verifier path is exercised — signing-key hygiene problems surface early.
    cli = shutil.which("minisign")
    if cli is None:
        raise RuntimeError(
            "signature verification requested but the `minisign` CLI is not installed"
        )

    result = subprocess.run(
        [cli, "-V", "-m", str(manifest_path), "-x", str(sig_path), "-p", str(pubkey_path)],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise ValueError(f"manifest signature verification failed: {detail}")
    return True


def diff_manifests(
    old: Manifest | None, new: Manifest
) -> tuple[list[DocRef], list[DocRef], list[str]]:
    """Return (added, changed, removed_doc_ids)."""
    # [SL-08] Doc refs diff by content_hash plus pack slot so same-content repacks still hydrate updated pack-side fields.
    old_ix: dict[str, DocRef] = old.doc_index() if old else {}
    new_ix = new.doc_index()
    added: list[DocRef] = []
    changed: list[DocRef] = []
    for doc_id, ref in new_ix.items():
        if doc_id not in old_ix:
            added.append(ref)
        elif not doc_ref_matches(old_ix[doc_id], ref):
            changed.append(ref)
    removed = [doc_id for doc_id in old_ix if doc_id not in new_ix]
    return added, changed, removed


def doc_ref_matches(old: DocRef, new: DocRef) -> bool:
    return (
        old.content_hash == new.content_hash
        and old.pack_sha8 == new.pack_sha8
        and old.offset == new.offset
        and old.length == new.length
    )
