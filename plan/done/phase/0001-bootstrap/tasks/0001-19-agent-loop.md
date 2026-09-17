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

## Decisions

- **Cache-hit annotation moved to tracing.** `image::ensure_image` keeps its
  `Result<ImageTag>` signature and emits `tracing::info!(cache_hit = ..., ...)`
  in both probe-hit and build-success branches; the banner shows just
  `[outrig] image: outrig-cache:<hash>`. `doc/usage/run.md` was edited to drop
  `(cache hit)` from the example. Reason: keeping the bool in the return type
  meant either churning the only existing caller's API or adding a sibling
  function -- neither earned its keep when verbose users already get the
  status via `OUTRIG_LOG=info`.
- **REPL gained `on_tools` and `on_reset` callbacks.** `Repl::run` /
  `Repl::run_with` each grew two `FnMut() -> Future<String>` callbacks
  (separate from `on_prompt`) instead of a single dispatch closure or shared
  `Arc<Mutex<...>>` state. Keeps the REPL's "decode line, dispatch to
  callback, write to right stream" shape; pushes nothing about history
  lifetime into the REPL.
- **History sharing via `Rc<RefCell<Vec<Message>>>`.** Two callbacks need
  mutable access to the same history (prompt reads+writes; reset clears).
  Since `Repl::run` runs callbacks sequentially on one task, no `Send` or
  cross-thread sync is needed. The `RefCell` borrow is dropped before the
  rig `await` (via `mem::take` swap) to avoid `clippy::await_holding_refcell_ref`.
- **MCP cleanup before container.** `Arc::try_unwrap` each `McpClient` Arc
  after the agent (which held adapter clones) is dropped, then `shutdown()`
  each, then `container.stop()`. Order matters: MCP children are
  `podman exec` processes whose pipes go through the container; tearing down
  the container first races them.
- **Iteration cap via `OutrigPromptHook`.** Cap is `MAX_TOOL_CALLS = 50`,
  enforced by a custom `PromptHook` that prints `[outrig] tool call: ...` to
  stderr and returns `ToolCallHookAction::terminate(...)` after 50 calls. Rig
  surfaces this as `PromptError::PromptCancelled`; we map that to a stderr
  notice plus a one-line stdout reply (`(turn ended; tool-call cap reached)`)
  so a captured-stdout consumer doesn't see an empty assistant turn.
  History is unchanged for the capped turn -- partial mid-turn splicing was
  judged a correctness rabbit hole for v0.
- **`OutrigError::Configuration(String)`.** Added to distinguish user-
  configuration errors ("no --agent and no default-agent configured", etc.)
  from `NotImplemented`, which is for genuinely unimplemented commands.
- **`Container::session_suffix()`.** Centralizes "strip `outrig-` from the
  container name" next to where the name is constructed, so cli/run.rs
  doesn't repeat the parsing.
- **e2e test uses a hand-rolled mock OpenAI server.** ~60 lines on a
  `tokio::net::TcpListener`, no `wiremock`/`hyper`/`axum` dev-dep. The mock
  serves one `tool_calls` response then one final-text response. The test
  spawns `CARGO_BIN_EXE_outrig` as a subprocess, pipes stdin, and asserts on
  banner / tool-call trace / canned reply / no orphan-this-run-container.
- **`tokio::runtime::Builder::new_current_thread()` in the binary.** Lets
  the orchestrator use `Rc<RefCell<...>>` without `Send` bounds.
- **Sessions / per-MCP stderr persistence deferred to 0020.** This task
  writes MCP stderr into a tempdir keyed on session id, but doesn't
  materialize a `session.json` or surface it to `outrig ls`/`logs`/`discard`.
