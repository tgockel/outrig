# Diagnostics

What a reader of a session's records can rely on: which OutRig wrote them, how they join, what each
audience receives, and what the stream, the subscription and the report already give. This page
decides three things and leaves the events of the new facilities to the pages that build them.
`0003-66` adds a record naming the producer -- OutRig's name and version, and the event schema's
version -- as the first fact of a session's stream and file, and gives the renderer its rule for a
type or a version it does not know. `0003-67` closes the correlation gaps where an id exists and is
not carried, says which ids the host minted and which the interpreter claimed, separates
session-local ids from the durable task identity an owner supplies, and writes the rule every new
event family follows. `0003-68` classifies each field where the event is made and projects by
audience before delivery. The requirements are OR-OBS1 through OR-OBS5. The validation of
2026-10-28 found them largely met -- one stream, counted gaps, a public subscription, a report
independent of readers -- with these additions: producer-version fields; correlation where it is
missing; redaction by audience; and the rule that each new facility's task adds its own events,
which `observability.md` already states ("The boundary"). Nothing on this page reverses a recorded
decision. The sentences the tasks change are reference statements, and each task lists them.

CocoClaw is the first application and the running example. No mechanism here names it: an owner's
durable task identity is an opaque string, an embedder is any program that subscribes, and the
operator is whoever runs OutRig on the host.

## What exists

The subject's foundations are built and public, and the tasks here extend them rather than add a
second system.

- **One stream, numbered at publication.** `observability.md` ("One stream, not one file per
  subject") decided it, and `crates/outrig/src/events.rs:203-237` builds it: one lock gives each
  event its id and hands it to every subscriber's queue; a full queue drops its oldest and counts
  it (`events.rs:299-316`). `events.jsonl` is one subscriber of the stream (`events.rs:440-485`),
  in a CloudEvents 1.0 envelope whose top level carries only the standard attributes
  (`events.rs:572-600`; `doc/reference/events.md`, "The envelope").
- **A public subscription**, made on the builder before the start so it sees every event from the
  first (`crates/outrig/src/harness/builder.rs:289-301`), yielding `Received::Event` or
  `Received::Missed(n)` (`crates/outrig/src/harness/event.rs:1313-1319`). A host program's
  `Session.events()` is the same subscription pushed over the owner connection as the file's
  records (`crates/outrig/src/owner/session.rs:446-462`;
  `crates/outrig/src/python/outrig/_host.py:404-417`, `:776-795`).
- **A report that never depends on a reader** (`lifecycle.md`, "The report";
  `crates/outrig/src/harness/report.rs:19-37`): what closed admission, whether the session is
  proven stopped, each outcome live at the close, and `EventDelivery` -- the last sequence
  published, how many events each subscription lost, and what the file did not get
  (`report.rs:169-191`).
- **The catalog**: 66 kinds in `Payload` (`harness/event.rs:168-685`), named in `kind()`
  (`:689-757`), with the subject rule (`:763-781`) and the id types (`:114-156`). Every type is
  `#[non_exhaustive]`, so a field or a kind can be added without a break.
- **The semantics OR-OBS2 asks for**: every provider request recorded once, a late usage record
  replacing a null once, aggregates from unique attempts never adding a parent's inclusive total
  to a child's (`crates/outrig/src/agent/ledger.rs:113-168`; `events.md`, "Totals"); each model
  call's manifest of what it carried, evicted, withheld and left out (`events.md`, `model.call`;
  `history.md`, "Record the view that was actually sent"); and a shutdown report whose stop proof
  and unknown outcomes are fields, not narration (`lifecycle.md`, "The report").
- **The renderer**, `scripts/render-session.py`, reads a session directory
  (`render-session.py:970-1038`), checks the record's health (`:822-858`), shows an unknown type as
  its escaped `data` (`:466-479`), and joins a tool call to its execution by matching source text
  (`:447-456`). Everything on the page is escaped, nothing on it runs, and nothing loads from
  anywhere (`observability.md`, "The renderer").

## Records name their producer (`0003-66`)

CloudEvents' `specversion` versions the envelope. Nothing versions the payloads: a reader of
`events.jsonl` cannot tell which OutRig wrote a record, and a renderer meeting a type it does not
know cannot say whether the type is new or the record is damaged. The companion note that fed
OR-OBS1 says it directly: "CloudEvents `specversion=1.0` versions the envelope, not every payload.
Record producer/renderer versions and define compatibility behavior."

**The producer record.** `session.producer` is the first event of every session: no subject,
`name` (`outrig`), `version` (the crate's version, `env!("CARGO_PKG_VERSION")`) and `schema`, an
integer naming the event schema. `0003-67` adds `task`, the durable task identity below. It is
published where the first event is published today: `Lifecycle::starting`
(`crates/outrig/src/harness/lifecycle.rs:38-49`) emits `session.state` `starting` as a session's
first event, and every start path reaches it -- `SessionBuilder::start` through `open_stream`
(`builder.rs:506-527`), `start_on_host` (`:592-598`), and the test path that hands the builder a
stream. The record goes ahead of `starting` there, so it is id 1 of the stream, line 1 of the file,
and the first `Event` a host program's `Session.events()` yields.

**The schema's version.** One number, `harness::event::SCHEMA`, starting at 1. An additive change
-- a new kind, a new field, a new variant of an enum a field holds -- keeps the number: every type
in the catalog is `#[non_exhaustive]` for that reason, and a reader that meets a field or a kind it
does not know keeps reading. A field removed or renamed, or one whose meaning changes, moves the
number. `events.md` gains a "Versions" section carrying that rule and the current number.

**The reader's rule.** A reader -- the renderer, an embedder's UI, a program replaying a file --
treats a type it does not know as a record to keep and show, never as an error and never as
something to drop: its kind as recorded, its `data` as data, cut to a bound with the omitted size
said, and a notice that the reader does not interpret it. A `schema` greater than the reader
knows gets a notice too: fields the reader does know may have changed meaning. A file with no
producer record was written before this task, and the reader says so rather than fail. The
renderer applies all three, and shows the producer's name, version and schema in its facts.

**A version a program can read without the stream.** `Session::producer()` returns the same facts
as a value, and a host program's `session.producer` mirrors it after entry. The stream is enough
for a subscriber that reads from the start; a reader that lets 4,096 events queue before reading
loses the oldest (`_host.py:579-610`), and the first record with them, so the value stands beside
the record.

**Two renderer notes change with this.** "The event log is empty: the agent never started."
(`render-session.py:829`) claims more than an empty file shows; OR-OBS3 says "Missing evidence
cannot prove an agent never started", and the note becomes a statement that nothing was recorded.
And each timeline row names its record's id, so a reader can find the record a formatted row was
made from: for an operator that is the line in `events.jsonl`, reached by id; the redacted
raw-envelope access OR-OBS4 asks for an embedder is the embedder projection with `Event::redacted`
("Audiences"), reached by the same id, never the file.

**Reference statements the task changes.** `events.md`, "Execution diagnostics", `session.state`:
"The first event of a session is `starting`"; `_host.py:786`: "the first a `session.state` of
`starting`"; `lifecycle.rs:37`: "A session starting: the first event it publishes says so." Each
becomes a statement that the producer record comes first.

## Identities

Every id a record carries is minted inside one session and means nothing outside it. The table
says who mints each and where it travels.

- `source`, `/outrig/session/<id>` -- minted by the builder, once per session; on every
  record's envelope.
- `subject`, `agent/primary` and `agent/c<N>` -- minted by the host, in launch order
  (`children.rs:462`); on every event about an agent.
- `execid` and `message` -- minted by the interpreter's one counter (`python/host.rs:977`); on
  `exec.*`, `message.*` and `hosted.received`.
- `call_id` and `attempt_id` -- minted by the ledger (`ledger.rs:234-240`); on every model
  event.
- `request`, of a hosted request -- minted by the relay (`python/hosted.rs:255-258`); on
  `hosted.*` and the report.
- `request`, of a child -- minted by the interpreter's counter; on `agent.request.*`,
  `agent.call.*` and the report.
- `invocation` -- minted by the interpreter's counter; on `skill.*`, `agent.call.*` and
  `hosted.received`.
- `instance` -- minted by the interpreter; on `agent.instance.*`.
- `round` and `turn` -- minted by each agent's own history; on round, turn and call events.

**A durable task identity is the owner's, and OutRig carries it unread.** OR-OBS2 asks that
session-local ids be qualified "separately from durable task/claim identity", and the governing
shape makes one durable task span several sessions: a resume is a new session for the same task
(`continuation.md`; `lifecycle.md`, "No restart in 0.3"). So the owner may give the builder one
opaque string, `SessionBuilder::task(id)`, and a host program `outrig.Session(task=...)`, which
crosses in the owner protocol's `session.start` beside `workspace` and `mounts`
(`crates/outrig/src/owner/mod.rs:19-21`). The producer record carries it as `task`, `null` when
none was given; nothing else does, and OutRig never parses it. A reader that joins the sessions of
one task joins on `task`; `agent/c1` in one session is not `agent/c1` in the next, and `source`
changes with every session. The CLI gives none: `run-new` has no durable task.

**An id the host minted is not an id the interpreter claimed.** `hosted.received` carries
`execid` and `invocation` "as the interpreter said: diagnostic context, which the agent's own code
can change" (`events.md`; `hosted.rs:633-634`, `:696-712`), while its `request` is the relay's and
its `parent` the binding's. `skill.imported.agent` and `agent.call.started.invocation` are claims of
the same kind. `0003-67` has `events.md` say, for each id field, whether the host minted it or the
interpreter reported it, and the renderer labels a join made on a claim, as it labels its
source-text match. A reader joins on the host's ids and reads the claims as context. This is
OR-OBS2's "label heuristics", applied to the one heuristic the record has and the one class of
field that is not a fact of the host's.

**"Attempt" has two meanings, and each event uses one.** OR-OBS2's "round/attempt" conflates
them (conflict check, tension 7; validation, "Wording in the requirements to fix"). A *provider
attempt* is one request actually sent to a provider: `attempt_id`, unique within the session,
one per retry or failover hop under one `call_id` (`ledger.rs:238-240`; `observability.md`, "Every
model event ... carries two ids"). An *answer attempt* is one answer a child gave a request that
failed its check: `agent.request.replied.attempt`, `agent.request.invalid.attempt`,
`agent.request.settled.attempts` and `agent.request.sent.attempts_max`, three by default, and only
an invalid reply counts one (`agent-classes.md`, "Only an invalid reply counts toward the attempt
limit"). The requirement's word means the first. A third field, `model.retry.attempt`, is a
retry's ordinal within one candidate's retries (`events.md`, `model.retry`), neither of the two.
`0003-67` writes the three definitions into `events.md`'s "Integration audit" introduction, so a
reader of any event knows which it holds; whether the third field is renamed before the 0.3.0
release fixes the file's shape is that task's fork.

## Events correlate where they can (`0003-67`)

OR-OBS2 asks that "interleaved children/calls can be joined without ID collisions or guessed
causality". The ids exist; four joins are not carried.

1. **An execution to the tool call that asked for it.** `exec.submitted` carries `execid` and
   `source` (`host.rs:1011-1014`); the tool call's id is in the turn's messages
   (`turn.committed`); the renderer joins them by equal source text, first come first served
   (`render-session.py:447-456`), and two identical submissions in one round can be joined
   crosswise. `exec.refused` carries no source at all (`host.rs:981-985`, `:1000-1004`).
   `plan/next/exec-events-name-their-tool-call.md` proposed the fix and is absorbed here: the id
   is in hand where the round's hook sees the call about to run -- rig 0.40's
   `StepEvent::ToolCall` carries `tool_call_id`, the provider's, and `internal_call_id`, rig's
   (`rig-core-0.40.0/src/agent/hook.rs:516-525`; the arm at `agent/round.rs:670-690`) -- while the
   tool's own `call` receives only `args` (`rig-core-0.40.0/src/tool/mod.rs:292`;
   `agent/tool.rs:207-218`). rig runs a turn's calls one at a time and in order
   (`round.rs:375-380`), and the hook and the tool already share `Interrupts` (`tool.rs:61-67`;
   `round.rs:83`), so the hook can leave the call's id where the tool's submission reads it.
   `exec.submitted` and `exec.refused` gain `call`, the provider's id or `null`; `exec.refused`
   gains `source`.
2. **A child's round to the requests it opened on.** `round.started` carries `round` alone
   (`agent/mod.rs:155-157`, `:187-189`). The runner holds the summary -- the requests unread and
   taken, by id -- as it opens the round (`agent/runner.rs:100-134`), and the registry marks each
   open request with the round (`children.rs:1130-1148`), which is how `agent.request.received`
   carries `round` and `agent.request.settled` carries `rounds` (`events.md`). The join from the
   round's side is missing: `round.started` gains `requests`, the ids the round opened on -- a
   work child's one submission, a request child's unread and taken ids -- an empty list for an
   orienting round, which opens on no request (`agent-classes.md`, "The orienting round"), and
   `null` for the primary, whose rounds open on messages the announcer counts and does not name.
3. **A message the agent's code took to the execution that took it.** The interpreter's `took`
   line names the channel and the message (`interpreter.py:3654`; `host.rs:2269-2278`) and not the
   execution, though the kernel keeps the current execution in a context variable
   (`interpreter.py:186`). `message.received` gains `execid`, a claim like `hosted.received`'s,
   `null` when the receive ran outside every execution.
4. **A hosted or MCP call to its execution and invocation.** `hosted.received` carries both, as
   claims, and `parent` for a callback's call (`events.md`). An MCP call is new (`mcp-presenter.md`,
   `0003-56`), and its events carry the same three under the same labels; this page does not define
   them.

**The rule for every new event family.** The pages that define new families cite it: `mcp.*`
from `mcp-presenter.md`; the external-channel events from `external-channels.md`; the facility and
instance events from `facilities.md` and `task-agents.md`; the provisioning records from
`application-code.md`; and the assessment, designation, cut, publication, cleanup and
reconciliation events from `continuation.md`.

- Its subject is the agent the event concerns, or none for an event about the session or a
  process every agent shares (`events.md`, "The envelope"; `event.rs:763-781`).
- It has one id of its own, minted on the host, that every event of the family carries, as
  `request`, `instance` and `invocation` do today. A reader joins the family on it.
- It carries the session-local ids of what caused it that the host holds -- `round`, `execid`,
  `invocation`, a child's `request` -- and `events.md` says of each whether the host minted it or
  the interpreter reported it.
- A durable reference -- a backend's delivery id, a server's request id, the owner's task -- is an
  opaque string beside the session-local id, never in its place, and OutRig does not read it.
- A body or an argument is a bounded preview under `events.md`'s "Three categories", built from
  by-value data and never by calling into an object (`boundary-policy.md`, "Events").
- It uses one meaning of "attempt" and says which.
- Its fields are classified for audience when the family is added (below).
- It adds its rows to `events.md`, and to `lifecycle.md`'s close table where the thing it is about
  can be running at the close.

`observability.md`'s open question -- "whether every event carries a causal parent" -- is answered
narrowly: the four joins above, and the rule that a family carries the ids the host holds. For the
rest, "ordering is enough to read and not enough to query" stands.

## Audiences (`0003-68`)

Today every subscriber receives the same event: `Stream::publish` builds one `Arc<Event>` and
hands it to every queue (`events.rs:203-237`). One split exists outside the stream: a hosted call's
host traceback stays in its `hosted.ended` and out of the error the agent reads
(`hosted-objects.md`, "Exceptions"; `crates/outrig/src/python/binding.py:1181`, `:1405-1411`).
`boundary-policy.md` ("Events") and `observability.md` ("Not in this phase") point at
`plan/next/event-audience-projections.md` for the rest, and that entry is absorbed here.

OR-OBS4 asks that prompts, source, output, bodies and tracebacks be treated as untrusted and
sensitive, that injected secrets stay out of every view, and notes that "Escaping is not
redaction." The renderer escapes; nothing redacts. A UI that shows records to someone who is not
the host's user needs fields it may show, decided before the record reaches it.

**Three classifications, two subscriber audiences.** Each field of each event is classified where
the event is made, in the catalog, as one of:

- *agent* -- may reach the agent's own kernel as text: a refusal's reason, an exception's type,
  OutRig's own messages. Not a subscription: the agent reads what the interpreter is sent, and
  `binding.py` already keeps the traceback out of that (`:1405-1411`). The classification puts the
  existing rule in the table with the others.
- *embedder* -- may reach a program that owns the session and may show records to others: ids,
  outcomes, kinds, type names, durations, counts, the model's own text and source, the bodies on
  channels the embedder itself supplied, and the previews of what one agent gave another.
- *operator* -- reaches the operator's file and a subscription declared the operator's: text that
  came through the boundary from the host side or names the host -- a host library's traceback, a
  host path such as `binding.refused.declared_in`, the command lines in a report's `survivors` --
  and the argument and keyword previews of boundary requests, since "a token in a remote URL, a
  password in a command line" is the case `boundary-policy.md` warns of ("Events").

A subscription declares its audience, *embedder* or *operator*, when it is made. `events.jsonl`
takes the operator projection: it is "the user's own recording of their own session"
(`observability.md`, "Opt-in"), owner-only on disk, and the operator already holds the
environment any secret in it came from. `run-new`'s terminal subscription (`run_new.rs:168`) is
the operator's too. `SessionBuilder::subscribe()` and a host program's `Session.events()` default
to the embedder's, and each can ask for the operator's.

**Projection happens before delivery, on the trusted side.** `publish` builds one event per
audience that has a subscriber, under the same lock, and a field outside the audience takes its
absent form -- `None`, an empty list -- while the event names it in `redacted`, so a reader can
tell a field withheld from one never recorded. Building a projection never calls into an object:
previews are already by-value (`boundary-policy.md`, "Events"), and a projection only drops them.
A test holds the classification complete: every field of every variant, as
`each_event_has_exactly_its_data_fields` (`crates/outrig/src/events_tests.rs:121`) holds the
field names.

**The honest limit.** Redaction is by field, never by content: no projection scrubs a token out
of a string, because "no library-neutral rule can tell a token from text" (`hosted-objects.md`,
"Exceptions"; conflict check, C8; validation, constraint 10). So a hosted library that echoes its
environment into an exception's arguments puts that text into `hosted.ended`'s operator fields,
into the operator's file, and into the one-line rendering the agent reads -- the agent audience's
text is unscrubbed too -- while the embedder projection withholds the field whole. An MCP presenter
builds its events in Rust from fields OutRig knows (`mcp-presenter.md`), so a header or an
environment value OutRig injected is never placed in one: OR-MCP4's "never appear in evidence"
holds for the secrets OutRig injects, and not for content a server returns, which OR-MCP4 itself
exempts. The promise for hosted objects is non-injection (`security.md`, "The old guarantee,
restated") plus projection by audience; the promise for MCP is non-injection in every field.

## A bounded interface for a reader (OR-OBS3)

Met by what exists, with `0003-66`'s producer record. The subscription is the push interface and
the file the pull one; OR-OBS3 leaves the choice open, and both stand. Each record is delivered
once as it is published, so an embedder that stores records stores them as they come and never
receives accumulated history again; what it keeps, and for how long, is its own
(`observability.md`, "Opt-in": "what an embedder keeps is its own decision"), apart from the saves
`continuation.md` defines.

The stream is lossy and counted, by citation: a lagging subscriber "loses the oldest events it
has not taken and is told how many", and "every gap in what a subscriber saw is a counted one"
(`observability.md`, "One stream, not one file per subject"); the report says what each
subscription missed and what the file did not get (`lifecycle.md`, "The report";
`report.rs:169-191`); non-invocation is read from an authoritative outcome or from a history
known to be complete, "and
never from a dispatch event that is absent across a gap, from a file cut short, or from a receipt
with nothing after it" (`boundary-policy.md`, "Events"); and an owner that dies leaves no report,
so the tail of its record is unknown, with no exact loss count promised (`boundary-policy.md`,
same section; validation, constraint 6). From constraint 6 also: an acknowledged save
(`continuation.md`, `0003-63`) is acknowledged by the adapter, never by an event having been
published. Missing evidence proves nothing, which `0003-66`'s renderer wording states. Replay
renders records and executes nothing (`observability.md`, "The renderer"). A monitor that joins
late or falls behind has no current picture; the recovery alternative is
`plan/phase/0003-python/potential/live-state-snapshot.md`, not planned work. A record that must be
complete is `plan/next/mandatory-audit-sink.md`, out of this phase's scope (`README.md`, "Out of
scope").

## Evidence from the new facilities (OR-OBS5)

Each facility's task adds the records that explain what it did, under the rule above; this page
defines none of them. OR-OBS5 asks that runtime records "describe safe capability names/managed
identities, delegation and normal exposure, not same-VM security", and that suspend records
"distinguish assessment, preparation, designated state, publication, cleanup and reconciliation",
so that an application never infers these from dirtiness or from its own instrumentation. The
checklist, by task:

- `0003-41` -- the construction outcome: the hook ran, or the constructor or hook raised, with the
  exception's type, before any instance event and with no model round spent.
- `0003-42` -- the facility manifest on `agent.spawned` and `agent.instance.started`: each entry's
  name, its kind, its managed identity (a binding's or a service's name), and whether it is the
  parent's entry handed on; names and types, never values.
- `0003-43` -- provisioning: the application code's identity (a digest), what imported and what
  did not with the diagnostic, and on resume the identity compared with the save's.
- `0003-45` -- where the instance lives: a kernel-constructed `self`, and the collection backstop
  withdrawn for it.
- `0003-46` -- the root's parent is `owner`, and the handle's events under the root's subject.
- `0003-47` -- the instruction composition: which part came from OutRig's rules, the class, the
  application's contributions and the manifest, so a reader sees that an omitted facility was not
  advertised.
- `0003-51` through `0003-53` -- an arrival with its delivery id and backend reference, whether it
  woke or was announced, the agent's receipt, each send's outcome -- accepted, refused, or
  uncertain -- with the stable identity to resolve it by, a request's terminal outcome and the
  warning and cancel `0003-37` gives it, and the reconciliation of a backend's records.
- `0003-54` through `0003-57` -- each service's placement and start outcome with its class and a
  redacted cause; discovery and a later listing's changes; each call's service, tool, call id,
  execution, invocation, dispatch state, error category, deadline, cancel attempt and duration,
  with no mandatory body; the teardown.
- `0003-58` -- export and import: the format's version, the turns carried, and each opaque part
  carried or omitted.
- `0003-60` -- an assessment: the facts reported, whether the model was consulted, the advice's
  freshness, and an unknown said as unknown -- distinct from any preparation.
- `0003-61` -- the preparation round opened, for a sleep or a save, and the designation: what was
  named or that nothing was, and what was omitted or unsupported.
- `0003-62` -- the pause opened and reopened; each writer quiesced, or the refusal naming the
  blocker; the cut's history position and selected manifest; any degradation.
- `0003-63` -- staged, committed, published with the acknowledgement's state -- acknowledged,
  uncertain, failed -- the checkpoint's identity and order, and the resource-stop proof.
- `0003-64` -- the reconciliation notice: what was restored and what omitted, lost names and
  handles, changed services, open obligations, uncertain effects, and the application code's
  identity check.
- `0003-65` -- each open request's disposition at the cut, each child ended or refused, and each
  uncertain hosted or MCP call.
- `0003-44` -- a service: its binding by name and description, each call's method, agent, execution,
  dispatch and outcome with the duration and no body by default, and the cancel at shutdown.
- `0003-48` and `0003-49` -- the stop read and the roster are reads, not records; what they report
  is already in the round and request families, and the reads add nothing to the stream.
- `0003-50` -- the limit: each warning with its threshold and the count remaining, the stop with
  `spent` and `limit`, each request settled `limit`, and each reset or raise with who asked; the
  count and limit in the continuation metadata.
- `0003-59` -- selection: each move of the pin with its failure class, each pass-over that left it,
  and the member restored by name or the fallback to the head at resume; the selection in the
  continuation metadata.

Two warnings from the companion note hold for every row: a reader must not infer preservation
from dirty or untracked status, and no record carries a variable's value -- the inventory is names
and types (`events.md`, "Execution diagnostics").

## Rejected alternatives

**Rejected: a schema version on every record.** As an envelope extension attribute, it would
reverse `observability.md`'s decision that "Nothing OutRig-specific goes at the top level"; inside
every `data`, it would cost a field per event for a fact that does not change within a session.
The first record names it once, every subscription begins at the first event, and
`Session::producer()` covers a reader that lost it.

**Rejected: redaction by content.** A string filter over free text -- for tokens, URLs with
credentials, paths -- is the open question the absorbed entry recorded ("No string filter tells a
secret from a path, a URL or a commit message") and the rule `hosted-objects.md` already states.
Fields are typed by their producer, and a projection withholds a field whole.

**Rejected: filtering in the subscriber.** An embedder could drop fields itself. Then each
consumer removes what its audience should not see "from fields nobody classified when the event
was made" (the entry), and a new field reaches every consumer unclassified. Classification belongs
where the event is made, once.

**Rejected: a causal parent on every event.** `observability.md` left it open and this page
answers narrowly: the joins above carry ids the host holds, and nothing invents a parent where the
host has none.

**Rejected: a renderer laid out by child.** `plan/next/render-session-lays-out-children.md`
proposes it, and it is not an OR-OBS requirement; the renderer's changes here are the ones the
producer record, the new joins and the health notes need. The entry stays.

**Rejected: a live view.** `observability.md` rejected a socket to attach to and settled the
renderer as a directory reader with `--host` rebuilding the page per request. Unchanged.

## Open questions

- Whether `session.json`, which `outrig-cli` writes, names the OutRig version too. The producer
  record covers every session the library runs; the CLI's record is the CLI's.
- Whether `model.retry.attempt` is renamed before the 0.3.0 release fixes the file's shape, or
  documented as the third meaning (`0003-67`'s fork).
- Whether `message.received` should name the execution that took the message (`0003-67`'s fork):
  a claim of the interpreter's, useful to a reader and changeable by the agent's code.
- A usage summary on the report, from the ledger (`plan/next/usage-summary-from-the-ledger.md`):
  OR-OBS2's "unknown usage stays unknown" holds in the stream today, and a reader behind a counted
  gap has no total the report gives.
- A round sequence beside the round number (`plan/next/every-round-ending-carries-its-usage.md`),
  which would make a join on `round` unambiguous where a round that committed nothing left its
  number to the next.
- `binding.started` and a binding's stderr as events (`plan/next/binding-start-is-not-evented.md`,
  `plan/next/binding-diagnostics-are-not-evented.md`): records a reader of a session with bindings
  lacks, not required by OR-OBS; when added, a binding's stderr is an operator-only field.
- The renderer's fixture from the writer (`plan/next/render-session-fixture-from-the-writer.md`):
  `0003-66` adds a kind by hand to `every_event()`, as every task does today.

## Unverified

- rig 0.40's `tool_call_id` is documented "when available"; which providers give none was not
  checked. The fallback -- `call` null, a join by source text labeled as such -- covers it.
- That rig runs a turn's tool calls one at a time is read from `round.rs`'s comment on its default
  `tool_concurrency`, not measured; the hook-to-tool handoff depends on it, and `0003-67`'s test
  with two identical submissions in one turn is the check.
- The cost of building a second projection per event under the publish lock was not measured.
- That a host program's reader can lose the first record was reasoned from `_host.py:579-610`
  (a 4,096-push queue dropping its oldest), not run.
- The renderer's unknown-type path was read (`render-session.py:466-479`); whether a
  multi-megabyte `data` slows the page before the bound is added was not measured.
