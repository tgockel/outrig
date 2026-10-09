# Observability

What a human can see of a running system, and what a program that owns a session can read from it.
Sessions already exist for this -- `doc/usage/sessions.md` opens by saying they are there "so you
can go back and inspect what happened" -- but what they record is the container, its connections,
and the servers' stderr. The agent itself is absent, and the same page still admits it at `:317`:

> **TODO: Incomplete** -- opt-in transcript capture (per-turn JSON of user/assistant/tool
> messages) is deferred.

This page is the answer to that, widened by what a Python agent makes visible. Three decisions are
settled before anything else, because they bound the whole subject:

- **One stream in memory, and the session directory is one of its readers.** A session publishes
  its events to an in-memory stream, and subscribers read it in the owner's process: an embedder
  through the session API (`embedding.md`), and the CLI through a subscriber that writes
  `logs/events.jsonl` beside `network.jsonl`. Nothing listens: a subscriber is code the owner runs,
  not a connection anyone can open.
- **Reading is read-only.** Nothing an observer does perturbs the session it observes. No message
  is injected, no execution is interrupted, no expression is evaluated -- and a subscriber that
  falls behind loses events, counted, rather than slowing the session down.
- **Nothing is captured richer than its category allows.** Three categories, below, each with its
  own rule. An execution clipped to 16 KiB is recorded as 16 KiB, with the same truncation marker.

The third one matters most, and it started as a single rule -- "the record holds exactly what
the model saw" -- which the event set below already breaks. `messages.md` is explicit that a model
can be told a message exists without its body entering its context, and generated code can receive
a body and never print it. So recording message bodies is capture that rule does not license, and
pretending otherwise would have left the page contradicting itself.

Three categories instead, because they have genuinely different readers and risks:

**Model view** -- what was actually presented to a particular model call. Bounded as the model saw
it. This is the category the original rule described, and for it the original argument still holds:
it adds no reach a model did not have, because a model that saw it could already print it.

**Execution diagnostics** -- runtime state kept for debugging: outcomes, timings, what an
execution is awaiting, resource events. Not model-visible, so it needs its own justification
rather than inheriting one.

**Integration audit** -- what crossed the hosted-object boundary and what was decided about it
(`hosted-objects.md`, `boundary-policy.md`). A hosted request is recorded as events that share its
id: its receipt, published before dispatch, with bounded previews of its arguments; its decision --
the rule's action, the evaluator's verdict, or the approver's answer -- with the effective policy's
version, a digest of the rules in force; its dispatch; and its outcome. A request never dispatched
has no dispatch event. The category carries the warning any call log carries: the most useful
field, the arguments, is also the one most likely to hold a secret. It also holds things no model
saw -- a host traceback stays in the event and out of the agent's error -- so, like execution
diagnostics, it needs its own justification. An event never calls into an object to describe it:
a preview is built from the by-value form that crossed, and a host object is named by its type.

What the single rule got right and the three must keep: capture is a decision with consequences,
not a free byproduct. Even recording exactly model-visible text changes its retention and its
audience, so "no new disclosure" was always too strong for a file that outlives the session.

## Most of this is already on a wire

The interpreter protocol is NDJSON and the host reads both halves of every exchange, so the
expensive part of watching an agent is already paid:

| message    | direction     | what it already carries                                 |
|------------|---------------|---------------------------------------------------------|
| `exec`     | host → interp | the source an agent submitted                           |
| `result`   | interp → host | status, bounded output, and the traceback               |
| `inv`      | both          | a bounded name-and-type listing of what the agent holds |
| `msg`      | both          | a message on the `user` channel; answered with a count  |
| `pending`  | both          | how many messages wait unread, by channel               |
| `send`     | interp → host | a message the agent sent                                |
| `received` | host → interp | that the user took one, which paces the agent's sends   |

`inv` is already requested by the liveness probe in `runtime-protection.md` -- every thirty seconds
while an execution runs, and not otherwise -- so recording its answer costs nothing that is not
already spent.

Hosted calls add one more kind, `rpc` (`0003-16`), and are cheap to observe for the same reason:
each request is already decoded on the host side, where it is intercepted (`hosted-objects.md`).

**Messages between agents are the exception, and co-hosting is why.** `agent-placement.md` puts
every agent in one interpreter process, and `messages.md` says a message between two of them never
leaves it -- it moves between event loops through `loop.call_soon_threadsafe`. Nothing about it
reaches the host. So an inter-agent message has to be **emitted deliberately**, as an observation
the interpreter sends because someone is watching rather than because delivery requires it.
That is a real cost co-hosting created, and it is the one place this subject is not free.

It is cheap in the way that matters, though: `messages.md` restricts a contract to the
serializable subset, so every body already has a JSON form, and the `Delivery` that `receive()`
returns already carries the sender and the arrival time.

## One stream, not one file per subject

`network.jsonl` and the `resources.jsonl` proposed in `plan/next/dynamic-resource-mounts.md` are
independent subjects, and interleaving a connection with a mount means nothing. An agent's
timeline is not like that. A model turn produced this code, which printed this, which sent this
message, which woke that agent -- the ordering *is* the information, and recovering it by
timestamp-joining separate files is exactly the thing that goes wrong at the moment it is needed.

So a session publishes **one stream**, in memory, and fixes its order where an event enters it:

- **A sequence at publication.** Each event is numbered as it is published, and the numbers are
  unique and gap-free however many threads publish at once. The sequence is the order of
  publication, not a claim about the order of effects on the host: two hosted calls running at
  once publish in whatever order their events reach the stream.
- **Fan-out.** Every subscriber receives every event, in sequence order.
- **A lagging subscriber loses a counted gap.** A subscriber that falls too far behind loses the
  oldest events it has not taken and is told how many. Nothing it does makes a producer wait, so
  no subscriber can hold up a round, an execution, or a shutdown. Because the sequence itself has
  no gaps, every gap in what a subscriber saw is a counted one.

The CLI's subscriber writes **`<session_dir>/logs/events.jsonl`**, one append-only file in sequence
order, when the opt-in below is on. One file rather than one per subject diverges from the
`network.jsonl` convention on purpose.

## The envelope is CloudEvents; the payloads are OutRig's

The house rule is to borrow a schema rather than invent one, and Zeek was the right borrowing for
connections because connection logs have a field vocabulary that other tools already read. Nothing
comparable exists for "an agent ran some Python." What does exist is a standard for the *envelope*,
and a heterogeneous single stream is precisely what CloudEvents is for.

Borrow the envelope; define the payloads. One event, wrapped here for reading and written as one
line:

```json
{"specversion": "1.0",
 "id": "41",
 "source": "/outrig/session/20260921T103000-a1b2",
 "type": "org.outrig.exec.completed",
 "subject": "agent/primary",
 "time": "2026-09-21T10:30:07.412Z",
 "datacontenttype": "application/json",
 "data": {"execid": 7, "status": "error", "duration": 1.83,
          "output": "building...\n", "error": "Traceback (most recent call last):\n..."}}
```

`id` and `source` are the two required attributes that carry meaning here: the spec requires
`source` + `id` to be unique per event, and a session plus the stream's sequence number satisfies
that without coordination. `id` *is* that sequence, so a gap the CLI's subscriber was told about
shows in the file as a jump in `id`. `source` is a URI-reference rather than a free string, which
is why it is written as a path. `subject` names the agent, which is what the protocol's agent id
supplies.

`type` values take the `org.outrig.` prefix the codebase already uses for OCI and container labels
(`org.outrig.mcp`, `org.outrig.session`), which is also what the spec recommends -- a reverse-DNS
prefix naming whoever defines the semantics.

**One constraint decides the rest of the layout.** CloudEvents attribute names "MUST consist of
lower-case letters [a-z] or digits [0-9]", so the flat `outrig.session_id` style `network.jsonl`
uses for its extras is not a legal extension attribute. Nothing OutRig-specific goes at the top
level. Everything goes inside `data`, and the top level stays exactly the standard context
attributes. That is tidier than the alternative anyway.

The envelope belongs to the file. A subscriber in memory receives events as values
(`embedding.md`); the CLI's subscriber wraps each one in this envelope as it writes it.

## What is emitted

**The agent, from the protocol.** `exec.submitted` with the source; `exec.completed` with status,
the bounded output, the traceback, and a duration; `inventory.observed` with the names and type
names the probe already fetched.

**Messages**, per the emission above: `message.sent` and `message.received`, each with the
channel, the endpoint names at both ends, the sender, and the body.

**The model, from data the loop currently discards.** `llm.rs:1988` already calls
`.extended_details()`, which is rig's opt-in for usage tracking, and the success arm reads
`.messages`, `.output`, and `.content` while discarding `response.usage` and
`response.completion_calls`. So `model.round.completed` costs a field read: input, output, total,
cached, and reasoning tokens for the round, plus the per-turn breakdown. The context high-water
mark is the largest `input_tokens` across those calls -- rig's own documentation points at the
last entry for the final request's context length. A provider metric that is unavailable --
cached or reasoning tokens from a provider that does not report them -- is recorded as null, never
as zero, here and in every event that carries usage: zero is a count the provider did not give.

`model.retry` and `model.failover` are the same story one level down: `retry.rs:644` and
`failover.rs:351` format an attempt or a hop into `eprintln!` and drop it. A structured event also
retires a workaround -- today the failover chain's state is recovered by prefix-matching the
*error string* it was rendered into.

Every model event -- a round's usage and its per-call list, a retry, a failover hop -- carries two
ids (`0003-19`): a **logical call id**, one per model call the loop decided to make, and a
**provider attempt id**, one per request actually sent, so a call that was retried or failed over
has one logical id and several attempts. A failed attempt is recorded too, with the settings of
the request it sent and a usage that is null when the provider reported none. An aggregate -- a
round's total, a session's -- is derived from unique attempts, and never by summing a parent's
inclusive total with a child's, which would count the child's work twice.

A usage record can arrive after the attempt's event has been published. One that carries the same
attempt id replaces that attempt's null usage, once: a second record for that attempt is refused
and recorded as an event, and changes nothing. Spend from a later descendant attempt -- a retry or
a failover hop of the same logical call -- is a new attempt under its own id, never a correction
of an earlier one. After a replacement, every aggregate that counted the attempt is recomputed
from unique attempts, so a round's total changes once, by that attempt's usage. A round that
answers several requests is attributed to the round, and its usage is not divided among the
requests (`agent-classes.md`); `agent.request.settled` names the rounds a request spanned instead.

**Everything else that happens and is reported nowhere.** This is the part of the subject that
grows, and the first entries are already known: a tool result truncated (`rig_tool.rs` writes a
marker into model-visible text and counts nothing); the effective `max-tokens` ceiling, which never
escapes `build_agent`; agent lifecycle, including an agent that wedged past recovery; an interrupt
sent and a liveness probe that failed; a `MemoryError` raised by the memory ceiling.

**A promotion is an event**, and not an optional one. `history.md` lets an agent move something
from its full history into the context the provider sees. Without a record of that, the stream
shows a model suddenly citing something it was never sent, with no account of how it got there.
A promotion is a request, though, and not proof of what was sent: the window moves, duplicates
collapse, the budget evicts, and a retry or a failover reassembles everything. `history.md` makes
the per-call manifest authoritative for what a model actually received, and this event explains
how a thing came to be eligible rather than that it arrived.

The manifest names turns. A provider request also carries the preamble, the tool definition and
the effective ceiling, which `model.instructions` records (`0003-13`), and whatever the adapter
changes on the way out, which nothing records. An experiment that must reconstruct a request
exactly needs opt-in capture of the final request at serialization, credentials excluded; that is
`potential/request-capture-for-experiments.md`, not planned work.

It also settles a question left open in `agent-placement.md`: the unattributed-output bucket --
what reaches fd 1 without belonging to any agent -- is **logged as an event**. It cannot be billed
to an agent and it should not be silently dropped.

**The session's own state** (`0003-19`): starting, idle, round running, executing, closing, and
reported. *Closing* lasts from `close_admission()` to the report, through `lifecycle.md`'s
closing, draining and terminating steps. The interpreter's death is an event, not a state of its
own, after which the session passes through closing to reported. These are how an embedder knows
what its session is doing without parsing text (`embedding.md`), and the outcomes a shutdown
report lists are events too (`lifecycle.md`).

**The boundary, once there is one.** The integration-audit category has no producer until hosted
objects are built, and then three tasks add one each:

- `0003-22`: each hosted request's receipt, dispatch and outcome, sharing its id -- binding, agent,
  operation, member, host type, bounded previews, the outcome (`returned`, `raised`, `refused`,
  `cancelled` or `unknown`, as `lifecycle.md` defines them), duration, and the call a callback ran
  under;
- `0003-23`: each request's decision -- a rule's action with the rule's position and layer, the
  `default`, or the approver's answer -- with the effective policy's version; and each escalation
  with its answer, its cancellation, or the late answer that changed nothing;
- `0003-24`: each evaluator verdict, as a request's decision, and each evaluation's usage,
  attributed apart from the agent's, so a judge's tokens never appear in a round's total.

Three more tasks add events beside them that are execution diagnostics, not integration audit:

- `0003-26`: each request a child is given -- a submission, or a request on one of its request
  channels -- as one family, `agent.request.sent`, `received`, `replied`, `invalid`, `failed`,
  `cancelled` and `settled`, with request and reply bodies as bounded previews;
  `agent.call.started` and `agent.call.settled` only as the wrapper around a decorated call
  (`0003-27`). The child's model usage is attributed to the child's round and added to the skill
  invocation it ran under and to the main agent's round; a request's settled event names the
  rounds it spanned;
- `0003-29`: each skill invocation, which the hosted calls made inside it carry as context, not as
  authority. Invocation events are not an audit of what a skill did; only boundary events record
  every crossing (`skills.md`);
- `0003-30`: each agent instance -- `agent.instance.started`, `ready`, `released` and
  `collected`, the last when the runtime releases an instance that was collected unreleased.
  Execution diagnostics like the rest, with any body they carry as a bounded preview.

## Subscription becomes public; the file keeps its rules

This section used to conclude that nothing here becomes public. The caller supplied a directory,
the library owned the writer and the schema, and the phase's budget for public surface was the one
entry point `outrig-cli` needed, so a subject that published event types would have spent a budget
already committed. The first two still hold for the CLI's file. The third does not: the phase now
designs a public session API (`embedding.md`), and an embedder driving a session from Rust needs
its events as values, not as a file to read back.

So **event subscription becomes public through the session API**, and is fixed with the rest of it
at the 0.3.0 release. The event types stay crate-private until `0003-19` makes subscription public;
`0003-13` built the file writer and the event types without publishing either, and no
in-memory stream: its `emit` numbers each event and queues it to the file's sink. `0003-19` adds
the stream in front of that sink and makes subscription public. For the file, the revisit
`harness-components.md` asked for -- whether the library loop needs a session record of its own --
still concludes no, because `NetworkInterceptor::new(log_dir, ..)` already shows the shape. **The
caller supplies a directory; the library owns the writer and the schema.** Session directories and
their host conventions stay in `outrig-cli`.

The writer itself is not new work. `AuditSink` and `audit_writer` in `crates/outrig/src/network.rs`
are generic in everything but the record type: a bounded queue that applies backpressure rather
than dropping, a reserve-then-send enqueue so a cancelled producer leaves no phantom count, an
exclusive lock on the file, rollback to the last whole line on a partial write, poisoning when the
rollback cannot be proven, and bounded loss accounting at teardown. This subject is its second
caller and `resources.jsonl` would be the third, so extracting it is no longer speculative.

Its backpressure now stops at the subscriber. The writer can make the CLI's subscription wait, and
a subscription that waits too long falls behind and loses a counted gap; nothing the writer does
can make the session wait. So `events.jsonl` can miss events, which a writer fed directly by the
session would have ruled out with backpressure. Every miss is counted with the writer's other
losses, and a jump in the file's ids shows where each gap is. A record that must be complete is
the mandatory sink below, and not in this phase.

Two neighbors to keep distinct. `Transcript` is a public sink for podman and buildah transcripts
whose future `plan/todo/README.md` records as undecided; it is a text log, not an event stream,
and this subject should not absorb it. And `boundary-policy.md` is the other half of a hosted
call's record: it decides what crosses, before the call is dispatched. This page records what was
decided and what happened, and makes no enforcement claim. An event is a record of what the session
observed, not a grant, and a call denied before dispatch is denied whether or not anyone is
subscribed.

## Not in this phase

- **A mandatory audit sink** (`plan/next/mandatory-audit-sink.md`): a subscriber whose failure to
  take an event stops admission, so that no hosted call runs without a record.
- **Per-audience projections** (`plan/next/event-audience-projections.md`): different views of one
  event for the agent, an approver, the CLI's file, and an embedder's interface -- a host path an
  approver needs to see, for one, and a generic subscriber should not.

## The renderer

A single Python script that reads a session *directory* -- `session.json` for what the session was,
`events.jsonl` for the timeline, `network.jsonl` when it is there -- and writes one self-contained
HTML file. Deliberately minimal: a timeline, per-agent REPL history with tracebacks, the messages,
and a token summary. Nothing interactive, no server.

That is where it starts, not where it is expected to stay: the script is expected to become a
live monitoring page, reading the stream while the session runs and coupled more closely to
OutRig than a script over a directory is. How it reaches the stream is that work's question, and
the events do not change for it. A monitor that reads a lossy stream, or joins it late, needs a
current view to recover from; a coalescing latest-state snapshot, carrying the sequence number it
is current to, is `potential/live-state-snapshot.md`, an alternative to evaluate rather than
planned work.

Dependencies are declared inline and `uv` provisions them, which is the whole point of a PEP 723
block:

```python
# /// script
# requires-python = ">=3.12"
# dependencies = ["jinja2>=3.1"]
# ///
```

```
uv run --script scripts/render-session.py <session-dir> [--out PATH]
```

The page goes to `<session-dir>/report.html` by default, owner-only like `events.jsonl`, because it
holds the same things. `0003-14` records why it is not the current directory.

`uv run --script` rather than `uvx`: `uvx` runs a command from a published package, while PEP 723
inline metadata is what `uv run` reads from a local script.

The standard-library-only rule that `scripts/audit-doc-style.py` states for itself does **not**
apply here, and the reason it gives is why: "no `pip install` step needed in CI." That script is a
CI gate. This one is a thing a person runs against a session directory, with `uv` doing the
provisioning, so the constraint it was written under is absent.

A template engine is the example worth naming, because it earns its place on correctness rather
than convenience. Everything this script renders is hostile-shaped text going into HTML --
model-authored Python, tracebacks, captured subprocess output, message bodies. Hand-assembled
markup is how that becomes an injection in the viewer, and escaping every field by hand is the
kind of correctness that should not be re-derived per page.

Enable autoescaping explicitly. Jinja's `Environment` defaults `autoescape` to `False`, so a
template engine is not safe by having been imported -- a bare `Environment()` renders
`<script>alert(1)</script>` verbatim. The inputs to test against are the ones this record is made
of: submitted source, tracebacks, captured output, and message bodies.

Few dependencies, though. The inline block is an inventory a reader can check before running
something over their own session, and it is worth keeping short enough that they actually do.

## Opt-in

**Decided in `0003-13`: `[events] mode = "off" | "record"`,** defaulting off. `0003-13` merged it
as `[network].mode` merges, a repository's value replacing the global one. **Decided since (the
maintainer, 2026-10-02): the global config's value stands.** When the global config sets
`[events] mode`, that is the mode; a repository's value applies only when the global config says
nothing. The reason is whose file it is: the record is the user's own recording of their own
session, and a cloned project must not switch it on -- retaining the conversation and the argument
previews the user did not ask to keep -- or off. This is the same shape as the policy layer, and
`0003-23` lands it with that layer. There is no per-session CLI flag yet;
`plan/next/run-new-flag-parity.md` holds `--events`. Opt-in because the record holds the
conversation.

The mode governs the file, not the stream. An embedder's subscribers receive events whatever the
mode says, because what an embedder keeps is its own decision; the mode is the CLI's decision about
what it writes to disk.

**Boundary events follow the same opt-in (the maintainer's call).** They reach `events.jsonl` only
when the mode is on. The consequence is worth stating, because the default policy is allow,
published as events: with no rules configured, every hosted call runs and is evented
(`boundary-policy.md`), and with the mode off the CLI writes none of those events anywhere. A user
who wants a record of what an agent did through its bindings turns the mode on. Boundary events
carry argument previews, the same kind of content the opt-in already guards, so they are not
exempt from it.

## Rejected alternatives

**Rejected: a socket to attach to.** The JDWP shape, and what the subject was first described as.
Rejected because everything wanted here is a record or a subscription rather than a live
interrogation, and because it would add a listening surface: `outrig mcp --listen` already carries
the warning that "v1 has no built-in auth," and a port serving an agent's full history deserves
better than that before it exists. The in-memory subscription `embedding.md` makes public is not
this: a subscriber is code the owner runs in its own process, so nothing new can connect. The
events do not change if a socket is added later, which is the property worth keeping.

**Rejected: a stream that waits for its slowest subscriber.** It would make every subscriber's
record complete, the file included, as the earlier design did by letting the file's writer apply
backpressure to the session. Rejected because a stalled disk or a slow embedder would then stall
rounds, executions and shutdown, and a subscriber is never allowed to do that. A subscriber whose
record must be complete belongs to the mandatory sink, which stops admission instead of slowing
everything.

**Rejected: a file per subject.** Consistent with `network.jsonl`, and it loses the ordering that
is the reason to look.

**Rejected: one rule for every category.** The original was "the record holds exactly what the
model saw", which is right for the model-view category and wrong as a whole -- execution
diagnostics are not model-visible at all, and the event set contradicted the rule the day it was
written. Rejected in favor of three rules, because a single one either forbids diagnostics that
are clearly worth keeping or licenses capture nobody argued for.

**Rejected: recording an execution's full output.** Tempting, because "what did the command
actually print" is a real question the model-view record cannot answer. Rejected because it
creates capture that does not otherwise exist, in a file that outlives the session, which is the
disclosure the integration-audit warning above is about -- and because the agent has better tools
for it, which `history.md` records.

## Open questions

- Whether `events.jsonl` is reachable through `outrig logs`. Settled no by `0003-13`, on the
  grounds `network.jsonl` is not: it is not an MCP stderr log.
- Whether observability should eventually default on. It is the record a session was supposed to
  be, which argues yes; it is the conversation, which argues for a decision made deliberately.
- Whether the unowned session-record entries in `plan/next/` belong here --
  `session-record-error-variant`, `unreadable-session-records-are-unremovable`, and
  `dangling-session-symlink-is-invisible`. Two of them say they were deferred pending "its own
  design," and this is the first design that wants them.
- Where the renderer lives. Settled by `0003-14`: `scripts/render-session.py`, with the tree's
  description of `scripts/` widened to say it holds one thing meant for a user.
- Whether every event carries a causal parent -- which model turn produced which execution -- or
  whether ordering alone is enough. Hosted requests carry one, because a callback names the call it
  ran under (`0003-22`); for the rest, ordering is enough to read and not enough to query.
- How far a subscriber may fall behind before it loses events, and whether an embedder may choose
  that per subscriber.

## Unverified

- The stream's promise that a stalled subscriber never delays a round is `0003-19`'s acceptance
  and has not run. Its numbering has: `0003-13` assigns an event's id under one lock as it queues
  the event, so the id is the event's place in the file.
- The cost of emitting inter-agent messages was not measured. It is the one addition co-hosting
  forces, and a chatty pair of agents is the case to measure before assuming it is free.
- The CloudEvents attribute names, their required/optional split, and the lower-case-alphanumeric
  naming constraint were read from the v1.0 specification. The `type` prefix convention is a
  SHOULD, not a MUST.
- `AuditSink` is asserted to be reusable from its shape and its documentation, not from an attempt
  to extract it. The extraction is where that claim gets tested.
- The renderer's mechanics were confirmed rather than assumed: `uv run --script` resolved a PEP 723
  block declaring `jinja2>=3.1`, installed it with its one transitive dependency, and rendered a
  template whose autoescaping turned `<script>` into `&lt;script&gt;`. `0003-14` then ran it
  against a real `run-new` session directory, and records what that covered.
