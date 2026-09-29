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

## Decisions

- **`MessageAvailable` derives from `BaseException` (the maintainer's call).** This closes the
  open question `execution-and-rounds.md` carried, and the page now records it.
  - An `except Exception:` written around a wait cannot swallow the redirection. With
    `Exception`, `while True: try: await runtime.wait(fs) except Exception: ...` would never
    return again, since input wins and nothing is consumed.
  - `asyncio.CancelledError` moved to `BaseException` in 3.8 for the same reason. In 3.13 only
    `KeyboardInterrupt` and `SystemExit` are handled specially by `Task.__step` and
    `Handle._run`, and `Kernel._run` already catches `BaseException`, so nothing in the
    interpreter treats it differently from any other failure.
  - The preamble says `except Exception` does not catch it.

- **Reached as `runtime.MessageAvailable` (the maintainer's call).**
  - Defined at module level and aliased as a class attribute of `Runtime`. No new name enters the
    agent's namespace, and `help(runtime)` lists it.
  - At module level so its qualname is `MessageAvailable`, which is what the traceback and the
    tool's `[this code raised ...]` status then read. Defined inside `Runtime`, both would read
    `Runtime.MessageAvailable`.
  - It carries `.channels`, the names of the channels holding messages, and its text names them
    as the host's announcement does: `runtime.channels["user"]`.

- **`asyncio.tasks._wait`, ported, with the channels watched beside it -- not `asyncio.wait` in a
  helper task.**
  - Each future gets one callback, `completed`, which counts completions and runs the test
    `_wait` runs as each future completes. It and the timer wake one private future, `finished`,
    when asyncio's wait would return. Only `FIRST_EXCEPTION` reads an exception, as asyncio's
    does, since reading one marks it retrieved.
  - Each pass checks the channels, then `finished`, and otherwise waits on `finished` and a
    future the next delivery wakes. So the loop turns again only when a message arrives.
  - A helper task running `asyncio.wait` itself would have handed it `return_when` whole. But it
    is a task this program starts in agent context: `asyncio.all_tasks()` lists it, agent code
    can cancel it, and `_in_agent_task` would count it as agent code. It would also have checked
    for input before asyncio checked the arguments.

- **Asyncio's argument checks, verbatim, plus one.** The four checks and their messages are copied
  from the payload's 3.13.15 `asyncio.wait`. An iterable that turns out empty once read, such as
  `iter(())`, raises the same `ValueError`; asyncio's `_wait` fails an `assert` there instead.

- **Input is checked first on every pass, and without suspending.** It wins over futures that are
  done and over a timeout that has passed. Raising at once means that code which catches it and
  waits again spins without yielding, which the runaway probe detects, rather than keeping the
  loop turning forever.

- **It suspends once before returning, as `asyncio.wait` does, and never before raising.** A
  callback added to a future that is already done runs only when the loop turns, so even a wait
  whose `return_when` is met on entry returns after one suspension. The first cut returned at
  once there, a divergence it recorded; the review below removed it. A suspension added before
  *raising* would hide the spin above from the probe.

- **The watcher is a receive's waiter that takes nothing.** `Endpoint._watch` checks the queue and
  parks the wait's future in `_receivers` under one lock, so a delivery between the check and the
  park still wakes it; `_deliver` and `_wake_all` are unchanged. `_unwatch` checks membership
  before removing, so it raises nothing and allocates no `ValueError` to catch.

- **Every channel is watched, and the set is read again on each pass.** A channel added to the
  kernel is watched with no change to `wait`. That is how "any future host-delivered wake takes
  the raising path" is built rather than only documented: a wake the host adds is a message on a
  channel of its own. A channel added during a wait is watched from the wait's next pass.

- **A message another receive takes first does not end the wait.** Woken, the wait finds nothing
  unread and carries on. The reverse order -- the wait raises, then a background receive takes the
  message -- leaves the exception's "it has not been read" out of date; two readers on one
  channel is an arrangement the agent's code made.

- **Nothing changed on the host.** A `MessageAvailable` ends the execution as an error result like
  any other, and the per-call announcement is at its head. The pump, the announcement, and the
  round going on after an error result all came with `0003-08`; `public-api.txt` is untouched.

- **Not built: `messages.md`'s "`runtime.wait()` reports the failure once".** No endpoint can fail
  yet, so there is nothing for a wait to report.

- **How the tests know a wait has suspended.** A single `loop_turns` round trip does not show it:
  the execution's first step can still be queued behind the inventory. The code under test
  instead schedules a marker with `call_soon` just before its `await`, which runs only once that
  await has suspended -- a line on stderr in `interpreter_tests.rs`, a file in `agent_tests.rs`
  and `run_new_e2e.rs`. When a post ends a suspended wait, the reader thread's answer and the
  loop's result can arrive in either order, and `post_waking` takes both.

- **Mutation-checked.**
  - Checking `finished` before input fails `input_wins_when_it_arrives_with_a_completion`. Since
    the review, `input_wins_over_a_finished_task` no longer catches this alone: its message is
    queued before the call, and the first pass cannot find `finished` done.
  - Registering with each future again on every pass, as the first cut did, fails
    `a_wait_registers_with_each_future_once`.
  - A `_watch` that only checks, and parks nothing, fails
    `a_message_ends_a_wait_naming_its_channel_and_takes_nothing` at its 20-second step.
  - `MessageAvailable(Exception)` fails `except_exception_does_not_catch_message_available`.
  - A wait that cancels what is still pending when it returns fails
    `a_timeout_returns_without_raising_or_cancelling`. That test reads the task in a later
    execution, since a task that has been cancelled is not done until its next step.

- **What `/simplify` changed (the maintainer chose the merge).** An independent version was
  smaller, and gave up three things this one keeps:
  - its timeout was a deadline checked before any suspension, so a `timeout=0` polling loop never
    yielded and the task it polled never ran;
  - it called `set(fs)` before asyncio's empty check, so `wait(None)` raised a `TypeError` where
    asyncio raises its `ValueError`;
  - its `Runtime` edited `Endpoint._lock` and `_receivers` directly.

  Its tests also read a timed-out task in the same step the wait returned in, which a wait that
  cancels its pending tasks passes. Taken from it: the shorter preamble paragraph, which does not
  restate a signature the model already knows; the refusal test's body; and the input-wins test,
  which also catches the exception by name. Dropped from this one: the `asyncio.shield` row, an
  `iscoroutinefunction` check, and a second raise before the message was read. `_satisfied` and
  `_RETURN_WHEN` stayed helpers rather than being folded into the loop; the review below replaced
  `_satisfied` with the per-completion callback.

- **A review after the commit found the wait quadratic in its futures (taken).**
  - Each pass looked at every future to find the done ones, and handed `asyncio.wait` every
    future still pending, which registered a callback on each and removed it again. `N` futures
    finishing one loop turn apart cost `N` passes of up to `N` each.
  - Measured against the payload with futures finished one per turn: 0.19 s, 0.69 s, and 2.65 s
    of CPU for 1,000, 2,000, and 4,000 futures, where `asyncio.wait` took 0.005 s, 0.010 s, and
    0.020 s. After the fix `runtime.wait` took 0.007 s, 0.011 s, and 0.019 s.
  - Reproduced by `a_wait_registers_with_each_future_once`, which counts registrations: 2,550 for
    100 futures before the fix, 100 after.
  - Its second note, on a failing `TaskGroup`'s status line, was already filed. The entry gained
    the `BaseExceptionGroup` case, which is how a redirection raised in a `TaskGroup`'s task
    arrives.

- **The CLI's redirection is covered under the e2e feature, as `0003-08`'s pump is.**
  `a_typed_line_ends_a_wait_and_the_round_goes_on` drives the binary under podman. The same
  redirection through `PythonAgent`, without the pump, is
  `a_message_ends_a_wait_and_the_round_goes_on`, which `cargo test --workspace` runs.

- **Follow-ups filed.**
  - `plan/next/an-exception-group-status-names-its-border.md`: a failing `TaskGroup`'s status
    line reads `[this code raised +------------------------------------]`. Checked against the
    payload while this was built, and likelier now that the preamble tells the model to wait on
    tasks.
  - `plan/next/stop-a-held-slot-outside-any-call.md` now says that a holder parked in
    `runtime.wait` is freed by the next typed line, and that a bare `await` is the case left.
