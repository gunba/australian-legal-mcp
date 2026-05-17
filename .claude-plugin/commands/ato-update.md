---
description: Manually trigger an ato-mcp corpus update (otherwise auto-runs weekly).
---

Run `ato-mcp update` in a shell and report the result to the user. The corpus
download is ~4 GB and takes 5-10 min on a typical connection. After completion
the user must restart their MCP client for the new corpus to take effect.

Auto-update normally handles this once per week on `ato-mcp serve` startup;
use this command when the user wants the latest corpus immediately.
