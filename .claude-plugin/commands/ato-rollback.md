---
description: Roll back to the previous ato-mcp corpus from backups/ato.db.prev.
---

Run `ato-mcp doctor --rollback` and report the result. This restores the
previous corpus snapshot kept in `<data_dir>/backups/ato.db.prev`. The user
must restart their MCP client for the rolled-back corpus to take effect.
