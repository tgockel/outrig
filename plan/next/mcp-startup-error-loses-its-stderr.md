# A loaded host turns an MCP startup failure into an empty diagnostic

## Context

When an MCP server dies before `initialize`, `enrich_startup_error`
(`crates/outrig/src/mcp.rs`) builds the `McpStartupFailed` the user sees. It waits for the child
with a **250 ms** timeout and then reads the stderr tail regardless:

```rust
let exit_status = match tokio::time::timeout(Duration::from_millis(250), child.wait()).await {
```

The child is a `podman exec` -- a container engine in the path -- and the stderr file is handed
to it as a raw fd, so the bytes land when the child writes them. On a loaded host the wait loses:
`exit` becomes "still running (wait timed out)" and `stderr_tail` becomes "(empty)". The error
names a path and says nothing, which is the one thing `read_stderr_tail` exists to prevent.

## Why it might matter

It fires exactly when it is least affordable -- a busy machine, a failing server -- and it
degrades silently: the error still arrives, still looks well-formed, and has had its content
removed. A user reads "(empty)" and has nothing to go on but the file, which by then usually does
have the message in it.

## Evidence

0002-53 hit the test-side half of this. `mcp_handshake::stderr_captured_on_crash` allowed two
seconds for the same bytes and failed on an empty file during the third full live run; the window
is ten seconds now. But the test was already routing around the production path -- it asserts
`payload.name` and `payload.stderr_path` and then polls the *file* rather than asserting on
`payload.stderr_tail`, because the payload's tail is not reliable. That the test was written that
way is the symptom; 250 ms is the cause.

`enrich_startup_error_carries_name_command_and_stderr` does not reach it: its child is `false`,
which exits instantly and never probes the budget.

## Goal

The diagnostic a user gets for a server that died before `initialize` carries what the server
said.

## Deliverables

- **A wait sized for a container engine rather than for a process.** It must stay bounded --
  `serve_client` can also fail against a server that is still running, which is the only reason
  a timeout belongs here -- but a few seconds bounds a hang, where 250 ms bounds a flush.
- **A test that asserts on the payload.** With the wait fixed, `stderr_captured_on_crash` can
  assert `payload.stderr_tail` and `payload.exit` directly and delete its poll loop, which turns
  a timing-sensitive wait into a deterministic assertion and covers the promise the production
  path makes.

## Dependencies

- None.
