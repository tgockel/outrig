# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

| Task | Title | Dependencies |
|---|---|---|
| 0071 | `outrig image build` for standalone image projects | 0069, 0070 |
| 0072 | `outrig image inspect` and standalone design prompts | 0069, 0071 |
