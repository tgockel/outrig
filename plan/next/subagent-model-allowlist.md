# An operator cannot constrain which models a subagent may be launched under

## Symptom

None observed. This is a missing policy control, not a live defect.

0101 gave `outrig__subagent` an optional `model` argument naming a `[models.<name>]`, and left the
choice entirely with the launching agent: any model the running build can reach is offered in the
tool schema's `enum` and accepted at launch. That was sequenced deliberately -- the feature is worth
having before the policy schema is designed, and 0101's fork 1 records the omission as ordering
rather than oversight -- but it leaves the operator with no say.

The cost asymmetry runs the wrong way. Delegating *down* is the use case the argument was built for
and needs no guardrail. Delegating *up* is the one nobody asked for: an agent running on a cheap
model can name the expensive one for mechanical work and spend the operator's money with no signal
at the time. Nothing in the session surfaces the choice as a decision -- the launch prints
`(model: <name>)` and proceeds. The only lever today is removing the model from the config
altogether, which also removes it from `outrig run --model`.

## Shape

An `[agents.<name>].subagent-models` allowlist, constraining the argument to a named set and
refusing the rest. Points that follow from how 0101 landed:

- **Absent means unconstrained.** Adding the key later cannot break a config that never had one,
  which is what made deferring it safe in the first place.
- **The `enum` narrows with it.** `usable_model_names` (`crates/outrig-cli/src/subagent/mod.rs`)
  currently returns everything the build can reach; the schema should advertise the *allowed*
  intersection instead, for the same reason it already excludes unreachable models -- a name
  guaranteed to be refused teaches the agent nothing and costs a tool call to learn.
- **The omit-when-singleton rule already covers the degenerate case.** An allowlist naming one model
  drops the `model` property from the schema entirely, which is exactly right: there is no choice
  to offer.
- **The refusal is a third case, not the unknown-model message.** `unusable_model_message` says
  `no usable model named "x"; available: ...`. A model that exists and resolves but is not permitted
  should say so, the way `MistralrsFeatureDisabled` is kept distinct from "unknown" -- the remedy is
  the operator's config, not a corrected guess.
- **Validation belongs with the other agent keys.** Entries naming a model with no `[models.<name>]`
  block should fail config validation, beside `subagent-depth-max` / `subagent-width-max`'s range
  checks in `crates/outrig/src/config/validate.rs`.

## See also

- `plan/done/0101-subagent-model-selection.md` -- fork 1 states the case for the allowlist and why
  it was left out; decisions 1-3 cover the enum and the refusal text this would extend.
- `crates/outrig-cli/src/subagent/mod.rs` -- `usable_model_names`, `resolve_launch_model`, and
  `unusable_model_message`.
- `crates/outrig-cli/src/mcp_self/docs/reference/config.md` -- the `[agents.<name>]` table the key
  would join.
