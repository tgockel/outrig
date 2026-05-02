# 0019 -- Agent loop (`outrig run`)

## Goal

The headline feature. Wire every previous task into a working `outrig run`: load configs,
ensure image, start container + bootstrap user, connect every MCP, build adapters, build Rig
agent, hand to REPL, run until EOF, clean up.

## Deliverables

- `src/cli/run.rs::execute(args: RunArgs) -> Result<i32>` orchestrating the flow:
  1. Resolve repo + global config paths (0002).
  2. Load + merge + validate (0005).
  3. Resolve agent (0012).
  4. Resolve container-config (CLI flag > agent's `container` > `default-container`).
  5. Ensure image (0007).
  6. Start container (0008).
  7. Bootstrap user (0009).
  8. Connect every MCP server in `[containers.<n>.mcp]` (0010); collect their `McpClient`s.
  9. List tools per server, build `McpToolAdapter`s with sanitized names (0011).
  10. Build the Rig agent (0012).
  11. Print the banner exactly as `doc/usage/run.md`'s "REPL banner" example.
  12. Run REPL with the prompt callback that calls `agent.chat(line, &mut history).await`.
  13. On exit: shut down each MCP client, stop container, return appropriate exit code.
- Slash command wiring:
  - `/tools` lists every registered tool with description and source server.
  - `/reset` clears the conversation history (`history.clear()`); container + MCPs stay up.
  - `/help`, `/quit` from 0018.
- Iteration cap: if Rig's chat loop exceeds 50 tool calls in one turn, print
  `[outrig] tool-call iteration cap (50) reached; ending turn` on stderr and yield to user.
  (If rig-core has its own cap, this is a safety net.)
- Replace the "not implemented" stub from 0001 with this real implementation.
- `tests/run_smoke.rs` (`#[cfg(feature = "e2e")]`): scripted prompt -> assert tool call landed
  + a stdout reply was printed.

## Acceptance

- `cargo run -- run` from a configured fixture repo opens the REPL, banner shows
  agent/model/provider, `/tools` lists `fs__*` tools, a single user prompt produces at least
  one MCP tool call and a stdout reply, Ctrl-D shuts down cleanly with no orphan containers.
- `cargo test --features e2e run_smoke` passes.
- Drop the `> TODO: Incomplete` markers on `doc/usage/run.md` (most -- multi-line input is
  still deferred), `doc/README.md` (introduction), `doc/concepts/containers.md`.

## Dependencies

- 0007-image-build
- 0009-runtime-user-bootstrap
- 0011-rig-tool-adapter
- 0012-llm-resolver
- 0018-repl-skeleton

## Notes

- This task doesn't include sessions. Running this without 0020 means no `session.json` is
  written and no per-MCP stderr is captured to disk. That's fine for the v0 of `outrig run`
  -- 0020 layers session persistence on top.
- Container cleanup is critical. Test by `^C`-ing manually mid-turn and verifying
  `podman ps -a --filter name=outrig-` is empty.
- The banner text must match `doc/usage/run.md` verbatim (modulo concrete IDs/timestamps).
