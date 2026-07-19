#!/usr/bin/env python3
"""Run the deterministic HarbourGrid MCP retrieval, latency, and resilience evaluation."""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import pathlib
import re
import statistics
import threading
import time
import urllib.error
import urllib.request
from typing import Any

ASSET_RE = re.compile(r"\[asset:([a-z0-9-]+):([^\]]+)]")
DASHES = str.maketrans({"‐": "-", "‑": "-", "‒": "-", "–": "-", "—": "-", "−": "-"})


def normalized_evidence_text(value: str) -> str:
    return " ".join(value.translate(DASHES).casefold().split())


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


OPENER = urllib.request.build_opener(NoRedirect())


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, max(0, round((len(ordered) - 1) * fraction)))]


def load_secret(path: pathlib.Path | None) -> str | None:
    if path is None:
        return None
    value = path.read_text(encoding="utf-8").strip()
    if not value:
        raise SystemExit(f"credential file is empty: {path}")
    return value


class McpClient:
    def __init__(self, endpoint: str, api_key: str | None, bearer: str | None):
        if not endpoint.startswith(("http://127.0.0.1:", "https://")) or not endpoint.endswith("/mcp"):
            raise SystemExit("endpoint must be exact loopback HTTP or public HTTPS /mcp")
        if api_key and bearer:
            raise SystemExit("provide at most one of --api-key-file or --bearer-file")
        if not api_key and not bearer and not endpoint.startswith("http://127.0.0.1:"):
            raise SystemExit("a public endpoint requires --api-key-file or --bearer-file")
        self.endpoint = endpoint
        self.headers = {
            "Accept": "application/json, text/event-stream",
            "Content-Type": "application/json",
            "MCP-Protocol-Version": "2025-06-18",
        }
        if api_key:
            self.headers["X-API-Key"] = api_key
        if bearer:
            self.headers["Authorization"] = f"Bearer {bearer}"
        self.next_id = 1
        self.id_lock = threading.Lock()

    def rpc(self, method: str, params: dict[str, Any] | None = None) -> tuple[dict[str, Any], float, int]:
        with self.id_lock:
            request_id = self.next_id
            self.next_id += 1
        body = json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, **({"params": params} if params is not None else {})}, separators=(",", ":")).encode()
        started = time.perf_counter()
        request = urllib.request.Request(self.endpoint, data=body, headers=self.headers, method="POST")
        try:
            response = OPENER.open(request, timeout=180)
            raw = response.read()
            status = response.status
        except urllib.error.HTTPError as error:
            raw = error.read()
            status = error.code
        elapsed_ms = (time.perf_counter() - started) * 1000
        if status != 200:
            raise RuntimeError(f"{method} failed with HTTP {status}")
        try:
            value = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RuntimeError(f"{method} returned invalid JSON") from error
        if value.get("error"):
            raise RuntimeError(f"{method} returned JSON-RPC error {value['error'].get('code')}")
        return value, elapsed_ms, len(raw)

    def tool(self, name: str, arguments: dict[str, Any]) -> tuple[Any, float, int, dict[str, Any]]:
        envelope, elapsed_ms, size = self.rpc("tools/call", {"name": name, "arguments": arguments})
        result = envelope.get("result", {})
        if result.get("isError"):
            text = "\n".join(str(item.get("text", "")) for item in result.get("content", []) if isinstance(item, dict))
            raise RuntimeError(f"{name} returned a tool error: {text[:240]}")
        texts = [str(item.get("text", "")) for item in result.get("content", []) if isinstance(item, dict) and item.get("type") == "text"]
        if not texts:
            return result, elapsed_ms, size, result
        try:
            return json.loads(texts[-1]), elapsed_ms, size, result
        except json.JSONDecodeError:
            return texts[-1], elapsed_ms, size, result


def document_ids(payload: dict[str, Any]) -> set[str]:
    ids: set[str] = set()
    for key in ("hits", "title_hits"):
        for hit in payload.get(key, []):
            document = hit.get("document", {}) if isinstance(hit, dict) else {}
            if isinstance(document.get("native_id"), str):
                ids.add(document["native_id"])
    return ids


def chunk_for_document(payload: dict[str, Any], native_ids: set[str]) -> dict[str, Any]:
    for hit in payload.get("hits", []):
        if hit.get("document", {}).get("native_id") in native_ids and isinstance(hit.get("chunk"), dict):
            return hit["chunk"]
    raise RuntimeError("expected document had no retrievable chunk hit")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--api-key-file", type=pathlib.Path)
    parser.add_argument("--bearer-file", type=pathlib.Path)
    parser.add_argument("--manifest", type=pathlib.Path, default=pathlib.Path("tests/evals/harbourgrid.json"))
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--repetitions", type=int, default=3)
    args = parser.parse_args()
    if not 1 <= args.repetitions <= 10:
        raise SystemExit("--repetitions must be between 1 and 10")

    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    client = McpClient(args.endpoint, load_secret(args.api_key_file), load_secret(args.bearer_file))
    failures: list[str] = []
    records: list[dict[str, Any]] = []

    initialized, latency, _ = client.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "harbourgrid-eval", "version": "1"}})
    if initialized.get("result", {}).get("serverInfo", {}).get("name") != "australian-legal-mcp":
        failures.append("initialize returned the wrong server identity")
    records.append({"case": "initialize", "latency_ms": round(latency, 3)})

    listed, latency, _ = client.rpc("tools/list")
    names = [tool.get("name") for tool in listed.get("result", {}).get("tools", [])]
    if len(names) != len(manifest["expected_tools"]) or set(names) != set(manifest["expected_tools"]):
        failures.append(f"tool list mismatch: {names}")
    records.append({"case": "tools-list", "latency_ms": round(latency, 3)})

    stats, latency, size, _ = client.tool("stats", {})
    if size > manifest["maximum_stats_bytes"]:
        failures.append(f"stats response is {size} bytes")
    if set(stats.get("source_stats", {})) != {"ato", "federal-court", "frl", "high-court", "nsw-caselaw", "nsw-legislation", "qld-legislation", "sa-legislation", "tas-legislation", "wa-legislation"}:
        failures.append("stats did not expose exactly ten registered sources")
    if not stats.get("source_stats", {}).get("ato", {}).get("types"):
        failures.append("stats.source_stats.ato.types is missing")
    records.append({"case": "stats", "latency_ms": round(latency, 3), "bytes": size})

    keyword_latencies: list[float] = []
    hybrid_latencies: list[float] = []
    retrieval_latencies: list[float] = []
    for case in manifest["search_cases"]:
        search_args = {"source": case["source"], "mode": case["mode"], "query": case["query"], "doc_scope": case["doc_scope"], "k": 8}
        # One unmeasured warm-up establishes immutable range/eligibility caches.
        client.tool("search", search_args)
        payload = None
        samples: list[float] = []
        for _ in range(args.repetitions):
            payload, elapsed, _, _ = client.tool("search", search_args)
            samples.append(elapsed)
        assert isinstance(payload, dict)
        expected = set(case["expected_documents"])
        if not expected.issubset(document_ids(payload)):
            failures.append(f"{case['id']}: expected documents not returned")
            continue
        if case["mode"] == "keyword":
            keyword_latencies.extend(samples)
        else:
            hybrid_latencies.extend(samples)
        chunk = chunk_for_document(payload, expected)
        chunks, chunk_ms, _, _ = client.tool("get_chunks", {"chunks": [chunk], "before": 2, "after": 8, "max_chars": 100000})
        retrieval_latencies.append(chunk_ms)
        rendered = json.dumps(chunks, ensure_ascii=False)
        normalized_rendered = normalized_evidence_text(rendered)
        for expected_text in case.get("expected_text", []):
            if normalized_evidence_text(expected_text) not in normalized_rendered:
                failures.append(f"{case['id']}: retrieved context omitted {expected_text!r}")
        if case.get("requires_asset"):
            match = ASSET_RE.search(rendered)
            if not match:
                failures.append(f"{case['id']}: formula context exposed no typed asset")
            else:
                _, asset_ms, _, raw_result = client.tool("get_asset", {"asset": {"source": match.group(1), "asset_id": match.group(2)}})
                retrieval_latencies.append(asset_ms)
                if not any(isinstance(item, dict) and item.get("type") == "image" for item in raw_result.get("content", [])):
                    failures.append(f"{case['id']}: get_asset returned no image")
        document = {"source": case["source"], "native_id": case["expected_documents"][0]}
        anchors, anchor_ms, _, _ = client.tool("get_doc_anchors", {"document": document})
        retrieval_latencies.append(anchor_ms)
        if not isinstance(anchors, dict) or anchors.get("document") != document:
            failures.append(f"{case['id']}: anchor response identity mismatch")
        records.append({"case": case["id"], "source": case["source"], "mode": case["mode"], "search_ms": [round(value, 3) for value in samples], "get_chunks_ms": round(chunk_ms, 3)})

    definition, elapsed, _, _ = client.tool("get_definition", manifest["definition_case"])
    retrieval_latencies.append(elapsed)
    if not isinstance(definition, dict) or not definition.get("definitions"):
        failures.append("definition case returned no definitions")

    fetched, elapsed, _, _ = client.tool("fetch", manifest["fetch_case"])
    retrieval_latencies.append(elapsed)
    if not isinstance(fetched, dict) or fetched.get("uri") != manifest["fetch_case"]["uri"]:
        failures.append("fetch case returned the wrong canonical URI")

    def ready_probe() -> float:
        ready_url = args.endpoint.removesuffix("/mcp") + "/readyz"
        started = time.perf_counter()
        response = OPENER.open(urllib.request.Request(ready_url, method="GET"), timeout=5)
        if response.status != 200:
            raise RuntimeError(f"readyz returned HTTP {response.status}")
        response.read()
        return (time.perf_counter() - started) * 1000

    load_case = next(case for case in manifest["search_cases"] if case["mode"] == "hybrid")
    load_args = {"source": load_case["source"], "mode": "hybrid", "query": load_case["query"], "doc_scope": load_case["doc_scope"], "k": 8}
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
        futures = [executor.submit(client.tool, "search", load_args) for _ in range(4)]
        time.sleep(0.02)
        ready_ms = ready_probe()
        for future in futures:
            future.result()
    if ready_ms > manifest["latency_slo_ms"]["ready_during_load"]:
        failures.append(f"readyz under load took {ready_ms:.1f} ms")
    records.append({"case": "ready-during-load", "latency_ms": round(ready_ms, 3)})

    metrics = {
        "keyword_warm_p95": percentile(keyword_latencies, 0.95),
        "hybrid_warm_p95": percentile(hybrid_latencies, 0.95),
        "retrieval_p95": percentile(retrieval_latencies, 0.95),
    }
    for name, actual in metrics.items():
        budget = manifest["latency_slo_ms"][name]
        if actual > budget:
            failures.append(f"{name} {actual:.1f} ms exceeds {budget} ms")
    report = {
        "schema_version": 1,
        "evaluation": manifest["name"],
        "endpoint": args.endpoint,
        "active_generation": stats.get("active_generation"),
        "counts": {key: stats.get(key) for key in ("documents", "chunks", "chunk_embeddings", "definitions")},
        "metrics_ms": {key: round(value, 3) for key, value in metrics.items()},
        "records": records,
        "failures": failures,
        "passed": not failures,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps({"output": str(args.output), "passed": not failures, "failures": len(failures)}, sort_keys=True))
    return 0 if not failures else 1


if __name__ == "__main__":
    raise SystemExit(main())
