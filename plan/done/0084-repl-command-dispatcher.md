# 0084 -- REPL slash-command dispatcher

## Goal

`Repl::run`/`run_with` carry one typed callback pair per slash command (`on_tools`,
`on_reset`, `on_sidecar` since 0081). Adding `/sidecar` grew the generic signature by two
type params and touched every `run_with` call site in `tests/repl_io.rs`. Fold the
per-command callbacks into a single caller-supplied dispatcher so adding a command no
longer grows the signature.

## Deliverables

- A single dispatcher -- e.g. `on_command(cmd: &str, args: Vec<String>) -> Option<String>`
  with `None` meaning "unknown command" -- keeping only `/help` and `/quit` built in.
- `HELP_TEXT`'s command lines move out of the transport layer, so one file owns each
  command's name, help line, and semantics.
- Related altitude notes from the 0081 review, worth folding into the same pass if it
  happens nearby:
  - An `llm::RebuildingAgent` wrapper (owns adapters + dirty flag + rebuild recipe,
    exposes `extend_tools` / `run_turn`) would replace the `Rc<RefCell<Rc<RigAgent>>>` +
    `Rc<Cell<bool>>` plumbing in `run.rs`.
  - A `SessionRuntime { containers, mcp_arcs, network, watcher }` struct would collapse
    the four parallel `&mut` handles threaded through `execute` -> `RunInnerArgs` ->
    `SidecarCmdState` -> `teardown`.

## Acceptance

- Existing REPL behavior is unchanged; `tests/repl_io.rs` passes (updated to the new
  dispatcher shape).
- Adding a new slash command touches the command's owning module, not the `Repl` generic
  signature or every `run_with` call site.

## Dependencies

None (follows up on completed task 0081).

## Decisions

1. **Scope includes both 0081 altitude notes** (user-confirmed): `llm::RebuildingAgent` and
   `SessionRuntime` land in this pass alongside the dispatcher.
2. The dispatcher takes owned `(String, Vec<String>)` and returns `Option<String>` (`None` =
   unknown command). Owned params dodge the async-closure HRTB wall a borrowed `&str` would
   hit; one small alloc per slash command is irrelevant at REPL cadence. No `Send` bounds --
   the binary's runtime is current-thread.
3. `/help` is composed from structured `HelpEntry { syntax, description }` values
   (user-confirmed over a raw pre-formatted string): the caller passes its commands' entries,
   the `Repl` sandwiches them between its built-in `/help` and `/quit` lines and pads one
   column across all lines. Production output is byte-identical to the deleted `HELP_TEXT`,
   locked by a unit test against the old literal.
4. The unknown-command notice echoes the **raw** post-`/` text as typed (args, original
   whitespace), matching the old exact-match arms for every unknown input. Callers keep
   `/tools foo` / `/reset foo` unknown by guarding on `args.is_empty()`; built-ins `/quit`
   and `/help` match only arg-less, so `/quit foo` stays unknown.
5. Two accepted edge changes (undocumented, untested quirks of the old exact-match arms):
   trailing-whitespace commands (`/quit `) now execute, and tab-separated arguments
   (`/sidecar<TAB>add tools`) now parse. Preserving them would mean raw-string
   special-casing in the transport loop -- exactly what this task removes.
6. The command table (`REPL_COMMANDS`) and dispatcher stay in `cli/run.rs`, which already
   owns the command semantics; no new module. The two `/sidecar` help rows map to one
   dispatch arm, so dispatch is a `match` beside the table rather than derived from it.
7. `RebuildingAgent` **owns** its rebuild recipe (`ResolvedAgent`, cache root, and the
   `LlmRegistry` under `local-llm`), taking the pre-built first agent so `ProgressSpan`
   reporting (and any model download) stays in `run_inner`. Rebuild happens inside
   `run_turn`; the inner `Rc` keeps any `RefCell` borrow from spanning an await. The
   mem::take-history dance stays in `run_repl` (REPL-history policy, not agent policy).
8. `SessionRuntime` field order mirrors teardown order (watcher, mcp_arcs, network,
   containers) so an implicit `Drop` on an abort path stays orderly. `SessionSetup` is
   unchanged; each of the three `teardown` callers assembles the runtime itself
   (`show-merged` never has MCP arcs, so embedding it in `SessionSetup` buys nothing).
9. Pre-existing bug found and filed, not fixed: SIGINT during a turn drops the taken
   history vec before writeback, silently emptying conversation history
   (`plan/next/repl-interrupt-history-loss.md`).
