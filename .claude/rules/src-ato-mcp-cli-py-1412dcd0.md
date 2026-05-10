---
paths:
  - "src/ato_mcp/cli.py"
---

# src/ato_mcp/cli.py

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## CLI Commands
Typer command surface, end-user vs maintainer split, defaults and global excludes.

- [CC-01 L22] Two-tier command surface: end-user commands (serve, init, update, doctor, stats) ship in the wheel; maintainer commands (refresh-source, build-index, release) require a repo checkout. The split keeps end-user installs minimal.
- [CC-06 L23] typer.Typer is configured with no_args_is_help=True and add_completion=False — the agent surface is intentionally small, no shell-completion magic, no implicit subcommands.
- [CC-05 L51] refresh-source defaults to --output-dir ./ato_pages; build-index requires --pages-dir pointing at a populated ato_pages/. The split keeps the scrape and index stages independently re-runnable — same pages dir can feed multiple builds.
