# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

## Phase F -- interactive scaffolding

- **[0022](0022-prompt-ux.md)** Prompt UX wrapper (defaults + `?`-help).
- **[0023](0023-init-container.md)** `outrig init-container` with Dockerfile templates.
- **[0024](0024-init.md)** `outrig init` (chains into `init-container`).

## Phase G -- build, CI, end-to-end

- **[0025](0025-build-subcommand.md)** `outrig build` subcommand.
- **[0027](0027-e2e-acceptance.md)** End-to-end quickstart acceptance.
