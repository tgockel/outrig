# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

## Phase J -- post-v0 features and fixes

- **[0057](0057-runtime-bind-mounts.md)** Runtime bind mounts.
- **[0058](0058-capability-profiles.md)** Capability profiles.
- **[0059](0059-network-interceptor-plumbing.md)** Network interceptor
  plumbing.
- **[0060](0060-network-interceptor-enforcement.md)** Network interceptor
  enforcement.
- **[0061](0061-outrig-mcp-http-sse.md)** `outrig mcp` HTTP / SSE
  transport.
- **[0062](0062-streaming-mistralrs-output.md)** Streaming output for the
  in-process mistralrs path.
- **[0063](0063-mistralrs-gpu-device.md)** GPU / non-CPU device support for the
  in-process mistralrs path.
- **[0064](0064-network-interceptor-mitm.md)** Network interceptor MITM.
