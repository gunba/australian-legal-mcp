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

The plugin's `.mcp.json` connects to `http://127.0.0.1:${env:ATO_MCP_PORT}/mcp`.
Run the one-shot setup once so the env var resolves:

```bash
ato-mcp install
```

`install` picks a free port, persists it, and prints the
`export ATO_MCP_PORT=<port>` line for the user to add to their shell rc.
After that, the user starts the HTTP server from a terminal:

```bash
ato-mcp serve              # binds the persisted port
ato-mcp serve --port 51235 # explicit override
```

The plugin includes a skill (`skills/ato-mcp-server/SKILL.md`) that the agent
loads on ATO-related queries. If the server isn't reachable, the skill
instructs the agent to ask the user for permission to start it via a
background `ato-mcp serve` invocation.

On the first MCP `initialize`, the server tells the agent whether the corpus
is installed. If not, the agent surfaces "run `ato-mcp update`" to the user;
the agent can invoke that via Bash with the user's approval.

## Verify the install

```bash
ato-mcp stats              # JSON: documents, chunks, embeddings, austlii session
ato-mcp search "research and development tax incentive eligibility" --k 5
```

Inside the MCP host, invoke `search` and confirm results include
`canonical_url` links.

## AustLII access

The `austlii:` URI scheme for `fetch` and the `search_austlii` MCP tool reach
`*.austlii.edu.au` through Cloudflare's bot management. Document fetches
against `classic.austlii.edu.au` work with a browser-grade User-Agent alone;
SINO search needs a `cf_clearance` cookie tied to a real browser session
that's cleared the JS challenge.

The user runs `ato-mcp austlii setup` once to grant consent, open AustLII
in their default browser, and acquire the cookie. The cookie and the
browser's User-Agent string are persisted to
`<data_dir>/austlii_session.json` and reused on subsequent MCP calls.
`stats` reports the cached browser, cookie age, and `cf_clearance` presence;
`ato-mcp austlii clear` deletes the session file.

Override the detected browser with `ATO_MCP_BROWSER=chrome|edge|firefox`
when the registry / xdg-mime lookup returns the wrong default. Safari
isn't supported by `rookie`; macOS Safari users either override to
Chrome/Firefox or paste the cookie manually with
`ato-mcp austlii setup --cookie '<value>'`.

`ato-mcp austlii search "<query>"` runs the CLI variant of the MCP
`search_austlii` tool.

## Updates

```bash
ato-mcp update
```

Full corpus replacement: fetch the published `manifest.json`, download the
new `ato.db.zst`, verify sha256, atomic-rename into `live/`. Restart the MCP
client (or the `ato-mcp serve` process) for the new corpus to take effect.

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
| `ato-mcp serve: bind ... already in use` | Re-run `ato-mcp install --port <other>` and update `ATO_MCP_PORT` in the shell rc to match. |
| `stats` reports zero documents | `update` didn't complete; rerun after deleting the incomplete `live/` dir. |
| `search` returns no hits | Confirm `stats` shows `chunks > 0`; use `include_old=true` for older authorities. |
| `austlii search` returns 403 | The cf_clearance cookie expired. Run `ato-mcp austlii setup` again. |
