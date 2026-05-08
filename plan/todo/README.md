# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

## Phase J -- post-v0 features and fixes

- **[0048](0048-configurable-tool-call-cap.md)** Configurable + resumable
  per-turn tool-call cap.
- **[0049](0049-configurable-tool-result-cap.md)** Configurable per-tool-result
  truncation cap.
- **[0050](0050-cli-env-flags.md)** `--env` flags on `outrig run` /
  `outrig mcp`.
- **[0051](0051-image-name-container.md)** `image-name` field on
  `[containers.<name>]` (skip `buildah build`).
- **[0052](0052-cargo-doc-private-link-warnings.md)** `cargo doc
  --no-deps` private-link warnings cleanup.
- **[0053](0053-embedded-container-config.md)** Embedded
  `container.toml` -- image-side MCP config.
- **[0054](0054-outrig-mcp-attach.md)** `outrig mcp --attach` to an
  existing container.
- **[0055](0055-outrig-mcp-self.md)** `outrig mcp self` --
  self-description MCP server.
- **[0056](0056-outrig-design-prompt.md)** `outrig design prompt` --
  one-shot prompt printer.
