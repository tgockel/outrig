# 0003-03 -- The host starts the interpreter and correlates its replies

## Context

The interpreter answers on NDJSON over `podman exec -i`. The host half owns the child, correlates
replies to requests, and turns the protocol into something the rest of `outrig` can call.

`execution-and-rounds.md` settles the contract this task implements, and two parts of it are easy
to get subtly wrong. An execution has **three** outcomes, not two: `ok`, `error`, and `unknown`
for a result the host never received -- and `unknown` covers two situations that must not be
collapsed, since a confirmed interpreter exit ends the execution while an unanswered one may
still be running and its slot is not free. Nothing is ever rolled back, so a lost result is never
retried automatically: re-running a submission whose effects are unknown is how one `git push`
becomes two.

The host-side `Child` is the `podman exec` client, not the interpreter. Killing it does not stop
the interpreter, which runs under conmon and outlives its client.

## Goal

A handle on a session's interpreter that starts it, submits executions, reads inventories, and
reports outcomes honestly -- including the outcome that means "I do not know".

## Deliverables

- The host module in `outrig`: start the interpreter through `Container::exec_stdio`, wait for its
  `ready` greeting before returning, and spawn a driver task that owns the child and correlates
  replies by request id. Cheap to clone, so more than one caller can hold it.
- **Waiting for `ready` at startup**, so "the payload does not run in this image" is a startup
  error rather than a hang on the first tool call.
- Submission, inventory, and the outcome type carrying `ok` / `error` / `unknown`.
- **An unresolved state, holding the execution id.** An unanswered execution keeps its slot, and a
  late reply resolves the uncertainty as a new observation rather than by rewriting the old one. A
  late reply must not be attributed to whatever execution is running by then.
- **The execution's task handle is retained**, not discarded. `runtime-protection.md` records that
  the prototype starts executions with `asyncio.ensure_future(...)` and throws the handle away, so
  there is nothing to cancel -- and cancellation is the only remedy that works for an execution
  suspended on an await.
- The child's stderr drained to tracing, so an interpreter that dies on startup says why.

## Acceptance

- Against the real payload: starting, submitting `1 + 1`, and reading the echoed value back.
- A submission that raises comes back as `error` with the traceback, and the interpreter is still
  usable afterwards -- the point of the outcome split is that a raising submission is not a
  transport failure.
- **An execution whose reply is suppressed lands as `unknown` with its id retained**, a second
  submission does not silently take its slot, and a late reply for the original id is correlated
  to it rather than to the newer one. Tested with a fake transport, since suppressing a reply is
  not something the real interpreter offers.
- Starting against an image with no payload produces a startup error naming the cause, not a hang.
- `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whether the outcome enum is public -- Recommended: no.** The phase budgets one public entry
   point and this is not it. The outcome crosses into `outrig-cli` only through whatever that
   entry point returns.

## Dependencies

- **Hard: 0003-02.** There is no protocol to drive until the interpreter speaks one.

## See also

- `plan/phase/0003-python/execution-and-rounds.md` -- outcomes, the single slot, no rollback, and
  why the task handle is kept.
- `crates/outrig/src/container/mod.rs:954` -- `exec_stdio`, and the note that killing the client
  does not stop the command.
- The prototype branch's `crates/outrig-cli/src/python/kernel.rs` -- the driver and correlation to
  port, including the liveness probe that `0003-06` builds on.

## Decisions

- **The host half is `python/host.rs`**, beside `interpreter.py`. Everything in it is
  `pub(crate)`, so the public API, `public-api.txt`, and the CHANGELOG are unchanged. Design fork
  1 went as recommended.
  - `ARGS` (the program, run with `-I`) and `PRIMARY` live in `host.rs`. `interpreter_tests.rs`
    and `host_tests.rs` start the interpreter with them, so the tests run the same command `start`
    runs. `interpreter_tests.rs` no longer has its own `include_str!`.
  - `payload::mount` is now the one place the payload's bind is built. Both the session launch
    and the e2e tests use it.
  - Nothing outside the tests calls the module yet. `mod host` carries `expect(dead_code)`
    except in e2e test builds, where `start` is reached. Any one dead item fulfills the
    expectation, so it holds until the last item has a caller. At that point it fails the build
    rather than lingering as an `allow` would.

- **`unknown` is two variants, not one with a flag.**
  - `Unknown::Exited { id, cause }`: the interpreter's output closed before the reply. No reply
    can come, and the handle is spent; every later call returns `Gone` with the same cause.
  - `Unknown::Unresolved { id }`: the caller stopped waiting, explicitly through
    `Execution::stop_waiting`. The execution may still be running.
  - Nothing retries either one.
  - The host has no deadline of its own for an execution, because a round has no fixed
    duration. What decides that waiting has gone on long enough is the liveness probe, and that
    is `0003-06`'s. Until then, `Unresolved` is reached only by a caller that chooses it.

- **The host gates the slot itself.** While an execution is outstanding, answered or not, a
  submission is refused locally and nothing is written. The refusal is
  `Outcome::Refused { holder }`.
  - This is what makes "a second submission does not silently take its slot" hold when the
    interpreter cannot answer. It is also why no late reply can land on a newer execution: there
    is never more than one outstanding execution to correlate against.
  - Against a live interpreter the two gates agree. `interpreter.py` releases its slot before it
    sends the result, so by the time the host sees a result, the interpreter would admit the
    next submission.
  - A `refused` from the wire therefore means the two disagree. It is logged and still reported
    as the refusal it is.

- **`Refused` is a variant of `Outcome`**, although nothing ran. It arrives on the same wire
  message as `ok` and `error`, and the caller renders all four at one match site. A refused
  submission still consumes an id: ids name submissions.
  - A locally refused submission's id never reaches the wire, so an interrupt aimed at it finds
    nothing.
  - The review proposed returning the refusal from `submit` instead. That was not taken: the
    wire's own `refused` would still need an outcome, and the answer would then have two match
    sites.

- **The host drives the primary alone.** Every reply is routed on its `agent` once, and a reply
  for any other agent is logged and dropped. The table holds a single slot. Opening a second
  agent belongs to `work.md`, which is unqueued; when it lands, the table becomes one slot per
  agent. The interpreter is already multi-agent-shaped (`0003-02`), so no protocol message
  changes.

- **A late reply is a `Late` record, taken with `Interpreter::take_late`.** It never rewrites
  the `Unresolved` the caller was already given. It is also where a reply lands when its
  `Execution` was dropped unread, so a round cancelled just as its result arrived does not lose
  the result.
  - The race between giving up and the reply arriving is closed with oneshot `close` then
    `try_recv`. A reply lands in the receiver or in `late`, never in a receiver about to be
    dropped.
  - Where a late result is shown to the model is `0003-04`'s to decide. This task only
    guarantees that one is never lost or misattributed.

- **Reader and writer are separate tasks.**
  - `0003-02` recorded that the interpreter writes refusals and opened agents' `ready`s from
    its reader thread. A host that stops reading while it writes can therefore deadlock both
    sides on full pipes, and the prototype's single `select!` loop did exactly that: it awaited
    `write_all` inside a branch.
  - Callers enqueue whole lines on an unbounded channel. That keeps `submit` synchronous, and
    means a cancelled caller cannot tear a line.
  - When every handle drops, the writer closes stdin, and the interpreter exits through the EOF
    path `0003-02` added.
  - A failed write is recorded as the cause every later call reports. So is the reader's account
    of the exit, which replaces it once it arrives.
  - A failed startup closes stdin before it waits for the exit. A live interpreter then exits
    at once instead of waiting out the grace period.

- **Startup waits for the primary's `ready`, bounded at 30 s.** A missing payload is not what
  the bound is for: podman fails at once. The failure message is the problem, then the process's
  exit status, then the last 20 stderr lines. Measured against rootless podman and runc:

  ```text
  the Python interpreter did not start: its output closed before it reported ready; its process
  exited (exit status: 127); stderr:
  Error: runc: exec failed: unable to start container process: exec: "/outrig/python/bin/python3":
  stat /outrig/python/bin/python3: no such file or directory: OCI runtime attempted to invoke a
  command that was not found
  ```

- **stderr is split by level.** The interpreter's own `outrig-interpreter:` lines go to `warn`.
  Everything else goes to `debug`: `os.write(1, ...)`, `os.system`, and anything else no
  execution can be billed for. An agent's stray output would otherwise print on the user's
  terminal at the CLI's default `info`. The same lines feed the 20-line tail that explains an
  exit.

- **Every line the host reads is bounded**, which settles `0003-02`'s note that the stderr drain
  now needs a bound.
  - A reply line is capped at 16 MiB and a longer one is dropped. The interpreter's largest
    legitimate reply is an inventory, under 5 MiB escaped, so an overlong line can only be
    executed code writing to the protocol descriptor.
  - A stderr line is capped at 4 KiB and cut.
  - `lines()` would have buffered a newline-free flood whole.

- **For later tasks.**
  - `0003-04`:
    - Render all four outcomes. `Unknown` must never read as a failure worth retrying.
    - Decide where `take_late` records surface. The next tool result is the obvious place,
      since the interpreter's own `background` already travels there.
    - Convert `InterpreterError` at the public entry point.
  - `0003-06`:
    - The probe is `Interpreter::inventory` under a timeout, since it is answered on the agent's
      loop.
    - Giving up is `Execution::stop_waiting`.
    - An interrupt or cancel targets `Execution::id`, and it goes out on the same writer queue.

- **The error is crate-private** (`InterpreterError`: `Launch`, `Startup`, `Gone`). A new
  `OutrigError` variant would have grown the frozen public enum for a module nothing public
  reaches, and `0003-04` converts at its boundary anyway.

- **The execution's task handle was already retained by `0003-02`.** Its slot holds
  `_Execution.task`, and nothing in Python changed here.

- **Tests.**
  - Against the payload run on the host, as `interpreter_tests.rs` does:
    - `1 + 1`;
    - a raise followed by a clean run;
    - a flood whose `dropped` is exact, and a task started by one execution whose output arrives
      as another's `background`, billed to the first;
    - an inventory;
    - an `os._exit(3)` that comes back `Exited` naming status 3;
    - a start with no agent id, which fails naming the interpreter's own `usage:` line.
  - Against a fake transport over `tokio::io::duplex`, since the real interpreter withholds no
    reply on request: the acceptance sequence for `unknown`, a closed transport, the `late`
    paths, the wire's three statuses, lines the host cannot place, and both startup failures.
    Where a wait must give up, time is paused. Where the reader must have caught up, an
    inventory round-trip orders it.
  - Through podman, behind `e2e`: alpine with the payload answers `1 + 1`, and alpine without
    it fails at startup naming podman's error. Both were run locally against rootless podman.
    The sandbox this was written in mounts `/run/user/1000/libpod` read-only, so they ran
    outside it.
  - **Mutation-checked:** settling a result regardless of its id, and removing the host gate.
    Each fails the acceptance test for `unknown`. The second first hung the test instead of
    failing it: an unguarded `outcome().await` under paused time. Every await in the tests is
    now bounded.
