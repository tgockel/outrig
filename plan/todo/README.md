# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

| Task | Title                                              | Dependencies |
|------|----------------------------------------------------|--------------|
| 0082 | Fix pre-existing doc-style audit violations        | --           |
| 0083 | Fix stale `malformed_mcp_label_is_hard_error` test | --           |
| 0084 | REPL slash-command dispatcher                      | --           |
| 0085 | from_image_config sidecar translation              | --           |
| 0086 | Sidecar startup and clean-sweep performance        | --           |
