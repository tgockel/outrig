# The Rust owner

A **Rust owner** is a program that embeds `outrig::harness`, builds a session with no primary
agent, and constructs a task agent in a kernel of it (`task-agents.md`; the 0003 README's
"owner"). The first application is one: a Rust program with no Python host program, whose
facilities reach agent Python as services (`services.md`) and whose Python package is application
code (`application-code.md`). This page decides what such an owner holds and reads: the
construction operation, the **control handle** -- readiness, release and the authoritative reads
-- the **stop read**, the per-round facts, and the **roster**. `0003-46` builds construction and
the handle's readiness and release; `0003-48` the stop read, the per-round facts and the
distinction between retrying and broken; `0003-49` the roster; `0003-50` adds the state a
model-request limit puts an agent in (`model-policy.md`); `0003-60` reads the roster for the
assessment (`continuation.md`). Parity for the Python host program of `0003-35` is an open choice
recorded as `0003-46`'s fork, not a deliverable.

## The shape

What the owner already has, and keeps. `SessionBuilder` takes a `LaunchSpec`, `Secrets`,
`bindings_env`, bindings, skill sources, subscriptions and `no_primary`
(`crates/outrig/public-api.txt:1833-1847`). A session with no primary agent "resolves no model,
and asks the secret resolver for nothing, as it starts", and "everything else -- its container,
interpreter and bindings, its admission, its events and its report -- is the session's, as it is
with one" (`crates/outrig/src/harness/builder.rs:160-174`). `Session` offers `close_admission`,
`closer`, `shutdown`, `round`, `interrupter`, `stop_held`, `user_channel`, `id`, `model`,
`python_version` and `container_name` (`public-api.txt:1817-1830`); `round()` answers `NoPrimary`
for such a session (`crates/outrig/src/harness/session.rs:350-353`), and the interrupter and the
user channel are `None`. The children's registry keeps every agent of the tree -- the primary's
node at depth 1 through `Children::root`, and each child's node with its `Life`, the round it is
in, its model and its requests (`crates/outrig/src/agent/children.rs:82-145`, `:251-264`) -- and
each child's runner drives its rounds from it (`crates/outrig/src/agent/runner.rs`). Nothing of
that is readable by an owner today: the registry tells a child's parent kernel what the child is
doing (`tell_life`, `children.rs:1175-1194`), and the report says how open requests ended
(`crates/outrig/src/harness/report.rs:19-37`).

```text
  RUST OWNER, one process                                  CONTAINER
  +--------------------------------------------------+    +------------------------------+
  | Config in memory    Secrets    LaunchSpec         |    | interpreter                  |
  | application code  ---- provisioned (0003-43) ---> |    |   /outrig/app/<digest>       |
  | services, Rust    ---- bound by name (0003-44) -> |    |   kernel c1: the root agent  |
  | channel backends  ---- defined (0003-51) -------> |    |     self   = the instance    |
  | construct(class, args, model)   (0003-46)         |    |     task is self._task       |
  |   -> control handle: ready(), release(),          | <- |   its own runner opens       |
  |      status()/next_stop(), rounds()  (0003-48)    |    |   its rounds                 |
  | Session::roster()               (0003-49)         |    +------------------------------+
  +--------------------------------------------------+
```

The instance is a root agent -- a child whose parent is the owner, in a session with no primary
(the 0003 README's vocabulary; `agent-classes.md`, "From the host program") -- which the
application calls its primary. Its rounds open through its own runner: the orienting round as it
launches, then a round on each arrival (`agent-classes.md`, "The child's rounds";
`external-channels.md`, "Initial input is a delivery"). The owner never drives one:
`Session::round()` stays `NoPrimary`, and the application builds no loop around the instance
(CC-AGT-10). What the owner does is construct, hold the handle, deliver into the channels it
defined and receive from them, read, and end.

## Construction from Rust

`task-agents.md`, "The owner-constructed root", decides the construction; this page says what a
Rust owner passes and gets. The operation takes a class reference into the application code
(`module:qualname`), constructor arguments, and a model, and `0003-46` offers it in two forms,
its fork 1: on the builder, so the instance is constructed as the session starts, after
provisioning, and `check()` resolves its model and every `${VAR}` before any container exists
(OR-A2-3; `builder.rs:312-319`); and on the started `Session`, for a root constructed later.

- **Arguments** are data in the serializable subset (`messages.md`), as JSON, each optionally
  named with the provisioned dataclass it decodes into strictly; or **facility bindings named by
  their bound names** (OR-A1-2): a service `0003-44` bound, a binding, a channel `0003-51`
  defined, each as `{kind, name}`, singly or as a dict of them (`facilities.md`, "A facility
  reference"). No Python object of the owner's crosses, no pickle, no reference into the owner's
  process; a value that is neither data nor a facility is refused naming the argument.
- **Aliases** name arguments to bind as globals in the kernel beside `self`, the same object the
  argument decoded to, so `task is self._task` holds when `__init__` stored it unchanged
  (OR-A1-4; `0003-46`'s fork 3 decides whether any is installed by default).
- **The model** is a configured model or alias from the in-memory `Config` the embedder gave
  `SessionBuilder::new`, resolved as a child's is (`children.rs:518-532`); `[agents.<name>]`,
  `default-agent` and a profile's image or preamble never apply (`0003-35`'s R7), and `make`
  drops the preamble already (`:532`).
- **A class with no `@outrig.agent` channel is constructed** when the owner constructs it as a
  root (OR-A1-1): the refusal that an instance "would answer nothing"
  (`crates/outrig/src/python/outrig/_agents.py:533-535`) is for a child a kernel constructs,
  whose only way to be given work is a request; a root is given work by its channels and its
  task. `0003-46`'s fork 2 decides whether the exception is a construction-site flag or a
  subclass marker.

What comes back is the control handle. What never comes back is the Python object: ordinary
method calls from the owner and any transfer of `__dict__` are not offered, since the object
lives in the kernel and the boundary between the processes is a real one.

## The control handle

The handle is what a Rust owner holds for an instance it constructed (brief vocabulary). It has:

- **Readiness.** `ready()` resolves once the orienting round has ended, and fails with what the
  launch met, as `agent-classes.md`, "Construction and readiness", has `ready()` report it: the
  constructor's or the hook's exception, rendered with a bounded traceback; an unknown model with
  the configured names; `children-max`; closed admission. A failure spends no model round and
  leaves the session clean: the kernel is ended, its place freed, and the report of the session's
  shutdown is `Clean` (`0003-46`).
- **Release.** `release()` ends the instance and settles its open requests `AgentReleased`, as
  `agent-classes.md`, "Release", has it; a release while the instance is still launching waits
  for the construction's answer and then releases, as `_Instance._release` waits for the launch
  task today (`crates/outrig/src/python/interpreter.py:5216-5218`), and `ready()` then raises
  `AgentReleased`. The session's exit releases a live instance through the close rows
  (`lifecycle.md`, "The close table", **Root agents**).
- **Requests**, when the class declares channels (OR-A2-5): `channels()` lists each channel with
  its request and reply schema text; `request(channel, json)` sends and returns a request handle
  whose outcome is the reply as JSON typed by that schema text, or `work.md`'s documented error,
  one terminal outcome per request. The kernel checks the reply against the declared type as a
  parent kernel does today (`agent-classes.md`, "Calling"), and the embedder deserializes with
  serde. An application verdict through `0003-35`'s acceptance handshake is `0003-46`'s fork 5.
  A class with no channel has no `request`, and construction is the same.
- **The reads**: the stop read and the per-round facts (`0003-48`), below; the session's roster
  (`0003-49`) is on `Session`, since it covers every agent.

The handle is reachable from the `Session` by the instance's id under `0003-46`'s fork 4, since
the roster names ids.

## The stop read

The owner must act on why an instance is at rest -- park it, hold a claim, fail a task, wait --
and cannot do so from the stream: a subscription is bounded and lossy by design, and a missing
event proves nothing (`observability.md`, "Subscription becomes public; the file keeps its
rules"; `doc/reference/events.md`, "Durability and loss"). The requirement is narrow by design
(A-06; the planning's `decisions.md`, section 4, rejects generic stop-cause machinery and an
application-specific enum): a small generic set, read authoritatively.

**The read** is a status with a change sequence, and an awaitable over it (`0003-48`'s fork 1):
`status()` returns the instance's current state and a sequence number; `changed(seq)` waits
until the sequence advances; `next_stop()` waits for the next state at rest. The states:

| State                | Meaning                                                   | From         |
| -------------------- | --------------------------------------------------------- | ------------ |
| launching            | constructed; the orienting round has not ended            | `0003-46`    |
| running              | a round is open, numbered                                 | `0003-48`    |
| unavailable          | the last round failed on a failure that may clear: the    | `0003-48`    |
|                      | provider's class, since when, and the next attempt        |              |
| idle                 | the last round ended with nothing outstanding             | `0003-37`    |
| limit reached        | the agent's model-request limit stopped it, resumably     | `0003-50`    |
| refused              | the provider refused on content policy                    | `0003-48`    |
| failed               | the last round failed on a failure that does not clear    | `0003-48`    |
| released             | the instance was released                                 | `0003-46`    |
| cancelled            | the launch was given up before its first round            | `0003-46`    |
| terminated           | the interpreter died under it                             | `0003-48`    |

Each state at rest carries the round it belongs to and the provider attempt that decided it,
where one did (`model.attempt`'s id; `doc/reference/events.md`), so the owner can find the
round's events when it wants the detail. Task completion is not in the set: it is the
application's explicit operation through a service it bound (CC-MSG-7; `services.md`), and
nothing OutRig reads infers it. A spent token budget stays what `0003-30` made it, an error
that ends requests and makes `ready()` raise `BudgetExhausted`; the model-request limit is the
resumable stop, and `model-policy.md` has the difference.

**Where it comes from.** The runner holds, per child, the baseline summary, whether its model is
broken, and the backoff (`runner.rs:64-69`); the registry holds each agent's `Life`, the round it
is in, and its requests, under one lock (`children.rs:78`, `:114-125`, `:1130-1173`). `0003-48`
puts what the read needs into that state, under that lock, as the runner moves: the round's
`Finished` when a round ends, which the runner discards today (`runner.rs:187-196`; `Finished`,
`crates/outrig/src/agent/round.rs:41-50`); the failure's class and the next attempt when a round
fails and the runner backs off (`:166-182`, `:198-209`); the terminal failure when the runner
gives up (`:212-216`); and the rest when the runner waits (`:95`, `:119`). A `watch` beside the
lock carries the sequence. The reads are served from that state and nowhere else, so they cannot
disagree with what the agent is doing; events stay the diagnostic surface, and dropping every
subscription changes no read.

## Retrying is distinguishable from broken

Two layers retry. Within one model call, the HTTP loop retries a transient failure and the chain
fails over, each evented (`model.retry`, `model.failover`;
`crates/outrig/src/harness/event.rs:575-592`); `is_recoverable` is the predicate both share, so
"what a chain calls recoverable is exactly what was retried"
(`crates/outrig/src/agent/retry.rs:1213-1246`). When that call fails for good, the
round fails with `AgentError::Prompt` for a failure that may clear or `PromptTerminal` for one
that does not, and the two after-work forms (`crates/outrig/src/agent/mod.rs:390-408`). Then the
runner's layer: on `Prompt` it waits two seconds, doubling to sixty, and opens the round again
with the same opening (`RETRY_FIRST`, `RETRY_MAX`, `back_off`, `runner.rs:48-50`, `:222-225`);
on a terminal failure it marks the child broken and ends what it holds (`:66`, `:212-216`). The
second layer is `tracing::warn!` today (`:174`, `:204`), which the planning's
`stop-events-clarification.md`, section 3, called a possible diagnostic enhancement; A-06 needs
it as a read, because an owner that cannot tell retrying from broken either kills live work or
waits on dead work.

So the read says **unavailable** while the runner will try again -- the provider's failure class
as `is_recoverable` and the attempt's label classify it, when the first failure of this run of
failures happened, and when the next attempt is due -- and **failed** once the runner has given
up, with the last error's class. A round that fails in a way that is not a provider failure -- the
interpreter did not answer, a kernel ended under it -- is failed, not unavailable. A round that
ends with an error in the agent's own Python is neither: its execution's error is a result the
model read, and the round ended as any does.

## Per-round facts

For a conversational task the owner mirrors a round's final text to the user only when the round
made no deliberate send (A-00's third decision), so it needs the facts, not a policy. For each
round of the instance the owner can read: the round's number; its final text, which is
`Finished::reply`, what `RoundEnd::reply` carries for a primary agent (`round.rs:41-50`;
`session.rs:591-597`); why a limit stopped it, when one did, and its final reasoning when it held
no text; and the **deliberate sends** the round made on each external channel, by channel name,
count and item id, with whether it made any. The sends come from the external channels'
registry, which records each send against the holder's round it was made in
(`external-channels.md`, "Sending", "Positions, the registry and the close"; `0003-51` records,
`0003-53` reads by round), and never from `message.sent` events. The order of the tasks decides
how the field fills: `0003-48` defines the per-round read with the round's final text and a
`sends` field that is empty until `0003-51` lands, and says so; `0003-51` adds the round's sends
to the read from its registry. The item ids are the stable ids `0003-51` mints for outbound items;
without an external channel a root has nothing a send can reach, since a root has no `user`
channel and the host refuses a send on one it does not read
(`crates/outrig/src/python/host.rs:2243-2255`).

OutRig implements no reply policy: it reports the text and the sends, delivers the text to no
one, and the owner decides what the user sees. The facts of the last ended round ride on the
state at rest; a bounded history of earlier rounds is `rounds()` (`0003-48`'s fork 2).

## The roster

`Session::roster()` is a snapshot of the session's agents: for each, its id; its class, for an
instance, or its kind, for a work or request child; its name where one exists; its parent -- the
owner, or an agent's id -- and depth; its state, in the stop read's terms plus `wedged`
(`children.rs:128-134`) and `releasing`; its last state at rest; the model it runs on
(`Node::model`, `:104`) and, for an agent on an alias, the member in use (`0003-59`); and, for a
child, the requests outstanding on it, unread and taken (`summary`, `:1012-1037`). It is a read, not
a subscription: taken under the registry's lock, so it lags by at most one lock, and it agrees with
the shutdown report at the end -- every request a roster taken after admission closed lists
outstanding is in the report's `requests`, ended `closed` or `terminated` (`report.rs:113-126`;
`children.rs:266-287`). An agent at its model-request limit shows so (`0003-50`); a retrying one
shows its next attempt (OR-A6-4). The assessment reads the roster for its agent-level facts
(`continuation.md`, "Assessment").

## Python-host parity

`0003-35` gives a Python host program an agent class it declares in its own process, driven over
the owner connection; `0003-46` constructs from provisioned code and returns a control handle in
the owner's process. Giving the host program the same -- `session.construct` over the owner
protocol, with a handle whose replies decode into classes rebuilt from the kernel's declaration,
and the stop read and the roster as owner-protocol requests -- is possible on the same kernel
side and is not needed by the first application (A-00's first decision). It is `0003-46`'s fork
6, an open choice, and this page's "Open questions" names it; nothing here forecloses it.

## Events

The reads are not events, and `0003-48` and `0003-49` add none: the stream stays the diagnostic
surface, and the reads are the authority. `0003-46`'s events are `task-agents.md`'s. Whether the
runner's backoff is also evented, for the renderer, is `diagnostics.md`'s question. No close row:
a read closes nothing; the handle's rows are the **Root agents** rows `0003-46` extends.

## Rejected alternatives

**Rejected: the owner driving the root's rounds through `Session::round()`.** The instance's
runner opens rounds on arrivals and on what it holds open, which is what makes `0003-37`'s rule
and the external channels' wake policies hold; an owner loop beside it would be a second
scheduler, and the application forbids itself one (CC-AGT-10).

**Rejected: reading stops from the event stream.** A subscription drops with a counted gap, and
an owner's claim-state transition cannot rest on a record that may have been dropped.

**Rejected: a translation of the application's old stop causes.** The set above is generic and
small; what a stop means for a task -- park, hold, fail, wait -- is the owner's policy, applied
outside OutRig (`decisions.md`, section 4).

**Rejected: inferring task completion.** A round that ends, a text that reads as final, or a send
that says "done" proves nothing; completion is the application's operation on a service it bound
(CC-MSG-7).

**Rejected: handing the owner the Python object, or a proxy of it.** The object is in the kernel;
every ordinary method call would be a boundary request, which `task-agents.md` rejects for the
reason OR-AG3 gives: a helper runs where it is called.

**Rejected: a roster as a subscription.** A subscription is the stream; the roster is a read of
state, which is what a heartbeat and an assessment want.

## Open questions

- The spellings: the construct call, the handle type, the state enum's variants, `status()`,
  `changed()`, `next_stop()`, `rounds()`, `roster()`. Each is its task's fork.
- Whether the state at rest and the per-round facts are one read or two (`0003-48`'s fork 2).
- Whether the handle is reachable from `Session` by instance id (`0003-46`'s fork 4).
- Python-host parity for construction, the stop read and the roster (`0003-46`'s fork 6).
- Whether the runner's backoff is evented beside the read (`diagnostics.md`).
- Whether the roster carries token usage per agent, which the ledger has (`agent/ledger.rs`),
  or the owner reads it from `model.round.*` events and the report.
- How a content-policy refusal is told from a round the provider cut short for length: the
  finish reason reaches the round as a cut-short reason today (`retry.rs:1509`, `round.rs:221`),
  and `0003-48` decides the mapping by reading what each provider style reports.

## Unverified

- That the runner can keep a round's `Finished` without changing `Agent::opened`'s signature was
  read from `runner.rs:166-196`, where `Ok(_)` discards it; `0003-48` keeps it.
- The claim that a roster lags by at most one lock rests on every state change the runner makes
  being made under the registry's lock, which is so for `round_opened`, `round_ended` and `idle`
  today (`children.rs:1130-1173`) and is `0003-48`'s to keep for the state it adds.
- That a Rust test can drive a `no_primary` session with a mock model through construction and
  rounds without a container was assumed from `start_on_host` (`builder.rs:592`), which the
  existing tests use; the e2e with a real model is gated as `0003-35`'s is.
- Whether a provider's content-policy refusal is distinguishable from other cut-short reasons in
  every supported style was not checked against each; the open question above names it.
