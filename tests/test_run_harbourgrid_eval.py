import importlib.util
import pathlib
import unittest
import urllib.error


SCRIPT = pathlib.Path(__file__).parents[1] / "scripts" / "run-harbourgrid-eval.py"
SPEC = importlib.util.spec_from_file_location("run_harbourgrid_eval", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class Response:
    def __init__(self, status: int):
        self.status = status
        self.read_called = False

    def read(self):
        self.read_called = True
        return b""


class Opener:
    def __init__(self, result):
        self.result = result

    def open(self, request, timeout):
        assert request.full_url.endswith("/readyz")
        assert timeout == 5
        if isinstance(self.result, Exception):
            raise self.result
        return self.result


class HarbourGridReadySurfaceTests(unittest.TestCase):
    def setUp(self):
        self.original_opener = MODULE.OPENER

    def tearDown(self):
        MODULE.OPENER = self.original_opener

    def test_loopback_requires_successful_private_ready_route(self):
        response = Response(200)
        MODULE.OPENER = Opener(response)
        MODULE.probe_ready_surface("http://127.0.0.1:51235/mcp")
        self.assertTrue(response.read_called)

    def test_loopback_rejects_hidden_ready_route(self):
        MODULE.OPENER = Opener(
            urllib.error.HTTPError("http://127.0.0.1:51235/readyz", 404, "", {}, None)
        )
        with self.assertRaises(urllib.error.HTTPError) as raised:
            MODULE.probe_ready_surface("http://127.0.0.1:51235/mcp")
        raised.exception.close()

    def test_public_endpoint_requires_hidden_ready_route(self):
        MODULE.OPENER = Opener(
            urllib.error.HTTPError("https://legal.example/readyz", 404, "", {}, None)
        )
        MODULE.probe_ready_surface("https://legal.example/mcp")

    def test_public_endpoint_rejects_exposed_ready_route(self):
        MODULE.OPENER = Opener(Response(200))
        with self.assertRaisesRegex(RuntimeError, "must remain hidden"):
            MODULE.probe_ready_surface("https://legal.example/mcp")

    def test_asset_marker_match_cannot_cross_truncated_chunk_text(self):
        rendered = "[asset:frl:C1/sha256-abc\n[asset:frl:C1/sha256-def]"
        markers = MODULE.ASSET_RE.findall(rendered)
        self.assertEqual(markers, [("frl", "C1/sha256-def")])

    def test_public_asset_id_is_decoded_for_the_typed_tool_argument(self):
        self.assertEqual(MODULE.decode_public_asset_id("folder/name%3Apart"), "folder/name:part")
        for invalid in ["folder%2fname", "folder%41", "folder%", "folder%FF"]:
            with self.assertRaises((UnicodeDecodeError, ValueError)):
                MODULE.decode_public_asset_id(invalid)

    def test_image_content_requires_nonempty_valid_base64_and_image_mime(self):
        self.assertTrue(MODULE.valid_image_content({"content": [{"type": "image", "mimeType": "image/png", "data": "eA=="}]}))
        self.assertFalse(MODULE.valid_image_content({"content": [{"type": "image", "mimeType": "text/plain", "data": "eA=="}]}))
        self.assertFalse(MODULE.valid_image_content({"content": [{"type": "image", "mimeType": "image/png", "data": ""}]}))
        self.assertFalse(MODULE.valid_image_content({"content": [{"type": "image", "mimeType": "image/png", "data": "***"}]}))


if __name__ == "__main__":
    unittest.main()
