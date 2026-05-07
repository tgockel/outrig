# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

## Phase I -- post-acceptance v0 polish

- **[0043](0043-verbose-flag.md)** Wire `--verbose` global flag.
- **[0044](0044-dedup-init-tracing.md)** Dedup `init_tracing` into `tests/common/mod.rs`.
- **[0045](0045-bound-stderr-capture.md)** Bound stderr capture in `run_capture`.
- **[0046](0046-pick-a-license.md)** Pick a license + ship `LICENSE`.

## Phase J -- post-v0 features and fixes

- **[0047](0047-env-value-for-build-args.md)** `${VAR}` substitution for
  `build-args` values.
- **[0048](0048-configurable-tool-call-cap.md)** Configurable + resumable
  per-turn tool-call cap.
- **[0049](0049-configurable-tool-result-cap.md)** Configurable per-tool-result
  truncation cap.
- **[0050](0050-cli-env-flags.md)** `--env` flags on `outrig run` /
  `outrig mcp`.
- **[0051](0051-image-name-container.md)** `image-name` field on
  `[containers.<name>]` (skip `buildah build`).
