# 0003-69 -- The session page lays out one agent's rounds

## Context

`scripts/render-session.py` read a session's events as one agent's timeline: a `round.started`
opened a block, and the next `model.round.completed` closed it. Since `0003-30` a session can hold
child agents, each with rounds of its own, under subjects `agent/c1` and so on, and their rounds
run while the main agent's round runs. The page closed the main agent's open round as unfinished
when a child's began, showed a child's round as an attempt of the main agent's, and left
`agent.spawned`, `agent.request.*`, `agent.call.*`, `agent.released`, `agent.wedged` and
`model.round.usage` as raw data -- a block whose fields the sweep in
`scripts/render-session-test.py` still accepted, because every field reached the page escaped
regardless. Filed as
[tgockel/outrig#684](https://github.com/tgockel/outrig/issues/684); the shape below was sketched
in `plan/next/render-session-lays-out-children.md`, folded in by this task.

The root cause was a single `Builder.open`/`Builder.blocks` pair shared by every subject: an
event's own subject was never consulted when deciding which round was open, so a child's
`round.started` closed whichever round happened to be open for anyone.

## Goal

Each subject -- the main agent and each child -- keeps its own open round and its own list of
blocks, so a child's round neither closes nor joins its parent's; every `agent.*` event and
`model.round.usage` has a handler, so none renders as raw data; and a reader can find a child's
section, a request's outcome and value, and which agent a token-table row belongs to.

## Deliverables

- **A `Section` per subject.** `Builder.sections: dict[str, Section]`, made the first time
  anything of a subject's is read or added, in the order each subject first appears. `here()`,
  `begin()`, `close()` and `within()` all take an optional `subject` (default the event being
  handled) and act on that subject's `Section` alone.
- **Each child's section is linked from the `agent.spawned` that launched it.** The spawning row
  still lands in the child's own section (its subject), and a short cross-post --
  `launched <name> (<id>)`, linked to the section -- lands in the parent's, via a new
  `Builder.add_to(subject, summary, **kw)` that posts a row into another subject's section.
- **A handler for every `agent.*` kind and for `model.round.usage`:** `agent.spawned` (already
  there, now cross-posting), `agent.request.sent/received/replied/invalid/progress/failed/
  cancelled/settled/refused`, `agent.call.started/settled`, `agent.released`, `agent.wedged`,
  `agent.instance.started/ready/released/collected/handle.collected`, and `model.round.usage`.
  A request's events are gathered on one `Request` entry (keyed by its id, correlated the way
  `Exec` and `Msg` are), linked from both the sender's row (`agent.request.sent` cross-posts to
  the sender) and the child's own timeline.
- **A Requests section, beside Messages**: one row per request or typed call -- id, sender,
  child, the call's qualname and digest when `agent.call.started` named one, an inputs preview,
  the outcome, the answer attempts, the value preview, and the usage a settled typed call
  reported.
- **A round's `total` and `descendants` beside its own usage.** `Block` gains `total`,
  `descendants`, `subject` and `agent`; `close()` reads `total`/`descendants` off
  `model.round.*` the way it already read `usage`/`input_tokens_max`; a `model.round.usage` that
  names an already-closed round in the same subject's section adds to that round's `total` and
  `descendants` instead of opening a new row with nothing to attach to.
- **The token table names each round's agent, its total and its descendants.** `TOKEN_COLUMNS`
  gains `agent` (right after `round`) and `total (with descendants)` plus `descendants` at the
  end; `agent_label()` says `primary` for the main agent and the launch name otherwise, set once
  the whole record has been read (so a child's name, seen on its `agent.spawned`, is known by
  the time a row needs it).
- **The Timeline is laid out one section per subject.** Each section gets its own heading, with
  its own rounds and "outside a round" blocks beneath it, in the order sections first appeared;
  a session with one agent renders exactly as it did before (`sections|length > 1` gates the
  per-section heading).
- `doc/usage/sessions.md`'s "Reading it in a browser" paragraph, which said what the page shows,
  extended with the per-agent sections, the per-round agent/total/descendants, and the
  Requests section.

## Acceptance

- A fixture with a primary and one child, the child's round inside the primary's, renders the
  primary's round unbroken (no "unfinished" outcome) and the child's round under its own
  section heading, linked from a `launched <name> (<id>)` row in the primary's timeline.
  (`test_a_childs_round_does_not_interrupt_its_parents`)
- No `agent.*` event, and no `model.round.usage`, renders with the raw-data kind an unhandled
  type gets (`org.outrig.<kind>` as the row's own label), and
  `test_the_sweep_covers_every_event_there_is` stays green.
  (`test_no_agent_event_renders_as_an_unknown_type`)
- A child's replied value and the request's outcome -- and, for a decorated call, its qualname
  and digest -- appear in the Requests section.
  (`test_a_requests_outcome_and_value_are_shown`)
- The token table shows which agent each round belongs to, and a round's total with its
  descendants. (`test_the_token_table_names_each_rounds_agent_and_its_total`)
- `uv run --script scripts/render-session-test.py` passes in full, including every case that
  predates this task: a session with one agent is unaffected.
- `python3 scripts/audit-doc-style.py` passes.

## Dependencies

- **Hard: `0003-14`.** The renderer and its sweep test.
- **Hard: `0003-30`.** Child agents, their rounds, and the subjects they run under.

## Decisions

- **Where the cross-post lands.** `agent.spawned`'s own subject is the child (`events.md`), so
  the launch row the event itself describes stays there, as the first row of the child's new
  section. The parent's timeline gets a second, short row from the same event
  (`Builder.add_to`), rather than moving the original row to the parent: a reader scrolling the
  parent's round sees the launch where it happened, and a reader opening the child's section
  sees why it exists without following a link first.
- **A request's events all carry the subject of the child**, confirmed by reading
  `crates/outrig/src/harness/event.rs`'s doc comments for each `AgentRequest*` and `AgentCall*`
  variant: the receiver, not the sender, is the subject throughout. This is what makes gathering
  a `Request` by id straightforward -- every event that touches it is handled from the same
  subject -- and what makes the one cross-post (on `agent.request.sent`, into the sender's
  section) the only one needed; a reply, a settlement or a call's end all stay on the child's own
  timeline, with an `href` into the Requests table for whoever wants the gathered view.
  `agent.instance.*` was handled the same way but given no Requests-table entry: an instance is a
  standing relationship that answers many requests over its life, not a single one.
  `agent.released`'s subject is whoever released; `agent.wedged`'s is the child that did not
  stop (both confirmed the same way).
- **The token table's extra columns are additive and textual.** `total (with descendants)` and
  `descendants` are plain text (`usage_text()`, a count), not run through the numeric `USAGE`
  columns: a round's own usage is still the only thing summed into the session's grand total
  (every attempt belongs to exactly one round, so summing every subject's own-round usage once
  is already correct), and `total`/`descendants` are read-only cross-checks beside it. Adding
  cells does not disturb `scripts/render-session-test.py`'s `token_rows()`, which extracts cells
  by class, not by position.
- **Session-level ordering.** `Builder.sections` is a plain `dict`, so sections render in
  first-appearance order with no separate bookkeeping; the main agent's is first whenever its
  own first event (`agent.started`, or no subject at all) precedes every child's.
- **Three rendering errors found in review, corrected before landing:**
  - `model.round.usage` is not only for a descendant that settles after its round closed: the
    ledger (`crates/outrig/src/agent/ledger.rs`'s `descendant()`) decides which case it is by
    comparing against its own lock-guarded state, which a concurrent descendant can race ahead
    of the completing event's own publish. So `model.round.usage` can reach the log before its
    round's `model.round.completed`/`failed`/`dropped` does. The lookup in
    `on_model_round_usage` now matches the round whether it is still open or already closed --
    `begin()` already puts the open block in `Section.blocks`, so dropping the "closed only"
    filter was enough -- and `close()` adds its own event's `total` to whatever is already
    there instead of replacing it, and appends to `descendants` instead of overwriting, so
    neither order loses the other's contribution.
  - `agent.request.replied` was setting the Requests table's aggregate `attempts` count from its
    own per-reply attempt number. A reply can arrive after the request has already settled --
    cancellation racing an answer being checked -- and a late one was overwriting the count
    `agent.request.settled` had already recorded authoritatively. `replied` no longer touches
    `req.attempts`; only `settled` does, which is the one event that counts them.
  - A session-level event -- `session.state`, `session.report`, and the rest the schema gives no
    subject -- was read as its own subject, `""`, making it a second section instead of joining
    the main agent's. `section()` now canonicalizes through `MAIN`, the existing constant that
    already said the two name the same agent for the facts table; a session-level event now
    joins the main agent's section, as it did before sections existed.
  - Regression tests for all three: `test_a_descendants_usage_reported_before_its_round_closes_
    still_counts`, `test_a_late_reply_does_not_undo_a_settled_attempt_count`,
    `test_shared_events_with_no_subject_join_the_primary_section`.
- **A fourth, found on a second review pass: a `no_primary` session's shared section.**
  `section()`'s canonicalization through `MAIN` assumed `agent/primary` was always real, so a
  session built with `SessionBuilder::no_primary` -- an owner constructing agent-class instances
  directly, with no primary agent of its own -- got a section titled `primary` holding only the
  shared, subjectless events, never opening a round. `build()` now checks the whole record for
  a real `agent/primary` subject before the pass (`records` is already fully read by then), and
  `agent_label()` titles the shared section `session` rather than `primary` when there is none.
  Regression test: `test_a_session_with_no_primary_titles_its_shared_section_session`, over a new
  `no_primary_session()` fixture -- an owner-launched instance's round, with no `agent/primary`
  anywhere in the record.
- **A fifth, on a third review pass: the `no_primary` section's block titles.** Since that
  section never opens a round, "Before the first round" and "Outside a round" are both wrong for
  it -- each promises one is coming or has passed. `here()` titles its block "Session events"
  plainly instead, checked by `sec.subject == MAIN[1] and not self.has_primary` rather than by
  anything about the block itself, so a child's own legitimate pre-round prelude is untouched.
  The regression test now checks the two sections apart: "Session events" in the shared one,
  "Before the first round" still in the child's.
- **A sixth, on a fourth review pass: the owner's own section has the same problem.** `owner`,
  the host program itself, never opens a round either -- it is reached by a spawn's or a
  request's cross-post into its section, or directly by the `agent.released` of a child it
  released itself (`doc/reference/events.md`, "A child agent") -- so its block got the same
  wrong title. `here()`'s check is now two subjects, not one: `owner` titled "Owner events",
  the no-primary shared section "Session events" as before. The fixture gained an owner-sent
  request and an owner release so the section exists to check; the regression test now checks
  all three sections apart.
- **No change to any Rust crate.** Every field this task reads --
  `descendants`/`total` on `model.round.completed/failed/dropped`, the whole `agent.*` family,
  `model.round.usage` -- already exists in `crates/outrig/src/harness/event.rs` and is already
  documented in `doc/reference/events.md`, landed by `0003-19`, `0003-30`, `0003-31`, `0003-34`
  and `0003-35`. This is a renderer-only change.
