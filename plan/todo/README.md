# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

| Task | Title                                      | Dependencies |
|------|--------------------------------------------|--------------|
| 0080 | Sidecar entrypoint-stdio transport         | 0079         |
| 0081 | Dynamic sidecar addition                   | 0079         |

Tasks 0078-0081 share a design reference: [`mcp-sidecars-spec.md`](mcp-sidecars-spec.md).
The spec is deleted when the last of the four lands.
