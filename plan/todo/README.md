# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

## Phase H -- outrig mcp subcommand

- **[0035](0035-outrig-mcp-session-setup.md)** Refactor: extract `SessionSetup` from
  `cli/run.rs`.
- **[0036](0036-outrig-mcp-agent-name-option.md)** Refactor: `Session::agent_name` ->
  `Option<String>`.
- **[0037](0037-outrig-mcp-tool-name-extract.md)** Refactor: factor `tool_name::sanitize`
  out of `rig_tool`.
- **[0038](0038-outrig-mcp-rmcp-features.md)** Add rmcp `server` + `transport-io`
  features.
- **[0039](0039-outrig-mcp-proxy-server.md)** Land `ProxyServer`.
- **[0040](0040-outrig-mcp-wire-subcommand.md)** Wire `outrig mcp` subcommand.
- **[0041](0041-outrig-mcp-docs.md)** Docs for `outrig mcp`.

## Phase I -- post-acceptance v0 polish

- **[0042](0042-library-surface.md)** Curated `outrig::*` library API.
- **[0043](0043-verbose-flag.md)** Wire `--verbose` global flag.
- **[0044](0044-dedup-init-tracing.md)** Dedup `init_tracing` into `tests/common/mod.rs`.
- **[0045](0045-bound-stderr-capture.md)** Bound stderr capture in `run_capture`.
- **[0046](0046-pick-a-license.md)** Pick a license + ship `LICENSE`.
