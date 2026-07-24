#!/usr/bin/env python3
"""Content-addressed Azure transport for immutable legal-mcp generations.

A generation is split at fixed 64 MiB, SQLite-page-aligned boundaries. Chunks
are named by SHA-256 and shared by every generation. Upload therefore sends
only chunks Azure Blob Storage does not already have. Restore reconstructs an
`incoming/<generation>` directory, optionally CoW-cloning the active generation
first and overwriting only changed chunks.

The canonical `generation.json` and `legal-mcp activate` validation remain the
authority. This transport is only a resumable, deduplicating byte carrier.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import dataclasses
import email.utils
import fcntl
import hashlib
import json
import os
import pathlib
import re
import shutil
import stat
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from typing import BinaryIO

FORMAT = "australian-legal-mcp-chunk-transport"
FORMAT_VERSION = 1
GENERATION_SCHEMA_VERSION = 12
CHUNK_SIZE = 64 * 1024 * 1024
CHUNK_ENCODING = "zstd-v1"
MIN_FREE_MARGIN = 5 * 1024 * 1024 * 1024
MAX_MANIFEST_BYTES = 2 * 1024 * 1024
MAX_GENERATION_FILE_BYTES = 64 * 1024 * 1024 * 1024
MAX_GENERATION_BYTES = 128 * 1024 * 1024 * 1024
MAX_GENERATION_CHUNKS = 4096
MAX_WORKERS = 8
AZURE_STORAGE_RESOURCE = "https://storage.azure.com/"
AZURE_API_VERSION = "2023-11-03"
IMDS_TOKEN_URL = (
    "http://169.254.169.254/metadata/identity/oauth2/token"
    "?api-version=2018-02-01&resource=https%3A%2F%2Fstorage.azure.com%2F"
)
GENERATION_RE = re.compile(r"^[0-9a-f]{64}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
AZURE_BLOB_HOST_RE = re.compile(r"^[a-z0-9]{3,24}\.blob\.core\.windows\.net$")
SOURCE_IDS = (
    "ato",
    "federal-court",
    "frl",
    "high-court",
    "nsw-caselaw",
    "nsw-legislation",
    "qld-legislation",
    "sa-legislation",
    "tas-legislation",
    "wa-legislation",
)
EXPECTED_PATHS = {
    "generation.json",
    "legal.db",
    "model.onnx",
    "tokenizer.json",
    *(f"ann/{source}.ann" for source in SOURCE_IDS),
    *(f"lexical/{source}.db" for source in SOURCE_IDS),
}
FICLONE = 0x40049409


class TransportError(RuntimeError):
    pass


@dataclasses.dataclass(frozen=True)
class Chunk:
    sha256: str
    size: int
    offset: int


@dataclasses.dataclass(frozen=True)
class FileEntry:
    path: str
    size: int
    sha256: str
    chunks: tuple[Chunk, ...]


@dataclasses.dataclass(frozen=True)
class TransportManifest:
    generation_id: str
    chunk_size: int
    chunk_encoding: str
    generation_manifest_sha256: str
    files: tuple[FileEntry, ...]

    def to_dict(self) -> dict[str, object]:
        return {
            "format": FORMAT,
            "format_version": FORMAT_VERSION,
            "generation_id": self.generation_id,
            "chunk_size": self.chunk_size,
            "chunk_encoding": self.chunk_encoding,
            "generation_manifest_sha256": self.generation_manifest_sha256,
            "files": [
                {
                    "path": entry.path,
                    "size": entry.size,
                    "sha256": entry.sha256,
                    "chunks": [
                        {
                            "offset": chunk.offset,
                            "size": chunk.size,
                            "sha256": chunk.sha256,
                        }
                        for chunk in entry.chunks
                    ],
                }
                for entry in self.files
            ],
        }

    def bytes(self) -> bytes:
        return (
            json.dumps(self.to_dict(), sort_keys=True, separators=(",", ":")) + "\n"
        ).encode("utf-8")


@dataclasses.dataclass(frozen=True)
class ChunkLocation:
    source: pathlib.Path
    offset: int
    size: int


@dataclasses.dataclass(frozen=True)
class RestoreTarget:
    destination: pathlib.Path
    offset: int
    size: int


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: ANN001
        raise urllib.error.HTTPError(req.full_url, code, msg, headers, fp)


class TokenProvider:
    def __init__(self, mode: str):
        self.mode = mode
        self._lock = threading.Lock()
        self._token: str | None = None
        self._expires_at = 0

    def get(self, *, force: bool = False) -> str:
        with self._lock:
            now = int(time.time())
            if not force and self._token and now + 300 < self._expires_at:
                return self._token
            if self.mode == "azure-cli":
                result = subprocess.run(
                    [
                        "az",
                        "account",
                        "get-access-token",
                        "--resource",
                        AZURE_STORAGE_RESOURCE,
                        "--output",
                        "json",
                    ],
                    check=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                payload = json.loads(result.stdout)
                token = payload.get("accessToken")
                expires = payload.get("expires_on") or payload.get("expiresOn")
                try:
                    expires_at = int(expires)
                except (TypeError, ValueError):
                    expires_at = now + 45 * 60
            elif self.mode == "managed-identity":
                request = urllib.request.Request(
                    IMDS_TOKEN_URL,
                    headers={"Metadata": "true", "Accept": "application/json"},
                )
                with urllib.request.urlopen(request, timeout=10) as response:
                    payload = json.load(response)
                token = payload.get("access_token")
                try:
                    expires_at = int(payload.get("expires_on"))
                except (TypeError, ValueError):
                    expires_at = now + 45 * 60
            else:
                raise TransportError(f"unsupported token mode: {self.mode}")
            if not isinstance(token, str) or not token:
                raise TransportError("Azure credential source returned no access token")
            self._token = token
            self._expires_at = expires_at
            return token


class BlobStore:
    def get(self, name: str, *, maximum: int | None = None) -> bytes:
        raise NotImplementedError

    def put_immutable(
        self, name: str, data: bytes, *, sha256: str, tier: str | None = None
    ) -> bool:
        """Return True when created, False when identical content existed."""
        raise NotImplementedError

    def list_names(self, prefix: str) -> set[str]:
        raise NotImplementedError


class FileBlobStore(BlobStore):
    def __init__(self, root: pathlib.Path):
        self.root = root.resolve()
        self.root.mkdir(parents=True, exist_ok=True)

    def _path(self, name: str) -> pathlib.Path:
        validate_blob_name(name)
        path = self.root.joinpath(*name.split("/"))
        if self.root not in path.resolve().parents:
            raise TransportError("blob path escaped file store")
        return path

    def get(self, name: str, *, maximum: int | None = None) -> bytes:
        path = self._path(name)
        data = path.read_bytes()
        if maximum is not None and len(data) > maximum:
            raise TransportError(f"blob exceeds size limit: {name}")
        return data

    def put_immutable(
        self, name: str, data: bytes, *, sha256: str, tier: str | None = None
    ) -> bool:
        del tier
        if hashlib.sha256(data).hexdigest() != sha256:
            raise TransportError(f"refusing to upload mis-hashed blob: {name}")
        path = self._path(name)
        path.parent.mkdir(parents=True, exist_ok=True)
        try:
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o440)
        except FileExistsError:
            existing = path.read_bytes()
            if existing != data:
                raise TransportError(f"immutable blob collision: {name}")
            return False
        try:
            with os.fdopen(fd, "wb") as handle:
                handle.write(data)
                handle.flush()
                os.fsync(handle.fileno())
        except BaseException:
            path.unlink(missing_ok=True)
            raise
        fsync_directory(path.parent)
        return True

    def list_names(self, prefix: str) -> set[str]:
        validate_blob_name(prefix.rstrip("/"))
        names: set[str] = set()
        for path in self.root.rglob("*"):
            if path.is_file():
                name = path.relative_to(self.root).as_posix()
                if name.startswith(prefix):
                    names.add(name)
        return names


class AzureBlobStore(BlobStore):
    def __init__(self, base_url: str, token_mode: str):
        parsed = validate_azure_blob_base(base_url)
        self.base_url = base_url.rstrip("/")
        self.container_url = urllib.parse.urlunsplit(
            (parsed.scheme, parsed.netloc, parsed.path, "", "")
        )
        self.tokens = TokenProvider(token_mode)
        self.opener = urllib.request.build_opener(NoRedirect())

    def _url(self, name: str) -> str:
        validate_blob_name(name)
        encoded = "/".join(urllib.parse.quote(part, safe="") for part in name.split("/"))
        return f"{self.base_url}/{encoded}"

    def _request(
        self,
        method: str,
        url: str,
        *,
        data: bytes | None = None,
        headers: dict[str, str] | None = None,
        maximum: int | None = None,
        allowed: set[int] | None = None,
    ) -> tuple[int, bytes, dict[str, str]]:
        allowed = allowed or {200, 201, 202}
        last_error: BaseException | None = None
        for attempt in range(6):
            request_headers = {
                "Authorization": f"Bearer {self.tokens.get(force=attempt > 0 and isinstance(last_error, urllib.error.HTTPError) and last_error.code == 401)}",
                "x-ms-version": AZURE_API_VERSION,
                "x-ms-date": email.utils.formatdate(usegmt=True),
                "Accept": "application/json, application/xml, */*",
            }
            if headers:
                request_headers.update(headers)
            request = urllib.request.Request(
                url, data=data, headers=request_headers, method=method
            )
            try:
                with self.opener.open(request, timeout=120) as response:
                    body = read_bounded(response, maximum)
                    status = response.status
                    response_headers = {
                        key.lower(): value for key, value in response.headers.items()
                    }
                if status not in allowed:
                    raise TransportError(f"Azure Blob returned HTTP {status} for {method}")
                return status, body, response_headers
            except urllib.error.HTTPError as error:
                if error.code in allowed:
                    try:
                        return error.code, read_bounded(error, maximum), {
                            key.lower(): value for key, value in error.headers.items()
                        }
                    finally:
                        error.close()
                last_error = error
                retryable = error.code in {401, 408, 429, 500, 502, 503, 504}
                delay = retry_delay(error.headers, attempt) if retryable else 0
                error.close()
                if not retryable:
                    break
            except (TimeoutError, urllib.error.URLError) as error:
                last_error = error
                delay = min(2**attempt, 30)
            if attempt < 5:
                time.sleep(delay)
        raise TransportError(f"Azure Blob request failed: {method} {url}: {last_error}")

    def get(self, name: str, *, maximum: int | None = None) -> bytes:
        _, body, _ = self._request("GET", self._url(name), maximum=maximum)
        return body

    def put_immutable(
        self, name: str, data: bytes, *, sha256: str, tier: str | None = None
    ) -> bool:
        if hashlib.sha256(data).hexdigest() != sha256:
            raise TransportError(f"refusing to upload mis-hashed blob: {name}")
        headers = {
            "Content-Type": "application/octet-stream",
            "Content-Length": str(len(data)),
            "If-None-Match": "*",
            "x-ms-blob-type": "BlockBlob",
            "x-ms-meta-sha256": sha256,
        }
        if tier:
            headers["x-ms-access-tier"] = tier
        status, _, _ = self._request(
            "PUT",
            self._url(name),
            data=data,
            headers=headers,
            allowed={201, 412},
        )
        if status == 201:
            return True
        existing = self.get(name, maximum=len(data) + 1)
        if existing != data:
            raise TransportError(f"immutable blob collision: {name}")
        return False

    def list_names(self, prefix: str) -> set[str]:
        validate_blob_name(prefix.rstrip("/"))
        names: set[str] = set()
        marker = ""
        while True:
            query = urllib.parse.urlencode(
                {
                    "restype": "container",
                    "comp": "list",
                    "prefix": prefix,
                    "maxresults": "5000",
                    "marker": marker,
                }
            )
            _, body, _ = self._request(
                "GET", f"{self.container_url}?{query}", maximum=16 * 1024 * 1024
            )
            root = ET.fromstring(body)
            for element in root.findall("./Blobs/Blob/Name"):
                if element.text:
                    validate_blob_name(element.text)
                    names.add(element.text)
            marker = root.findtext("./NextMarker") or ""
            if not marker:
                break
        return names


def retry_delay(headers, attempt: int) -> float:  # noqa: ANN001
    milliseconds = headers.get("x-ms-retry-after-ms") if headers else None
    if milliseconds and milliseconds.isdigit():
        return min(int(milliseconds) / 1000.0, 60.0)
    retry_after = headers.get("Retry-After") if headers else None
    if retry_after and retry_after.isdigit():
        return min(float(retry_after), 60.0)
    return float(min(2**attempt, 30))


def read_bounded(handle: BinaryIO, maximum: int | None) -> bytes:
    if maximum is None:
        return handle.read()
    data = handle.read(maximum + 1)
    if len(data) > maximum:
        raise TransportError("HTTP response exceeded size limit")
    return data


def validate_blob_name(name: str) -> None:
    if not name or name.startswith("/") or "\\" in name:
        raise TransportError(f"invalid blob name: {name!r}")
    parts = name.split("/")
    if any(part in {"", ".", ".."} for part in parts):
        raise TransportError(f"invalid blob name: {name!r}")
    if any(any(ord(character) < 0x20 for character in part) for part in parts):
        raise TransportError(f"invalid blob name: {name!r}")


def validate_azure_blob_base(value: str) -> urllib.parse.SplitResult:
    parsed = urllib.parse.urlsplit(value)
    if (
        parsed.scheme != "https"
        or not parsed.hostname
        or not AZURE_BLOB_HOST_RE.fullmatch(parsed.hostname)
        or parsed.username is not None
        or parsed.password is not None
        or parsed.port is not None
        or parsed.query
        or parsed.fragment
        or len(parsed.path.split("/")) != 2
        or not parsed.path[1:]
        or not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{1,61}[a-z0-9])?", parsed.path[1:])
        or value.endswith("/")
    ):
        raise TransportError(
            "Azure Blob destination must be canonical "
            "https://ACCOUNT.blob.core.windows.net/CONTAINER"
        )
    return parsed


def blob_store(value: str, token_mode: str) -> BlobStore:
    parsed = urllib.parse.urlsplit(value)
    if parsed.scheme == "file":
        if parsed.netloc not in {"", "localhost"} or not parsed.path:
            raise TransportError("file Blob store must be an absolute file:/// path")
        return FileBlobStore(pathlib.Path(urllib.parse.unquote(parsed.path)))
    return AzureBlobStore(value, token_mode)


def validate_generation_id(value: str) -> str:
    if not GENERATION_RE.fullmatch(value):
        raise TransportError("generation ID must be 64 lowercase hexadecimal characters")
    return value


def regular_file(path: pathlib.Path, *, expected_size: int | None = None) -> os.stat_result:
    try:
        result = path.lstat()
    except FileNotFoundError as error:
        raise TransportError(f"missing generation file: {path}") from error
    if not stat.S_ISREG(result.st_mode) or result.st_nlink != 1:
        raise TransportError(f"generation path must be a non-hard-linked regular file: {path}")
    if expected_size is not None and result.st_size != expected_size:
        raise TransportError(f"generation file size mismatch: {path}")
    return result


def expected_generation_files(generation_dir: pathlib.Path) -> dict[str, tuple[int, str]]:
    manifest_path = generation_dir / "generation.json"
    regular_file(manifest_path)
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise TransportError(f"invalid generation.json: {error}") from error
    try:
        if manifest["schema_version"] != GENERATION_SCHEMA_VERSION:
            raise TransportError("generation manifest schema is not supported")
        files = {
            str(manifest["db"]["path"]): (
                int(manifest["db"]["size"]),
                str(manifest["db"]["sha256"]),
            ),
            str(manifest["model"]["model"]["path"]): (
                int(manifest["model"]["model"]["size"]),
                str(manifest["model"]["model"]["sha256"]),
            ),
            str(manifest["model"]["tokenizer"]["path"]): (
                int(manifest["model"]["tokenizer"]["size"]),
                str(manifest["model"]["tokenizer"]["sha256"]),
            ),
        }
        ann = manifest["ann"]
        if set(ann) != set(SOURCE_IDS):
            raise TransportError("generation manifest source set is not exact")
        for source in SOURCE_IDS:
            info = ann[source]
            if info["source_id"] != source or info["path"] != f"ann/{source}.ann":
                raise TransportError("generation manifest ANN source binding is not exact")
            files[str(info["path"])] = (int(info["size"]), str(info["sha256"]))
        lexical = manifest["lexical"]
        if set(lexical) != set(SOURCE_IDS):
            raise TransportError("generation manifest lexical source set is not exact")
        for source in SOURCE_IDS:
            info = lexical[source]
            if (
                info["source_id"] != source
                or info["path"] != f"lexical/{source}.db"
            ):
                raise TransportError(
                    "generation manifest lexical source binding is not exact"
                )
            files[str(info["path"])] = (int(info["size"]), str(info["sha256"]))
    except (KeyError, TypeError, ValueError) as error:
        raise TransportError("generation manifest file metadata is malformed") from error
    if set(files) != EXPECTED_PATHS - {"generation.json"}:
        raise TransportError(f"generation manifest file set is not exact: {set(files)!r}")
    for path, (size, digest) in files.items():
        if path not in EXPECTED_PATHS or size <= 0 or not SHA256_RE.fullmatch(digest):
            raise TransportError(f"invalid generation file metadata: {path}")
    generation_bytes = manifest_path.read_bytes()
    files["generation.json"] = (
        len(generation_bytes),
        hashlib.sha256(generation_bytes).hexdigest(),
    )
    return files


def validate_source_generation_root(
    generation_dir: pathlib.Path,
) -> tuple[pathlib.Path, str]:
    try:
        metadata = generation_dir.lstat()
    except OSError as error:
        raise TransportError(f"cannot inspect source generation: {generation_dir}") from error
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise TransportError("source generation path must be a real non-symlink directory")
    generation_id = validate_generation_id(generation_dir.name)

    # Reject anything outside the immutable generation contract before reading
    # generation.json or resolving the caller-supplied root.
    validate_no_unexpected_entries(generation_dir)
    try:
        resolved = generation_dir.resolve(strict=True)
        resolved_metadata = resolved.lstat()
    except OSError as error:
        raise TransportError("source generation changed while resolving its root") from error
    if (
        stat.S_ISLNK(resolved_metadata.st_mode)
        or not stat.S_ISDIR(resolved_metadata.st_mode)
        or (metadata.st_dev, metadata.st_ino)
        != (resolved_metadata.st_dev, resolved_metadata.st_ino)
    ):
        raise TransportError("source generation root changed while it was validated")
    validate_no_unexpected_entries(resolved)
    return resolved, generation_id


def build_manifest(generation_dir: pathlib.Path) -> tuple[TransportManifest, dict[str, ChunkLocation]]:
    generation_dir, generation_id = validate_source_generation_root(generation_dir)
    expected = expected_generation_files(generation_dir)
    entries: list[FileEntry] = []
    unique_chunks: dict[str, ChunkLocation] = {}
    for relative in sorted(expected):
        expected_size, expected_sha = expected[relative]
        source = generation_dir.joinpath(*relative.split("/"))
        regular_file(source, expected_size=expected_size)
        whole = hashlib.sha256()
        chunks: list[Chunk] = []
        offset = 0
        with source.open("rb", buffering=0) as handle:
            while offset < expected_size:
                size = min(CHUNK_SIZE, expected_size - offset)
                data = handle.read(size)
                if len(data) != size:
                    raise TransportError(f"short read while hashing {relative}")
                whole.update(data)
                digest = hashlib.sha256(data).hexdigest()
                chunks.append(Chunk(digest, size, offset))
                previous = unique_chunks.get(digest)
                location = ChunkLocation(source, offset, size)
                if previous and previous.size != size:
                    raise TransportError("SHA-256 chunk collision with different lengths")
                unique_chunks.setdefault(digest, location)
                offset += size
        actual_sha = whole.hexdigest()
        if actual_sha != expected_sha:
            raise TransportError(
                f"generation file SHA-256 mismatch for {relative}: "
                f"expected {expected_sha}, got {actual_sha}"
            )
        entries.append(FileEntry(relative, expected_size, expected_sha, tuple(chunks)))
    validate_no_unexpected_entries(generation_dir)
    generation_sha = expected["generation.json"][1]
    return (
        TransportManifest(
            generation_id=generation_id,
            chunk_size=CHUNK_SIZE,
            chunk_encoding=CHUNK_ENCODING,
            generation_manifest_sha256=generation_sha,
            files=tuple(entries),
        ),
        unique_chunks,
    )


def parse_manifest(data: bytes, expected_generation: str) -> TransportManifest:
    if len(data) > MAX_MANIFEST_BYTES:
        raise TransportError("transport manifest exceeds size limit")
    try:
        raw = json.loads(data)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise TransportError("transport manifest is invalid JSON") from error
    if not isinstance(raw, dict) or set(raw) != {
        "format",
        "format_version",
        "generation_id",
        "chunk_size",
        "chunk_encoding",
        "generation_manifest_sha256",
        "files",
    }:
        raise TransportError("transport manifest has an invalid top-level shape")
    if (
        raw["format"] != FORMAT
        or raw["format_version"] != FORMAT_VERSION
        or raw["generation_id"] != expected_generation
        or raw["chunk_size"] != CHUNK_SIZE
        or raw["chunk_encoding"] != CHUNK_ENCODING
        or not SHA256_RE.fullmatch(str(raw["generation_manifest_sha256"]))
        or not isinstance(raw["files"], list)
    ):
        raise TransportError("transport manifest identity or version is invalid")
    entries: list[FileEntry] = []
    paths: set[str] = set()
    generation_sha: str | None = None
    total_size = 0
    total_chunks = 0
    for raw_entry in raw["files"]:
        if not isinstance(raw_entry, dict) or set(raw_entry) != {
            "path",
            "size",
            "sha256",
            "chunks",
        }:
            raise TransportError("transport file entry is malformed")
        path = raw_entry["path"]
        size = raw_entry["size"]
        digest = raw_entry["sha256"]
        if (
            path not in EXPECTED_PATHS
            or path in paths
            or not isinstance(size, int)
            or size <= 0
            or size > MAX_GENERATION_FILE_BYTES
            or not isinstance(digest, str)
            or not SHA256_RE.fullmatch(digest)
            or not isinstance(raw_entry["chunks"], list)
            or not raw_entry["chunks"]
        ):
            raise TransportError("transport file metadata is invalid")
        paths.add(path)
        chunks: list[Chunk] = []
        expected_offset = 0
        for raw_chunk in raw_entry["chunks"]:
            if not isinstance(raw_chunk, dict) or set(raw_chunk) != {
                "offset",
                "size",
                "sha256",
            }:
                raise TransportError("transport chunk entry is malformed")
            offset = raw_chunk["offset"]
            chunk_size = raw_chunk["size"]
            chunk_sha = raw_chunk["sha256"]
            if (
                not isinstance(offset, int)
                or offset != expected_offset
                or not isinstance(chunk_size, int)
                or chunk_size <= 0
                or chunk_size > CHUNK_SIZE
                or offset + chunk_size > size
                or not isinstance(chunk_sha, str)
                or not SHA256_RE.fullmatch(chunk_sha)
            ):
                raise TransportError("transport chunk layout is invalid")
            chunks.append(Chunk(chunk_sha, chunk_size, offset))
            expected_offset += chunk_size
        if expected_offset != size:
            raise TransportError("transport chunks do not cover their file exactly")
        total_size += size
        total_chunks += len(chunks)
        if total_size > MAX_GENERATION_BYTES or total_chunks > MAX_GENERATION_CHUNKS:
            raise TransportError("transport manifest exceeds generation resource limits")
        if path == "generation.json":
            generation_sha = digest
        entries.append(FileEntry(path, size, digest, tuple(chunks)))
    if paths != EXPECTED_PATHS or generation_sha != raw["generation_manifest_sha256"]:
        raise TransportError("transport manifest file set or generation binding is invalid")
    entries.sort(key=lambda entry: entry.path)
    return TransportManifest(
        generation_id=expected_generation,
        chunk_size=CHUNK_SIZE,
        chunk_encoding=CHUNK_ENCODING,
        generation_manifest_sha256=str(raw["generation_manifest_sha256"]),
        files=tuple(entries),
    )


def read_chunk(location: ChunkLocation) -> bytes:
    with location.source.open("rb", buffering=0) as handle:
        handle.seek(location.offset)
        data = handle.read(location.size)
    if len(data) != location.size:
        raise TransportError(f"short read from {location.source}")
    return data


def chunk_blob_name(digest: str) -> str:
    if not SHA256_RE.fullmatch(digest):
        raise TransportError("invalid chunk digest")
    return f"chunks/{CHUNK_ENCODING}/sha256/{digest[:2]}/{digest}"


def compress_chunk(data: bytes) -> bytes:
    result = subprocess.run(
        ["zstd", "--compress", "--stdout", "-3", "--single-thread", "--no-progress"],
        input=data,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
    )
    return result.stdout


def decompress_chunk(data: bytes, expected_size: int) -> bytes:
    process = subprocess.Popen(
        ["zstd", "--decompress", "--stdout", "--no-progress", "--memory=128MB"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    assert process.stdin is not None
    assert process.stdout is not None
    write_errors: list[BaseException] = []

    def feed() -> None:
        try:
            process.stdin.write(data)
            process.stdin.close()
        except (BrokenPipeError, OSError) as error:
            write_errors.append(error)

    writer = threading.Thread(target=feed, name="zstd-input", daemon=True)
    writer.start()
    output = process.stdout.read(expected_size + 1)
    if len(output) > expected_size:
        process.kill()
    process.stdout.close()
    return_code = process.wait()
    writer.join()
    if return_code != 0 or write_errors or len(output) != expected_size:
        raise TransportError("decompressed chunk length or zstd frame is invalid")
    return output


def upload_generation(
    generation_dir: pathlib.Path,
    destination: str,
    *,
    token_mode: str,
    tier: str,
    workers: int,
    cache_dir: pathlib.Path | None = None,
) -> dict[str, object]:
    generation_dir, generation_id = validate_source_generation_root(generation_dir)
    cache_path = cache_dir / f"{generation_id}.json" if cache_dir else None
    if cache_dir:
        cache_dir.mkdir(parents=True, exist_ok=True)
        cache_metadata = cache_dir.lstat()
        if not stat.S_ISDIR(cache_metadata.st_mode) or cache_dir.is_symlink():
            raise TransportError("transport cache path must be a real directory")
    # A generation is immutable. Rebuild the transport manifest from the live
    # bytes on every invocation before trusting a cache or skipping a remote
    # chunk. Size/metadata checks alone cannot detect same-size mutation.
    manifest, chunks = build_manifest(generation_dir)
    manifest_bytes = manifest.bytes()
    if cache_path and cache_path.exists():
        regular_file(cache_path)
        cached = parse_manifest(cache_path.read_bytes(), generation_id)
        if cached.bytes() != manifest_bytes:
            raise TransportError("cached transport manifest differs from current generation bytes")
    elif cache_path:
        temporary = cache_path.with_name(f".{cache_path.name}.{os.getpid()}.tmp")
        try:
            with temporary.open("xb") as handle:
                handle.write(manifest_bytes)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary, cache_path)
            fsync_directory(cache_path.parent)
        finally:
            temporary.unlink(missing_ok=True)
    validate_no_unexpected_entries(generation_dir)
    store = blob_store(destination, token_mode)
    existing = store.list_names(f"chunks/{CHUNK_ENCODING}/sha256/")
    missing = [
        (digest, location)
        for digest, location in sorted(chunks.items())
        if chunk_blob_name(digest) not in existing
    ]
    uploaded_bytes = 0
    uploaded_chunks = 0
    result_lock = threading.Lock()
    print(
        json.dumps(
            {
                "event": "azure-upload-plan",
                "generation_id": manifest.generation_id,
                "unique_chunks": len(chunks),
                "missing_chunks": len(missing),
            },
            sort_keys=True,
        ),
        file=sys.stderr,
        flush=True,
    )

    def upload_one(item: tuple[str, ChunkLocation]) -> None:
        nonlocal uploaded_bytes, uploaded_chunks
        digest, location = item
        raw = read_chunk(location)
        if hashlib.sha256(raw).hexdigest() != digest:
            raise TransportError("generation changed while a chunk was being uploaded")
        data = compress_chunk(raw)
        encoded_sha = hashlib.sha256(data).hexdigest()
        created = store.put_immutable(
            chunk_blob_name(digest), data, sha256=encoded_sha, tier=tier
        )
        if created:
            with result_lock:
                uploaded_bytes += len(data)
                uploaded_chunks += 1
                if uploaded_chunks % 10 == 0 or uploaded_chunks == len(missing):
                    print(
                        json.dumps(
                            {
                                "event": "azure-upload-progress",
                                "uploaded_chunks": uploaded_chunks,
                                "missing_chunks": len(missing),
                                "uploaded_bytes": uploaded_bytes,
                            },
                            sort_keys=True,
                        ),
                        file=sys.stderr,
                        flush=True,
                    )

    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as executor:
        futures = [executor.submit(upload_one, item) for item in missing]
        for future in concurrent.futures.as_completed(futures):
            future.result()

    # Re-read and re-hash the exact tree after all chunk work. This catches a
    # local mutation even when every remote chunk already existed or the file
    # changed after its chunk was uploaded.
    final_manifest, _ = build_manifest(generation_dir)
    if final_manifest.bytes() != manifest_bytes:
        raise TransportError("generation changed while it was being uploaded")
    manifest_name = f"generations/{manifest.generation_id}/transport.json"
    manifest_created = store.put_immutable(
        manifest_name,
        manifest_bytes,
        sha256=hashlib.sha256(manifest_bytes).hexdigest(),
        tier=tier,
    )
    return {
        "generation_id": manifest.generation_id,
        "unique_chunks": len(chunks),
        "uploaded_chunks": uploaded_chunks,
        "skipped_chunks": len(chunks) - uploaded_chunks,
        "uploaded_bytes": uploaded_bytes,
        "transport_manifest_created": manifest_created,
        "transport_manifest": manifest_name,
    }


def ensure_restore_root(path: pathlib.Path, generation_id: str) -> pathlib.Path:
    if path.name != generation_id:
        raise TransportError("restore output directory name must equal the generation ID")
    parent = path.parent.resolve()
    if not parent.is_dir() or parent.is_symlink():
        raise TransportError("restore output parent must be a real existing directory")
    if path.exists():
        metadata = path.lstat()
        if not stat.S_ISDIR(metadata.st_mode) or path.is_symlink():
            raise TransportError("restore output must be a real directory")
    else:
        path.mkdir(mode=0o750)
        fsync_directory(parent)
    return path.resolve()


def validate_no_unexpected_entries(root: pathlib.Path) -> None:
    expected_directories = {"ann", "lexical"}
    for path in root.rglob("*"):
        relative = path.relative_to(root).as_posix()
        metadata = path.lstat()
        if stat.S_ISLNK(metadata.st_mode):
            raise TransportError(f"generation tree contains symlink: {relative}")
        if stat.S_ISDIR(metadata.st_mode):
            if relative not in expected_directories:
                raise TransportError(f"generation tree contains unexpected directory: {relative}")
        elif stat.S_ISREG(metadata.st_mode):
            if relative not in EXPECTED_PATHS:
                raise TransportError(f"generation tree contains unexpected file: {relative}")
            if metadata.st_nlink != 1:
                raise TransportError(f"generation tree contains hard-linked file: {relative}")
        else:
            raise TransportError(f"generation tree contains special file: {relative}")


def clone_or_create(
    destination: pathlib.Path,
    size: int,
    basis: pathlib.Path | None,
    *,
    allow_full_copy: bool,
) -> None:
    destination.parent.mkdir(mode=0o750, parents=True, exist_ok=True)
    if destination.exists():
        regular_file(destination)
        os.chmod(destination, 0o640)
        with destination.open("r+b", buffering=0) as handle:
            handle.truncate(size)
        return
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    destination_fd = os.open(destination, flags, 0o640)
    try:
        if basis is not None and basis.exists():
            regular_file(basis)
            source_flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
            source_fd = os.open(basis, source_flags)
            try:
                try:
                    fcntl.ioctl(destination_fd, FICLONE, source_fd)
                except OSError as error:
                    if not allow_full_copy:
                        raise TransportError(
                            f"CoW clone failed for {basis}; Azure data disk must use "
                            "a reflink-capable XFS filesystem"
                        ) from error
                    os.lseek(source_fd, 0, os.SEEK_SET)
                    while True:
                        data = os.read(source_fd, 8 * 1024 * 1024)
                        if not data:
                            break
                        written = 0
                        view = memoryview(data)
                        while written < len(data):
                            count = os.write(destination_fd, view[written:])
                            if count <= 0:
                                raise TransportError("short write during full-copy fallback")
                            written += count
            finally:
                os.close(source_fd)
        os.ftruncate(destination_fd, size)
        os.fsync(destination_fd)
    finally:
        os.close(destination_fd)


def hash_region(path: pathlib.Path, offset: int, size: int) -> str:
    digest = hashlib.sha256()
    remaining = size
    with path.open("rb", buffering=0) as handle:
        handle.seek(offset)
        while remaining:
            data = handle.read(min(8 * 1024 * 1024, remaining))
            if not data:
                raise TransportError(f"short read while checking {path}")
            digest.update(data)
            remaining -= len(data)
    return digest.hexdigest()


def sha256_file(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb", buffering=0) as handle:
        while True:
            data = handle.read(8 * 1024 * 1024)
            if not data:
                break
            digest.update(data)
    return digest.hexdigest()


def available_bytes(path: pathlib.Path) -> int:
    statvfs = os.statvfs(path)
    return statvfs.f_bavail * statvfs.f_frsize


def restore_generation(
    generation_id: str,
    source: str,
    output_dir: pathlib.Path,
    *,
    basis_dir: pathlib.Path | None,
    token_mode: str,
    workers: int,
    allow_full_copy: bool,
    minimum_free_margin: int = MIN_FREE_MARGIN,
) -> dict[str, object]:
    generation_id = validate_generation_id(generation_id)
    store = blob_store(source, token_mode)
    manifest_name = f"generations/{generation_id}/transport.json"
    manifest_data = store.get(manifest_name, maximum=MAX_MANIFEST_BYTES)
    manifest = parse_manifest(manifest_data, generation_id)
    output = ensure_restore_root(output_dir, generation_id)
    validate_no_unexpected_entries(output)
    basis = basis_dir.resolve() if basis_dir else None
    if basis is not None:
        metadata = basis.lstat()
        if not stat.S_ISDIR(metadata.st_mode) or basis.is_symlink():
            raise TransportError("basis generation must be a real directory")

    if basis is None or allow_full_copy:
        required_full_copy = sum(entry.size for entry in manifest.files) + minimum_free_margin
        available_before_restore = available_bytes(output)
        if available_before_restore < required_full_copy:
            raise TransportError(
                "insufficient data-disk space for a full restore: "
                f"available={available_before_restore}, required={required_full_copy}"
            )

    for entry in manifest.files:
        destination = output.joinpath(*entry.path.split("/"))
        basis_file = basis.joinpath(*entry.path.split("/")) if basis else None
        clone_or_create(
            destination,
            entry.size,
            basis_file,
            allow_full_copy=allow_full_copy,
        )

    missing: dict[str, list[RestoreTarget]] = {}
    missing_sizes: dict[str, int] = {}
    reused_targets = 0
    for entry in manifest.files:
        destination = output.joinpath(*entry.path.split("/"))
        for chunk in entry.chunks:
            if hash_region(destination, chunk.offset, chunk.size) == chunk.sha256:
                reused_targets += 1
                continue
            previous_size = missing_sizes.setdefault(chunk.sha256, chunk.size)
            if previous_size != chunk.size:
                raise TransportError("transport chunk digest has conflicting sizes")
            missing.setdefault(chunk.sha256, []).append(
                RestoreTarget(destination, chunk.offset, chunk.size)
            )
    required = (
        sum(target.size for targets in missing.values() for target in targets)
        + minimum_free_margin
    )
    available = available_bytes(output)
    if available < required:
        raise TransportError(
            f"insufficient data-disk space: available={available}, required={required}"
        )

    restored_bytes = 0
    restored_chunks = 0
    result_lock = threading.Lock()
    print(
        json.dumps(
            {
                "event": "azure-restore-plan",
                "generation_id": generation_id,
                "missing_unique_chunks": len(missing),
                "required_allocated_bytes": required,
                "available_bytes": available,
            },
            sort_keys=True,
        ),
        file=sys.stderr,
        flush=True,
    )

    def restore_one(item: tuple[str, list[RestoreTarget]]) -> None:
        nonlocal restored_bytes, restored_chunks
        digest, targets = item
        size = missing_sizes[digest]
        encoded = store.get(chunk_blob_name(digest), maximum=size + 1024 * 1024)
        data = decompress_chunk(encoded, size)
        if hashlib.sha256(data).hexdigest() != digest:
            raise TransportError(f"downloaded chunk failed SHA-256: {digest}")
        for target in targets:
            flags = os.O_WRONLY | getattr(os, "O_NOFOLLOW", 0)
            fd = os.open(target.destination, flags)
            try:
                written = 0
                view = memoryview(data)
                while written < len(data):
                    count = os.pwrite(fd, view[written:], target.offset + written)
                    if count <= 0:
                        raise TransportError(f"short write to {target.destination}")
                    written += count
                os.fsync(fd)
            finally:
                os.close(fd)
        with result_lock:
            restored_bytes += len(data)
            restored_chunks += 1
            if restored_chunks % 10 == 0 or restored_chunks == len(missing):
                print(
                    json.dumps(
                        {
                            "event": "azure-restore-progress",
                            "restored_chunks": restored_chunks,
                            "missing_unique_chunks": len(missing),
                            "restored_bytes": restored_bytes,
                        },
                        sort_keys=True,
                    ),
                    file=sys.stderr,
                    flush=True,
                )

    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as executor:
        futures = [executor.submit(restore_one, item) for item in sorted(missing.items())]
        for future in concurrent.futures.as_completed(futures):
            future.result()

    for entry in manifest.files:
        destination = output.joinpath(*entry.path.split("/"))
        regular_file(destination, expected_size=entry.size)
        actual = sha256_file(destination)
        if actual != entry.sha256:
            raise TransportError(
                f"restored file SHA-256 mismatch for {entry.path}: {actual}"
            )
        os.chmod(destination, 0o640)
        fd = os.open(destination, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    for directory in sorted(
        {output, *(path.parent for path in output.rglob("*") if path.is_file())},
        key=lambda path: len(path.parts),
        reverse=True,
    ):
        fsync_directory(directory)
    validate_no_unexpected_entries(output)
    return {
        "generation_id": generation_id,
        "restored_unique_chunks": restored_chunks,
        "restored_bytes": restored_bytes,
        "reused_chunk_targets": reused_targets,
        "output_dir": str(output),
    }


def fsync_directory(path: pathlib.Path) -> None:
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def positive_workers(value: str) -> int:
    parsed = int(value)
    if not 1 <= parsed <= MAX_WORKERS:
        raise argparse.ArgumentTypeError(f"workers must be between 1 and {MAX_WORKERS}")
    return parsed


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    subcommands = root.add_subparsers(dest="command", required=True)

    manifest = subcommands.add_parser("manifest", help="build and print a transport manifest")
    manifest.add_argument("--generation-dir", required=True, type=pathlib.Path)
    manifest.add_argument("--output", type=pathlib.Path)

    upload = subcommands.add_parser("upload", help="upload only missing content-addressed chunks")
    upload.add_argument("--generation-dir", required=True, type=pathlib.Path)
    upload.add_argument("--destination", required=True)
    upload.add_argument(
        "--token-mode", choices=("azure-cli",), default="azure-cli"
    )
    upload.add_argument("--tier", choices=("Hot", "Cool"), default="Cool")
    upload.add_argument("--workers", type=positive_workers, default=4)
    upload.add_argument("--cache-dir", type=pathlib.Path)

    restore = subcommands.add_parser("restore", help="restore a generation into incoming storage")
    restore.add_argument("--generation-id", required=True)
    restore.add_argument("--source", required=True)
    restore.add_argument("--output-dir", required=True, type=pathlib.Path)
    restore.add_argument("--basis-dir", type=pathlib.Path)
    restore.add_argument(
        "--token-mode",
        choices=("managed-identity", "azure-cli"),
        default="managed-identity",
    )
    restore.add_argument("--workers", type=positive_workers, default=4)
    restore.add_argument(
        "--allow-full-copy",
        action="store_true",
        help="test-only fallback when the destination filesystem cannot CoW clone",
    )
    return root


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if shutil.which("zstd") is None:
            raise TransportError("zstd executable is required")
        if args.command == "manifest":
            manifest, _ = build_manifest(args.generation_dir)
            data = manifest.bytes()
            if args.output:
                args.output.write_bytes(data)
            else:
                sys.stdout.buffer.write(data)
            return 0
        if args.command == "upload":
            report = upload_generation(
                args.generation_dir,
                args.destination,
                token_mode=args.token_mode,
                tier=args.tier,
                workers=args.workers,
                cache_dir=args.cache_dir,
            )
        else:
            report = restore_generation(
                args.generation_id,
                args.source,
                args.output_dir,
                basis_dir=args.basis_dir,
                token_mode=args.token_mode,
                workers=args.workers,
                allow_full_copy=args.allow_full_copy,
            )
        print(json.dumps(report, sort_keys=True))
        return 0
    except (TransportError, OSError, subprocess.SubprocessError) as error:
        print(f"azure generation transport: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
