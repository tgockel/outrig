# 0003-53 -- A send is accepted, refused, or uncertain

## Context

Every message the host posts or the agent sends already has an id. `Interpreter::posting`
records `message.sent` under a `MessageId` before the `msg` line goes out
(`crates/outrig/src/python/host.rs:1100-1140`), and a `send` line from the agent is given one as
it arrives (`:2232-2242`); `Delivery.id` carries it into the agent's code
(`crates/outrig/src/python/interpreter.py:3500-3515`), and `Endpoint.send` returns nothing
(`:3698-3735`). The host knows, per child channel, every request ever delivered and which are
unread (`children.rs:114-125`, `counts`, `:1041-1070`), and per request its state and the rounds
it spanned (`:147-156`); the interpreter answers `pending` with each channel's unread and delivered
counts (`_pending`, `:6489-6493`). No registry collects a channel's positions, nothing records a
consumed watermark, and the report lists only child requests open at the close
(`crates/outrig/src/harness/report.rs:35`; `children.rs:269-287`). `0003-51` makes a send go to
a Rust backend behind a trait and return on its acceptance, raising a typed exception for refused,
known not accepted and uncertain, the last carrying the item id; it makes inbound delivery the
owner's push with the owner's item id, deduplicated as a repeat, and reports the consumed position
to the backend at receipt; `0003-52` gives an outbound request its handle and an inbound one its
record, each settled under the backend.

OR-CH3 asks for stable item and request identities and backend references that allow
acknowledgement, query and reconciliation; to distinguish backend acceptance from local queueing,
agent consumption and incorporation into model history; to distinguish refused or definitively
not accepted from an uncertain send after possible acceptance; to capture descriptors, unread and
consumed positions, outstanding requests and their disposition rather than blocked channel stacks
or pickled clients; and to let the caller resolve an uncertain send by stable identity rather
than by sending another item. OR-A7-1's acceptance fixes the shape of resolution: a backend that
returns uncertain once and then accepted for the same item id leaves one stored item and one
`accepted` in the agent's view. OR-A7-2 fixes what the cut hands the backend: the consumed and
unread positions per channel, so a fetched-but-unread item stays deliverable after a resume and
an item whose consumption was not acknowledged is redelivered by item id. OR-A6-2 asks the owner's
per-round read for the deliberate sends a round made, by channel, count and item id, from
authoritative state. The other open choices of OR-CH3 -- wake defaults, opening-round ordering,
progress coalescing, mirrored commentary -- are answered in `external-channels.md` and `0003-51`.

The validation carried three constraints here: the stream is lossy and the report authoritative,
so positions are host state read on demand and never reconstructed from events (constraint 6);
the local queue is a window the owner paces (constraint 5); and the close settles every open
request (reversal 4, second part), so the registry's snapshot must be readable before the close
and the cut must read it then -- which is `continuation.md`'s pause and `0003-65`'s recording.
This task defines the registry and the hand-over, not the cut: `0003-62` and `0003-65` call them.

## Goal

An uncertain send is resolved by its identity -- once by OutRig before the agent is told, and
again on the agent's asking -- and a host-side registry answers at any moment, per channel, what
is unread, what is consumed and acknowledged, what is received and unanswered, what is
outstanding, and what each round sent, and hands the backend its positions at a cut.

## Deliverables

- **Identities.** An outbound item's id is the host's `MessageId`, minted as it is sent and given to
  the backend on every call about the item; `send()` returns it (fork 1) and an outbound request's
  `Handle.id` is it. An inbound item has two: the owner's item id from the push, by which OutRig
  deduplicates and reports positions, and OutRig's delivery id, which `Delivery.id` carries and a
  reply names; the registry keeps both and every notice to the backend carries both. Ids are stable
  for the life of the session and never reused; across a resume the manifest carries them and
  `0003-64` seeds the registry with them, the new session's minting continuing past the highest
  saved id: an item consumed before the save is a repeat when pushed again, and an item unread at
  the save is awaiting redelivery, queued under its saved delivery id on its first push and a repeat
  after (OR-A7-2).
- **Backend references.** The backend's `send` answers `accepted` with a reference of its own, a
  `str` (fork 3); the owner's push may carry one. OutRig keeps each beside the item's ids, returns
  it with every later notice about the item -- `consumed`, `settled`, `withdrawn`, `resolve`,
  `positions` -- and in the registry and the events, and never parses it.
- **Resolution by identity.** A new trait method `resolve(channel, item, operation)` answers from
  the backend's durable record for the operation named: for a `send`, `accepted` with its reference,
  `not accepted`, or still `uncertain`; for a `withdrawal` (`0003-52`'s `withdrawn`), `withdrawn`,
  `replied` or `failed` with what the backend recorded, or still `uncertain`. When a `send` is
  uncertain -- the backend said so, its call ran past its bound -- OutRig asks once, by the item id,
  before the agent is told: accepted makes `send` return as accepted, not accepted raises the
  not-accepted exception, and still uncertain raises the uncertain exception carrying the item id,
  as `0003-51` built it. `await endpoint.resolve(item)` asks again for the agent's code, and an
  outbound request's handle left `uncertain` by its send is moved by either asking to `waiting` or
  ended `not accepted`. A handle left `uncertain` by a withdrawal whose verdict did not arrive is
  resolved only by the withdrawal query, which settles it cancelled, replied or failed; the send
  query's `accepted` says nothing about a request withdrawn after acceptance and never moves such a
  handle. Each answer is recorded as `channel.item.resolved` with the operation. OutRig never
  resends and never retries a send.
- **The registry.** Host-side, one per session, keyed by channel: the descriptor (name, kind,
  direction, contracts' schema text, bound, wake policy, holder, the backend's reference for the
  channel if it gave one); per inbound item its item id, delivery id and reference; the unread
  delivery ids in order; the consumed watermark, the highest delivery id the holder's code has
  taken, and the acknowledged watermark, the highest the backend has confirmed; the
  received-and-unanswered request ids with how long ago each was taken; per outbound item the
  holder's round it was sent in; the outstanding outbound request ids with state (`sending`,
  `waiting`, `uncertain`) and reference; and the items whose send is uncertain. A snapshot is
  taken under the registry's lock, so an id is never in two sets of one snapshot, and is built
  from host state, never from events. `Session::channels()` returns it to the owner (fork 4), and
  the per-round sends of one holder are readable by round, which is what the owner's per-round
  read (`rust-owner.md`, `0003-48`) serves to its reply policy (OR-A6-2).
- **Positions handed to the backend.** A new trait method `positions(channel, positions)` carries
  a channel's consumed and acknowledged watermarks, by item id and delivery id, and its unread
  item ids in order. This task defines it and calls it from a test; `continuation.md`'s cut
  (`0003-62`, `0003-65`) calls it at a cut, so the backend knows what to redeliver by item id
  after a resume and what it may discard.
- **Four positions, kept apart.** Accepted by the backend (the backend's own state, which OutRig
  learns only by a push), queued locally (the push answered, unread in the window), consumed
  (`receive()` took it; the consumed watermark moved), observed by the model (what the execution
  printed entered the turn, recorded as `exec.completed` and `turn.committed`, not as a channel
  position). The consumed notice is retried until the backend acknowledges it (fork 5), and
  `channel.consumed` is recorded on acknowledgement, when the acknowledged watermark moves.
- **Events**, in `doc/reference/events.md`: `channel.item.resolved` -- the item's ids, channel,
  and what the backend answered (integration audit).
- **The close**, in `lifecycle.md`'s table, tested here: an item uncertain at the close has one
  `resolve` attempted within the drain; resolved, the report lists its outcome; not, it is listed
  `uncertain` with its id and reference, and nothing is sent again. The report gains the entry.
- `crates/outrig/CHANGELOG.md`'s `[Unreleased]` paragraph; rustdoc on every new public item;
  `crates/outrig/public-api.txt` regenerated.

## Acceptance

Each is a mock-driven test unless it says otherwise.

- **Uncertain, then accepted, is one item and one acceptance** (OR-A7-1). A test backend whose
  `send` records the item durably and answers uncertain, and whose `resolve` for that item id
  answers accepted with a reference: the mock's `send("x")` returns; the backend's record holds
  exactly one item; the record shows `channel.item.outcome` `uncertain` then
  `channel.item.resolved` `accepted` with the reference; no second item was sent. The same when
  `send` never answers within the bound and `resolve` answers accepted.
- **Still uncertain is raised with the id, and resolved later.** A backend that answers uncertain
  to `send` and to the first `resolve`: `send("x")` raises the uncertain exception carrying
  `item`; `await endpoint.resolve(item)` with the backend now answering accepted gives
  `"accepted"` and the reference; the backend's record holds one item.
- **Known not accepted is retry-safe.** A backend whose `send` rejects before taking the item
  makes `send` raise the not-accepted exception, whose message says a retry is safe; a second
  `send` succeeds; the backend's record holds one item under the second id and none under the
  first. A backend whose `resolve` answers not accepted after an uncertain `send` is the same.
- **Refused never reaches the backend.** A body outside the contract, a send the close refuses,
  and a send on a closed channel each raise with the reason; the backend's `send` and `resolve`
  are never called.
- **An outbound request's uncertainty.** `h = request("may I?")` with a backend that answers
  uncertain to the send and to the first `resolve`: `h.status.state == "uncertain"` and `await h`
  does not settle; `resolve(h.id)` answering `accepted` moves it to `waiting`, and the owner's
  `settle` then settles it with the reply; `resolve` answering `not accepted` makes `await h`
  raise the not-accepted exception.
- **A withdrawal's uncertainty.** `h.cancel()` against a backend whose `withdrawn` never answers
  within the bound: `h.status.state == "uncertain"`; `resolve(h.id, withdrawal)` answering `replied`
  with a value settles `await h` with that reply and `channel.item.resolved` names the operation; a
  `resolve(h.id, send)` answering `accepted` meanwhile changes nothing; answering `withdrawn`
  settles it `RequestCancelled`.
- **`send` returns the id** (fork 1): `await runtime.channels["inbox"].send("x")` returns an int
  equal to the id the backend's `send` received; the same on the `user` channel equals the
  `message.sent` event's `message`.
- **The registry.** With two items pushed and unread, one consumed and acknowledged, one consumed
  and not yet acknowledged, one inbound request received and unanswered, one outbound request
  `waiting` with a reference and one `uncertain`, and two sends made in round 3: `Session::
  channels()` lists per channel the descriptor fields; the two unread with their item ids and
  delivery ids in order; the consumed watermark at the second consumed item and the acknowledged
  watermark at the first; the received request with a duration; the two outstanding ids with their
  states and references; the uncertain item; and round 3's two sends by channel, count and id. A
  snapshot taken while a push and a `receive()` race never shows an id in two sets, and the ids
  of every event about these items appear in it.
- **Positions handed over.** A direct call of the hand-over for `inbox` gives the test backend
  the consumed and acknowledged watermarks and the unread item ids in order, equal to the
  snapshot's; a push of an unread item's id afterward is a repeat, and a push of a consumed item
  whose consumption was never acknowledged is a repeat too, nothing queued for either.
- **The watermarks.** A backend whose `consumed` fails once is called again until it returns;
  `channel.consumed` is recorded once, the acknowledged watermark moves then and the consumed
  watermark moved at `receive()`; the backend sees each id at least once and dedups by id.
- **Four positions.** A test walks one item through them: in the backend's store and not yet
  pushed, the registry knows nothing of it; pushed, `pending()` is 1 and the watermark unchanged;
  received into a variable and not printed, the watermark moved and the turn's recorded output has
  no body; printed, `exec.completed` carries it.
- **The close row.** Shutdown with one item uncertain: `resolve` is attempted once within the
  drain; a backend that answers `accepted` has the report list it so; one that does not has it
  listed `uncertain` with id and reference; the backend's `send` was called once in all.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`, `cargo clippy
  --all-targets`, `cargo fmt --check` pass; `doc/reference/events.md` and `lifecycle.md`'s close
  table carry what this task adds, checked by reading.

## Design forks

1. **What `send` returns -- Recommended: the item's id, on every channel**, the `user` channel's
   included, since the host already mints one for each sent message and the id is the identity
   OR-CH3 asks for. The alternative keeps `None` and exposes the id only on the uncertain
   exception, which leaves an accepted item with no identity in the agent's hands.
2. **Resolving an uncertain item -- Recommended: by query, once by OutRig before the agent is
   told and again on `resolve(item)`.** The alternative resends under the same id and relies on
   the backend to deduplicate, which presumes a dedup the contract does not require; a backend that
   offers one can do so in its own API, outside this layer.
3. **The backend reference type -- Recommended: `str`**, since it crosses into events, the
   registry's snapshot and the save as JSON unchanged. The alternative is opaque bytes, which a
   backend keyed by binary ids would prefer and which every JSON surface would have to encode.
4. **Who may read the registry -- Recommended: a public snapshot, `Session::channels()`, with
   the per-round sends read by round**, since the owner's backend reconciles against it at a
   resume, its reply policy needs the round's sends, and an application's monitor wants it. The
   alternative keeps it crate-private for the cut and the stop read alone, which leaves a backend
   to infer positions from the lossy stream. A Python host program's read of the same is part of
   the parity `0003-51`'s fork 9 leaves open.
5. **The consumed notice -- Recommended: retried until the backend acknowledges it, idempotent
   per id for the backend.** The alternative sends it once; a notice lost to a backend's hiccup
   would then leave the acknowledged watermark behind for good, and redelivery at a resume would
   repeat items the holder consumed.

## Dependencies

- **Hard: `0003-51`.** The backend trait, the owner's push and its repeat answer, the host end,
  `send` through the backend with its exceptions, the consumed notice, the per-send round.
- **Hard: `0003-52`.** The outbound handle whose states this task extends, the inbound records
  the registry lists as received and unanswered, and the settlement notices it reports as
  acknowledged or not.

## See also

- `plan/phase/0003-python/external-channels.md` -- "Sending", "The local queue is a window",
  "Positions, the registry and the close".
- `plan/phase/0003-python/continuation.md` -- `0003-62` and `0003-65`, which read the
  registry and call the hand-over at the cut.
- `plan/phase/0003-python/rust-owner.md` -- the per-round read (`0003-48`) the registry's
  per-send rounds serve.
- `plan/phase/0003-python/messages.md` -- "Delivery rules", "Routing and attribution".
- `plan/phase/0003-python/history.md` -- "Why this is newly safe", on what enters the model's
  history.
- `plan/phase/0003-python/embedding.md` -- "What stays the embedder's", on retries OutRig does not
  make.
- `plan/phase/0003-python/observability.md` -- the lossy stream the registry does not depend on.
- `requirements/outrig/A-07-owner-side-channel-backend.md` -- OR-A7-1 and OR-A7-2;
  `A-06-owner-readable-outcomes-and-roster.md` -- OR-A6-2.
- `crates/outrig/src/python/host.rs` (`posting`, `Reply::Sent`), `crates/outrig/src/agent/
  children.rs` (`counts`, `report`), `crates/outrig/src/harness/report.rs`,
  `crates/outrig/src/python/interpreter.py` (`Endpoint.send`, `Delivery`, `_pending`).
