# Startup lost its MCP-config progress step

`ef5b983` (exec-stdio sidecars) deleted the CLI's `merged_mcp` wrapper, which was the only
thing emitting two startup progress lines:

```
[outrig] reading and merging MCP configuration
[outrig] MCP configuration ready: <n> servers
```

The wrapper was a thin `ProgressSpan` around `embedded::merged_mcp`. When MCP resolution
moved into placement planning, the span went with it and nothing replaced it, so startup
now jumps straight from `container user ready` to `MCP fs: initializing`. The work still
happens; only the reporting is gone.

This was not called out in the commit message, so it reads as incidental rather than
intended. It went unnoticed because the assertion that covered it lives in an e2e test
(`run_smoke.rs`, `run_drives_one_tool_call_and_prints_reply`) and e2e does not run in CI --
the test had simply been red on trunk ever since. Cutting 0.2.0-rc.1 found it; the stale
assertion was dropped there to get the suite green, which is what makes this entry worth
keeping.

Decide whether the visibility is worth restoring. Arguments for: the merge reads OCI labels
off the image and can be the step that fails on a malformed `org.outrig.mcp`, so a user
watching startup has no marker for where that happened. It is also the one remaining
startup phase with no progress line, which makes the sequence read as if a step is missing.
Arguments against: with placement planning the resolution is no longer a single contiguous
phase, so an honest span may not have a clean start and end any more, and inventing one
would report a phase that does not exist.

If restored, re-add the expected lines to the `assert_stderr_lines_in_order` list in
`run_smoke.rs` in the same change, so the test and the output stay in step.
