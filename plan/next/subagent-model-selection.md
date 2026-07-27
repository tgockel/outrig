# Launching a subagent under a different model

## Context

A subagent today is the parent's agent loop with one field replaced. `SubagentContext`
(`crates/outrig-cli/src/subagent/mod.rs`) clones the session's `ResolvedAgent` wholesale, and
`build_subagent_agent` overwrites only `preamble` before handing it to `llm::build_agent`. Model,
provider, sampling, and every limit come along unchanged -- which `doc/concepts/subagents.md`
states outright, listing "The model, provider, and limits" in its *Inherited* column.

That uniformity is the thing worth relaxing. Fan-out is most useful when the pieces are not all
worth the same money: a parent on an expensive reasoning model wants to farm mechanical work --
grep this tree, summarize these logs, check whether the tests still name that symbol -- out to a
cheap fast one, and keep the expensive context for synthesis. Today the only way to get a cheap
subagent is to run the whole session cheap.

The resolution machinery for this already exists and is already exercised. `outrig run --model
<name>` overrides the agent's configured model for one invocation, and
`llm::resolve_agent_with_overrides(cfg, agent_name, model_override, device_override)` is the
function that does it. This feature points that same override at a subagent launch instead of at
the session.

Nothing about the trust boundary moves. A model is a host-side API endpoint or a host-side set of
weights; picking a different one starts no container, connects no MCP server, and grants no tool
the operator did not already grant the session. The
[MCP trust model](../../doc/concepts/mcp-trust-model.md) invariant is untouched. What *does* move
is spend, which is why the restriction fork below exists.

## Goal

Let a launching agent name the model its subagent runs under, defaulting to the parent's model
when it does not, so cheap work can be delegated to a cheap model without changing the session.

## Deliverables

- An optional `model` argument on `outrig__subagent`, naming a `[models.<name>]` block. Omitted
  means "the model I am running under", so every existing call site keeps its behavior.
- Enough of the config carried on `SubagentContext` to re-resolve an agent at launch time, rather
  than only the already-resolved `ResolvedAgent` it holds today.
- Re-resolution at launch that preserves the session's CLI overrides (see the ordering hazard
  under Risks) and replaces only the model-derived fields.
- Model names discoverable from the tool schema: the launching agent cannot guess names it was
  never shown (see fork 2).
- An unknown-model failure that names the configured models, refuses the launch, and leaves no
  half-registered entry in the registry. Note this is **new** error text, not a reuse:
  `LlmResolveError::UnknownModel` carries only `name` and renders `model "x" is not defined under
  [models.<name>]`. Only `UnknownAgent` carries a `known` list today. Either add a `known` field to
  `UnknownModel` -- which also changes the CLI-level message, so its existing coverage moves with
  it -- or compose the enumeration at the tool boundary and leave the resolver error alone.
- Model attribution in the places a subagent is already visible: the `[outrig]   [<name>]` stderr
  traces, and a header line in `logs/subagent-<name>.log`. The transcript has **no** header today
  -- `Transcript` writes only `=== prompt ===`, `--- reply ---`, and `--- <kind> ---` blocks -- so
  this is a new write, not an amended one. It opens in append mode, so the line lands once per
  open rather than once per file.
- Documentation: amend the *Inherited* table in `doc/concepts/subagents.md`, which currently
  promises the opposite, and describe the argument. That path is a **symlink** to
  `crates/outrig-cli/src/mcp_self/docs/concepts/subagents.md`, which is `include_str!`'d into the
  `outrig mcp self` bundle -- one file, edited under `crates/outrig-cli/src/`. There is no second
  copy to reconcile and no way for the two to disagree.

## Tool surface

```
outrig__subagent({
  "name":   "audit-config",
  "prompt": "...",
  "model":  "fast"      // optional; omitted inherits the launching agent's model
})
```

The argument is a **model name** -- a key under `[models.<name>]` -- not a provider, not a wire
identifier like `gpt-4o-mini`, and not an `[agents.<name>]`. That is the layer the config already
asks users to think in, and the layer `--model` already accepts.

Names are user-chosen and frequently *already* intent-shaped: `fast` and `smart` are the names
the docs use throughout, and the model layer exists precisely so an agent can say `fast` and the
operator can repoint it at a new API identifier without touching anything else. A model argument
therefore gets intent-naming for free where users want it, without OutRig inventing a second
vocabulary of tiers to maintain and map. What it does *not* get for free is discoverability: an
agent that is never shown the names cannot use them, which is fork 2.

## Runtime Behavior

`SubagentRegistry::launch` grows a model parameter. When it is `None` the context's existing
`ResolvedAgent` is cloned exactly as it is today and the path is byte-for-byte what it was. When
it is `Some(name)`, the launch re-resolves the *launching agent* against that model, then applies
the session's CLI overrides on top before building.

Re-resolution is a config lookup and produces the model-derived fields: `model_name`,
`model_identifier`, `provider_name`, `provider`, and `model_weights`. Everything else on
`ResolvedAgent` continues to come from the parent's context, unchanged:

| Re-resolved from the named model | Still inherited from the launching agent     |
|----------------------------------|----------------------------------------------|
| `model_name`                     | `agent_name` (the parent's, unchanged)       |
| `model_identifier`               | `preamble` (composed from the parent's text) |
| `provider_name`, `provider`      | `temperature`, `max_tokens`                  |
| `model_weights`                  | `tool_call_max`, `tool_result_max_bytes`     |
| --                               | `max_subagent_depth`, `image`                |

The split is exhaustive over `ResolvedAgent`'s fields on purpose -- it is what enforces the
CLI-override invariant under Risks, so a field missing from it is a field nobody decided about.
`agent_name` in particular has to stay the parent's: `build_subagent_agent` passes it to
`SetResultTool` for the trace prefix, and a re-resolution would not change it anyway.

Sampling and limits staying with the agent is deliberate and is the resolved answer to "should
this really be an *agent* argument": it should not. Switching the whole agent would drag in a
second preamble that collides with the parent-supplied one and force `compose_preamble` to
arbitrate between three sources. The model is the axis with a real use case; the rest is not.

A subagent may select any configured model, including one on a different provider and a different
provider *style*. `RigAgent` is already a runtime-dispatched enum over the OpenAi and Mistralrs
arms, so a cheap local `mistralrs` subagent under an expensive hosted parent needs no new
dispatch -- it falls out of the existing shape. The `LlmRegistry` is shared through
`SubagentContext` and keyed by model name, so two subagents naming the same local model share one
loaded engine rather than loading the weights twice.

The `mistralrs` half of that is gated: `RigAgent::Mistralrs` and `SubagentContext.registry` are
both `#[cfg(feature = "local-llm")]`, and without the feature `ResolvedProvider::Mistralrs`
resolves to a `MistralrsFeatureDisabled` error. So a subagent naming a local model in a default
build fails, but with that error rather than "unknown model" -- correct, and worth stating so it
does not read as a bug. The re-resolution plumbing has to stay `cfg`-clean for the same reason:
the shared registry it leans on only exists in one of the two builds.

An unknown model name fails the launch and reaches the model as a tool error naming the
configured models, in the same shape `LlmResolveError::UnknownModel` already produces. The failure
happens before any registry entry is inserted, so a bad name costs a tool call and nothing else --
no half-registered handle, and the name stays free for a corrected retry.

Depth, release, and shutdown are untouched. A subagent on another model is an ordinary subagent:
it counts against `subagent-max-depth` identically, launches its own children under the parent's
default model unless they too name one, and is reaped by the same tree walk.

## Acceptance

- `outrig__subagent` with no `model` produces exactly today's behavior -- same model, same
  provider, same limits.
- `outrig__subagent` with `model` naming a configured model runs that subagent against it, while
  the parent's own turns continue on the parent's model.
- A subagent can name a model on a different provider, and on a different provider *style*
  (hosted OpenAi parent, in-process `mistralrs` subagent) without new dispatch. Testable only in a
  `local-llm` build; the default build's criterion is that the same launch fails with
  `MistralrsFeatureDisabled` rather than an unknown-model error.
- Sampling and limits (`temperature`, `max-tokens`, `tool-call-max`, `tool-result-max`) come from
  the launching agent regardless of the model named.
- Session CLI overrides (`--max-tool-calls`, `--max-tool-result-bytes`) still apply to a subagent
  launched with an explicit model -- the regression named under Risks.
- An unknown model name fails the launch with a message naming the configured models, registers
  nothing, and leaves the handle free for a retry.
- Two subagents naming the same in-process model share one loaded engine.
- The stderr traces and `logs/subagent-<name>.log` for a subagent identify which model it ran
  under.
- `doc/concepts/subagents.md` no longer claims the model is inherited.

## Design forks

Each item leads with its status: **Resolved** (committed here), **Recommended** (a lean that a
prototype should confirm), or **Open** (deferred).

1. **Who chooses -- Resolved: the model chooses, unrestricted in the first pass.** The launching
   agent names any configured model. An operator-declared allowlist -- something like
   `[agents.<name>].subagent-models`, constraining the argument to a named set and refusing the
   rest -- is the right eventual shape, because the cost asymmetry runs the wrong way: an agent
   that escalates itself onto the expensive model for mechanical work spends the operator's money
   with no signal at the time. It is deliberately **not** in the first pass, since the feature is
   worth having before the policy schema is designed, and adding a constraint later cannot break a
   config that never had one. Note it here so the omission reads as sequencing rather than
   oversight.

2. **Discoverability -- Recommended: enumerate the configured model names in the tool schema.**
   An agent that is not shown the names will guess wire identifiers (`gpt-4o-mini`) or invent
   plausible ones, and every guess costs a tool call to fail. Listing the names in the `model`
   property's `description` is the cheap version; a JSON-Schema `enum` is the strict version, which
   also makes the argument checkable before the call is generated. The strict version interacts
   with fork 1: once an allowlist exists, the enum should list the *allowed* set, not every
   configured model. Prefer the enum if the provider bridges in play honor it, since `enum` is one
   of the few constraints models reliably attend to -- the same reasoning that shaped
   `set_result`'s `status`.

3. **Where the model is named -- Resolved: an argument on `outrig__subagent`.** Not a separate
   `outrig__subagent_with_model` tool, which would duplicate a three-field schema (`name`,
   `preamble`, `prompt`) for one optional field and add a sixth entry to a five-tool parent
   surface that already occupies context. Not a mode on
   `outrig__subagent_send` either: a subagent's model is fixed for its lifetime, and re-pointing a
   live one mid-round would invalidate the history it has accumulated under the old model.

4. **Model *and* device -- Open.** `resolve_agent_with_overrides` also takes a device override,
   and `outrig run --device` uses it. Whether a subagent may name a device (`cuda:1` for the
   subagent, `cpu` for the parent) is a real question for multi-GPU hosts, but it is a narrow
   audience and easy to add later behind the same argument shape. Out of scope here.

5. **Per-model sampling defaults -- Open.** Sampling lives on the agent, so a subagent on a
   different model inherits a `temperature` tuned for the parent's. That is usually harmless and
   occasionally wrong. If it becomes a real problem the fix is per-model sampling defaults in
   config, which is a config-schema change independent of this feature.

## Risks

- **CLI overrides are applied after the context is captured.** In `cli/run.rs`, `run_inner`
  resolves the agent, then mutates it with `apply_tool_call_max_override` and
  `apply_tool_result_max_override`, and only then clones the result into `SubagentContext`. A
  naive re-resolve at launch time calls `resolve_agent_with_overrides` again and gets a
  `ResolvedAgent` that never saw those flags, silently dropping the session's
  `--max-tool-calls` / `--max-tool-result-bytes` for exactly the subagents that name a model.
  Carry the limits forward from the context rather than from the fresh resolution -- the field
  split in the table above is what enforces this -- and cover it with the acceptance test named
  for it.
- **Cold in-process model loads block the parent's tool call.** `SubagentTool::call` awaits
  `launch`, which awaits `build_agent`, which awaits `registry.get_or_init` and therefore
  `mistralrs::load`. A subagent that names a local model the session has never loaded stalls the
  parent's launch call for the whole multi-gigabyte load -- minutes on first use, and there is no
  progress UI on that path. Today this cannot happen, because the parent's model is by definition
  already loaded. Decide whether the launch reports the load or accepts the stall; either way it
  should not read as a hang. Kin to `keepid-first-run-layer-remap-cost.md`.
- **`Config` lifetime on the context.** `SubagentContext` is `Clone` and holds owned data.
  Re-resolution needs the merged `Config`, which the registry does not have today. An `Arc<Config>`
  on the context is the obvious shape; the alternative -- pre-resolving every configured model at
  session start -- does needless work for models no subagent ever names, and would move in-process
  weight loading to startup.
- **Fan-out times model choice is a spend multiplier.** Breadth is unbounded today (see
  `subagent-width-cap.md`), and model choice makes the worst case more expensive rather than merely
  slower. The two features are independent but compose badly; the width cap is the containment
  half.
- **Per-model failure behavior is not uniform.** In-flight work adds a repeat-failure breaker that
  ends a subagent round after N consecutive identical tool failures, and stop reasons that a round
  reports to its parent. Those thresholds were tuned against the parent's model; a cheaper or
  smaller model is likelier to loop on a tool error, so a subagent deliberately placed on one will
  hit the breaker more often. That is the breaker working, not a bug -- but the stop reason
  reaching the parent should make the model legible, or a parent will read "stopped early" without
  knowing it delegated to something less capable.

## Dependencies

- **Soft: `subagent-width-cap.md`.** Independent, but the containment half of the same concern.
  Either may land first.
- **Soft: the `--model` override path.** `resolve_agent_with_overrides` and its
  `llm_resolve.rs` coverage are the machinery reused here.
- **Soft: `anthropic-native-api.md`.** That entry adds a third provider style and a third
  `RigAgent` variant, and already lists auditing the provider matches in `subagent/mod.rs` among
  its deliverables. The two compose without conflict -- a third arm on a runtime-dispatched enum
  is still no new dispatch for this feature -- but whichever lands second inherits the other's
  match arms. Landing the provider work first makes the payoff here larger, since a cheap-model
  subagent under an expensive parent is most compelling across providers.

## See also

- `doc/concepts/subagents.md` -- the *Inherited* table this feature amends.
- `doc/concepts/llm-providers.md` -- the provider / model / agent layering the argument names.
- `doc/concepts/in-process-llm.md` -- engine-per-model-name lifecycle and first-use download
  stalls, both of which a model-selecting subagent can now trigger.
- `plan/next/subagent-width-cap.md` -- bounding how many subagents run at once.
- `plan/next/anthropic-native-api.md` -- the third provider style this feature would let a
  subagent select independently of its parent.
