# REPL slash-command dispatcher

`Repl::run`/`run_with` carry one typed callback pair per slash command (`on_tools`,
`on_reset`, `on_sidecar` since 0081). Adding `/sidecar` grew the generic signature by two
type params and touched every `run_with` call site in `tests/repl_io.rs`.

When the next command lands, fold the per-command callbacks into a single caller-supplied
dispatcher -- e.g. `on_command(cmd: &str, args: Vec<String>) -> Option<String>` with `None`
meaning "unknown command" -- keeping only `/help` and `/quit` built in. That also moves
`HELP_TEXT`'s command lines out of the transport layer, so one file owns each command's
name, help line, and semantics.

Related altitude notes from the 0081 review, worth folding into the same pass if it
happens nearby:

- An `llm::RebuildingAgent` wrapper (owns adapters + dirty flag + rebuild recipe, exposes
  `extend_tools` / `run_turn`) would replace the `Rc<RefCell<Rc<RigAgent>>>` +
  `Rc<Cell<bool>>` plumbing in `run.rs`.
- A `SessionRuntime { containers, mcp_arcs, network, watcher }` struct would collapse the
  four parallel `&mut` handles threaded through `execute` -> `RunInnerArgs` ->
  `SidecarCmdState` -> `teardown`.
