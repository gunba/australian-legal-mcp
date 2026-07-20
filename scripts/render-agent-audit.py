#!/usr/bin/env python3
"""Render a self-contained, redacted HTML/JSON audit from a Pi session JSONL."""

from __future__ import annotations

import argparse
import datetime as dt
import html
import json
import math
import pathlib
import re
import statistics
from typing import Any

KEY_RE = re.compile(r"\b[a-z0-9][a-z0-9_-]{0,63}\.[A-Za-z0-9_-]{43}\b")
BEARER_RE = re.compile(r"(?i)(authorization[\"']?\s*[:=]\s*[\"']?bearer\s+)[^\s\"']+")
MCP_PREFIXES = ("australian_legal_remote_", "australian_legal_")
SENSITIVE_KEYS = {"authorization", "x-api-key", "api_key", "api-key", "access_token", "refresh_token"}


def redact(value: Any) -> Any:
    if isinstance(value, str):
        value = KEY_RE.sub("[REDACTED_API_KEY]", value)
        return BEARER_RE.sub(r"\1[REDACTED_TOKEN]", value)
    if isinstance(value, list):
        return [redact(item) for item in value]
    if isinstance(value, dict):
        return {
            str(key): "[REDACTED_CREDENTIAL]"
            if str(key).lower() in SENSITIVE_KEYS
            else redact(item)
            for key, item in value.items()
        }
    return value


def epoch_ms(value: Any, fallback: Any = None) -> int | None:
    if isinstance(value, (int, float)):
        return int(value)
    if isinstance(value, str):
        try:
            return int(dt.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000)
        except ValueError:
            pass
    if fallback is not None and fallback is not value:
        return epoch_ms(fallback)
    return None


def iso_time(value: int | None) -> str:
    if value is None:
        return "unknown"
    return dt.datetime.fromtimestamp(value / 1000, tz=dt.timezone.utc).isoformat().replace("+00:00", "Z")


def percentile(values: list[int], fraction: float) -> int | None:
    if not values:
        return None
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, math.ceil(fraction * len(ordered)) - 1))
    return ordered[index]


def text_content(content: Any) -> str:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return json.dumps(content, ensure_ascii=False, sort_keys=True)
    parts: list[str] = []
    for block in content:
        if not isinstance(block, dict):
            parts.append(str(block))
        elif block.get("type") == "text":
            parts.append(str(block.get("text", "")))
        elif block.get("type") == "image":
            data = str(block.get("data", ""))
            parts.append(f"[image {block.get('mimeType', 'unknown')} base64_chars={len(data)}]")
        elif block.get("type") in {"resource", "resource_link"}:
            parts.append(json.dumps(redact(block), ensure_ascii=False, sort_keys=True))
    return "\n".join(parts)


def reasoning_summary(block: dict[str, Any]) -> str:
    signature = block.get("thinkingSignature")
    if isinstance(signature, str):
        try:
            parsed = json.loads(signature)
            summaries = parsed.get("summary", [])
            text = " ".join(
                str(item.get("text", ""))
                for item in summaries
                if isinstance(item, dict) and item.get("type") == "summary_text"
            ).strip()
            if text:
                return text
        except json.JSONDecodeError:
            pass
    return ""


def classify_tool(name: str, arguments: dict[str, Any]) -> tuple[bool, str]:
    normalized = name.replace("-", "_")
    if normalized.startswith(MCP_PREFIXES):
        return True, normalized.split("_remote_", 1)[-1] if "_remote_" in normalized else normalized
    if name == "mcp":
        requested = str(arguments.get("tool", ""))
        if requested.startswith(MCP_PREFIXES):
            return True, requested.split("_remote_", 1)[-1]
        if requested in {"search", "get_chunks", "get_asset", "get_doc_anchors", "get_definition", "stats", "fetch"}:
            return True, requested
    return False, name


def result_summary(text: str) -> str:
    clean = text.strip()
    if not clean:
        return "Empty result"
    try:
        value = json.loads(clean)
    except json.JSONDecodeError:
        line = next((line.strip() for line in clean.splitlines() if line.strip()), "")
        return line[:220] + ("…" if len(line) > 220 else "")
    if isinstance(value, dict):
        if "hits" in value and isinstance(value["hits"], list):
            hits = value["hits"]
            first = hits[0] if hits else {}
            title = first.get("title") if isinstance(first, dict) else None
            return f"{len(hits)} search hit(s)" + (f"; first: {title}" if title else "")
        if "chunks" in value and isinstance(value["chunks"], list):
            return f"{len(value['chunks'])} chunk(s) returned"
        if "definitions" in value and isinstance(value["definitions"], list):
            return f"{len(value['definitions'])} definition(s) returned"
        if "documents" in value and "chunks" in value:
            return f"stats: {value.get('documents')} documents; {value.get('chunks')} chunks"
        return "JSON object: " + ", ".join(list(value)[:8])
    if isinstance(value, list):
        return f"JSON array with {len(value)} item(s)"
    return str(value)[:220]


def load_session(path: pathlib.Path) -> dict[str, Any]:
    rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    session = next((row for row in rows if row.get("type") == "session"), {})
    model = next((row for row in rows if row.get("type") == "model_change"), {})
    calls: dict[str, dict[str, Any]] = {}
    conversation: list[dict[str, Any]] = []

    for row in rows:
        if row.get("type") != "message" or not isinstance(row.get("message"), dict):
            continue
        message = row["message"]
        role = message.get("role")
        timestamp = epoch_ms(message.get("timestamp"), row.get("timestamp"))
        dispatch_timestamp = epoch_ms(row.get("timestamp"), message.get("timestamp"))
        content = message.get("content", [])
        if role == "user":
            conversation.append({"kind": "user", "timestamp_ms": timestamp, "text": redact(text_content(content))})
        elif role == "assistant":
            for block in content if isinstance(content, list) else []:
                if not isinstance(block, dict):
                    continue
                block_type = block.get("type")
                if block_type == "thinking":
                    summary = reasoning_summary(block)
                    if summary:
                        conversation.append({"kind": "reasoning_summary", "timestamp_ms": timestamp, "text": redact(summary)})
                elif block_type == "text":
                    text = str(block.get("text", ""))
                    if text.strip():
                        conversation.append({"kind": "assistant", "timestamp_ms": timestamp, "text": redact(text)})
                elif block_type == "toolCall":
                    call_id = str(block.get("id", ""))
                    arguments = redact(block.get("arguments", {}))
                    if not isinstance(arguments, dict):
                        arguments = {"value": arguments}
                    is_mcp, display_name = classify_tool(str(block.get("name", "")), arguments)
                    calls[call_id] = {
                        "id": call_id,
                        "name": str(block.get("name", "")),
                        "display_name": display_name,
                        "arguments": arguments,
                        "is_mcp": is_mcp,
                        "started_ms": dispatch_timestamp,
                        "model_turn_started_ms": timestamp,
                        "finished_ms": None,
                        "latency_ms": None,
                        "is_error": None,
                        "result": None,
                        "result_details": None,
                    }
                    conversation.append({"kind": "tool_call", "timestamp_ms": dispatch_timestamp, "call_id": call_id})
        elif role == "toolResult":
            call_id = str(message.get("toolCallId", ""))
            result = redact(text_content(content))
            call = calls.get(call_id)
            if call is None:
                call = {
                    "id": call_id,
                    "name": str(message.get("toolName", "unknown")),
                    "display_name": str(message.get("toolName", "unknown")),
                    "arguments": {},
                    "is_mcp": False,
                    "started_ms": None,
                }
                calls[call_id] = call
            call["finished_ms"] = dispatch_timestamp
            call["latency_ms"] = dispatch_timestamp - call["started_ms"] if dispatch_timestamp is not None and call.get("started_ms") is not None else None
            call["is_error"] = bool(message.get("isError", False))
            call["result"] = result
            call["result_details"] = redact(message.get("details"))
            call["summary"] = result_summary(result)
            conversation.append({"kind": "tool_result", "timestamp_ms": dispatch_timestamp, "call_id": call_id})

    ordered_calls = sorted(calls.values(), key=lambda item: (item.get("started_ms") or 0, item["id"]))
    start_candidates = [item.get("timestamp_ms") for item in conversation if item.get("timestamp_ms") is not None]
    end_candidates = [item.get("timestamp_ms") for item in conversation if item.get("timestamp_ms") is not None]
    start_ms = min(start_candidates) if start_candidates else epoch_ms(None, session.get("timestamp"))
    end_ms = max(end_candidates) if end_candidates else start_ms
    return {
        "session": redact(session),
        "model": {"provider": model.get("provider"), "model": model.get("modelId")},
        "start_ms": start_ms,
        "end_ms": end_ms,
        "duration_ms": end_ms - start_ms if start_ms is not None and end_ms is not None else None,
        "conversation": conversation,
        "calls": ordered_calls,
    }


def esc(value: Any) -> str:
    return html.escape(str(value), quote=True)


def pretty(value: Any) -> str:
    return json.dumps(value, indent=2, ensure_ascii=False, sort_keys=True)


def render(report: dict[str, Any], case_text: str, title: str, audit_json_name: str) -> str:
    calls = report["calls"]
    mcp_calls = [call for call in calls if call.get("is_mcp")]
    latencies = [call["latency_ms"] for call in mcp_calls if isinstance(call.get("latency_ms"), int)]
    max_latency = max(latencies, default=1)
    errors = sum(1 for call in calls if call.get("is_error"))
    total_seconds = (report.get("duration_ms") or 0) / 1000

    call_rows: list[str] = []
    for index, call in enumerate(calls, 1):
        latency = call.get("latency_ms")
        width = 8 if not isinstance(latency, int) else 8 + 92 * math.sqrt(max(latency, 0) / max_latency)
        status = "error" if call.get("is_error") else "ok"
        tool_class = "mcp" if call.get("is_mcp") else "local"
        result = call.get("result") or ""
        call_rows.append(f"""
        <article class="call-card {status}" id="call-{index}">
          <div class="call-head">
            <div><span class="step">{index}</span><strong>{esc(call.get('display_name'))}</strong>
              <span class="pill {tool_class}">{tool_class.upper()}</span>
              <span class="pill {status}">{status.upper()}</span></div>
            <div class="latency">{esc(latency if latency is not None else 'n/a')} ms</div>
          </div>
          <div class="bar-track"><div class="bar {status}" style="width:{width:.1f}%"></div></div>
          <div class="call-summary">{esc(call.get('summary', 'No result recorded'))}</div>
          <details><summary>Request arguments</summary><pre>{esc(pretty(call.get('arguments', {})))}</pre></details>
          <details><summary>Exact result delivered to the agent</summary><pre>{esc(result)}</pre></details>
        </article>""")

    conversation_rows: list[str] = []
    call_index = {call["id"]: index for index, call in enumerate(calls, 1)}
    for event in report["conversation"]:
        kind = event["kind"]
        when = iso_time(event.get("timestamp_ms"))
        if kind in {"user", "assistant", "reasoning_summary"}:
            label = {"user": "Prompt / follow-up", "assistant": "Agent said", "reasoning_summary": "Reasoning summary"}[kind]
            conversation_rows.append(
                f'<article class="message {kind}"><div class="message-label">{label}<time>{esc(when)}</time></div>'
                f'<pre>{esc(event.get("text", ""))}</pre></article>'
            )
        elif kind == "tool_call":
            number = call_index.get(event.get("call_id"), "?")
            conversation_rows.append(f'<div class="event-marker">↓ request <a href="#call-{number}">call {number}</a></div>')
        elif kind == "tool_result":
            number = call_index.get(event.get("call_id"), "?")
            conversation_rows.append(f'<div class="event-marker">↑ result <a href="#call-{number}">call {number}</a></div>')

    p50 = percentile(latencies, 0.50)
    p95 = percentile(latencies, 0.95)
    mean = round(statistics.mean(latencies)) if latencies else None
    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{esc(title)}</title>
<style>
:root{{--ink:#14213d;--muted:#61708a;--paper:#f6f8fb;--card:#fff;--line:#dbe3ef;--blue:#1f6feb;--teal:#0f8b8d;--amber:#d97706;--red:#b42318;--violet:#6d5bd0}}
*{{box-sizing:border-box}} body{{margin:0;background:var(--paper);color:var(--ink);font:14px/1.5 Inter,ui-sans-serif,system-ui,-apple-system,Segoe UI,sans-serif}}
header{{background:linear-gradient(120deg,#101b35,#203f73);color:white;padding:42px max(28px,calc((100vw - 1180px)/2)) 36px}}
header h1{{font-size:34px;line-height:1.1;margin:0 0 10px}} header p{{margin:0;color:#dbe9ff;max-width:850px}}
main{{max-width:1180px;margin:0 auto;padding:28px}} h2{{font-size:22px;margin:32px 0 14px}} h3{{margin:0}}
.grid{{display:grid;grid-template-columns:repeat(6,minmax(0,1fr));gap:12px}} .metric{{background:var(--card);border:1px solid var(--line);border-radius:14px;padding:16px;box-shadow:0 4px 16px #1b315012}}
.metric b{{display:block;font-size:23px}} .metric span{{color:var(--muted);font-size:12px;text-transform:uppercase;letter-spacing:.08em}}
.notice{{border-left:4px solid var(--violet);background:#f1efff;padding:12px 15px;border-radius:8px;margin:18px 0}}
.call-card,.message,.case{{background:var(--card);border:1px solid var(--line);border-radius:14px;padding:16px;margin:12px 0;box-shadow:0 3px 14px #1b31500d}}
.call-card.error{{border-color:#f3b4ae}} .call-head{{display:flex;justify-content:space-between;gap:12px;align-items:center}} .step{{display:inline-grid;place-items:center;width:27px;height:27px;border-radius:50%;background:var(--ink);color:white;margin-right:9px}}
.pill{{display:inline-block;margin-left:8px;border-radius:999px;padding:2px 8px;font-size:10px;letter-spacing:.07em}} .pill.mcp{{background:#dff5f2;color:#075e5e}} .pill.local{{background:#e8eef8;color:#3e526e}} .pill.ok{{background:#e5f7eb;color:#176b36}} .pill.error{{background:#fee9e7;color:#a02a22}}
.latency{{font-variant-numeric:tabular-nums;font-weight:700}} .bar-track{{height:7px;background:#e8edf5;border-radius:99px;margin:12px 0;overflow:hidden}} .bar{{height:100%;background:linear-gradient(90deg,var(--blue),var(--teal));border-radius:99px}} .bar.error{{background:var(--red)}}
.call-summary{{color:var(--muted);margin-bottom:8px}} details{{border-top:1px solid var(--line);padding-top:8px;margin-top:8px}} summary{{cursor:pointer;font-weight:650;color:#31547f}} pre{{white-space:pre-wrap;word-break:break-word;margin:10px 0 0;font:12px/1.45 ui-monospace,SFMono-Regular,Consolas,monospace;max-height:520px;overflow:auto}}
.message-label{{display:flex;justify-content:space-between;font-weight:750;margin-bottom:8px}} time{{font-weight:400;color:var(--muted);font-size:11px}} .message.user{{border-left:4px solid var(--blue)}} .message.assistant{{border-left:4px solid var(--teal)}} .message.reasoning_summary{{border-left:4px solid var(--violet);background:#fbfaff}}
.event-marker{{text-align:center;color:var(--muted);font-size:12px;margin:5px}} a{{color:var(--blue)}} .case pre{{max-height:460px}} footer{{color:var(--muted);padding:30px 0 10px}}
@media(max-width:850px){{.grid{{grid-template-columns:repeat(2,1fr)}} .call-head{{align-items:flex-start;flex-direction:column}}}} @media print{{details{{display:block}} details>summary{{display:none}} .call-card,.message{{break-inside:avoid}}}}
</style>
</head>
<body>
<header><h1>{esc(title)}</h1><p>Auditable agent research trace: prompts, provider-supplied reasoning summaries, tool requests, exact returned results, and client-observed round-trip latency.</p></header>
<main>
<section class="grid">
  <div class="metric"><b>{len(calls)}</b><span>Total tool calls</span></div>
  <div class="metric"><b>{len(mcp_calls)}</b><span>MCP calls</span></div>
  <div class="metric"><b>{p50 if p50 is not None else 'n/a'} ms</b><span>MCP p50</span></div>
  <div class="metric"><b>{p95 if p95 is not None else 'n/a'} ms</b><span>MCP p95</span></div>
  <div class="metric"><b>{mean if mean is not None else 'n/a'} ms</b><span>MCP mean</span></div>
  <div class="metric"><b>{total_seconds:.1f} s</b><span>Conversation span</span></div>
</section>
<div class="notice"><strong>Interpretation:</strong> latency is measured from Pi's persisted tool-dispatch event to its matching persisted tool-result event and therefore includes local adapter, network, server, and response handling. It excludes model time before dispatch. Concurrent calls delivered as one batch may share the batch completion time. “Reasoning summary” contains only provider-supplied summary text; hidden chain-of-thought and encrypted reasoning are deliberately excluded. Credentials are pattern-redacted.</div>
<section><h2>Request and evidence timeline</h2>{''.join(call_rows)}</section>
<section><h2>Conversation trace</h2>{''.join(conversation_rows)}</section>
<section class="case"><h2>Case study supplied to the agent</h2><details><summary>Show full case</summary><pre>{esc(case_text)}</pre></details></section>
<footer>Model: {esc(report['model'].get('provider'))}/{esc(report['model'].get('model'))} · Start: {esc(iso_time(report.get('start_ms')))} · Errors: {errors} · Machine-readable audit: <a href="{esc(audit_json_name)}">{esc(audit_json_name)}</a></footer>
</main>
</body></html>"""


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--session", required=True, type=pathlib.Path)
    parser.add_argument("--case", required=True, type=pathlib.Path)
    parser.add_argument("--output-dir", required=True, type=pathlib.Path)
    parser.add_argument("--title", default="Australian Legal MCP agent audit")
    args = parser.parse_args()

    report = load_session(args.session)
    report["source_session"] = str(args.session)
    report["case_file"] = str(args.case)
    report["latency_definition"] = "Pi persisted tool-result event minus matching persisted tool-dispatch event; excludes pre-dispatch model time and may reflect a concurrent batch barrier"
    report["reasoning_policy"] = "Provider-supplied summary text only; hidden/encrypted chain-of-thought omitted"
    case_text = args.case.read_text(encoding="utf-8")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    json_path = args.output_dir / "agent-audit.json"
    html_path = args.output_dir / "index.html"
    json_path.write_text(json.dumps(redact(report), indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    html_path.write_text(render(report, case_text, args.title, json_path.name), encoding="utf-8")
    print(json.dumps({"html": str(html_path), "json": str(json_path), "calls": len(report["calls"])}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
