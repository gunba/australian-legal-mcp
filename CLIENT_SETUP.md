# Client setup

Australian Legal MCP is a public Streamable HTTP endpoint with authenticated,
read-only legal-research tools. The current validation endpoint is:

```text
https://139-144-99-80.ip.linodeusercontent.com/mcp
```

This Linode hostname is temporary. Replace it with the organisation's stable
DNS name before a durable production integration.

The server exposes exactly `search`, `get_chunks`, `get_asset`,
`get_doc_anchors`, `get_definition`, `stats`, and `fetch`. Every source-scoped
call must name one of the ten registered sources. The server never implicitly
federates sources or defaults to ATO.

> Legal research infrastructure, not legal or tax advice. Verify authority,
> status, point-in-time applicability, and facts before relying on an answer.

## Authentication and secret handling

The live test deployment currently uses an individually revocable API key:

```http
X-API-Key: KEY_ID.BASE64URL_SECRET
```

Create a separate key ID for each client or team. Never put a key in a
repository, shared `.mcp.json`, prompt, command argument, image, log, or
Terraform state. A client that cannot protect a custom header must use the
future OAuth/Entra deployment instead; do not weaken the server to accommodate
it.

The examples below use `YOUR_API_KEY` only as a placeholder. User-private files
containing a key must be mode `0600`. Prefer an OS credential store and a
dynamic header helper where the client supports one.

## Compatibility matrix

| Client | Current API-key endpoint | OAuth/Entra endpoint | Recommended path |
|---|---|---|---|
| Pi | Yes, custom HTTP headers | Yes, via `pi-mcp-adapter` OAuth | Pi-owned private config |
| Claude Code | Yes, `headersHelper` or private headers | Yes | Dynamic keychain helper |
| Codex CLI / IDE | Yes, private `http_headers` | Yes | Private `config.toml`; OAuth for teams |
| ChatGPT desktop MCP host | Shares Codex MCP configuration | Yes | Codex configuration |
| Claude.ai / Claude Desktop custom connector | No arbitrary API-key header in the cloud connector flow | Yes | Configure OAuth first |
| ChatGPT web developer-mode app | Do not provision a shared custom API-key header | Yes | Configure OAuth first |
| OpenAI Responses API remote MCP tool | OAuth bearer token only | Yes | Pass a short-lived access token in `authorization` |
| Microsoft Copilot Studio / Microsoft 365 | Do not use API keys for user identity | Yes | Delegated Entra/OBO |

## Pi

Pi deliberately has no built-in MCP client, so the generic
[`pi-mcp-adapter`](https://www.npmjs.com/package/pi-mcp-adapter) remains
required. The Australian Legal MCP plugin/package is **not** required for the
remote service.

```bash
pi install npm:pi-mcp-adapter@2.11.0
```

Put the client-specific key only in Pi's private override, not a project file:

```json
{
  "mcpServers": {
    "australian-legal": {
      "url": "https://139-144-99-80.ip.linodeusercontent.com/mcp",
      "headers": {
        "X-API-Key": "YOUR_API_KEY"
      },
      "lifecycle": "lazy",
      "requestTimeoutMs": 180000,
      "directTools": true
    }
  }
}
```

Save this as `~/.pi/agent/mcp.json` and run:

```bash
chmod 600 ~/.pi/agent/mcp.json
```

Restart Pi, then use `/mcp reconnect australian-legal`. Seven tools are small
enough to expose directly; proxy mode is also valid if `directTools` is
omitted. The 180-second timeout accommodates first model load and the unusually
large global `stats` response.

This workstation is configured under the collision-free name
`australian-legal-remote`; the superseded user-global local ATO server and ATO
Pi package were removed. The credential remains redacted from repository files
and terminal output.

## Claude Code

Claude Code recommends HTTP for remote MCP and supports a dynamic
`headersHelper`. On Linux, store the client key in Secret Service:

```bash
secret-tool store \
  --label='Australian Legal MCP' \
  service australian-legal-mcp \
  key-id YOUR_KEY_ID \
  endpoint 139-144-99-80.ip.linodeusercontent.com
```

Create `~/.local/bin/australian-legal-mcp-headers`:

```bash
#!/usr/bin/env bash
set -euo pipefail
umask 077
key="$(secret-tool lookup \
  service australian-legal-mcp \
  key-id YOUR_KEY_ID \
  endpoint 139-144-99-80.ip.linodeusercontent.com)"
[[ -n "$key" ]]
printf '%s' "$key" | python3 -c '
import json, sys
key = sys.stdin.read()
print(json.dumps({"X-API-Key": key}, separators=(",", ":")))
'
unset key
```

```bash
chmod 700 ~/.local/bin/australian-legal-mcp-headers
```

Add this user-scoped server with `claude mcp add-json`, or place the equivalent
entry in a private Claude configuration:

```json
{
  "mcpServers": {
    "australian-legal": {
      "type": "http",
      "url": "https://139-144-99-80.ip.linodeusercontent.com/mcp",
      "headersHelper": "/home/YOU/.local/bin/australian-legal-mcp-headers",
      "timeout": 180000
    }
  }
}
```

Run `claude mcp list`, then inspect `/mcp` inside Claude Code. Do not commit a
project `.mcp.json` containing a static key. Claude Code also supports
`${VAR}` expansion in `headers`, but the helper avoids retaining the plaintext
key in a long-lived process environment.

Official reference: [Connect Claude Code to tools via MCP](https://docs.anthropic.com/en/docs/claude-code/mcp).

## Codex CLI, Codex IDE extension, and ChatGPT desktop MCP host

These clients share Codex MCP configuration. Add the following to the private
`~/.codex/config.toml`:

```toml
[mcp_servers.australian-legal]
url = "https://139-144-99-80.ip.linodeusercontent.com/mcp"
http_headers = { "X-API-Key" = "YOUR_API_KEY" }
tool_timeout_sec = 180
startup_timeout_sec = 30
required = true
default_tools_approval_mode = "auto"
```

```bash
chmod 600 ~/.codex/config.toml
```

All seven tools are read-only, but legal professionals should still review the
queries and returned authorities. Codex also supports `env_http_headers`; use
that only where the organisation explicitly permits credentials in process
environments. For OAuth, remove `http_headers` and run
`codex mcp login australian-legal` after the hosted Entra/OAuth mode is
configured.

In ChatGPT desktop, the same server can be managed under **Settings → MCP
servers** and then **Restart**. The CLI/IDE/desktop host read the same
`config.toml`.

Official references:

- [Codex MCP configuration](https://developers.openai.com/codex/mcp)
- [Codex configuration reference](https://developers.openai.com/codex/config-reference)

## Claude.ai and Claude Desktop custom connectors

Remote custom connectors are brokered from Anthropic's cloud, even when used
through Claude Desktop. Add the exact public `/mcp` URL under **Settings →
Connectors** only after the server is configured for standards-based OAuth and
Anthropic's caller has been allowlisted. The current API-key-only deployment
has no safe cloud-connector field for a private `X-API-Key`, so it must not be
added as an anonymous or shared-key connector.

Official reference: [Get started with custom connectors using remote MCP](https://support.anthropic.com/en/articles/11175166-get-started-with-custom-connectors-using-remote-mcp).

## ChatGPT web developer mode

ChatGPT Business, Enterprise, and Edu can use remote MCP apps in developer mode.
Register the exact `/mcp` URL only after OAuth is enabled and organisational
administrators have reviewed the server and its seven read-only tools. Do not
paste an API key into instructions or a conversation. The current API-key-only
endpoint is intended for controlled local clients, not a ChatGPT cloud app.

Official reference: [Developer mode and MCP apps in ChatGPT](https://help.openai.com/en/articles/12584461-developer-mode-and-full-mcp-connectors-in-chatgpt-beta).

## OpenAI Responses API

The Responses API's remote MCP tool accepts a public `server_url` and, for an
authenticated server, an OAuth bearer access token in `authorization`. It does
not provide an arbitrary `X-API-Key` header field. After OAuth/Entra is enabled:

```python
from openai import OpenAI

client = OpenAI()
response = client.responses.create(
    model="gpt-5.6",
    input="Find the relevant Australian authorities, naming one source per search.",
    tools=[{
        "type": "mcp",
        "server_label": "australian_legal",
        "server_description": "Read-only official Australian legal research",
        "server_url": "https://LEGAL_HOST/mcp",
        "authorization": short_lived_access_token,
        "allowed_tools": [
            "search", "get_chunks", "get_asset", "get_doc_anchors",
            "get_definition", "stats", "fetch",
        ],
        "require_approval": "never",
    }],
)
```

Send the short-lived token on every Responses API request; OpenAI does not store
it in the response object. Preserve `mcp_list_tools` in conversation context to
avoid repeated tool-discovery latency.

Official reference: [MCP and Connectors](https://platform.openai.com/docs/guides/tools-connectors-mcp).

## Microsoft Copilot Studio and Microsoft 365

Use delegated single-tenant Entra identity and the documented OBO/custom
connector path. API keys are not a substitute for end-user identity. Follow
[MICROSOFT_COPILOT.md](MICROSOFT_COPILOT.md), including exact tenant, audience,
scope, caller-app allowlists, consent, DLP, and negative-token tests.

## Verification prompt

After setup, ask the client:

> Use Australian Legal MCP only. List exactly seven tools, call `stats`, then
> search `research and development tax incentive` with source `ato`, keyword
> mode, and `k=1`. Report the active generation, counts, typed document and
> chunk references, and stored official URL. Do not infer a source or use web
> search.

Expected generation:

```text
a6e7da47edf2c332dbe616b2014a8b63dbdd9e793065c85da959cf56a2791aa3
```

A successful connection must show 409,528 documents, 6,968,250 chunks and
embeddings, 20,170 definitions, and all ten sources. Treat any different
identity as the wrong server or corpus.

Documentation links and product behaviour were checked on 2026-07-19; provider
UIs and plan availability can change, so recheck the linked official guidance
before an enterprise rollout.
