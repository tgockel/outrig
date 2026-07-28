# Report every MCP server that failed to start, not the first

## Context

`Outrig::launch` connects MCP servers in a serial loop that gives up on the first failure
(`crates/outrig/src/outrig_.rs:840-854`):

```rust
for (name, server) in &mcp {
    let client = McpClient::connect_via_podman_exec_with_source(...).await?;
    tools.extend(tool_handles(name, client.list_tools().await?));
    clients.insert(name.clone(), Arc::new(client));
}
```

The `?` is the whole issue. An image-config naming three servers whose binaries are all missing
from the image reports one, the user fixes it, the next launch reports the second, and so on --
three build-and-relaunch cycles to learn three facts that were all knowable in the first pass.

The diagnostic itself is good: `OutrigError::McpStartupFailed` (`crates/outrig/src/error.rs:137`,
struct at `:177`) carries the server name, the exit status, the argv, and a stderr tail. There is
just only ever one of them.

## Goal

Attempt every declared server, then report all the failures together, so one launch tells the
user everything that is wrong with the image.

## Deliverables

- **Collect rather than short-circuit.** Run the loop to completion, accumulating failures
  instead of returning at the first. Successfully-connected clients from a failed launch are
  shut down on the way out, the same way the existing failure path unwinds them.

- **An error that holds many.** `McpStartupFailed` becomes -- or gains a sibling that holds --
  a list, formatting as one message per failed server with a leading count:

  ```
  3 of 4 mcp servers failed to start:

    fs      exit 127  command: mcp-server-filesystem /workspace
            stderr: exec: "mcp-server-filesystem": executable file not found in $PATH
    git     exit 127  command: mcp-server-git /workspace
            stderr: exec: "mcp-server-git": executable file not found in $PATH
    shell   exit 1    command: bash -lc exec shell-mcp-command
            stderr: bash: shell-mcp-command: command not found
  ```

  Keep the single-failure rendering close to today's, so the common case does not get noisier
  to serve the rare one.

- **Decide what a partial success means, and say so.** Today a failure is total: `launch`
  returns `Err` and no session exists. Collecting failures does not by itself change that, and
  it should not change it silently. The default stays fail-the-launch. Whether an opt-in
  "start with a reduced tool set" mode is worth having is a separate question -- `on-failure =
  "warn"` already exists for *sidecars* (`plan/done/0079-sidecar-core-exec-stdio.md`), so if
  primary-hosted servers ever gain the same knob it should reuse that vocabulary rather than
  invent one.

- **Sidecar bring-up is already batched differently.** Check whether the sidecar path
  (`Outrig::add_sidecar`, `crates/outrig/src/outrig_.rs:873-896`) has the same
  first-failure-wins shape and, if so, give it the same treatment -- a session with two broken
  sidecars should report both.

- **Docs.** `doc/concepts/mcp-servers.md`'s Lifecycle section says "If any server fails to
  initialize, `outrig run` reports the error on stderr and exits before the REPL starts -- you
  don't get partial sandboxes." That stays true; the sentence just needs to say *errors*.

## Acceptance

- An image-config declaring three servers, all with missing binaries, produces one error naming
  all three with their exit codes, argv, and stderr tails.
- An image-config with one missing binary produces a message no noisier than today's.
- A launch where every server starts is unchanged.
- Clients that connected before a later server failed are shut down; no container or child
  process is leaked by a partially-successful launch. Assert this directly -- it is the part
  that the short-circuit made trivially true and that collecting makes non-trivial.
- The `library_surface` e2e still passes.

## Design forks

Each item leads with its status: **Resolved** (committed here), **Recommended** (a lean a
prototype should confirm), or **Open** (deferred).

1. **Error shape -- Recommended: one variant holding a list, not a list of variants.** Callers
   that match on `McpStartupFailed` want "MCP startup failed" as one condition; making them
   iterate a `Vec<OutrigError>` pushes the aggregation onto every consumer. Confirm against the
   downstream consumer: CocoClaw matches this variant to render a per-harness message
   (its `0006-08`), and one variant carrying many entries is what that wants.

2. **Concurrency -- Open.** Connecting servers in parallel would also shorten startup, and the
   collect-all shape is a prerequisite for it. Worth doing, but it interacts with the
   name-ordered connect the logs and `outrig logs <session> <server>` assume, so it is its own
   change.

## Dependencies

- None.

## Consumers

- CocoClaw's phase 0006 `0006-08-image-contract-failure` explicitly gives up reporting all
  missing commands at once and files this as the reason. When this lands, that task's
  "N others were declared and not reached" line becomes a full list.
