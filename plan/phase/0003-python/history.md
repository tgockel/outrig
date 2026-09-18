# History

Today a conversation is one thing: a `Vec<Message>` that is simultaneously the record of what
happened and the payload sent to the provider. Those are different jobs, and this page separates
them. The **store** is everything that happened, held in Python where the agent can read it. The
**view** is the subset that goes over the API. The agent scans the store with ordinary code and
promotes what it wants into the view.

The economics are the point. Scanning the store costs no context, because it happens in Python
and only the result of the expression is observed. So an agent can look at everything it has ever
done and pay only for what it decides to keep -- which is the same trick the rest of this phase
rests on, applied to the conversation itself.

## A round contains turns

The vocabulary is introduced here because this is where the distinction does work. It holds
across the phase.

A **round** is one prompt or message, the agent working, and yielding control back --
`execution-and-rounds.md` owns the lifecycle. A **turn** is
one model call and the tool results it requested -- what rig already means by `max_turns` and
`ModelTurnFinished`. A round contains many turns.

This concedes "turn" to the inner meaning rather than competing with rig for it, and promotes the
word the repo already uses for the outer one. It makes `max_turns = tool_call_max + 2` readable:
a round permits at most that many turns.

Only the new loop and these documents adopt it. `outrig-cli` uses "turn" across twelve code files
and `doc/` across eight, and editing those is what `crate-split-tradeoffs.md` rules out. The
legacy loop keeps its own vocabulary because it is a different system.

## Why this is newly safe

Pruning a conversation used to destroy the agent, because history *was* the state. This phase
already changed that -- `README.md` puts it as "the conversation history is no longer the only
thing carrying state" -- and `messages.md` makes it sharper still: a round can end with an
operation still running under a name, and a later execution awaits it again. So at the moment an
agent yields, the work is in a name and the state is in the namespace. History is the only thing
that does not have to survive.

The honest limit is that an agent's *intent* is in history and is not in the namespace unless the
agent put it there. Pruning is safe exactly to the degree state has been externalized. The
variable inventory is how a pruned agent re-orients, and binding what matters to a name is the
habit the whole design rewards.

## The store

The full history, mirrored into the interpreter as ordinary Python data as the host authors
it. Not a proxy: the agent writes expressions, not queries.

```python
# None of this reaches the model. It is Python, over Python data.
failed = [t for t in runtime.history.turns if t.failed]
costly = sorted(runtime.history.turns, key=lambda t: t.tokens)[-5:]

# Only what is promoted is paid for.
runtime.context.promote(failed[-2:])
```

The shapes above are illustrative; the signatures are not settled here.

Holding it whole is the simple choice rather than the clever one. The cost is interpreter memory
proportional to the session, under the same address-space ceiling as everything else
(`agent-placement.md`). If that becomes a problem the fallback is known -- mirror each turn's
shape and fetch bodies on demand -- and it is a change to one side of the protocol rather than to
the idea.

Treat that as a measurement gate rather than a worry. Bounding observations per execution does not
bound a session: sixteen kilobytes a thousand times is sixteen megabytes, and the ceiling is shared
with every co-hosted agent, so history growth is a way for one agent's long session to degrade its
siblings. Measure the host record, the transport, and the Python mirror separately, and decide the
response before the growth is real rather than after.

## The view

What the provider actually receives. By default a window: the first few rounds and the most
recent few, with everything between reachable in the store and promotable into the view.

**The unit of both the window and a promotion is a turn.** `doc/usage/run.md:298` requires
protocol-valid history -- an assistant message carrying a tool call must be followed by its
result, and OutRig already synthesizes a placeholder when the tool-call cap fires mid-sequence. A
turn is exactly the span that keeps that pairing intact. Cutting by message would mean writing
repair logic and keeping it correct across providers whose role-alternation rules differ; cutting
by round is coarse enough that one tool-heavy round is most of a budget. A turn is the unit that
is safe without repair.

### The budget

Counting retained rounds is not a budget. A first-and-recent window plus a few promotions can
still exceed the context window, and a single turn -- fifty tool calls and their results -- can
exceed it alone. So the view is assembled against a size estimate rather than a count:

- a conservative token estimate for the assembled request, the provider's limit, and a reserve for
  the completion;
- a priority order when it does not fit -- the current turn and the protocol data it requires are
  not ordinary eviction candidates;
- deduplication, since a promoted turn may already be inside the recent window;
- an explicit, actionable failure when one intact turn cannot fit at all.

Post-call usage reporting is good calibration and cannot admit the first oversized request safely,
so the estimate has to happen before the call.

What this fixes is worth stating precisely, because an earlier draft of this page overstated it.
There is no token accounting anywhere in the tree today, so a session that outgrows its window
sends an oversized request, takes a 400, and -- because `PromptError::CompletionError` carries no
history -- has nothing appended and is told "history unchanged, send the prompt again." Every
later round fails identically and the only escape is `/reset`. A budget does not make overflow
impossible; it makes it **recoverable and legible**: the loop reports which turn would not fit
and what it dropped, instead of resending the same doomed request forever.

## Promotion

The store is host-authoritative and append-only, with a stable id per turn. The Python mirror is
for reading: editing a local object must not rewrite the authoritative past, and a promotion names
a canonical id rather than submitting content. An agent cannot promote a turn it invented, and a
mutated copy promotes as whatever the original said. Immutable-feeling objects are a reasonable
API choice for making that obvious; they are not a security boundary, and nothing here treats a
Python attribute as one.

The semantics that have to be decided rather than discovered, because the implementation will
answer them accidentally otherwise:

- **Lifetime.** A promotion persists until removed. The underlying `RequestPatch` is per-turn and
  non-sticky, so something has to remember; that is an implementation detail and the model-facing
  promise should not leak it.
- **Idempotence.** Promoting the same turn twice is the same as once.
- **Order.** Promoted turns appear in their original chronological position, not in promotion
  order. A conversation that jumps backward reads as corruption to a provider and to a human.
- **Timing.** A promotion during an execution affects the next model call in the same round.
- **Stability.** A scan sees a stable prefix; turns appended while it runs do not shift what it
  already looked at.
- **Completeness.** An in-flight turn becomes promotable when it commits, and not before.

**Record the view that was actually sent.** The promotion event in `observability.md` says what
the agent asked for, which is not the same as what the provider saw: the window moves, duplicates
collapse, the budget evicts, and a retry or a failover to a smaller model reassembles everything.
So each model call records an ordered manifest of the canonical ids it actually carried, plus the
selection metadata. Content need not be duplicated when it can be reconstructed from the records.
That manifest is the only precise answer to "what did this decision have available", which is what
both a human and any future diagnostic agent will want first.

## What the store holds

Exactly what the model saw: an execution clipped to 16 KiB is stored as 16 KiB with the same
marker, matching `observability.md`.

Large output is not this subsystem's problem, and making it one would be a mistake. The agent
already has better tools: redirect to a file under `/workspace` and read it back, or stream it and
filter as it arrives.

```python
proc = await asyncio.create_subprocess_exec(
    "cargo", "build", stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT
)
errors = [line async for line in proc.stdout if b"error" in line]
```

That is the idiom worth teaching, and it composes with everything else here: what the agent binds
to a name survives pruning by construction, while what it merely printed does not.

## The mechanism exists

`RequestPatch.history` in rig 0.40 is documented for this exact purpose -- "Replace the prior chat
history sent to the provider this turn only ... The enabling primitive for context-window
compaction / summarization middleware." OutRig already uses it: `subagent/injection.rs` patches
history mid-round to fold in a parent's steer, applied from `OutrigPromptHook` on
`StepEvent::CompletionCall`. It appends; a view returns a shorter list through the same call.

Three constraints come from that precedent, each of which has already caught someone:

**The patch is per-turn and non-sticky.** `injection.rs` documents this in its module header,
having been bitten by it: a steer applied once vanishes from the next model call. The view has to
be re-applied on every turn and folded back at round end.

**`extend_history_with_new_suffix` compares by prefix.** It decides whether rig returned the whole
history or only the new part by testing equality against the live history. Mutating the store
while rig holds a stale snapshot silently fails that test and takes the branch that concatenates
the entire pre-prune history back on.

**The hook is already the seam.** `OutrigPromptHook` is `Clone` with its state behind `Arc`, and
the loop already clones it before handing it to rig so the original stays interrogable. A
store-backed view uses the same seam.

## History stops being owned by whoever runs the round

Today it is moved rather than borrowed:

```rust
let mut h = std::mem::take(&mut *history.borrow_mut());
let result = agent.run_turn(&line, &mut h).await;
*history.borrow_mut() = h;
```

Two things follow, and they are the two complaints that started this. Nothing can touch history
mid-round, because during the await it is a local inside a future. And an interrupt loses all of
it: cancellation drops that future, the write-back never runs, and the cell keeps the empty vector
`mem::take` left -- which is `plan/next/repl-interrupt-history-loss.md`, "SIGINT silently empties
conversation history."

The store owns the history; a round borrows a view of it. That is the same change in both cases.

Ownership settles when history is written, too. A turn commits when it is complete; a round that
dies partway leaves the turns that finished and an explicitly incomplete one, rather than all or
nothing. Two rules follow and neither is optional: an executed effect is never erased because a
later model call failed, and a placeholder standing in for a missing tool result never implies the
call did not happen. An unknown outcome stays unknown. Repairing history is not a reason to re-run
anything that was already accepted.

Worth sequencing with this: `plan/next/partial-turn-history-on-failed-model-call.md` proposes
accumulating history in the hook so OutRig holds its own copy, and warns the fix is only right "if
the hook's copy becomes *the* copy rather than a second one." A store makes that true by
construction.

## Rejected alternatives

**Rejected: summarizing what gets dropped.** The standard answer, and the most forgiving of an
agent that never thinks about its context. It costs a model call at exactly the moment the design
was trying to save time, and it introduces a failure that is hard to see -- a confident summary
that dropped the one constraint the user cared about. The store makes it unnecessary: nothing is
lost, so nothing has to be compressed.

**Rejected: cutting by message, with synthesized repair.** The finest control, and it makes every
provider's validity rules OutRig's problem. A turn gives most of the benefit with none of that.

**Rejected: a proxy store the host answers queries against.** Bounded memory and one copy, which
is genuinely attractive. It is not "scanning with Python" -- every new kind of question becomes a
new protocol message, and the query vocabulary becomes a surface to design and maintain forever.

## Open questions

- How the model learns the window exists. An agent that cannot see the middle will not know to go
  looking for it, and whatever tells it costs context on every round. A line in the preamble, a
  marker where the cut is, and a count are all candidates and none is obviously right.
- Whether the default window is configurable, and in what unit. Rounds read well and a tool-heavy
  round is not a predictable size.
- What the user gets beyond `/reset` -- a way to see what the model is currently being sent would
  answer a question that is currently unanswerable.
- Whether a promotion is visible to the model as an event in its own context, or silently changes
  what it sees between rounds.

## Unverified

- That a turn is a safe cut on every provider. It follows from tool-call pairing, which is
  universal, but role-alternation rules are not: a Bedrock-backed Claude reached over an
  OpenAI-compatible gateway is already known to be strict about consecutive roles. Tool-call ids,
  provider reasoning metadata, empty content, and gateway rewriting are all candidates to break it
  too. This is an acceptance gate rather than a caveat: exercise a shortened `RequestPatch.history`
  against each supported adapter with representative fixtures, and publish the supported subset
  plus an intelligible error for what falls outside it.
- The memory cost of mirroring a long session's history into the interpreter was not measured. It is
  the assumption behind holding the store whole, and the fallback exists because it might be wrong.
- `RequestPatch.history` was read from rig 0.40's documentation and from `injection.rs`'s use of
  it, not exercised with a shorter list than rig supplied.
