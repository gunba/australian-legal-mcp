from __future__ import annotations

import importlib.util
import pathlib
import unittest


SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "scripts" / "render-microsoft-integrations.py"
SPEC = importlib.util.spec_from_file_location("render_microsoft_integrations", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class RenderMicrosoftIntegrationsTests(unittest.TestCase):
    def test_provider_neutral_canonical_dns_hosts(self) -> None:
        for host in (
            "legal.example.com",
            "mcp.sydney.example.com",
            "legalmcptest.australiaeast.cloudapp.azure.com",
        ):
            with self.subTest(host=host):
                self.assertTrue(MODULE.is_canonical_public_host(host))

        for host in (
            "localhost",
            "127.0.0.1",
            "Legal.example.com",
            "legal_example.com",
            "-legal.example.com",
            "legal.example.com.",
            "legal..example.com",
        ):
            with self.subTest(host=host):
                self.assertFalse(MODULE.is_canonical_public_host(host))


if __name__ == "__main__":
    unittest.main()
