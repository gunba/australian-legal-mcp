# AGENTS.md

Instructions for agents installing or operating `ato-mcp` for a user.
Read this first; use [README.md](README.md) for design detail.

## Design Philosophy

ATO-MCP should expose clean, source-grounded retrieval primitives and let agents
do the reasoning. Prefer fewer tools, fewer parameters, and less context over
feature breadth.

Good features are deterministic and derived from stable, ubiquitous source
structure. Examples: parsing an ATO document URL into its exact `doc_id`,
constructing titles from HTML headings, removing repeated history/navigation
metadata that appears across the corpus, and preserving exact chunk/document
references.

Do not add features built on hacky string substitutions, guessed citation
aliases, hand-maintained act maps, or fragile interpretations of user prose.
If logic would need ongoing maintenance against new ATO document shapes, it is
not a good runtime feature. If it relies on an ephemeral ATO structure, add an
audit/telemetry step first or leave it out.

Do not add backwards-compatibility shims for users or installs that do not
exist. Prefer one current deterministic layout, one environment variable, and
one source-derived code path. If a breaking change is needed before there is a
real installed user base, make the break cleanly and remove the old surface.

No arbitrary timers, sleeps, or polling loops as control flow. Use deterministic
completion signals or do not implement the behavior.

Do not expose date-sensitive law resolution, historical-version selection, or
similar legal interpretation helpers unless the corpus contains broad,
source-derived version/effective-date data that can support the feature safely.

## Install

End users install the Rust binary from GitHub Releases. Do not install the
Python package on user machines.

1. Download the platform asset from the latest release:
   - Linux x64: `ato-mcp-x86_64-unknown-linux-gnu.tar.gz`
   - macOS Apple Silicon: `ato-mcp-aarch64-apple-darwin.tar.gz`
   - Windows x64: `ato-mcp-x86_64-pc-windows-msvc.zip`
2. Put `ato-mcp` / `ato-mcp.exe` on `PATH`.
3. Run:

```bash
ato-mcp init
ato-mcp doctor
ato-mcp stats
```

The Rust client does not read GitHub token environment variables and does
not shell out to `gh`. If release assets are private, use an approved
mirror via `ATO_MCP_RELEASES_URL` or install from an offline bundle.

## Register With The MCP Host

Claude Code:

```bash
claude mcp add --scope user ato -- ato-mcp serve
claude mcp list
```

Claude Desktop:

```json
{
  "mcpServers": {
    "ato": { "command": "ato-mcp", "args": ["serve"] }
  }
}
```

Cursor, Continue, and other stdio MCP clients use the same command:

```text
ato-mcp serve
```

## Verify

```bash
ato-mcp stats
ato-mcp doctor
ato-mcp search "research and development tax incentive eligibility" --k 5
```

Inside the MCP host, invoke `ato.search` and confirm results include
`canonical_url` links.

## Routine Maintenance

Weekly:

```bash
ato-mcp update
ato-mcp doctor
```

Rollback:

```bash
ato-mcp doctor --rollback
```

## Search Policy

Default search is intentionally current-guidance-first:

- `Edited_private_advice` is excluded unless explicitly requested in
  `types`.
- Non-legislation documents dated before `2000-01-01` are excluded unless
  `include_old=true`.
- Legislation is exempt from the old-content rule.

## Maintainer-Only Work

Python is maintainer tooling only. Use it only on a machine that has the
source corpus, model files, and a GPU-backed ONNX Runtime setup.

Do not run `refresh-source`, `catch-up`, `build-index`, or `release` on a
user install. Those commands require the maintainer checkout and model
assets.

## Don'ts

- Do not edit files under `$XDG_DATA_HOME/ato-mcp/live/` manually.
- Do not run two `ato-mcp update` processes at the same time.
- Do not paste or print local tokens. The Rust updater does not need them.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `ato-mcp: command not found` | Put the release binary on `PATH`. |
| `init` cannot download release assets | Use a public release URL, an approved internal mirror, or an offline bundle. |
| `doctor` reports zero documents | `init` did not complete; rerun after deleting the incomplete data dir. |
| `search` returns no hits | Confirm `stats` shows `chunks > 0`; use `include_old=true` for older authorities. |
