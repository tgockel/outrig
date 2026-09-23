# 0003-08 -- The user reaches the agent through a channel, not a prompt

## Context

`0003-05` shipped a CLI that passes a typed line to the model as an ordinary prompt. That is the
arrangement this phase exists to replace. `README.md` lists the `user` channel as a deliverable:
messages are announced to the model by name and count, and reading one is an act the generated
code takes.

`messages.md` designs the layer for more than one relationship so the shape does not have to
change when subagents arrive, but only the `user` channel is built here. The endpoint offers three
operations -- `receive`, `send`, `pending` -- and `pending()` is what makes notification possible
without disclosure: the host can tell a model that three messages are waiting without putting any
of them in its context.

The distinction between the model's own text and a channel send is worth keeping straight. Model
text is running commentary. A send is a deliberate message, and it is the only one available to
code running after a round has ended -- which is what makes it more than a second way of printing.

## Goal

A typed line arrives as a message the agent's code reads, and the agent can send to the user from
code that runs after it stopped writing.

## Deliverables

- Endpoints in the interpreter: `receive`, `send`, `pending`, reachable as
  `runtime.channels["user"]`.
- The `msg` and `send` protocol messages, carrying the agent id like everything else.
- **An input pump that reads while a round runs**, which the existing REPL does not do. Its
  `select!` races *reading a line* against an interrupt, and then the round callback runs with
  nothing polling stdin -- so a line typed mid-round is buffered by the terminal and delivered
  only after the round ends. Redirection through `runtime.wait` is unreachable until this changes.
  Reuse the terminal helpers; do not reuse the blocking callback lifecycle.
- **One owned round driver.** Input arriving mid-round enqueues a message on the channel. It must
  not start a second model loop.
- The REPL routing a typed line into the channel rather than into a prompt, and rendering a `send`
  to the terminal, including a send from a background task while a round is running -- two writers
  to one terminal need a stated arrangement.
- **Announcement by name and count.** The model is told what is waiting, not what it says.
- The contract enforced: the `user` channel carries text both ways, and construction validates
  against the serializable subset rather than failing at first send.
- **`receive()`'s shape decided.** `messages.md` marks this as a proposal rather than a settled
  call -- body-only with a separate `receive_delivery()` for attribution, against one method
  always returning an envelope -- and says it should be decided before the first channel task is
  written. This is that task.

## Acceptance

- A typed line reaches the agent as a message its code reads, and the agent's reply reaches the
  terminal through a send.
- **Typed input reaches the channel while a round is running.** Through the CLI test seam: with a
  round in flight, type a second line *without* Ctrl-C and observe it arrive as a message the
  agent can read. A test that posts a message to the interpreter directly passes while the input
  pump is broken, which is why this one drives the seam. The fuller claim -- a wait yields to the
  input, its operation survives, and the same round continues -- needs `runtime.wait`, so it is
  `0003-09`'s acceptance rather than this task's.
- **`pending()` reports a count without the body entering the model's context.** Checked by
  asserting on what the model was actually sent, not by reading the code.
- Receiving consumes; notification does not. Reading twice does not deliver the same message
  twice, and declining to read leaves it queued.
- A send from a background task, after the round that started it ended, reaches the user.
- Ordering holds within the channel.
- A contract declared outside the serializable subset is refused at construction, with the
  offending type named.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **`receive()` versus an always-enveloped receive -- Open, and this task closes it.**
   `messages.md` argues the common case stays readable if the body comes back bare, against
   uniformity. Whichever is chosen goes into the page as a decision rather than a proposal.
2. **What the model is told when a message arrives mid-round -- Recommended: the announcement,
   not the body.** Anything else undoes `pending()`.

## Dependencies

- **Hard: 0003-05.** There is no REPL to route from until the command exists.

## See also

- `plan/phase/0003-python/messages.md` -- channels, endpoints, contracts, delivery rules, and the
  undecided `receive` shape.
- `crates/outrig-cli/src/repl.rs` -- where a typed line currently becomes a prompt.
