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
- **The interpreter starts with no kernel** (fork 2): `_open` creates every kernel, and the
  session opens the primary's as it builds the primary agent, once its bindings are attached
  (revised 2026-10-19, from `/simplify`: `Interpreter::start` takes no primary). The manifest
  `bindings` line, the relay -- `Relay::opened` becomes live for any agent id -- and the exit hook
  assume no `primary`.
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
- Every `run-new` test passes unchanged. `agent_tests.rs` passes with only mechanical edits: a
  handle fork 1 makes optional is unwrapped, and the primary is reached through a test accessor
  rather than the field. No assertion changes.
- `crates/outrig/public-api.txt` regenerated: the builder method and the no-primary error are its
  additions, and `user_channel`, `interrupter` and `model` return `Option`;
  `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

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

## Decisions

The three forks were taken as recommended. The builder's spelling, and the shape of a primary that
`_open` builds like any kernel, are recorded here because no doc settled them.

- **The builder asks with `SessionBuilder::no_primary()`.** It is a method on the builder `new`
  returns, not a second constructor, because the acceptance has `check()` resolve a model the
  builder names, so a no-primary builder still takes `agent` and `model`. `agent` still chooses
  the default container's image, and the method's doc says so. One `resolution()` serves `start`
  and the test-only `start_on_host`, so neither resolves where the other does not. `check()`
  resolves when there is a primary or a named model.
- **An agent the builder names must still exist.** A review of the first cut found the no-primary
  path skipped the `UnknownAgent` check that `resolve_agent` makes first. `default_container`
  then read a misspelled agent as none and launched `default-image`, so the session ran in the
  wrong environment rather than failing. The lookup is now `resolve::find_agent`, which
  `resolve_agent` calls, and a no-primary builder makes it in `check()` and at the start before
  anything starts. It resolves no model and asks for no key. No agent, or one that names no
  image, still runs in `default-image`. `/simplify` preferred this shape, one function and a
  re-export, over a first version's three.
- **Fork 1, method by method.**
  - `round()` returns `SessionError::NoPrimary`, checked before `Closed`. Having no primary is a
    property of the session rather than of its state, so a closed no-primary session still
    says which.
  - `user_channel()` and `interrupter()` are handles, and return `Option`.
  - `model()` returns `Option<&str>`. It is a getter, and an error would make every caller with
    a primary handle one that cannot happen.
  - `stop_held()` keeps its signature, since its `None` already means nothing is held.
  - `python_version()` turned out not to be primary-only. The interpreter greets with its version
    before any kernel exists, so it answers in every session and keeps `&str`.
  - `container_name()` moved onto the session.
  - `NoPrimary` is a unit variant without `#[non_exhaustive]`, so `run-new` can name it where it
    unwraps the handles.
- **The public API changed beyond its two additions, and `agent_tests.rs` changed with it.** Fork
  1's `Option` returns change three signatures. `agent_tests.rs` called them at 12 lines, and the
  builder's and the README's doctests called `user_channel()` too, so "passes unchanged" could not
  hold. Keeping the old signatures would have needed a stand-in channel and model name, which R2
  rules out. The maintainer chose (2026-10-19) a plain `Option<Agent>` with test-only `primary()`
  and `primary_mut()` accessors, in `interpreter()`'s style, over a newtype that would deref to
  `Agent` in tests only to keep the old field paths. The file changed by 39 mechanical lines,
  and no assertion changed. The acceptance was reworded to say so.
- **The interpreter starts with no kernel, and the session opens the primary's (fork 2).**
  - `Interpreter::start` starts the interpreter and nothing in it. `Session::build_primary` opens
    the primary's kernel through `Interpreter::open`, awaited, and builds the agent over it.
  - That is the second of fork 2's two placements. The deliverable spelled the first,
    `Interpreter::start` taking a primary, and this task built it that way first. `/simplify`'s
    review showed that opening the primary before the bindings connect had cost a list of open
    agents and a loop in `attach_relay` to tell the relay of them, plus a parameter through
    three functions, about 20 lines with no behavior of their own. The maintainer chose the
    second (2026-10-19), and the deliverable was revised to match.
  - The program's argv is `<outrig-dir> [<rpyc-dir>]`. It greets with a `ready` that names no
    agent, which supersedes `0003-02`'s rule that the primary's `ready` doubles as the greeting.
  - `_open` builds every kernel, on the reader thread.
- **The primary is served on the main thread, and its `open` asks for that.**
  - Python runs a signal handler on the main thread alone. A primary on a thread of its own, like
    any other `_open` kernel, would lose the interrupt path `runtime-protection.md` rests on.
  - `open` carries `"main": true` for the primary; the host sends it exactly when the agent is
    `PRIMARY`. `_open` hands that kernel to the main thread through a `queue.SimpleQueue`, which
    `main` waits on once the reader has started. That wait stays interruptible, so a signal
    still reaches the handler.
  - With no primary, `_on_sigint` returns at once. Without that guard, the `AttributeError` it
    would raise there ends `main`, and with it the process.
  - A second `main` open is refused before anything is built.
- **A failed `open` is answered,** with `{"t": "ready", "agent": A, "error": E}`, as `msg`
  answers a refusal, beside the diagnostic.
  - Without the answer, a primary that cannot be built would cost the whole 30-second
    `READY_TIMEOUT` and say nothing of why. Before this task such a failure ended `main` and
    failed the greeting at once.
  - `Interpreter::open` fails as `InterpreterError::Open { agent, reason }` rather than
    `Startup`, since the interpreter did start. A session whose primary does not open fails its
    start with that.
  - `/simplify`'s independent alternative opened a kernel without waiting for its `ready`. It
    was smaller, but a primary that could not be built would have let the start succeed and the
    first round fail later, unanswered and without the cause, so it was not taken.
  - A `MemoryError` is left to `_with_room`'s retry and goes unanswered, since the retry may yet
    open the kernel.
  - `messages_the_interpreter_cannot_route_are_ignored` gave its three bad opens to a test of
    these answers.
- **`observe` is process-wide.** The host asks once, before any kernel exists. Every kernel, the
  primary and any opened later, then says when its code takes a message, where a per-kernel
  `observe` would have had to follow each open.
- **The relay knows no agent until it is told of one.**
  - `Relay::new` no longer seeds the primary, and `Relay::opened` is live production code.
  - `Interpreter::open` tells the relay attached by then of each kernel it opens. The session
    opens every kernel, the primary's included, once its bindings have connected, so no kernel
    is opened before its session's relay exists.
- **The session owns its resources, and the primary is built over them.**
  - `Session::new` takes the interpreter, the container and its name, the lifecycle and the
    bindings. It attaches the gate and the exit hook, which were `assemble`'s, and makes the
    session's `Ledger`.
  - `build_primary` opens the primary's kernel and builds the agent from them, which is the
    ledger's production reader.
  - There is no placeholder field: `0003-30` adds its registry beside the ledger.
  - A primary that does not open, or fails to build, abandons the session as a failed start did
    before: the bindings stop, then the container.
  - `Agent` lost its `python_version` and `container_name`, which nothing read any more.
- **Fork 3: a no-primary session resolves nothing, and keeps nothing to resolve later.** The
  config and resolver a root child's launch needs are `0003-30`'s and `0003-35`'s to carry.
  Nothing launches here, so holding them would be fields nothing reads.
- **The host still drives no kernel's work but the primary's.**
  - A non-primary kernel's replies other than `ready`, `rpc` and hosted notices are dropped, with
    a warning.
  - Its `user` sends go unacknowledged, so one would stall after the send window. Children have
    no user channel in this phase, and per-kernel routing is `0003-30`'s.
  - The relay test speaks for its kernels through a test-only `Interpreter::send_line`. It counts
    `call` requests alone, because `fx.thread_name()` also makes a `getattr` first.
