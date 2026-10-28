# 0003-51 -- An external channel has a backend

## Context

Every channel an agent holds today has one of two far ends. The primary's `user` channel ends on
the host, in `UserChannel` (`crates/outrig/src/agent/channel.rs:173-292`): the owner's `send`
posts a `msg` line the interpreter queues (`Interpreter::posting`,
`crates/outrig/src/python/host.rs:1100-1140`; `_msg`, `crates/outrig/src/python/interpreter.py:
6309-6336`), the agent's `send` arrives as a `send` line the host routes to the one outbox it has,
and every other channel's send is refused as "nothing on the host reads this channel"
(`host.rs:2232-2262`). A child's channels end in its parent's kernel and never reach the host
(`messages.md`; `agent-placement.md`). The interpreter's own comment names the generalization
point: "Today the other end of every channel is the host: an agent's `user` channel"
(`interpreter.py:3453-3455`). What exists and is kept: `Endpoint` with its bounded queue, its
delivered count, `pending()`, `receive()` and `send()` (`:3582-3783`; `QUEUE_MAX`, `MESSAGE_MAX`
and `SEND_WINDOW` at `:3466-3468`); `Delivery` with `id`, `body`, `sender`, `received_at` and
`skill` (`:3500-3515`); `runtime.wait` watching every endpoint and taking handles through their
futures (`:4036-4130`); the `Announcer` that tells the model what waits by name and count
(`channel.rs:42-134`); the `pending` and `received` lines (`:6489-6493`, `:6339-6349`); and the
`took` line, sent only once the host has asked to observe (`:3651-3654`, `:6478-6486`). Channel
sets are fixed when a kernel is built (`Kernel.__init__`, `:5384-5397`). The owner's inputs are
fixed on `SessionBuilder` before the start (`crates/outrig/src/harness/builder.rs:75-117`), and
`SkillSource` is the template for one the owner implements: synchronous methods on a thread where
blocking is fine, within a bound and a concurrency cap (`crates/outrig/src/harness/skills.rs:
64-76`, `:98-102`). `Session::user_channel` hands the owner a cheap, cloneable end of a channel
(`crates/outrig/src/harness/session.rs:246-248`). A host program's owner protocol carries requests
from the program and pushes from the engine, and no request from the engine
(`crates/outrig/src/owner/mod.rs:16-28`; `owner/connection.rs:141-242`), reading lines of at most
64 KiB (`:35`).

OR-CH1 asks for an external-channel abstraction with an application-supplied definition and
backend: OutRig owns local queues, arrivals and announcements, sending, receiving, wake and wait
integration and runtime lifecycle; the application owns routing, durable storage, identities and
authority; OutRig must not learn the job schema; definitions describe kinds, contracts, bounds and
wake policy; native waiting does not consume; queue ingestion is distinct from agent receipt;
channel discovery follows supplied facilities; push against poll is inside the integration. A-07
makes it concrete for a Rust embedder, the first application (`A-00-index.md`, decision 1):
OR-A7-1, a backend trait registered with the definitions, called for every outbound item with its
metadata and a stable item id OutRig mints, answering accepted with an opaque reference, known
not accepted with a reason, or uncertain, with `send` returning on accepted and raising typed
exceptions otherwise, and the agent unable to reach the backend by any name; OR-A7-2, inbound
delivery as an owner push naming the channel, with item id, sender, kind, body, correlation id
and backend reference, the consumed position reported when the agent receives and never when
queued, and redelivery by item id; OR-A7-3, one uniform user channel for initial and later input,
empty initial input delivering nothing, the orientation apart from it with a documented order;
OR-A7-4, wake policy per channel enforced by OutRig, and `runtime.wait` over a child handle and an
external channel as one wait. A-02 fixes who the primary is: the application's primary is an
owner-constructed root in a `no_primary` Session, driven by its own runner, whose constructor dict
is how a channel reaches it (`rust-owner.md`; `0003-46`).

The validation (`external-channels.md`, "Reserved ground") classified this as a deferral the layer
reserved -- `messages.md`: "the rest of the layer stays design" -- and found the generalization
point above. Two recorded decisions stand and are cited, not reopened: the direction of a request
channel between parent and child is fixed (`agent-classes.md`, "Explicit plumbing"), and children
have no `user` channel (`messages.md`, "The user channel"). The wording fixes it carries: one
definition and one backend mechanism serve two kinds, and the definition declares its kind; a body
the backend owns crosses as `str` or untyped JSON, and a body the definition types is checked
against the serializable subset. Constraints it carries: the local queue is bounded and refuses
rather than waits, so it is a window the owner paces (constraint 5 of the validation: anything
the agent waits on must return promptly and leave the waiting to `runtime.wait`); the stream is
lossy, so positions are state the host keeps, not events (constraint 6); children have no `user`
channel, and a child is addressed by the owner only through a channel it was given (constraint 9).
This task builds the plain kind end to end for a Rust owner; `0003-52` adds the request kind and
`0003-53` resolution by identity and the registry's positions.

## Goal

A Rust owner declares a channel with a definition and a backend, an item the owner pushes becomes
a pending delivery on the holder's channel that wakes or announces as the definition says, a send
goes through the backend and returns accepted or raises why not, the agent's receipt is reported
to the backend per item, and the primary's initial and later input arrive on one such channel.

## Deliverables

- **The definition.** A channel's name; its kind, `plain` here and `request` in `0003-52`; its
  contracts per direction, each a set of types in the serializable subset, `str`, untyped JSON,
  or empty; its bound, default and ceiling 256; its wake policy, `wake`, `announce` or `silent`
  (A-07's wake, count-only and neither); its description; and its backend. `user`, `runtime`,
  `asyncio`, a non-identifier and a duplicate are refused when the builder checks or starts, as
  `0003-20` refuses a binding's name. A typed contract names its type by import path into the
  application code, `module:Name` (fork 7), checked in the kernel by `outrig.schema`; the owner
  sends and receives JSON the type's schema text types. Spellings --
  `SessionBuilder::channel(definition, backend)` and the definition's type -- are this task's.
- **The backend trait.** Given with the definition; `SkillSource`'s calling convention
  (fork 2): synchronous methods on a blocking thread, a per-call bound, bounded concurrency. This
  task's methods: `send(outbound item)`, called with the item's direction, kind, type, sender,
  correlation id, the item id OutRig minted and any reference already known, answering accepted
  with the backend's opaque reference, known not accepted with a reason, or uncertain; and
  `consumed(channel, item id)`. A call that does not answer within its bound is uncertain.
  `0003-52` and `0003-53` add theirs. An in-memory test backend lives in the tests. The backend is
  reachable from no kernel: not a binding, not a global, not in `runtime.bindings`.
- **The owner's push.** A channel handle from `Session::channel(name)` (fork 3), cheap to clone
  and `Send + 'static` as `UserChannel` is: `deliver(item)`, where an item carries the owner's
  item id, the sender, the kind (a message here; a request in `0003-52`), the body, a correlation
  id and the backend's reference; answered once the item is queued in the holder's window, with
  OutRig's delivery id, or as a repeat -- an item id the channel has already taken, answered with
  the existing delivery id and position, nothing queued -- or refused with `full`, `contract` and
  the problems, `unknown channel` or `closed`; `close(reason)`, after which the
  endpoint is failed as `messages.md`, "Failure", has it -- `send` and `receive` raise,
  `runtime.wait` reports it once -- which is how the owner reports a peer's termination; and
  `room()`, a wait that resolves when the window has room. `deliver` never blocks on the agent:
  the answer comes from the interpreter's `msg` reply, within the host's own bound.
- **The host end.** A table of (holder, channel) to far end, generalizing the one outbox
  `host.rs:2232-2262` consults: a `send` line from an external endpoint goes to its backend, and
  the `received` acknowledgement that frees the window (`host.rs:1903-1907`) is sent when the
  backend's `send` has answered. `Interpreter::post` on the holder's handle delivers pushed items,
  with `sender` in the `msg` line, which `Delivery.sender` carries. Each send is recorded with the
  holder's round it was made in, for the owner's per-round read (`rust-owner.md`; OR-A6-2). The
  registry's first form: each channel's descriptor, holder and taken item ids, which `0003-53`
  extends with positions.
- **The endpoint in the kernel.** An external endpoint built at kernel build for the holder, with
  `receive()`, `send()` and `pending()` as `Endpoint` has them. `send` writes its line under an
  id and awaits the host's answer on the kernel's loop, as `Kernel.asks` waits for answers
  (`interpreter.py:5375`): accepted returns; refused, not accepted and uncertain each raise a
  typed exception, the last carrying the item id (spellings this task's; `0003-53` adds the query
  before the raise and `resolve`). `receive()` sends the consumed notice unconditionally -- the
  `took` line made unconditional for external endpoints, or a line of its own (fork 6) -- and the
  host forwards it to the backend's `consumed` with the owner's item id; nothing is reported when
  an item is queued. A typed body is checked against the schema as JSON on arrival, with no code
  of the agent's run, and constructed into the declared class at `receive()` on the agent's loop
  (fork 5); an untyped body is `str` or the JSON value as decoded; `MESSAGE_MAX` holds.
- **Attachment.** A channel's facility reference (`facilities.md`; `0003-42`) resolves to the
  endpoint in the one kernel the channel is attached to and raises elsewhere, naming the holder. A
  dict naming a channel attaches it to the kernel built from that dict, which is how the
  application's primary holds its channels (`0003-46`); a dict naming a channel already attached
  fails the launch, surfacing at readiness and the first handle as every launch failure does
  (`agent-classes.md`, "Construction and readiness"); a dict-less child of the holder inherits the
  holder's other facilities and never its channel references (`facilities.md`), so the refusal
  meets only a dict that names the channel. A declared channel no dict names is the
  primary's when the session has a primary the harness built. In a session with no primary, every
  declared channel's window exists from session start on the host, so a push before any kernel
  holds the channel queues and is answered with a delivery id, and the kernel a dict names takes
  the window as it stands; a channel no dict ever names holds its items until the close, which
  reports them unread, and the handle's `holder()` reads none (fork 4).
- **A directive on a plain channel.** The push's item may carry `skill`, a name in the catalog,
  checked at the push and refused `unknown skill` when it is not; the delivery reaches the holder
  with `Delivery.skill` set as `0003-33` sets it for the user channel, the holder runs it through
  its own code as the primary does, and `skill.directive.received` records it with the channel
  (`external-channels.md`, "A directive crosses as an item"). A request carries none.
- **Wake policy.** `_watch` reports unread input to `runtime.wait` only for `wake` channels; the
  announcer counts `wake` and `announce` channels and never `silent` ones, in `Interpreter::
  pending`'s answer (`host.rs:1147-1149`) and in the opening and result-prefix texts
  (`agent/tool.rs:147-160`; `channel.rs:95-134`). For a runner-driven holder -- the application's
  primary, or a child -- what the runner compares at a round's end (`crates/outrig/src/agent/
  runner.rs:64`, `:100-125`) gains the delivered and unread counts of its `wake` and `announce`
  plain channels, compared as the `Announcer` compares counts, so an arrival opens a round for an
  idle holder, on an `announce` channel too (the departure from A-07's count-only the page
  records), and an unread item earns one round at most. Nothing warns about or cancels a plain
  item. `runtime.wait` over a child's handle and an external channel stays one wait: nothing is
  added for it, and the acceptance runs it.
- **The uniform user channel** (OR-A7-3). The owner pushes the primary's initial input through
  the channel handle as it pushes any later input, under the same envelope; with empty initial
  input nothing is pushed and nothing is announced or invented; no first-message prompt is
  composed, and the instructions carry no item's body. The order of the docstring, the
  contributions and the first delivery is `external-channels.md`'s, "Initial input is a
  delivery": the instructions whole, then the orienting round's opening line announcing what
  waits (fork 8), recorded in this task's `## Decisions` beside `0003-47`'s.
- **The manifest.** The holder's instructions carry, per channel it holds: name, kind, contract
  text per direction, description, what the wake policy means in the agent's terms, and that a
  message on a plain channel owes no reply while a request does. Added to `preamble` for the
  primary and `child_preamble` for a runner-driven holder (`crates/outrig/src/agent/orientation.rs:
  66-84`, `:213-241`); composed by `0003-47` once it lands.
- **Events**, in `doc/reference/events.md`: `message.sent`, `message.received` and
  `message.refused` generalized, with `from` and `to` naming the backend's end and a `sender`
  field; `channel.attached` (execution diagnostics); `channel.consumed`, `channel.closed` and
  `channel.item.outcome` -- `accepted`, `refused`, `not_accepted` or `uncertain`, with the reason
  and the reference (integration audit). Each carries the channel and the item's ids.
- **The close**, in `lifecycle.md`'s table, tested here: unread items in the window at the close
  are left, not consumed, and listed in the report by item id; a `send` not yet handed to the
  backend is refused `SessionClosing`; one the backend holds drains within the backend's bound, and
  its outcome is the backend's answer; a consumed notice the backend has not acknowledged is
  retried through the drain and listed in the report if still unacknowledged. `ShutdownReport`
  gains the entries.
- **A Python host program's backend is not built** (fork 9). The constraint found for it is
  recorded here so whoever builds it meets it: the engine reads lines of at most 64 KiB from the
  program (`owner/connection.rs:35`) while a channel item may be 1 MiB (`interpreter.py:3467`), so
  a host program's push would need that bound raised for channel lines or the body carried in
  parts; the Rust push crosses no line and has no such bound.
- `crates/outrig/CHANGELOG.md`'s `[Unreleased]` says what an external channel is; every new
  public item has rustdoc and `crates/outrig/README.md`'s embedding example declares one channel;
  the documentation task that closes this work writes the page.
  `crates/outrig/public-api.txt` regenerated.

## Acceptance

Each is a mock-driven test unless it says otherwise.

- **A pushed item becomes a pending delivery.** A Rust session whose application primary
  (`0003-46`) holds a plain channel `inbox`, `str` both ways, policy `wake`, through its dict: the
  owner's `deliver` with item id `"m-1"`, sender `"alice"` and body `"hello"` answers with the
  delivery id; the root's runner opens a round whose opening reads `1 message is waiting on
  runtime.channels["inbox"]`; the mock's code receives it, and `d.body == "hello"`,
  `d.sender == "alice"` and `d.id` is the delivery id; the backend's `consumed` is called with
  `inbox` and `"m-1"` only then; `message.sent`, `message.received` and `channel.consumed` carry
  the ids.
- **Waiting leaves it unread, and queued is not consumed.** A mock that calls `runtime.wait`
  while an item waits gets `MessageAvailable` naming `inbox`; `pending()` is 1 before and after;
  the backend hears no `consumed` until `receive()`.
- **One wait** (OR-A7-4). A mock waits with `runtime.wait({child_handle})` while a child works:
  an arrival on `inbox` first raises `MessageAvailable` with the handle still pending; the child's
  reply first returns `(done, pending)` with the handle in `done` and no exception.
- **A repeat is a no-op.** `deliver` of item `"m-1"` again, before and after the agent received
  it, answers as a repeat with the same delivery id; `pending()` does not change; the agent sees
  one delivery; the backend hears one `consumed`.
- **A send uses the backend, and the three answers are told apart.** `await runtime.channels[
  "inbox"].send("hi")` calls the backend's `send` with body `"hi"` and the item's id, and returns
  once it answers accepted; a backend that answers not accepted makes `send` raise the
  not-accepted exception whose message carries the backend's reason and says a retry is safe; one
  that answers uncertain, or never answers within the bound, makes `send` raise the uncertain
  exception carrying the item id; `send(1)` raises `TypeError` and a body past `MESSAGE_MAX`
  raises `ValueError`, neither reaching the backend; with 16 sends unanswered the 17th waits until
  one is answered. `channel.item.outcome` records each outcome.
- **The agent cannot reach the backend.** In the holder's kernel, `runtime.bindings` lists no
  backend, the kernel's globals and `runtime.names()` hold nothing of it, and `dir(runtime.
  channels["inbox"])` exposes no object of the owner's.
- **The window.** With `bound = 2` and two items unread, `deliver` is refused `full`; after one
  `receive()` it is accepted; `room()` resolves then and not before. An item outside the contract
  is refused `contract`, naming the problem, and nothing is queued. A push into a declared
  channel of a no-primary session before any kernel holds it is answered with a delivery id and
  waits in the window, whether the root is then constructed by the builder form or by
  `Session::construct`, and is delivered at the orienting round; `holder()` reads none before and
  the root's id after; a channel no dict names is listed at the close with its items unread.
- **A directive.** A push with `skill: "review"` on `inbox` reaches the mock as a delivery whose
  `skill` is `review` and `skill.directive.received` names the channel; one naming a skill not in
  the catalog is refused `unknown skill` and nothing is queued; a request carrying `skill` is
  refused `contract`.
- **A typed body.** A contract naming `app.messages:Note` in the provisioned application code:
  JSON that fits is received as a `Note`; JSON missing a field is refused with `$.field` named;
  a `__post_init__` that records the thread it ran on shows the agent's loop, never the reader
  thread.
- **Wake policy.** On an `announce` channel an arrival during a mock's `runtime.wait` does not
  end the wait, the next tool result's prefix announces it, and an idle holder gets a round whose
  opening names it. On a `silent` channel nothing is announced and no round opens; `pending()`
  shows it. The announcer's counts leave `silent` channels out. The round an
  `announce` arrival opens for an idle holder is the departure from A-07's count-only the page
  records.
- **One holder.** The root built with `{"inbox": <reference>}` has `runtime.channels["inbox"]`
  and a manifest entry for it; a child it constructs with `{"files": ...}` has no `inbox` in
  `runtime.channels` and no channel in its instructions; a child constructed with the same
  reference fails at readiness naming the root as the holder. A session with a harness-built
  primary and a declared channel no dict names gives it to the primary.
- **A runner-driven holder.** An arrival on a `wake` channel of the idle root opens a round whose
  opening names the channel and count; an item left unread earns no second round; a round in
  which the mock receives it ends with nothing further opened; the root is never warned about a
  plain item and nothing is cancelled.
- **Peer termination.** `close("peer gone")` makes the mock's `receive()` and `send()` raise;
  `runtime.wait` reports the failure once and not on a later wait; a later `send` raises;
  `channel.closed` is recorded.
- **The uniform user channel.** A root built with no initial input runs its orienting round with
  no announcement and instructions holding no item body; one whose owner pushed `"What does this
  project do?"` before the kernel existed opens the orienting round with its instructions whole
  and the opening line `1 message is waiting on runtime.channels["inbox"]`, with `wake` ending a
  `runtime.wait` in that round; a later push is announced the same way, with the same envelope.
- **The manifest.** The holder's instructions name each channel it holds with its description and
  its wake sentence, say that a message owes no reply and a request does, and contain no body of
  any item.
- **The close rows.** Shutdown with two unread items, one `send` the test backend holds unanswered
  and one `consumed` notice it has not acknowledged: the report lists the two items by item id,
  the send ends with the backend's answer within the drain or is listed uncertain, and the notice
  is listed as unacknowledged; nothing is consumed by the close.
- e2e, with a real model, gated as `0003-35`'s is: a Rust embedder constructs the application's
  primary with an `inbox` channel (`0003-46`), the test backend's owner pushes "What does this
  project do?", the model's code sends its answer on the channel, the backend's `send` receives
  it, and the session's report is clean.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`, `cargo clippy
  --all-targets`, `cargo fmt --check` pass; `doc/reference/events.md` has the new events, and
  `lifecycle.md`'s close table the new rows, checked by reading.

## Design forks

1. **Wake defaults -- Recommended: `wake` for a plain channel.** It is the `user` channel's
   behavior, and a definition that wants a quiet feed says `announce` or `silent`. The alternative
   defaults plain channels to `announce`, so no arrival ends a wait unless asked; it trades the
   redirection `messages.md` calls "input wins" for quiet, on every channel, by default.
2. **The backend trait's calling convention -- Recommended: asynchronous methods, each bounded by
   `SkillSource`'s timeout and concurrency cap.** The first consumer's backend is tonic clients on a
   tokio runtime and the harness is tokio already, so a synchronous trait would make every call a
   `Handle::block_on` inside a blocking thread; parity with `SkillSource`'s synchronous convention
   was the only argument for it. A bound is still what turns silence into a reported outcome. The
   alternative is synchronous methods on a blocking thread, which suit a backend that is a blocking
   store and which an adapter over the async trait can offer later.
3. **Where the owner's push lives -- Recommended: a channel handle from `Session::channel(name)`**,
   cheap to clone and `Send + 'static`, as `UserChannel` is handed out, so the owner's delivery
   code need not hold the `Session`. The alternative is a method on `Session` taking the channel's
   name, one fewer type and one more thing every pusher must hold.
4. **A declared channel no dict names, in a session with no primary -- Recommended: its window
   exists from session start and holds pushes until a dict names it**, so a root constructed later
   by either form takes what was pushed, and the application's primary has its initial input waiting
   when its orienting round opens with no retry on the owner's side (OR-A7-3). The alternatives
   refuse a push into an unheld channel, which makes the owner retain and retry initial input it
   pushed before constructing, or fail the start, which refuses a session whose channels are meant
   for a child.
5. **Where a typed body is decoded -- Recommended: the structural check on arrival, construction
   at `receive()` on the agent's loop**, for the reason `_deliver` gives (`interpreter.py:
   3759-3762`). The alternative constructs on arrival, which runs the class's code on the reader
   thread.
6. **The consumed notice -- Open; recommended: a line of its own, part of delivery**, leaving the
   `took` line what it is, an observation sent when asked. The alternative makes `took`
   unconditional for external endpoints, one line fewer and one condition more.
7. **How a Rust owner declares a typed contract -- Recommended: by import path into the
   application code, `module:Name`**, so the one declaration of the type is Python's and the kernel
   checks it with `outrig.schema` as it checks every contract; the owner handles JSON the schema
   text types. The alternative declares the type from Rust in a description of its own, the form
   `0003-35` crosses from a host program, which needs that form first and declares the type twice.
8. **The opening order -- Recommended: the instructions whole, then the orienting round's opening
   line announcing the first delivery**, as the primary's opening announces what waits, so the
   model has its instructions and the announcement together and the delivery is never a body in
   the instructions. The alternative lets the orienting round end and opens a round on the
   arrival, which spends a round to tell the model what the opening line could have.
9. **A Python host program's backend -- Open; not built here.** `A-00-index.md` makes Python-host
   parity for the new operations an open choice. If taken later: a Python object per definition
   given to `outrig.Session`, called by the engine over the owner connection, which needs
   engine-initiated requests the protocol does not carry today (`owner/connection.rs:141-242`) and
   the 64 KiB line bound raised for channel lines. Nothing here depends on it.

## Dependencies

- **Hard: `0003-39`.** The interpreter program off its argument cap; this task adds endpoint code.
- **Hard: `0003-42`.** The facility dict and the facility reference a channel's reference is one of.
- **Hard: `0003-46`.** The application's primary: the owner-constructed root in a `no_primary`
  Session, whose constructor dict is how a channel reaches it, and whose runner drives it.
- **Soft: `0003-43`.** The provisioned application code a typed contract names its type in.
- **Soft: `0003-47`.** The manifest becomes one contribution to composed instructions, and the
  opening order is recorded beside its decisions.
- **Soft: `0003-35`.** Only if fork 9 is ever taken: the owner connection a host program's
  backend would be called over.

## See also

- `plan/phase/0003-python/external-channels.md` -- "The shape", "One definition, two
  kinds", "Bodies", "The local queue is a window", "Wake policy", "Who holds a channel",
  "Sending", "Initial input is a delivery", "The owner's side", "What the agent is told".
- `plan/phase/0003-python/rust-owner.md` -- the application's primary, its control handle
  and the per-round read the registry serves.
- `plan/phase/0003-python/messages.md` -- channels, contracts, delivery rules, failure, waiting.
- `plan/phase/0003-python/embedding.md` -- "What the embedder supplies", which a backend joins.
- `plan/phase/0003-python/lifecycle.md` -- "The close table".
- `plan/phase/0003-python/facilities.md` -- the facility reference a channel reaches an
  agent through.
- `requirements/outrig/A-07-owner-side-channel-backend.md` -- OR-A7-1 through OR-A7-4.
- `crates/outrig/src/python/interpreter.py` (`Endpoint`, `_deliver`, `runtime.wait`, `_msg`,
  `_pending`), `crates/outrig/src/python/host.rs` (`posting`, `acknowledge`, `Reply::Sent`),
  `crates/outrig/src/agent/channel.rs`, `runner.rs`, `orientation.rs`,
  `crates/outrig/src/harness/builder.rs`, `session.rs`, `skills.rs`.
