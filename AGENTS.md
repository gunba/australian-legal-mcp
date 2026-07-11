# AGENTS.md

Instructions for agents installing or operating `ato-mcp` for a user.
Read this first; use [README.md](README.md) for design detail.

## Design Philosophy

ATO-MCP exposes clean, source-grounded retrieval primitives and lets agents
do the reasoning. Prefer fewer tools, fewer parameters, and less context over
feature breadth.

Good features are deterministic and derived from stable, ubiquitous source
structure. Examples: parsing an ATO document URL into its exact `doc_id`,
constructing titles from HTML headings, removing repeated history/navigation
metadata that appears across the corpus, and preserving exact chunk/document
references.

The document surface is cleaned source HTML. Preserve stable HTML structure
so agents can navigate tags and attributes directly. Internal ATO document
links become deterministic `data-doc-id` attributes rather than retained
`href` URLs; retained images are compact `data-asset-ref` references
resolvable through the asset tool. Image bytes and decorative/history icons
stay out of context.

The semantic search/index path uses plain, source-derived text from the
cleaned HTML. Headings live in metadata; links and images contribute only
useful visible text to search.

Avoid features built on string substitutions, guessed citation aliases,
hand-maintained act maps, or fragile interpretations of user prose. If
logic would need ongoing maintenance against new ATO document shapes, it
belongs in source acquisition, not in runtime retrieval.

Each surface has one path. If a breaking change is needed before there is a
real installed user base, make the break cleanly and remove the old surface.

## Plugin install

```bash
git clone https://github.com/gunba/ato-mcp.git
claude plugin install ./ato-mcp
```

For Pi, install the MCP adapter and then install this checkout as a Pi package:

```bash
pi install npm:pi-mcp-adapter
pi install ./ato-mcp
```

The Pi package manifest in `package.json` exposes the two ATO skills. The MCP
server remains a standard MCP config entry, so Pi needs `pi-mcp-adapter`.
Project-local Pi sessions can read this repository's `.mcp.json`; for
user-global access from any project, add the same `mcpServers.ato` entry to
`~/.config/mcp/mcp.json` or `~/.pi/agent/mcp.json`.

The plugin's `.mcp.json` registers `mcpServers.ato` as the stdio command
`ato-mcp mcp`. That command starts or reuses one local loopback HTTP backend
and proxies MCP messages to it. This avoids first-run generated-port reloads
while keeping the SQLite corpus and semantic model in one backend process per
user data dir.

```bash
ato-mcp mcp               # MCP host entry point
ato-mcp serve             # advanced/manual HTTP backend
ato-mcp serve --port 51235
```

The plugin includes two skills:

- `skills/ato-mcp-server/SKILL.md` is intentionally small and is loaded for
  ordinary ATO/tax research. It tells the agent to use the ATO tools and gives
  the minimal recovery path when the server is down.
- `skills/setup-ato-mcp/SKILL.md` is the larger install/repair guide. Load it
  only for first-run setup, MCP startup repair, missing corpus, corpus
  updates, or repeated startup failures.

Installer agents should not ask the user to choose ports or edit config. The
MCP host starts `ato-mcp mcp`; if no backend is running, that command starts
one and records the endpoint in `<data_dir>/http.json`.

The binary install location is independent from the corpus data directory.
Installer agents must choose one corpus data directory and use it consistently
for `ato-mcp update`, `ato-mcp mcp`, the backend server, `stats`, and
verification searches.
Default mode leaves `ATO_MCP_DATA_DIR` unset and uses the default user data dir
(`%APPDATA%\ato-mcp` on Windows, `~/.local/share/ato-mcp` on Linux, and
`~/Library/Application Support/ato-mcp` on macOS). Portable/co-located mode
sets `ATO_MCP_DATA_DIR` to a stable directory next to the binary for every
future `ato-mcp` command and backend start. Do not install the corpus under a
temporary extraction directory.

On the first MCP `initialize`, the server tells the agent whether the corpus
is installed. If not, the agent explains the large download and runs
`ato-mcp update` with the user's approval.

## Verify the install

```bash
ato-mcp stats              # JSON: documents, chunks, embeddings, search policy
ato-mcp search "research and development tax incentive eligibility" --k 5
```

Inside the MCP host, invoke `search` and confirm results include
`canonical_url` links.

## Updates

```bash
ato-mcp update
```

Full corpus replacement uses paginated release discovery to find the newest
non-prerelease `manifest.json`, streams and verifies `ato.db.zst`, `ato.ann`,
and model artifacts, then assembles an immutable generation. Atomic replacement
of `active-generation` activates the complete set; restart the MCP client and
local backend to use it.

When a newer corpus is published, the server's `initialize` instructions
include the available index version. The agent surfaces the suggestion and
runs `ato-mcp update` after the user agrees.

## Search policy

Defaults are tuned for current-guidance-first retrieval:

- Edited private advice (`EV`) is excluded unless `types` includes it.
- Non-legislation documents dated before `2000-01-01` are excluded unless
  `include_old=true`.
- Legislation is exempt from the old-content rule.
- `current_only=true` (default) filters withdrawn and superseded rulings.

## Maintainer-only

Corpus builds happen via `cargo build --release --features cuda && scripts/maintainer-sync.sh`
on a machine with the source corpus, Granite embedding model files, and a
GPU-capable ONNX runtime. The `cuda` Cargo feature both enables the build's
CUDA execution provider and the runtime's GPU path; there is no separate
`--gpu` flag.

The end-user runtime stays CPU-safe — install, update, search, and serve
never require a GPU.

`build`, `tree-crawl`, `link-download`, `scrape-diff`, `package-corpus`, and
`publish-release` are maintainer commands and require the maintainer
checkout plus model assets. Don't run them on a user install.

## Don'ts

- Do not edit files under `<data_dir>/live/` manually.
- Do not run two `ato-mcp update` processes at the same time.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `ato-mcp: command not found` | Put the release binary on `PATH`. |
| MCP startup reports a stdio command failure | Confirm the MCP entry runs `ato-mcp mcp` and that the release binary is on `PATH` or configured by absolute path. |
| `ato-mcp serve: bind ... already in use` | Stop whatever holds the port, or run `ato-mcp serve --port <other>` for manual HTTP testing. |
| `stats` reports zero documents | `update` didn't complete; rerun after deleting the incomplete `live/` dir. |
| `search` returns no hits | Confirm `stats` shows `chunks > 0`; use `include_old=true` for older authorities. |
