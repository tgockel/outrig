# External channels

An **external channel** is a channel whose far end is a **backend** the owner supplies: the
`user` channel's shape, generalized, so that an application's own messaging reaches an agent as
`runtime.channels[name]` and what the agent sends there reaches the application. OutRig owns the
local queue, the announcement, the wait, the delivery ids and the request ids; the backend owns
routing, durable storage and identities (`README.md`, "Vocabulary"). This page decides the shape
of that layer: one definition with two kinds, plain and request, over one backend mechanism; a
backend that is a Rust trait, given to the builder with the definition, called by OutRig for every
outbound item and told of every receipt and every settlement; inbound delivery as the owner's push
into a named channel; a local queue that is a bounded window over the backend's durable one; a
wake policy per channel; a consumed position reported when the agent receives and never when an
item is queued; request settlement on which the agent's handle and the backend's record agree; a
send whose outcome is accepted, refused, known not accepted or uncertain, with an identity to
resolve by; one external channel for the primary's initial and later input; and a host-side
registry of channel descriptors and positions that the cut reads and hands to the backend.
`0003-51` builds the definition, the trait, the push, plain channels, the wake policy, the
consumed position, the four outcomes as the agent sees them, and the uniform user channel;
`0003-52` the request kind in both directions, its settlement under the backend, and the liveness
rule's coverage of it; `0003-53` identities and references, resolution by identity, the registry,
and the positions handed to the backend at the cut.

The requirements this page answers are OR-CH1 through OR-CH3 of CocoClaw's phase 0008
(`requirements/outrig/02-channels-and-mcp.md`) and OR-A7-1 through OR-A7-5 of its addendum
(`A-07-owner-side-channel-backend.md`), which make the backend a Rust trait on the owner's side:
the first application is a Rust embedder of `outrig::harness` and runs no Python host program
(`A-00-index.md`, decision 1). CocoClaw's messaging is the running example and never a special
case: no mechanism here names an application's type, channel or schema, and the only backend a
task builds is the one its tests need. `messages.md` stays authoritative for channels, endpoints,
contracts, delivery rules, waiting and the `user` channel; `work.md` for the handle and for
settlement; `agent-classes.md` for a child's rounds; `lifecycle.md` for the close. Each is cited
where it decides something, and nothing of theirs is restated here as a decision of this page.

## Reserved ground, and two decisions that stand

`messages.md` opens with the ground this page lands on: "Only the `user` channel is built in the
first milestone. The rest is designed now so that adding a second relationship later does not
change the shape of the first", and "the rest of the layer stays design". The interpreter says the
same of its code: "Today the other end of every channel is the host: an agent's `user` channel"
(`crates/outrig/src/python/interpreter.py:3453-3455`). An external channel is that second
relationship, and this page changes nothing about the first: an endpoint keeps its three
operations, a `Delivery` keeps its shape, `runtime.wait` keeps its rule, and the serializable
subset is still the contract. Plain external channels are therefore a deferral the layer
reserved, not a reversal, and this phase reverses nothing of `messages.md`.

Two recorded decisions near this page stand, and the tasks cite them rather than reopen them:

- **The direction of a request channel between a parent and its child is fixed.**
  `agent-classes.md`, "Explicit plumbing": "The direction is fixed in 0.3 -- the parent requests,
  the child replies -- and a child's own requests go to children of its own." That sentence is
  about the relationship between a parent and its child. An external request channel is a
  relationship between an agent and a backend; its direction is the definition's, inbound or
  outbound, fixed when the channel is declared ("One definition, two kinds"). Nothing here gives a
  child a `send` on a channel its parent declared.
- **Children have no `user` channel.** `messages.md`, "The user channel": "Decided in planning
  (2026-09-30): children have none in 0.3." An external channel is not the user channel: its far
  end is a backend, not the user, its name is the application's, and `UserChannel` and the REPL
  still address the primary only (`README.md`, "Out of scope"). A child given an external channel
  through its facility dict ("Who holds a channel") is not given a user channel, and
  `plan/next/children-have-a-user-channel.md` stays what it is.

## The shape

```text
  OWNER'S PROCESS (the Rust embedder)                  CONTAINER
  +------------------------------------------------+   +-----------------------------------+
  | backend: the embedder's object behind a trait   |   | interpreter                       |
  |   routing, durable queue, identities            |   |   kernel of the holder:           |
  |     ^ send / consumed / settled / positions     |   |     runtime.channels["inbox"] <-+ |
  |     |                                           |   |       local queue (the window)  | |
  | OutRig's host end of the channel                |<--|-- msg / send / consumed lines --+ |
  |   the registry: descriptor, unread ids,         |   |     runtime.wait watches it       |
  |   received-and-unanswered ids, outstanding      |   |     the announcer counts it       |
  |   outbound requests, two watermarks;            |   +-----------------------------------+
  |   events; the close rows                        |
  |     ^ deliver / cancel / settle / close          |
  |     |                                           |
  | the owner's own code, through a channel handle  |
  +------------------------------------------------+
```

Three parties, two boundaries. The **backend** is the embedder's Rust object, given to
`SessionBuilder` with the channel's definition; OutRig calls it through its trait for every
outbound item, every receipt and every settlement, and never the other way. Inbound items come
from the owner's own code, pushed through a **channel handle** OutRig hands out as
`Session::user_channel` hands out `UserChannel` today (`crates/outrig/src/harness/session.rs:
246-248`). OutRig's **host end** holds the channel's record and talks to the interpreter through
the protocol lines the `user` channel already uses -- `msg` in, `send` out, `pending` and
`received` beside them (`observability.md`, "Most of this is already on a wire"). The
**endpoint** in the holder's kernel is `interpreter.py`'s `Endpoint` (`:3582`), with its bounded
queue, its delivered count, its `pending()`, `receive()` and `send()`, and the watchers
`runtime.wait` registers (`_watch`, `:3682`). Nothing of the backend runs in the container, and
nothing of the agent's runs in the owner's process.

Which side owns what, as OR-CH1 divides it and as the layer already divides it for the `user`
channel:

| OutRig owns                                   | the backend owns                             |
| --------------------------------------------- | -------------------------------------------- |
| the local queue and its bound                 | the durable queue and what is in it          |
| the announcement, by name and count           | who the sender is, and who the addressee is  |
| `runtime.wait` watching the channel           | routing an item to the agent's channel       |
| the delivery id and the outbound item id      | the item id of what it delivers; references  |
| the consumed position, reported at receipt    | the position it records from it              |
| the request's local record and its settlement | the durable record that must agree with it   |
| the four outcomes of a send, as reported      | whether an item was taken, when asked        |
| the positions handed over at the cut          | redelivery by item id, and deduplication     |
| events, the registry, the close rows          | retries and reconciliation                   |

A message to the agent is **data the agent's code reads**, as `messages.md` has it for the user
channel: the model is told that something waits, by channel and count, and only what the
execution that read it observed enters its history (`history.md`, "Why this is newly safe").
Nothing on an external channel is placed into the model's context by OutRig. That is what lets a
backend deliver without knowing whether the agent is mid-round.

## One definition, two kinds

OR-CH1 describes one abstraction with one definition and one wake policy; OR-CH2 distinguishes
plain items from reply-owed requests. In `messages.md` those are two channel kinds with different
ends: a plain channel's endpoint has `receive`, `send` and `pending`, and a request channel's two
ends are `request() -> handle` with `pending()` on one side and `receive()`, `reply` and `fail` on
the other ("Request channels"). This page keeps both facts: **one definition and one backend
mechanism serve two kinds, and a definition declares its kind.** The wording fix the validation
asked for is this sentence.

A **definition** names a channel and fixes, for its lifetime:

- its **kind**: `plain`, or `request`;
- for a request channel, its **direction**: `inbound`, the backend requests and the agent
  replies, or `outbound`, the agent requests and the backend replies;
- its **contracts**, one per direction, as `messages.md`'s "Contracts" has them, each a set of
  types or empty ("Bodies");
- its **bound**: how many items the local queue holds unread, and for an inbound request channel
  how many requests may be unsettled -- unread, or received and unanswered -- at once; the default
  and the ceiling are the interpreter's own, 256 (`interpreter.py:3466`);
- its **wake policy**: `wake`, `announce` or `silent` ("Wake policy");
- its **description**, which the agent's instructions carry ("What the agent is told");
- its **backend**, the embedder's object behind the trait, which is the channel's far end
  (OR-A7-1: a definition names the backend the embedder implements).

What the agent sees per kind:

| kind, direction      | the agent's end                                        | the far end     |
| -------------------- | ------------------------------------------------------ | --------------- |
| plain                | `receive()`, `send()`, `pending()`; `Delivery` in      | the backend     |
| request, inbound     | `receive()` -> `RequestDelivery`; `reply`, `fail`      | the backend     |
| request, outbound    | `request(body) -> handle`; `pending()`                 | the backend     |

A plain channel may carry nothing in one direction, which `messages.md` already allows ("A
channel may also carry nothing in one direction"). So a one-way progress feed from the agent and
a one-way notification feed to it are plain channels with one empty contract, not kinds of their
own: the four kinds the companion note lists (message, request/reply, progress, notification)
collapse to two, because one-way is an empty contract and wake is a per-channel policy. OutRig
does not coalesce a one-way feed; a backend that wants progress coalesced coalesces it
("Rejected alternatives").

The channel's name is the agent's name for its endpoint, `runtime.channels[name]`, and the
owner's name for the channel at the host end; the two are the same string, since there is one
definition. `user` is reserved (`messages.md`, "The user channel"), and so are the names a
kernel binds at boot, `runtime` and `asyncio`, and the names of the agent's request methods when
the holder is an agent class instance (`agent-classes.md`, "The declaration").

## Bodies

OR-CH1 says "OutRig must not learn the job schema" and, in the same breath, that "Definitions
describe channel kinds, contracts, bounds and wake policy". A contract OutRig checks is OutRig
learning the shape of what crosses. The consistent reading, which this page adopts: **the envelope
is OutRig's, and the body is one of two things.**

- **A body the backend owns** crosses as `str`, or as untyped JSON -- any JSON value, which the
  agent's code reads as the `dict`, `list` or scalar it decodes to. OutRig checks that it is JSON
  and that it fits `MESSAGE_MAX` (`interpreter.py:3467`), and nothing else. An application whose
  job records have a schema keeps that schema on its own side, in the backend, and the agent's
  code reads what arrives.
- **A body the definition types** is declared in the serializable subset, checked by
  `outrig.schema` as every contract is (`messages.md`, "Contracts": "a contract is checked by
  `outrig.schema` itself, so a channel and a result type never accept different types"), and
  decoded strictly into the agent's own instance of the declared type, as a child decodes a
  request (`agent-classes.md`, "Message and reply types"). The definition declared the type, so
  OutRig knows it; that is the application's choice, made once, in the definition. A Rust owner
  names the type by its import path into the application code (`application-code.md`;
  `0003-43`), `module:Name`, so the kernel checks with `outrig.schema` and the owner sends and
  receives JSON that the type's schema text types; declaring the type from Rust in a description
  of its own is `0003-51`'s alternative.

Where a typed body is decoded follows from a rule the interpreter already has. `_deliver` runs on
the reader thread and refuses to build a dataclass there, "since building one runs the agent's
code, and this is the reader thread" (`interpreter.py:3759-3762`). So an arriving typed body is
checked against the schema as JSON on arrival, with no code of the agent's run -- a body that does
not fit is refused to the owner with the problems, and nothing is queued -- and is constructed
into the declared class at `receive()`, on the agent's own loop. `0003-51` builds this; its fork
is whether anything but the structural check runs on arrival.

Sender identity comes from the owner's push, never from the body: an inbound item carries a
`sender` the backend set, which `Delivery.sender` carries into the agent's code, as `"user"` is
carried today (`interpreter.py:3500-3515`). `messages.md`, "Routing and attribution": "A body
that claims to be from another agent is a body with a field in it." An outbound item has no
addressee field; the channel is the addressee, and the backend routes it.

## The local queue is a window

An endpoint's queue is bounded, `QUEUE_MAX` of 256 unread (`interpreter.py:3466`), and a delivery
past the bound is refused rather than waited for: `_deliver` raises `_Refused` "already holds 256
unread messages", having queued nothing (`:3773-3774`), and the host's `post` answers with that
refusal (`crates/outrig/src/python/host.rs:1100-1140`). The bound is kept, and it is what makes
the following true: **the local queue is a window over the backend's durable queue, and the
owner paces delivery into it.** The backend holds every item durably; OutRig holds at most the
window's worth, unread, in the interpreter's memory; an item the window has no room for is refused
with `full`, and the owner delivers it later. Push against poll is inside the integration: how the
owner learns of new items -- a subscription, a poll of its store -- is its own, and OutRig sees
only pushes into the window.

**Inbound delivery is the owner's push into a named channel** (OR-A7-2). The owner's code calls
`deliver` on the channel's handle with the item: its **item id**, the identity the backend's store
gave it, by which OutRig deduplicates and reports positions; its **sender**; its **kind**, a message
or a request; its **body**; a **correlation id**, which a reply or a progress item may name; and the
**backend's reference**, opaque to OutRig. The push answers once the item is queued in the holder's
window, with OutRig's **delivery id** -- the id `Delivery.id` carries and a reply names -- or
refused with `full`, `contract` and the problems, `unknown channel` or `closed`. A push that names
an item id the channel has already taken -- unread, consumed, or consumed and not yet acknowledged
-- is a **repeat**: it is answered with the item's existing delivery id and position, nothing is
queued, and the agent sees one delivery. That is what makes redelivery by item id safe (OR-A7-2): a
backend that redelivers what it was never told was consumed loses nothing and repeats nothing. An
item id a resumed session holds as awaiting redelivery (`continuation.md`, "Open work at the cut")
is not yet taken: its first push is queued under the delivery id the save recorded, and only a
later push of it is a repeat.

**A directive crosses as an item.** An item on a plain channel may name a skill -- the push carries
`skill`, the name in the catalog -- and the delivery reaches the holder with `Delivery.skill` set,
as a `/name` line on the user channel does (`skills.md`; `0003-33`); the holder runs it through its
own code as the primary does, and `skill.directive.received` records it with the channel. A name not
in the catalog is refused at the push with `unknown skill`, and nothing is queued. A request carries
no directive. `skills.md`'s "a directive never reaches a child" was decided for children under a
primary's user channel and stands there; a holder of an external channel is reached this way.
`0003-51` builds it.

Three positions, which OR-CH3 asks to keep apart, and which the layer already keeps apart:

1. **Queued locally.** The host posted the item on the holder's channel and the interpreter
   queued it, answering with the count then waiting (`_msg`, `interpreter.py:6309-6336`). The
   push returns here. The item is in the window and nothing has read it. `messages.md`:
   "Receiving removes a message. Notification does not." Nothing is reported to the backend at
   this position: queued is not consumed.
2. **Consumed.** The agent's code took it with `receive()`, which removes it from the queue
   (`:3631-3649`). The interpreter tells the host, the host tells the backend, and the backend
   records its position. This is the **consumed position**, reported when the agent receives and
   never when the item was queued (OR-A7-2): one notice per delivery, kernel to host to backend.
   Today the interpreter sends a `took` line only once the host has asked to observe (`_took`,
   `:3651-3654`; `_observe`, `:6478-6486`), because nothing needed it to deliver. For an external
   channel the notice is part of delivery and is always sent; `0003-51` decides whether that is
   the `took` line made unconditional or a line of its own.
3. **Observed by the model.** What the execution that read the item printed entered the turn,
   and nothing else did (`history.md`, "Why this is newly safe"). OutRig records the turn
   (`turn.committed`), not a per-item mark: whether a consumed item was "incorporated into model
   history" is a question about the execution's output, and the stream answers it.

Accepted by the backend is a fourth position, for the other direction ("Sending"). None of the
four implies the next: accepted is not queued, queued is not consumed, consumed is not observed.

Ordering holds within a channel and nowhere else (`messages.md`, "Delivery rules"), and
`receive()` takes in order, so the consumed positions of one channel are a **watermark**: the
highest delivery id consumed, below which every item of that channel is consumed. The registry
keeps two of them per channel -- **consumed**, the holder's code took it, and **acknowledged**,
the backend confirmed the notice -- beside the unread ids ("Positions, the registry and the
close"). The consumed notice is at least once from OutRig's side: a notice the backend has not
acknowledged is retried while the session lives, so a backend treats it as idempotent per item.

**At a cut OutRig hands the backend the consumed and unread positions per channel** (OR-A7-2),
by item id, so that a fetched-but-unread item stays deliverable after a resume and an item
consumed but never acknowledged is redelivered by its id. `0003-53` defines the registry's
snapshot and the trait method that receives it; `continuation.md`'s cut (`0003-62`, `0003-65`)
calls it, and what the save carries of the registry is `continuation.md`'s.

## Wake policy

`runtime.wait` watches every endpoint of the calling agent and raises `MessageAvailable` while any
holds unread input (`interpreter.py:4036-4130`; `messages.md`, "Waiting": "Input wins"). The
announcer tells the model what waits, by channel and count, at a round's opening and on the next
tool result (`crates/outrig/src/agent/channel.rs:42-134`; `agent/tool.rs:147-160`). For the
`user` channel both are unconditional. A definition chooses, per channel, what an arrival does
(OR-A7-4), in the three policies below; A-07 names them wake, count-only and neither, and this
page's spellings are:

- **`wake`** -- the `user` channel's behavior. An arrival ends a `runtime.wait` in the holder,
  raising `MessageAvailable` naming the channel; for an idle holder it opens a round; during a
  round it is announced on the next tool result. The companion note's reason for making this a
  choice: many reporters on a feed could otherwise end every wait.
- **`announce`** -- A-07's count-only. An arrival does not end a `runtime.wait`; it is announced on
  the next tool result and at the next round's opening, and for an idle holder it opens a round. The
  agent reads it when it chooses, told that it waits. That last clause departs from A-07's "only
  counted in the announcement", and the departure is recorded here: count-only means an arrival does
  not end a running wait, not that it never opens a round, since an idle holder would otherwise
  never learn of the item; CocoClaw amends OR-A7-4 to match.
- **`silent`** -- A-07's neither. An arrival neither ends a wait nor is announced nor opens a
  round. The agent reads the channel when its own code does, with `pending()` and `receive()`. A
  feed the agent polls on its own schedule.

The policy changes what `_watch` reports to a wait and what the announcer counts; it changes
nothing about delivery, the queue or the consumed position. For a request channel it decides
only whether an arrival ends a wait: a request is in the holder's summary whatever the policy
("Requests"), so a round opens on it by the rule `agent-classes.md` states.

How an arrival opens a round for an idle holder depends on who drives the holder. A primary the
harness builds -- `run-new`'s -- is driven through `Session::round`, which returns `NothingNew`
when nothing was delivered since the last round that ended well, with the `Announcer` keeping the
baseline it compares (`channel.rs:59-64`, `:95-134`). The application's primary is not that: under
A-02 it is an owner-constructed root in a `no_primary` Session, driven by its own runner, and the
owner never calls `Session::round` for it (`rust-owner.md`; `0003-46`). A runner-driven agent --
that root, or an agent class instance's child -- has the runner's rule: a round opens when what
the child holds open has changed since the last round the host opened on it (`agent-classes.md`,
"The child's rounds"; `crates/outrig/src/agent/runner.rs:1-35`). `0003-51` extends what the
runner compares with the delivered and unread counts of the holder's plain channels whose policy
is `wake` or `announce`, compared as the `Announcer` compares counts, so that an arrival opens a
round and an unread item earns one round at most -- the one its arrival opened or was announced
in. A plain item owes no reply, so nothing warns about it and nothing cancels it; `0003-37`'s rule
is about requests, and "Requests" says how far it reaches here.

**`runtime.wait` over a child's handle and an external channel is one wait** (OR-A7-4). A handle
is watched through its `future` and every channel through `_watch`, in the one loop
`runtime.wait` already runs (`interpreter.py:4036-4130`), so a wait over a child's handle and a
channel returns on whichever comes first: the handle settling, as `(done, pending)`, or the
arrival, as `MessageAvailable`. Nothing is added for it; `0003-51` states it as acceptance, since
an embedder offers one wait over local children and remote peers on it.

The recommended defaults, which `0003-51`'s fork settles: `wake` for a plain channel and for an
inbound request channel. The default is a default; a definition for a progress feed to the agent
says `announce`, and one for a feed the agent polls says `silent`.

## Who holds a channel

A channel connects two endpoints (`messages.md`, "Channels and endpoints"). The backend is one;
the other is one agent's, and this page calls that agent the channel's **holder**. Channel sets
are fixed when a kernel is built (`interpreter.py:5384-5397`), so a channel is attached to its
holder as the holder's kernel is built, and never later.

An external channel reaches an agent as a **facility** (`README.md`, "Vocabulary"; `facilities.md`;
OR-CH1: "Channel discovery follows supplied facilities"): the owner declares the channel on the
session, and the dict an agent is constructed with carries the channel's facility reference. The
kernel built from that dict has the channel's endpoint under the channel's name. A **facility
reference** for a channel resolves to the endpoint of the one kernel the channel is attached to; in
any other kernel it raises, naming the holder. So the application's primary -- the root the owner
constructs in its kernel from a class in the application code, with a dict of facilities
(`rust-owner.md`; `0003-46`) -- holds the channels its dict named, and a child the root constructs
holds a channel only if the root's code put the reference of a channel no kernel holds into the
child's dict: a reviewer built with `{"files": ...}` holds none, which is OR-AG4's acceptance, and
the orientation of each lists only what it holds.

Two rules follow, and `0003-51` builds both:

- **One holder per channel.** A dict that names a channel already attached to another kernel is
  refused at the launch it belongs to, naming the holder, and the refusal surfaces where every
  launch failure does: at readiness and the first handle (`agent-classes.md`, "Construction and
  readiness"). An application that wants a child to relay gives the child a narrower facility of
  its own making -- an ordinary Python object the child's dict carries -- not a second end of the
  channel.
- **A declared channel no dict names is the primary's**, when the session has a primary the harness
  built -- `run-new`'s, or a Rust embedder's that drives `Session::round`: that is how such an
  embedder gives its agent an external channel beside `user`. In a session with no primary, every
  declared channel's window exists from session start, held by the host before any kernel holds the
  channel: a push queues into it and is answered with a delivery id, whether the owner constructs
  the root before or after pushing and by either form of `0003-46`'s operation, and the kernel a
  dict names takes the window as it stands. That is how the application's primary has its initial
  input waiting when its orienting round opens (OR-A7-3), with no retry on the owner's side. A
  channel no dict ever names holds its items until the close, which reports them unread, and the
  channel handle's `holder()` -- the spelling is `0003-51`'s -- reads none, so an owner can tell.
  Refusing a push into an unheld channel, and refusing the start, are `0003-51`'s alternatives.

The holder's kernel is the one whose loop the endpoint's receives and sends run on, whose
`runtime.wait` the channel wakes, and whose rounds the channel's arrivals open. A root agent's
child is a kernel in the same interpreter (`agent-placement.md`), so an item a root receives and
hands to its child by ordinary Python is a Python object crossing between two co-hosted kernels,
and `messages.md`'s rule holds: identity does not cross.

## Sending

`await endpoint.send(message)` on a plain channel, and `endpoint.request(body)` on an outbound
request channel, hand an item to the backend. Today a `send` on the `user` channel writes one
protocol line and returns, and the host frees the send window of 16 when the user receives
(`interpreter.py:3698-3748`; `host.rs:1597-1628`, `:1903-1907`). On an external channel the item
goes to the backend through the trait, with the item's metadata -- direction, kind, type, the
sender as the backend sets it, the correlation id, a stable item id OutRig mints, and any backend
reference already known (OR-A7-1) -- and **`send` returns once the backend has taken it durably**:
the `accepted` of OR-CH3, which `messages.md` already says is what success means, "accepted for
delivery, not processed by the recipient". `SEND_WINDOW` keeps bounding what one endpoint has sent
and not yet heard about (`interpreter.py:3468`), so an agent that sends faster than its backend
accepts waits in its own `send`, in its own memory, as it does today.

The backend answers one of three things (OR-A7-1): **accepted**, with its opaque durable
reference; **known not accepted**, with a reason, so a retry is safe; or **uncertain**, a timeout
or a lost acknowledgement after a possible accept. OutRig adds the fourth, **refused**, decided
before the backend is called, and one addition to the backend's three: a trait call that runs past
its bound is uncertain, since OutRig cannot know. `send` and `request` return after accepted and
raise a typed exception for each of the other three, because what the agent may do next differs
for each (OR-CH3; section 3 of the companion note):

| outcome            | what is known                                   | the agent sees           |
| ------------------ | ----------------------------------------------- | ------------------------ |
| accepted           | the backend took it; it is not processed        | `send` returns           |
| refused            | known not sent: contract, bound, unknown or     | raised, naming why       |
|                    | closed channel, admission closed                |                          |
| known not accepted | the backend definitively did not take it:       | raised; a retry is safe  |
|                    | unreachable before the send, or an explicit     |                          |
|                    | rejection                                       |                          |
| uncertain          | the failure came after the item may have been   | raised, with the item id |
|                    | taken: a timeout or a lost answer               |                          |

Only the backend can say what became of an uncertain item, and it says so when asked: **an
uncertain item is resolved by its identity**, never by sending another. `0003-53` has OutRig ask
once, by the item id, before the agent is told: a backend that answers uncertain and then, asked,
accepted leaves one stored item and a `send` that returned, which is OR-A7-1's acceptance; one
that cannot answer leaves the item uncertain, raised to the agent with its id, and
`endpoint.resolve(item)` asks again, through the same trait method. The backend answers from its
durable record -- accepted, not accepted, or still unknown. "Retries with stable identities
require backend-supported deduplication/query semantics, not a generic transport promise"
(OR-CH3), so OutRig retries nothing and promises nothing a backend did not; `embedding.md`, "What
stays the embedder's", says the same of hosted services, and it holds here.

The identity is OutRig's: every item an agent sends is given an id by the host, the `MessageId`
its `message.sent` event already carries (`host.rs:2232-2242`), and the backend receives it with
the item on every call about it. A backend may return a **reference** of its own on acceptance --
a row id, a message key -- which OutRig keeps beside the item id and returns with every later
notice about the item, and never parses. `0003-51` builds the trait's answers and the agent's
exceptions; `0003-53` the query by identity, the references' bookkeeping and the registry.

Sending is not a lifecycle operation, on an external channel as on the user channel: it ends no
round and cancels nothing (`messages.md`, "The user channel"). A send from a task still running
after the round ended is the ordinary way code that finished late reaches the application. Each
send is recorded against the holder's round it was made in, so the owner's per-round read -- the
deliberate sends a round made, by channel, count and item id, which the owner's reply policy
needs (`A-06`, OR-A6-2; `rust-owner.md`) -- is served from the registry and never from the stream.

## Requests

A request owes a reply; a plain message owes nothing. `work.md`, "A request settles once", gives the
rules a request follows, and every one of them holds across an external channel: one terminal
outcome per request in a session, which does not move; a late answer refused and evented; cancelling
a waiter cancels nothing but the wait; `h.cancel()` as the explicit act. What this page adds is
which side is the backend, and how the backend's durable record and OutRig's local record are kept
in agreement: **a request on an external channel settles under the backend** (OR-A7-5).

### Inbound: the backend requests, the agent replies

The owner pushes a request as it pushes a message, with kind `request`; the push answers once the
request is queued, with its delivery id. The agent's end is a request child's end (`messages.md`,
"Request channels"): `receive()` gives a `RequestDelivery` with `id`, `body` and `sender`, and
`await d.reply(value)` or `await d.fail(message)` answers it (`interpreter.py:3518-3538`;
`RequestEndpoint`, `:5310-5362`). `reply` checks the value against the channel's reply contract
strictly, and raises the problems in the agent's code when it does not fit, as a child's does --
for a `str` or untyped-JSON contract there is nothing to fail. The request's terminal outcome is
one of:

- **replied** -- the reply was accepted; the backend receives the value, as JSON;
- **failed** -- the agent ended it with `fail(message)`; the backend receives the message;
- **cancelled** -- the owner withdrew it, because the requester gave up or the peer is gone, or
  the host cancelled it under the liveness rule (below); the backend receives which;
- **closed** -- the session closed with it open (`lifecycle.md`, "Closing a session"); the backend
  receives `closed`;
- **suspended** -- a sleep's cut ended it in this session (`continuation.md`, "Open work at the
  cut"; `0003-65`); the backend receives `suspended` under the durable id and keeps the request, and
  the session that resumes from the save takes it up under that id, where it ends once more in one
  of the ways above. The outcome that does not move is the session's; across a suspension the
  backend's record moves once, from `suspended` to the resumed session's end.

OutRig tells the backend each outcome once, through the trait, with the request's ids and the
backend's reference, and the backend records it durably: that is the one path by which the
requester learns the outcome, and it is what "local handles and durable backend state must agree
on settlement" (OR-CH2) means here, since the requester's handle, if the application keeps one,
is the backend's and not OutRig's. A reply after the request ended -- answered, cancelled, closed
-- is refused in the agent's code, evented as `work.md` has it, and reported to the backend as a
refusal that changes no outcome (OR-A7-5): a settled outcome does not move. The owner's cancel of
a request the agent has not received withdraws it from the queue, so the agent never sees it; one
the agent has received stays with the agent, and its `reply` or `fail` then raises
`RequestCancelled`, exactly as a parent's `h.cancel()` does (`agent-classes.md`, "The child's
rounds"). Cancelling a request never interrupts the holder's execution.

The inbound bound counts unsettled requests, unread and received-and-unanswered alike, as
`requests-max` counts them for a child (`work.md`, "Limits belong outside generated code"), and a
push past it is refused `full`. Nothing waits outside the window, and a backend that requests
faster than the agent answers holds the surplus in its own queue.

### Outbound: the agent requests, the backend replies

The agent's end is a parent's end: `h = runtime.channels["asks"].request(body)` checks the body
against the request contract, hands it to the backend as a send ("Sending"), and returns
`work.md`'s handle at once -- awaitable, settling once, with `cancel()`, `done()`, `result()`,
`status` and `future`, which `runtime.wait` takes directly and `asyncio.wait` takes through
`.future` (`work.md`, "The handle"). Its settlement is the backend's, which the owner's code
delivers through the channel handle: a reply, decoded strictly into the reply contract on the
agent's side as a parent decodes a child's reply; a failure and its message; or a cancellation,
for a requester released or a peer that finished. A reply that does not fit the contract ends the
request `failed`, naming the problems, and the backend is told; a repair loop is for a model, and
a backend is code. A settlement of a request that has ended is refused by OutRig, and the refusal
is the owner's answer, so the backend is told (OR-A7-5).

`h.cancel()` tells the backend through `withdrawn` and settles the handle on the backend's verdict:
`cancelled` when the backend withdrew the request, or `replied` or `failed` with the value the
backend had already recorded, so the handle and the durable record agree (OR-A7-5, OR-CH2). Until
the verdict arrives the handle is pending; a verdict that does not arrive within the call's bound
leaves the handle and the request `uncertain` in the registry, resolved by asking the backend what
became of the withdrawal -- `resolve(channel, request, withdrawal)`, which answers withdrawn,
replied or failed with the value, or still uncertain (`0003-53`) -- and never by the send query,
whose `accepted` says nothing about a request withdrawn after acceptance. The handle stays
`uncertain` until a terminal verdict is known. The
backend decides what a withdrawal means at its peer: "request cancellation does not inherently stop
peer effects" (OR-CH2), and OutRig claims nothing about them. A `runtime.wait` that times out with
the handle pending has settled nothing, as `work.md` says; the request stands, the backend still
owes its outcome, and the registry lists it as outstanding. Abandoning a local wait never declares
the remote peer idle. `pending()` on an outbound channel counts requests the backend has not yet
accepted.

An outbound request's acceptance is a send: the handle's status reads `sending` until the backend
accepts, then `waiting` until it settles; a refused or known-not-accepted send settles the handle
failed with that reason; an uncertain one leaves it `uncertain`, a non-terminal state that
resolution moves to `waiting` or to `not accepted`, by the request's id.

### Peer termination reaches the sender once

The backend reports a peer's termination by closing the channel, `close(reason)` on the channel
handle. The endpoint is then failed as `messages.md`, "Failure", has it: `runtime.wait` reports the
failure once and not on a later wait, and a later `send` or `receive` raises. Every request
outstanding on the channel ends `cancelled`, with the closure as its cause -- an outbound handle
raises `RequestCancelled` naming it, an inbound request is withdrawn or its `reply` raises -- and
each end is told to the backend as that request's one terminal outcome (OR-A7-5; CC-MSG-5).

### The liveness rule covers an inbound request channel on a runner-driven holder

`0003-37`'s rule: a request a child holds is either being worked on or cancelled, and its sender
is told which; a round that changes nothing is followed by a warning round, and a second such
round cancels every request the summary holds (`plan/todo/0003-37-*.md`; `agent-classes.md`, "The
child's rounds"). The rule is the runner's, and the runner compares a `Summary` built from the
host's record of the child's requests, by id (`crates/outrig/src/agent/children.rs:181-184`,
`summary`, `:1012-1035`; `runner.rs:64`, `:100-125`). The application's primary is a runner-driven
agent: an owner-constructed root in a `no_primary` Session, whose parent is the owner, whose
rounds open through its own runner, and for which the owner never calls `Session::round`
(`rust-owner.md`; `0003-46`; A-02, OR-A2-2). So the rule covers an inbound external request
channel on it exactly when the request is in that summary -- and `0003-52` puts it there: an
inbound external request is a request of the holder, with the same record, the same `unread` and
`taken` states, the same nudge and warning texts naming it by id and channel
(`agent/orientation.rs:405-418`), and the same cancel, which reaches the backend through the
trait as the request's one terminal outcome, so the requester is told (OR-A7-5). That is the
design point the validation named, stated rather than discovered, and the rule itself is
untouched. Two things it does not cover, each on purpose:

- **The `user` channel stays exempt**, as `0003-37` has it: plain messages owe no reply and have
  no sender to notify. The same is true of every plain external channel (OR-A7-4: the rule
  "applies to reply-owed requests inbound to the local agent, not to unread plain messages or
  notifications").
- **The task itself is not a request.** OR-AG1: "No synthetic execution request represents the
  task." An agent holding no request is idle, and idle is the owner's signal to decide what the
  task's next step is -- to wait, to deliver more, to sleep or to end (`lifecycle.md`;
  `embedding.md`, "What stays the embedder's": the task's transitions are the embedder's). Nothing
  in this layer keeps an agent "working on the task", and task completion has no channel kind:
  its signaling is the application's, on a facility of its own (`facilities.md`), never inferred
  from a round's end or from commentary.

A primary the harness builds for `run-new` is not runner-driven: the CLI drives its rounds with
`Session::round`, and it declares no external channel today. An inbound request channel on such a
primary, should a Rust embedder that drives `Session::round` declare one, is announced and listed
in the registry, and that owner is its policy; the warning-then-cancel does not run there, and
`0003-52` says so. It is the one holder the rule does not reach.

## Positions, the registry and the close

OutRig's host end knows, for every external channel, what the backend needs at a cut and what the
cut needs from the backend's side (OR-CH3: "Capture descriptors, unread/consumed positions,
outstanding requests and their relevant generation/history disposition, not blocked channel
stacks or pickled clients"; OR-A7-2: the consumed and unread positions per channel, handed to the
backend). `0003-53` makes that knowledge a **registry**, host-side, with a snapshot that can be
read at any moment while the session lives:

- per channel, its **descriptor**: name, kind and direction, the contracts' schema text, bound,
  wake policy, the holder, and the backend's reference for the channel if it gave one;
- per inbound item, the owner's **item id** and OutRig's **delivery id**; per inbound channel,
  the **unread** delivery ids in order, the **consumed watermark** and the **acknowledged
  watermark**;
- per inbound request channel, the **received-and-unanswered** request ids, each with how long
  ago it was taken;
- per outbound item, the **round** of the holder in which it was sent, so the owner's per-round
  read has the round's deliberate sends by channel, count and id (OR-A6-2);
- per outbound channel, the **outstanding** request ids with their state -- `sending`,
  `waiting`, `uncertain` -- and their backend references, and the items whose send is uncertain;
- for each id, the backend's reference, opaque.

The registry is what `continuation.md`'s cut reads (`0003-62`, `0003-65`), and this page defines
the registry and not the cut. The cut hands the backend each channel's consumed and unread
positions through a trait method `0003-53` defines, so that the backend knows what to redeliver
by item id after a resume. One fact about the close bears on why the registry must be readable
before it: `lifecycle.md`, "Closing a session", has the close settle "child work and unanswered
requests ... every waiter on them settled with a documented error", and the interpreter's `_close`
ends every open request `SessionClosing`. A sleep that ran through today's close would therefore
end every external request before anything recorded its position. `continuation.md` decides the
pause and what `0003-65` records before the close settles; what this page supplies is the
registry's snapshot, and the request ends `0003-52` gives the backend -- `closed` for an inbound
request through `settled`, `released` for an outbound one through `withdrawn` -- so that the
backend's record agrees with what the handle saw even when the session ends abruptly.

The registry is also what the shutdown report draws on. `ShutdownReport::requests` already lists
each child request open at the close and how it ended (`crates/outrig/src/harness/report.rs:35`;
`children.rs:269-287`), and the external channels' tasks add their rows to the close table
(`lifecycle.md`, "The close table") and their entries to the report: unread items left in the
window, sends and settlement notices still awaiting the backend's answer, items left uncertain.

## Initial input is a delivery

**The primary's initial task input and every later user input arrive on one external channel, with
the same envelope, the same announcement and the same consumption rules** (OR-A7-3, which makes
OR-CH3's proposed mechanism a requirement). Nothing about the first delivery is privileged, no
special first-message prompt is composed, and empty initial input delivers nothing. The agent's
instructions -- what it is, what it holds, how its channels work -- are a separate thing, composed
by `0003-47` from OutRig's rules, the class docstring, the application's contributions and the
facility and channel manifest ("What the agent is told"; `task-agents.md`), and they say nothing
about any particular message. The owner pushes the initial input through the channel handle as it
pushes anything else, before or after the holder's kernel exists; the window exists from session
start and holds it either way ("Who holds a channel").

OR-A1-5 requires one documented order for the docstring, the contributions and the first
delivery, and this page documents it with `0003-47`: the instructions come first and whole --
`ACT` and the framework's rules, the class docstring, then the contributions, the manifest among
them, in `task-agents.md`'s order -- and the orienting round's opening line comes after them,
announcing what waits by channel and count. The first delivery is therefore the last thing the
model reads before it acts, and it is an announcement, never a body in the instructions: with
`wake`, a `runtime.wait` in that round is ended by it; with `announce`, the model is told it waits
and reads it when it chooses. `agent-classes.md`, "The orienting round", gives the round itself,
opened at once on a fixed lead with an empty baseline (`orientation.rs:360`); `0003-47`'s fork
gives the root's lead its one extra clause. The alternative, which `0003-51`'s fork records,
waits for the orienting round to end and opens a round on the arrival, which costs a round and
tells the model nothing the opening line could not have.

## The owner's side

A backend is one of the things an embedder supplies before the session starts (`embedding.md`,
"What the embedder supplies"), beside the secret resolver, the bindings, the approver and the
skill sources, and like them it is given to the builder and fixed at start. The first application
is a Rust embedder, so the Rust owner is the one this page builds for; a Python host program's
parity is an open choice ("Open questions").

**A Rust owner** declares each channel on `SessionBuilder` with its definition and its backend,
as it gives a skill source (`crates/outrig/src/harness/builder.rs:278-284`; `SkillSource`,
`harness/skills.rs:64-76`). The backend is a trait object. `SkillSource`'s calling convention is
the template: each method runs on a thread where blocking is fine, within a bound, with bounded
concurrency (`skills.rs:98-102`), and a call that does not answer in time is given up on --
which, for a send, is the `uncertain` outcome. The owner's own code pushes inbound items and
settles outbound requests through a channel handle the session hands out. Illustrative only;
every spelling is `0003-51`'s, `0003-52`'s or `0003-53`'s:

```rust
// Illustrative only. The methods are asynchronous in 0003-51's recommendation (its fork 2), each
// bounded by a timeout and a concurrency cap; shown without `async` for brevity.
pub trait ChannelBackend: Send + Sync {
    /// An item the agent sent: accepted with a reference, not accepted, or uncertain. 0003-51.
    fn send(&self, item: &Outbound) -> Result<Accepted, SendError>;
    /// The agent's code received inbound `item`, by the owner's item id. 0003-51.
    fn consumed(&self, channel: &str, item: &ItemId) -> io::Result<()>;
    /// An inbound request ended in this session, once, as `end` says -- replied, failed,
    /// cancelled, closed or suspended (0003-65) -- or a late reply was refused. 0003-52.
    fn settled(&self, channel: &str, request: &ItemId, end: &Settlement) -> io::Result<()>;
    /// The agent's side ended outbound request `request`: `cancelled` by the agent, `released` by
    /// the close, `suspended` by a cut. The backend answers `Withdrawn`, or `AlreadyReplied` /
    /// `AlreadyFailed` with what it recorded, which settles the handle instead. 0003-52, 0003-65.
    fn withdrawn(&self, channel: &str, request: MessageId, end: &Withdrawal) -> io::Result<Verdict>;
    /// What became of an operation whose outcome is uncertain: for a `Send`, accepted with its
    /// reference, not accepted, or still uncertain; for a `Withdrawal`, withdrawn, replied or
    /// failed with what the backend recorded, or still uncertain. 0003-53.
    fn resolve(&self, channel: &str, item: MessageId, op: Operation) -> io::Result<Resolution>;
    /// The channel's consumed and unread positions, handed over at a cut. 0003-53.
    fn positions(&self, channel: &str, positions: &Positions) -> io::Result<()>;
}

// The channel handle, from `Session::channel(name)`, for the owner's own code:
//   handle.deliver(Inbound { item, sender, kind, body, correlation, reference }).await
//       -> Queued { delivery } | Repeat { delivery } | Refused(full | contract | ...)   0003-51
//   handle.cancel(request)                  -- the requester gave up                   0003-52
//   handle.settle(request, reply | fail | cancel)  -- an outbound request's outcome   0003-52
//   handle.close(reason)                    -- the peer is gone; the endpoint fails    0003-51
//   handle.room().await                     -- the window has room again               0003-51
```

Whether the push lives on a channel handle or on `Session` itself is `0003-51`'s fork; the
handle is recommended because the owner's delivery code then need not hold the `Session`, as
`UserChannel` is handed out today. The backend is reached only through the trait and the handle:
it is not a hosted object, its calls are not boundary requests, nothing of it is proxied into the
kernel, and the agent cannot reach it by any name (OR-A7-1). What the agent holds is an endpoint
OutRig made.

**A Python host program**, which `0003-35` and `0003-36` serve for another program, could be
given the same definitions with a Python object as each backend, which the engine would call over
the owner connection. That path is an open choice, not a requirement (`A-00-index.md`, decision
1), and no task here builds it; `0003-51` records it as a fork. Two facts bind it if it is ever
built: the owner protocol carries requests from the program and pushes from the engine, and no
request from the engine (`crates/outrig/src/owner/mod.rs:16-28`; `owner/connection.rs:141-242`),
so it would need engine-initiated requests; and the engine reads lines of at most 64 KiB from the
program (`owner/connection.rs:35`) while a channel item may be 1 MiB (`interpreter.py:3467`), so a
host program's push of a large item would need that bound raised or the body carried in parts.
The Rust push crosses no line and has no such bound.

## What the agent is told

The holder's instructions carry a **channel manifest**: for each channel it holds, the name,
the kind and direction, the contract text per direction as `outrig.schema` renders it, the
description from the definition, and what its wake policy means in the agent's terms -- that
arrivals here end a `runtime.wait`, or are announced only, or are not announced. The manifest
says, once, the one rule the companion note and OR-A7-3 ask for: a message on a plain channel
owes no reply, and a request on an inbound request channel does, answered by `reply` or `fail`
on its delivery. Today the primary's text names the `user` channel in `preamble`
(`orientation.rs:66-84`) and a child's lists its request channels in `child_preamble`
(`:213-241`); `0003-51` adds the manifest to both, and `0003-47` composes the whole from
contributions, so the manifest is one contribution and the per-kind texts are another. Nothing of
a backend's, and nothing of the task's records, is read to build it: the manifest is rendered from
the definition.

## Events

An external channel's items cross the host, so they are observed on the wire as the user channel's
are, not emitted deliberately as messages between co-hosted agents must be (`observability.md`,
"Most of this is already on a wire"; `agent-placement.md`: "A channel between two co-hosted agents
never reaches the host"). The existing message events apply, generalized: `message.sent`,
`message.received` and `message.refused` (`crates/outrig/src/harness/event.rs:612-633`) with
`from` and `to` naming the backend's end as well as `user` and an agent's subject, and `sender`
the identity the backend gave. `message.received` for an inbound item is its consumption, as it is
today for a message to the agent (`doc/reference/events.md`, "Integration audit"). Each task adds
its own events to `doc/reference/events.md`, in the categories `observability.md` fixes:

- `0003-51`: `channel.attached` -- a channel attached to its holder's kernel: name, kind,
  direction, contracts, bound, wake policy (execution diagnostics); `channel.consumed` -- the
  backend acknowledged a consumed notice; `channel.closed` -- the owner closed the channel, with
  its reason; `channel.item.outcome` -- a send's outcome at the backend: `accepted`, `refused`,
  `not_accepted` or `uncertain`, with the reason and the backend's reference (integration audit).
- `0003-52`: inbound requests under the request family `agent.request.*`, with `sender` the
  backend's identity, since an inbound external request is a request of the holder (above);
  `channel.request.sent`, `channel.request.settled` and `channel.request.refused` for outbound
  requests, under the requesting agent's subject, since their recipient is not an agent;
  `channel.settled` -- the backend acknowledged a settlement notice.
- `0003-53`: `channel.item.resolved` -- an uncertain item resolved by query, and to what.

Every event carries the item's or the request's id, so a reader can follow one item from
`message.sent` through `channel.item.outcome` to `channel.consumed` or `channel.request.settled`.
Bodies are recorded as the user channel's are, whole up to the channel's limit, since the
category is integration audit and the file is its owner's alone. The owner's authoritative reads
-- the per-round sends, the positions -- never come from these events (`A-06`); they come from
the registry.

## Rejected alternatives

**Rejected: the application's receive loop in Python, over a binding or a live object.** The
replaced surface was a bundle of application calls -- send, wait for messages, questions -- and
the obvious port is a hosted client the agent polls. It cannot have the properties this page
needs: a hosted call blocks the kernel's loop and cannot be stopped (`execution-and-rounds.md`, "A
hosted call blocks its caller"; `plan/next/cancel-a-running-hosted-call.md`), so a wait inside it
is a wait `runtime.wait` cannot end and a message cannot redirect; and nothing on the host would
know a message waited, so nothing could announce it. Wake, wait and announce are harness
semantics (the companion `conversation-design-check.md` says so), and they belong in the channel.

**Rejected: the backend as a hosted object.** Reaching the backend through RPyC would make every
send a boundary request through the bindings process, with its policy, its pool and its
uncancellable call, for something that is the owner's own code on the owner's own side; and A-07
requires that the backend "must not be a hosted binding that agent code could call directly". A
backend is given to the builder as the secret resolver and the skill sources are, and the agent
cannot reach it by any name.

**Rejected: OutRig polling the backend for inbound items.** One inbound path, the owner's push,
is one place to deduplicate by item id and one answer per item. A backend whose store is polled
polls it in the owner's own code and pushes what it finds; a trait method OutRig polls would be
a second path with its own pacing and its own duplicates.

**Rejected: a progress kind, and a notification kind.** A one-way feed is a plain channel with one
empty contract, which `messages.md` already allows, and the only property the companion note gave
progress -- that it might not wake -- is the wake policy, which every channel has. Coalescing is
not built: OutRig delivers each item the agent sends, and a backend that wants coalescing does it
at its end, where it knows what a later item supersedes.

**Rejected: mirroring the model's commentary onto a channel.** `messages.md`, "The user channel",
separates the model's own text from a deliberate send, and only the second is an act. The owner
that mirrors a round's final text to its user when the round made no deliberate send (`A-06`,
OR-A6-2) does so from the per-round facts and its own backend; OutRig sends on a channel only what
the agent's code sends.

**Rejected: a privileged first message.** The orientation is composed from the definition and
the class, and the first input is a delivery like the rest ("Initial input is a delivery"). A
prompt built from the first message would put a body into the model's context that the agent's
code never read, against the rule that input is data the code reads.

**Rejected: OutRig retrying an uncertain send, or redelivering an unacknowledged item itself.**
Only the backend knows whether an item was taken, and only it can deduplicate. OutRig asks, by
the item id, and promises nothing a backend did not (`embedding.md`, "What stays the
embedder's"); redelivery is the backend's, by item id, against the positions the cut handed it.

**Rejected: a channel added after the kernel is built.** Channel sets are fixed at kernel build
(`interpreter.py:5384-5397`), and a definition is given to the builder. Adding one mid-session
would need the kernel's channel dict to grow, the registry to learn of it, and the orientation --
composed once -- to be told; nothing this phase needs asks for that.

**Rejected: one wake policy for the session.** Many reporters on one feed would end every wait,
and a question channel that did not wake would wait for the next round. The policy is the
definition's, per channel.

**Rejected: a second holder for one channel.** Two kernels with endpoints on one channel is a
three-ended channel, on which an inbound request has no one answerer. A child that should relay
is given a facility of the application's making.

## Open questions

- Whether a Python host program gets backends of its own, Python objects the engine calls over
  the owner connection, with the same definitions. `A-00-index.md` makes Python-host parity for
  the new operations an open choice, not a requirement; `0003-35` and `0003-36` stay the phase's
  Python-host deliverables for another program. If it is built, the owner protocol needs
  engine-initiated requests and the 64 KiB line bound needs raising for channel lines ("The
  owner's side").
- Whether `Delivery.skill` can be set on an external channel. **Decided on review:** an item on a
  plain channel may name a skill ("A directive crosses as an item"), since the first application
  relies on `/name` commands; `0003-51` builds it.
- Whether `pending()` on an outbound request channel should count requests the backend has
  accepted and not answered, rather than those it has not accepted. The first needs the backend
  to say when its peer took the request, which the contract does not require of it.
- Whether a plain channel's unread items should have a position the agent can read without
  consuming -- a bounded, attributed preview, as `potential/user-input-preview.md` proposes for
  the user channel. It waits for that entry's evaluation.

## Unverified

- Nothing on this page is built. `interpreter.py`'s endpoint, `pending()`, the announcer, the
  delivery ids and `runtime.wait`'s watching exist and are tested for the `user` channel and for
  request channels between agents; that they generalize to a host end other than the user is
  reasoned from `host.rs:2232-2262`, where the only host end today is the primary's outbox and
  every other channel's send is refused as "nothing on the host reads this channel".
- That the runner's `Summary` can take external request records without a change to the rule is
  reasoned from `children.rs:1012-1035`, which builds the summary from `state.requests` by
  `record.child`; whether an id space shared between the host's `MessageId` and the interpreter's
  request ids is needed, or two registries are unioned, is `0003-52`'s fork.
- That `runtime.wait` over a child's handle and an external channel is one wait is reasoned from
  `interpreter.py:4036-4130`, where handles are watched through their futures and channels through
  `_watch` in one loop; `0003-51`'s acceptance runs it.
- The owner connection's 64 KiB line bound (`owner/connection.rs:35`) against a 1 MiB item
  (`interpreter.py:3467`) binds only a Python host program's push, which no task here builds; the
  Rust push crosses no line. Recorded so that whoever builds that path meets it knowingly.
- The interpreter program's size after `0003-39` is what makes room for the external endpoint
  classes; the cap it would otherwise hit is recorded in `0003-34`'s `## Decisions`.
- Whether `SkillSource`'s bounds -- 30 seconds per call, eight calls at once -- suit a backend
  whose store is slow, or whether a backend needs bounds of its own in its definition, is not
  measured; `0003-51` starts from the template and records what it finds.
