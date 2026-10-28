# Continuation

How a session is saved while it runs, put to sleep when its owner decides, and continued in a new
session: the agent chooses what of its own state survives, OutRig cuts history and bytes at one
point, and every piece of open work has a disposition at that point. This page decides the shape
and seven tasks build it. `0003-58` builds the history format with its export and import;
`0003-60` the assessment; `0003-61` the designation round and its runtime API; `0003-62` the pause,
the cut and the package; `0003-63` the outcomes of a save and a sleep and the order in which
checkpoints are published; `0003-64` the resume and its notice; `0003-65` the disposition of open
work at the cut. The vocabulary is `README.md`'s ("Vocabulary"): the **pause**, the **cut**, a
**designation**, a **save**, a **sleep** and a **resume** mean exactly what it says there, and this
page adds four words below. CocoClaw is the first application and the running example, never a
special case: it supplies a storage adapter, its current records and its policy, and OutRig ships
the operations, the format, the gate and the notice. The CLI uses the same operations with a
directory adapter of its own, so the exit criterion "the CLI saves and resumes a session through
the same mechanism an embedder uses" is one mechanism with two adapters.

Four more words, used exactly so on this page and in the tasks:

- A **package** is what a cut produces: the exported history, a manifest, and the designated
  files and data. A storage adapter receives it; a resume takes one.
- A **checkpoint** is one package with a stable identity and a place in an order: its sequence
  within its session, and the checkpoint its session resumed from, if any. A backend can tell an
  older one from a newer one along one lineage without reading either; two sessions resumed from one
  checkpoint are siblings the lineage cannot order, and the owner's fencing does (`0003-63`).
- A **storage adapter** is the owner-supplied object a package is handed to and read back from.
  OutRig ships the interface and the test adapter its acceptance needs; the CLI's directory
  adapter is the CLI's; an application's is the application's.
- A **writer** is anything that can change a designated file or a designated datum between the
  agent's preparation and the capture: the bindings process and the programs its libraries start,
  an MCP placement, a live object's call running in a host program, and the agent's own tasks,
  threads and subprocesses.

## One continuation: a new session

`lifecycle.md`, "The interpreter's death ends the session", records that a session has one
interpreter and that its death ends the session, and its "No restart in 0.3" records the one rule
any later continuation follows: "a fresh interpreter, a notice telling the model its Python state
was reset, and no replay of source that already ran." This phase builds no restart. A resume is a
new session -- a new container, a new interpreter, a new bindings process, new facility
connections -- for the same durable task, whose identity is the owner's (OR-SUS6; the validation's
constraint 7). Nothing of a session survives its stop except what a package holds.

What `plan/next/interpreter-restart-with-a-reset-notice.md` recorded becomes this page's rule for
the notice, and that entry is absorbed here. Its shape, restated as this page's rule and not as a
restart's: the resumed session presents what the session supplies again -- `runtime`, the
channels, the store's turns as `runtime.history`, the bindings and the skills -- and nothing of
the namespace; what was in flight at the cut is reported as ended, in the package and in events;
the first model call carries a notice that says the interpreter is new, which names were lost,
which work was reported ended, and that nothing was run again; no earlier source runs again,
imports included, and rebuilding state is the agent's decision made in code it writes. The
entry's open questions are answered below: a resume is an owner call, never automatic
("Resume"); the bindings process is new, since the session is ("One continuation"); and unread
messages are recorded at the cut and redelivered where the host authored them, and counted lost
where it did not ("Open work at the cut").

What `plan/next/history-export-and-import.md` recorded becomes `0003-58`'s format, and that entry is
absorbed too: an owner-side export on `Session` in a versioned format of OutRig's own with no rig
type in it; an import on the builder that seeds the new store under the same turn ids and mirrors
them into the interpreter; nothing of the namespace, the bound objects, the running work or the
child handles moving; import running no source. Its open questions -- provider-specific content and
whether a conversation exported under one provider can be sent to another, an unknown format
version, and whether promotions, manifests and usage travel -- are "The format"'s answer and
`0003-58`'s forks. `embedding.md`, "Not in this phase", listed the export as deferred; this phase
lands it before the 0.3.0 release (`README.md`, "Milestone 3"), while the public surface may still
change.

## What a package holds, and what it never holds

| part                     | holds                                                            |
| ------------------------ | ---------------------------------------------------------------- |
| history                  | each agent's store in the format of `0003-58`, promotions, round |
|                          | numbering, and optionally the per-call manifests and usage       |
| manifest                 | the format version, the checkpoint identity and order, the       |
|                          | session and agent relationships, the designated set with each    |
|                          | entry's digest, each open item's disposition, each uncertain     |
|                          | operation, and the restore obligations below                     |
| designated files         | the bytes of each designated path, read from the host mount      |
| designated data          | each designated datum, in the serializable subset                |
| restore obligations      | the application code's identity (`application-code.md`), each    |
|                          | facility and channel by name (`facilities.md`,                   |
|                          | `external-channels.md`), each MCP service by name                |
|                          | (`mcp-presenter.md`): what a resume must supply again            |

What it never holds: the namespace or any Python object in it, a coroutine, a lock, a socket, a
proxy or a client, a credential or a token, the container, the bindings process, a provider's
conversation handle or request JSON, the event log, and any file the agent did not designate. The
record of a session is its owner's (`observability.md`, "One stream, not one file per subject"),
and a package is not a second copy of it: the manifest names the stream sequence the cut was taken
at, so a reader with the log can line the two up, and nothing more.

The designated set is the agent's, and only the agent's (OR-SUS2; `decisions.md` section 8 of the
planning that fed this phase). OutRig never infers what matters from a scan of the workspace, from
Git status or from a file's being dirty or untracked, and a read-write mount proves nothing about
what survives: an undesignated file is not promised to survive, and the manifest and the notice say
so. What is not the agent's to designate is OutRig's to carry: the restore obligations above are
recorded by the cut, and the agent is told in the designation round that it need not designate
them.

## The format (`0003-58`)

**Reversed here.** `doc/reference/events.md`, `turn.committed`, says of `messages`: "the turn's
messages, in rig's own JSON form, which can change when OutRig upgrades rig"; and
`plan/next/history-export-and-import.md` says the store "is crate-private, holds rig `Message`s,
and ends with the session." What replaces them: the persisted form of a conversation is a format
of OutRig's own, versioned and identified, and a conversation outlives its session in it. Why:
`embedding.md`, "Stable at 0.3.0, not before", keeps rig out of everything an embedder reads so
that "a rig release cannot force an `outrig` major", and a save an embedder stores and reads back
is something an embedder reads; a save in rig's form would tie every stored conversation to the rig
version that wrote it. `0003-58` makes the change and its `## Decisions` records it. The store's
in-memory type stays crate-private and is the task's to choose; what the rule binds is the
persisted form, and whether `turn.committed` carries the canonical form in place of rig's is the
task's fork.

The format is built from what `history.md` decided and changes none of it. The unit is the turn
("The view": "The unit of both the window and a promotion is a turn"), and a turn's id is its
index in the store (`crates/outrig/src/agent/history.rs:607-620`, `Store::turns`; `:622-627`,
`Turn`). The store is host-authoritative and append-only ("Promotion"). The Python mirror is a
projection -- `mirror` keeps the prompt, the text, each call's source and result text and the
incomplete flag, and drops every other part (`history.rs:1043-1092`) -- so an export reads the
Rust store and never `runtime.history`. An incomplete turn, one a round ended while its calls ran
and whose missing results are OutRig's notes (`round.rs:497-539`, `keep_what_ran`), is exported
with its flag, because "a placeholder standing in for a missing tool result never implies the call
did not happen" ("History stops being owned by whoever runs the round").

**The canonical message model.** A turn is a round number, an incomplete flag and an ordered list
of messages; a message has a role and ordered parts; a part is text, a tool call (id, name,
arguments), a tool result (the call's id, ordered content parts), or a **typed opaque part**: a
`provider`, a `kind` the provider names (a signed reasoning block, a redacted one, an unsigned
reasoning text, an encrypted reasoning blob), the bytes or JSON the provider gave, and its
**provenance**: the provider row and model identifier that produced it and the call id of the
attempt that did. Nothing in the model names rig. Conversions run both ways in Rust, store to
format at export and format to store at import; a part the store's type cannot hold is kept
beside its turn and never sent, and the manifest of a call that would have carried it records it
as left out.

**The compatibility rule, per provider pair.** The precedent is the view's per-call eligibility:
`Wire::takes` (`crates/outrig/src/agent/budget.rs:202-217`) leaves an unsigned reasoning part out
of a call to an Anthropic model and takes everything for an OpenAI-compatible one, and the
manifest's `left_out` records each part so left (`history.rs:177-181`, `:186-228`). The format
adds provenance to that decision. For a conversation exported under one provider and resumed under
another:

- **Anthropic reasoning, signed or redacted.** To Anthropic: whole. To Chat Completions: omitted,
  recorded. To Responses: omitted, recorded.
- **Chat Completions `reasoning_content`, unsigned.** To Anthropic: left out, as `Wire::takes`
  leaves it today. To Chat Completions: whole. To Responses: omitted, recorded.
- **Responses encrypted reasoning.** To every provider, Responses included: omitted, recorded.
- **Text, tool calls and tool results.** To every provider: whole.

"Omitted, recorded" means the part is kept in the store, never sent, named in each call's manifest
as left out with its reason, and counted in the resume notice. The Responses column stands while
#716 -- which supplies Responses routing with client-supplied context and no `previous_response_id`
chain (OR-SUS7) -- leaves the eligibility of encrypted reasoning unresolved; when that issue settles
it, the row changes and the format version does not, since the rule is the importer's and not the
file's. Nothing is invented, rewritten or promised portable: a signed block goes back only to the
provider that signed it. `0003-58` makes the view's own `Wire::takes` provenance-aware, so that one
rule serves both the import and every later call; `potential/cost-aware-commentary-preamble.md`
records the per-wire rule as it stands today.

The rule covers a move an alias makes between providers in the middle of a session as well as a
resume (OR-A5-3): the next call's manifest records each part omitted in `left_out` with its
reason, visible and tool history go whole, and a cut taken afterwards records the omissions in
its manifest.

**Additive, with continuation metadata.** Within one version a reader keeps a field it does not
know and a writer may add one. Each agent's entry carries a continuation-metadata section with
two fields: the agent's model-request counter and limit (`0003-50`, OR-A4-2), so a resumed agent
reports its spent count and the owner may set a fresh limit, which `0003-58` carries since
`0003-50` lands before it under the phase's numbering; and its alias selection state -- the
alias, the member by name and the order at the time (`0003-59`, OR-A5-2) -- restored by member
name, with a member no longer present falling back to the head and the resume notice saying so,
which `0003-58` names and `0003-59` fills. `model-policy.md` owns what both fields mean, and
`0003-64` composes both notice lines, since both tasks land before it.

**Export at a quiescent boundary.** `Session::export_history()` -- the spelling is the task's --
reads every agent's store under the store's lock and refuses while a round runs, an execution
holds a slot or a child is not idle, naming what runs. That is the "simplest spike" the
adversarial review named (its issue 1): one agent's turns at a boundary with no Python running
and no open request. `0003-62`'s cut calls the same export after the pause, when the boundary
has been established rather than found.

**Import on the builder.** `SessionBuilder::history(part)` -- the spelling is the task's -- checks
the version by name and refuses an unknown one with the version named, validates every turn against
the model, and seeds the store before the agent is built: `History::new` is made in
`Agent::assemble` (`crates/outrig/src/agent/mod.rs:286`), and the imported turns are mirrored into
the interpreter through `push_turn` (`crates/outrig/src/python/host.rs:1492-1500`) once the kernel
opens, under their original ids, so `runtime.history.turns` reads as it did. Round numbering
continues from the last imported round. Promotions are restored. The window stays the default
(`history.rs:55-60`). The imported turns are recorded by one event naming their count and what was
omitted, and not re-emitted as `turn.committed`, since this session did not commit them. Import runs
no source. When the root is owner-constructed (`0003-46`), the document's root store is attached to
the `ConstructSpec` by the agent id the manifest names: the builder form seeds it as the root's
kernel opens, before its orienting round, and `Session::construct` on a started session names the
import in its spec and seeds the same way; an import whose root no spec constructs is refused at the
start, naming the id. `0003-58` builds the attachment and the refusal; `0003-46` lands before it.

What travels beside the turns is the task's fork, with the recommendation that the per-call
manifests travel, since they are small and are "the only precise answer to 'what did this decision
have available'" (`history.md`, "Promotion"), and that usage totals per agent travel as optional
data from the ledger, with no price and no durable accounting engine; the latter is the scope
`plan/next/usage-summary-from-the-ledger.md` sets for itself, and that entry is unchanged.
`plan/next/history-bodies-on-demand.md` bears on a long conversation's size and not on its form:
a package holds every turn whole, and a mirror that fetches bodies on demand would still be
seeded from the same part. `plan/next/strict-provider-promotion-brings-the-round.md` bears on
what a promotion carries under `role-alternation = "strict"`, which the import restores as the set
of promoted ids and nothing more.

## Assessment (`0003-60`)

The owner asks how costly a sleep would be now, as often as it likes, and nothing changes for the
asking (OR-SUS1). `Session::assess()` -- a `Session` operation; the spelling is the task's --
returns **runtime facts with a freshness stamp**, never prepares anything, saves nothing and tells
the model nothing. The agent-level facts are the roster snapshot `0003-49` serves (OR-A6-3;
`rust-owner.md`): each agent's id, kind, parent, depth, state -- `rust-owner.md`'s names: launching,
running, unavailable, idle, limit reached, refused, failed, released, cancelled, terminated -- last
stop, model in use and outstanding requests, read under the runner's lock; the assessment reads that
snapshot rather than assembling its own, and adds what the roster does not carry. The facts are what
the host already knows:

- the requests open on each child -- unread, queued and taken, the roster's three counts
  (`0003-49`), projected from every non-ended record of the children's registry
  (`crates/outrig/src/agent/children.rs`) and never from `summary`, which keeps one submission for a
  work child -- and each child's life -- launching, running, idle, wedged, released (`:127-134`,
  `Life`);
- the executions holding a slot, and what a round left running as its ending names it
  (`crates/outrig/src/harness/session.rs:605-631`, `StillRunning`); the tasks on each loop
  (`crates/outrig/src/python/host.rs:1153-1156`, `tasks`), when the loop answers;
- the hosted calls the relay was sent and has not seen end (`crates/outrig/src/python/hosted.rs:
  509-511`, what `drained` waits on), the MCP calls in flight as `mcp-presenter.md` records them,
  and the live-object calls running in a host program;
- each channel's positions: on the primary's `user` channel and on each external channel the
  delivered and unread counts (`host.rs:1147-1149`, `pending`); on a local child's channels the
  ids the registry holds;
- the known writers: the bindings process and whether anything else is in its group, each MCP
  placement and whether it can be paused, each live object;
- the standing designation, if the agent has made one, and when;
- the last successful save's identity and the time since it, and any publication in flight.

The stamp is the stream sequence at which the facts were read (`crates/outrig/src/events.rs:
203-237`, `publish`) and the wall time, so a reader that also has the stream can tell what
happened since. A fact that could not be read in time -- a kernel whose loop is not turning, a
probe unanswered -- is reported as unavailable or timed out, never as a guess: the probe pattern
of `runtime-protection.md`, "What the port does" ("The probe asks two questions"), is the
precedent, and `kernel.cpu` (`crates/outrig/src/python/interpreter.py:5887-5899`) answers on the
reader thread while a loop does not. The facts are the same facts a latest-state snapshot would
carry; `potential/live-state-snapshot.md` is the stream-sequenced version for a monitor, and the
assessment adds the continuation fields and the stamp without adopting that entry.

**Model consultation is optional and costed.** With `consult`, the assessment opens one round on
the agent -- the primary, or each root agent of a session with none -- whose fixed opening says it
is a question: how costly an interruption and a restoration would be now, what the agent would have
to prepare, and with what confidence; and that nothing is being prepared and no save follows from
the answer. The agent answers in its reply text; the host returns it as advice with the facts, and
an agent that does not answer in time is reported as unavailable. The round is a turn of history
like any other, and it costs a model call, which the owner chose to spend. Advice is advisory: the
cut revalidates everything ("The pause and the cut"), and stale advice is never a capture
certificate.

**An application hook, optional, on the base class.** `outrig.Agent` gains one method with a
default of none (`crates/outrig/src/python/outrig/_agents.py:490-570`), which a subclass may
override to return data in the serializable subset -- the state the application knows about and
the runtime does not. It runs in the kernel on the agent's loop under a bound, and a loop that
does not answer makes the hook unavailable, which the facts say. No subclass is required to
implement it (OR-SUS1: "without requiring `CococlawAgent.assess_sleep`"), and the application's
scheduling decision -- combining the facts, the advice, its deadlines and its capacity -- stays the
application's. The method's name is reserved on every subclass as every member of `outrig.Agent`
is (`agent-classes.md`, "The declaration"), which is a cost the task names.

Constraint 12 of the validation holds here: the task itself is not a request, `0003-37`
guarantees only that no child idles while it holds a request, and an agent holding none is idle.
Idle is a fact the assessment reports, and the owner decides what idle means for its task; nothing
in OutRig keeps an agent "working on the task".

## Designation (`0003-61`)

For a sleep, the model is told that the interpreter will restart and given a bounded opportunity
to designate and prepare what matters; for a save, it is told that the save is a recovery point
from which a later session may continue while this interpreter runs on, and it is never told that
a restart is imminent when none is (OR-SUS2). Both are one mechanism: a **host-opened round** on
the primary, or on each root agent of a session with none, with a fixed opening the host writes,
as `0003-34` opens the orienting round on `ORIENTING` (`crates/outrig/src/agent/orientation.rs:
358-362`) and as `0003-37` opens its warning round. The opening states which of the two it is,
what the agent has designated so far, what OutRig carries without designation (the restore
obligations), that undesignated files are not promised to survive, and that the agent's own
tasks, threads and subprocesses are writers it must finish or stop before its round ends if they
touch what it designates. The round, the designation it makes, and whatever the agent ran to
prepare are turns of history, so the resumed model reads its own preparation.

**The runtime API.** The agent designates through `runtime` -- the namespace and the spelling are
the task's fork -- naming paths and data: a path under a mounted directory, by its container path,
which the cut reads from the host side of the same mount (`crates/outrig/src/harness/builder.rs:
723-771`, `same_paths`, where the workspace's container path becomes its host path when a
session has bindings, and `spec.workspace.host` otherwise); and data in the serializable subset
of `messages.md`, "Contracts", checked by `outrig.schema` as a channel contract is, under a name.
A designation may be made in any execution and stands until withdrawn; the round is the bounded
opportunity, not the only one. A path outside every mount, a path that is not a regular file or a
directory under one, and a datum outside the subset are refused at the call with the reason, and
the round's result says so; the designation the cut captures is the standing one, and the
manifest lists what was designated, what was captured, and what was refused or unsupported, so
the agent learns on resume which declared state came back and which did not. Nothing is captured
that was not designated, and nothing is inferred.

Whether a periodic save reuses the standing designation or opens the round again is the task's
fork (OR-SUS2's open choice). An operation ticket -- an upload's id, a question's id -- is data
like any other and is designated the same way; a reference to a live object, a hosted proxy or a
facility is refused, since none of them survives, and the opening says so.

**Exempt from `0003-37`'s count.** `agent-classes.md`, "The child's rounds", has the host open a
round on a change to a child's summary and keep that summary as the baseline; `0003-37` adds that
a round ending with the summary unchanged and holding something earns the warning and then the
cancel of every request held. A round the host opens for assessment or designation ends with the
summary unchanged by construction -- the agent was asked about something else -- so counted, it
would be a step toward cancelling every request the agent holds (the conflict check's tension 2).
`0003-37` already exempts a round that ended in an error: "it neither gives nor uses the warning".
A host-opened assessment or designation round is exempt the same way, moves no baseline, and
counts no attempt against any request; `0003-60` builds the exemption with the first such round
and `0003-61` relies on it, and the runner's comparison at `crates/outrig/src/agent/runner.rs:
100-126` and its baseline update at `:187-196` are where it attaches. The task that first builds
it records the exemption in its `## Decisions`, and `agent-classes.md` gains one sentence saying
so (the integrator's edit, proposed in the report).

## The pause and the cut (`0003-62`)

**Reversed here, in part.** `lifecycle.md`, "Admission", says of the gate: "Once it closes, a
boundary request the owner has not forwarded to its binding is refused, a submission is refused
rather than run, and a child launch fails at once"; and the session's states run `idle` to
`closing` to `reported` with nothing returning from `closing` (`embedding.md`, "What the embedder
drives"; `crates/outrig/src/harness/lifecycle.rs:1-10`). What is added beside them: a gate that
stops new work while a cut is established and reopens if the cut is abandoned or the operation
was a save. What is not changed: `close_admission()` stays one-way
(`crates/outrig/src/harness/session.rs:299-301`), the close stays the first step of a shutdown,
and the word **admission** keeps the design's meaning, the terminal close. The requirements use
"admission" for both the pausable phase and the terminal close (the validation's wording fix; the
conflict check's tension 3); here the pausable phase is the **pause**, and a task that writes
"admission reopens" has written something this page does not mean. `0003-62` makes the change and
its `## Decisions` records the states added, that `close_admission()` is untouched, and how each of
the three mechanisms below was made to hold and release.

**The pause reuses what the close built and adds the way back.** Three mechanisms order new work
against the close today, and each has a hold the pause can use:

- Rust's gate refuses a submission while `Table.closing` is set
  (`crates/outrig/src/python/host.rs:971-987`, `submit`); the pause gates at the round boundary
  instead. A round already running when the pause begins runs to its end with its submissions
  admitted -- a held submission would hold the round open while the export refuses on a running
  round and the barrier waits for none to be open, and the operation could only time out -- and no
  round opens while paused: a child's runner opens none, as none opens once closed
  (`runner.rs:74-76`), and `Session::round` answers that the session is paused rather than driving
  one. The pause is established when every running round has ended or the operation's deadline
  passes; a round still running at the deadline makes the cut refuse naming it, or degrade under the
  options' policy. The session's state gains the pause -- `pausing` while rounds end and the drain
  runs, `paused` while the cut is held, or one state; the names are the task's -- with a path back
  to `idle`, published as every state is (`crates/outrig/src/harness/event.rs:1022-1035`,
  `SessionState`; `lifecycle.rs:85-95`, `set`).
- The relay stops forwarding boundary requests while `closing` is set and refuses what it holds
  (`crates/outrig/src/python/hosted.rs:471-501`, `close`); a pause stops forwarding and holds what
  arrives in the queue, whose bounds still refuse past them (`:592-662`, `first`), and forwards
  again on release. Bookkeeping passes, as it does after the close.
- Each serving process's flag is installed by a control line behind the frames already forwarded
  (`crates/outrig/src/python/binding.py:292-304`, `_control`; `:1738-1740`); a pause sends a
  control line that holds a request read after it rather than refusing it, and a second line
  releases. A live object's serving module in a host program takes the same lines (`0003-36`).

A child launch during the pause waits as a submission does; a request sent to a child is queued as
any request is, since the queue is the interpreter's. The pause is bounded by a deadline the
operation carries, so a hold is never indefinite: past it, the operation fails or degrades
("Save and sleep outcomes") and the pause releases.

**The drain.** Running executions and forwarded hosted and MCP calls are given until the deadline,
as `lifecycle.md`, "Closing a session", gives them at a close, and for the same reason: a call
allowed to return has a known outcome. What is still running at the deadline is not interrupted
by a save -- a save continues the session -- and is a writer the cut must account for: a hosted
call running in the bindings process, an MCP call in flight, a live object's call running in the
program (`0003-36`'s not-proven-stopped rule; `lifecycle.md`, "Live-object calls"; the
validation's constraint 5: a running hosted or live-object call cannot be stopped). The cut
refuses, or degrades, naming each such call by binding or service and member.

**Writers that can touch the designated set.** The obligation is for the designated set alone
(OR-SUS3; `outrig-mcp-needs.md`, section 10.1, as corrected in 10.4): a writer that cannot reach
any designated path imposes nothing, and no whole-workspace snapshot follows from unrelated dirty
files. For each writer that can:

- **The bindings process.** Held by the control line, with no call in flight after the drain, it
  is quiescent for what its libraries do on a call. What a library started and left running is a
  process in the bindings group; the cut lists the group as the shutdown's `BindingStop` does
  (`crates/outrig/src/harness/report.rs:84-99`, `survivors`) and names every process in it beside
  the bindings process itself as a possible writer: degraded, or refused under a policy that asks
  for a clean cut.
- **An MCP placement.** Paused or stopped through the placement control `mcp-presenter.md`
  defines, with its outcome reported; one that cannot be controlled and has write access to a
  designated path's mount is an uncontrolled writer, and the cut refuses or degrades naming the
  service. For a save, placements resume once the staged cut is immutable; for a sleep, they stop
  with the session.
- **A live object's call.** Cannot be ended (`hosted-objects.md`, "Live objects": "Nothing the
  engine does ends a call running in the program's thread"). Running at the deadline, it is named,
  and the cut refuses or degrades.
- **The agent's own tasks, threads and subprocesses.** The design lets them outlive rounds
  (`execution-and-rounds.md`, "One at a time"; `lifecycle.md`, "Three things end separately"),
  and a save-only cut leaves the interpreter running, so a Python barrier alone proves nothing
  about them (the conflict check's tension 6). Two things stand in for proof. The designation
  round tells the agent to finish or stop what writes to its designated set before its round ends,
  and the agent's compliance is in its history. Then a **brief execution barrier**: with
  no round open and none opening, the host reads each designated path's size and digest from
  the host mount, waits a short interval, and reads them again; a difference names an uncontrolled
  writer, and the loop's tasks (`host.rs:1153-1156`) and the subprocesses the interpreter's `Popen`
  patch knows of (`agent-placement.md`, "Output stays attributed") are listed beside it. A cut
  with a difference is refused or degraded naming the path. One with no difference and nothing
  listed is taken clean; one with no difference and a writer listed -- a loop task or a known
  subprocess still running -- is `degraded` naming them, since nothing proved they were done, and
  refused under the policy that asks for a clean cut; `continuation.cut.degraded` carries the
  listing, so a reader knows what was running and unproven. The barrier's length is the task's
  fork.

**Capture and packaging.** After the barrier the cut exports every agent's history ("The
format"), reads each designated file from the host mount into a staging area, encodes each
designated datum, writes the manifest, and hands the package to the storage adapter as **staged**.
From that point the staged package is immutable: a save releases the pause and the session
continues while the adapter publishes; a sleep keeps the pause and goes on to the close
("Save and sleep outcomes"). History and bytes agree because both were taken inside one pause
with no execution admitted between the export and the reads: a history that says the index was
written never points at bytes from before the write (OR-SUS3's acceptance).

**The adapter's interface.** A trait on the builder, as `SkillSource`, `Secrets` and `Approver`
are (`crates/outrig/src/harness/builder.rs:187-191`, `:251-254`, `:278-285`): the owner supplies
an object that takes a staged package under a checkpoint identity, publishes it and answers with
acknowledged, failed or uncertain, and reads one back by identity for a resume. For a host program
the adapter is a Python object the program passes to `outrig.Session(...)`; the engine sends each
call to the program over the owner connection as a request the program's object answers, JSON
crossing in both directions (`crates/outrig/src/owner/mod.rs:12-28` lists the protocol's
requests; this adds pushes the engine makes of the program). In one process the package is a
value and never JSON. OutRig builds the test adapter its acceptance needs and no other; the CLI's
directory adapter is `0003-63`'s, in `outrig-cli`.

**Cancelling a preparation.** An owner that abandons the operation before the staged package
exists releases the pause: submissions and boundary requests held are forwarded, runners resume,
the state returns to `idle`. What the preparation did stands -- a file the agent wrote to prepare,
a designation it made -- because nothing is rolled back (`execution-and-rounds.md`, "No rollback,
ever"), and the outcome says so rather than claiming a rollback (OR-SUS4's acceptance). A
cancellation after staging is a save that was not published, and the staged package is discarded
or kept as the policy says.

## Save and sleep outcomes (`0003-63`)

A save and a sleep share the pause, the cut and the package and differ after staging (OR-SUS4). The
outcomes are distinct, and each is reported separately from the cleanup that follows it:

| outcome                    | means                                                              |
| -------------------------- | ------------------------------------------------------------------ |
| staged                     | the package exists, immutable, under its checkpoint identity       |
| committed                  | the adapter acknowledged publication of that identity              |
| stop proven                | a sleep's close ran and the report's `stopped` is `Proven`         |
| failed                     | the cut was refused, or the adapter refused or failed; the staged  |
|                            | package, if any, is kept, and the session is as the pause left it |
| uncertain acknowledgement  | the adapter's answer was lost; the identity is what resolves it    |

A **save** continues once staged: the pause releases, the writers resume, and publication
proceeds beside the running session. A **sleep** stays paused through persistence and is reported
successful only after the adapter acknowledges the committed checkpoint; then admission closes
and `shutdown` runs (`crates/outrig/src/harness/session.rs:396-522`) with the sleep's dispositions
already recorded ("Open work at the cut"), and the report is the sleep's last word on the stop. A
failed publication never discards the only recoverable state: the staged package stays until a
newer committed one replaces it or the owner discards it. The acknowledgement comes from the
adapter and from nowhere else -- never from an event having been published, since the stream is
lossy and the report is what is authoritative (the validation's constraint 6; `observability.md`,
"One stream, not one file per subject").

**Checkpoint identity and order.** Each checkpoint carries the session's id, a sequence number
the session increments, the stream sequence the cut was taken at, and a digest of the package. A
backend publishes monotonically by that order, so a delayed older upload cannot replace a newer
save, and a lost acknowledgement is resolved by asking the backend about the identity rather than
by publishing again (OR-SUS4's recommendation). Saves in flight are bounded, one at a time by
default, and a save requested while one stages is coalesced into it; a sleep requested while a
save publishes waits for the staging it needs and then takes its own cut. Owner fencing -- whether
this owner may still publish for this task -- stays the owner's: OutRig carries the identity and
order and obeys a refusal (OR-SUS4; `embedding.md`, "What stays the embedder's").

**Forced stop is a policy.** An owner whose sleep failed may close the session anyway, through the
ordinary `close_admission()` and `shutdown`. The outcome then says what it relies on: the last
committed checkpoint by identity, and what has happened since it that a resume will not know --
the turns committed, the files changed in the designated set, the open work. Nothing is hidden
behind a clean report.

**The CLI.** `run-new` gains `/save` and `/sleep`, each reporting its outcome in the terminal in
the words above, and a directory adapter that writes each checkpoint under the session directory
`crates/outrig-cli/src/session.rs:1-14` describes, beside `logs/`. The adapter is the CLI's,
built in `outrig-cli`, so the exit criterion runs through the same `Session` operations an
embedder calls.

## Resume (`0003-64`)

A resume is a new session that takes a package on the builder -- `SessionBuilder::resume(package)`,
the spelling the task's -- and starts as any session does (`crates/outrig/src/harness/builder.rs:
377-457`, `start`), with four steps added in this order, each refusing or degrading as the
manifest requires (OR-SUS6):

1. **Application code by identity.** The package names the application code's identity;
   `application-code.md` provisions it again before any construction, and a mismatch is handled
   as that page says and the task's fork chooses -- refuse, or proceed with the difference named in
   the notice. No upgrade policy is prescribed (OR-SUS7).
2. **Facilities by name, with fresh authority.** The manifest names each facility, channel and
   MCP service the agent had; the owner supplies each again, as it did at the first start, and
   `facilities.md` binds them under the same names. No proxy, client, token or socket is revived
   (`lifecycle.md`, "Rejected alternatives": "Rejected: reattaching to proxies after the owner
   restarts"). A name the owner does not supply again is a **lost name**, and the notice says so.
3. **Designated files and data.** Each designated file is written into the owner-supplied
   workspace from the host side before the interpreter starts; a path that exists with other
   content is handled as the task's fork chooses, since the world may be ahead of the save, and
   the notice names each file restored, replaced or left. Designated data is handed to the agent
   as a mapping the notice names.
4. **History.** Imported as "The format" says, before the agent is built.

**The reset-and-reconciliation notice** opens the resumed agent's first round, written by the host
as the orienting round's opening is, and is therefore a turn of history. It states, in order: that
this is a new interpreter and nothing was run again; which declared state was restored, which was
omitted or unsupported and is therefore not present, and which undesignated state was never
promised; which names were lost -- facilities, channels, services, and every variable of the old
namespace; which services changed; which obligations are outstanding -- each open request's
disposition from the manifest, each uncertain operation with its ticket where the agent
designated one; and the owner's **current records**, folded in: the questions the owner knows
were answered since the save, the messages it knows were consumed, the effects it knows
completed or failed. The owner supplies those records to the resume as a value in Rust and as JSON
over the owner connection for a host program, matched to durable ids where the backend has them,
and the notice presents authoritative newer facts rather than reposting a question or replaying an
action. A summary restart -- a new session from the prompt alone -- is a different operation the
owner may choose; it is never presented as this one.

**The CLI.** `run-new --resume <checkpoint directory>` is the minimal spelling that makes the exit
criterion true: it reads the CLI's directory adapter, builds the session through `resume`, and
shows the notice as the first round's opening. The flag joins `RunNewArgs`
(`crates/outrig-cli/src/cli/run_new.rs:83-106`).

## Open work at the cut (`0003-65`)

**Reversed here, in part.** `lifecycle.md`, "Closing a session", says closing cancels "child work
and unanswered requests cancelled, and every waiter on them settled with a documented error", and
the interpreter's `_close` settles them `SessionClosing`
(`crates/outrig/src/python/interpreter.py:6207-6238`). A sleep that ended in today's close would
cancel every request the agent holds and tell the owner `SessionClosing`, which is the opposite of
recording their positions. What is added: a **terminal request end for suspension**, recorded
before the close settles anything, so the close finds each request already ended and settles
nothing it should have recorded; the `terminated` end the close uses for an interpreter death
(`crates/outrig/src/harness/event.rs:1238-1256`, `RequestEnd`; `children.rs:269-287`, `report`)
is the precedent. And `lifecycle.md`, "The interpreter's death ends the session", says "A fresh
interpreter inherits no pending handles and no proxies, only the notice". What is narrowed: a
handle to a **durable external** request is reattached by its logical id through
`external-channels.md`'s registry, because the backend holds the request and its identity outlives
the session; every other handle is ended. No proxy is reattached, which OR-AG5 and OR-SUS6 reject
as the design does. `0003-65` makes both changes and its `## Decisions` records the new end, its
Python exception, the order against the close, what is reattached and what is ended, and the
children fork it took.

**Dispositions.** Each item open at the cut gets one of these, recorded in the manifest:

- **A request on an external channel, reply owed:** `suspended`. The backend is told by durable
  id, the request stays the backend's, and it is reattached on resume.
- **A request the agent made on an outbound channel, outstanding:** `suspended` at a sleep. The
  backend is told under the durable id and the request stays its; the manifest records the id, the
  channel and the backend's reference. On resume the registry is seeded with it, so the owner's
  later `settle(request_id, reply)` is accepted and delivered as an arrival announced with the
  channel and the request id, the reply under `runtime.continuation.restored[request_id]`; the
  notice lists it as an outstanding obligation. At a save nothing ends: the request stays open and
  its state is recorded.
- **An external channel's unread deliveries:** positions recorded by `0003-53`'s descriptors -- ids,
  never bodies, since the package holds no queue -- and the backend redelivers by id. The resumed
  registry is seeded with the consumed and acknowledged watermarks and with each unread item id as
  **awaiting redelivery** under its saved delivery id (`0003-64`): the first push of such an id is
  queued under that delivery id, so the agent receives it, and only a later push of it, or a push of
  an item consumed before the save, is a repeat that queues nothing. Until the redelivery arrives
  the window holds nothing of it, `pending()` says so, and the notice names the ids awaiting
  redelivery per channel.
- **The primary's `user` channel, unread:** recorded with their bodies and redelivered, since the
  host authored them; the task's fork.
- **A local child, and the requests it holds:** the cut is refused unless the child is idle with
  nothing open, naming the child and the ids; an idle one is released at a sleep and left running
  at a save. Under the fork's alternative, a busy child's requests end `suspended` at a sleep and
  stay open at a save.
- **A hosted or MCP call still running:** recorded `uncertain`, with its receipt's fields.
- **A child at its model-request limit** (`0003-50`, OR-A4-3): limit-reached is not open work; it
  is released at a sleep as an idle child is, stays resident at a save, and the manifest records
  its spent count and limit so the owner can set a fresh one at resume.
- **An execution still running:** at a sleep, `unknown`, as the close records it; at a save, the
  cut is refused or degraded, as "The pause and the cut" says.

Why local children's requests end rather than reattach: a message between co-hosted agents never
reaches the host (`messages.md`, lead; `observability.md`, "Most of this is already on a wire"),
so the host holds a bounded preview of a local request's body (`event.rs`, `AgentRequestSent`)
and never the body, and nothing could re-send it. Their positions are recorded from the host's
request registry -- the ids unread and taken per child (`children.rs:1012-1037`, `summary`;
`:1041-1072`, `counts`) -- not from queue state, which the host cannot see. Root-only histories are
the recommendation: a child's history travels as a record, and a child is not restored; the
alternative, restoring the interacting session tree, is the task's fork, and the adversarial
review's issue 1 says the choice "must be stated, not assumed". A request child whose request is
ended `suspended` sees it as `0003-37`'s cancel is seen -- `reply` and `fail` raise -- with an
exception of its own saying the session was suspended, and a host program's handle (`0003-35`)
raises the same. An MCP call "possibly dispatched" (`0003-56`) and a hosted call `unknown` are
recorded as uncertain and never cancelled optimistically or replayed; the notice carries them to
the resumed agent.

## Where the queued 0003 tasks interact

- **`0003-58`.** `0003-35`: a root child's store is exported by its agent id, with the owner as
  its parent. `0003-36`: nothing. `0003-37`: nothing; a cancel is in history as events, not as
  turns.
- **`0003-60`.** `0003-35`: a root child's open requests are facts, and the program's handles are
  the owner's. `0003-36`: a running live call is a fact, and one that cannot be stopped.
  `0003-37`: the consultation round is exempt from the count.
- **`0003-61`.** `0003-35`: the round opens on each root agent, as the warning round would.
  `0003-36`: a running live call is named as a writer the agent cannot quiesce. `0003-37`: the
  designation round is exempt from the count and moves no baseline.
- **`0003-62`.** `0003-35`: root children are children. `0003-36`: a live call running at the
  deadline refuses or degrades the cut, named. `0003-37`: a paused runner opens no round, so no
  count moves.
- **`0003-63`.** `0003-35`: the program's handles settle `suspended`, then the close runs.
  `0003-36`: a sleep's stop-proven outcome is the report's `stopped`, and `ShutdownException` is
  raised as `0003-36` has it. `0003-37`: nothing.
- **`0003-64`.** `0003-35`: the program declares its class again; an owner-constructed root is
  re-provisioned (`0003-46`). `0003-36`: the program injects again, and a name not injected is a
  lost name in the notice. `0003-37`: the count starts from nothing on resume.
- **`0003-65`.** `0003-35`: a program's handle raises the suspension exception, where that task
  exists; the first application reads the end on the Rust control handle of `0003-46` instead.
  `0003-36`: a live call at the cut is `uncertain`. `0003-37`: an external request at the cut
  ends `suspended`, not cancelled, and the count starts from nothing on resume; a child at its
  model-request limit (`0003-50`) is not open work, so no warning round is spent on it and it
  neither refuses the cut nor ends `suspended`.

The first application's primary is an owner-constructed root in a `no_primary` Session (A-02;
`rust-owner.md`; `0003-46`): `Session::round()` answers `NoPrimary` there
(`crates/outrig/src/harness/session.rs:350-353`), and rounds open through the instance's own runner.
So a sleep's preparation round, the consultation round and the cut's barrier reach that primary
through its runner -- a host-opened round as the orienting and warning rounds are, and the hold on
runners the pause imposes -- never through `Session::round()`; the owner holds the control handle;
the stop read `0003-48` gives it reads the instance's state at rest, and the pause is the session's
state, read as `session.state` (`0003-62`), not an instance state. What this page says of "the
primary, or each root agent of a session with none" is that path for the first application.

## Events and close rows

Each task adds its events to `doc/reference/events.md` and its rows to `lifecycle.md`'s close
table, as `observability.md`, "What is emitted", and `lifecycle.md`, "The close table", require of
every task that adds something that can be running at a close. The families this page foresees,
each name the task's to settle: `history.exported` and `history.imported` (`0003-58`);
`continuation.assessed` with the stamp and whether the model was consulted (`0003-60`);
`continuation.designated` with the paths and data names, and what was refused (`0003-61`);
`session.state` gaining the pause, `continuation.cut.staged`, `continuation.cut.refused` and
`continuation.cut.degraded` naming the blocker (`0003-62`); `continuation.published` with the
identity and the acknowledgement, and `session.report` gaining the last committed checkpoint and
what a forced stop relied on (`0003-63`); `continuation.resumed` and the notice as a bounded
record (`0003-64`); the `suspended` outcome on `agent.request.settled` and
`continuation.disposition` per open item (`0003-65`). All are execution diagnostics except the
notice and the designation round's turns, which are model view, and the adapter's calls, which
are integration audit.

## Constraints carried from the validation

Stated here so that no task rediscovers them:

- **A running hosted or live-object call cannot be stopped** (constraint 5;
  `plan/next/cancel-a-running-hosted-call.md`; `0003-36`). The cut names it and refuses or
  degrades; a backend or facility the agent waits on returns promptly and lets `runtime.wait` do
  the waiting (the adversarial review's issue 8), which `external-channels.md` and
  `mcp-presenter.md` state for their facilities.
- **The stream is lossy and the report is authoritative** (constraint 6). An acknowledgement is
  the adapter's; a package records the stream sequence for a reader and depends on no event.
- **Interpreter death ends the session** (constraint 7). Resume is the only continuation, and it
  is a new session.
- **The interpreter program is near its argument cap** (constraint 11). The designation API and
  the assessment hook are interpreter code; `0003-39` comes first.
- **The task itself is not a request** (constraint 12). Idle is a fact; the owner decides.

## Rejected alternatives

**Rejected: restarting the interpreter in place.** `lifecycle.md`, "No restart in 0.3", and
`README.md`, "Out of scope". A fresh interpreter in the same container would still inherit the
bindings process, every facility connection and every writer, with none of their state known;
a new session starts with none, which is the condition the notice describes.

**Rejected: exporting `runtime.history`.** The mirror drops every part that is not text
(`history.rs:1043-1092`), so an export of it would lose signed reasoning, tool-call ids and
message structure; the adversarial review and `embedding-contracts.md`, section 1, both found it.
The export reads the Rust store.

**Rejected: capturing the dirty tree, untracked files or the whole workspace.** The authoritative
correction of the planning that fed this phase (`decisions.md`, section 8;
`preservation-correction-summary.md`) puts the choice with the agent. A scan would promise what
it cannot keep -- a writer the cut did not quiesce -- and would carry what the agent never meant to
keep.

**Rejected: pickling the namespace, or replaying source to rebuild it.** `lifecycle.md`,
"No restart in 0.3": replaying source repeats its effects. Objects that cannot be serialized are
not serialized; the agent designates data and writes reconstruction into its history.

**Rejected: a provider's conversation handle as the checkpoint.** OR-SUS7; a
`previous_response_id` chain names a provider's store, not OutRig's, and ties the save to one
route. The format carries client-supplied context, as #716's routing does.

**Rejected: a pausable `close_admission()`.** One word for two gates is the conflict the
requirements carried in (tension 3). The close stays one-way and the pause has its own name,
state and path back.

**Rejected: a durable call ledger for exactly-once across a resume.** `lifecycle.md` rejected it
for the close and `embedding.md`, "What stays the embedder's", puts retries and receipts with the
embedder. The manifest records uncertainty; the owner's records resolve it.

**Rejected: restoring children as distributed tasks.** The research note
`outrig-suspend-points-needs.md` (section 4) says children need not become distributed tasks and, in
the same paragraph, recommends capturing the interacting session tree. Root-only is the choice here
because the host never holds a local request's body ("Open work at the cut"), stated as `0003-65`'s
fork 1, which the adversarial review calls defensible when stated. Children are refused unless idle
with nothing open, or ended; their histories travel as records.

**Rejected: prescribing copy-on-write for the staged cut.** OR-SUS4 asks for an immutable staged
package and no more. A copy into a staging area is immutable by construction; a filesystem
snapshot is an adapter's choice.

**Rejected: a summary restart presented as a resume.** OR-SUS6. A new session from the prompt
alone is the owner's degraded fallback, chosen and labeled as such; the notice is what makes a
resume one.

## Open questions

- The designation API's namespace and spelling, and whether a periodic save reuses the standing
  designation or opens the round again (`0003-61`'s forks).
- The pause's state names (`0003-62`'s fork). The gate sits at the round boundary, so no submission
  is made during the pause; that is decided above, not a fork.
- The barrier's interval, and whether it is a configuration key (`0003-62`).
- The concurrent-save policy beyond one in flight, and the cadence of optional timer- or
  work-based saves, which `0003-63` makes an owner policy and does not default on.
- Whether the resume notice is the first round's opening alone, or also a one-line pointer in the
  preamble that the session was resumed (`0003-64`).
- What a resume does with a designated file whose path exists with other content (`0003-64`).
- Initial child support beyond root-only histories (`0003-65`).
- Whether `turn.committed` carries the canonical form in place of rig's (`0003-58`).

## Unverified

- Nothing on this page has run. The time a pause takes -- the drain, the barrier, the reads from
  the host mount -- is unmeasured; `shutdown`'s container stop, about two seconds (`lifecycle.md`,
  "Unverified"), is the nearest measurement and does not apply to a save.
- That a read from the host side of a bind mount sees every byte the container wrote before the
  barrier's first read. Both sides are one filesystem on one kernel, so a completed `write(2)` is
  visible to the next `read(2)`; a write the agent's process buffered in user space and has not
  flushed is not, which is why the designation round asks the agent to finish its writes.
- That the bindings process observes a pause control line promptly. The close's control line is
  read "on its own schedule" (`lifecycle.md`, "The close table"), and a pause needs an
  acknowledgement before the barrier that the close does not: the task measures it.
- Whether an MCP placement can be paused rather than stopped, and what a stopped server loses;
  `mcp-presenter.md`'s control decides and this page assumes only that an outcome is reported.
- The compatibility table's Responses column is written against the requirements' description of
  #716 and not against its adapter, which does not exist at `8fb6eac`.
- That `0003-37`'s exemption composes with the warning state it keeps beside the baseline: the
  task that builds it tests a designation round opened while a warning is pending.
