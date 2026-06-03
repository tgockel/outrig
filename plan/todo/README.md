# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

| Task | Title | Dependencies |
|---|---|---|
| 0072 | OCI labels for standalone image config (stamp + read) | 0069, 0071 |
| 0073 | Remove the baked `/etc/outrig/image.toml` | 0072 |
| 0074 | `outrig image inspect <ref>` (local, read-only) | 0072 |
| 0075 | `outrig design prompt --standalone` | 0073 |
