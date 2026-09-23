# 0003-06 -- A runaway execution is survivable, and the user can end one

## Context

`runtime-protection.md` is blunt about why this is not optional polish: without it the first
`while True:` ends the session, and an agent that cannot survive its own mistakes cannot be
evaluated. It arrives immediately after the CLI because the CLI is what makes it reachable.

The page settles more than the prototype proved, and the additions matter. There are **two**
failures needing **two** remedies, and they are mutually exclusive. A wedge -- a loop that stopped
turning -- is recovered by a SIGINT raised on the reader thread, because `call_soon_threadsafe` is
exactly the queue a wedged loop is not draining. An execution suspended on an await that never
resolves is the opposite failure: measured, the SIGINT lands in the event loop rather than in
agent code and the interpreter exits, while `task.cancel()` through `call_soon_threadsafe`
recovers it cleanly.

Neither is reachable by a user today. The interrupt message has one sender, the probe, and the
REPL's Ctrl-C never reaches the interpreter -- it drops the host's side of the turn. That is fine
for a wedge, which the probe detects alone; it is not fine for a suspended await, which the probe
correctly calls healthy. From outside, a dead wait and a long one are identical, and only the user
knows which.

## Goal

A session survives generated code that never yields, and a person who knows a wait is dead can end
it without stopping the container.

## Deliverables

- The interrupt path, ported: an `interrupt` message handled **on the reader thread**, raising
  SIGINT, with a handler installed from inside the loop so it overrides the one `asyncio.run` set.
- The liveness probe on the host: probe before interrupting, so a legitimately long execution
  keeps its slot indefinitely and only one that has gone quiet is acted on.
- **A targeted cancel**, using the task handle `0003-03` retained: `task.cancel()` delivered
  through `loop.call_soon_threadsafe`, which is the remedy for a suspended execution.
- **Ctrl-C reaches the interpreter**, targeting a stable execution id, and the host keeps its
  correlation until a terminal reply arrives or the execution is recorded unresolved. Dropping the
  host's future *and* sending a cancel loses the reply that says whether the slot came back.
- **The wrapper catches `CancelledError` deliberately.** It derives from `BaseException`, so an
  ordinary handler misses it -- and catching `BaseException` indiscriminately would swallow the
  interrupt path's `KeyboardInterrupt` in cases that should end the interpreter.
- **A cancel request is not a completion.** Generated code may suppress `CancelledError`, so the
  slot is freed by a terminal reply and never by having sent the request.
- **The state table extended past the foreground execution.** Three shapes it omits. A retained
  background task that wedges after its originating execution finished leaves no foreground
  execution, so the handler's `_running` gate declines to raise and nothing recovers -- and if a
  *different* foreground execution is suspended, the signal lands in the background code rather
  than in that execution's wrapper. Native code that holds the GIL starves the sibling loops and
  the reader thread alike, which the pure-Python switch-interval measurement does not cover. And a
  synchronous `subprocess.run` blocks the agent loop while being perfectly healthy, so a failed
  probe there is not evidence of a runaway. Each shape either gets a remedy or is named as session
  failure; none may be left implied.

## Acceptance

- `while True: pass` is recovered: the execution returns an error result with a traceback, the
  interpreter survives, and the next submission runs. Repeated enough times to show it is not
  luck -- the prototype's evidence was 5 of 5.
- **A bare `await` on a future that never resolves is ended by Ctrl-C**, the slot is freed, and
  the next prompt is accepted. This is the failure the probe cannot see and the one a user is most
  likely to hit.
- **Applying the wrong remedy is not silently tolerated.** A test that a suspended execution is
  cancelled rather than signalled, since signalling it exits the interpreter.
- A cancel delivered after its execution already finished does not affect the next one.
- An execution that suppresses `CancelledError` does not free the slot until it replies.
- **A background task wedges with no foreground execution**, and again while a foreground
  execution is suspended. Whatever happens is the documented outcome, and the execution ids stay
  correct in both.
- **A failed probe alone does not justify interrupting a synchronous `subprocess.run`.** It blocks
  the loop while being perfectly healthy, so the probe's verdict is not evidence about the child.
  The task records what additional state gates an *automatic* escalation.
- **An explicit Ctrl-C on that same wait has its own stated outcome**, which is a different
  question: a user may legitimately stop healthy work. Execution correlation survives it, and the
  outcome acknowledges that cancelling Python does not necessarily end the descendant it started.
- Interrupting with nothing executing changes nothing.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whether the probe's verdict picks the remedy automatically -- Open.** `runtime-protection.md`
   records that the probe is evidence about the recent past, not a guarantee about the instant of
   delivery: an execution can yield between the probe and the signal. Targeted cancellation is
   cheap and safe to attempt; escalating to a signal can cost the session. Prefer attempting the
   cancel first and escalating only on evidence, and record what was chosen.

## Dependencies

- **Hard: 0003-05.** Ctrl-C has nowhere to come from until there is a prompt.

## See also

- `plan/phase/0003-python/runtime-protection.md` -- the three SIGINT states, the two remedies, the
  probe as evidence, and the measurements behind each.
- `plan/phase/0003-python/execution-and-rounds.md` -- why a bare `await` is a reasonable thing to
  write, and what it gives up.
- The prototype branch's `kernel-findings.md` -- the interrupt evidence, and the two rejected
  alternatives.
