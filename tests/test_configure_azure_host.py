import os
import pathlib
import sys
import unittest


TESTS_DIR = pathlib.Path(__file__).resolve().parent
if str(TESTS_DIR) not in sys.path:
    sys.path.insert(0, str(TESTS_DIR))

from configure_azure_host_fixture import AzureHostFixture, ROOT  # noqa: E402


class ConfigureAzureHostFixtureTests(unittest.TestCase):
    def assert_rejected_before_mutation(self, fixture, result):
        self.assertNotEqual(0, result.returncode, result.stdout + result.stderr)
        self.assertEqual([], fixture.mutations, result.stdout + result.stderr)

    def test_rejects_quadlet_in_every_system_generator_search_path(self):
        for directory in AzureHostFixture.QUADLET_DIRECTORIES:
            with self.subTest(directory=directory), AzureHostFixture() as fixture:
                fixture.write_file(
                    f"{directory}/legal-mcp.container", "[Container]\nImage=fixture\n"
                )

                result = fixture.run()

                self.assert_rejected_before_mutation(fixture, result)

    def test_rejects_dangling_quadlet_in_every_system_generator_search_path(self):
        for directory in AzureHostFixture.QUADLET_DIRECTORIES:
            with self.subTest(directory=directory), AzureHostFixture() as fixture:
                quadlet = fixture.dangling_symlink(
                    f"{directory}/legal-mcp.container"
                )
                self.assertTrue(os.path.islink(quadlet))
                self.assertFalse(os.path.exists(quadlet))

                result = fixture.run()

                self.assert_rejected_before_mutation(fixture, result)

    def test_rejects_loaded_generated_unit_without_remaining_quadlet_source(self):
        with AzureHostFixture() as fixture:
            fixture.set_systemd_state(
                unit_file_status=0,
                unit_file_state="generated",
                load_state="loaded",
                fragment_path="/run/systemd/generator/legal-mcp.service",
            )

            result = fixture.run()

            self.assert_rejected_before_mutation(fixture, result)

    def test_rejects_existing_native_unit(self):
        with AzureHostFixture() as fixture:
            fixture.write_file(
                "/usr/lib/systemd/system/legal-mcp.service", "[Service]\n"
            )
            fixture.set_systemd_state(
                unit_file_status=1,
                unit_file_state="disabled",
                load_state="loaded",
                fragment_path="/usr/lib/systemd/system/legal-mcp.service",
            )

            result = fixture.run()

            self.assert_rejected_before_mutation(fixture, result)

    def test_rejects_transitional_stale_loaded_fragment(self):
        with AzureHostFixture() as fixture:
            fixture.set_systemd_state(
                unit_file_status=4,
                unit_file_state="not-found",
                load_state="loaded",
                fragment_path="/run/systemd/generator/legal-mcp.service",
            )

            result = fixture.run()

            self.assert_rejected_before_mutation(fixture, result)

    def test_fails_closed_when_systemd_state_cannot_be_read(self):
        with AzureHostFixture() as fixture:
            fixture.fail_systemctl_show()

            result = fixture.run()

            self.assert_rejected_before_mutation(fixture, result)

    def test_clean_install_finishes_as_enabled_inactive_native_unit(self):
        with AzureHostFixture() as fixture:
            result = fixture.run()

            self.assertEqual(0, result.returncode, result.stdout + result.stderr)
            state = fixture.state
            self.assertEqual("loaded", state["legal_load_state"])
            self.assertEqual(
                "/etc/systemd/system/legal-mcp.service",
                state["legal_fragment_path"],
            )
            self.assertEqual("enabled", state["legal_unit_file_state"])
            self.assertEqual(0, state["legal_unit_file_status"])
            self.assertEqual("inactive", state["legal_active"])
            self.assertEqual("disabled", state["caddy_enabled"])
            self.assertEqual("inactive", state["caddy_active"])
            self.assertEqual(
                (ROOT / "systemd" / "legal-mcp.service").read_bytes(),
                fixture.path("/etc/systemd/system/legal-mcp.service").read_bytes(),
            )
            for directory in AzureHostFixture.QUADLET_DIRECTORIES:
                self.assertFalse(
                    os.path.lexists(
                        fixture.path(f"{directory}/legal-mcp.container")
                    )
                )


if __name__ == "__main__":
    unittest.main()
