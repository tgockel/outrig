# 0003-28 -- A session runs with no primary agent

## Context

`harness::Session` holds an `Agent` that is not optional (`crates/outrig/src/harness/session.rs`):
the rig agent over its failover model, the `History`, the `Budget`, the `Interrupts`, the
`UserChannel` and the `Announcer`. `agent=None` on the builder means an agentless profile -- no
preamble, the model from the override or `default-model` -- and start still resolves that model,
calls the secret resolver for its key, builds the agent in `assemble` and opens one kernel the
protocol names `primary`. `Relay::opened` for any other agent is dead code until children arrive.

A host program's session (R2) has no primary agent: no model-driven rounds, no user channel, no
primary preamble. The program constructs the agents it wants (`0003-35`) and owns its own control
flow. What such a session still has is every resource: the container, the interpreter, the
bindings and their relay, admission, the event stream, the ledger and, from `0003-30`, the child
registry. R2 also rules out the shortcut: no invisible dummy primary made to satisfy the API.

So this task separates the two. Session resources become the session's own, reachable without an
`Agent`; the primary agent becomes optional state a session may or may not have. `run-new` keeps
its primary and sees no change; the program's session has none. `0003-30` then builds the child
runner on the session rather than on the primary, which is what lets `0003-35` launch a child
whose parent is the owner.

## Goal

A session can start, run its bindings, admit work from agents created later, and stop with a
report while having no primary agent: no model resolved at start, no user channel, no rounds.

## Deliverables

- **The builder** gains a way to ask for no primary, spelled per fork 1. Such a session resolves no
  model and asks the secret resolver for nothing at start; `check()` still resolves a named model
  when the caller gives one. `run-new`, `Session::start(config, agent, model)` and every session
  with a primary are unchanged.
- **The interpreter starts with no kernel** (fork 2): `Interpreter::start` takes an optional
  primary, `_open` creates every kernel, and the manifest `bindings` line, the relay --
  `Relay::opened` becomes live for any agent id -- and the exit hook assume no `primary`.
- **Session-owned state is reachable without an `Agent`**: the admission gate, the event stream,
  the `Ledger`, the interpreter, the relay and the place `0003-30` fills with the child registry
  hang off the session, so `0003-30` builds its runner on the session and `0003-35` can launch a
  root child from it.
- **Primary-only operations** -- `round()`, `user_channel()`, `interrupter()`, `stop_held()`,
  `model()` and `python_version()` -- answer per fork 1 on a session with no primary.
- **States and the report.** A no-primary session publishes `starting`, `idle`, `closing` and
  `reported`; `round running` and `executing` occur only with a primary, and what a child's
  activity does to the session's state is `0003-30`'s to decide. The report lists no executions
  and everything else as today. `embedding.md`'s state list says so.
- **Model validation moves to the launch** of whatever first needs a model (fork 3): a session
  that never launches an agent resolves nothing, and a root child's launch resolves its own model
  and fails that launch, not the session.

## Acceptance

- A no-primary session publishes `session.state` `starting`, `idle`, `closing` and `reported`, in
  that order; the mock provider receives no request; the resolver is never called; `round()`
  returns the documented error; there is no user channel.
- A `Config` with neither `default-model` nor a named model starts a no-primary session and stops
  clean; `check()` on the same builder with an explicit unknown model fails before anything
  starts.
- **The relay serves a kernel that is not the primary.** A no-primary session with `0003-16`'s
  fixture binding: a kernel the test opens over the protocol reaches the binding through the
  production relay, a request's events share its id, and the report shows the bindings group
  empty; a second kernel works beside it.
- `shutdown` reports `Proven` with no executions; `agent.started` and `agent.stopped` are not
  published; `session.report` is.
- Every `run-new` test and `agent_tests.rs` pass unchanged.
- `crates/outrig/public-api.txt` regenerated: the builder method and the no-primary error are its
  additions; `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **The API's shape -- Recommended: one `Session` type, with the primary-only methods returning
   `SessionError::NoPrimary`, or `None` where the return is a handle.** A second type would split
   `close_admission`, `shutdown` and the report across two, and an embedder with a primary sees
   nothing change.
2. **The interpreter with zero kernels -- Recommended: yes; every kernel is opened by `_open`.** A
   hidden primary kernel is what R2 rules out. What is left to choose is whether
   `Interpreter::start` keeps a `primary` parameter as an `Option`, or `assemble` opens the
   primary as it would any kernel.
3. **Whether `default-model` is validated at start regardless -- Recommended: no.** R2 asks that
   no unnecessary primary model be resolved; a root child's launch resolves its own and fails that
   launch, not the session.

## Dependencies

- **Hard: 0003-19.** The builder, the session, the states and the report this task reshapes.
- **Hard: 0003-20.** The bindings a no-primary session still starts, declares and stops.
- **Hard: 0003-22.** The relay, whose `opened` this task makes live for any agent.

## See also

- `plan/phase/0003-python/embedding.md` -- "Sessions without a primary agent".
- `plan/phase/0003-python/lifecycle.md` -- the report with no executions.
- `crates/outrig/src/harness/session.rs`, `builder.rs` -- `Session`, `assemble`, `start`.
- `crates/outrig/src/python/host.rs`, `hosted.rs` -- `Interpreter::start`, `Relay::opened`.
