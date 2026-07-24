import hashlib
import importlib.util
import json
import os
import pathlib
import sys
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).parents[1] / "scripts" / "azure_generation_transport.py"
SPEC = importlib.util.spec_from_file_location("azure_generation_transport", SCRIPT)
transport = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = transport
assert SPEC.loader is not None
SPEC.loader.exec_module(transport)
transport.CHUNK_SIZE = 64 * 1024


class AzureGenerationTransportTests(unittest.TestCase):
    def create_generation(
        self, root: pathlib.Path, generation_id: str, *, changed: bool = False
    ) -> pathlib.Path:
        generation = root / generation_id
        (generation / "ann").mkdir(parents=True)
        (generation / "lexical").mkdir()
        files = {
            "legal.db": bytearray((b"database-page-contents\0" * 12_000)[:230_000]),
            "model.onnx": bytearray((b"model" * 30_000)[:110_000]),
            "tokenizer.json": bytearray((b'{"token":"value"}\n' * 4_000)[:70_000]),
        }
        for index, source in enumerate(transport.SOURCE_IDS):
            files[f"ann/{source}.ann"] = bytearray(
                (f"ann-{index}-{source}\n".encode() * 4_000)[:45_000]
            )
            files[f"lexical/{source}.db"] = bytearray(
                (f"lexical-{index}-{source}\n".encode() * 4_000)[:48_000]
            )
        if changed:
            files["legal.db"][70_000:70_100] = b"changed-pages".ljust(100, b"!")
        metadata = {}
        for relative, contents in files.items():
            path = generation.joinpath(*relative.split("/"))
            path.write_bytes(contents)
            metadata[relative] = {
                "path": relative,
                "size": len(contents),
                "sha256": hashlib.sha256(contents).hexdigest(),
            }
        manifest = {
            "schema_version": 12,
            "index_version": "test",
            "created_at": "2026-01-01T00:00:00Z",
            "min_client_version": "0.17.0",
            "db": metadata["legal.db"],
            "model": {
                "id": "test",
                "fingerprint": "0" * 64,
                "model": metadata["model.onnx"],
                "tokenizer": metadata["tokenizer.json"],
            },
            "ann": {
                source: {
                    "source_id": source,
                    **metadata[f"ann/{source}.ann"],
                }
                for source in transport.SOURCE_IDS
            },
            "lexical": {
                source: {
                    "source_id": source,
                    **metadata[f"lexical/{source}.db"],
                }
                for source in transport.SOURCE_IDS
            },
        }
        (generation / "generation.json").write_text(
            json.dumps(manifest, sort_keys=True), encoding="utf-8"
        )
        return generation

    def assert_generation_equal(self, left: pathlib.Path, right: pathlib.Path) -> None:
        for relative in transport.EXPECTED_PATHS:
            self.assertEqual(
                left.joinpath(*relative.split("/")).read_bytes(),
                right.joinpath(*relative.split("/")).read_bytes(),
                relative,
            )

    def test_upload_restore_delta_and_resume_use_content_addressed_chunks(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            first_id = "1" * 64
            second_id = "2" * 64
            first = self.create_generation(root / "source", first_id)
            second = self.create_generation(root / "source", second_id, changed=True)
            blob_root = root / "blob"
            blob_url = blob_root.as_uri()
            cache_dir = root / "cache"

            first_upload = transport.upload_generation(
                first,
                blob_url,
                token_mode="azure-cli",
                tier="Cool",
                workers=2,
                cache_dir=cache_dir,
            )
            self.assertTrue((cache_dir / f"{first_id}.json").is_file())
            self.assertGreater(first_upload["uploaded_chunks"], 0)
            self.assertEqual(first_upload["skipped_chunks"], 0)
            repeat = transport.upload_generation(
                first,
                blob_url,
                token_mode="azure-cli",
                tier="Cool",
                workers=2,
                cache_dir=cache_dir,
            )
            self.assertEqual(repeat["uploaded_chunks"], 0)
            self.assertFalse(repeat["transport_manifest_created"])

            incoming = root / "incoming"
            incoming.mkdir()
            restored_first = incoming / first_id
            first_restore = transport.restore_generation(
                first_id,
                blob_url,
                restored_first,
                basis_dir=None,
                token_mode="managed-identity",
                workers=2,
                allow_full_copy=True,
                minimum_free_margin=0,
            )
            self.assertGreater(first_restore["restored_unique_chunks"], 0)
            self.assert_generation_equal(first, restored_first)

            second_upload = transport.upload_generation(
                second,
                blob_url,
                token_mode="azure-cli",
                tier="Cool",
                workers=2,
                cache_dir=cache_dir,
            )
            self.assertGreater(second_upload["skipped_chunks"], 0)
            self.assertLess(
                second_upload["uploaded_chunks"], second_upload["unique_chunks"]
            )
            restored_second = incoming / second_id
            second_restore = transport.restore_generation(
                second_id,
                blob_url,
                restored_second,
                basis_dir=restored_first,
                token_mode="managed-identity",
                workers=2,
                allow_full_copy=True,
                minimum_free_margin=0,
            )
            self.assertGreater(second_restore["reused_chunk_targets"], 0)
            self.assert_generation_equal(second, restored_second)

            database = restored_second / "legal.db"
            with database.open("r+b") as handle:
                handle.seek(70_010)
                handle.write(b"corrupt")
            resumed = transport.restore_generation(
                second_id,
                blob_url,
                restored_second,
                basis_dir=restored_first,
                token_mode="managed-identity",
                workers=2,
                allow_full_copy=True,
                minimum_free_margin=0,
            )
            self.assertGreaterEqual(resumed["restored_unique_chunks"], 1)
            self.assert_generation_equal(second, restored_second)

    def test_cached_upload_rehashes_same_size_source_bytes_before_skipping(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            generation_id = "9" * 64
            generation = self.create_generation(root / "source", generation_id)
            blob_url = (root / "blob").as_uri()
            cache_dir = root / "cache"
            transport.upload_generation(
                generation,
                blob_url,
                token_mode="azure-cli",
                tier="Cool",
                workers=2,
                cache_dir=cache_dir,
            )

            database = generation / "legal.db"
            original_size = database.stat().st_size
            with database.open("r+b") as handle:
                handle.seek(80_000)
                handle.write(b"same-size-mutation")
            self.assertEqual(database.stat().st_size, original_size)

            with self.assertRaises(transport.TransportError):
                transport.upload_generation(
                    generation,
                    blob_url,
                    token_mode="azure-cli",
                    tier="Cool",
                    workers=2,
                    cache_dir=cache_dir,
                )

    def test_manifest_parser_rejects_paths_and_incomplete_layouts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            generation_id = "a" * 64
            generation = self.create_generation(root, generation_id)
            manifest, _ = transport.build_manifest(generation)
            raw = manifest.to_dict()
            raw["files"][0]["path"] = "../escape"
            with self.assertRaises(transport.TransportError):
                transport.parse_manifest(json.dumps(raw).encode(), generation_id)

            raw = manifest.to_dict()
            raw["files"].pop()
            with self.assertRaises(transport.TransportError):
                transport.parse_manifest(json.dumps(raw).encode(), generation_id)

    def test_generation_manifest_rejects_wrong_source_sidecar_bindings(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            generation = self.create_generation(
                pathlib.Path(temporary), "c" * 64
            )
            manifest_path = generation / "generation.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["lexical"]["ato"]["source_id"] = "frl"
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaises(transport.TransportError):
                transport.expected_generation_files(generation)

    def test_upload_rejects_extra_source_file_even_with_cached_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            generation_id = "d" * 64
            generation = self.create_generation(root / "source", generation_id)
            manifest, _ = transport.build_manifest(generation)
            cache = root / "cache"
            cache.mkdir()
            (cache / f"{generation_id}.json").write_bytes(manifest.bytes())
            (generation / "unexpected.secret").write_text("not transportable")

            with self.assertRaises(transport.TransportError):
                transport.upload_generation(
                    generation,
                    (root / "blob").as_uri(),
                    token_mode="azure-cli",
                    tier="Cool",
                    workers=1,
                    cache_dir=cache,
                )

    def test_source_generation_rejects_extra_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            generation = self.create_generation(
                pathlib.Path(temporary), "e" * 64
            )
            (generation / "unexpected").mkdir()
            with self.assertRaises(transport.TransportError):
                transport.build_manifest(generation)

    @unittest.skipUnless(hasattr(os, "mkfifo"), "FIFO creation is unavailable")
    def test_source_generation_rejects_special_file(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            generation = self.create_generation(
                pathlib.Path(temporary), "f" * 64
            )
            os.mkfifo(generation / "unexpected.fifo")
            with self.assertRaises(transport.TransportError):
                transport.build_manifest(generation)

    @unittest.skipIf(sys.platform == "win32", "symlink creation requires extra privileges")
    def test_source_generation_rejects_symlinked_root_before_resolving(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            generation_id = "1" * 64
            generation = self.create_generation(root / "real", generation_id)
            linked_parent = root / "linked"
            linked_parent.mkdir()
            linked = linked_parent / generation_id
            linked.symlink_to(generation, target_is_directory=True)
            with self.assertRaises(transport.TransportError):
                transport.build_manifest(linked)

    def test_decompression_and_manifest_resource_limits_are_enforced(self) -> None:
        encoded = transport.compress_chunk(b"x" * (1024 * 1024))
        with self.assertRaises(transport.TransportError):
            transport.decompress_chunk(encoded, 16)

        with tempfile.TemporaryDirectory() as temporary:
            generation_id = "b" * 64
            generation = self.create_generation(pathlib.Path(temporary), generation_id)
            manifest, _ = transport.build_manifest(generation)
            raw = manifest.to_dict()
            raw["files"][0]["size"] = transport.MAX_GENERATION_FILE_BYTES + 1
            with self.assertRaises(transport.TransportError):
                transport.parse_manifest(json.dumps(raw).encode(), generation_id)

    def test_cloud_url_and_generation_contracts_are_strict(self) -> None:
        transport.validate_azure_blob_base(
            "https://legalmcp.blob.core.windows.net/generations"
        )
        for value in [
            "http://legalmcp.blob.core.windows.net/generations",
            "https://example.com/generations",
            "https://legalmcp.blob.core.windows.net/generations/",
            "https://legalmcp.blob.core.windows.net/a/b",
        ]:
            with self.assertRaises(transport.TransportError, msg=value):
                transport.validate_azure_blob_base(value)
        with self.assertRaises(transport.TransportError):
            transport.validate_generation_id("A" * 64)


if __name__ == "__main__":
    unittest.main()
