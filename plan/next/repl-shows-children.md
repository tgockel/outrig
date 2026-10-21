# The REPL shows only the main agent's code

## Context

`run-new` prints each piece of Python the main agent submits as it starts
(`crates/outrig-cli/src/cli/run_new/converse.rs`). Since `0003-30` a child agent's code runs under
the child's own event subject, `agent/c1` and so on, and the REPL shows only the main agent's: a
child's submissions, its replies, and its rounds reach the event log and nothing on the terminal.
A user watching a session whose main agent awaits a child sees nothing until the child answers.

## Shape

- Each child submission is printed under a heading naming the child -- its id and the name its
  spawn gave it -- so it cannot be read as the main agent's.
- A child's launch, release, and wedging get one line each, from `agent.spawned`,
  `agent.released`, and `agent.wedged`.
- Whether to print a child's code at all is a choice the user can make: a session with many
  children working at once interleaves their code line by line.

## Open questions

- Whether children's code is shown by default, or only on request.
- How the REPL names a grandchild: by id alone, or by its path from the main agent.
