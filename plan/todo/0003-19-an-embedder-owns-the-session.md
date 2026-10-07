# 0003-19 -- An embedder owns the session, and `run-new` is one

## Context

`PythonAgent` is the one public way into the agent loop, and its doc comment calls it provisional:
it exists because the binary has to reach the loop, "not as an interface to build on", and "will
change shape without notice". That suited the first milestone, when `run-new` was the loop's only
caller. The maintainer has since decided that the loop gets a designed public surface
(`embedding.md`): a Rust program -- the CLI, or an application that runs agents for its own tasks
-- holds a session, drives its rounds, reads its events, and stops it with a report it can act on.
`run-new` is rebuilt on that API and uses nothing else of the loop, which is how the API is proved
in this phase; no test names an embedder, and there is no separate service fixture in 0.3. The API
may change freely on `version/0.3.x` until the 0.3.0 release fixes it in `public-api.txt`, and
CocoClaw integrates as soon as it exists, through a git dependency, so what a service needs that
the CLI does not shows up there (`embedding.md`).

Most of what it needs exists:

- `PythonAgent::start(&Outrig, &Config, agent, model)` resolves a model, starts the interpreter in
  `outrig`'s primary container, and builds the agent. `check` resolves without starting anything,
  which `run-new` calls before it ensures an image.
- `round()` succeeds with an `Option<String>`: `None` when nothing new arrived, otherwise the
  model's text, with `(round ended: <reason>)` appended when the tool-call cap stopped it. A round
  whose final message held only reasoning returns `""`, which a caller cannot tell from a model
  that chose to say nothing. `plan/next/python-agent-textless-round.md` left the fix to "whichever
  task next reshapes `PythonAgent`", and this is that task.
- `user_channel()`, `interrupter()` and `on_submit()` are what `run-new`'s terminal uses.
- A provider key is a `${VAR}` reference, and `ApiKeyRef::resolve` reads it from the process
  environment. Sessions in one process that need different keys cannot share that table, and
  `std::env::set_var` is `unsafe` in edition 2024, which this workspace uses, for the reason
  `embedding.md` gives.
- Nothing stops a session as a whole. `run-new` calls `PythonAgent::shutdown`, which only finishes
  the event log (`0003-13`), and then `Outrig::shutdown`; an execution still running at that
  moment is accounted for nowhere.
- `0003-13` records the session's events into `events.jsonl` through `LineSink`, behind
  `[events] mode = "record"`. Its `emit` encodes an event, then numbers and queues it under one
  lock without waiting, so an event's id is its place in the file; `ready()` slows the agent rather
  than lose events when the disk stalls. There is no in-memory stream, and nothing is public but
  the config block and `PythonAgent::shutdown`, which finishes the log.

rig stays private: `public_api_boundary.rs` already fails if a rig type reaches the snapshot.

## Goal

A Rust program can start a session, drive it, watch it, and stop it with a report through one
public module, and `run-new` uses that module and nothing else of the loop.

## Deliverables

- **A public module** (fork 1) holding the session API. Every type it exposes is OutRig's own.
- **A builder** taking the container (fork 2), the `Config`, the agent and the model, and a
  **secret resolver**: a hook the session calls with each `${VAR}` name its model calls need --
  today the provider's `api-key`, resolved in `agent/resolve.rs` -- which returns the value for
  this session. `run-new` passes one that reads the process environment, which is today's
  behavior. The check `PythonAgent::check` makes is still available before anything starts,
  through the resolver, so `run-new` still fails before it ensures an image. Image build arguments
  and MCP server environments are resolved outside the session and are not the hook's.
- **One call starts a session from a loaded `Config`.** The builder's `start()` -- or a shorthand
  that takes the `Config`, the agent and the model and uses every default -- launches the
  container, starts the interpreter, resolves the model and returns the running `Session`; an
  embedder orders no steps itself. `run-new` starts its session through that call, with its
  prompts and its file writer given as inputs to it, so the CLI, CocoClaw and any other embedder
  start a system the same way (`embedding.md`).
- **`Session`**, keeping `PythonAgent`'s behavior: rounds, the user channel, the interrupter, and
  the model, Python version and container name that `run-new`'s banner prints.
- **A round outcome richer than `String`**: whether a round ran; the reply text; why the round
  ended when a limit stopped it, in place of text appended to the reply; and, for a final message
  that held only reasoning, that reasoning, recovered as `plan/next/python-agent-textless-round.md`
  describes, with `recover_non_text` and `is_blank` copied from `outrig-cli`'s `llm.rs`.
- **A round's closing line names what is still running.** The outcome carries the background
  tasks still running in the agent's kernel when the round ended -- the `asyncio` tasks it left
  under names -- and `run-new` prints them in the round's closing line, so a prompt that returns
  is not presented as done (`embedding.md`). `0003-21` adds hosted requests in flight and `0003-25`
  children with a round running, to the same line.
- **The in-memory stream, public**: `emit` also hands each numbered event to every subscriber
  given to the builder, in id order. A subscriber that falls behind loses events and is told how
  many, and never makes `emit` wait. The `events.jsonl` writer becomes one subscriber, kept behind
  `[events] mode` and keeping `0003-13`'s `ready()` backpressure for itself. `0003-13`'s event types
  become public with it. `PythonAgent::shutdown`, which `0003-13` added to finish the log, is
  replaced by `shutdown(deadline)`, which finishes it the same way and reports its losses in the
  `ShutdownReport`. The
  session states `embedding.md` lists -- starting, idle, round running, executing, closing,
  reported -- are events, and this task adds those `0003-13` does not already emit. Closing lasts
  from `close_admission()` until the report exists, which spans `lifecycle.md`'s closing, draining
  and terminating steps. The interpreter's death is an event, not a state of its own: the session
  then goes through closing to reported.
- **Attempt identity on every model event.** `model.retry`, `model.failover`,
  `model.round.completed` with its per-call list, and every other event that carries usage name a
  **logical call id**, one per model call the loop decided to make, and a **provider attempt id**,
  one per request actually sent, so a call that was retried or failed over has one logical id and
  several attempts. A failed attempt is recorded as an event of its own, with the settings of the
  request it sent -- the candidate, the model, the effective ceiling -- and a usage that is null
  when the provider reported none, never zero. A usage record that arrives after the attempt's
  event, carrying the same attempt id, replaces that attempt's null usage, once; a second record
  for the attempt is refused and evented; spend from a later descendant attempt is a new attempt,
  never a correction. The accepted update and the refused one are told apart in the events --
  distinct kinds, or one kind with a field that says which -- each naming the attempt, and an
  attempt id is unique within its session, so two sessions in one process never update each
  other's attempts. Aggregates are derived from unique attempts, recomputed after a replacement,
  and a parent's inclusive total is never summed with a child's; `0003-25` inherits the rule when
  children arrive. `0003-15` landed the retry and failover events without either id, and this
  task adds them before those events become public with the stream.
- **The history and view types grow without a break.** The turn, the round, the per-call manifest
  and the selection metadata that become public with the event types are additive-extensible
  (fork 6): a later addition -- an active-intent record (`potential/active-intent-record.md`), a
  selection strategy's name -- adds a field or a variant and changes no existing one, as
  `history.md` requires.
- **Admission and shutdown** (`lifecycle.md`):
  - `close_admission()` returns at once, and can be called from another task while a round runs,
    as the interrupter can.
  - `shutdown(deadline) -> ShutdownReport` closes admission if it is open, lets running executions
    finish until the deadline (fork 4), interrupts every kernel, and stops the container, which
    ends the interpreter with it.
  - The report says that admission closed, whether owned execution is proven stopped, an outcome
    for each execution live at the close -- `ok` or `error` if its result arrived, `unknown` if
    not -- the last sequence the stream published, and how many events each subscriber missed.
    `0003-21` adds hosted calls to it and `0003-25` child work, the rest of what `lifecycle.md`'s
    report lists.
  - After the close, a new round returns a documented error, and a submission the model makes is
    refused with it (fork 3 for a round already running). The interpreter's exit closes admission
    too, an execution running at that moment is `unknown`, and `shutdown` still returns the
    report.
- **`/quit` and EOF leave through the report.** Either one calls `close_admission()`, then
  `shutdown(deadline)` with fork 4's deadline, prints the report, and exits with a status the
  report decides: 0 when it reads stopped with every outcome known, 2 when it reads stopped with
  some outcome `unknown`, 3 when it reads not proven stopped (`lifecycle.md`). Neither asks the
  user to choose a cancellation mechanism first.
- **`lifecycle.md`'s rows for executions, the interpreter and the container**, true of the
  implementation and each tested.
- **`run-new` rebuilt on the module.** `events.jsonl` is written by a subscriber `run-new` gives the
  builder, under `0003-13`'s opt-in, with the content and permissions `0003-13`'s tests assert.
- `PythonAgent` and the crate-root `UserChannel` removed.

## Acceptance

- `run-new`'s tests, and the agent tests in `agent/agent_tests.rs`, pass against the new API with
  no assertion weakened.
- The regenerated snapshot names the new module and no `PythonAgent`, which a new test in
  `public_api_boundary.rs` asserts beside the existing check that no rig type is public.
- **A stalled subscriber never delays a round**, and when it reads again it is told how many events
  it missed.
- **Every attempt is recorded once, failed ones included.** A call that is retried once and then
  fails over shows one logical call id across three provider attempt ids: the first two carry
  their request settings and null usage, the third the usage the provider reported.
- **Totals derive from unique attempts.** A test totals a session's usage from its unique
  attempts and from the per-attempt events and gets the same number, and shows that adding a
  round's inclusive total to the per-call entries beneath it counts each attempt twice. The same
  test shape holds a parent's total apart from a child's once `0003-25` adds children.
- **A late usage record replaces a null once, and a replay agrees.** Attempt N's event carries
  null usage; a record of 7 for N arriving afterward replaces it, and the round's total changes
  once, to 7; a second record for N, of 9, is refused and evented, and the total stays 7. The two
  updates are told apart in the events, each naming N. The test then reconstructs the total from
  the event log alone, through the same aggregation the live session uses, deduplicating by
  attempt id so an attempt counted once live is counted once however many events name it: the
  reconstructed total is 7, and a reconstruction that applied the refused 9 would read 9. Attempt
  ids are scoped to the session: a second session in the process with an attempt of the same id
  is not updated by either record, and its reconstruction reads its own total.
- **The session's state is in its events.** A session that runs one round in which the model
  submits one execution, and is then shut down, publishes starting, idle, round running,
  executing, round running, idle, closing and reported, in that order. Closing is published when
  admission closes, and reported once the report exists, with no state between them.
- **The interpreter's death is an event, then a close.** Agent code calls `os._exit(1)` during an
  execution. The stream records the exit as an event, not as a state; the state then becomes
  closing, since admission has closed, and a later `round()` returns the documented error.
  `shutdown` returns a report with that execution `unknown`, and reported follows.
- **Each session uses its own key.** Two sessions in one process, each with a resolver that returns
  a different key: the mock provider, extended to record request headers, sees each session's
  model calls carry its own key.
- A turn whose content is only a `thinking` block ends in an outcome that says so and carries the
  reasoning.
- **`run-new` starts its session with the one call**, its prompts and its writer given as inputs
  to it, and calls nothing else of the loop; a `Config` built in code, with no input replaced,
  starts a session with that call alone.
- **A round ending with a task running says so.** A round whose execution runs
  `asyncio.create_task(asyncio.sleep(60), name="ci_run")` and yields ends with a closing line
  that names `ci_run`; a round that leaves nothing running prints none.
- e2e: **each exit status, through `run-new`.** `/quit` with nothing running exits 0 and prints a
  report that reads stopped with every outcome known; EOF does the same. `/quit` while an
  execution catches every `KeyboardInterrupt` in a loop exits 2, the printed report listing that
  execution `unknown`. A report that reads not proven stopped exits 3, forced at the point where
  `run-new` maps a report to a status, since no test can make podman fail to stop a container on
  demand.
- The regenerated snapshot shows each public history and view type as `#[non_exhaustive]`, or
  constructed only through a builder, per fork 6.
- e2e: `shutdown` while an execution catches every `KeyboardInterrupt` in a loop returns within a
  stated bound after the deadline, reports that execution `unknown`, and leaves the container
  stopped.
- e2e: **the report covers each execution.** `shutdown` with a 10 s deadline, called while an
  execution sleeps 1 s and then returns, reports it `ok`; in a second session, one that sleeps 1 s
  and then raises is reported `error`.
- e2e: **a stalled subscriber does not hold shutdown back.** With a subscriber that takes nothing
  while more events are published than the stream holds, `shutdown` returns within the bound the
  first e2e item states, and the report gives a nonzero count of events that subscriber missed.
- After `close_admission()`, `round()` returns the documented error, and a submission in a round
  already running gets that error as the tool's result, per fork 3.
- `run-new`'s `events.jsonl` holds what `0003-13`'s tests assert, written through the public
  subscription.
- `crates/outrig/public-api.txt` regenerated, and `scripts/check-public-api.py` passes.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **The module's public name -- Open.** `embedding.md` leaves it to this task.
   - `outrig::session` names the value an embedder holds, in the word the docs use for a run.
     `outrig-cli` already has a private `session` module for the on-disk record, so the CLI
     imports one of the two under another name.
   - `outrig::agent` is the private module the loop lives in today, so it is the smallest change.
     The name describes the model loop rather than what owns a container and, later, bindings and
     children.
   - `outrig::embed` names the audience rather than the thing, and reads oddly in the CLI, which is
     its first user.
2. **What the builder takes for the container -- Recommended: the `LaunchSpec`, which the builder
   launches once the model and its secrets have resolved.** The alternative is an `Outrig` already
   launched, which is what `PythonAgent::start` takes. That serves this task, but a running
   container cannot gain a mount, and `0003-20` has to mount each binding's package directory and,
   when any binding exists, the workspace at its host path; with a launched `Outrig`, a binding
   given to the builder arrives after the mounts are fixed. Taking the spec also makes "a secret
   that cannot be resolved fails the start before any container starts" (`embedding.md`) true of
   the start itself rather than of a separate check. Either way `run-new` ensures the image first,
   as it does now; whether the session should do that is `embedding.md`'s open question.
3. **A round still running at the close -- Recommended: it runs on, each submission refused with
   the closing error as its tool result, until the model yields or the owner drops the round.** The
   model reads why its code did not run and can say so in its reply, and the drain's deadline still
   bounds the session. Ending the round at once is simpler: the round keeps what a dropped round
   keeps today, and the model is never told why it ended. `lifecycle.md` leaves this to this task.
4. **The default drain deadline -- Open.** `shutdown` takes the deadline as an argument, so the
   question is the value `run-new` passes and whether the library offers a default. A few seconds
   lets an execution that is finishing report, and keeps exiting prompt. Thirty seconds, RPyC's
   default request timeout, lets a slow hosted call (`0003-21`) finish rather than end `unknown`,
   at the cost of a session that can take that long to exit.
5. **Whether `on_submit` survives -- Recommended: events replace it.** `embedding.md` notes that
   the `exec.submitted` event carries what `on_submit` shows, so `run-new` prints each submission
   from its subscription, and there is one mechanism. `on_submit` runs inside the tool, so a
   terminal that stops reading delays the round. A subscriber that stops reading loses events
   instead and is told how many, so the terminal can say that a submission was not shown.
6. **How the history and view types stay additive -- Recommended: `#[non_exhaustive]` on each
   struct and enum.** An embedder reads them and constructs none, so with the attribute a field
   or a variant can be added without a break, at the cost of no struct literal and a wildcard arm
   in every match outside the crate. A builder with getters is the alternative, and becomes
   necessary for any such type an embedder constructs -- a selection strategy's settings would be
   one (`potential/history-selection-seam.md`) -- which nothing in 0.3 exposes.

## Dependencies

- **Hard: 0003-13.** Its event types and its `events.jsonl` writer, in front of which this task
  puts the in-memory stream it makes public.
- **Hard: 0003-15.** Its retry and failover events, which this task gives a logical call id and a
  provider attempt id. It also settles which model answered a round and how an exhausted failover
  chain ends one, both of which the round outcome reports; designing the outcome after it avoids
  reshaping it.

## See also

- `plan/phase/0003-python/embedding.md` -- what the embedder supplies and drives, and what stays
  its own.
- `plan/phase/0003-python/lifecycle.md` -- the close sequence, the close table and the report.
- `plan/phase/0003-python/observability.md` -- the stream, its gaps, the opt-in, and the two ids
  every model event carries.
- `plan/phase/0003-python/history.md` -- the requirement that the history and view types take
  additions without a break, which fork 6 meets.
- `crates/outrig/src/agent/mod.rs` -- `PythonAgent`, which this replaces.
- `crates/outrig-cli/src/cli/run_new.rs` and `crates/outrig-cli/src/cli/run_new/converse.rs` --
  the terminal rebuilt on the new module.
- `crates/outrig/tests/public_api_boundary.rs` -- the snapshot rules the new test joins.
- #471 -- the terminal setting a round's pace, which fork 5 bears on.
