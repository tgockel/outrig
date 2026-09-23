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
