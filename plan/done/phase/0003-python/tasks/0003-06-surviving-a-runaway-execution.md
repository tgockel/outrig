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

1. **Whether the probe's verdict picks the remedy automatically -- Resolved.**
   `runtime-protection.md` records that the probe is evidence about the recent past, not a
   guarantee about the instant of delivery: an execution can yield between the probe and the
   signal. Targeted cancellation is cheap and safe to attempt; escalating to a signal can cost the
   session. Prefer attempting the cancel first and escalating only on evidence, and record what
   was chosen. See `## Decisions`: a user's request always cancels first and the check decides
   whether to signal too; the host on its own never cancels, and signals only a runaway.

## Dependencies

- **Hard: 0003-05.** Ctrl-C has nowhere to come from until there is a prompt.

## See also

- `plan/phase/0003-python/runtime-protection.md` -- the three SIGINT states, the two remedies, the
  probe as evidence, and the measurements behind each.
- `plan/phase/0003-python/execution-and-rounds.md` -- why a bare `await` is a reasonable thing to
  write, and what it gives up.
- The prototype branch's `kernel-findings.md` -- the interrupt evidence, and the two rejected
  alternatives.

## Decisions

- **When the host acts on its own (the maintainer's call): against a runaway, gated on CPU.**
  - Every 30 s from submission, the host checks: a CPU reading, the inventory as a probe (5 s),
    and a second reading. A loop that answers is healthy. One that does not, with its thread idle,
    is blocked in a system call -- a synchronous `subprocess.run`, a `time.sleep`, a read -- and is
    left alone. One that does not while its thread stays busy is interrupted.
  - That idle/busy reading is what gates an automatic escalation, which is what the acceptance
    criterion on `subprocess.run` asks to be recorded. The CPU time comes from a new `cpu` request
    the interpreter answers on its reader thread, from `time.pthread_getcpuclockid` on the loop's
    thread, so it answers while the loop does not.
  - The line is 0.1 of a CPU, not the plan's first 0.5. The design review measured a spinning
    thread's share beside other CPU-bound threads at 0.505, 0.336, and 0.252 beside one, two, and
    three; a blocked thread measures 0.000.
  - An interrupt proves nothing about the execution waited on: it may end a task another execution
    left spinning. So the host checks again after 5 s rather than concluding, and gives up -- the
    execution is `Unresolved`, holding its slot -- only after 3 interrupts each leave the loop
    spinning. The plan's first cut stopped waiting 10 s after any unanswered interrupt, which the
    review showed would mark a healthy suspended execution unresolved.
  - The host never cancels on its own, so a suspended execution keeps its slot indefinitely.
  - An unanswered first CPU reading means the reader thread is starved: native code holds the GIL.
    The host warns once and keeps waiting, since that may be long rather than endless.

- **After Ctrl-C the model reacts (the maintainer's call).** The stopped execution's result goes
  back to the model and the round continues.
  - The first press on an execution cancels it and runs a check with a 2 s probe. A loop that does
    not answer is interrupted too: with the widest scope if its thread is busy, otherwise aimed at
    the execution's own code, which breaks its blocking call.
  - A second press on the same execution stops waiting for it, after a 1 s last look for an
    outcome already on its way, so a cancel landing now is not reported unknown.
  - A press with no Python running returns `None`, and the REPL drops the round as it always did.
  - **My addition: the turn's later calls are skipped** (`CALL_STOPPED_TURN`), counted as started
    but not as run and not against the cap. The model wrote them before it knew the call before
    them would be stopped.

- **Fork 1: a user's request cancels first, always.** The check decides only whether to signal as
  well. The design review measured that an answered probe does not prove the cancel was
  delivered -- `task.cancel()` schedules a wake-up that lands a loop turn after the probe's reply
  -- so nothing relies on it.

- **The signal is `pthread_kill`ed at the main thread, not raised from the reader thread.**
  `raise(3)` is thread-directed, so the prototype's `signal.raise_signal` signalled the reader and
  the main thread ran the handler only at its next bytecode boundary. Measured on the payload:
  with `raise_signal`, `time.sleep(2)` ran its full 2 s; with `pthread_kill`, `time.sleep` broke at
  once and `subprocess.run(["sleep", "3"])` 0.25 s later (`Popen.wait`'s own grace). That is what
  lets an explicit Ctrl-C end a synchronous `subprocess.run`, which `run` then answers by killing
  its direct child. Mutation-checked: with `raise_signal` the grandchild test hangs.

- **The handler raises only into agent code.** No lock, no I/O. All three must hold:
  - an interrupt is armed and its execution still holds the slot, so an unarmed SIGINT --
    `pkill -INT python3` from agent code -- is ignored;
  - walking out from the interrupted frame, a `<execution>` frame comes before the loop's
    dispatch frames (`_run_once`, `Handle._run`) or the fork hooks. Every callback the loop runs,
    the interpreter's own included, runs beneath `Handle._run`, so those two cover them all. This
    settles `0003-02`'s two notes: no torn protocol line, no lost callback. The interpreter's
    output routing is deliberately not a barrier, since a printing runaway spends its time there.
  - a scope: the armed execution's own code (by the `_CURRENT` context variable, which the
    handler sees), or any agent code when the execution has not started, or when the host sent it
    as a `runaway`. The review found that without the scope a user's interrupt for a suspended
    execution would kill another execution's healthy task during a 2 s synchronous stretch.
  - Agent code that pumps the loop by hand puts the loop's machinery before its own frames, so an
    interrupt declines there. Such an execution is unrecoverable by signal; that is recorded
    rather than worked around.

- **A cancel goes through the loop, and a cancel before the first step is kept, not delivered.**
  A task cancelled before its first step throws into a coroutine that never entered its `try`,
  and so never replies. `_cancel` instead sets `cancel_requested`, and `_run` checks it first and
  reports `CancelledError: stopped before it started` without running anything. This replaced the
  plan's re-queue, which the review showed could spin and relied on FIFO ordering. `_start` builds
  the execution's task with `asyncio.Task` rather than `create_task`, so an eager task factory
  the agent sets cannot run the body inside `_start`.

- **The wrapper names `CancelledError`, and keeps its catch-all.** An exception that left `_run`
  would end the task with no reply while the host held a slot the interpreter had freed. On the
  port no `KeyboardInterrupt` that reaches it should end the interpreter: the handler raises only
  into agent code, so one arriving there came from the execution's own body or from awaiting a
  task an interrupt ended. The deliverable's worry about swallowing one does not arise.
  - Formatting the traceback runs agent code, so it is guarded against `BaseException` with a
    fallback line. The review said `traceback` guards `__str__` only against `Exception`; on the
    payload it uses a bare `except`, but its read of `__notes__` does catch only `Exception`, and
    that is what the test raises through.
  - The arm is cleared on the way into the failure path, so a stale interrupt cannot land in the
    formatting.

- **An interrupt that ends a background task is billed to the execution that started it, once.**
  `serve()` recognizes the exception its handler raised and writes a header and the traceback
  through that execution's own `write`: into its result if it is still running, its backlog if
  not. It retrieves the task's exception so asyncio does not report the death again when the task
  is collected. The first cut kept the task alive in a local in `serve`'s frame, so it was never
  collected at all; the mutation test on the retrieval found it.

- **The interpreter's `podman exec` client runs in a process group of its own.** A latent `0003-05`
  bug, found here and measured: SIGINT kills a `podman exec -i` client (exit 1), and the process
  it started then sees EOF on stdin, so a terminal's Ctrl-C -- sent to the whole foreground group,
  prompt included -- ended the session's Python. `0003-05`'s e2e sent `kill -INT <pid>`, never a
  group signal. The e2e now leads its own group and sends `kill -INT -- -<pgid>`; with the fix
  reverted it fails with "the Python interpreter is gone: its process exited (exit status: 1)".
  - The fix is crate-private: a `Cmd::in_own_process_group` flag `spawn_owned` turns into
    `process_group(0)`, and `Container::exec_stdio_in_own_group`. The public `exec_stdio` and
    every legacy path are unchanged.
  - When outrig dies, stdin EOF still ends the interpreter. The exception is a GIL-starved reader
    thread, whose client then outlives outrig in its own group.

- **Presses are counted against the execution they were made during.** `recovery::Presses` holds
  the one execution a call waits on and its count, under a mutex, with a `Notify` only to wake. A
  press racing an outcome counts against that execution and never reaches the next, and the tool
  marks the execution waited on before calling the submission observer, which is what a person
  presses at. A `watch` channel, the plan's first idea, could carry an unseen press across
  executions.
  - What a press does is decided in `recovery` (`Press::Stop` or `Press::GiveUp`), beside
    `settle`, which does it. The tool only phrases it.
  - The turn stops from what `settle` reports (`Stop::User`), not at the press, so a press that
    lost the race to the outcome skips nothing.

- **A refusal behind an abandoned execution checks it.** With no call waiting there is otherwise no
  moment to look at an `Unresolved` holder again. The check interrupts a holder it finds spinning
  and tells the model what it found, and never cancels. A user-facing stop for such a holder is
  `plan/next/stop-a-held-slot-outside-any-call.md`.

- **What the model reads follows the outcome, not the remedy.** "The user interrupted this call"
  and the real exception line, with a note that stopping Python does not stop the processes or
  threads it started; a runaway interrupt that ended someone else's task says so beside this
  code's clean run. Unresolved outcomes say why. The tool description gains the runaway rule and
  `asyncio.to_thread`. None of it names a key: `interrupter` is public, and a front end other than
  a terminal may drive it.

- **Where it lives.** The policy is `python/recovery.rs` (`settle`, `Timings`, `Presses`); the
  host gains `cancel`, `interrupt`, `cpu`, and `abandoned`; the tool, hook, and `PythonAgent`
  share an `Interrupts` handle. The fake transport moved from `host_tests.rs` to `testing.rs` so
  `recovery_tests.rs` can use it.

- **The public surface grows by one line**, `PythonAgent::interrupter`, returning
  `impl Fn() -> Option<String>`. It is a closure rather than a type so the surface stays under
  `PythonAgent`, as `0003-05`'s additions did. Regenerating the snapshot needed the pinned nightly,
  installed with the script's `--install-missing`.

- **The CLI.** `run-new` calls `Repl::run_with` with its own interrupt source, so `repl.rs` is not
  edited. One SIGINT stream serves the whole session; a fresh `ctrl_c()` per wait would miss a press
  landing between two, and a quick second press is the one that stops waiting.

- **A probe the loop has not answered is not sent again.** The next check waits on it, so a loop
  blocked for an hour in a healthy `subprocess.run` does not come back to a hundred queued
  inventories.

- **What `/simplify` changed, and left.**
  - Changed:
    - `inventory` and `cpu` share one query path and one pending map;
    - `cancel` and `interrupt` return nothing, since nothing read their `bool`;
    - the interpreter routes by a handler table;
    - `_landed` is cleared when an execution finishes, so a caught interrupt does not keep its
      frames alive;
    - the test helpers were reused rather than repeated.
  - Left for later, and filed:
    - the watch belongs to the slot, not the call, which would also make a refusal behind an
      abandoned holder instant (`plan/next/recovery-watches-the-slot-not-the-call.md`);
    - legacy `run`'s primary-placed MCP servers should end on a Ctrl-C the same way the
      interpreter did (`plan/next/legacy-mcp-servers-die-on-ctrl-c.md`), and `process.rs`'s
      module docs now say which children take the terminal's signal.
  - Left as is: `run-new` rebuilds `Repl::run`'s stdio to pass its own interrupt source, the
    price of not editing `repl.rs`.

- **Every shape's outcome** is in `runtime-protection.md`'s "What the port does", along with the
  measurements. Native code holding the GIL is the named session failure. `os.system` discards an
  interrupt because musl's `system()` ignores SIGINT while it waits;
  `plan/next/os-system-discards-interrupts.md` proposes routing it through `subprocess`.

- **Tests.**
  - `interpreter_tests.rs`, raw NDJSON on the payload:
    - a wedge interrupted 10 times in a row;
    - an unarmed SIGINT, idle and mid-wedge, changing nothing;
    - a suspended await cancelled;
    - an interrupt declining on a suspended execution, with no "escaped" line;
    - stale requests leaving the next execution alone;
    - a cancel before the first step;
    - a caught cancel holding the slot;
    - a background wedge, alone and beside a suspended execution, billed once and to the right id;
    - the CPU clock telling spin from `waitpid`;
    - a blocking `subprocess.run` interrupted with its grandchild still alive;
    - a hand-pumped loop declining;
    - an unformattable exception still reported;
    - a sub-agent refused.
  - `recovery_tests.rs`:
    - a fake transport with time paused, asserting exactly what is and is not sent for each
      verdict, the three-attempt give-up, the starved wait, a press racing an outcome, and the
      holder check;
    - the real interpreter with short timings: a wedge recovered unasked, a blocking `run` left to
      finish, a background wedge ended while the execution it stalled finishes, a press ending a
      bare await.
  - `agent_tests.rs`: through `interrupter` and a scripted model, a bare await stopped and read,
    `None` at rest, a later call in the turn skipped and not run, a blocking `run`'s caveat, two
    presses giving up, and the stop texts.
  - `process_tests.rs`: the process-group flag.
  - `run_new_e2e.rs`: the group Ctrl-C at the prompt and mid-round, above.
  - **Mutation-checked**, each failing at least one test:
    - `raise_signal` instead of `pthread_kill`;
    - no loop barrier;
    - no start deferral;
    - no scope;
    - no arm gate;
    - no formatting guard;
    - no retrieval;
    - a 0.5 CPU line;
    - signalling a turning loop on a press;
    - giving up after one interrupt;
    - no holder check;
    - a fresh probe on every check;
    - the client back in outrig's group.

- **After review, five fixes.** A review of the landed commit rejected it on four findings and
  reported one more as a pre-existing failure. Each was confirmed against the code, has a test that
  fails without its fix, and is mutation-checked.
  - **A user's stop was overwritten by a later runaway interrupt.** `settle` kept one `Stop`, so
    code that survived the user's cancel and interrupt and was then interrupted automatically read
    as a runaway alone: the turn's later calls ran, and a give-up afterwards could not say which
    stopped waiting. `Stop` became `Waited`, which records the user's stop, a runaway interrupt,
    why the host gave up, and a refused submission's holder check, each on its own.
  - **A probe answered before a check was taken as that check's evidence.** A probe kept from an
    earlier check could have been answered while the loop briefly turned between two blocking
    calls. A user's check then read the loop as turning and sent no interrupt, and the cancel sat
    queued behind the second call. A probe already answered when a check begins is now dropped and
    a fresh one sent; only a genuinely unanswered one is waited on again.
  - **A press during a check waited for the check.** The second press of a double tap waited out
    the first's probe and CPU readings -- five seconds, measured, against a one-second last look.
    Presses are now raced against every check, the host's own included, and act at once; the
    check's probe stays pending to be waited on again.
  - **A clean run after a runaway interrupt claimed the interrupt hit someone else.** Code can
    catch the interrupt and finish. The status now says what the host did and that the call ran to
    completion, without naming a target.
  - **A spin in imported code, run as a task, could not be interrupted.** A coroutine a module
    defines has no `<execution>` frame, so the walk reached `Handle._run` and declined, and the
    session wedged. The review called it pre-existing -- the base had no interrupt at all -- but it
    is this task's gate that declined it. A walk that reaches `Handle._run` from inside a task
    other than an execution's own wrapper now counts as agent code, since the interpreter starts no
    other tasks. A plain callback agent code schedules, with no task around it, still declines.
