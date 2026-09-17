# 0112 -- A connect failure is not the same kind of transient as a read timeout

## Context

`is_transient` (`crates/outrig-cli/src/llm/retry.rs:473`) treats **every** `reqwest` transport
error as retry-worthy. That is right for a read timeout or a connection reset mid-request, and
wrong for the errors that mean the endpoint was never reachable at all: a misspelled host, a DNS
failure, a refused connect, a TLS mismatch. None of those heal in ten minutes.

The visible cost: a typo in `base-url` used to fail the first turn immediately and exit non-zero.
It now spends the full `retry-budget-secs` backing off and then ends the turn, so the user watches
ten minutes of retry lines for a one-character config error. The recovery arm's doc comment
(`exhausted_transient_label`, retry.rs:492-497) owns the *classification* as a decision rather than
an oversight -- for an interactive REPL, ending the turn and naming the connection failure beats
killing the session -- but the *wait* is not defensible.

Two in-crate test fixtures point at the discard port and both set `retry_budget_secs: Some(0)` to
stay fast (`crates/outrig-cli/src/cli/run.rs`, `crates/outrig-cli/src/subagent/mod.rs`). That is
the tell: they are compensating for the classification, not for the fixture.

## Goal

Separate "the endpoint never answered" from "the endpoint answered badly", and give the first a
much shorter budget than the second.

`reqwest::Error::is_connect()` separates the two mechanically, but not cleanly enough to make
connect failures terminal outright -- it is also true for a provider briefly down behind a load
balancer, which is exactly the case the retry exists for. The honest middle is a second, much
shorter bound for failures that happen before the endpoint has ever returned bytes: long enough to
ride out a restarting load balancer, nowhere near long enough to sit through a typo. Once the
connection has produced a response, the full budget applies for the rest of that request.

## Deliverables

- **`RetryPolicy` carries both bounds.** The existing `budget` field keeps its meaning and its
  name; a second field bounds the pre-first-byte case, defaulting to roughly 30 seconds.
  **Two distinct names, not one field that means different things depending on state** -- 0113
  reads the short one directly as its "move to the next candidate now" signal, and a field whose
  meaning depends on the caller's context is not a signal anything else can consume.
- **`send_with_retry` (retry.rs:283) tracks whether any attempt has produced a response**, and
  `next_delay` (retry.rs:437) checks `elapsed` against the applicable bound. `next_delay` already
  takes every input by parameter so the whole decision is unit-testable with no RNG and no
  sleeping; the new bound joins them rather than being read off `self`.
- **Both discard-port fixtures drop `retry_budget_secs: Some(0)`** and stay fast, which is the
  end-to-end proof that the classification and not the fixture was the problem.
- **`doc/concepts/llm-providers.md`**'s transient-failures section names the split, including the
  fact that the two bounds are separately configurable or separately fixed -- whichever the
  implementation lands on, the doc has to say which.
- **`crates/outrig-cli/src/llm/retry.rs`'s module doc** grows a sentence: it currently describes
  one wall-clock budget shared by both layers, and that is about to be two.

## Design forks

1. **Is the short bound configurable -- Recommended: no, a constant, like `MAX_RETRY_AFTER`.**
   retry.rs already holds five tuning constants (`BASE_DELAY`, `MAX_DELAY`, `MAX_RETRY_AFTER`,
   `MIN_DELAY`, `RESPONSE_RETRY_ATTEMPTS`) that no config key reaches, on the reasoning that they
   describe how to be a well-behaved HTTP client rather than a preference. "How long a restarting
   load balancer takes" is that kind of number. A config key can be added later additively; the
   reverse is not true.
2. **What `retry-budget-secs = 0` means -- Resolved: still no retries, anywhere.** It is documented
   as the one "no retries" knob and the module doc says so outright (retry.rs:33-34). A zero budget
   must short-circuit the short bound too, or the knob acquires an exception.
3. **Whether the short bound also gates `RetryingModel` -- Resolved: no.** That layer retries a
   `200 OK` with an unusable body, which by construction means the endpoint answered. It is bounded
   by `RESPONSE_RETRY_ATTEMPTS` and the full budget, and neither changes here.

## Acceptance

- A misspelled `base-url` ends the turn in seconds rather than minutes, and the message says the
  endpoint was never reached.
- A read timeout or a `503` still gets the full `retry-budget-secs`.
- A request that connects, gets a `503`, and then fails to reconnect on the retry is on the *full*
  budget, not the short one -- the connection produced a response once.
- Both discard-port fixtures drop `retry_budget_secs: Some(0)` and the suite stays fast.
- `retry-budget-secs = 0` still performs exactly one attempt, with no wait, on both paths.
- `RetryPolicy`'s two bounds are named distinctly enough that 0113 can read the short one without
  knowing which request state produced it.

## Risks

- **`exhausted_transient_label` gets a second way to be reached quickly.** Its doc comment
  (retry.rs:492-497) explicitly names the typo'd `base-url` case as the deliberate reach of the
  current classification and points here as the narrowing. That comment must be rewritten by this
  task, not left describing the old behavior -- it is the only in-tree prose that explains why an
  unreachable endpoint ends a turn rather than the session.
- **Thirty seconds is a guess.** It is bounded on one side by a load balancer restart and on the
  other by a user's patience, and neither is measured. Landing it as a named constant with the
  reasoning in a doc comment is what makes it revisable.

## Dependencies

None hard. Post-0.2.0: this touches only `outrig-cli`, whose internals 0093 made private, so no
`crates/outrig` public surface moves and no CHANGELOG entry is required.

**0113 depends on this**, and closely: without a short pre-first-byte bound, a failover chain of
three candidates multiplies the full budget by three against a total outage. See 0113's
Dependencies.

## See also

- `crates/outrig-cli/src/llm/retry.rs` -- the module doc's one-budget description (19-34),
  `RetryPolicy` (76), `send_with_retry` (283), `next_delay` (437), `is_transient` (473), and
  `exhausted_transient_label` (502) with the comment that points here.
- `plan/todo/0113-model-alias-failover.md` -- the consumer of the short bound.
- `plan/next/streaming-path-has-no-http-retry.md` -- the other gap in the same layer, deliberately
  not folded in here.

## Decisions

- **`is_connect()` is read where the type still exists.** `send_with_retry` boxes the
  `reqwest::Error` into `HttpError::Instance(Box<dyn Error>)`, which erases it; every
  downstream reader, `is_transient` included, sees only the box. So the connect/answered
  question is settled in the `Err(error)` arm *before* the box, and carried forward as a
  `bool`. `is_transient` is unchanged -- it still answers "was this retried", which stays
  true of a connect failure, and 0112 was never about the classification.

- **`answered` latches rather than being recomputed per attempt.** The acceptance case of
  a request that connects, takes a `503`, then fails to reconnect requires the *history*
  of the request, not the state of the current attempt: `answered |= !error.is_connect()`,
  and a non-success response sets it outright. Once true it never clears, so the full
  budget applies for the rest of that request.

- **The bound is `budget.min(connect_budget)` while unanswered, not `connect_budget`.**
  Fork 2 requires `retry-budget-secs = 0` to stay the single "no retries" knob; taking the
  smaller of the two gives that for free and also stops the short bound from *widening* a
  deliberately tiny budget. Both directions are pinned by tests.

- **`next_delay` takes `answered` as a parameter, not off `self`.** It already takes every
  input by parameter so the whole decision is testable with no RNG and no sleeping; the
  new bound joins that convention rather than breaking it.

- **`RetryingModel` passes `answered: true` unconditionally.** Per fork 3 that layer is not
  gated by the short bound: reaching it at all means a `200 OK` came back, so the endpoint
  answered by construction.

- **One real-clock test needed an explicit opt-out.**
  `full_idle_tree_shuts_down_within_the_grace` measures shutdown join time against the
  discard port on a real clock (`multi_thread`, no `start_paused`). With the fixtures'
  `retry_budget_secs: Some(0)` dropped, its rounds legitimately spent the full 30s connect
  budget before settling, turning a 1.5s test into a 30.3s one. It now sets
  `retry_budget_secs` off locally via a new `test_resolved_without_retries` fixture, which
  names *why* in the test that measures something other than retry. The two fixtures 0112
  names are unchanged in intent: they run on the shared default and are fast because of the
  connect bound, not because of a crutch. Sibling
  `full_in_flight_tool_tree_shuts_down_within_the_grace` needed nothing -- it already points
  at a live loopback server rather than the discard port.

- **The retry progress line reports the bound actually in force.** It previously always
  printed `policy.budget`; against an unreachable endpoint that would count seconds down
  against a ten-minute budget the loop would never reach.

- **`RetryPolicy::bound(answered)` owns the choice, rather than each reader making it.**
  The loop both *decides* against the bound and *prints* it, and a rule spelled out twice
  is a rule that can drift. It also closes a gap the field doc invites: a consumer reading
  `connect_budget` raw gets the wrong number whenever `budget` is smaller, so the `min`
  belongs on the type where 0113 will find it.

- **The two loop tests drive `send_with_retry` against a real socket**, because `answered`
  is a local of that loop and `next_delay` cannot see it: every `next_delay` test passes
  the bool as a literal, which pins the arithmetic and not the bookkeeping. A one-shot
  listener answers `503` and is then dropped, so the retry's connect is refused -- the
  acceptance case exactly. Both were checked by mutation: flipping `answered = true` to
  `false` on the response arm, and `answered |= !error.is_connect()` to `answered = true`,
  each turns one of them red.
  They run under `start_paused`, so 120 virtual seconds of backoff cost ~10ms, and they
  build their client with `no_proxy()` for the reason `remote_http_client` already sets it
  under `cfg(test)`: reqwest reads `ALL_PROXY` from the environment and exempts no address,
  so on a machine behind a proxy a loopback request would be forwarded rather than refused.

- **The connection gets its own cap, and the wait for a response does not.** The loop
  reads the clock only *between* attempts, so the connect budget bounds the retrying and
  not the attempt: a host that drops packets instead of refusing them leaves `send`
  pending until `request-timeout-secs` -- ten minutes by default, an hour at the maximum --
  and the 30-second bound is only consulted afterwards. The fix is `connect_timeout` on
  the `reqwest` client (`CONNECT_TIMEOUT`, ten seconds, two tries inside the budget),
  *not* a deadline around the whole `send`. `send` resolves when response **headers**
  arrive, and a non-streaming completion sends them only once the model has finished
  generating -- which is why `request-timeout-secs` defaults to 600 in the first place --
  so capping the whole request at 30 seconds would cut off every long reasoning turn to
  fix a black-holed address. The split's premise survives because a connect timeout is
  `is_connect()`, so it stays classified as never-answered; that is reqwest's call rather
  than ours, and `a_connection_that_never_completes_is_a_connect_failure` pins it.

  What remains deliberately unbounded by the short budget: a connection that *is*
  established and then goes silent. From outside it is indistinguishable from a model
  thinking, so it belongs to `request-timeout-secs` and the docs now say where the line
  is.

- **The connect cap is one flat constant, not a slice of the remaining budget.** Making it
  track the budget would mean a per-request connect timeout, and reqwest sets one per
  *client* -- so a state-aware cap means rebuilding the client mid-request and discarding
  its connection pool. Two consequences are documented rather than fixed, because both
  follow the rule `budget` already had, that the clock running out never cancels an attempt
  in flight: the last attempt can overrun the short bound by up to one connect cap (~40s
  worst case, not 30), and a `retry-budget-secs` below ten seconds does not shrink the cap.
  The cap also applies after `answered` latches, so a handshake slower than ten seconds
  fails -- as a *connect* failure, which is transient, so it is retried on the full budget
  rather than ending the request.

- **The unreachable-port fixture uses a port below the ephemeral range, not a released
  one.** Binding to `:0` and dropping the listener hands the port back to the pool, where
  another test in the same binary can take it before the request goes out; a stolen port
  answering `404` would satisfy a timing-only assertion while exercising nothing. A probe
  narrows that window but cannot close it -- the port has to be one the kernel will not
  hand out, which is what the discard port the crate's other fixtures already use gives.
  The one test that cannot use it needs a port that answers *once* and then refuses, so it
  keeps a real listener; there the failure-class assertion is what catches a steal.

- **`is_connect()` is pinned on the half that can be produced locally.** A refused connect
  is deterministic and unstealable, and `a_refused_connection_is_a_connect_failure` holds
  reqwest to it. A connect *timeout* has no local recipe -- a stalled handshake needs SYNs
  to go unanswered, and filling a listener's accept queue does not do it, since the kernel
  completes the handshake from the SYN queue and the client sees a connection that is
  established and then silent. An earlier draft used an RFC 5737 documentation address,
  which is only unreachable if no local, VPN, or container route claims it; that is a test
  that passes where it is written and fails elsewhere, so it is gone.

- **`test_resolved_at` takes the budget as a parameter.** The first draft of
  `test_resolved_without_retries` cloned the fixture and poked the field through an
  `if let ResolvedProvider::OpenAi { .. }`, which would silently do nothing -- restoring a
  30-second real-clock test with nothing to point at -- if the shared fixture ever moved to
  another provider variant. The constructor already takes `base_url` this way.

- **`cli/run.rs`'s fixture keeps the default but not the original claim.** Its tests call
  `handle_sidecar_command` and never drive a turn, so the budget there is inert either way;
  the comment now says that rather than crediting the connect bound with a promptness the
  module never measures. The deliverable is still met -- the crutch is gone from both --
  but only `subagent`'s fixture was ever leaning on it.
