# Services

A **service** is a Rust object an embedder binds by name and agent Python reaches as a generated
object: one method per operation the service describes, with a signature and a docstring from the
description, called synchronously as a hosted object is called, the call crossing the interpreter
pipe as JSON and dispatched by Rust to the service's `call`. A service is a facility kind beside a
hosted object (the phase README, "Vocabulary"; `facilities.md`). This page decides the trait and
its description, how a service is bound, how a call crosses and is admitted, checked, dispatched,
bounded, cancelled and reported, what the generated object offers, how a service sits in a
facility dict, and the events. `0003-44` builds all of it with a test service and registers the
`service` facility kind in `0003-42`'s registry, beside `binding` and `object`
(`facilities.md`); `0003-46` constructs a root with a service in its dict; `0003-54` and
`0003-55` make the MCP presenter a service OutRig ships (`mcp-presenter.md`). The requirements
are OR-A3-1 through OR-A3-5 of `A-03-rust-service-facilities.md`. The first application is a
Rust embedder of `outrig::harness` whose facilities are Rust over its own clients; nothing of its
names appears in a mechanism here.

Spellings of types, methods, lines and keys are illustrative; `0003-44` fixes them and records them
in its `## Decisions`.

## What exists, and what this page reverses

Phase 0003 hosts Python objects only. A factory binding's library runs in the bindings process and
agent Python reaches it over RPyC frames (`hosted-objects.md`); a live object is served from a host
program (`0003-36`). For a service written in Rust the phase decided a route without a trait.
`embedding.md`, "Services written in Rust": "There is no Rust trait for presenting a Rust object
to agent Python in 0.3. An embedder whose service is Rust ships a pure-Python client package and
declares it as a binding, hosted like any library". Its reason: "A trait needs a descriptor format
-- methods, argument and result schemas -- and a second way of presenting an object beside hosted
Python objects, with interception and events built a second time. A pure-Python client uses the
one path this phase builds." `hosted-objects.md`, "Rejected alternatives": "**Rejected: a Rust
service trait.** Presenting a Rust object to agent Python means writing RPyC's server side in Rust,
or an FFI layer under a Python class. A Rust service can instead ship a pure-Python client that is
hosted like any library. The trait idea is `plan/next/rust-object-as-python-object.md`." The 0003
README's out-of-scope list says the same of "A Rust embedder injecting a live object. Its objects
are bindings through `bind`; `plan/next/rust-object-as-python-object.md` is the adapter."

That entry holds the shape this page starts from: a trait with `describe` and `call`, an instance
bound by name on the builder, a generated object per kernel with a signature and docstring per
method, calls by value as JSON in a message kind of their own on the interpreter pipe, checked
against the schema in Rust, with no binding process and no RPyC, synchronous with a hosted
object's spelling, under admission and the report, and a `CallContext` carrying "the call id, the
binding and a cancellation signal, so unlike an RPyC call this one can be told to stop". Its open
questions -- a result holding further service objects, a `describe` that changes, the error kinds,
how an MCP tool's schema and name become a method -- are `0003-44`'s forks, and `0003-44` absorbs
the entry. `potential/ticket-based-service-waits.md` names the case: a Rust service reached from
agent Python, with waits as tickets rather than blocked calls; `0003-17` measured that a blocked
hosted call holds one connection and one thread, so tickets "stay an application's choice", and
"the step past tickets -- a direct Rust service adapter, with no hosted client and no RPyC -- is
`plan/next/rust-object-as-python-object.md`".

**What replaces the decision, and why.** The trait is built, by `0003-44`, and the two quoted
sentences are reversed; the task records both in its `## Decisions`. The first application's
facilities are Rust over clients the embedded static CPython cannot load (`discovery.md`,
"Capability is a contract"), so the 0.3 route costs a second client stack per service and a
process between the agent and each one, and the application does not take it (A-03, "What
exists"). The descriptor format `embedding.md` said a trait would need is the price of two
mechanisms built once: the MCP presenter needs a Rust-served call with a cancellation signal
(`mcp-presenter.md`, "Calls"), and one adapter serves both (OR-A3-5), so the second presentation
is written once and not twice. What stands from the 0003 decision: a Python library stays a
hosted object; a Rust service that already has a pure-Python client may still be bound as one. The
relay's shape for admission and events (`crates/outrig/src/python/hosted.rs`) is reused as the
pattern, not the code: a service call never touches RPyC or the bindings process.

## The trait

```rust
pub trait Service: Send + Sync + 'static {
    fn describe(&self) -> ServiceDescription;
    fn call(&self, method: &str, args: Value, cx: CallContext)
        -> BoxFuture<'_, Result<Value, ServiceError>>;
}
```

- **`ServiceDescription`**: a one-line `description` for the orientation, the `methods`, each a
  `MethodDescription` -- `name` (any string; kept exact), `description`, `params` (a JSON Schema
  for the arguments object), `result` (an optional JSON Schema), `extras` (a map the presenter of
  the service's kind may read and the generic one ignores) -- `presenter`, the kind of Python
  presenter the object is built from: `service`, the generic one, unless a kind OutRig ships says
  otherwise ("The generated object") -- and an optional `call_deadline`, the service's own ceiling
  in place of the policy's default, for a service whose calls legitimately run long. A
  description may change after start ("The crossing", `service.described`).
- **`CallContext`**: the call's id as the host minted it, the service's bound name, the agent and
  execution the call is attributed to, the deadline, and a `CancellationToken` (`tokio-util`,
  already a dependency) that the deadline, an interrupt and the close trigger. A `call` that
  observes the token returns promptly with `ServiceError::Cancelled`; one that does not is aborted
  ("Dispatch, deadline and cancel").
- **`ServiceError`**: a `category` from a `#[non_exhaustive]` enum -- `InvalidArguments`,
  `NotFound`, `Refused` (the service does not permit it), `Unavailable`, `Failed`, `Cancelled`,
  `Other` -- and a `message` the service wrote for a reader, redacted by the service. OutRig adds
  nothing of the transport to either ("Internals never cross").
- **`Value` in, `Value` out.** Arguments are a JSON object by value, checked before `call` runs;
  a result is JSON by value and becomes Python data in the interpreter. A result never carries a
  further service object (fork 1 of `0003-44`, recommended values only).

**Binding.** `SessionBuilder::serve(name, Arc<dyn Service>)`, beside `bind`
(`crates/outrig/src/harness/builder.rs:225`), under `bind`'s name rule (`builder.rs:214-228`: a
Python identifier, not a keyword, not `runtime`, `asyncio` or `outrig`, not a name the
configuration or another binding uses; the start fails otherwise). A service given through the
API is the embedder's own and needs no approval, as a binding given through the API needs none
(`embedding.md`, "What the embedder supplies"). Services are fixed when the session starts, as
bindings are: "The agent still cannot grow its own environment" (`security.md`, "What this does not
change"), and no agent-callable operation binds one. Binding the service is the grant
(`boundary-policy.md`, "The binding is the grant"): every described method is callable by every
agent of the session, recorded as events, and a service that should do less describes less
(`boundary-policy.md`, "Narrow bindings for consequential actions" -- the service author resolves
identifiers from its own configuration and rechecks state when it executes; the mechanism enforces
nothing beyond the schema).

A Rust owner may also hand a service to the root it constructs, by its bound name, in the
constructor's facility dict (`0003-46`, `Facility { kind, name }`; OR-A3-2), the `task` object
included.

## The crossing

JSON crosses the interpreter pipe and nothing else does; in each process a call and its result are
Python or Rust objects.

- **The host tells the interpreter the services** once, before any kernel opens, in a line beside
  `bindings` and `skills` (`crates/outrig/src/python/host.rs:1220`, `:950`;
  `crates/outrig/src/python/interpreter.py:6176`): each service's name, its description, its
  methods and its `generation`, 1 at start. A description that changes is a `service.described`
  line of its own with the next generation.
- **A call is a `service.call` line** from the interpreter: the ask id the answer is matched by,
  the `agent` and `exec` from the calling context (as `_from_agent` names them for a skill request,
  `crates/outrig/src/python/outrig/skills.py:461`), the `invocation` when inside a skill, the
  service, the method's exact name, the generation the method came from, the arguments, and an
  optional `timeout` that may only lower the deadline. The host accepts `agent` only for a kernel
  it opened, as the relay accepts an `rpc` line's (`hosted.rs:343-353`). Before sending, the
  interpreter measures the complete encoded line against the host's line bound (`REPLY_LINE_MAX`,
  16 MiB, `host.rs:147`): the reader cuts a longer line and keeps the stream in step
  (`next_line`, `host.rs:2379-2408`), so such a line would never be dispatched and no deadline
  would ever be armed for it. A line over the bound raises `ValueError` in the caller naming the
  bound and the size, sends nothing and registers no wait, as a spawn whose arguments take more
  than 1 MiB to describe does (`interpreter.py:4844`).
- **The answer is a `service` line** with the ask id and `ok` holding the result, or `error`
  holding the call id, the outcome, the category, the message and, for an argument the schema
  refused, the JSON path. The host answers every line it accepts, at the close included, so no
  thread waits for good, as `skills.rs:1241` guarantees for a skill request.
- **The wait blocks the calling thread**, as `_ask_host` blocks (`interpreter.py:4219`): on a
  queue the reader thread fills (`:4212`, the `skill` arm at `:6670`), interruptible for agent
  code by the same mechanism, with no timeout of its own -- the host's deadline governs, and the
  size check above is what guarantees a deadline is armed for every line that is sent. The
  reader thread never waits (`hosted-objects.md`, "Calls are synchronous"). A call on a kernel's
  own thread holds that kernel's loop until the answer, and `await asyncio.to_thread(...)` keeps
  it turning; the orientation says so once for bindings and services together.
- **`service.cancel`** is a notice from the interpreter naming a call whose caller was interrupted;
  the host triggers the call's token.

The lines with a `service.` prefix are routed as `skill.` lines are (`host.rs:2109`, `:944`) to a
hook the service table attaches, and answered from a task of their own.

## The table

One table per session, in Rust, holding each bound service, its current description and
generation, and its calls in flight. It:

- **Checks admission** through the session's `Lifecycle`
  (`crates/outrig/src/harness/lifecycle.rs:117`, `:77`) before anything else, as the relay's
  `first()` does
  (`hosted.rs:592`): a call after the close is refused, admission closed, and the agent sees the
  closing error (`lifecycle.md`, "Admission").
- **Checks the method and the arguments.** An unknown method, or one from an older generation
  that the current description no longer lists, is refused as unknown method before dispatch. The
  arguments are checked against the method's `params` schema by a JSON Schema validator in Rust
  (fork 2 of `0003-44`: the crate is chosen by survey), and a failure is refused as invalid
  arguments with the first failing JSON path named; the check runs on the trusted side because
  "Nothing on the container side is trusted" (`boundary-policy.md`). A schema the validator cannot
  compile is recorded once as a `service.described` detail, and that method is dispatched
  unchecked, so a service is never lost for an unusual schema.
- **Mints the call's id and record**: the id every event of the call carries, the service, the
  method, the generation, the agent and execution claimed, the time received, and the token.
- **Publishes `service.call.dispatched` before `call` is invoked**, as `hosted.dispatched` is
  published before a target is invoked (`boundary-policy.md`, "Events"), then runs `call` as a task
  of its own with the `CallContext`.
- **Bounds the result**: a result whose serialized size exceeds the policy's `result_max` is not
  handed over; the call ends refused as result too large, naming the limit and the size, with the
  outcome `returned` recorded -- the service did the work. The bound exists because a result
  crosses the pipe as one line and lands in the interpreter's heap, under its memory ceiling
  (`agent-placement.md`); its default is `0003-44`'s fork 3.
- **Keeps the drain's set**: the calls in flight, which `Session::shutdown` waits for beside the
  gate's executions and the relay's forwarded calls (`crates/outrig/src/harness/session.rs:396`),
  and hears the close through `Lifecycle::on_close`.

## Dispatch, deadline and cancel

A call has a **deadline**: the service's policy default, `ServicePolicy::call_deadline` (OutRig's
default is `0003-44`'s fork 4), lowered by the call's `timeout` and never raised. At the deadline
the token is triggered and `service.call.cancel.sent` says `deadline`; a `call` that returns within
the policy's `cancel_grace` ends with its own outcome -- `raised` with the category `Cancelled`
when it honored the token, or `returned` when it finished anyway -- and one that does not is
aborted (its task dropped at its next await point) and ends `unknown`, reason `deadline`, since
what it did before the abort stands. A `call` that blocks a thread rather than awaiting cannot be
aborted and is the service author's to bound; the abort frees OutRig, not that thread, and no owner
teardown waits on it (OR-A3-4).

An **interrupt** raises where the call is made, as for a hosted call (`hosted-objects.md`, "Calls
are synchronous"): the waiting thread is woken, agent code gets the interrupt, the interpreter
sends `service.cancel`, the token is triggered with the reason `interrupted`, and the call ends as
above, its late answer dropped as `_ask_host` drops one. A call in a `to_thread` worker is not
woken and ends by its deadline.

The **close** triggers nothing until the drain deadline: calls in flight drain (`lifecycle.md`,
"Closing a session"); at terminating each one left is sent the token, reason `closed`, given the
grace, and then aborted and reported `unknown`. That is the addendum's decided point -- "a call
still running at shutdown is cancelled through `CallContext` and reported `unknown`" -- and
`execution-and-rounds.md`'s "No rollback, ever": an `unknown` result is never retried by OutRig.

**Outcomes** are the five a hosted call has (`boundary-policy.md`, "Events"), so one reader's rule
covers both: `returned`; `raised`, with the service's category; `refused`, never invoked, with the
reason -- admission closed, unknown method, invalid arguments with its path, paused, result too
large (the one `refused` that follows an invocation, said so in the event); `cancelled`, never
invoked, when the close or an interrupt reached the call before its dispatch; `unknown`, invoked
and aborted, with the reason `deadline`, `interrupted` or `closed`. An MCP call maps its three
dispatch states onto these (`mcp-presenter.md`, "Calls").

## The generated object

**A class per description, an instance per kernel.** From each service's description the
`outrig` package generates a class once per interpreter -- the class of the service's `presenter`
kind, `outrig.Service` for the generic one -- and each kernel gets an instance at its build, as
`bind_stubs` gives it a stub per binding (`interpreter.py:5439`). The instance knows its kernel,
so a call made from a thread without an execution context is still attributed to that kernel's
agent; the execution is read from the context when there is one.

- **Methods from the schema.** One method per described operation whose name is a Python
  identifier and not a keyword, with `__signature__` and `__doc__` rendered from `params` --
  properties as parameters, `required` ones without defaults, descriptions as parameter notes --
  by a renderer the package owns (`outrig.schema`, `crates/outrig/src/python/outrig/schema.py:818`,
  declares Python types and renders nothing from a schema; this renderer is its counterpart and
  is shared with the MCP presenter). A `params` schema the renderer cannot turn into a signature
  yields `(**arguments)` with the description, and the method stays callable. `help(obj)` renders
  from the same data, bounded as `discovery.md`'s `help()` is.
- **Exact names, always.** `obj.call(name, arguments=None, *, timeout=None)` calls any described
  method by its exact name, `obj.method(name)` returns a callable bound to it, and `obj.methods`
  lists the descriptions. A method whose name is not an identifier (`read-file`), is a keyword
  (`class`) or clashes with the class's own members is reachable this way without exception; the
  convenience below is a mapping over it, never the only path (OR-A3's open choice, decided yes).
- **Attribute convenience**, deterministic and documented: `-` becomes `_`; a keyword or a clash
  with the class's own members gains a trailing `_`; `dir(obj)` lists what resolves; two exact
  names that meet one attribute, or an attribute that is also another method's exact name, make
  that attribute raise an ambiguity error naming the exact names and `.method(name)`. Nothing
  picks.
- **Listed as a binding is.** `runtime.bindings` lists the service with `kind: "service"`, its
  description and the generated class's qualified name (`interpreter.py:4017`; the `Binding` record
  gaining `kind` is `0003-42`'s); `outrig.bindings[name]` is the authoritative lookup, resolving to
  the calling kernel's instance as it resolves a binding's stub
  (`crates/outrig/src/python/outrig/_kernel.py:55`); a convenience global of the service's name is
  installed where the name is free (`0003-36`'s rule for a live object; `0003-42`'s `bind_stubs`
  rule for a dict-constructed kernel). `repr(obj)` and `help(obj)` show the name, the description
  and the methods, and nothing else.
- **A presenter per kind.** A service kind OutRig ships may register a subclass of
  `outrig.Service` as its presenter, named by `presenter` in the description: the subclass
  presents results and adds operations the kind has -- the MCP presenter wraps a call's JSON into
  content blocks and adds `relist()` (`mcp-presenter.md`, "The Python surface"). The generic class
  returns Python data as the JSON decodes. Whether an application may supply a presenter class
  from its own code is an open question; nothing here reads one.

**The orientation** lists each service by name and description beside the bindings
(`crates/outrig/src/agent/orientation.rs:267`), by `discovery.md`'s test for the preamble -- names
an agent cannot learn by looking -- with one rule stated once for bindings and services: a call
blocks this agent's loop until the host answers, and `asyncio.to_thread` keeps it turning; and
one sentence for services: a call returns at once with a value, so a method that starts long work
hands back a ticket and the agent waits with `runtime.wait`, never inside a call ("Tickets").

## A service is a facility

`facilities.md` owns the dict, the reference and the view. What a service adds to each:

- **The kind.** `service` registers a resolver and a brief with `0003-42`'s registry
  (`0003-42`, "The facility reference"): `outrig.facility(name)` for a bound service makes a
  reference of kind `service` that resolves, at each use, to the calling kernel's instance; its
  brief is the service's description and the generated class's name. On the host it is data an
  owner sends (`0003-46`).
- **Each receiving agent has its own instance** of the same service's class, so a call made
  through a child's dict is attributed to the child by the kernel the instance belongs to and
  the context it is made in; two children given one service produce `service.call.received`
  events naming themselves (OR-A3-2). There is no connection to share and nothing the parent's
  instance holds that a child could send on.
- **A child's release drops its instance** with its namespace and leaves the service serving its
  parent; a service lives for the session (`facilities.md`, "Lifetime").
- **An agent whose dict lacks the service does not see it**: not in its orientation, not in
  `runtime.bindings`, `KeyError` from `outrig.bindings[name]`, `NameError` for the global -- the
  presentation-only narrowing `facilities.md` decides, with the grant the session's.
- **The `task` object may itself be a service** handed in the dict or as the `task` argument
  (`task-agents.md`, `0003-46`); its methods are then recorded calls, where an `object` facility's
  are not (`facilities.md`, "Attribution").

## Tickets

A call returns promptly or it is holding a kernel's loop, a `to_thread` worker, or -- at the
close -- the drain. The mechanism supports the ticket pattern
`potential/ticket-based-service-waits.md` describes and OR-A3-4 requires: a method that starts
long work
returns a ticket or handle at once, as a value; the owner delivers the completion on a channel --
the user channel, or an external channel the owner defined (`external-channels.md`) -- and the
agent waits with `runtime.wait`, which watches its channels (`execution-and-rounds.md`,
"`runtime.wait` mirrors `asyncio.wait`"). OutRig supplies the prompt return, the deadline as the
backstop for a call that does not return, and the sentence in the orientation; the ticket's state
and its completion are the service's and the owner's. A service that must wait inside a call
declares a longer deadline in its policy and pays with the caller's thread, as a hosted call does.

## Internals never cross

A service's clients, tokens, fences and connection details stay in Rust. What crosses to the
interpreter is the description and a call's result; what reaches events and the report is built in
Rust from the fields the table holds -- names, ids, categories, sizes, durations -- and never from
a service's state. `repr`, `help()`, the listing and every exception the mechanism raises are built
from the description and the answer line. A service's `message` is the service's: OutRig forwards
it as written and adds nothing of the transport, so a service that puts a token in a message has
put it in Python; the rule the service author follows is OR-MCP4's, applied here (OR-A3-3). A
result is the service's too, and OutRig redacts nothing in it. Argument and result previews in
events are off by default and, when the policy turns them on, are built from the JSON by value and
cut to the bound children's previews use (`doc/reference/events.md`, "Three categories").

## Admission, the close and the report

The close table (`lifecycle.md`, "The close table") gains **Service calls** (`0003-44`): made after
the close -- refused, admission closed, never invoked, no effects; in flight -- drains to the
deadline; `returned` or `raised` if `call` finishes in time; else the token, the grace, an abort,
`unknown` with reason `closed`, effects open. A service has no process of its own to stop: it is
the embedder's object, dropped with the session.

`ShutdownReport` (`crates/outrig/src/harness/report.rs:19`) gains `services`: each service call
whose outcome came after the close, with service, method, agent, outcome, category and reason, as
`HostedCall` (`report.rs:105`) lists a hosted one. An `unknown` service call is an unknown outcome
for the verdict (`report.rs:41`): stopped with unknown, never clean and never failed -- so a
session whose last call was aborted at terminating is not `Clean`, while one whose service call
was cancelled and ended mid-session is, since the report lists outcomes after the close
(`lifecycle.md`, "The report"). The three rules of the report hold: no false clean report, no
rollback, a local stop says nothing about a remote outcome.

## Events

All `service.*` events are in the integration-audit category (`doc/reference/events.md`, "Three
categories"; `boundary-policy.md`, "Events", whose warning about arguments holds) and follow
`diagnostics.md`'s rule for a new family: a call's events carry the agent's subject as `hosted.*`
do (`crates/outrig/src/harness/event.rs:763`); `service.bound` and `service.described` carry none;
the family's own id is `call`, minted on the host; `execid` and `invocation` are claims the
interpreter made, said so in `events.md`; "attempt" is not used.

- `service.bound` -- at start, per service: `service`, `kind` (`rust`, or the presenter kind of a
  service OutRig ships, such as `mcp`), `methods` (count), `generation`.
- `service.described` -- a description changed: `service`, `generation`, `changes` (`added`,
  `removed`, `changed`, by exact name), `unchecked` (methods whose schema did not compile).
- `service.call.received` -- `call`, `service`, `kind`, `method` (exact), `generation`, `agent`,
  `execid`, `invocation`, `deadline`, and `arguments`, a bounded preview present only when the
  policy records bodies.
- `service.call.dispatched` -- `call`, `service`, `method`, `agent`: published before `call` is
  invoked.
- `service.call.cancel.sent` -- `call`, `reason` (`deadline`, `interrupted`, `closed`, `stopped`).
- `service.call.ended` -- `call`, `service`, `kind`, `method`, `agent`, `execid`, `outcome` (the
  five), `category`, `reason`, `detail` (the path for invalid arguments; the limit and size for
  result too large), `bytes`, `duration`, and `body`, a bounded preview present only when the
  policy records bodies.

An MCP call is a service call and uses this family with `kind: "mcp"` and the fields
`mcp-presenter.md` adds (`tool`, `listing`, `state`, `failure`); the MCP lifecycle events --
placement, startup, reconnect, pause -- keep the `mcp.service.*` prefix, since a Rust service has
no placement (`mcp-presenter.md`, "Events"; the fork recorded there). That a call was never invoked
is read from `outcome`, never from a missing `service.call.dispatched` across a gap
(`boundary-policy.md`'s rule for a reader).

## What each task builds

| Task      | Builds                                                                              |
| --------- | ----------------------------------------------------------------------------------- |
| `0003-44` | the trait, `ServiceDescription`, `CallContext`, `ServiceError`, `ServicePolicy`;    |
|           | `SessionBuilder::serve`; the table with admission, the schema check, dispatch, the  |
|           | deadline, cancel and abort, the result bound, drain and report; the protocol lines; |
|           | the generated class with the schema renderer, exact-name call, convenience and      |
|           | `help()`; the `service` facility kind; the orientation; the events and close rows;  |
|           | a test service                                                                      |
| `0003-54` | the MCP `Service` implementation and its `presenter` kind, registered per placed    |
|           | service (`mcp-presenter.md`)                                                        |
| `0003-55` | the MCP presenter subclass: content-block results, `tools`, `relist()`, the         |
|           | description from `tools/list` with exact names                                      |

## Rejected alternatives

**Rejected: a pure-Python client per Rust service, hosted as a binding** -- the 0.3 route, quoted
above. It costs a client stack the embedded CPython may not be able to load, a process between the
agent and the service, and one more implementation of each service's protocol. It stays available
for a service that already has such a client.

**Rejected: RPyC's server side in Rust, or an FFI layer under a Python class.** `hosted-objects.md`
named both as what a trait would mean; neither is. A service is a described set of operations
called by value, which is a service model and not an object graph: no proxies, no references into
Rust, no attribute reads crossing the pipe.

**Rejected: results carrying further service objects.** A nested object needs a reference into
Rust and a lifetime for it; values need neither, and a service that wants to hand out a thing
hands out an id and a method that takes it. Recorded as `0003-44`'s fork 1, recommended values
only.

**Rejected: an async surface.** `mcp-wrappers.md` ("Synchronous, as a hosted object is") decided
one rule for both kinds of object, and `hosted-objects.md` rejected the awaited facade; the
synchronous spelling with `asyncio.to_thread` for a long call is kept.

**Rejected: checking arguments in the interpreter.** The check is enforcement, and "Nothing on the
container side is trusted" (`boundary-policy.md`); the interpreter may also check, for a better
error, and the host decides.

**Rejected: two event families for two surfaces.** One call family with a `kind` lets an owner
read every service call with one subscription and `diagnostics.md`'s correlation fields once; the
MCP lifecycle events are the exception, for the lifecycle a Rust service does not have.

**Rejected: a schema-less call.** A `call(method, Value)` with no description would give the model
`(*args, **kwargs)` and `help()` nothing; the description is what makes `discovery.md`'s `help()`
answer, and OR-A3-1 requires the check.

## Open questions

- Whether a description may change for a Rust service as it may for an MCP one: the mechanism
  carries a generation and a `service.described` line, so a service that calls a `redescribe`
  hook the builder returns could use it; `0003-44` builds the line for MCP's relisting and leaves
  the Rust-side hook to a later task unless its fork 5 takes it.
- Whether an application may supply a presenter class from its own provisioned code for a
  service it binds, named by `presenter`, as OutRig's kinds do. Not built here.
- Whether `runtime.bindings` is renamed once services and plain objects are listed under it
  (`facilities.md`, "Open questions").
- Python-host parity: a host program (`0003-29`) binds a Python object with `inject`
  (`0003-36`) and has no Rust object to serve; whether `outrig.Session` should accept a Python
  object described and called through this mechanism rather than through RPyC is an open choice
  the A-series leaves open, not a requirement.

## Unverified

- Which JSON Schema validator crate compiles draft 2020-12 and reports a failing instance path:
  `jsonschema` and `boon` are candidates, neither is in the workspace lock (`Cargo.lock` holds
  `schemars` and no validator), and `0003-44` chooses with the official test-suite results each
  publishes, per the project's rule to survey before naming a dependency.
- That aborting the dispatch task at the deadline frees the table's record without waiting on a
  `call` that blocks a thread: a `tokio::task::JoinHandle::abort` cancels at the next await point
  and cannot interrupt a blocking section, which is why the page says the abort frees OutRig and
  not that thread. Read from tokio's documentation; `0003-44` tests a `call` that sleeps on a
  blocking thread and one that awaits.
- That a generated class with a few hundred methods instantiated once per kernel costs nothing a
  kernel's build notices; `0003-44` measures a 500-method description.
- That the schema renderer's coverage -- `properties`, `required`, `default`, `description`,
  `type`, `enum`, `$ref` into `$defs`, `anyOf` -- is enough for the first application's methods
  and for the MCP servers in `tests/fixtures/`; anything beyond falls back to `(**arguments)`.
