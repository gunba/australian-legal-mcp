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

The plugin's `.mcp.json` ships with `http://127.0.0.1:0/mcp` as a sentinel.
On first start, `ato-mcp serve` picks a free port, binds it, and rewrites
the URL in the plugin's installed `.mcp.json` to match. The user exits and
resumes the Claude Code session so the new URL takes effect.

```bash
ato-mcp serve              # picks a free port on first run; reuses it after
ato-mcp serve --port 51235 # explicit override
```

The plugin includes a skill (`skills/ato-mcp-server/SKILL.md`) that the
agent loads on ATO-related queries. If the `ato` tools aren't in the agent's
tool list, the skill instructs the agent to ask the user for permission to
start the server via a background `ato-mcp serve` invocation and to exit +
resume the session.

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

The `austlii:` URI scheme for `fetch` reaches `classic.austlii.edu.au` for
known case and legislation paths, for example
`austlii:au/cases/cth/HCA/1992/23`.

Live AustLII search is currently unavailable. AustLII's published SINO CGI
endpoint (`/cgi-bin/sinosrch.cgi`) now reports that the resource is no longer
available, so this is not a cookie-configuration problem. Do not ask users to
open a SINO browser URL, paste a Cookie header, or run browser cookie-store
extraction. `search_austlii` and `ato-mcp austlii setup` fail fast with the
same diagnostic. `ato-mcp austlii clear` deletes any legacy session file.

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
| `ato-mcp serve: bind ... already in use` | Stop whatever holds the port, or run `ato-mcp serve --port <other>`; the new URL is written back into `.mcp.json` and the user exits + resumes the session. |
| `stats` reports zero documents | `update` didn't complete; rerun after deleting the incomplete `live/` dir. |
| `search` returns no hits | Confirm `stats` shows `chunks > 0`; use `include_old=true` for older authorities. |
| `austlii search` is unavailable | AustLII's published SINO CGI endpoint is no longer available. Use `fetch` only when the exact `austlii:<path>` is already known. |
