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
