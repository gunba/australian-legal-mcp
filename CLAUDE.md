# ATO-MCP Guidelines

The goal of this project is to maintain an on-device (with database build pulled from repo pre-baked) MCP server for searching the entire ATO database. 

IMPORTANT: The user is not expected to have a GPU and may be using a low performance enterprise laptop.

IMPORTANT: We should be looking to simplify the MCP surface and minimise extraneous context at all times.

IMPORTANT: Tool responses must contain only information a tax-professional agent would act on. Do NOT serialise internal debug metadata to the agent — no ranking scores, no model identifiers, no candidate counts, no reranker_used flags, no chunk ordinals, no echo of the query, no excluded-types policy, no `returned_chars`, no `distinct_docs`. If a field doesn't help the agent navigate or cite documents, it doesn't belong on the wire. Empty-value Option fields must use `skip_serializing_if = "Option::is_none"` rather than render as JSON null. Every byte the agent reads competes with the source text for their attention budget.

# Design Philosophy

- ATO-MCP should expose clean, source-grounded retrieval primitives and let agents do the reasoning. Prefer fewer tools, fewer parameters, and less context over feature breadth.

- Good features are deterministic and derived from stable, ubiquitous source structure. Examples: parsing an ATO document URL into its exact `doc_id`, constructing titles from HTML headings, removing repeated history/navigation metadata that appears across the corpus, and preserving exact chunk/document references.

The document surface is cleaned source HTML, not Markdown. Preserve stable HTML structure so agents can navigate tags and attributes directly. 
Internal ATO document links should become deterministic `data-doc-id` attributes rather than retained `href` URLs, and retained images should be compact `data-asset-ref` references resolvable through the asset tool. Do not inline image bytes or carry decorative/history icons into context.

- The semantic search/index path should use plain, source-derived text from the cleaned HTML. Do not introduce HTML-to-Markdown conversion, 
- Markdown escaping, or host-rendering assumptions into stored chunks. Headings belong in metadata; links and images should contribute only useful visible text to search.

- Do not add features built on hacky string substitutions, guessed citation aliases, hand-maintained act maps, or fragile interpretations of user prose.
- If logic would need ongoing maintenance against new ATO document shapes, it is not a good runtime feature. If it relies on an ephemeral ATO structure, add an audit/telemetry step first or leave it out.

- Do not add backwards-compatibility shims for users or installs that do not exist. Prefer one current deterministic layout, one environment variable, and one source-derived code path. If a breaking change is needed before there is a real installed user base, make the break cleanly and remove the old surface.

- No arbitrary timers, sleeps, or polling loops as control flow. Use deterministic completion signals or do not implement the behavior.

- Do not expose date-sensitive law resolution, historical-version selection, or similar legal interpretation helpers unless the corpus contains broad, source-derived version/effective-date data that can support the feature safely.

# Workflow

Use `/r`, `/j`, `/b`, `/c`, and `/rj` from this repo root.

For long-running Bash commands such as builds or test suites, launch them with background execution when the runtime supports it.
Do NOT poll background tasks. Wait for completion before acting on dependent results.

`build-index` consumes local embedding model files and writes corpus artifacts.
Do not thread hosted model URLs, reranker URLs, or other distribution metadata
through corpus building. The `release` step owns model/reranker distribution
metadata and final manifest publication.
Use `pip install -e '.[dev,gpu]'` for release builds; install the `cpu` and
`gpu` extras separately because their ONNX Runtime wheels conflict.
Maintainer corpus builds should pass `--gpu` and fail fast if CUDA is not
available. The Rust end-user runtime must remain CPU-safe; do not make ordinary
install, update, search, or serve require a GPU.
Maintainer corpus rebuilds should run with sleep prevention active. `build-index`
and `scripts/maintainer-sync.sh` do this automatically through `systemd-inhibit`
or `caffeinate` when available.

# Documentation

All tagged documentation is managed by `proofd`. Canonical rule data lives outside the repo in the proofd knowledge base. `proofd sync` generates Claude Markdown snapshots under `.claude/rules/`.
Codex does not have Claude-style path-scoped rule auto-load, so repo bootstrap configures Codex hooks that inject proofd guidance on session start and targeted proofd context on relevant prompts.

Do not hand-edit `.claude/rules/*.md`. They are refreshed by `proofd sync`, typically during janitor, build, release, or finalization work. Use `"$HOME/.claude/agent-proofs/bin/proofd.py"` subcommands to create rules, add entries, split rules, record verifications, and regenerate the rule output.
Generated rule markdown is file-scoped and intentionally omits stored file lists. If you need source-reference files for a tag, use `"$HOME/.claude/agent-proofs/bin/proofd.py" entry-files --tag <TAG>`.

Tags are embedded in source code as language-appropriate comments containing `[TAG]` near the implementation site. Tags must be allocated by `proofd`; agents must not invent tag IDs themselves.

Useful commands:
- `"$HOME/.claude/agent-proofs/bin/proofd.py" sync`
- `"$HOME/.claude/agent-proofs/bin/proofd.py" lint`
- `"$HOME/.claude/agent-proofs/bin/proofd.py" entry-files --tag <TAG>`
- `"$HOME/.claude/agent-proofs/bin/proofd.py" select-matching <paths...>`
- `"$HOME/.claude/agent-proofs/bin/proofd.py" context <paths...>`
