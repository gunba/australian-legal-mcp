from __future__ import annotations

import json
import os
import pathlib
import stat
import subprocess
import sys
import tempfile
import unittest


REPO = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = REPO / "scripts" / "manage-api-keys.py"


class ManageApiKeysTests(unittest.TestCase):
    def run_tool(self, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(SCRIPT), *args],
            check=check,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

    def test_generation_rotation_and_revocation_store_only_digests(self) -> None:
        with tempfile.TemporaryDirectory() as raw_directory:
            path = pathlib.Path(raw_directory) / "keys.json"
            first = self.run_tool("generate", "--file", str(path), "--id", "automation")
            second = self.run_tool("generate", "--file", str(path), "--id", "rotation")
            first_token = first.stdout.strip()
            second_token = second.stdout.strip()
            self.assertRegex(first_token, r"^automation\.[A-Za-z0-9_-]{43}$")
            self.assertRegex(second_token, r"^rotation\.[A-Za-z0-9_-]{43}$")
            raw = path.read_text()
            self.assertNotIn(first_token, raw)
            self.assertNotIn(second_token, raw)
            if os.name == "posix":
                self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o400)

            listing = self.run_tool("list", "--file", str(path))
            self.assertEqual(json.loads(listing.stdout), {"key_ids": ["automation", "rotation"]})
            self.run_tool("revoke", "--file", str(path), "--id", "automation")
            self.assertEqual(
                json.loads(path.read_text())["keys"][0]["id"],
                "rotation",
            )
            final = self.run_tool(
                "revoke", "--file", str(path), "--id", "rotation", check=False
            )
            self.assertEqual(final.returncode, 2)
            self.assertIn("final API key", final.stderr)

    @unittest.skipUnless(os.name == "posix", "Unix permission contract")
    def test_permissive_or_symlink_verifier_files_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw_directory:
            directory = pathlib.Path(raw_directory)
            path = directory / "keys.json"
            self.run_tool("generate", "--file", str(path), "--id", "automation")
            path.chmod(0o440)
            result = self.run_tool("list", "--file", str(path), check=False)
            self.assertEqual(result.returncode, 2)
            self.assertIn("group or other", result.stderr)
            path.chmod(0o400)
            link = directory / "keys-link.json"
            link.symlink_to(path)
            result = self.run_tool("list", "--file", str(link), check=False)
            self.assertEqual(result.returncode, 2)
            self.assertIn("non-symlink", result.stderr)


if __name__ == "__main__":
    unittest.main()
