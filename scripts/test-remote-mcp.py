#!/usr/bin/env python3
"""Probe a public authenticated MCP without following redirects or printing credentials."""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

EXPECTED_TOOLS = {
    "search",
    "get_chunks",
    "get_asset",
    "get_doc_anchors",
    "get_definition",
    "stats",
    "fetch",
}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


OPENER = urllib.request.build_opener(NoRedirect())


def require_success_json(status: int, body: bytes, operation: str) -> dict:
    if status != 200:
        raise SystemExit(f"authenticated {operation} failed with HTTP {status}")
    try:
        value = json.loads(body)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SystemExit(f"authenticated {operation} returned invalid JSON") from error
    if not isinstance(value, dict):
        raise SystemExit(f"authenticated {operation} returned a non-object JSON response")
    return value


def request(
    url: str,
    *,
    body: dict | None = None,
    token: str | None = None,
    api_key: str | None = None,
):
    headers = {"Accept": "application/json, text/event-stream"}
    data = None
    method = "GET"
    if body is not None:
        method = "POST"
        data = json.dumps(body, separators=(",", ":")).encode()
        headers.update(
            {
                "Content-Type": "application/json",
                "MCP-Protocol-Version": "2025-06-18",
            }
        )
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if api_key:
        headers["X-API-Key"] = api_key
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with OPENER.open(req, timeout=30) as response:
            return response.status, dict(response.headers), response.read()
    except urllib.error.HTTPError as error:
        try:
            return error.code, dict(error.headers), error.read()
        finally:
            error.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("endpoint")
    required_auth = parser.add_mutually_exclusive_group()
    required_auth.add_argument(
        "--require-token",
        action="store_true",
        help="fail unless LEGAL_MCP_TEST_ACCESS_TOKEN is set and accepted",
    )
    required_auth.add_argument(
        "--require-api-key",
        action="store_true",
        help="read one API key from stdin and fail unless it is accepted",
    )
    parser.add_argument(
        "--tools",
        type=Path,
        help="exact mcp-tools.json snapshot expected from authenticated tools/list",
    )
    args = parser.parse_args()
    endpoint = urllib.parse.urlsplit(args.endpoint)
    if (
        endpoint.scheme != "https"
        or not endpoint.hostname
        or endpoint.username is not None
        or endpoint.password is not None
        or endpoint.port is not None
        or endpoint.netloc != endpoint.hostname
        or endpoint.path != "/mcp"
        or endpoint.query
        or endpoint.fragment
    ):
        parser.error("endpoint must be a canonical https://HOST/mcp URL")
    authority = urllib.parse.urlunsplit(("https", endpoint.netloc, "", "", ""))
    metadata_url = f"{authority}/.well-known/oauth-protected-resource/mcp"

    metadata_status, _, metadata_body = request(metadata_url)
    metadata_state = "not-advertised"
    if metadata_status == 200:
        metadata = json.loads(metadata_body)
        if metadata.get("resource") != args.endpoint or not metadata.get(
            "authorization_servers"
        ):
            raise SystemExit("protected-resource metadata does not bind the endpoint")
        metadata_state = "ok"
    elif metadata_status != 404:
        raise SystemExit(f"protected-resource metadata returned HTTP {metadata_status}")

    status, headers, _ = request(
        args.endpoint,
        body={"jsonrpc": "2.0", "id": 1, "method": "ping"},
    )
    challenge = headers.get("WWW-Authenticate") or headers.get("Www-Authenticate")
    has_bearer = bool(challenge and "Bearer " in challenge)
    has_api_key = bool(challenge and "ApiKey realm=" in challenge)
    if status != 401 or not challenge or not (has_bearer or has_api_key):
        raise SystemExit("unauthenticated MCP request did not return an auth challenge")
    if has_bearer and metadata_state != "ok":
        raise SystemExit("Bearer challenge has no valid protected-resource metadata")

    token = os.environ.get("LEGAL_MCP_TEST_ACCESS_TOKEN")
    api_key = None
    if args.require_api_key:
        raw_key = sys.stdin.buffer.readline(257)
        if len(raw_key) > 256 or sys.stdin.buffer.read(1):
            raise SystemExit("API key stdin is oversized")
        try:
            api_key = raw_key.decode("ascii").rstrip("\r\n")
        except UnicodeDecodeError as error:
            raise SystemExit("API key stdin is not ASCII") from error
    if token and api_key:
        raise SystemExit("set only one test authentication credential")
    if args.require_token and not token:
        raise SystemExit("LEGAL_MCP_TEST_ACCESS_TOKEN is required")
    if args.require_api_key and not api_key:
        raise SystemExit("an API key is required on stdin")
    if not token and not api_key:
        print(
            json.dumps(
                {
                    "endpoint": args.endpoint,
                    "oauth_metadata": metadata_state,
                    "unauthenticated_challenge": "ok",
                    "authenticated_probe": "skipped",
                },
                sort_keys=True,
            )
        )
        return 0
    if token and (len(token) > 16 * 1024 or any(character.isspace() for character in token)):
        raise SystemExit("LEGAL_MCP_TEST_ACCESS_TOKEN is malformed")
    if api_key and not re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,63}\.[A-Za-z0-9_-]{43}", api_key):
        raise SystemExit("LEGAL_MCP_TEST_API_KEY is malformed")
    if args.tools is None:
        raise SystemExit("--tools is required for an authenticated probe")
    expected_document = json.loads(args.tools.read_text(encoding="utf-8"))
    if not isinstance(expected_document, dict) or set(expected_document) != {"tools"}:
        raise SystemExit("--tools must be the exact exported tools/list result")
    expected_tools = expected_document["tools"]
    if (
        not isinstance(expected_tools, list)
        or len(expected_tools) != 7
        or {tool.get("name") for tool in expected_tools if isinstance(tool, dict)}
        != EXPECTED_TOOLS
    ):
        raise SystemExit("--tools does not contain the exact seven tool descriptors")

    status, _, body = request(
        args.endpoint,
        token=token,
        api_key=api_key,
        body={
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "hosted-verifier", "version": "1"},
            },
        },
    )
    initialized = require_success_json(status, body, "initialize")
    if initialized.get("result", {}).get("serverInfo", {}).get("name") != "australian-legal-mcp":
        raise SystemExit("authenticated initialize returned the wrong server identity")
    status, _, body = request(
        args.endpoint,
        token=token,
        api_key=api_key,
        body={"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
    )
    listed = require_success_json(status, body, "tools/list")
    remote_tools = listed.get("result", {}).get("tools", [])
    names = {tool.get("name") for tool in remote_tools if isinstance(tool, dict)}
    if remote_tools != expected_tools:
        raise SystemExit("authenticated tools/list differs from the exact rendered snapshot")
    print(
        json.dumps(
            {
                "endpoint": args.endpoint,
                "oauth_metadata": metadata_state,
                "unauthenticated_challenge": "ok",
                "authenticated_probe": "ok",
                "auth_method": "entra" if token else "api-key",
                "tools": sorted(names),
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
