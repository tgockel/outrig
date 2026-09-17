# 0113 -- Model alias failover: move to the next candidate when one fails mid-turn

## Context

`plan/done/0110-model-aliases.md` divides aliases into a **static half** -- select one candidate at
resolve time from what this build is credentialed for -- and a **runtime half**, which moves to the
next candidate when one fails mid-session. 0110 ships only the static half, and says so twice:
under Deliverables ("**Not in the first pass**: the runtime half") and under Design forks §2, whose
whole argument is that the split is safe *because* "the config surface is identical for both, so
the second half is additive to the first."

This is that second half. It is the one the phrase "failover" actually describes, and 0110 is
emphatic that the static half must not be mistaken for it:

> Building a remote client does no network I/O [...] so static selection is deliberately blind to
> whether the endpoint is *up*. It answers "am I configured for this" and not "is this working",
> and the docs must say so, or the failover use case will be assumed to be covered when it is
> half-covered.

The gap the static half leaves is exactly the one 0110's Context named as the second trigger:
**which vendor is up right now?** A rate-limit window on one endpoint ends the turn
(`handle_prompt_error`, `crates/outrig-cli/src/llm.rs:1113`) even though two other endpoints serve
the same weights, and the static half cannot help, because it made its choice before the window
opened.

Nothing in this entry changes the config surface. `alias = ["opus-5-bedrock", "opus-5-anthropic",
"opus-5-azure"]` already means an ordered list of provider-equivalent rows; 0110 committed to that
meaning holding for both halves, and this entry is what makes the ordering matter at runtime rather
than only at startup.

## Goal

Lift the candidate choice from once-per-session to once-per-`completion()`-call, so a rate limit or
an outage on candidate one moves the turn to candidate two instead of ending it -- without
re-executing any container tool call the turn has already run.

## Deliverables

- **`FailoverModel`**, a `CompletionModel` holding an ordered set of candidates, in
  `crates/outrig-cli/src/llm/` beside `retry.rs`. It is the third sibling to `RetryingHttpClient`
  and `RetryingModel`, at the same layer and for the same reason -- see Layer below.
- **An object-safe candidate shim**, because `CompletionModel` is not object-safe.
- **`ResolvedAgent` carries the chain**, via a new `ResolvedCandidate`. See Shape.
- **`build_agent` gains a fourth construction path** and `RigAgent` a `Failover` variant.
- **A chain-scoped retry deadline**, which is the blocker 0110 named and the reason this entry
  depends on 0112. `RetryPolicy` loses `Copy`. See The shared budget.
- **`handle_prompt_error`'s wording changes for a multi-candidate chain**, because the sentence it
  prints today is a claim about one endpoint and would become false. See The move rule.
- **Docs**: `doc/concepts/llm-providers.md` (whose transient-failures section 0112 also edits, and
  which is where 0110 was required to state that static selection does *not* cover failover -- that
  sentence needs its counterpart here), and `doc/reference/config.md` (a **symlink** into
  `crates/outrig-cli/src/mcp_self/docs/` -- edit the target) wherever `alias` describes what an
  ordered list buys.
- **Not required**: any change to `crates/outrig`. Nothing in the public surface moves; see
  Dependencies.

## Runtime behavior

### Layer -- Resolved

Failover happens *inside* one `completion()` call, never around `agent.prompt(...)`. The obvious
placement -- run the whole turn against candidate one, re-run it against candidate two -- is wrong,
and `crates/outrig-cli/src/llm/retry.rs`'s module doc (8-13) already contains the argument:

> rig runs tools *between* `completion()` calls, never inside one, so replaying either a single
> request or a single model call re-executes no container tool call. Retrying the whole
> `agent.prompt(...)` would -- a turn that fails on a *later* model call has already run the tool
> calls from the earlier ones -- which is why neither layer is up there.

Failover inherits that constraint unchanged. A turn is a sequence of `completion()` calls with
container tool calls between them; moving candidates *between* calls is free, and moving them
*around* the turn re-runs side effects that already happened in a container. This is the property
that decides the design, so it earns a test rather than only a comment -- see Acceptance.

### Shape -- Resolved

**`ResolvedAgent` grows the chain.** Today it flattens to a single candidate: `provider_name`,
`provider`, `model_identifier`, `model_weights`, and `max_tokens` are five fields describing one
row (`llm.rs:193-219`). Under failover those five travel *per candidate*. Introduce a
`ResolvedCandidate` holding them and have `ResolvedAgent` hold a `Vec<ResolvedCandidate>`, with the
first element being what every existing reader already means.

`model_name` keeps the meaning 0110's Risks pinned it to -- the *concrete* row, because
`LlmRegistry` is keyed on it (`llm/registry.rs:27`) and an alias name reaching it loads the same
GGUF twice. With a chain, that reasoning applies per candidate: the key is the candidate's concrete
name, never the alias's.

**Candidates need an object-safe shim.** `CompletionModel` is not object-safe -- a `Clone`
supertrait, associated types, `impl Future` returns, and a generic `make` -- so the candidates
cannot be `Box<dyn CompletionModel>` directly. A small trait with boxed futures and
`CompletionResponse<()>`, implemented over the concrete arms, is what lets one `CompletionModel`
impl stand in front of a heterogeneous set. `FailoverModel` holds the `Vec` and implements
`CompletionModel` in terms of it.

Erasing `Response` to `()` is sound because nothing in outrig ever *reads*
`CompletionResponse::raw_response`. Note this is a property of outrig, not of rig, so it wants a
comment at the erasure site: the only production construction is `llm/mistralrs.rs:591` and it is
never consumed, and the two remote arms' `Response` types are rig's own, which outrig never
touches. (0110 also cites `llm.rs:1719` for this; that line is now `llm.rs:1745` and is a test
fixture inside `mod tests`, so it is not evidence either way.)

**`type Client = ()` and an unreachable `make`.** `RetryingModel::make` faces the same situation
and resolves it differently -- it delegates to `M::make` (retry.rs:209-211) on the reasoning that
outrig never constructs a model through rig's client path but it has a real `Client` to delegate
with. `FailoverModel` erases the client type and so has nothing to build from; the comment there
should say that, rather than repeating `RetryingModel`'s.

**`finish_agent` is already generic** over `M: CompletionModel + 'static` (`llm.rs:1457`), so it
takes a `FailoverModel` with no change.

**`composes_native_output_with_tools` must answer for the whole chain.** `RetryingModel` delegates
rather than defaulting, and its doc comment (retry.rs:274-278) explains why the trait's `false` is
not free: answering `false` for OpenAI and Anthropic costs them guaranteed structured output on
every turn that has tools. A chain cannot delegate to candidate one, because rig resolves the
output mode *before* the call and a move mid-call cannot re-resolve it. The conservative reduction
-- every candidate ANDed -- is the answer, and a homogeneous chain (the overwhelmingly common case:
the same weights on three vendors) pays nothing for it.

### What gets rewritten per candidate -- Resolved

0110 says the wrapper retargets `request.model` because `anthropic.claude-opus-5-v1:0` is not
`claude-opus-5`. Checked against rig 0.40, that is true but not load-bearing, and the distinction
matters for what the shim must store:

- **`request.model` arrives `None` on the agent path.** `Agent::completion` builds through
  `model.completion_request(prompt)` and sets only `.temperature_opt` / `.max_tokens_opt`
  (`rig-core-0.40.0/src/agent/completion.rs:555-559`), and each candidate's rig model already
  carries its own identifier from `client.completion_model(&identifier)`. Both providers do honor
  an override when one is present (`providers/anthropic/completion.rs:2456-2459`,
  `providers/openai/completion/mod.rs:1853`), so setting it is correct and costs nothing -- but it
  is belt-and-braces against a future rig that populates the field, not the mechanism.
- **`request.max_tokens` is the rewrite that matters**, and 0110's Design fork §4 is why. The
  ceiling is folded into the agent at resolve time (`resolve_agent_with_overrides`, `llm.rs:413`),
  and the Anthropic arm additionally caps it against what the identifier publishes
  (`llm.rs:633-636`) before `finish_agent` bakes it into the `Agent`. So the value reaching
  `request.max_tokens` is *candidate one's, computed at build time* -- under failover the ceiling
  would stay candidate one's while the identifier moved. Each candidate's build-time ceiling must
  therefore be stored on the shim and written into the request on a move.

**This resolves 0110's Design fork §4** as: yes, `FailoverModel` rewrites `max_tokens` per
candidate, from a value `build_agent` computes per candidate through exactly the precedence the
single-candidate path uses today.

### The move rule -- Resolved

`FailoverModel` moves to the next candidate when the current candidate's own retry stack has given
up -- that is, on any `Err`, including ones that are terminal *for that candidate*. A `401` is not
retryable and must keep ending the process today; in a chain it is a reason to try the next
candidate, whose key may be fine. What must not happen is a failure disappearing: the chain records
why each candidate was abandoned and, on exhaustion, reports all of them together, in the
per-candidate shape 0110 already specifies for the static half's no-selectable-candidate error.

This touches a soundness claim that is currently written down, and rewriting it is part of the
work. `exhausted_transient_label`'s doc comment (retry.rs:486-490) says:

> Sound only because there is exactly one retry layer, and everything it can retry it *did* retry
> until the budget stopped it. Adding a second retry layer, or an early return for a retryable
> status in `send_with_retry`, would make this claim more than it knows.

Failover is a third layer, and the claim survives only if what reaches `handle_prompt_error` means
"every candidate failed" rather than "one endpoint failed". Concretely, `handle_prompt_error`'s
`"LLM endpoint failed and did not recover ({label})"` (`llm.rs:1125`) is a sentence about one
endpoint and becomes false under a chain; it needs a multi-candidate form. The single-candidate
path must keep the exact text it has today.

### The shared budget -- Resolved: a chain-scoped deadline

This is the blocker 0110 names, and the reason 0112 comes first:

> Three candidates at the default `retry-budget-secs = 600` is a thirty-minute turn against a total
> outage. Worse, most of that is spent retrying endpoints already known to be down. [...] without
> it failover's worst case is worse than no failover at all.

The obstacle is structural: `RetryPolicy` is baked into each candidate's `RetryingHttpClient` at
build time, and rig's `HttpClientExt::send` has no per-call context channel through which a
per-turn deadline could be handed down.

The resolution: **`RetryPolicy` carries a chain-scoped deadline handle** -- an `Arc` -- cloned into
every candidate's HTTP client and every `RetryingModel`, armed at the top of each
`FailoverModel::completion` and consulted by `next_delay` (retry.rs:437) alongside the existing
per-attempt bounds. Two consequences to state plainly rather than discover:

- **`RetryPolicy` loses `Copy`.** Its doc comment (retry.rs:73-74) names that exact cost --
  "`Copy` so the whole policy moves into a `'static` per-request future without an `Arc`" -- so
  this pays a price the code already priced rather than finding a new one. `send_with_retry` takes
  the policy by value into a `'static` future (retry.rs:283-290), which becomes an `Arc` clone.
- **A chain of one behaves exactly as today.** Arming a deadline for a single candidate reproduces
  the bound the per-attempt budget already gives, so the no-alias path is byte-for-byte unchanged
  -- the property 0110 required of the static half and this entry inherits.

Rejected: a static `budget / N` split at build time. It needs no plumbing and keeps `Copy`, but it
shrinks the budget in the case that matters most -- candidate one instantly dead, candidate two
deserving the whole thing.

0112's short pre-first-byte bound is what makes the chain *fast* rather than merely *bounded*: the
deadline caps the worst case at one budget, and the short bound is what stops each dead endpoint
from eating it.

### Exclusions -- Resolved

> **Superseded in part.** The two bullets below were written before execution and the first
> clause of each was overturned by what it found -- see decisions 9 and 16, which are the
> normative record. Streaming is **refused**, not delegated, and local candidates load **lazily**,
> not in `build_agent`. The struck text is kept because the reasoning around it still explains
> *why* those clauses looked right, but do not implement from it.

- **Streaming.** ~~`FailoverModel::stream` delegates to the first candidate.~~ **Refuses** --
  decision 9. Unifying `StreamingResponse` costs more and buys nothing here: outrig's remote turns
  are non-streaming, and the streaming arm is mistralrs-only, which is the one style with no
  endpoint to fail over from. That is also why delegating would have been an unreachable path
  whose type surgery bought nothing, and an error is the honest encoding. Matches
  `RetryingModel::stream` (retry.rs:262-270) and the precedent in
  `plan/next/streaming-path-has-no-http-retry.md`.
- **Mistralrs candidates** stay permitted in a chain -- 0110's Design fork §6 allows an alias to
  name one -- ~~but a cold multi-gigabyte load happens in `build_agent`, before the first turn,
  not on a move.~~ **It happens on first use** -- decision 16. Loading in `build_agent` let a
  fallback with missing weights take down a session whose primary was healthy. 0101's decision 7
  is still the right reference, but for the reason it gives rather than the placement: it answers
  a cold in-process load by *announcing* it, which is what decision 20 does at the moment the
  chain reaches the candidate.
- **The static half's selection rule stays.** A candidate this build cannot reach at all -- no
  provider, wrong feature, unset api-key -- is still dropped at resolve time by 0110's rule. The
  chain holds only candidates that were selectable; failover is about which of *those* is working.

## Acceptance

- A chain of two remote candidates where the first returns `503` past its bounds completes the turn
  on the second.
- **A turn that has already run tool calls and then loses candidate one moves candidates within the
  `completion()` call, and the earlier tool calls are not re-executed.** This is the property that
  decides the layer; it is a test, not a comment.
- The chain's total wall clock against a total outage is bounded by one `retry-budget-secs`, not by
  N of them.
- After a move, candidate two's `max-tokens` is what reaches the wire, not candidate one's.
- A single-candidate chain is byte-for-byte identical to today, in behavior and in the retry and
  turn-failure log lines -- including `handle_prompt_error`'s existing wording.
- Exhausting every candidate reports each candidate and its own distinct reason, not just the last.
- A `401` on candidate one moves to candidate two; a `401` on every candidate still ends the
  process rather than inviting a resend.
- `retry-budget-secs = 0` still means one attempt and no waiting, anywhere in the chain.
- `FailoverModel::stream` reaches ~~only the first candidate~~ **no candidate at all**: it
  refuses, per decision 9, and `streaming_is_refused_rather_than_fanned_out` pins that.
- A chain whose candidates disagree on `composes_native_output_with_tools` resolves to `false`; a
  homogeneous chain resolves to what its candidates say.
- Two candidates naming the same in-process model share one loaded engine -- the `LlmRegistry` key
  is the concrete name, never the alias's.

## Design forks

1. **Where failover sits -- Resolved: inside `completion()`.** See Layer; the reasoning is
   retry.rs's own and applies unchanged.
2. **Shared budget mechanism -- Resolved: a chain-scoped deadline in `RetryPolicy`, at the cost of
   `Copy`.** See The shared budget for the rejected alternative.
3. **`request.max_tokens` per candidate -- Resolved**, which closes 0110's Design fork §4.
4. **Does a move print -- Recommended: yes, once per move, on stderr beside the retry lines.**
   0110's Risks argues an alias widens what a config typo can silently do, and a chain that moves
   mid-turn means one session can span two models. That is a stronger version of the same hazard
   and the same mitigation applies: 0110's Attribution section prints the alias hop at startup;
   this prints the move when it happens. The retry loop already prints a progress line per attempt
   (retry.rs:349-360), so there is a precedent for the volume and the format.
5. **`warn_fallback_ceiling` under a chain -- Open.** See Risks. Small enough to decide during
   execution, and neither answer blocks anything.
6. **Whether the chain is per-`RigAgent` or per-turn -- Resolved: per-`RigAgent`.** Candidates are
   built once in `build_agent`, exactly as the single candidate is today, and `RebuildingAgent`
   (`llm.rs:828`) rebuilds them all when the tool list changes. Building per turn would re-pay the
   mistralrs load and buy nothing, since building a remote client does no I/O anyway.
   *The conclusion stands; the last clause does not. Building a local candidate is no longer the
   same thing as loading it -- see decision 16 -- so `LlmRegistry`, not the chain's lifetime, is
   what keeps a rebuild from re-paying the load.*
7. **Resetting to candidate one -- Recommended: yes, every `completion()` call starts at the head
   of the list.** The list is a preference order, and the whole point of the first entry is that it
   is preferred; sticking to candidate two for the rest of a long session because of one rate-limit
   window silently downgrades the user's choice. The cost is one wasted attempt against a
   still-dead endpoint per call, which 0112's short bound makes cheap -- another way this entry
   depends on that one.

## Risks

- **`warn_fallback_ceiling` fires once per process** (`llm.rs:703`), keyed on the resolved model.
  A turn that moves to a candidate with a different published ceiling has already spent the warning
  on the first. 0110 flags this and hands it here. Options: key the `Once` per candidate, or accept
  the gap and note it. The warning exists so a reply cut off at an invented ceiling reads as a
  config gap rather than a bad model, and that reasoning does not weaken under a chain.
- **A third retry layer is a claim about the other two.** `exhausted_transient_label` and
  `unusable_response_label` are both documented as sound *because* there is one layer that retried
  everything it could. Both doc comments have to be rewritten, not just the code -- they are the
  only in-tree prose explaining why a transient failure ends a turn and a bad key ends the process.
- **One session can now span two models.** The static half's hazard was picking the wrong candidate
  invisibly at startup; this one can change models between two calls of a single turn, which means
  a single reply can be half one model's work. Design fork §4 is the mitigation and is why it is
  Recommended rather than Open.
- **`RetryPolicy` losing `Copy` reaches further than this feature.** It is threaded through
  `retry_policy` (`llm.rs:525`), `remote_http_client` (`llm.rs:538`), both retry layers, and the
  `Default` impl rig's `CompletionModel` bounds require (retry.rs:100-109). The change is
  mechanical but it is not local.
- **A chain that exhausts every candidate loses the partial turn**, the same way a single endpoint
  does -- `PromptError::CompletionError` carries no `chat_history` (`llm.rs:1121-1124`). Failover
  makes that path likelier to be reached, not less. Filed separately; see Dependencies.

## Dependencies

- **Landed: `plan/done/0110-model-aliases.md`.** The config surface, the flattening walk, and the
  static half's selectability rule. This entry is 0110's deferred second half and nothing here is
  meaningful without it. Three things it left on the table, all in its `## Decisions`:
  `selectability` (`llm.rs`) predicts `build_agent`'s `local-llm` check without being linked to
  it, so a precondition added to either goes stale silently -- this entry reshapes `build_agent`
  and is the natural place to unify them; a single-target alias deliberately bypasses candidate
  selection, so a chain of one must keep doing so; and `Unselectable` carries `LlmResolveError`
  values rather than restating their text, which is the shape `FailoverModel`'s per-candidate
  exhaustion report should reuse.
- **Hard: `plan/todo/0112-connect-failures-are-not-really-transient.md`.** 0110 calls this
  dependency "close to hard"; with the chain deadline resolved it is hard. The deadline bounds the
  chain's worst case at one budget, but only the short pre-first-byte bound stops each dead
  endpoint from consuming it -- and it is also the signal Design fork §7's reset-to-head rule needs
  to be cheap.
- **Soft: `plan/next/partial-turn-history-on-failed-model-call.md`.** See the last Risk.
- **Soft: `plan/next/subagent-model-allowlist.md`.** 0110's Design fork §8 settles pre- vs
  post-resolution matching; a chain does not change that answer, but it does mean "the model this
  subagent ran on" is no longer a single value for audit purposes.
- **Scheduling: after 0.2.0 final, and unconstrained by it.** Everything here is in `outrig-cli`,
  whose internals 0093 made private, so no `crates/outrig` public surface moves, no
  `public-api.txt` regeneration, and no CHANGELOG entry. That is the reason this sits after 0111
  rather than inside the pre-freeze block.

## See also

- `plan/done/0110-model-aliases.md` -- the static half. Its Runtime behavior section
  ("Selecting a candidate: the runtime half") is the specification this entry executes; its Design
  forks §2 and §4 and its `warn_fallback_ceiling` risk are the loose ends it inherits.
- `crates/outrig-cli/src/llm/retry.rs` -- the module doc (8-13) that places this layer,
  `RetryPolicy` (73-76) and the `Copy` comment, `RetryingModel::make` (209), `::stream` (262),
  `composes_native_output_with_tools` (276), `send_with_retry` (283), `next_delay` (437), and
  `exhausted_transient_label` (502) with the one-layer soundness claim.
- `crates/outrig-cli/src/llm.rs` -- `ResolvedProvider` (156), `ResolvedAgent` (193),
  `resolve_agent_with_overrides` (248) and the ceiling fold (413), `RigAgent` (487), `retry_policy`
  (525), `build_agent` (551) and the Anthropic cap (633-636), `warn_fallback_ceiling` (703),
  `RebuildingAgent` (828), `handle_prompt_error` (1113), `finish_agent` (1457).
- `crates/outrig-cli/src/llm/registry.rs` -- keyed by concrete model name.
- `rig-core-0.40.0/src/agent/completion.rs:555-559` -- the request builder that leaves
  `request.model` unset and `max_tokens` set, which is what makes the second the load-bearing
  rewrite.
- `plan/done/0101-subagent-model-selection.md` -- decision 7's cold-load announcement.

## Decisions

1. **The chain deadline lives on `RetryPolicy` as an `Arc<ChainDeadline>`**, as specified, and
   `RetryPolicy` accordingly loses `Copy` (keeping `Clone`). It also loses `PartialEq`/`Eq`, which
   were derived and unused outside one test that compares a single field. `bound()` is the one
   reader: it takes the *minimum* of its own bound and whatever the deadline has left, so the
   chain caps the per-attempt budgets rather than replacing them, and `budget = 0` still means no
   retries anywhere (the minimum of zero and anything is zero). Two `Copy` sites needed an
   explicit `.clone()`, both in `build_agent`, where one policy is handed to both the HTTP client
   and the model wrapper.

2. **`ResolvedAgent` grew `candidates: Vec<ResolvedCandidate>`** and lost the five flattened
   fields, plus `max_tokens`, which is per candidate for the same reason (its fallback half is the
   *model's* ceiling). `alias_name`, the preamble, the temperature and the limits stay on the
   agent. Rather than repoint ~70 read sites at `candidates[0]`, `ResolvedAgent` grew accessors
   (`model_name()`, `provider()`, `max_tokens()`, ...) delegating to `primary()`, so every
   pre-failover reader keeps reading "the model" and says so in one place. `primary()` carries the
   non-empty invariant in a single `expect`.

3. **The static half now yields every selectable candidate, not the first.** `selectable_candidates`
   is the new traversal and `first_selectable` is redefined on it, so the two cannot disagree; the
   selection *rule* is untouched. A candidate this build cannot reach is still dropped at resolve
   time, so failover only ever chooses among candidates that were configured -- a chain never
   spends a move on a row whose api-key was never set.

4. **A chain of one takes the single-candidate path in `build_agent`**, producing the same
   `RigAgent` variant with the same retry stack rather than a one-element `FailoverModel`. This is
   what makes the no-alias path byte-for-byte rather than arguably-equivalent: the variant, the
   error text, and the log lines are the ones that shipped. `FailoverModel::completion` *also*
   returns a lone candidate's error unwrapped, so even a hand-built chain of one keeps
   `exhausted_transient_label`'s classification -- pinned by
   `a_single_candidate_chain_returns_its_error_unwrapped`.

5. **The chain's `retry-budget-secs` comes from the first candidate's provider.** A chain spanning
   providers that disagree has no single right answer, and the head of a preference order is the
   defensible one: it is the endpoint the user said to use.

6. **Exhaustion is a `CompletionError::ProviderError` carrying a marked prefix**, detected by
   `chain_exhausted_label`. `CompletionError` is rig's enum and has no variant for "every
   candidate failed", so the prefix is the seam; it is a named constant shared by the constructor
   and the predicate so the two cannot drift.

   Two corrections to how this was first written down, both found in review. **The ordering is
   not the mechanism.** The three predicates are disjoint by `CompletionError` variant --
   `chain_exhausted_label` matches `ProviderError`, `exhausted_transient_label` matches
   `HttpError`, `unusable_response_label` matches `ResponseError` -- so reordering them changes
   no outcome. What actually keeps `exhausted_transient_label`'s one-retry-layer claim true is a
   property of `FailoverModel`: a multi-candidate chain aggregates rather than re-raising, so no
   candidate's `HttpError` ever escapes it. Widening decision 4's "a chain of one returns its
   error unwrapped" rule to any other case is what would break it, and that is now what the
   comments say. **And the claim that both predicate docs had been rewritten was false when
   written** -- they still read "there is exactly one retry layer". They have since been
   rewritten for real, and `a_chain_exhaustion_is_a_class_of_its_own` pins the disjointness the
   prose now rests on.

   Also considered and rejected: rig's `CompletionError::RequestError(Box<dyn Error>)` is a
   real typed seam, so "rig has no variant for this" overstates it -- a `ChainExhausted` struct
   could be boxed in and recovered by downcast. Left as a prefix anyway, because
   `crates/outrig-cli/src/error.rs` already string-matches `RequestError`'s message for rig's
   own missing-`max_tokens` text, and routing a chain aggregate through that variant would put
   it past a `.contains()` check on unrelated text. The prefix is the narrower seam here, which
   is the honest reason rather than the one first recorded.

7. **Design fork §5 (`warn_fallback_ceiling` under a chain) -- resolved: keyed per model.** The
   process-wide `Once` was the wrong shape once a chain builds several candidates, since it would
   spend the warning on candidate one and stay silent about a second candidate with a different
   published ceiling -- exactly the case where a reply cut short reads as the model's doing. It is
   now a `BTreeSet` of concrete model names, so two agents on one model still warn once between
   them. The message dropped its `[agents.<name>]` clause, which needed the agent the candidate no
   longer carries.

8. **A move prints (§4), and the banner also lists the fallbacks up front.** The plan asked for the
   move announcement; the banner line is the cheaper half of the same mitigation, since a chain
   means one session can span two models and the first move should not be the first the user hears
   of them.

9. **`FailoverModel::stream` refuses rather than delegating to the first candidate.** The plan
   allowed delegation, but the only streaming style is mistralrs, which `build_agent` never wraps
   in a chain -- so delegation would be an unreachable path whose type surgery bought nothing. An
   error is the honest encoding, and `streaming_is_refused_rather_than_fanned_out` pins that it
   reaches no candidate.

10. **`build_agent`'s provider arms were factored into `openai_client`, `anthropic_client`,
    `anthropic_model` and `mistralrs_model`**, shared by the single path and the chain. Without
    this the three-tier ceiling precedence and the Anthropic cap would exist twice, and a chain
    whose candidates were built by a simplified copy is exactly how the per-candidate ceiling
    would quietly stop being the real one. The OpenAI arm was left transcribed twice in the
    first pass and caught in review -- which is the failure mode this decision describes,
    arriving in the change that introduced the decision: `match` exhaustiveness catches a
    *missing* arm and nothing catches a *divergent* one, and the chain's OpenAI path is the one
    with no test of its own (the mock-chain test is Anthropic at both ends, and every
    single-candidate session short-circuits to `build_single`).

11. **Not done: the `selectability` / `build_agent` unification** 0110 left on the table and this
    entry called "the natural place". `build_agent` no longer has a `local-llm` precondition to
    unify -- the check reached is `MistralrsFeatureDisabled` in the per-candidate builder, which
    `selectability` already predicts by the same `cfg!`. Left as is rather than adding indirection
    to formalize an agreement that is now one line in each.

12. **The layer property is pinned by an integration test against two mock endpoints**, not by a
    unit test over fake candidates. `tools_run_before_a_move_are_not_re_executed` lives in
    `tests/anthropic_mock.rs` beside `a_retry_mid_turn_does_not_re_run_the_tool_calls_before_it`,
    which is the same property one layer down; the head serves a `tool_use`, the tool runs, and
    only then does the head `503`, so the move happens with a tool result already in the history.
    Its sharpest assertion is that the head's failed request body and the next candidate's body
    are *equal*: a move replays one model call verbatim against another endpoint rather than
    rebuilding the turn. Building it widened `build_mock_agent` to take a slice of env-var names,
    because candidate selection drops any row whose key is unset -- a chain that set only the
    head's var would have resolved to a chain of one and passed for the wrong reason.

13. **The chain deadline is tested through `left()` under a paused clock**, not through wall
    time. Eight tests in `retry.rs` cover the armed cap, the unarmed no-op, a spent deadline
    leaving the next candidate no retries, `budget = 0` surviving an armed deadline, that a
    deadline caps but never widens a candidate's own bounds, and the two in decision 18. The
    one worth naming is
    `a_cloned_policy_shares_the_deadline_rather_than_copying_it`: arming happens on the
    `FailoverModel`, above the candidates, and has to be visible in the retry loops underneath --
    which is the entire reason the handle is an `Arc` and `RetryPolicy` gave up `Copy`. A clone
    that copied the deadline would compile, pass every other test, and leave each candidate
    spending a full budget.

14. **Two gaps left by the change were filed rather than fixed here.** Every attribution surface
    reports candidate one, so a subagent transcript names a model that may not have written the
    reply (`plan/next/chain-attribution-names-the-first-candidate.md`) -- the banner and the move
    announcement mitigate this for the interactive user only. And `print_banner` has no tests at
    all, so the failover line it gained can be deleted with every test still green
    (`plan/next/startup-banner-has-no-tests.md`).

15. **`plan/todo/README.md` lost its per-task retrospective narrative.** Roughly 125 of its 157
    lines were `NNNN landed, and ...` paragraphs restating design calls that `plan/done/<task>.md`
    already records -- and `.claude/CLAUDE.md` names *that* section the authoritative one. A
    second copy of a design call is one that can disagree with the first, so the file is now the
    directory's contract (ordering rule, the two skills, where follow-up work goes) plus the one
    genuinely forward-looking note it carried, on `public-api.txt` regeneration being each task's
    own deliverable.

16. **Design fork §6 was wrong about local candidates, and they now load lazily.** It reasoned
    that building every candidate up front is free because a remote client does no I/O, and
    chose eager construction so a cold multi-gigabyte load could not surprise anyone mid-turn.
    A mistralrs row breaks that premise twice: its construction *is* the load, and unlike a
    remote client it can **fail**. Eagerly, a fallback whose GGUF is missing takes down a
    session whose hosted primary is perfectly healthy -- which inverts the reason for naming a
    fallback at all, making a second model a new way to fail to start rather than a way to
    survive an outage. `LazyLocalCandidate` defers the load to the moment the chain reaches the
    candidate, where a failure becomes that candidate's failure and the chain moves past it.
    The mid-turn-surprise objection is answered the way 0101's decision 7 already answered it,
    by announcing the wait rather than by moving it -- though that announcement was *claimed*
    here before it was written, and only exists after the correction in decision 20. Two
    consequences: `build_agent` and friends
    take `&Arc<LlmRegistry>` so a candidate can outlive the call, and a lazy candidate must
    answer `composes_native_output_with_tools` *unloaded* -- sound only because that is a
    property of the type (`MistralrsModel` takes rig's `false` default), which is now stated at
    the site. `a_broken_local_fallback_does_not_stop_the_session_from_starting` pins it and
    needs no weights, because the whole point is that none are loaded.

17. **All-terminal chain exhaustion ends the process; mixed exhaustion ends the turn.** The
    first cut routed *every* chain exhaustion through the recoverable path, so two candidates
    returning `401` for revoked keys were reported as a temporary outage and the user was told
    to resend a prompt no key could satisfy. That silently violated this entry's own acceptance
    criterion -- "a `401` on every candidate still ends the process rather than inviting a
    resend" -- which had a test for the moving half and none for the ending half.
    `ChainExhaustion` now carries a `recoverable` flag, set when *any* candidate failed for a
    reason a resend could fix, because one vendor rate-limiting while another's key is revoked
    is still worth waiting out. The predicate is `retry::is_recoverable`, defined beside the two
    label predicates it is the union of so a chain's verdict on an error cannot drift from what
    the single-candidate paths do with that same error.

18. **The chain deadline was subtracting elapsed time twice.** `bound()` folded the deadline's
    *absolute* remainder in beside the candidate's *relative* budget, and `next_delay` then
    subtracted `elapsed` from the result -- charging the same seconds to both. A chain therefore
    gave up after roughly half its budget and moved off the preferred candidate early. The two
    quantities are now separate: `bound()` is the duration from this request's first attempt
    (and so the denominator the retry line prints), and `left()` subtracts `elapsed` from that
    before capping by the deadline's own countdown.
    `elapsed_is_not_charged_against_the_deadline_as_well` pins it, and fails with `0ns` against
    the old arithmetic where it should see half the budget.

19. **The `local-llm` matrix was not run before the first commit**, which is how 16-18's
    neighborhood stayed broken: a useless conversion and a needless borrow failed
    `clippy -D warnings` outright, and the cfg-gated tests in `subagent/mod.rs` still read the
    five `ResolvedAgent` fields decision 2 replaced with accessors. Nothing in the default
    feature set covers any of it. Both feature configurations are now part of the gate.

20. **The deferred load announces itself, and `is_loaded` had to be fixed for it to.** Decision
    16 argued that moving the load was safe *because* the wait is announced, and then did not
    announce it: the only cold-load line in the tree is the subagent launch path's, which fires
    at launch about the primary and never about a candidate a chain reaches mid-turn. The move
    line above it says the chain moved on, which reads as "and the next one is answering now"
    rather than "and it is about to spend four minutes mapping weights".
    `LazyLocalCandidate::completion` now prints the subagent path's wording before awaiting the
    load, because it is the same wait.

    That exposed a pre-existing bug in the predicate it has to ask. `is_loaded` tested map
    membership, but a loader returning `Err` deliberately leaves its `OnceCell` behind, empty,
    so the next caller retries -- the failure semantics `registry.rs`'s own module doc promises.
    Membership therefore reported a *failed* load as warm and swallowed the advisory line before
    exactly the wait it exists to explain. It now tests whether the cell holds a model.
    `a_failed_load_does_not_count_as_loaded` pins both halves.

21. **The docs described the pre-correction behavior.** Decisions 17 and 18 changed what a user
    sees without the prose following: `doc/concepts/llm-providers.md` still said exhausting the
    chain ends the turn, which is now only true when some candidate failed recoverably, and
    described the shared `retry-budget-secs` without saying that the *first selectable
    candidate's* provider supplies it (decision 5) -- so a `0` on the head silently disables
    retries for a fallback that configures 600, and reordering an alias changes which budget
    governs. Both are now stated in the concept page and in the embedded config reference that
    `doc/reference/config.md` symlinks to.

22. **A chain's budget comes from the first candidate that *has* one, not the first candidate.**
    Decision 5 read `retry-budget-secs` off the head's provider, but `style = "mistralrs"` has
    no such key -- an in-process model does no HTTP and so has nothing to retry -- and `None`
    from a local head fell through to the compiled 600-second default. A local-first alias
    therefore handed 600 seconds to its remote fallback while the user's top-level
    `retry-budget-secs = 0` said the opposite, since resolution folds the top-level value into
    remote providers only. Skipping candidates with no opinion keeps the argument decision 5
    actually rests on -- the earliest candidate with a preference decides -- and an all-local
    chain still yields `None`, harmlessly, because nothing in it retries.
    `a_chain_budget_skips_candidates_that_have_none` pins all three cases.

23. **The cold-load line is announced once, by whoever performs the load.** Adding decision 20's
    announcement gave a cold subagent whose *primary* is local two of them: the launch-time
    warning in `subagent/mod.rs`, then the lazy one, for a single initialization. The launch
    warning now fires only for a lone candidate, which is the shape whose load really does
    happen at build time. For a chain it was doubly wrong -- duplicated, and predicting a load
    that may never happen, since a working primary means the fallback is never touched.

24. **A remote candidate's absent `retry-budget-secs` is an answer, not an absence.** Decision 22
    fixed the local-head case by taking the first candidate that yields a budget, which flattened
    two different `None`s into one. A local row has no such key and must be skipped; a *remote*
    row whose value is `None` has inherited the compiled default, and that is a decision -- so it
    must stop the search. Collapsing them let a chain of `remote(None), remote(Some(0))` hand the
    fallback's explicit `0` to the preferred vendor, which is the same class of override the
    decision was written to prevent, pointing the other way. The search now stops at the first
    remote *variant* and flattens one level of the `Option` it carries, and the test covers the
    `None`-then-`Some` orderings as well as the local-skip.

25. **Statements the execution overturned are struck in the body, not just superseded in
    `## Decisions`.** `Exclusions -- Resolved` still told a reader that streaming delegates to
    candidate one and that local candidates load in `build_agent` -- both labeled *Resolved*, both
    rejected by decisions 9 and 16, and both sitting in the part of the file that reads as the
    contract. The acceptance bullet for streaming said the same. A future maintainer reading top
    to bottom could have "restored" the code to behavior this task deliberately abandoned. The
    superseded clauses are now struck through and annotated in place, with the surrounding
    reasoning kept because it still explains why they looked right.

26. **The docs described the selector as it was first written, not as it ended up.** Decision 21
    added prose saying the chain budget comes from the *first selectable candidate's* provider,
    which was true when written and then stopped being true twice -- decision 22 made local rows
    skipped, and decision 24 made the stopping rule the first *remote* variant. Left as it was,
    it pointed a user with a local-first alias at a provider that has no `retry-budget-secs` key
    to set. Both guides now say *first selectable remote candidate* and state that in-process
    rows are skipped, and why.
