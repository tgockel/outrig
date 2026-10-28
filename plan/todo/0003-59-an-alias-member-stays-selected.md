# 0003-59 -- An alias member stays selected

## Context

An alias resolves to every concrete row this build can reach, in order
(`crates/outrig/src/agent/resolve.rs:270-317`, `selectable`; `:354-363`), as
`ResolvedAgent.candidates` under the alias's name in `ResolvedAgent.model` (`:198-216`).
`build_agent` makes one `FailoverModel` per agent over them
(`crates/outrig/src/agent/build.rs:56-100`), with one `RetryPolicy` whose budget is the head's
provider's `retry-budget-secs` (`:70-73`, `:210-216`) and a `ChainDeadline` armed per call
(`crates/outrig/src/agent/retry.rs:177-215`; `crates/outrig/src/agent/failover.rs:588`).
Every call starts at the head -- "sticking to candidate two for the rest of a session because of
one rate-limit window would silently downgrade the user's choice" (`failover.rs:597-601`) -- and
moves to the next member on any failure once that member's own retries are spent (`:674-680`),
each member sent a view assembled for its own window, protocol and calibration (`:349-354`,
`held_to`; `:609-616`; `crates/outrig/src/agent/budget.rs:154-185`, `:195-241`, `:122-150`). A
member with no room for the call's latest turn is passed over (`failover.rs:643`; `:521-528`);
each move is a `model.failover` event (`:381-402`); exhaustion reports every member's reason and
whether any may clear (`:405-441`, `:471-518`). The loops classify what they retry: `408`, `425`,
`429` and every `5xx`, honoring `Retry-After` (`retry.rs:1063-1065`, `:938-944`, `:1100-1133`);
connection errors and timeouts unless the request could not be built (`:1071-1073`,
`:1213-1224`); an unusable `200`, twice (`:546-647`); everything else is final at once
(`:1241-1248`; `doc/concepts/llm-providers.md`, "Transient failures"). A `400` or `422` is read
only for the role-alternation hint (`failover.rs:539-542`), and `content_filter` on an empty reply
is a cut-short turn (`retry.rs:757-759`). `Session::model()` and `agent.started.model` name the
head's row (`crates/outrig/src/agent/mod.rs:116-118`, `:312-318`; `failover.rs:356-358`). The
alias row is `Model.alias` (`crates/outrig/src/config/mod.rs:1218-1245`), and
`doc/reference/config.md:706-710` describes the head rule as the alias's only behavior. Nothing is
pinned, and nothing of the selection is saved: `0003-58`'s format carries a continuation-metadata
section per agent whose `selection` field it names and leaves empty for this task.

OR-A5-1 asks for a sticky policy per alias -- start at the head, move only when the current
member fails in a class the policy says warrants moving, stay on the member that worked -- with
the head restart kept as the other policy, classification OutRig's own (rate-limited, server or
transport, authentication, content policy, context window), and strike counts and backoff as
policy inputs with OutRig defaults. OR-A5-2 asks that the current member by name, the alias and
the order at the time be continuation metadata, restored by member name so a catalog change does
not shift the pin, a missing member falling back to the head with a notice. OR-A5-3 asks that a
move across providers follow the compatibility rules: opaque parts omitted or refused, visible and
tool history kept, the omission in the call manifest and at a cut. OR-A5-4 asks that each member
keep its own output ceiling and window, a request that does not fit following the pass-over rule
and the pin unchanged by a pass-over.

The validation listed the pin among the research findings the requirements had dropped ("A
sticky alias pin persisted across resume ... `FailoverModel` restarts every completion at the
head; nothing is pinned or saved"); the adversarial review's issue 2 named the consequences --
an alias whose head is failing pays the failure on every call, and a resumed conversation cannot
carry the pin. A-05 makes it required. The recorded reason for the head restart is not reversed:
`head` stays the default and `model-policy.md`, "Two policies, spelled on the alias", says why the
reason does not hold where the alias row asks for a pin. `model-policy.md`, "Sticky selection",
designs the rest: the classes, strikes, the rotation on exhaustion, what never moves the pin, the
pin's scope, no automatic return, restore by name, the cross-provider move, and the retry budget.

Downstream, for the record: `0003-64` adds the notice line that reads `model.selection.restored`.

## Goal

An alias declared `sticky` keeps each agent's calls on the member that last answered, moving only
on a failure class the policy names, and the member is saved by name and restored by name in a
resumed session.

## Deliverables

- **Two keys on the alias row.** `selection`, `"head"` (the default) or `"sticky"`, and `strikes`,
  an integer of at least 1, default 1, `sticky` only: fields on `Model`
  (`config/mod.rs:1218-1245`), refused on a provider-shape row as `max-tokens` is refused on an
  alias, and `strikes` refused on a `head` alias; carried to `ResolvedAgent` by `resolve_agent`.
  `doc/reference/config.md`'s "Alias models" rewrites the head rule as the `head` policy, adds the
  other and the two keys with their validation rules; `doc/concepts/llm-providers.md`, "Aliases",
  says the same in its words. `outrig run` ignores both and says so at startup, as it does
  `context-window`.
- **The pin.** `FailoverModel` under `sticky` holds the index of the member a call starts at,
  beginning at the head, or at the member the import restored, or at the parent's pin for a child
  launched on the parent's alias (fork 3). `completion()` (`failover.rs:581-690`) starts at the
  pin, moves on a moving-class failure to the members after it in order and then, per fork 2,
  rotates through the members before it from the head; the member that answers becomes the pin.
  Exhaustion leaves the pin where it was and ends as today (`Exhausted`). Under `head` nothing
  changes.
- **Classification.** One function over a member's failure -- the `CompletionError`, its status
  and transport cause, and the finish or stop reason the body carries (`retry.rs:861`,
  `body_finish_reason`) -- into the classes `model-policy.md` lists: rate-limited; server or
  transport; authentication; content policy; context window; other. `0003-48` lands first and
  reports a failure's class as the label the attempt renders (`retry.rs:1194-1211`); this task
  makes the classification a type, and the stop read's `unavailable` and `failed` states carry
  it beside the label from then on, so the owner and the policy read one class.
- **What each class does under `sticky`**: rate-limited retries in place within the budget and is
  one strike when the budget is spent, the pin moving after `strikes` in a row and an answer
  clearing them; server or transport retries in place and moves when the loop gives up;
  authentication and content policy never move, the call ending on the member and the failure
  surfacing to the owner; context window passes the call over to the next member that fits with
  the pin unchanged, the existing pass-over (`failover.rs:643`) and a too-long `400` where the body
  can be read; other never moves. No new backoff input: the loop's curve and `Retry-After`
  (`retry.rs:100-108`, `:1116-1133`), the runner's reopen backoff
  (`crates/outrig/src/agent/runner.rs:46-50`) and `retry-budget-secs` are the inputs, each with
  its default, and the chain's budget stays the head's provider's (`build.rs:70-73`).
- **Per-member ceiling and window, unchanged.** `held_to(index)` and the per-member view stay as
  they are; a request that does not fit the pinned member is passed over and reported in the
  manifest, never truncated; the pin is unchanged by a pass-over.
- **Events**, integration audit, added to `doc/reference/events.md`: `model.failover` gains
  `class`; `model.selection.moved` (`alias`, `from`, `to`, `class`, `call_id`), once per change of
  pin; `model.selection.restored` (`alias`, `member`, `requested`, `order`, `fallback`), once per
  resumed agent on a sticky alias. `agent.started.model` and `Session::model()` name the pinned
  member at start, which keeps their meaning; the roster row of `0003-49` gains `member`, the
  pinned member in use for an agent on an alias, beside `model` as named.
- **Continuation metadata.** The `selection` field of `0003-58`'s section, per agent: `alias`,
  `member`, `order` and `policy`, written at export for an agent on a sticky alias and null
  otherwise. At import (`0003-58`'s `SessionBuilder::history`), the alias is resolved as the
  catalog stands, and the pin is restored by name, wherever the member stands in the new order;
  a member not among the new candidates falls back to the head with `fallback: true`; a saved
  selection for an alias now `head`, or for an agent now on a concrete row, is kept unapplied and
  the event says so. `0003-64` adds the notice line that reads the event.
- **A cross-provider move** follows `0003-58`'s compatibility rule: the next call's `model.call`
  names each part omitted in `left_out` with its reason, visible and tool history go whole, and an
  export taken after records the omission; this task adds the sticky case to `0003-58`'s
  mid-session move and changes nothing of the rule.
- **A child's starting pin**, per fork 3: the parent's pin at launch, given with the model name
  the launch inherits (`crates/outrig/src/agent/children.rs:455-456`), for a child on the same
  alias; the head otherwise.
- **`lifecycle.md`'s close table** gains a row: *a call moving between members at the close* --
  the round is dropped as today and the attempt in flight records itself cancelled
  (`crates/outrig/src/agent/ledger.rs:707-720`, `Pending`'s drop); the pin is as it was before the
  call; nothing waits. Tested here.
- **Docs and records.** `doc/reference/config.md`, `doc/reference/events.md` and
  `doc/concepts/llm-providers.md` as above; `failover.rs`'s module comment and the comment at
  `:597-601` describe both policies; the `[Unreleased]` paragraph of `crates/outrig/CHANGELOG.md`
  names the policy; `crates/outrig/public-api.txt` regenerated. No item of A-08's facade changes.
- **`## Decisions` records**: that the head restart stays the default and why the recorded reason
  does not reverse; the rotation or the stop at the end of the order (fork 2); the pin's scope and
  a child's starting pin (fork 3); no automatic return (fork 4); the class each failure the tests
  drive fell into, and where a too-long `400` could and could not be read; and the `selection`
  field's schema as written.

## Acceptance

Each is a mock-driven test unless it says otherwise, over an alias `[a, b, c]` of mock members.

- **A sticky alias moves on a server error and stays.** `a` answers `500` past its retry budget
  (the test's budget is short); the call completes on `b`; `model.failover` names `a` to `b` with
  class server or transport; `model.selection.moved` names `b`; the next call starts on `b` and
  `a` receives no request. The same alias under `head`, or with `selection` unset, sends the next
  call to `a` first and records no `model.selection.moved`.
- **A rate limit retries in place, then strikes.** `b` answers `429` with `Retry-After: 1`; the
  loop waits and retries `b` within the budget before any move; with `strikes = 1` the pin moves to
  `c` when the budget is spent; with `strikes = 2` the first spent budget fails the round on `b`
  with the pin unchanged, and the next call's spent budget moves it; an answer from `b` between
  the two clears the strike, so the pin stays.
- **A `401` stays and surfaces.** `b` answers `401`; the call fails on `b`; `c` receives no
  request; the pin is `b`; the round fails terminally as today; after `0003-48` lands, its stop
  read reports the class. A content-policy refusal -- an empty reply with `content_filter` after
  the model layer's re-roll -- stays on `b` the same way and reports its class.
- **A pass-over leaves the pin.** `b` has a window the call's latest turn does not fit; the call
  goes to `c`, `model.failover` records the pass-over with `attempt_id` null and the manifest
  records what each was sent, as today; no `model.selection.moved` is recorded; the next call
  that fits starts on `b`. Each member's `max_tokens` on the wire is its own.
- **Exhaustion and rotation.** With the pin on `c` and every member failing in a moving class,
  the call tries `c`, then `a`, then `b`, ends `Exhausted` naming all three, and the pin is still
  `c`; with `a` recovered, the next call tries `c`, fails, tries `a`, answers, and the pin is `a`.
- **Restore by name.** A session on the sticky alias is exported with the pin on `b`; a new
  session whose alias is `[c, b, a]` imports it and its first call goes to `b`, with
  `model.selection.restored` naming `b` and `fallback: false`; one whose alias is `[a, c]` imports
  it and its first call goes to `a`, with `fallback: true` and `requested: "b"`; one whose alias is
  now `head` applies nothing and says so; one on a concrete row keeps the field unapplied. The
  re-exported document carries the field as the new session left it.
- **A cross-provider move.** `0003-58`'s mid-session move, with the alias sticky: `a`, an
  Anthropic mock, fails in a moving class after a signed reasoning block was committed; the call
  completes on `b`, a Chat Completions mock; `b`'s `model.call` names the block in `left_out` with
  its reason; visible and tool history are on the wire; the pin is `b`; an export taken after
  records the omission.
- **A child starts at its parent's pin.** A parent whose pin moved to `b` launches a child on the
  same alias: the child's first call goes to `b`; a child launched on another alias starts at its
  own head; a later move in the child does not move the parent.
- **One request of the limit.** With `0003-50`'s limit, a call that moved from `a` to `b` advances
  the agent's count by one.
- **Config.** `selection = "sticky"` on a provider-shape row, `strikes` on a `head` alias, and
  `strikes = 0` are refused at load naming the key; `outrig run` prints its ignoring line.
- **Close row.** A session closed while a call is moving from `a` to `b` records the attempt
  cancelled and leaves the pin on `a` in the export of a cut taken before the close, where
  `0003-62` exists, and otherwise in the pin a test reads off the chain.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Where the policy is spelled -- Recommended: on the alias row, `selection` and `strikes`.**
   Which members are equivalent and how to treat a failure among them is a property of the alias;
   an agent chooses an alias. The alternative, an owner input on the builder or the agent, lets two
   agents on one alias disagree about it and gives a config file no place to say it.
2. **At the end of the order -- Recommended: rotate through the members before the pin, from the
   head**, so no member is skipped and a pin on the last member does not fail every call while
   the head has recovered. The alternative stops at the end and ends the call `Exhausted` with the
   members before the pin untried, which is simpler and leaves the silent downgrade `failover.rs`
   warns of in place for a pin on the last member.
3. **The pin's scope -- Recommended: per agent, with a child starting at its parent's pin** when
   it runs on the same alias. The chain, its calibrations and its slot are per agent, and the
   metadata is per agent. The alternative is one pin per alias per Session, shared by every
   agent's chain: a move in one agent moves every other's next call.
4. **Moving back -- Recommended: never on its own.** The rotation is the one way the pin lands on
   an earlier member. The alternative probes the head after a time or a count of calls, which adds
   a timer and a kind of move the owner cannot predict.
5. **Reading the member in use -- Open; recommended: `Session::model()` keeps its meaning and
   names the pinned member**, and the roster of `0003-49` carries the model in use per agent. The
   alternative returns the alias name with the member beside it, a richer public type to settle
   before the release.

## Dependencies

- **Hard: `0003-15`.** The chain, the retry loops, the classification they apply, and the
  `model.retry` and `model.failover` events.
- **Hard: `0003-12`.** The per-member budget, the pass-over and the manifest's `left_out`.
- **Hard: `0003-58`.** The continuation-metadata section this task fills, the import that restores
  the pin, and the compatibility rule a cross-provider move follows.
- **Soft: `0003-48`.** The stop read, whose `unavailable` and `failed` states carry a failure's
  class as a label, which this task's classification joins, and the owner's read of a `401` or a
  refusal on the pinned member.
- **Soft: `0003-49`.** The roster row that gains the member in use.
- **Soft: `0003-50`.** One `call_id` is one request of the limit, which one acceptance asserts.

## See also

- `plan/phase/0003-python/model-policy.md` -- "Sticky selection", every section, and "What
  the two share".
- `plan/phase/0003-python/continuation.md` -- "The format": the compatibility rule and the
  metadata section.
- `plan/phase/0003-python/rust-owner.md` -- the stop read and the roster.
- `plan/phase/0003-python/history.md` -- "The view", "The budget".
- `plan/phase/0003-python/observability.md` -- "What is emitted", the two ids.
- `plan/done/phase/0003-python/tasks/0003-15-retry-and-failover-come-across.md` -- its
  `## Decisions` on the chain, the shared budget and the per-candidate ceiling.
- `plan/phase/0003-python/potential/cost-aware-commentary-preamble.md` -- the per-wire rule as
  it stands.
- `crates/outrig/src/agent/failover.rs`, `retry.rs`, `build.rs`, `resolve.rs`, `budget.rs`,
  `ledger.rs`; `crates/outrig/src/config/mod.rs`, `validate.rs`; `crates/outrig/src/harness/
  event.rs`; `doc/reference/config.md`; `doc/concepts/llm-providers.md`.
