# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

## Phase F-pre -- foundation gaps (run first)

- **[0029](0029-mistralrs-model-vs-provider.md)** Reshape mistralrs schema:
  weight-source fields move off the provider, onto the model. Pulled in from
  `plan/next/` -- a 0005 oversight that surfaced during 0024.
- **[0030](0030-fuzzy-prompt-impl.md)** Rich-TUI `PromptSource` impl
  (FuzzySelect for picking models / agents / containers from N candidates).

## Phase F -- interactive scaffolding

- **[0031](0031-container-add.md)** `outrig container add` with Dockerfile templates.
- **[0033](0033-init.md)** `outrig init` orchestrator (idempotent; loops `container add`).

## Phase G -- build, CI, end-to-end

- **[0032](0032-build-subcommand.md)** `outrig build` subcommand.
- **[0034](0034-e2e-acceptance.md)** End-to-end quickstart acceptance.
