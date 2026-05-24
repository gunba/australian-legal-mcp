---
paths:
  - "src/config.rs"
---

# src/config.rs

Tag line: `L<n>`; code usually starts at `L<n+1>`.

## Rust Update Mechanism
End-user update flow: update.json fast-path when local DB/model match, otherwise staged model/corpus rebuild and guarded promotion, with single-writer LOCK and doctor rollback backup.

- [UM-02 L85] The writer lock is implemented with fs2::FileExt::lock_exclusive on the app LOCK file, giving a cross-platform advisory lock around update/install mutation.
