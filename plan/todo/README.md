# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

## Phase F -- interactive scaffolding

- **[0023](0023-container-add.md)** `outrig container add` with Dockerfile templates.
- **[0024](0024-config-init.md)** `outrig config init` (global config; new `config` group).
- **[0026](0026-init.md)** `outrig init` orchestrator (idempotent; loops `container add`).

## Phase G -- build, CI, end-to-end

- **[0025](0025-build-subcommand.md)** `outrig build` subcommand.
- **[0027](0027-e2e-acceptance.md)** End-to-end quickstart acceptance.
