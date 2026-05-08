# ATO-MCP Guidelines

The goal of this project is to maintain an on-device (with database build pulled from repo pre-baked) MCP server for searching the entire ATO database. 

The user is not expected to have a GPU and may be using a low performance enterprise laptop.

IMPORTANT: We should be looking to simplify the MCP surface and minimise extraneous context at all times.

# Design Philosophy

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

# Workflow

Use `/r`, `/j`, `/b`, `/c`, and `/rj` from this repo root.

For long-running Bash commands such as builds or test suites, launch them with background execution when the runtime supports it.
Do NOT poll background tasks. Wait for completion before acting on dependent results.

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
