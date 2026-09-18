# Observability

What a human can see of a running system. Sessions already exist for this -- `doc/usage/sessions.md`
opens by saying they are there "so you can go back and inspect what happened" -- but what they
record is the container, its connections, and the servers' stderr. The agent itself is absent, and
the same page still admits it at `:294`:

> **TODO: Incomplete** -- opt-in transcript capture (per-turn JSON of user/assistant/tool
> messages) is deferred.

This page is the answer to that, widened by what a Python agent makes visible. Three decisions are
settled before anything else, because they bound the whole subject:

- **The session directory is the interface.** Nothing listens. What a session already writes is
  what an observer reads, and the only new thing is another file beside `network.jsonl`.
- **Reading is read-only.** Nothing an observer does perturbs the session it observes. No message
  is injected, no execution is interrupted, no expression is evaluated.
- **Nothing is captured richer than its category allows.** Three categories, below, each with its
  own rule. An execution clipped to 16 KiB is recorded as 16 KiB, with the same truncation marker.

The third one carries the weight, and it started as a single rule -- "the record holds exactly what
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

**Integration audit** -- policy decisions and bounded call metadata, which is `call-inspection.md`'s
subject and carries its warning: the most useful field is also the one most likely to hold a secret.

What the single rule got right and the three must keep: capture is a decision with consequences,
not a free byproduct. Even recording exactly model-visible text changes its retention and its
audience, so "no new disclosure" was always too strong for a file that outlives the session.

## Most of this is already on a wire

The interpreter protocol is NDJSON and the host reads both halves of every exchange, so the
expensive part of watching an agent is already paid:

| message  | direction     | what it already carries                              |
|----------|---------------|------------------------------------------------------|
| `exec`   | host → interp | the source an agent submitted                        |
| `result` | interp → host | status, bounded output, and the traceback            |
| `inv`    | both          | a bounded name-and-type listing of what the agent holds |
| `msg`    | host → interp | a message arriving on the `user` channel             |
| `send`   | interp → host | a message the agent sent                             |

`inv` is already requested every thirty seconds by the liveness probe in `runtime-protection.md`,
so a periodic inventory costs nothing that is not already spent.

**Messages between agents are the exception, and co-hosting is why.** `agent-placement.md` puts
every agent in one interpreter process, and `messages.md` says a message between two of them never
leaves it -- it moves between event loops through `loop.call_soon_threadsafe`. Nothing about it
reaches the host. So an inter-agent message has to be **emitted deliberately**, as an observation
the interpreter sends because someone is watching rather than because delivery requires it.
That is a real cost co-hosting created, and it is the one place this subject is not free.

It is cheap in the way that matters, though: `messages.md` restricts a contract to the
serializable subset, so every body already has a JSON form, and `receive_delivery()` already
carries the sender and the arrival time.

## One stream, not one file per subject

`network.jsonl` and the `resources.jsonl` proposed in `plan/next/dynamic-resource-mounts.md` are
independent subjects, and interleaving a connection with a mount means nothing. An agent's
timeline is not like that. A model turn produced this code, which printed this, which sent this
message, which woke that agent -- the ordering *is* the information, and recovering it by
timestamp-joining separate files is exactly the thing that goes wrong at the moment it is needed.

So: **`<session_dir>/logs/events.jsonl`**, one append-only stream, ordering guaranteed by the
single writer that owns the file. This diverges from the per-subject convention on purpose.

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
 "subject": "agent/root",
 "time": "2026-09-21T10:30:07.412Z",
 "datacontenttype": "application/json",
 "data": {"execid": 7, "status": "error", "duration": 1.83,
          "output": "building...\n", "error": "Traceback (most recent call last):\n..."}}
```

`id` and `source` are the two required attributes that carry meaning here: the spec requires
`source` + `id` to be unique per event, and a session plus a sequence number satisfies that without
coordination. `source` is a URI-reference rather than a free string, which is why it is written as
a path. `subject` names the agent, which is what the protocol's agent id supplies.

`type` values take the `org.outrig.` prefix the codebase already uses for OCI and container labels
(`org.outrig.mcp`, `org.outrig.session`), which is also what the spec recommends -- a reverse-DNS
prefix naming whoever defines the semantics.

**One constraint decides the rest of the layout.** CloudEvents attribute names "MUST consist of
lower-case letters [a-z] or digits [0-9]", so the flat `outrig.session_id` style `network.jsonl`
uses for its extras is not a legal extension attribute. Nothing OutRig-specific goes at the top
level. Everything goes inside `data`, and the top level stays exactly the standard context
attributes. That is tidier than the alternative anyway.

## What is emitted

**The agent, from the protocol.** `exec.submitted` with the source; `exec.completed` with status,
the bounded output, the traceback, and a duration; `inventory.observed` with the names and type
names the probe already fetched.

**Messages**, per the emission above: `message.sent` and `message.received`, each with the
channel, the endpoint names at both ends, the sender, and the body.

**The model, from data the loop currently discards.** `llm.rs:1988` already calls
`.extended_details()`, which is rig's opt-in for usage tracking, and the success arm reads
`.messages`, `.output`, and `.content` while dropping `response.usage` and
`response.completion_calls` on the floor. So `model.round.completed` costs a field read: input,
output, total, cached, and reasoning tokens for the round, plus the per-turn breakdown. The
context high-water mark is the largest `input_tokens` across those calls -- rig's own
documentation points at the last entry for the final request's context length.

`model.retry` and `model.failover` are the same story one level down: `retry.rs:644` and
`failover.rs:351` format an attempt or a hop into `eprintln!` and drop it. A structured event also
retires a workaround -- today the failover chain's state is recovered by prefix-matching the
*error string* it was rendered into.

**Everything else that happens and is reported nowhere.** This is the part of the subject that
grows, and the first entries are already known: a tool result truncated (`rig_tool.rs` writes a
marker into model-visible text and counts nothing); the effective `max-tokens` ceiling, which never
escapes `build_agent`; agent lifecycle, including an agent that wedged past recovery; an interrupt
sent and a liveness probe that failed; a `MemoryError` raised by the address-space ceiling.

**A promotion is an event**, and not an optional one. `history.md` lets an agent move something
from its full history into the context the provider sees. Without a record of that, the stream
shows a model suddenly citing something it was never sent, with no account of how it got there.
A promotion is a request, though, and not proof of what was sent: the window moves, duplicates
collapse, the budget evicts, and a retry or a failover reassembles everything. `history.md` makes
the per-call manifest authoritative for what a model actually received, and this event explains
how a thing came to be eligible rather than that it arrived.

It also settles a question left open in `agent-placement.md`: the unattributed-output bucket --
what reaches fd 1 without belonging to any agent -- is **logged as an event**. It cannot be billed
to an agent and it should not be silently dropped.

## Nothing new becomes public

`harness-components.md:80` leaves `session.rs` and `paths.rs` in `outrig-cli` and says to "revisit
if the library loop needs a session record of its own." This is that need, and the revisit
concludes no: `NetworkInterceptor::new(log_dir, ..)` already shows the shape. **The caller supplies
a directory; the library owns the writer and the schema.** Host conventions stay where they are,
and the library's public surface does not move.

That is not a preference. `plan/todo/0002-47` and `0002-48` are narrowing and then CI-gating
`crates/outrig/public-api.txt`, and this phase's entire budget is the one entry point `outrig-cli`
needs. A subject that published event types would spend a budget that is already committed.

The writer itself is not new work. `AuditSink` and `audit_writer` in `crates/outrig/src/network.rs`
are generic in everything but the record type: a bounded queue that applies backpressure rather
than dropping, a reserve-then-send enqueue so a cancelled producer leaves no phantom count, an
exclusive lock on the file, rollback to the last whole line on a partial write, poisoning when the
rollback cannot be proven, and bounded loss accounting at teardown. This subject is its second
caller and `resources.jsonl` would be the third, so extracting it is no longer speculative.

Two neighbors to keep distinct. `Transcript` is a public sink for podman and buildah transcripts
whose future `plan/todo/README.md:110` records as undecided; it is a text log, not an event
stream, and this subject should not absorb it. And `call-inspection.md` is the other half of the
same coin -- it says plainly that a passive tap "is observability, and it is not a boundary." This
page is that tap, deliberately, and makes no enforcement claim.

## The renderer

A single Python script that reads a session *directory* -- `session.json` for what the session was,
`events.jsonl` for the timeline, `network.jsonl` when it is there -- and writes one self-contained
HTML file. Deliberately minimal: a timeline, per-agent REPL history with tracebacks, the messages,
and a token summary. Nothing interactive, no server.

Dependencies are declared inline and `uv` provisions them, which is the whole point of a PEP 723
block:

```python
# /// script
# requires-python = ">=3.12"
# dependencies = ["jinja2>=3.1"]
# ///
```

```
uv run --script scripts/render-session.py <session-dir> --out report.html
```

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

A config block with a mode, mirroring `[network]`, defaulting off, with a CLI override for one
session. `sessions.md:294` already frames transcript capture as opt-in, and the record holds the
conversation.

## Rejected alternatives

**Rejected: a socket to attach to.** The JDWP shape, and what the subject was first described as.
Rejected because the session directory already is the interface, and everything wanted here is a
record rather than a live interrogation. It also avoids a second listening surface: `outrig mcp
--listen` already carries the warning that "v1 has no built-in auth," and a port serving an agent's
full history deserves better than that before it exists. The events do not change if a socket is
added later, which is the property worth keeping.

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
disclosure `call-inspection.md` is careful about -- and because the agent has better tools for
it, which `history.md` records.

## Open questions

- Whether `events.jsonl` is reachable through `outrig logs`. `network.jsonl` deliberately is not,
  on the grounds that it is not an MCP stderr log, and the same argument applies here.
- Whether observability should eventually default on. It is the record a session was supposed to
  be, which argues yes; it is the conversation, which argues for a decision made deliberately.
- Whether the unowned session-record entries in `plan/next/` belong here --
  `session-record-error-variant`, `unreadable-session-records-are-unremovable`, and
  `dangling-session-symlink-is-invisible`. Two of them say they were deferred pending "its own
  design," and this is the first design that wants them.
- Where the renderer lives. `scripts/` is described in the tree as repo-local tooling, and this is
  the first thing there meant for a user.
- Whether an event carries a causal parent -- which model turn produced which execution -- or
  whether ordering alone is enough. Ordering is enough to read; it is not enough to query.

## Unverified

- The cost of emitting inter-agent messages was not measured. It is the one addition co-hosting
  forces, and a chatty pair of agents is the case to measure before assuming it is free.
- The CloudEvents attribute names, their required/optional split, and the lower-case-alphanumeric
  naming constraint were read from the v1.0 specification. The `type` prefix convention is a
  SHOULD, not a MUST.
- `AuditSink` is asserted to be reusable from its shape and its documentation, not from an attempt
  to extract it. The extraction is where that claim gets tested.
- The renderer's mechanics were confirmed rather than assumed: `uv run --script` resolved a PEP 723
  block declaring `jinja2>=3.1`, installed it with its one transitive dependency, and rendered a
  template whose autoescaping turned `<script>` into `&lt;script&gt;`. What was not tested is any
  of it against a real session directory, because none exists yet.
