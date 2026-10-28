# 0003-67 -- Events correlate where they can

## Context

Every id a record carries is minted inside one session: `source` by the builder, `execid` and
`message` by the interpreter's one counter (`crates/outrig/src/python/host.rs:977`), `call_id` and
`attempt_id` by the ledger (`crates/outrig/src/agent/ledger.rs:234-240`), a hosted `request` by the
relay (`crates/outrig/src/python/hosted.rs:255-258`), a child's `request`, `invocation` and
`instance` by the interpreter, `round` and `turn` by each agent's history, and the subjects
`agent/c<N>` by the host in launch order (`crates/outrig/src/agent/children.rs:462`). `events.md`
says which events carry which (`plan/phase/0003-python/diagnostics.md`, "Identities"). Four
joins a reader wants are not carried:

- `exec.submitted` carries `execid` and `source` (`host.rs:1011-1014`), `exec.refused` neither
  source nor call (`:981-985`, `:1000-1004`), and the tool call's id is only in `turn.committed`;
  `scripts/render-session.py:447-456` joins by equal source text, first come first served.
  `plan/next/exec-events-name-their-tool-call.md` proposed the fix and is absorbed here. The id is
  in hand in the round hook's `ToolCall` arm (`crates/outrig/src/agent/round.rs:670-690`): rig
  0.40's `StepEvent::ToolCall` carries `tool_call_id`, the provider's, and `internal_call_id`,
  rig's (`rig-core-0.40.0/src/agent/hook.rs:516-525`), while `ToolDyn::call` receives only `args`
  (`rig-core-0.40.0/src/tool/mod.rs:292`; `crates/outrig/src/agent/tool.rs:207-218`). rig runs a
  turn's calls one at a time and in order (`round.rs:375-380`), and the hook and the tool share
  `Interrupts` (`tool.rs:61-67`; `round.rs:83`).
- `round.started` carries `round` alone (`crates/outrig/src/agent/mod.rs:155-157`, `:187-189`),
  though the runner holds the summary of requests it opens on
  (`crates/outrig/src/agent/runner.rs:100-134`) and the registry marks each open request with the
  round (`children.rs:1130-1148`), which is what `agent.request.received.round` and
  `agent.request.settled.rounds` carry.
- The interpreter's `took` line names a message's channel and id (`interpreter.py:3654`;
  `host.rs:2269-2278`), not the execution whose code took it, though the kernel keeps the current
  execution in a context variable (`interpreter.py:186`).
- `hosted.received.execid` and `.invocation` are "as the interpreter said: diagnostic context,
  which the agent's own code can change" (`events.md`; `hosted.rs:633-634`, `:696-712`), and
  nothing in `events.md` says, field by field, which ids the host minted and which it took on a
  kernel's word.

OR-OBS2 asks that "interleaved children/calls can be joined without ID collisions or guessed
causality", that session-local ids be qualified "separately from durable task/claim identity", and
that heuristics be labeled. Its phrase "round/attempt" conflates two things the record keeps apart
-- a provider attempt, `attempt_id`, and an answer attempt, `agent.request.*.attempt` (conflict
check, tension 7; validation, "Wording in the requirements to fix") -- and `model.retry.attempt`
is a third, a retry's ordinal. The governing shape makes one durable task span several sessions,
since a resume is a new session (`continuation.md`; `lifecycle.md`, "No restart in 0.3"), so a
reader that follows a task needs an identity no session mints. `diagnostics.md` ("Identities",
"Events correlate where they can") decides the joins, the labels, the task identity and the rule
every new event family follows; this task builds them. `0003-66` added the producer record this
task puts the task identity on. No recorded decision is reversed.

## Goal

Every record that can name the host-known id of what caused it does, `events.md` says of each id
whether the host minted it or the interpreter claimed it, an owner's durable task identity is
carried on the stream from its first record, and "attempt" means one thing per event.

## Deliverables

- **An execution names its tool call.** `exec.submitted` and `exec.refused` gain `call`, the
  provider's tool-call id as rig gives it, `null` when the provider gave none; `exec.refused`
  gains `source`. The round hook's `ToolCall` arm leaves the call's ids in `Interrupts`, which the
  tool's `call` reads as it submits and `Interpreter::submit` records; `events.md`'s note that a
  refused submission's source "is only in the turn that asked for it" is removed.
- **A child's round names what it opened on.** `round.started` gains `requests`: the ids of the
  requests the round opened on -- a request child's unread and taken ids, a work child's one
  submission -- `[]` for an orienting round, which opens on no request, and `null` for the
  primary, whose rounds open on messages the announcer counts and does not name.
- **A taken message names its execution.** The `took` line gains `exec` from the kernel's context
  variable, and `message.received` gains `execid`, `null` when the receive ran outside every
  execution; labeled a claim, as `hosted.received.execid` is.
- **Known against claimed.** `events.md` says for each id field whether the host minted it or the
  interpreter reported it, in a sentence per field and a summary table in "The envelope"; the
  renderer labels a join made on a claim ("as the agent's code reported") and a join by source
  text ("matched by source text, not by id"), and joins by `call` when it is there.
- **The durable task identity.** `SessionBuilder::task(id: impl Into<String>)`, an opaque string
  OutRig never parses, at most 256 bytes, refused at `check` and `start` past that;
  `session.producer` gains `task`, the string or `null`; `outrig.Session(task=...)` crosses in
  the owner protocol's
  `session.start` beside `workspace` and `mounts` (`crates/outrig/src/owner/mod.rs:19-21`) and
  `session.producer` mirrors it; the renderer shows it in the facts. Nothing else carries it:
  `source` stays `/outrig/session/<id>`, session-local.
- **The three meanings written down.** `events.md`'s "Integration audit" introduction defines a
  provider attempt (`attempt_id`), an answer attempt (`agent.request.replied.attempt`,
  `.invalid.attempt`, `.settled.attempts`, `.sent.attempts_max`) and a retry's ordinal
  (`model.retry.attempt`), and each event's entry uses one of the three words.
- **The rule for every new event family**, in a new `events.md` section "What every event
  carries": a subject, or none for the session's; one host-minted id of its own on every event of
  the family; the session-local ids of what caused it that the host holds, each said to be the
  host's or the interpreter's; a durable reference as an opaque string beside a session-local id,
  never in its place; bounded previews built from by-value data; one meaning of "attempt"; its
  fields classified for audience (`0003-68`); its rows in `events.md` and, where the thing can be
  running at the close, in `lifecycle.md`. The pages that define new families cite it
  (`diagnostics.md`, "Events correlate where they can").
- `render-session-test.py`: `every_event()` carries the new fields; the two-identical-submissions
  case; the labels.
- `crates/outrig/CHANGELOG.md`, `[Unreleased]`: the new fields, `SessionBuilder::task`,
  `Session(task=)`.
- No new resource can be running at the close, so no row is added to `lifecycle.md`'s close table.
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

Mock-driven unless it says otherwise.

- A round in which the mock submits the same source twice: each `exec.submitted.call` equals the
  `id` of its tool call in the turn's `turn.committed`, the two differ, and the renderer links
  each call to its own execution (`events_tests.rs` asserts the ids match; the renderer's test
  asserts the links).
- A submission refused because another held the interpreter, and one refused at the close: each
  `exec.refused` carries `source` and `call`, and the renderer shows the source on its entry.
- A mock whose tool calls carry no id: `call` is `null`, and the renderer joins by source text with
  the label "matched by source text, not by id".
- A request child whose round opens on requests 7 unread and 9 taken: `round.started.requests` is
  `[7, 9]`; its orienting round's is `[]`; a work child's round opened on submission 12 has `[12]`;
  the primary's has `null`.
- A message the agent's code receives inside execution 4: `message.received.execid` is `4`; one
  received by a task running outside every execution: `null`.
- `SessionBuilder::task("job-42")` puts `"job-42"` in `session.producer.task`; without it the
  field is `null`; a 300-byte id fails `check()` and `start()` naming the bound; a host program's
  `outrig.Session(task="job-42")` yields it in the first `Event` and in `session.producer`, and
  the renderer's facts show it.
- `events.md` says of every id field whether the host minted it or the interpreter reported it,
  defines the three meanings of "attempt", and carries "What every event carries"; checked by
  reading, and `test_the_sweep_covers_every_event_there_is` passes with the fields planted.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass; `uv run --script
  scripts/render-session-test.py` passes.

## Design forks

1. **Which tool-call id `call` carries -- Recommended: the provider's `tool_call_id` alone.** It
   is the `id` the turn's messages carry, which is the join a reader makes. The alternative adds
   rig's `internal_call_id`, which nothing else in the record holds.
2. **How the hook hands the id to the tool -- Recommended: a slot on `Interrupts`**, which the
   hook and the tool already share, set in the `ToolCall` arm and taken by `call`. The alternative
   is a new shared cell on `SubmitPython`, built and threaded for this alone.
3. **`round.started.requests` or a child-only event -- Recommended: the field.** `round.started`
   is already the round's opening record on every agent; a new `agent.round.opened` would split
   one fact across two kinds.
4. **`model.retry.attempt` -- Open; recommended: document, do not rename.** Renaming to `retry`
   changes a field the renderer, its fixture and `events.md` read, before the 0.3.0 release fixes
   the file's shape; documenting it as the third meaning costs one sentence. Renaming is the
   alternative while the shape may still change.
5. **`message.received.execid` -- Open; recommended: yes.** It is a claim the agent's code can
   change, like `hosted.received.execid`, and the label says so. The alternative leaves the join
   out, since the interpreter's `took` line is the one interpreter-side change here and
   `interpreter.py` is near its argument cap until `0003-39` lands.
6. **The task identity's bound -- Recommended: 256 bytes, refused at `check` and `start`.** The
   alternative cuts a longer id, which would make two tasks' ids equal.

## Dependencies

- **Hard: `0003-66`.** The producer record `task` is added to.
- **Hard: `0003-19`.** The stream, the ids, the renderer's inputs.
- **Hard: `0003-30`.** The request family, the runner's summary and the registry's round marks.
- **Soft: `0003-39`.** The `took` line's new field is interpreter code, which the argument cap
  bounds until the program is a file.

## See also

- `plan/phase/0003-python/diagnostics.md` -- "Identities", "Events correlate where they
  can".
- `plan/phase/0003-python/observability.md` -- "Open questions", the causal-parent question this
  answers narrowly; "Every model event ... carries two ids".
- `plan/phase/0003-python/agent-classes.md` -- "The child's rounds", "Only an invalid reply counts
  toward the attempt limit", "The orienting round".
- `plan/phase/0003-python/boundary-policy.md` -- "Events", previews built from by-value data.
- `plan/phase/0003-python/continuation.md` -- a resume is a new session for the same task.
- `crates/outrig/src/agent/round.rs`, `tool.rs`, `runner.rs`, `children.rs`;
  `crates/outrig/src/python/host.rs`, `hosted.rs`, `interpreter.py`; `doc/reference/events.md`.
