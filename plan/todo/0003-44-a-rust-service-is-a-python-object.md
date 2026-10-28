# 0003-44 -- A Rust service is a Python object

## Context

Phase 0003 presents Python objects to agent Python and nothing else: a factory binding's library
runs in the bindings process behind RPyC (`hosted-objects.md`), a live object in a host program
(`0003-36`). For a Rust service it decided a route without a trait. `embedding.md`, "Services
written in Rust": "There is no Rust trait for presenting a Rust object to agent Python in 0.3. An
embedder whose service is Rust ships a pure-Python client package and declares it as a binding,
hosted like any library". `hosted-objects.md`, "Rejected alternatives": "**Rejected: a Rust
service trait.** Presenting a Rust object to agent Python means writing RPyC's server side in Rust,
or an FFI layer under a Python class. A Rust service can instead ship a pure-Python client that is
hosted like any library. The trait idea is `plan/next/rust-object-as-python-object.md`." That entry
holds the shape -- `describe` and `call` with a `CallContext`, an instance bound by name, a
generated object per kernel, calls as JSON in a message kind of their own, a schema check in Rust,
no binding process, no RPyC, synchronous like a hosted call, under admission and the report -- and
four open questions; this task absorbs it, and its questions are the forks below.

What exists to build on. The skills package already asks the host synchronously and is answered
by Rust: `_ask` (`crates/outrig/src/python/outrig/skills.py:393`), `_ask_host`
(`crates/outrig/src/python/interpreter.py:4219`) blocking the calling thread on a queue the reader
thread fills (`:4212`, `:6670`), `_from_agent` naming the calling agent (`skills.py:461`), the
`skill.` prefix routed to an attached hook and answered from a task of its own
(`crates/outrig/src/python/host.rs:2109`, `:944`; `crates/outrig/src/harness/skills.rs:1241`).
The relay is the admission and event pattern: `first()` refuses once the session closed
(`crates/outrig/src/python/hosted.rs:592`), `refuse` and `receipt` publish `hosted.received` and
`hosted.ended` from a `Call` record (`:680`, `:151`), `report()` lists outcomes after the close
(`:529`), and `Session::shutdown` waits on `drained()` (`crates/outrig/src/harness/session.rs:396`;
`hosted.rs:509`). `SessionBuilder::bind` and its name rule
(`crates/outrig/src/harness/builder.rs:225`, `:214-228`), `BindingSpec`
(`crates/outrig/src/harness/bindings.rs:32`) and the
`Brief` the orientation lists (`bindings.rs:316`; `crates/outrig/src/agent/orientation.rs:267`) are
what a service sits beside. Each kernel gets a stub per binding at its build (`bind_stubs`,
`interpreter.py:5439`), `runtime.bindings` lists `Binding` records (`:4017`, `:6160`), and
`outrig.bindings[name]` resolves through the calling execution's kernel
(`crates/outrig/src/python/outrig/_kernel.py:55`). `outrig.schema` declares Python types and
renders their schema (`crates/outrig/src/python/outrig/schema.py:818`); nothing renders a JSON
Schema into a signature. `tokio-util` is a dependency; no JSON Schema validator is in the lock.
`0003-42` registers facility kinds with a resolver and a brief and spells the reference
`outrig.facility(name)`; `0003-46` names a bound facility in a root's constructor dict.

The requirement is OR-A3-1 through OR-A3-5 of `A-03-rust-service-facilities.md`: bind by name and
present as an object with signatures, docstrings and `help()` from the description, arguments
checked against the schema before `call`, calls evented like hosted calls with attribution and
under admission and the report; a service as a facility-dict entry with per-agent proxies and
attribution, a child's release leaving the parent served, and no discovery without the entry; the
service's internals never crossing; calls that return promptly, the ticket pattern, a per-call
deadline as a policy input, and a call still running at shutdown cancelled through `CallContext`
and reported `unknown`; one adapter serving MCP where that fits. The design is
`plan/phase/0003-python/services.md`. This task reverses the two quoted decisions and
records both in its `## Decisions`. It touches no item A-08 freezes (`Outrig`, `image::*`,
`LaunchSpec`, `SidecarSpec`, `MountSpec`, `EmbeddedMcpPolicy`, `McpToolResult`, `ToolHandle`,
`CapabilityProfile`): everything here is added beside the harness.

## Goal

An embedder binds a Rust service by name and agent Python calls its described methods as a
generated object -- checked, attributed, evented, bounded, cancellable and reported like a hosted
call, with no RPyC and no binding process -- and a service sits in an agent's facility dict as a
kind of its own.

## Deliverables

- **The trait and its types** (`services.md`, "The trait"): `Service` with `describe()` and
  `call(method, args, cx)` returning a boxed future; `ServiceDescription` with `description`,
  `methods` (`MethodDescription`: exact `name`, `description`, `params` and optional `result`
  schemas, `extras`) and `presenter`; `CallContext` with the call id, the service's name, the agent
  and execution, the deadline and a `CancellationToken`; `ServiceError` with a `#[non_exhaustive]`
  `category` (`InvalidArguments`, `NotFound`, `Refused`, `Unavailable`, `Failed`, `Cancelled`,
  `Other`) and a `message`; `ServicePolicy` with `call_deadline`, `cancel_grace`, `result_max` and
  `record_bodies`, each with OutRig's default (forks 3 and 4). A result is JSON by value and never
  a service object (fork 1).
- **Binding.** `SessionBuilder::serve(name, Arc<dyn Service>)` and `service_policy(policy)`, under
  `bind`'s name rule; a name given twice, or that a binding uses, fails the start. Services are
  fixed at start. No approval: the embedder is the owner.
- **The table**, in Rust, attached to the interpreter's lines with a `service.` prefix as the
  skills hook is: admission through `Lifecycle` before anything (`lifecycle.rs:117`, `:77`); the
  method looked up in the current description by exact name and generation, unknown refused; the
  arguments checked against `params` by the validator fork 2 chooses, a failure refused as invalid
  arguments with the first failing JSON path; a schema that does not compile recorded once and
  that method dispatched unchecked; the call's id minted and its record kept; `service.call.
  dispatched` published, then `call` run as a task with the `CallContext`; the result measured
  against `result_max` and refused as too large with the limit and the size when over; the
  answer sent, every accepted line answered, at the close included; the drain's set exposed for
  `Session::shutdown`, and `on_close` heard.
- **Deadline, cancel, abort** (`services.md`, "Dispatch, deadline and cancel"): the deadline from
  the policy, lowered by a call's `timeout` and never raised, a value above it raising
  `ValueError` in Python before anything is sent (fork 6); the complete encoded `service.call`
  line measured against the host's line bound (`REPLY_LINE_MAX`, 16 MiB) before it is sent, a
  line over it raising `ValueError` naming the bound and the size, sending nothing and
  registering no wait, since the host's reader cuts such a line and would never dispatch it or
  arm a deadline for it (`services.md`, "The crossing"); at the deadline the token with reason
  `deadline`, the grace, then abort and `unknown`; on an interrupt the interpreter's
  `service.cancel`, the token with `interrupted`, the agent's interrupt raised where the call was
  made and the late answer dropped; at terminating the token with `closed`, the grace, abort,
  `unknown`. A `call` that honors the token ends `raised` with category `Cancelled`.
- **The protocol lines**: the `services` declaration before any kernel opens (name, description,
  methods, generation); `service.described` for a later change, with the next generation;
  `service.call`, `service`, `service.cancel` as `services.md`, "The crossing", lists them; the
  wait as `_ask_host`'s, interruptible, with no timeout of its own. JSON crosses the pipe; in each
  process the call and the result are objects.
- **The generated object** (`services.md`, "The generated object"): a class per description per
  interpreter from the `presenter` kind -- `outrig.Service` here -- and an instance per kernel at
  its build, knowing its kernel; a method per identifier-named operation with `__signature__` and
  `__doc__` from a renderer the package owns, falling back to `(**arguments)`; `call(name,
  arguments=None, *, timeout=None)`, `method(name)` and `methods` for every operation by exact
  name; the attribute convenience and its ambiguity error; `__dir__`, a safe `repr`, a bounded
  `help()`; `runtime.bindings` listing it with `kind: "service"` and the class's qualified name;
  `outrig.bindings[name]` resolving to the kernel's instance; a convenience global where the name
  is free. The presenter-kind registry, with `service` registered, for `0003-54` to add `mcp` to.
- **Exceptions**: `outrig.ServiceError` with `outcome`, `category`, `service`, `method`, `call`,
  `message` and `path`, raised for every refused, raised or unknown outcome; the interrupt is the
  interrupt.
- **The facility kind** `service`, registered with `0003-42`'s registry: `outrig.facility(name)`
  for a bound service, resolving to the calling kernel's instance, with the description as its
  brief; a dict-constructed kernel builds an instance only for the services its dict names; a
  child's release drops its instance; on the host the reference is data `0003-46` sends.
- **The orientation**: each service by name and description beside the bindings, the blocking
  rule stated once for both, and the ticket sentence (`services.md`, "Tickets"); `child_preamble`
  the same for a dict-constructed child through `0003-42`'s manifest.
- **Events**, integration audit, in `doc/reference/events.md`: `service.bound`, `service.
  described`, `service.call.received`, `service.call.dispatched`, `service.call.cancel.sent`,
  `service.call.ended`, with the fields `services.md`, "Events", lists; bodies off by default and
  bounded previews under `record_bodies`; `diagnostics.md`'s rule for a family met and said so.
- **The close and the report**: `lifecycle.md`'s close table gains the **Service calls** rows;
  `ShutdownReport` gains `services` (`report.rs:19`, beside `hosted` at `:105`); an `unknown`
  service call is an unknown outcome for the verdict (`report.rs:41`). The Python `ShutdownReport`
  mirrors it.
- **A test service**, the only backend this task builds: `echo`, `fail(category)`, `sleep(secs)`
  awaiting without the token, `sleep_honoring(secs)` selecting on it, `block(secs)` sleeping on a
  blocking thread, `ask(question) -> ticket`, a `read-file` and a `class` method, one with a
  `params` schema the renderer cannot render, one whose schema does not compile, a method whose
  result is 20 MiB, and a description that can be changed under a running session; it holds a
  token string that must appear nowhere.
- **Docs.** `doc/concepts/` gains the service beside hosted objects in the Python runtime page
  `0003-38` writes, with the ticket pattern; `SECURITY.md`'s known boundaries gain a service entry
  parallel to the binding's; `events.md` and `lifecycle.md` as above; `crates/outrig/CHANGELOG.md`.
- **`## Decisions` records**: the two quoted sentences with what replaces each and why; the
  validator crate chosen and its test-suite evidence beside the candidates not chosen; the
  renderer's coverage; the defaults; the generated class's cost at 500 methods; the spellings.

## Acceptance

Mock-driven, in the host-session test mode `builder.rs` provides, with the test service bound and
the Python side driven as the interpreter's tests drive agent code.

- **A call returns data.** `svc.echo(x=1)` returns `{"x": 1}` as a dict; `service.call.received`
  and `service.call.ended` name the service, `echo`, the agent, the execution and a duration, with
  `arguments: null` and `body: null`; with `record_bodies` they hold bounded previews.
- **The schema refuses before `call`.** `svc.echo(x="no")` against `x: integer` raises
  `ServiceError` with `outcome == "refused"`, `category == "invalid_arguments"` and `path ==
  "/x"`; the fixture's call count is unchanged; `service.call.ended` says so and no
  `service.call.dispatched` precedes it. The method whose schema does not compile is callable,
  `service.bound` names it as unchecked, and its arguments reach `call` as given.
- **Exact names.** `svc.call("read-file", {"path": "a"})` and `svc.method("class")()` reach the
  fixture under those names; `svc.read_file(path="a")` and `svc.class_()` resolve them; with a
  second method `read_file` present, `svc.read_file` raises the ambiguity error naming both and
  `.method(name)`; `dir(svc)` lists what resolves; `help(svc)` renders `echo(x: int)` from the
  schema and `(**arguments)` for the unrenderable one, both with their descriptions.
- **The deadline, the grace, the abort.** `svc.sleep(60)` under a 1-second deadline raises
  `ServiceError` with `outcome == "unknown"` and `reason == "deadline"` after about a second plus
  the grace; `service.call.cancel.sent` says `deadline`; the session's later `shutdown` is `Clean`.
  `svc.sleep_honoring(60)` raises `outcome == "raised"`, `category == "cancelled"` within the
  grace. `svc.block(60)` is aborted the same way and the shutdown returns without waiting on the
  blocked thread, which the test joins afterward. `svc.call("echo", {}, timeout=10_000)` above the
  deadline raises `ValueError` naming the ceiling and sends nothing. `svc.echo("x" * (17 << 20))`
  raises `ValueError` naming the line bound and the size before anything is sent: the fixture's
  `call` count stays at zero, no `service.call.dispatched` is recorded, and the calling thread
  returns at once; the same call from a `to_thread` worker returns the same way.
- **An interrupt cancels.** An interrupt of the primary during `svc.sleep(60)` raises in agent
  code, `service.call.cancel.sent` says `interrupted`, the token fired in the fixture, and the
  kernel's loop is free; a call in a `to_thread` worker is not woken and ends at its deadline.
- **The close.** `shutdown(deadline)` with `svc.sleep_honoring(60)` in flight ends it `raised`
  within the drain; with `svc.sleep(60)` in flight it ends `unknown`, reason `closed`, after the
  deadline and the grace, `services` lists it and the verdict is stopped with unknown; a call
  made after `close_admission()` raises `outcome == "refused"`, `category == "admission_closed"`,
  and the fixture never sees it.
- **The result bound.** The 20 MiB method under a 16 MiB bound raises `outcome == "refused"`,
  `category == "result_too_large"` naming the limit and the size; `service.call.ended` records
  `returned` for the service and `bytes`; under a 32 MiB bound the result arrives.
- **A facility.** A child constructed with `{"svc": outrig.facility("svc")}` has its own instance:
  its call's `service.call.received` names the child; the parent's names the parent; releasing the
  child leaves the parent's next call working; a child built without it gets `KeyError` from
  `outrig.bindings["svc"]`, `NameError` for `svc`, and no mention in its orientation or
  `runtime.bindings`. A root `0003-46` constructs with the service as its `task` reaches it as
  `self._task` (`0003-46`'s test, cited here).
- **Tickets.** `t = svc.ask("q")` returns a ticket dict within the round; the owner later sends
  the answer on the user channel; `await runtime.wait({...})` in the agent's code returns on the
  arrival; no call blocked meanwhile.
- **Internals never cross.** The fixture's token string appears in no `repr`, `help()`,
  exception, event or report, checked by searching their serialized forms.
- **Listed and resolved.** `runtime.bindings` lists `svc` with `kind == "service"` and the class's
  qualified name; `outrig.bindings["svc"]` is the kernel's instance; the global `svc` exists where
  free and not where a name is taken.
- **A description changes.** The fixture adds `new` and removes `echo`; `service.described`
  carries generation 2 and the changes; `svc.new()` works after the interpreter has the line; the
  held `svc.echo` raises `outcome == "refused"`, `category == "unknown_method"` and the fixture
  sees nothing.
- **Measured.** A 500-method description builds its class and a kernel's instance in a time the
  test records in `## Decisions`.
- **Classified by name.** A test names every `ServiceError` category and every outcome, and the
  mapping from each to the Python `ServiceError` fields.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass; `services.md`, `lifecycle.md`'s rows,
  `events.md`'s entries and the docs say what the code does, checked by reading.

## Design forks

1. **A result holding further service objects -- Recommended: values only.** A nested object needs
   a reference into Rust and a lifetime; a service hands out an id and a method that takes it. The
   alternative returns references to further Rust objects, which is the object graph this page
   rejects.
2. **The JSON Schema validator -- Open; survey before choosing.** Candidates: `jsonschema`
   (crates.io, draft 2020-12, publishes official test-suite results) and `boon` (pure Rust, draft
   2020-12, the same suite); neither is in the lock. The task compares their suite results, their
   error paths and their dependency footprint under `default-features = false`, and records the
   choice with the evidence. The alternative is an in-house check of `required`, top-level `type`
   and `additionalProperties` with paths, which is lenient and must say so in `help()`.
3. **The result bound's default -- Recommended: 16 MiB**, measured on the serialized JSON. A
   result crosses the pipe as one line into the interpreter's heap; the default sits well under
   the memory ceiling and far above a tool result a model reads. The alternative is smaller and
   forces the policy sooner.
4. **The deadline and grace defaults -- Recommended: 600 seconds and 5 seconds**, the first equal
   to `DEFAULT_MCP_CALL_TIMEOUT_SECS` (`crates/outrig/src/config/mod.rs:177`) so an MCP service
   needs no separate default. The alternative is a shorter deadline, which a ticket-shaped service
   never needs and a blocking one pays twice.
5. **A Rust-side redescribe hook -- Open; recommended: not in this task.** The `service.described`
   line exists for the MCP presenter's relisting (`0003-55`); a hook the builder returns for a Rust
   service to call is one method and can follow when a service needs it.
6. **`timeout=` above the deadline -- Recommended: `ValueError` naming the ceiling.** A clamp would
   let code believe it bought time it did not get. The alternative clamps silently.
7. **Where the class is generated -- Recommended: in the interpreter, from the description line.**
   The alternative generates Python source on the host and sends it; it would cross code, which
   nothing here does.

## Dependencies

- **Hard: `0003-39`.** The interpreter program as a file; this task adds interpreter code.
- **Hard: `0003-42`.** The facility-kind registry, the reference's spelling and the `Binding`
  record's `kind`; this task registers `service`.
- **Hard: `0003-19`, `0003-22`.** The session, its lifecycle and report; the relay's admission and
  event pattern.
- **Soft: `0003-36`.** The rule that a convenience global is installed where free; without it
  landed, this task applies the rule to services alone.

## See also

- `plan/phase/0003-python/services.md` -- the design, every section.
- `plan/phase/0003-python/facilities.md` -- the dict, the reference and the view a service
  joins.
- `plan/phase/0003-python/mcp-presenter.md` -- the first service OutRig ships over this
  mechanism (`0003-54`, `0003-55`).
- `plan/phase/0003-python/embedding.md` ("Services written in Rust"), `hosted-objects.md`
  ("Rejected alternatives") -- the sentences this task reverses.
- `plan/phase/0003-python/boundary-policy.md` -- the grant, the events and the reader's rule.
- `plan/phase/0003-python/potential/ticket-based-service-waits.md` -- the ticket pattern.
- `plan/next/rust-object-as-python-object.md` -- absorbed here.
- `crates/outrig/src/python/hosted.rs`, `host.rs`, `interpreter.py`, `outrig/skills.py`,
  `outrig/_kernel.py`, `outrig/schema.py`; `crates/outrig/src/harness/builder.rs`, `bindings.rs`,
  `report.rs`, `lifecycle.rs`; `crates/outrig/src/agent/orientation.rs`.
