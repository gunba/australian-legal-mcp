# Microsoft Copilot and Entra integration

The recommended first enterprise test is **Copilot Studio using a Streamable
HTTP custom connector with Microsoft Entra on-behalf-of (OBO) authentication**.
The agent can then be published to Microsoft 365 Copilot. A direct Microsoft 365
declarative-agent MCP plugin is also supported, but it has a different Microsoft
caller identity and Teams Developer Portal registration.

Do not combine these identities casually. The server has exact allowlists for:

- tenant ID;
- token audience;
- delegated `scp` value;
- caller application (`azp`).

The server accepts only v2 Entra access tokens signed with current Entra RS256
keys. It validates signature, issuer, audience, expiry/not-before, tenant,
delegated scope, token version, user object ID, and caller application on every
MCP request. It never logs or forwards bearer tokens.

Official Microsoft guidance:

- [Connect an MCP server to Copilot Studio](https://learn.microsoft.com/microsoft-copilot-studio/mcp-add-existing-server-to-agent)
- [Configure OBO for custom connectors](https://learn.microsoft.com/microsoft-copilot-studio/advanced-custom-connector-on-behalf-of)
- [Build Microsoft 365 MCP plugins](https://learn.microsoft.com/microsoft-365/copilot/extensibility/build-mcp-plugins)
- [Microsoft 365 plugin authentication](https://learn.microsoft.com/microsoft-365/copilot/extensibility/plugin-authentication)
- [Plugin manifest v2.4](https://learn.microsoft.com/microsoft-365/copilot/extensibility/plugin-manifest-2.4)
- [MCP 2025-06-18 authorization](https://modelcontextprotocol.io/specification/2025-06-18/basic/authorization)

## Security boundary implemented here

With `LEGAL_MCP_HTTP_AUTH=entra`, the loopback service publishes:

```text
GET /.well-known/oauth-protected-resource/mcp
POST /mcp
```

An unauthenticated request receives:

```http
HTTP/1.1 401 Unauthorized
WWW-Authenticate: Bearer resource_metadata="https://HOST/.well-known/oauth-protected-resource/mcp", scope="api://SERVER_APP_ID/legal.read"
```

A valid token without the required scope/client receives 403. `/livez` and
`/readyz` remain loopback-only because Caddy routes only MCP and protected
resource metadata.

The Rust service never binds publicly. Caddy terminates TLS and forwards to
`127.0.0.1:51235`. Caddy is disabled until the configuration script has proved
that missing bearer tokens receive 401 locally.

## A. Copilot Studio OBO path (recommended)

### 1. Create the resource/server app registration

In Entra admin center -> **App registrations** -> **New registration**:

1. Name: `Australian Legal MCP API`.
2. Supported account type: **Accounts in this organizational directory only**.
3. Under **Expose an API**, accept/set:
   `api://<server-application-client-id>`.
4. Add delegated scope:
   - scope name: `legal.read`;
   - enabled;
   - least-privilege read description;
   - choose tenant-appropriate consent policy.
5. In the app manifest, ensure the API requests v2 access tokens
   (`api.requestedAccessTokenVersion` is `2`).
6. Record the tenant ID, application/client ID, Application ID URI, and full
   scope URI.

The server app needs no client secret because `legal-mcp` is a resource server,
not an OAuth confidential client.

### 2. Create the Power Platform connector app registration

Create another single-tenant app: `Australian Legal MCP Connector`.

1. Under **API permissions**, add the server app's delegated `legal.read`
   permission.
2. Grant tenant admin consent.
3. Create a short-lived client secret. Copy it once; it is entered into Power
   Apps and is **never** copied to the MCP VM or repository.
4. Under **Expose an API**, add `access_as_user`.
5. Under **Authorized client applications**, preauthorize **Microsoft Azure API
   Connections** with client ID:

   ```text
   fe053c5f-3692-4f14-aef2-ee34fc081cae
   ```

6. Select the connector app's `access_as_user` scope for that preauthorization.
7. Record the connector application/client ID.

Assign a named owner and expiry alert to the connector secret. Rotate with an
overlap: create the replacement, update and test the connector, then revoke the
old secret. Never let the only credential expire during a pilot. If the tenant
supports a secretless connector identity in this exact OBO flow, validate it in
the test tenant before replacing the documented static registration.

### 3. Render the connector assets locally

Build/export the exact seven tool schemas and render the tenant-specific
Swagger file:

```bash
cargo build --release --locked
target/release/legal-mcp export-mcp-tools \
  --output data/cache/microsoft-integration/tools.json
rm -rf data/cache/microsoft-integration/rendered-copilot

python3 scripts/render-microsoft-integrations.py \
  --tools data/cache/microsoft-integration/tools.json \
  --public-host 'legal.example.com' \
  --tenant-id '<tenant-id>' \
  --server-app-id '<server-app-id>' \
  --scope legal.read \
  --output-dir data/cache/microsoft-integration/rendered-copilot
```

The generated Copilot Studio Swagger uses:

```yaml
x-ms-agentic-protocol: mcp-streamable-1.0
```

Copilot Studio supports Streamable transport; its old separate SSE transport is
no longer supported. This server returns synchronous JSON for POST and 405 for
GET `/mcp`, which is a conforming Streamable HTTP subset.

### 4. Enable public hosted ingress only after auth values exist

Set Linode OpenTofu `public_mcp_enabled=true`, review the plan, and apply it
immediately before cutover. The Akamai Cloud Firewall then admits TCP 80/443,
but host UFW and Caddy remain closed.

Configure Entra and enable Caddy atomically on the host:

```bash
ssh -T legal-mcp-admin@legal.example.com \
  'sudo /usr/local/sbin/legal-mcp-configure-auth \
    --mode entra \
    --public-host legal.example.com \
    --tenant-id <tenant-id> \
    --server-app-id <server-app-id> \
    --audiences <server-app-id>,api://<server-app-id> \
    --scope legal.read \
    --scope-uri api://<server-app-id>/legal.read \
    --allowed-client-ids <connector-app-id>'
```

The script:

1. journals the prior root-owned runtime/verifier configuration;
2. restarts the private digest-pinned container and prewarms Entra signing keys;
3. proves exact readiness, metadata, and unauthenticated 401 challenges;
4. opens host UFW 80/443, enables Caddy, and checks public TLS without redirects;
5. commits only after public probes pass, otherwise restoring the prior service,
   auth files, closed host firewall, and safe Caddy state.

### 5. Create the Power Apps custom connector

In Power Apps -> **Custom connectors**:

1. Import
   `data/cache/microsoft-integration/rendered-copilot/copilot-studio-connector.swagger.yaml`.
2. On **Security** select OAuth 2.0 / Microsoft Entra ID.
3. Enter:

   | Field | Value |
   |---|---|
   | Client ID | connector app client ID |
   | Client secret | connector app secret |
   | Authorization URL | `https://login.microsoftonline.com` |
   | Tenant ID | exact tenant ID, not `common` |
   | Resource URL | `api://<server-app-id>` |
   | Enable on-behalf-of login | `true` |
   | Scope | `api://<server-app-id>/legal.read` |

4. Save/update the connector.
5. Copy the generated redirect URL.
6. In the connector app registration -> **Authentication**, add that URL as a
   **Web** redirect URI.
7. Share the connector with makers/users using least privilege.
8. Test the connector in Power Apps.

### 6. Add it to Copilot Studio

1. Enable generative orchestration for the agent.
2. **Tools** -> **Add a tool** -> the custom connector/MCP server.
3. Create the connection and consent as the signed-in user.
4. Confirm all seven tools appear and are marked read-only.
5. Test explicit-source search, chunks, definitions, anchors, asset retrieval,
   stats, and canonical fetch.
6. Publish to Teams/Microsoft 365 Copilot only after the test environment's DLP
   policy allows the connector and administrators permit publishing.

Power Platform DLP applies to MCP connectors in real time. Put interacting
connectors in compatible Business/Non-business groups. Trials can be useful for
authoring/testing but current licensing may restrict publication; verify the
current [Copilot Studio licensing guide](https://learn.microsoft.com/microsoft-copilot-studio/billing-licensing).

## B. Direct Microsoft 365 declarative-agent MCP plugin

This path uses Microsoft 365 Agents Toolkit 6.3.x or newer and plugin manifest
v2.4 `RemoteMCPServer`. Tenant-built agents should package a static snapshot of
`tools/list`; the renderer produces both inline definitions and
`mcp-tools.json`.

### 1. Register the Microsoft 365 SSO client

In Teams Developer Portal -> **Tools** -> **Microsoft Entra SSO client ID
registration**:

1. Base URL: exact `https://HOST/mcp`.
2. Restrict organization to **My organization only** for testing.
3. Restrict app once the final Teams app ID exists.
4. Client ID: the same server/API app registration.
5. Save and record the generated SSO registration ID and Application ID URI.

Update the server app registration:

- add the generated URI to `identifierUris` without removing
  `api://<server-app-id>`;
- add Web redirect URI:
  `https://teams.microsoft.com/api/platform/v1.0/oAuthConsentRedirect`;
- preauthorize Microsoft's enterprise token-store client:
  `ab3be6b7-f5df-413d-ac2d-abf1e3fd9c0b`;
- select the delegated API scope.

Rerun host auth configuration with both exact audiences and known callers:

```bash
ssh -T legal-mcp-admin@legal.example.com \
  'sudo /usr/local/sbin/legal-mcp-configure-auth \
    --mode entra \
    --public-host legal.example.com \
    --tenant-id <tenant-id> \
    --server-app-id <server-app-id> \
    --audiences <server-app-id>,<teams-generated-application-id-uri> \
    --scope legal.read \
    --scope-uri api://<server-app-id>/legal.read \
    --allowed-client-ids <connector-app-id>,ab3be6b7-f5df-413d-ac2d-abf1e3fd9c0b'
```

### 2. Render the v2.4 plugin

```bash
rm -rf data/cache/microsoft-integration/rendered-m365
python3 scripts/render-microsoft-integrations.py \
  --tools data/cache/microsoft-integration/tools.json \
  --public-host 'legal.example.com' \
  --tenant-id '<tenant-id>' \
  --server-app-id '<server-app-id>' \
  --scope legal.read \
  --sso-registration-id '<teams-sso-registration-id>' \
  --output-dir data/cache/microsoft-integration/rendered-m365
```

Use Agents Toolkit to create **Declarative Agent -> Add an Action -> Start with
an MCP Server**. Replace its generated plugin manifest/tool snapshot with the
rendered `microsoft-365-ai-plugin.json` and `mcp-tools.json`, then provision and
sideload. Confirm **Custom App Upload Enabled** and **Copilot Access Enabled**.

The checked-in plugin template classifies the seven tools as non-consequential
operations retrieving public legal-source data. Validate the rendered manifest
against Microsoft's live v2.4 schema and Agents Toolkit before every package;
then review the classification with enterprise compliance before publishing.

## Verification

### Local gates (no cloud or Microsoft licence required)

```bash
cargo test --locked http_auth
cargo test --locked --test http_smoke \
  entra_mode_publishes_resource_metadata_and_challenges_every_mcp_request
python3 -m unittest \
  tests/test_manage_api_keys.py \
  tests/test_remote_mcp.py \
  tests/test_render_microsoft_integrations.py
```

These prove strict Entra configuration, signed-token audience/scope/client
checks, OAuth resource metadata/challenges, exact seven-tool annotations, and
content-addressed generation transport.

### Public boundary without a token

```bash
python3 scripts/test-remote-mcp.py 'https://HOST/mcp'
```

This proves TLS, metadata binding, and the 401 challenge without exposing a
credential.

### Full token probe

Obtain a real delegated access token through the configured connector/test
client, place it only in the process environment, and run:

```bash
export LEGAL_MCP_TEST_ACCESS_TOKEN='<short-lived-token>'
python3 scripts/test-remote-mcp.py --require-token \
  --tools data/cache/microsoft-integration/rendered-m365/mcp-tools.json \
  'https://HOST/mcp'
unset LEGAL_MCP_TEST_ACCESS_TOKEN
```

The script never prints the token or follows redirects. It verifies authenticated
initialize and the complete seven-tool descriptor snapshot, not just names.
Then test consent, expiry, revoked consent, wrong tenant,
wrong audience, missing scope, unapproved client, Conditional Access, disabled
users, and Entra signing-key rotation before any wider pilot.

## Not implemented by design

- no anonymous or shared/unidentified API-key mode; API keys are not Copilot
  user identity;
- no multi-tenant issuer acceptance;
- no application-permission/daemon tokens (delegated users only);
- no token passthrough to ATO or any other source;
- no public `/readyz` or `/livez` route;
- no dynamic client registration (Entra does not provide it for this path);
- no automatic production publishing or tenant-wide admin approval.
