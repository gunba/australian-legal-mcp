# Client setup

Australian Legal MCP is a public Streamable HTTP endpoint with authenticated,
read-only legal-research tools. The validation endpoint, currently offline during configured-dark maintenance,
is:

```text
https://139-144-99-80.ip.linodeusercontent.com/mcp
```

Do not run client validation until the recovery runbook records republication.
This Linode hostname is temporary. Replace it with the organisation's stable
DNS name before a durable production integration.

The server exposes exactly `search`, `get_chunks`, `get_asset`,
`get_doc_anchors`, `get_definition`, `stats`, and `fetch`. Every source-scoped
call must name one of the ten registered sources. The server never implicitly
federates sources or defaults to ATO.

> Legal research infrastructure, not legal or tax advice. Verify authority,
> status, point-in-time applicability, and facts before relying on an answer.

## Authentication and secret handling

When republished, the test deployment uses an individually revocable API key:

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

A project `.mcp.json` or `.pi/mcp.json` can override a global server definition
field by field. Do not use a secret-bearing global definition from an untrusted
workspace: a same-named project entry could replace its URL while inheriting the
header. The enterprise-vault procedure below uses a dedicated Pi directory and
a launcher that rejects both project override paths before startup.

This workstation is configured under the collision-free name
`australian-legal-remote`; the superseded user-global local ATO server and ATO
Pi package were removed. The credential remains redacted from repository files
and terminal output.

### Enterprise Windows laptop: Pi in a Desktop Obsidian vault

Use this layout when the Obsidian vault is the agent workspace but its contents
are synchronised between devices. The MCP credential and Pi sessions remain in
the Windows user profile and **must not** be stored in the vault.

Before setup, obtain approval under the organisation's AI, legal-data, and
software-installation policies. Use a new revocable key issued specifically for
the enterprise laptop; do not copy another client's key. Start with synthetic
or public facts, not employer, client, personal, or legally privileged data.

The adapter is a full-privilege Pi extension. The organisation should review
and allowlist exactly `pi-mcp-adapter@2.11.0` and its dependency tree, preferably
through its approved npm registry. Also note that the adapter's project MCP
files override user definitions field by field. A project file could otherwise
replace the URL while inheriting a global secret header. The dedicated launcher
below therefore refuses to start when the vault contains `.mcp.json` or
`.pi\mcp.json`; Pi's project-trust prompt is not a substitute for this check.

1. Install current Node.js LTS and Git for Windows through the organisation's
   managed software channel. Pi requires Bash on Windows and automatically uses
   `C:\Program Files\Git\bin\bash.exe` when Git for Windows is installed. Do not
   disable TLS verification to bypass an enterprise proxy.
2. Open PowerShell and resolve the actual Desktop location. This works when the
   enterprise redirects Desktop into OneDrive. Change only `$vaultName` if the
   vault has another name.

   ```powershell
   $vaultName = 'Obsidian'
   $desktop = [Environment]::GetFolderPath('Desktop')
   $vault = Join-Path $desktop $vaultName
   if (-not (Test-Path -LiteralPath $vault -PathType Container)) {
       throw "Obsidian vault not found: $vault"
   }

   $agentDir = Join-Path $HOME '.pi\australian-legal-enterprise'
   $sessionDir = Join-Path $agentDir 'sessions'
   $vaultFull = [IO.Path]::GetFullPath($vault).TrimEnd('\') + '\'
   $agentFull = [IO.Path]::GetFullPath($agentDir).TrimEnd('\') + '\'
   if ($agentFull.StartsWith($vaultFull, [StringComparison]::OrdinalIgnoreCase)) {
       throw 'The private Pi directory resolves inside the synced vault.'
   }
   if (Test-Path -LiteralPath $agentDir) {
       throw "Private Pi directory already exists; review it instead of overwriting: $agentDir"
   }

   New-Item -ItemType Directory -Path $sessionDir -Force | Out-Null
   $identity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
   & icacls $agentDir /inheritance:r /grant:r "${identity}:(OI)(CI)F" | Out-Null
   if ($LASTEXITCODE -ne 0) {
       Remove-Item -LiteralPath $agentDir -Recurse -Force
       throw 'Could not make the private Pi directory user-only.'
   }
   $unexpected = (Get-Acl -LiteralPath $agentDir).Access | Where-Object {
       $_.AccessControlType -eq 'Allow' -and
       $_.IdentityReference.Value -ne $identity
   }
   if ($unexpected) {
       Remove-Item -LiteralPath $agentDir -Recurse -Force
       throw 'The private Pi directory has an unexpected allow entry.'
   }
   ```

3. Select the dedicated Pi directory for this PowerShell process, then install
   the pinned, organisation-approved Pi build and reviewed adapter. Do not add
   `-l`; project-local installation would write package state beneath the synced
   vault.

   ```powershell
   $env:PI_CODING_AGENT_DIR = $agentDir
   $env:PI_CODING_AGENT_SESSION_DIR = $sessionDir
   npm install -g --ignore-scripts @earendil-works/pi-coding-agent@0.80.10
   Set-Location -LiteralPath $agentDir
   pi install npm:pi-mcp-adapter@2.11.0 --no-approve
   pi --version
   pi list
   ```

4. Create the private MCP override. This refuses to replace an existing file,
   writes first into the already-private directory, verifies the file ACL, and
   publishes it with an atomic same-directory rename. Replace the endpoint only
   when the project documents a stable successor hostname.

   ```powershell
   $mcpPath = Join-Path $agentDir 'mcp.json'
   if (Test-Path -LiteralPath $mcpPath) {
       throw "Refusing to replace existing MCP configuration: $mcpPath"
   }
   $tempPath = Join-Path $agentDir ('.mcp.' + [guid]::NewGuid().ToString('N') + '.tmp')
   $secureKey = Read-Host 'Paste the dedicated enterprise-laptop MCP key' -AsSecureString
   $bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secureKey)
   try {
       $key = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr)
       if ([string]::IsNullOrWhiteSpace($key)) { throw 'The MCP key is empty.' }
       $config = [ordered]@{
           mcpServers = [ordered]@{
               'australian-legal-remote' = [ordered]@{
                   url = 'https://139-144-99-80.ip.linodeusercontent.com/mcp'
                   headers = [ordered]@{ 'X-API-Key' = $key }
                   lifecycle = 'lazy'
                   requestTimeoutMs = 180000
                   directTools = $true
               }
           }
       }
       $json = $config | ConvertTo-Json -Depth 8
       [IO.File]::WriteAllText($tempPath, $json, [Text.UTF8Encoding]::new($false))
       & icacls $tempPath /inheritance:r /grant:r "${identity}:F" | Out-Null
       if ($LASTEXITCODE -ne 0) { throw 'Could not restrict the MCP file ACL.' }
       $unexpected = (Get-Acl -LiteralPath $tempPath).Access | Where-Object {
           $_.AccessControlType -eq 'Allow' -and
           $_.IdentityReference.Value -ne $identity
       }
       if ($unexpected) { throw 'The MCP file has an unexpected allow entry.' }
       Move-Item -LiteralPath $tempPath -Destination $mcpPath
   }
   catch {
       Remove-Item -LiteralPath $tempPath -Force -ErrorAction SilentlyContinue
       throw
   }
   finally {
       [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
       Remove-Variable key, secureKey, config, json -ErrorAction SilentlyContinue
   }
   ```

5. Write a private launcher outside the vault. It fixes both Pi paths, checks
   that neither path is inside the vault, and rejects project MCP overrides
   before Pi or the adapter starts.

   ```powershell
   $vaultPathFile = Join-Path $agentDir 'obsidian-vault-path.txt'
   [IO.File]::WriteAllText($vaultPathFile, $vault, [Text.UTF8Encoding]::new($false))
   & icacls $vaultPathFile /inheritance:r /grant:r "${identity}:F" | Out-Null
   if ($LASTEXITCODE -ne 0) { throw 'Could not restrict the vault-path ACL.' }

   $launcher = Join-Path $agentDir 'Start-ObsidianPi.ps1'
   $launcherText = @'
   $ErrorActionPreference = 'Stop'
   $agentDir = $PSScriptRoot
   $sessionDir = Join-Path $agentDir 'sessions'
   $vault = [IO.File]::ReadAllText(
       (Join-Path $agentDir 'obsidian-vault-path.txt'),
       [Text.Encoding]::UTF8
   ).Trim()
   if (-not (Test-Path -LiteralPath $vault -PathType Container)) {
       throw "Obsidian vault not found: $vault"
   }
   $sharedMcpPath = Join-Path $HOME '.config\mcp\mcp.json'
   if (Test-Path -LiteralPath $sharedMcpPath) {
       throw "Refusing user-global shared MCP configuration in the isolated profile: $sharedMcpPath"
   }
   $vaultFull = [IO.Path]::GetFullPath($vault).TrimEnd('\') + '\'
   foreach ($path in @(
       (Join-Path $vault '.mcp.json'),
       (Join-Path $vault '.pi\mcp.json')
   )) {
       if (Test-Path -LiteralPath $path) {
           throw "Refusing a synced project MCP override: $path"
       }
   }
   $agentFull = [IO.Path]::GetFullPath($agentDir).TrimEnd('\') + '\'
   if ($agentFull.StartsWith($vaultFull, [StringComparison]::OrdinalIgnoreCase)) {
       throw 'The private Pi directory resolves inside the synced vault.'
   }
   $env:PI_CODING_AGENT_DIR = $agentDir
   $env:PI_CODING_AGENT_SESSION_DIR = $sessionDir
   Set-Location -LiteralPath $vault
   & pi --no-approve --no-context-files --no-builtin-tools
   exit $LASTEXITCODE
   '@
   [IO.File]::WriteAllText($launcher, $launcherText, [Text.UTF8Encoding]::new($false))
   & icacls $launcher /inheritance:r /grant:r "${identity}:F" | Out-Null
   if ($LASTEXITCODE -ne 0) { throw 'Could not restrict the launcher ACL.' }
   ```

   If the organisation blocks unsigned PowerShell scripts, have this launcher
   reviewed and signed; do not bypass execution policy. Always start this
   workspace through the launcher rather than bare `pi`:

   ```powershell
   & "$HOME\.pi\australian-legal-enterprise\Start-ObsidianPi.ps1"
   ```

   This is intentionally an MCP-only profile: `--no-approve` ignores synced
   project packages, `--no-context-files` ignores vault instructions, and
   `--no-builtin-tools` removes Pi's unsandboxed filesystem/shell tools while
   retaining extension tools. The launcher also rejects the user-global shared
   `~/.config/mcp/mcp.json`, so only the dedicated agent directory's seven-tool
   server is available. Attach a specific synthetic note with Pi's `@` file
   picker when needed; the agent cannot browse the rest of the mixed
   personal/work vault. Use a separately reviewed sandboxed profile if an agent
   ever needs broader vault access.

6. In Pi, run `/login` and select an organisation-approved model/provider if
   authentication is not already configured. Restart after the adapter's first
   installation, launch it again through the wrapper, then run:

   ```text
   /mcp reconnect australian-legal-remote
   ```

7. Run the [verification prompt](#verification-prompt). The MCP panel must show
   exactly seven Australian Legal tools. The synced validation pack is at
   `Tax\Australian Legal MCP\Validation` in the Obsidian vault. If the
   connection fails behind a corporate proxy, ask IT to allow the documented
   HTTPS hostname on port 443; do not weaken certificate validation or move the
   key into the vault.

For a macOS or Linux enterprise laptop, use the same vault/private-config
separation with the commands in [Pi](#pi), then launch `pi` after changing into
`$HOME/Desktop/Obsidian` (or the actual Obsidian vault path).

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
