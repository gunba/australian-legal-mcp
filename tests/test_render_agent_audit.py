import importlib.util
import json
import pathlib
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).parents[1] / "scripts" / "render-agent-audit.py"
SPEC = importlib.util.spec_from_file_location("render_agent_audit", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class RenderAgentAuditTests(unittest.TestCase):
    def test_redacts_credentials_and_uses_only_signed_reasoning_summaries(self):
        summary_signature = json.dumps(
            {
                "summary": [
                    {"type": "summary_text", "text": "Checked the official source result."}
                ]
            }
        )
        key = "client." + "A" * 43
        rows = [
            {"type": "session", "timestamp": "2026-01-01T00:00:00Z"},
            {"type": "model_change", "provider": "openai", "modelId": "test"},
            {
                "type": "message",
                "timestamp": "2026-01-01T00:00:01Z",
                "message": {
                    "role": "user",
                    "timestamp": 1000,
                    "content": [{"type": "text", "text": "Research the issue"}],
                },
            },
            {
                "type": "message",
                "timestamp": "2026-01-01T00:00:02Z",
                "message": {
                    "role": "assistant",
                    "timestamp": 100,
                    "content": [
                        {
                            "type": "thinking",
                            "thinking": "private reasoning must not be rendered",
                            "thinkingSignature": summary_signature,
                        },
                        {
                            "type": "thinking",
                            "thinking": "unsigned reasoning must not be rendered",
                        },
                        {
                            "type": "toolCall",
                            "id": "call-1",
                            "name": "australian_legal_remote_search",
                            "arguments": {
                                "source": "ato",
                                "query": "official test",
                                "X-API-Key": "short-secret",
                            },
                        },
                    ],
                },
            },
            {
                "type": "message",
                "timestamp": "2026-01-01T00:00:03Z",
                "message": {
                    "role": "toolResult",
                    "timestamp": 9999,
                    "toolCallId": "call-1",
                    "toolName": "australian_legal_remote_search",
                    "isError": False,
                    "content": [{"type": "text", "text": json.dumps({"key": key, "hits": []})}],
                },
            },
        ]
        with tempfile.TemporaryDirectory() as directory:
            session = pathlib.Path(directory) / "session.jsonl"
            session.write_text("\n".join(json.dumps(row) for row in rows) + "\n")
            report = MODULE.load_session(session)

        encoded = json.dumps(report)
        self.assertNotIn("private reasoning must not be rendered", encoded)
        self.assertNotIn("unsigned reasoning must not be rendered", encoded)
        self.assertIn("Checked the official source result.", encoded)
        self.assertNotIn("short-secret", encoded)
        self.assertNotIn(key, encoded)
        self.assertIn("[REDACTED_CREDENTIAL]", encoded)
        self.assertIn("[REDACTED_API_KEY]", encoded)
        self.assertTrue(report["calls"][0]["is_mcp"])
        self.assertEqual(report["calls"][0]["latency_ms"], 1000)
        self.assertEqual(report["calls"][0]["model_turn_started_ms"], 100)


if __name__ == "__main__":
    unittest.main()
