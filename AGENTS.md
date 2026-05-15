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

The document surface is cleaned source HTML, not Markdown. Preserve stable
HTML structure so agents can navigate tags and attributes directly. Internal
ATO document links should become deterministic `data-doc-id` attributes rather
than retained `href` URLs, and retained images should be compact
`data-asset-ref` references resolvable through the asset tool. Do not inline
image bytes or carry decorative/history icons into context.

The semantic search/index path should use plain, source-derived text from the
cleaned HTML. Do not introduce HTML-to-Markdown conversion, Markdown escaping,
or host-rendering assumptions into stored chunks. Headings belong in metadata;
links and images should contribute only useful visible text to search.

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

End users install the Rust binary from GitHub Releases. There is no
Python package — the Rust binary is the entire product.

1. Download the platform asset from the latest release:
   - Linux x64: `ato-mcp-x86_64-unknown-linux-gnu.tar.gz`
   - macOS Apple Silicon: `ato-mcp-aarch64-apple-darwin.tar.gz`
   - Windows x64: `ato-mcp-x86_64-pc-windows-msvc.zip`
2. Put `ato-mcp` / `ato-mcp.exe` on `PATH`.
3. Register `ato-mcp serve` with the MCP host (next section). On first use
   the MCP server tells the agent that the corpus is not yet installed and
   asks the user to run `ato-mcp update`. After download completes the user
   restarts the MCP client.

Manual one-shot install (terminal, no MCP client):

```bash
ato-mcp update
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

Maintainer corpus builds happen via `cargo build --release --features cuda && scripts/maintainer-sync.sh`
on a machine with the source corpus, Granite embedding model files, and a
GPU-capable ONNX runtime. The Rust binary's `--gpu` flag should be set
and the build should fail fast if CUDA is not available. The Rust
end-user runtime must remain CPU-safe; do not make ordinary install,
update, search, or serve require a GPU.
Maintainer corpus rebuilds should run with sleep prevention active. `build`
and `scripts/maintainer-sync.sh` do this automatically through `systemd-inhibit`
or `caffeinate` when available.

`build` consumes local embedding model files and writes corpus artifacts.
Do not thread hosted model URLs or other distribution metadata through corpus
building. The `release` step owns model distribution metadata and final
manifest publication.

Do not run `tree-crawl`, `link-download`, `scrape-diff`, `build`, or
`publish-release` on a
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
| `update` cannot download release assets | Use a public release URL, an approved internal mirror, or an offline bundle. |
| `doctor` reports zero documents | `update` did not complete; rerun after deleting the incomplete data dir. |
| `search` returns no hits | Confirm `stats` shows `chunks > 0`; use `include_old=true` for older authorities. |
