# 0003-11 -- The full conversation lives in Python; a view goes to the provider

## Context

Today a conversation is one thing: a `Vec<Message>` that is both the record of what happened and
the payload sent to the provider. `history.md` separates them. The store is everything, mirrored
into the interpreter as ordinary Python data; the view is the subset the provider receives.

The economics are the point. Scanning the store costs no context, because it happens in Python and
only the result of the expression is observed -- the same trick the rest of the phase rests on,
turned on the conversation itself.

The mechanism exists and is documented for this. rig's `RequestPatch.history` calls itself "the
enabling primitive for context-window compaction", and `subagent/injection.rs` already uses it to
fold a steer into a running round. It appends; a view returns a shorter list through the same
call.

Ownership is also the fix for a known defect. History is currently moved out of its cell for a
round and written back after, so nothing can touch it mid-round and an interrupt loses all of it
-- `plan/next/repl-interrupt-history-loss.md`, "SIGINT silently empties conversation history".

## Goal

An agent can read its whole conversation with ordinary Python, and the provider receives a subset
the agent had a hand in choosing.

## Deliverables

- The store: host-authoritative, append-only, a stable id per turn, mirrored into the interpreter
  as ordinary Python data. Not a proxy -- the agent writes expressions, not queries.
- The view, applied through `RequestPatch.history`, defaulting to a window of the first few rounds
  and the most recent few.
- **The unit of both the window and a promotion is a turn**, because an assistant message carrying
  a tool call must be followed by its result. A turn is the span that keeps that intact, so no
  repair logic is needed.
- **History stops being owned by whoever runs the round.** The store owns it; a round borrows a
  view. That is the same change that retires the interrupt loss.
- The three `RequestPatch` constraints honored: the patch is per-turn and non-sticky so the view
  is re-applied every turn and folded back at round end; `extend_history_with_new_suffix` decides
  by prefix-equality, so mutating the store while rig holds a stale snapshot silently concatenates
  the whole pre-prune history back on; the hook is already cloned before being handed to rig,
  which is the seam.

## Acceptance

- An agent scans its own history in Python and the scan costs no model context -- asserted on what
  was sent, not on the code.
- A promoted turn appears in the provider's view, in its original chronological position.
- **Interrupting a round no longer empties the conversation -- in `run-new`.** The defect
  `plan/next/repl-interrupt-history-loss.md` describes, tested in the shape that entry asks for:
  interrupt mid-callback, confirm the follow-up round sees prior history. Name the new driver in
  the assertion, and **leave the `plan/next/` entry open**: `run` and `run-legacy` still take the
  old path, and a fix that reaches only the new loop has not closed the reported bug.
- **A pruned view does not resurrect the pre-prune history.** The prefix-equality hazard, tested
  directly, because it fails silently and by concatenation.
- The view is re-applied on every turn of a round, not only the first -- the non-sticky property,
  which `injection.rs` was bitten by.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whole mirror versus metadata-with-bodies-on-demand -- start whole.** `history.md` records the
   fallback and why the simple thing goes first; `0003-12` carries the measurement gate.

## Dependencies

- **Hard: 0003-04.** There is no loop holding a conversation until the loop exists.
- **Soft: 0003-09.** Both touch what an agent does between rounds.

## See also

- `plan/phase/0003-python/history.md` -- the store/view split, the unit, and the rig hazards.
- `plan/next/repl-interrupt-history-loss.md` and
  `plan/next/partial-turn-history-on-failed-model-call.md` -- the defects this ownership change
  retires or enables; the second warns its fix is only right "if the hook's copy becomes *the*
  copy rather than a second one", which a store makes true by construction.

## Decisions

- **The window, and how the model hears of it (the maintainer's call).** Planning put both to the
  maintainer.
  - By default a model call is sent the first two rounds, the six before the current one, and the
    current one whole. It is counted in rounds, but cut only between turns, so a promotion and the
    window share one unit. There is no setting: one would have to pick a unit before `0003-12`'s
    budget does, so `Window` has a constant and a test hook and nothing else.
  - The model hears of it twice. A paragraph in the orientation takes its numbers from
    `Window::DEFAULT`. When turns are left out, the round's opening line ends with "N earlier
    turns are not shown; runtime.history has them."
  - A marker message at the cut was rejected. Synthesizing a message is the repair `history.md`
    rejects, and it would sit exactly where providers' role rules differ.
  - The count is part of the round's prompt, so it is stored with the round and sent again while
    that round is in view, by which time it is stale. It was true when the round opened, and that
    is what the prompt records.

- **rig is handed nothing from before the round.** `.history(Vec::new())`, and on every
  `CompletionCall` the hook's patch is the view of the turns before the round, then rig's own
  `history`. So rig's record, `response.messages`, and every error's `chat_history` are exactly
  the round's messages. Nothing rig returns is compared with the store, and
  `extend_history_with_new_suffix` is gone.
  - The alternative was to hand rig the whole store and patch it down. That keeps the old splice
    working, but it holds a second copy of the conversation for every round, and a prefix
    comparison that concatenates silently once anything changes the store mid-round -- the hazard
    this task was asked to test.
  - Checked in rig 0.40's source rather than its docs:
    - `CompletionCall` fires, and the patch applies, on the first call too.
    - The patch never reaches `AgentRun`.
    - `prompt` is always appended after the patched history.
    - `MaxTurnsError` is raised before the event fires.
    - `response.messages` excludes the input history. `PromptResponse`'s docs say otherwise, and
      its own test agrees with the code.
  - The cost is that a call the patch missed would carry the round alone, which is why the view
    is re-applied on every call. `the_view_is_sent_on_every_call_of_a_round` pins it. A mutation
    that patched only the first call failed it and the promotion test.

- **A turn commits when the next model call starts, or when the round ends.** At a
  `CompletionCall`, `history` and `prompt` together are whole turns.
  - Committing there lets the agent's code read its own round's earlier turns mid-round. A round
    that fails or is dropped has then already kept every turn it finished.
  - A turn starts at each assistant message. That is the span `history.md` calls a turn, and it
    keeps each tool call beside its results.
  - rig only appends to a round, so a count of committed messages says where the rest begins.
    Only the rest is copied, since cloning the whole round on every call is quadratic in a round
    that reaches the 50-call cap.
  - `keep_what_ran` takes only the in-flight call. The count and `ran` outlive it, and it still
    returns `ran`, since what its caller reports is whether Python ran.

- **The store owns the round in progress, and the line that opened it.** `/simplify` found that
  "where the round began" and "its prompt has no reply yet" were each written into several places.
  - The first cut carried a `before` through four signatures, and a flag to hold the prompt back
    from committing mid-round. It also needed a `pending` copy of the prompt for a dropped round
    to keep, and a rule that a turn's leading non-reply messages join it.
  - Now `RigAgent::round` builds the opening `Message` once and hands the same value to rig and
    to `History::begin_round`. rig's first message is always the opening, so the journal's count
    starts at one, and the store puts the opening in front of the round's first turn.
  - A round that commits nothing -- its first call failed or was dropped -- leaves the opening
    uncommitted, and the next round replaces it. That is "a round that ran no Python leaves the
    conversation as it was".
  - A round that ends cleanly with no turn is the one exception: the model's first reply was
    empty, and rig leaves an empty reply out of its messages. There `finish_round` commits the
    opening alone, since it was sent, and `history.extend` used to keep it.
  - Rounds are numbered when their first turn commits, so a round that commits nothing leaves no
    gap, and both ends of the window are ranges of round numbers.
  - One `Window` is chosen in `PythonAgent::build` and given to both the store and the
    orientation, so what the model is told and what it is sent cannot disagree. The tests' narrow
    window is set on the store alone, and none of them reads the orientation's numbers.

- **What a turn is in Python.** It is a frozen `Turn(id, round, prompt, text, calls)`, with each
  call a `Call(source, result)`, and both print as a short summary so an accidental `print` stays
  small.
  - `prompt` is the turn's user text, which is the round's opening line on its first turn.
    `text` is what the model wrote.
  - A call is paired with its result by tool-call id. Arguments without a string `source` are
    kept as their JSON. Reasoning and images are left out.
  - On the host a turn's id is its index. The mirror holds a turn only if its id is above the
    last one it holds, so `_handle`'s retry never holds one twice. A line lost to a second
    `MemoryError` leaves a gap rather than stopping every turn after it. Ids are therefore
    promised to be stable, not dense.
  - `turns` is a tuple rebound on each append, so a scan reads the turns that were there when it
    started.

- **A promotion is a line from Python that nothing asked for, as `send` is.** The host does not
  query for promotions before each model call.
  - The pipe already writes a promotion ahead of the result of the code that made it, so it is on
    the host when the tool returns, and the round's next model call carries it. No request and no
    timeout are needed.
  - Python refuses what it does not hold, raising `ValueError` for an unknown id and `TypeError`
    for anything that is not a turn or an int, `bool` included.
  - The transport holds no conversation state. `Interpreter::on_promote` registers a handler,
    which the reader task calls outside the table's lock, since a commit holds the store's lock
    while it pushes a turn through the table.
    - The store applies each promotion at once and ignores an id it does not hold. That is the
      host's one validator, so nothing an agent sends accumulates.
    - Promotions arrive in the order they were made, which a later removal will need.
    - The first cut buffered ids in the table, bounded by a count of turns pushed. `/simplify`
      found that it held conversation state in the transport and lost that order.
  - `promote` is synchronous and returns `None`. It writes through `_write_line`, not on the
    reserve, as `Endpoint.send` does.
  - Python keeps its own record of what it promoted, for `runtime.context.promoted`: the agent's
    one way to read back what it asked for. `/simplify` offered to drop it as a second copy; it
    stays, since the host takes exactly what Python checked.

- **What landed early from `0003-12`.** Promoted turns go in chronological order. A turn that is
  both promoted and in the window is sent once. Promoting twice is promoting once. A promotion
  takes effect at the round's next model call. All of these fall out of selecting from the store
  in order, and all are tested here.
  - Removing a promotion, the budget, the per-call manifest, the incomplete-turn marker, and
    per-adapter validation stay with `0003-12`, which `/groom-plan` can trim to match.

- **The interrupt defect, in `run-new`.** `0003-04`'s drop guard already meant `PythonAgent`
  borrowed history rather than taking it, so `run-new` never emptied it outright. What changed is
  who owns it.
  - `run_new_keeps_the_conversation_when_a_round_is_interrupted` drops a conversation's second
    round during its model call and again while its Python runs. The next round still carries the
    first.
  - `plan/next/repl-interrupt-history-loss.md` stays open for `run` and `run-legacy`, with a
    section saying so.

- **Anthropic only, and unmeasured.**
  - The mock speaks Anthropic, so the OpenAI arm of the view has not been exercised on the wire.
    `0003-12`'s per-adapter acceptance covers that, and `history.md`'s Unverified list now names
    the consecutive-assistant pair a promotion creates.
  - The mirror is the whole conversation, one unbounded line per turn, parsed on the reader
    thread. Its memory cost is `0003-12`'s measurement gate and was not measured here.
