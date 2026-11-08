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
(`crates/outrig/src/python/outrig/_interpreter.py:4219`) blocking the calling thread on a queue the
reader thread fills (`:4212`, `:6670`), `_from_agent` naming the calling agent (`skills.py:461`),
the `skill.` prefix routed to an attached hook and answered from a task of its own
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
`_interpreter.py:5439`), `runtime.bindings` lists `Binding` records (`:4017`, `:6160`), and
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
- `crates/outrig/src/python/hosted.rs`, `host.rs`, `outrig/_interpreter.py`, `outrig/skills.py`,
  `outrig/_kernel.py`, `outrig/schema.py`; `crates/outrig/src/harness/builder.rs`, `bindings.rs`,
  `report.rs`, `lifecycle.rs`; `crates/outrig/src/agent/orientation.rs`.

## Decisions

- **The two reversals.** `embedding.md`'s "There is no Rust trait for presenting a Rust object to
  agent Python in 0.3. An embedder whose service is Rust ships a pure-Python client package and
  declares it as a binding, hosted like any library" is replaced by `Service` and
  `SessionBuilder::serve`: the first application's facilities are Rust over clients the embedded
  CPython cannot load, so the binding route cost a second client stack and a process per service,
  and the descriptor format it said a trait needs is the one the MCP presenter needs too.
  `hosted-objects.md`'s "**Rejected: a Rust service trait.**" is replaced by a service model with
  no proxies -- not RPyC's server in Rust, not FFI under a Python class -- whose calls are JSON
  lines on the interpreter pipe. Both plan pages already pointed at `services.md`; a Python
  library stays a binding, and a Rust service with a pure-Python client may still be bound as one.
- **The validator is `jsonschema` 0.61.0**, `default-features = false`. Surveyed 2026-11-08
  against `boon` 0.6.1: both pass 100% of the official suite's required tests for draft 2020-12
  and draft 7 (Bowtie's badges, `bowtie.report/badges/rust-{boon,jsonschema}/compliance/*.json`),
  and the first cut took `boon` for its four-crate footprint. Review found that the suite does not
  test what a service needs most: `boon` compares numbers as `f64`. Of twelve numeric cases run
  against both, `boon` got six wrong -- `maximum: 9007199254740992` accepting
  `9007199254740993`, the same at `minimum`, `multipleOf: 2` and `3` accepting integers past
  2^53 that are not multiples, and `multipleOf: 0.1` and `0.01` refusing `0.3` and `19.99` --
  and `jsonschema` none, comparing integers exactly and `multipleOf` as fractions. Its cost:
  about twenty crates the lock lacked (`referencing`, `jsonschema-value`, `jsonschema-regex`,
  `fraction`, `num`, `num-cmp`, `email_address`, `uuid-simd`, `bytecount`, `micromap`, a second
  `fancy-regex`, ...), and `serde_json`'s `float_roundtrip` on for the whole workspace through
  feature unification, which makes float parsing exact and the suite passes with it. It is built
  `offline()`, loading nothing outside a schema: a `$ref` to a file, a URL or a relative document
  does not compile, and its method is unchecked. `default-features = false` alone does not hold
  that -- Cargo unifies an embedder's `resolve-file` or `resolve-http` into the one `jsonschema`
  both use -- so the tests turn `resolve-file` on, as such an embedder would, and
  `a_schema_is_compiled_and_checked` has the crate load a schema file by default and the check
  refuse it. The validator reads `$schema` and takes draft 2020-12 without one. A refusal names
  `instance_path()`, a JSON pointer (`/x`, `/a/b`, `""` for the object itself), and the `masked()`
  message, which names the problem and not the value. `a_schema_is_compiled_and_checked` holds the
  numeric cases.
- **The renderer's coverage**, `outrig/_services.py`: an object schema's `properties`,
  `required`, `default` and `description`; property types `string`, `integer`, `number`,
  `boolean`, `null`, `array` (with `items`), `object`, a list of types, `enum` and `const` as
  `Literal`, `anyOf` and `oneOf` as a union, and `$ref` into `#/$defs/` or `#/definitions/`. A
  required property is positional-or-keyword in schema order; an optional one is keyword-only,
  showing its schema default (or `...`), and is left out of the call when not passed -- the
  schema's default is the service's to apply. A property whose type it cannot spell is a
  parameter with no annotation. A schema that is not an object of identifier-named properties --
  a top-level `anyOf`, a property named `self`, `read-file` or `class`, a keyword it does not know
  at the top -- renders `(**arguments)` and its schema in the docstring, cut to 2,000 characters.
  `additionalProperties` adds no `**`: a call of an undeclared argument goes through `call()`.
- **Defaults**: deadline 600 s (`DEFAULT_MCP_CALL_TIMEOUT_SECS`), grace 5 s, `result_max`
  16 MiB measured as compact JSON by counting the serializer's bytes, `record_bodies` off.
  `ServiceDescription::call_deadline`, the service's own ceiling, is added: `services.md` and
  `mcp-presenter.md` rely on it though the task's list omitted it.
- **Spellings.** Builder: `serve`, `service_policy`. Types: `Service`, `ServiceFuture` (our own
  alias, so no `futures` type is in the API), `ServiceDescription` (`with_method`,
  `with_call_deadline`), `MethodDescription` (`with_result`), `CallContext` (`call`, `service`,
  `agent`, `execution`, `invocation`, `deadline`, `cancelled`, `is_cancelled`), `ServiceError`,
  `ServiceErrorCategory` (snake_case), `ServicePolicy`, `ServiceCall`; events' `ServiceCallId`,
  `ServiceKind::Rust`, `ServiceOutcome`, `ServiceReason`, `ServiceChanges`. Lines: `services`
  (only when a session has one, so existing sessions' wire is unchanged), `service.described`,
  `service.call`, `service`, `service.cancel`. Python: `outrig.Service`, `outrig.ServiceError`,
  `outrig.ServiceMethod`; a generated class is `outrig.services.<name>`. The presenter kind is
  crate-private (`ServiceKind` decides it), since no application presenter is read.
- **The token stays private.** The deliverable's `CancellationToken` is what `CallContext` holds,
  and what it offers is `cancelled()`, a future that resolves when the token is triggered, and
  `is_cancelled()`: `tokio-util` is not among the crates the public surface may name
  (`crates/outrig/tests/public_api_boundary.rs`), so a release of it would otherwise be an
  outrig major. A service's own task clones the context and awaits `cancelled()` there.
- **Category and reason.** In events and the report, `category` is the service's, for `raised`;
  `reason` says why for `refused`, `cancelled` and `unknown`, as `HostedReason` covers all three.
  The Python `ServiceError` also carries a refusal's reason as its `category`, as the acceptance
  reads it (`category == "invalid_arguments"`), so `category` answers "why" for every call but an
  abandoned one. `each_outcome_reaches_agent_code_as_its_fields` pins the mapping.
- **A call from a kernel the host did not open is answered**, refused `unknown_agent`, with a
  warning and no event -- unlike the relay, which drops such a line -- because the service wait
  has no timeout of its own and a dropped line would hold its thread for good. The object refuses
  in Python first: a call from another kernel's execution raises `TypeError` (as `_Pool.request`
  does), and one on a released kernel's object `RuntimeError`, so only a forged line reaches it.
- **The wait.** `_ask_host`'s wait is split out as `_await_host(ask_id, line, timeout)`, which a
  service call uses with its own encoded line and no timeout of its own, and `_ask_host` keeps its
  signature for the skills module. The line is measured by `_refuse_long`, which now takes a
  `limit` (`SERVICE_LINE_MAX`, `REPLY_LINE_MAX`'s twin, each naming the other) and returns the line
  it encoded, so it is encoded once. `_encode(strict=True)` refuses a NaN -- a token the host
  cannot parse, so a line it would never answer -- with `ValueError`, and writes text unescaped,
  so non-ASCII arguments do not reach the bound twice as soon; a lone surrogate falls back to
  `_clean`. An exception out of the wait -- the interrupt -- sends `service.cancel` and re-raises.
- **One session table.** A kernel with the session's view presents `_SESSION`, the bindings' and
  services' records together, each resolved through `_KINDS` by its record's `kind`:
  `offer_session` replaces `bind_stubs`, and `facility`, `facility_names` and `briefs` have no
  kind of their own. A binding is still left out of `outrig.bindings` without RPyC.
  `outrig.facility(name)` reads the kind from that table through `_agents._kind_of`, installed as
  `_resolve` is; on the host every name stays a binding's until a host program may give
  facilities.
- **The close is read, not heard.** A call that ends after `lifecycle.closed()` is listed in the
  report; the table needs no `on_close` hook of its own, since it reads the same admission at each
  end.
- **The close tells services first.** At the drain's end `Services::terminate` triggers every
  token, reason `closed`, before `stop_live` interrupts the kernels, so the interrupt's
  `service.cancel` cannot win the first-reason-holds race; the shutdown then waits the grace plus
  a second for them. `report()` seals the table and reports a call still in flight -- its abort
  not yet seen -- as `unknown`, `closed`, aborting it.
- **A panic** in a service's `call` ends the call `raised`, `failed`, with "the service's call
  panicked": no payload crosses, since a panic message is the service's internals. The fixture's
  credential is in its `panic` method's message, so `internals_never_cross` fails if one ever
  does.
- **A description that changes** is `Services::redescribe`, crate-private (fork 5 as
  recommended): a test drives it, and `0003-55`'s relisting will. The interpreter makes the next
  generation's class and reassigns `__class__` on every kernel's object, so `svc` and
  `outrig.bindings["svc"]` stay one object; a method held from the old class keeps its
  generation, and the host refuses one the new description lacks.
- **The attribute convenience**: a dunder-shaped name stands for no method, and a name the class
  already uses (`call`, `method`, `methods`, `_service`, ...) gains `_`. An ambiguous attribute is
  a descriptor raising `AttributeError`, so `hasattr` is false and `dir` leaves it out.
- **`help(svc)` shows `self`.** pydoc documents the generated class, whose methods are functions
  with `__signature__` including `self` -- so `inspect.signature(svc.echo)` is `(x: int)` -- and
  shows `echo(self, x: int)`, as it shows every class's methods.
- **Measured.** A 500-method description's class and one object of it take 18-20 ms in the
  interpreter (three runs: 18.2, 18.5, 19.7 ms), `five_hundred_methods_are_cheap`. Per kernel the
  object is one `object.__new__`; the class is made once per description.
- **The CLI prints no service rows**: `outrig run-new` serves none, so its report's `services` is
  always empty. `scripts/render-session.py` renders them in a session page's report.
- **Arguments and results cross as JSON text.** The arguments ride in the `service.call` line as
  a string, which the host parses on the call's task, and a result rides in the answer as one,
  which the caller decodes. A value one side's parser refuses -- nested past `serde_json`'s 128
  levels, say -- then ends that call, refused as invalid arguments or raised in the caller as
  `ValueError`, rather than making the whole line unreadable: the reader drops a line it cannot
  read, and a call waiting with no timeout of its own would wait for good. The result is
  serialized once, for its size and for the line. Previews are the texts cut to 1,024 characters,
  so an argument preview is the interpreter's spelling (`{"x": 12345}`) and a result's the
  host's. A result too deep for the interpreter's decoder is past where serializing it would
  already strain the host's stack, so `answer()`'s unit test holds the text and nothing drives
  one end to end. `arguments_the_host_cannot_read_are_refused_at_once` drives 200 levels.
- **A call's stop is told outside the table's lock.** `Flight::stop` is shared: `stop()` takes it
  out of the table and tells it with the table unlocked, and `run` copies what it says before
  `ended` locks the table, so the two locks are never held in opposite orders -- a call cancelled
  before dispatch, told to stop again by the close, deadlocked the reader and the shutdown
  otherwise. The race is too narrow to drive, so no test holds it.
- **The renderer stops at a depth.** `_annotation` follows `$ref`s, unions and `items` 32 steps
  and leaves a deeper type unspelled, so a chain of 1,100 `$ref`s renders `(*, x=...)` instead of
  raising `RecursionError` -- which, raised while the `services` line was taken, left every
  service of the session without an object (`a_long_reference_chain_leaves_every_service_bound`).
  It also visits at most 256 schemas for one type, a `$ref` counted each time it is met: a depth
  bounds each path but not how many there are, and definitions that branch to one another are as
  many as their product -- twenty, each an `anyOf` of three `$ref`s to the next, are 3^20 paths
  in under 2 KiB, which held the interpreter's reader of host lines for longer than the test's
  step allows (`a_branching_reference_graph_renders_at_once`). A budget over memoizing each
  definition's type, which would spell more such graphs: what a definition renders to depends on
  the path that reached it, through the cycle check and the depth.
- **The fallback's receiver is positional only**, `(self, /, **arguments)`, so a schema that
  names an argument `self` -- which sends its method to the fallback -- takes it as an argument
  (`an_argument_named_self_is_an_argument`).
- **Not built here**: a cap on calls in flight per agent, as the relay's 1,024 --
  `plan/next/bound-service-calls-in-flight.md`. Audience classification of the new fields is
  `0003-68`'s, with the rest.
