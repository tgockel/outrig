# 0003-09 -- `runtime.wait` mirrors `asyncio.wait` and watches the channels

## Context

`runtime.wait` is the phase's one genuinely novel primitive, and `execution-and-rounds.md`
reduces it to a sentence: `asyncio.wait`, which additionally watches the agent's input channels
and is tied to the round. Everything else follows, including the rule worth memorizing -- **it
returns for asyncio's reasons and raises for the runtime's.**

The prototype's version takes a single operation and offers no `return_when`, so an agent waiting
on both a CI run and a review cannot say "whichever finishes first". Mirroring asyncio fixes that
without inventing anything, and the constants are ordinary strings in CPython, so taking
`asyncio.FIRST_COMPLETED` directly costs nothing.

Two consequences come with the mirror. The return becomes `(done, pending)` rather than the
operation's result, which is better defined for N operations. And `timeout` means what asyncio
means: it returns when it expires, cancels nothing, and raises no `TimeoutError` -- "come back to
this in an hour", not "give up on it".

## Goal

An agent can wait on several operations, say which completion it cares about, bound the wait, and
still be reachable by a message -- with semantics a Python programmer already knows.

## Deliverables

- The signature, mirrored: `runtime.wait(fs, *, timeout=None, return_when=ALL_COMPLETED)`, an
  iterable first, keyword-only after, `(done, pending)` back, `asyncio`'s constants accepted
  directly.
- The three properties preserved: input wins over a completed operation, the operation is not
  cancelled, and a pending message is not consumed.
- `timeout` honored with asyncio's semantics -- and it earns its place, since an execution
  awaiting something that never resolves otherwise holds its slot until someone cancels it.
- **The rule, implemented and not merely documented.** Operations satisfying `return_when` and an
  expiring timeout return; input on a channel raises. Any future host-delivered wake takes the
  raising path, so adding one later changes no signature.
- Bare coroutines refused, which asyncio now enforces itself with
  `TypeError: Passing coroutines is forbidden, use tasks explicitly`.
- **The preamble gains the sentence.** `discovery.md`'s rule is that what an agent cannot learn by
  looking and needs every round goes in the preamble, and the signature is identical to asyncio's
  by design, so nothing in it discloses the channel watching.

## Acceptance

- Two operations, `FIRST_COMPLETED`: the wait returns when the first finishes and the other is in
  `pending`, still running.
- **A timeout returns `(done, pending)` without cancelling anything and without raising.** The
  easiest semantics to get wrong, and the one a model is most likely to lean on.
- A failed task is a *completed* task in `done` with its exception retrievable, not an exception
  raised out of the wait.
- A message arriving raises, naming the channel, without consuming the message, and the operations
  keep running.
- **The whole redirection, end to end through the CLI**, which `0003-08` could not complete
  without this task: a wait that will not finish on its own yields to a typed line, its operation
  is still alive afterwards, and the same round continues. This is the user-facing escape hatch
  and the reason the input pump exists.
- **A task passed to `runtime.wait` survives cancellation of the execution**, where the same task
  reached by a bare `await` does not. `execution-and-rounds.md` measures both; this is the
  property the page promises and the one an agent will rely on.
- Passing a bare coroutine raises `TypeError` rather than misbehaving.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Dependencies

- **Hard: 0003-08.** Watching the channels requires channels.

## See also

- `plan/phase/0003-python/execution-and-rounds.md` -- the mirror, the rule, the timeout semantics,
  and the measured cancellation table.
- `plan/phase/0003-python/messages.md` -- the three properties, and why the second is load-bearing
  beyond that page.
- `plan/phase/0003-python/discovery.md` -- why this goes in the preamble.
