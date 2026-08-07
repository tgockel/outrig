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

- **Streaming.** `FailoverModel::stream` delegates to the first candidate. Unifying
  `StreamingResponse` costs more and buys nothing here: outrig's remote turns are non-streaming,
  and the streaming arm is mistralrs-only, which is the one style with no endpoint to fail over
  from. Matches `RetryingModel::stream` (retry.rs:262-270) and the precedent in
  `plan/next/streaming-path-has-no-http-retry.md`.
- **Mistralrs candidates** stay permitted in a chain -- 0110's Design fork §6 allows an alias to
  name one -- but a cold multi-gigabyte load happens in `build_agent`, before the first turn, not
  on a move. Confirm against 0101's decision 7, which added a one-line announcement before a cold
  in-process load for exactly this surprise.
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
- `FailoverModel::stream` reaches only the first candidate.
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
