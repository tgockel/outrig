# Lowering a configured `max-tokens` to the published ceiling says nothing

## Symptom

None observed, and that is the point. `build_agent`'s Anthropic arm lowers a tier-1 `max-tokens`
above the identifier's published ceiling to that ceiling, silently. The turn runs, so nothing
draws attention to the fact that the number the operator wrote is not the number in effect.

The neighboring case already decided this the other way. `warn_fallback_ceiling`
(`crates/outrig-cli/src/llm.rs`) exists because "a reply cut off at a ceiling nobody asked for
looks like a bad model rather than a config gap". A lowered ceiling has the same failure mode with
a smaller gap: replies stop shorter than the config says they may, and the config file is the last
place anyone looks because it plainly reads `128000`.

It was left out of the fix deliberately -- the fix's job was to make the launch work at all, and a
warning that fires on a *correct* config is a real cost. That is the question this entry is for,
not whether the clamp itself is right.

## The same gap, one layer over

The clamped value never leaves `build_agent`. It is a local handed to `finish_agent`, so nothing
else in the process can see the ceiling that actually applies -- and one caller needs to.
`build_subagent_agent` (`crates/outrig-cli/src/subagent/mod.rs`) constructs `SetResultTool` from
the *unclamped* `resolved.max_tokens` before it calls `build_agent`, so a subagent whose report is
truncated prints "the max-tokens in effect for agent X is 128000 -- raise it" when 64000 was in
effect and raising it cannot work.

That is the same defect the subagent ceiling fix removed one layer down, where the parent's number
was being reported for a subagent that never used it -- so the argument for fixing it is already
made and recorded in the comment at that `SetResultTool` construction. It is narrow today
(anthropic-style provider, recognized identifier, over-ceiling config, truncated report), which is
why it was left rather than fixed under a "just the fix" scope.

Both halves want the same thing: **the effective ceiling resolved once and stored where everyone
reads it**, rather than computed at the moment of client construction and discarded. A field on
`ResolvedAgent` is the obvious home; the obstacle is that the published ceiling is only knowable
after building a rig completion model, which is `build_agent`'s job and not the resolver's. Worth
solving properly rather than by passing the number down a second path.

## Shape

Follow `warn_fallback_ceiling` rather than inventing a second convention: `std::sync::Once`, one
stderr line, naming the identifier, the configured value, and the ceiling it was lowered to.

The one real design question is **once per process or once per (model, ceiling) pair**. The
fallback warning is once per process because it fires on a session-wide gap and a subagent fan-out
repeating it would bury the traces around it. A clamp is per-model, and the interesting case is
precisely a *second* model -- a parent at 128000 and a subagent clamped to 64000 are two different
facts, and once-per-process reports only the first. A small keyed set (`Mutex<HashSet<String>>`
or a `OnceLock` of one) keyed on the model name is probably right, but it is a new pattern in this
file and should be argued rather than assumed.

Worth pairing with `plan/next/validate-max-tokens-against-the-ceiling.md`: if validation rejects
the over-ceiling config at load, the only configs that reach the clamp are ones validation could
not judge, which narrows what the warning is for and may shrink it to nothing.

## See also

- `crates/outrig-cli/src/llm.rs` -- the clamp arm in `build_agent`, and `warn_fallback_ceiling`
  directly below it, whose doc comment is the argument this would extend.
- `crates/outrig-cli/tests/anthropic_mock.rs` -- `a_configured_ceiling_is_capped_at_the_published_one`
  pins the clamp itself; a warning wants stderr capture, which that harness does not do today.
