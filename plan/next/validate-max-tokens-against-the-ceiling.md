# An over-ceiling `max-tokens` is caught at turn time, not at config load

## Problem

`crates/outrig/src/config/validate.rs` has no `max_tokens` rule at all -- not a range check, not
a nonzero check. A `[models.<name>].max-tokens` or `[agents.<name>].max-tokens` above what the
model can serve is accepted, and what happens next depends on the provider:

- **Anthropic style, recognized identifier**: silently lowered to the published ceiling
  (see `plan/next/clamped-ceiling-is-silent.md`).
- **Anthropic style, unrecognized identifier**: sent whole and refused by the API, ending the
  turn.
- **OpenAI style**: sent whole. A gateway fronting Claude refuses it -- which is how the
  subagent-ceiling bug was found -- and outrig has no ceiling to compare against, because the
  tier-2 table is rig's and reached only through the Anthropic client.

Only the first is benign. The other two fail at turn time, after containers are up and a prompt
has been typed, for a mistake that was visible in the file.

## Shape, and why it is not obvious

The check the symptom argues for -- "reject `max-tokens` above the model's ceiling at load" --
cannot cover the case that motivated it. Validation runs in the `outrig` crate against `Config`,
and the ceiling table lives in rig, reached by constructing a completion model against a
*provider*, which validation does not do and should not start doing. Worse, the failing config
was `style = "openai"` with `identifier = "azure/anthropic/claude-haiku-4-5"`: nothing in outrig
knows a ceiling for that string, and inventing a rule that pattern-matches `claude-` inside an
arbitrary gateway identifier is guessing dressed as validation.

So the honest options, in ascending order of cost:

- **A nonzero / sanity range check only.** `max-tokens = 0` is accepted today and is never
  meaningful. Cheap, obviously correct, and catches approximately none of the real cases.
- **A ceiling table outrig owns**, consulted at validation and independent of provider style, with
  rig's remaining the authority at request time. Covers the gateway case. Costs a table that goes
  stale, which is exactly the objection `doc/reference/config.md` already records against tier 2
  ("an identifier newer than that release lands in tier 3 even though it is current") -- now with
  outrig owning the staleness instead of rig.
- **Nothing, and lean on the clamp plus a clear runtime error.** Defensible. `crates/outrig-cli/src/error.rs`
  already rewrites rig's missing-`max_tokens` error into config language; the over-ceiling refusal
  could get the same treatment, turning a provider 400 into "reduce `[models.haiku].max-tokens`".

The third is probably the best value and is not what the entry's title suggests, which is why this
is filed as a question rather than a task.

## See also

- `crates/outrig/src/config/validate.rs` -- the `*_CEILING` range checks for `tool-result-max`,
  `subagent-depth-max`, `subagent-width-max`, `retry-budget-secs`, which are the pattern a range
  rule would join.
- `crates/outrig-cli/src/error.rs` -- `RIG_MISSING_MAX_TOKENS` and the rewrite around it, the
  precedent for the third option.
- `doc/reference/config.md` -- the three-tier section, including the cap.
