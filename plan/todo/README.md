# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

The queue adopts the shared-filesystem techniques prototyped in
[prototype-podman-shared-fs](https://github.com/tgockel/prototype-podman-shared-fs): give a
process a *container's* filesystem view by joining its mount namespace and nothing else. 0088
through 0090 point it inward, so a third-party MCP image can serve the primary container's real
files; 0091 points it outward, so OutRig stops requiring `useradd` in every image.

| Task | Title                                                       | Dependencies |
|------|-------------------------------------------------------------|--------------|
| 0088 | Arguments for entrypoint-stdio MCP servers                  | none         |
| 0089 | `outrig-enter`, the static in-sidecar launcher              | none         |
| 0090 | Sidecars that share the primary container's filesystem view | 0088, 0089   |
| 0091 | Bootstrap the container user from the host, without useradd | none         |
