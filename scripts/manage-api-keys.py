#!/usr/bin/env python3
"""Generate and revoke high-entropy API keys without storing plaintext server-side."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import secrets
import stat
import sys
import tempfile

ID_RE = re.compile(r"^[a-z0-9][a-z0-9_-]{0,63}$")
MAX_KEYS = 32
MAX_FILE_BYTES = 64 * 1024


class KeyFileError(RuntimeError):
    pass


def validate_id(value: str) -> str:
    if not ID_RE.fullmatch(value):
        raise KeyFileError("key ID must be 1-64 lowercase ASCII identifier characters")
    return value


def empty_document() -> dict[str, object]:
    return {"version": 1, "keys": []}


def load_document(path: pathlib.Path, *, allow_missing: bool) -> dict[str, object]:
    if not path.is_absolute():
        raise KeyFileError("verifier file path must be absolute")
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        if allow_missing:
            return empty_document()
        raise KeyFileError(f"verifier file does not exist: {path}") from None
    if path.is_symlink() or not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
        raise KeyFileError("verifier file must be a single-link regular non-symlink file")
    if metadata.st_size <= 0 or metadata.st_size > MAX_FILE_BYTES:
        raise KeyFileError("verifier file has an invalid size")
    if os.name == "posix" and stat.S_IMODE(metadata.st_mode) & 0o077:
        raise KeyFileError("verifier file must not be accessible by group or other users")
    with path.open("rb") as handle:
        raw = handle.read(MAX_FILE_BYTES + 1)
    if len(raw) > MAX_FILE_BYTES:
        raise KeyFileError("verifier file exceeds its size limit")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise KeyFileError(f"invalid verifier JSON: {error}") from error
    if not isinstance(value, dict) or set(value) != {"version", "keys"} or value["version"] != 1:
        raise KeyFileError("verifier file must use the exact version-1 object schema")
    keys = value["keys"]
    if not isinstance(keys, list) or len(keys) > MAX_KEYS:
        raise KeyFileError(f"verifier file must contain no more than {MAX_KEYS} keys")
    seen_ids: set[str] = set()
    seen_hashes: set[str] = set()
    for item in keys:
        if not isinstance(item, dict) or set(item) != {"id", "sha256"}:
            raise KeyFileError("every verifier entry must contain exactly id and sha256")
        key_id = item["id"]
        digest = item["sha256"]
        if not isinstance(key_id, str):
            raise KeyFileError("key ID must be a string")
        validate_id(key_id)
        if (
            not isinstance(digest, str)
            or len(digest) != 64
            or any(character not in "0123456789abcdef" for character in digest)
        ):
            raise KeyFileError("verifier digest must be canonical lowercase SHA-256")
        if key_id in seen_ids or digest in seen_hashes:
            raise KeyFileError("verifier file contains a duplicate key ID or digest")
        seen_ids.add(key_id)
        seen_hashes.add(digest)
    return value


def write_document(path: pathlib.Path, document: dict[str, object]) -> None:
    parent = path.parent
    parent_metadata = parent.lstat()
    if parent.is_symlink() or not stat.S_ISDIR(parent_metadata.st_mode):
        raise KeyFileError("verifier parent must be a real directory")
    payload = (json.dumps(document, sort_keys=True, separators=(",", ":")) + "\n").encode()
    if len(payload) > MAX_FILE_BYTES:
        raise KeyFileError("verifier file exceeds its size limit")
    fd, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=parent)
    temporary = pathlib.Path(temporary_name)
    try:
        with os.fdopen(fd, "wb", closefd=True) as handle:
            os.fchmod(handle.fileno(), 0o400)
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
        directory_fd = os.open(parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    finally:
        temporary.unlink(missing_ok=True)


def generate(path: pathlib.Path, key_id: str) -> str:
    validate_id(key_id)
    document = load_document(path, allow_missing=True)
    keys = document["keys"]
    assert isinstance(keys, list)
    if len(keys) >= MAX_KEYS:
        raise KeyFileError(f"verifier file already contains {MAX_KEYS} keys")
    if any(item["id"] == key_id for item in keys):
        raise KeyFileError(f"key ID already exists: {key_id}")
    secret = secrets.token_urlsafe(32)
    if len(secret) != 43:
        raise KeyFileError("platform generated an unexpected API-key secret shape")
    token = f"{key_id}.{secret}"
    keys.append({"id": key_id, "sha256": hashlib.sha256(token.encode()).hexdigest()})
    keys.sort(key=lambda item: item["id"])
    write_document(path, document)
    return token


def revoke(path: pathlib.Path, key_id: str) -> None:
    validate_id(key_id)
    document = load_document(path, allow_missing=False)
    keys = document["keys"]
    assert isinstance(keys, list)
    retained = [item for item in keys if item["id"] != key_id]
    if len(retained) == len(keys):
        raise KeyFileError(f"unknown key ID: {key_id}")
    if not retained:
        raise KeyFileError("refusing to remove the final API key; configure another authenticator first")
    document["keys"] = retained
    write_document(path, document)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    subparsers = result.add_subparsers(dest="command", required=True)
    for command in ("generate", "revoke"):
        child = subparsers.add_parser(command)
        child.add_argument("--file", type=pathlib.Path, required=True)
        child.add_argument("--id", required=True)
    listing = subparsers.add_parser("list")
    listing.add_argument("--file", type=pathlib.Path, required=True)
    return result


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "generate":
            token = generate(args.file, args.id)
            print(token)
            print(
                "API key generated; store the stdout value now because only its digest was saved",
                file=sys.stderr,
            )
        elif args.command == "revoke":
            revoke(args.file, args.id)
            print(json.dumps({"revoked": args.id}, sort_keys=True))
        else:
            document = load_document(args.file, allow_missing=False)
            print(json.dumps({"key_ids": [item["id"] for item in document["keys"]]}, sort_keys=True))
        return 0
    except (KeyFileError, OSError) as error:
        print(f"manage-api-keys: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
