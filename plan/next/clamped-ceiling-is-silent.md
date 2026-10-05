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

The clamped value never leaves `build_agent`, so a truncated subagent report quotes the configured
number instead of the one in effect. That half is a bug, filed as #258. Its fix and this
warning want the same thing: **the effective ceiling resolved once and stored where everyone reads
it**, rather than computed at the moment of client construction and discarded.

## Shape

Follow `warn_fallback_ceiling` rather than inventing a second convention: one stderr line, naming
the identifier, the configured value, and the ceiling it was lowered to.

The question of once per process versus once per model is settled by that precedent.
`warn_fallback_ceiling` now keys a `Mutex<BTreeSet<String>>` on the model name, so a parent at
128000 and a subagent clamped to 64000 each get their line. What is still open is whether a
warning that fires on a *correct* config earns its place at all.

Worth pairing with `plan/next/validate-max-tokens-against-the-ceiling.md`: if validation rejects
the over-ceiling config at load, the only configs that reach the clamp are ones validation could
not judge, which narrows what the warning is for and may shrink it to nothing.

## See also

- `crates/outrig-cli/src/llm.rs` -- the clamp arm in `build_agent`, and `warn_fallback_ceiling`
  directly below it, whose doc comment is the argument this would extend.
- `crates/outrig-cli/tests/anthropic_mock.rs` --
  `a_configured_ceiling_is_capped_at_the_published_one` pins the clamp itself; a warning wants
  stderr capture, which that harness does not do today.

## The library's half of the export is settled

`0003-04` exported the effective ceiling from the library's copy of `build_agent`, and `0003-15`
made it per candidate: each failover candidate's `Budget` carries the ceiling that reaches the
wire for it, the chain rewrites each request from it, and every `model.call` manifest records the
`max_tokens` its call carried. The warning when the clamp fires is still open in both loops, and
the CLI's `SetResultTool` still reads the unclamped value.
