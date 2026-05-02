# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

## Phase B -- subprocess + container

- **[0008](0008-container-lifecycle.md)** Container start/stop with reliable cleanup.
- **[0009](0009-runtime-user-bootstrap.md)** Runtime UID/GID bootstrap inside the container.

## Phase C -- MCP + LLM

- **[0010](0010-mcp-client.md)** rmcp client over `podman exec` stdio.
- **[0011](0011-rig-tool-adapter.md)** MCP -> Rig dynamic-tool adapter.
- **[0012](0012-llm-resolver.md)** Resolve agent -> model -> provider, build Rig client.

## Phase D -- REPL + agent loop

- **[0013](0013-repl-skeleton.md)** stdin/stdout REPL with slash commands.
- **[0014](0014-agent-loop.md)** `outrig run` -- the headline agent loop.

## Phase E -- sessions

- **[0015](0015-session-store.md)** SessionStore (auto + explicit `--session-dir`).
- **[0016](0016-session-cli.md)** `outrig ls` / `outrig logs` / `outrig discard`.

## Phase F -- interactive scaffolding

- **[0017](0017-prompt-ux.md)** Prompt UX wrapper (defaults + `?`-help).
- **[0018](0018-init-container.md)** `outrig init-container` with Dockerfile templates.
- **[0019](0019-init.md)** `outrig init` (chains into `init-container`).

## Phase G -- build, CI, end-to-end

- **[0020](0020-build-subcommand.md)** `outrig build` subcommand.
- **[0021](0021-ci.md)** GitHub Actions CI (cargo + mdbook).
- **[0022](0022-e2e-acceptance.md)** End-to-end quickstart acceptance.
