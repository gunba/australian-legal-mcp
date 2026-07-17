#!/usr/bin/env python3
"""Render tenant-specific Copilot Studio and Microsoft 365 MCP assets."""

from __future__ import annotations

import argparse
import ipaddress
import json
import os
import pathlib
import re
import tempfile

EXPECTED_TOOLS = {
    "search",
    "get_chunks",
    "get_asset",
    "get_doc_anchors",
    "get_definition",
    "stats",
    "fetch",
}
UUID_RE = re.compile(r"^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$")
REGISTRATION_RE = re.compile(r"^[A-Za-z0-9._-]{1,256}$")


def is_canonical_public_host(value: str) -> bool:
    if len(value) > 253 or "." not in value or not value.isascii():
        return False
    try:
        ipaddress.ip_address(value)
    except ValueError:
        pass
    else:
        return False
    labels = value.split(".")
    return all(
        label
        and len(label) <= 63
        and label[0].isalnum()
        and label[-1].isalnum()
        and all(character.islower() or character.isdigit() or character == "-" for character in label)
        for label in labels
    )


def replace_strings(value, replacements: dict[str, str]):  # noqa: ANN001
    if isinstance(value, str):
        for before, after in replacements.items():
            value = value.replace(before, after)
        return value
    if isinstance(value, list):
        return [replace_strings(item, replacements) for item in value]
    if isinstance(value, dict):
        return {
            replace_strings(key, replacements): replace_strings(item, replacements)
            for key, item in value.items()
        }
    return value


def atomic_write(path: pathlib.Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary = pathlib.Path(temporary_name)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        temporary.unlink(missing_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tools", required=True, type=pathlib.Path)
    parser.add_argument("--public-host", required=True)
    parser.add_argument("--tenant-id", required=True)
    parser.add_argument("--server-app-id", required=True)
    parser.add_argument("--scope", default="legal.read")
    parser.add_argument(
        "--sso-registration-id",
        help="Teams Developer Portal SSO registration ID; omit for Copilot Studio-only output",
    )
    parser.add_argument("--output-dir", required=True, type=pathlib.Path)
    args = parser.parse_args()

    if not is_canonical_public_host(args.public_host):
        parser.error("--public-host must be a canonical lowercase DNS name")
    if not UUID_RE.fullmatch(args.tenant_id) or not UUID_RE.fullmatch(
        args.server_app_id
    ):
        parser.error("tenant and server app IDs must be canonical lowercase UUIDs")
    if not re.fullmatch(r"[A-Za-z0-9._-]{1,128}", args.scope):
        parser.error("scope name is malformed")
    if args.sso_registration_id and not REGISTRATION_RE.fullmatch(args.sso_registration_id):
        parser.error("SSO registration ID is malformed")

    if not args.tools.is_file() or args.tools.is_symlink() or args.tools.stat().st_size > 1024 * 1024:
        parser.error("--tools must be a bounded regular non-symlink file")
    tools_document = json.loads(args.tools.read_text(encoding="utf-8"))
    if not isinstance(tools_document, dict) or set(tools_document) != {"tools"}:
        parser.error("tools document must be the exact tools/list result object")
    tools = tools_document["tools"]
    if (
        not isinstance(tools, list)
        or len(tools) != 7
        or any(not isinstance(tool, dict) for tool in tools)
        or {tool.get("name") for tool in tools} != EXPECTED_TOOLS
    ):
        parser.error("tools document must contain exactly the seven legal-mcp tools")
    for tool in tools:
        if set(tool) != {"name", "description", "inputSchema", "annotations"}:
            parser.error("every MCP tool descriptor must have the exact exported shape")
        if not isinstance(tool["description"], str) or not tool["description"]:
            parser.error("every MCP tool needs a nonempty description")
        schema = tool["inputSchema"]
        if (
            not isinstance(schema, dict)
            or schema.get("type") != "object"
            or schema.get("additionalProperties") is not False
            or not isinstance(schema.get("properties"), dict)
        ):
            parser.error("every MCP tool needs a closed object input schema")
        annotations = tool.get("annotations")
        expected_annotations = {
            "readOnlyHint": True,
            "destructiveHint": False,
            "idempotentHint": True,
            "openWorldHint": tool["name"] == "fetch",
        }
        if annotations != expected_annotations:
            parser.error("every exported MCP tool must have the exact read-only annotations")

    repo = pathlib.Path(__file__).resolve().parents[1]
    scope_uri = f"api://{args.server_app_id}/{args.scope}"
    replacements = {
        "__PUBLIC_HOST__": args.public_host,
        "__TENANT_ID__": args.tenant_id,
        "__SCOPE_URI__": scope_uri,
    }
    if args.sso_registration_id:
        replacements["__SSO_REGISTRATION_ID__"] = args.sso_registration_id

    connector_template = (
        repo / "integrations/copilot-studio/connector.swagger.template.yaml"
    ).read_text(encoding="utf-8")
    for before, after in replacements.items():
        connector_template = connector_template.replace(before, after)
    if "__" in connector_template:
        parser.error("unresolved placeholder remains in connector template")

    encoded_tools = json.dumps(tools_document, indent=2, sort_keys=True) + "\n"

    rendered = {
        "copilot-studio-connector.swagger.yaml": connector_template.encode("utf-8"),
        "mcp-tools.json": encoded_tools.encode("utf-8"),
    }
    outputs = [
        "copilot-studio-connector.swagger.yaml",
        "mcp-tools.json",
    ]
    if args.sso_registration_id:
        plugin = json.loads(
            (repo / "integrations/microsoft-365/ai-plugin.template.json").read_text(
                encoding="utf-8"
            )
        )
        plugin = replace_strings(plugin, replacements)
        plugin["runtimes"][0]["spec"]["mcp_tool_description"]["tools"] = tools
        encoded_plugin = json.dumps(plugin, indent=2, sort_keys=True) + "\n"
        if "__" in encoded_plugin:
            parser.error("unresolved placeholder remains in plugin template")
        rendered["microsoft-365-ai-plugin.json"] = encoded_plugin.encode("utf-8")
        outputs.append("microsoft-365-ai-plugin.json")
    if args.output_dir.exists() or args.output_dir.is_symlink():
        parser.error("--output-dir must not already exist; remove the old snapshot first")
    args.output_dir.parent.mkdir(parents=True, exist_ok=True)
    if args.output_dir.parent.is_symlink() or not args.output_dir.parent.is_dir():
        parser.error("--output-dir parent must be a real directory")
    with tempfile.TemporaryDirectory(
        prefix=f".{args.output_dir.name}.", dir=args.output_dir.parent
    ) as temporary_dir:
        staging = pathlib.Path(temporary_dir)
        for name, data in rendered.items():
            atomic_write(staging / name, data)
        os.replace(staging, args.output_dir)
        directory = os.open(
            args.output_dir.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
        )
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    print(
        json.dumps(
            {
                "output_dir": str(args.output_dir),
                "mcp_url": f"https://{args.public_host}/mcp",
                "scope_uri": scope_uri,
                "tools": sorted(EXPECTED_TOOLS),
                "outputs": outputs,
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
