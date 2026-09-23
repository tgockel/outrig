# 0003-05 -- `outrig run-new` holds a session, and `run-legacy` names the old one

## Context

The first four tasks build a pipeline nobody can use. This one makes it a command.

`crate-split-tradeoffs.md` settles the naming: `run` keeps its current meaning and its current
code, `run-new` is added beside it, and `run-legacy` is added at the same time as an alias for
`run`. Adding the alias early is what makes the eventual retarget cheap -- by then `run-legacy`
has been the stable name for the old behavior for a while, and the switch strands nobody who had
been typing `run` and meaning the old thing. Retargeting `run` is a later milestone.

Interactive from the start, deliberately. The milestone's wording is "holds an interactive session
end to end", and a one-shot front end would be built only to be thrown away.

One staging note: this command passes the user's typed line to the model as an ordinary prompt.
The `user` channel that replaces that arrangement is `0003-08`. The phase's deliverable is the
channel; gating the first runnable CLI on it buys no verification the prompt does not already
give.

## Goal

A person can type at `outrig run-new`, watch an agent write and run Python against the workspace,
and get an answer -- with names the agent bound surviving into the next thing they type.

## Deliverables

- `run-new` as a subcommand: one enum variant, one match arm, and a reuse of the existing
  `repo_cmd_ctx` preamble. Global flags are inherited.
- `run-legacy` as an alias for `run`, reaching the same code.
- Session setup that starts the interpreter alongside the container the session already builds,
  and tears it down with the session.
- The REPL loop, reusing `repl.rs` rather than writing a second one. Terminal interaction stays in
  `outrig-cli`; `harness-components.md` is explicit that `run-new` drives the same loop the
  existing command does.
- A startup line that says what a person needs to know: the model, and that Python is ready with
  its version.
- **State across rounds, demonstrated rather than assumed.** Binding a name in one round and using
  it in the next is the phase's whole premise and the thing a reader will try first.
- **A stated policy on which integrations start.** `README.md` says the sandbox holds no
  credentials until isolation lands, and in the same breath says MCP servers still run and the
  model is simply not handed their tools. Those are in tension: a primary-placed server started
  with a resolved token is a same-UID process whose environment arbitrary Python can read where
  `/proc` policy permits. Not exposing a tool is not the same as putting the credential out of
  reach. Either do not start credential-bearing primary integrations for `run-new`, or narrow the
  claim to "OutRig does not inject credentials into the agent interpreter" and document the
  same-container exposure. Operator-supplied images, files, and mounts stay outside any blanket
  guarantee either way.

## Acceptance

- An end-to-end run against a real model: typed message in, Python executed against
  `/workspace`, answer out. This is the milestone's own criterion and it is checked by running
  it, with the transcript recorded in the task's `## Decisions`.
- **A name bound in one round is still bound in the next.** The test that distinguishes this
  phase from the legacy loop.
- `run` behaves exactly as before -- the existing `run` tests pass unchanged, and `run-legacy`
  reaches the same path. A test that invokes both and compares behavior, not an eyeball.
- `outrig run-new --help` describes the command without implying `run` has changed.
- **A dummy secret in a primary MCP configuration exercises whichever policy was chosen**, and
  the assertion is on the startup behavior rather than on `os.environ` inside Python -- reading
  the interpreter's own environment proves only that injection was avoided, not that the
  credential is unreachable.
- Ctrl-C at the prompt returns to the prompt rather than ending the session, matching `run`.
  Whether it reaches the interpreter is `0003-06`; here it must at least not make things worse.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whether `run-new` gets its own session-dir layout -- Recommended: no.** It is a session like
   any other. `logs/events.jsonl` arrives in `0003-13`.
2. **What the model is told about its one tool -- decide and record.** The preamble is the model's
   only orientation until `0003-10`, and `discovery.md` already argues that what an agent cannot
   learn by looking belongs there.

## Dependencies

- **Hard: 0003-04.** There is no round to drive until the loop and the tool exist.

## See also

- `plan/phase/0003-python/crate-split-tradeoffs.md` -- "Add `run-new`, and rename later".
- `crates/outrig-cli/src/cli/run.rs` and `repl.rs` -- the existing command and the line editor
  this reuses.
- `crates/outrig-cli/src/cli/session_setup.rs` -- where a session's container and logs are built.
