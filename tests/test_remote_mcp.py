import http.server
import importlib.util
import pathlib
import sys
import threading
import unittest


SCRIPT = pathlib.Path(__file__).parents[1] / "scripts" / "test-remote-mcp.py"
SPEC = importlib.util.spec_from_file_location("test_remote_mcp_script", SCRIPT)
remote = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = remote
assert SPEC.loader is not None
SPEC.loader.exec_module(remote)


class _Server:
    def __init__(self, handler):
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self) -> str:
        host, port = self.server.server_address
        return f"http://{host}:{port}/mcp"

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


class RemoteMcpProbeTests(unittest.TestCase):
    def test_authenticated_http_error_is_reported_before_body_parsing(self) -> None:
        with self.assertRaisesRegex(SystemExit, "authenticated initialize failed with HTTP 401"):
            remote.require_success_json(401, b"unauthorized", "initialize")

    def test_authenticated_success_requires_object_json(self) -> None:
        with self.assertRaisesRegex(SystemExit, "returned invalid JSON"):
            remote.require_success_json(200, b"not-json", "tools/list")
        with self.assertRaisesRegex(SystemExit, "non-object JSON"):
            remote.require_success_json(200, b"[]", "tools/list")

    def test_authenticated_requests_never_follow_redirects(self) -> None:
        received_credentials = []

        class Sink(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                received_credentials.append(
                    (self.headers.get("Authorization"), self.headers.get("X-API-Key"))
                )
                self.send_response(200)
                self.end_headers()

            def log_message(self, *_args):
                pass

        sink = _Server(Sink)

        class Redirect(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                self.send_response(302)
                self.send_header("Location", sink.url)
                self.end_headers()

            def log_message(self, *_args):
                pass

        redirect = _Server(Redirect)
        try:
            status, _, _ = remote.request(
                redirect.url,
                body={"jsonrpc": "2.0", "id": 1, "method": "ping"},
                token="secret-token",
            )
            self.assertEqual(status, 302)
            status, _, _ = remote.request(
                redirect.url,
                body={"jsonrpc": "2.0", "id": 2, "method": "ping"},
                api_key="automation.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            )
            self.assertEqual(status, 302)
            self.assertEqual(received_credentials, [])
        finally:
            redirect.close()
            sink.close()


if __name__ == "__main__":
    unittest.main()
