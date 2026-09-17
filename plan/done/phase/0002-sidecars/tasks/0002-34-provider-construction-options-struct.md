# 0111 -- `LlmProvider` construction now speaks two idioms

`LlmProvider::openai(base_url, api_key, request_timeout_secs)` and
`::anthropic(..)` take their three fields positionally. `retry-budget-secs`
could not join them without breaking every caller, so it arrived as
`with_retry_budget_secs(Option<u64>)` -- a consuming builder step bolted onto a
positional constructor.

That works, and it was the right call for an additive release, but it leaves
two problems:

- Construction is split: three fields one way, the fourth another, with nothing
  in the signature saying why.
- `with_retry_budget_secs` is a no-op on `Mistralrs`, which has no HTTP layer.
  Silently discarding a value is convenient for callers mapping over a
  heterogeneous list, and a trap for anyone who expects it to have taken.

The next optional connection field makes this worse, not better.

## Goal

Give `LlmProvider` one construction idiom instead of two, while the breaking window is open.
Rust cannot overload `openai`, so the reshape is a break whenever it happens; taking it before
0.2.0 final is the difference between one more line in an existing `### Changed` section and
waiting for the next major.

## Deliverables

An options struct, matching the shape 0095 used for the session options:

```rust
let provider = LlmProvider::openai(base_url, api_key, OpenAiOptions {
    request_timeout_secs: Some(600),
    retry_budget_secs: Some(300),
    ..Default::default()
});
```

`#[non_exhaustive]` with a `Default`, so later fields are additive again. The
`Mistralrs` no-op disappears because the options type belongs to the remote
variants only.

This is a breaking change to `openai` / `anthropic` and removes
`with_retry_budget_secs`, so it wants the next breaking window rather than a
point release.

## Acceptance

- `LlmProvider::openai` / `::anthropic` take an options struct; the builder
  method is gone.
- `crates/outrig/public-api.txt` regenerated.
- `crates/outrig/CHANGELOG.md` records it under `### Changed` as breaking, with
  the one-line migration.

## Dependencies

None hard. Queued last on purpose: rc.1 already shipped `with_retry_budget_secs`
as the non-breaking workaround and the variants are `#[non_exhaustive]`, so
downstream cannot construct them anyway. That makes this the one pre-final entry
that is API shape rather than API correctness, and the first to cut if the
window tightens. 0106 also touches `request-timeout-secs`; if both land, this one
moves the field into the options struct rather than reasoning about it twice.

## Decisions

1. **The options structs carry `new` and `with_*` setters, not just `pub` fields.** The first
   draft shipped bare fields plus a derived `Default`, which typechecks but leaves the type
   unusable in the shape the deliverable sketched: `#[non_exhaustive]` forbids struct-literal
   construction across a crate boundary, so `OpenAiOptions { request_timeout_secs: Some(600),
   ..Default::default() }` -- the example in this file's own Deliverables -- does not compile
   downstream. What is left without setters is
   `let mut o = OpenAiOptions::default(); o.request_timeout_secs = Some(600);`, and the three
   in-tree call sites that took that shape are the evidence for how it reads. `ExecOptions`
   (0107) and `ContainerCreateOptions` (0095, Decisions 2 and 11) both answer this the same
   way, so this is the settled convention rather than a new one.

2. **Clippy does not catch the field-assignment shape, contrary to the working assumption.**
   `field_reassign_with_default` was expected to fail the `-D warnings` gate on those three
   call sites and force the setters. It does not fire: the lint's whole suggestion is the
   `..Default::default()` form, which is exactly what `#[non_exhaustive]` makes unavailable
   here, so it declines to lint. Verified by reintroducing the pattern and re-running
   `cargo clippy --all-targets`, which was clean. Recorded because the absence generalizes --
   no lint guards *any* `#[non_exhaustive]` options struct in this crate against growing a
   field with no way to set it, so the convention has to be held by review.

3. **The setters take `u64`, not `Option<u64>`.** The removed `with_retry_budget_secs` took an
   `Option` because it was the only way to spell "unset" on a builder bolted onto a
   constructor. With an options struct, "unset" is spelled by not calling the setter, and
   `ExecOptions::with_workdir` already takes its value bare. 0095 Decision 9 chose the `Option`
   form for `with_transcript` on the grounds that every producer already held one; the opposite
   is true here -- the CLI reads these values *off* the enum variant (`llm.rs`), it never feeds
   the constructor, so every caller has a literal.

4. **Two structs rather than one shared `RemoteOptions`, even though the fields are currently
   identical.** Collapsing them would reintroduce the defect this task exists to remove, one
   level up: a field meaningful to one provider and not the other would have to be ignored in
   silence on one path. Worth stating plainly that this is *weaker* than the precedent it
   leans on. 0095 Decision 3 rejected the same sharing between `start_named` and
   `create_initialized` where the field sets already differed -- `podman run` accepts none of
   `env`, `intercept_dns`, `args` -- so a shared struct would have ignored real fields on a
   real path, present tense. Here the divergence is prospective, and the price is paid now: the
   two types are identical for 31 lines including every doc comment, nothing fails if an edit
   lands on one and not the other, and `plan/next/duration-keys-humantime.md` already has to
   list both. The trade is accepted, but "the duplication is four lines" -- the first draft of
   this entry -- was off by most of an order of magnitude, and a future reader deciding whether
   to merge them should weigh the real number.

5. **The options types shadow the variant fields rather than becoming the variant payload.**
   The one-place form is `OpenAi { base_url, api_key, #[serde(flatten)] options }`, which is
   what `ContainerCreateOptions` does -- the struct is the payload, not a mirror of it. Serde
   forbids it here: `LlmProvider` carries `deny_unknown_fields`, which serde does not support in
   combination with `flatten`, so the payload form would buy deduplication by giving up typo
   rejection on `[providers.<name>]` tables. That trade is clearly wrong -- a silently accepted
   misspelled key in a provider table is precisely the failure mode `deny_unknown_fields` is
   there for. The cost of the shadow is that each future connection setting is a two-place
   addition plus a copy line, and that the field docs live on the constructor type while the
   fields everything actually reads are the variant's.

6. **The `Mistralrs` no-op disappears by construction, not by a check.** The old builder matched
   on the enum and fell through on `Mistralrs`; nothing replaces that arm, because the options
   types are parameters to the two remote constructors and `Mistralrs` is a unit variant with
   no constructor to pass them to. A caller mapping over a heterogeneous provider list loses the
   convenience of setting a budget uniformly and gains a compile error where it previously got
   silence, which is the trade the task asked for.

   The deleted test carried a comment claiming the TOML path could not express the same thing --
   "`deny_unknown_fields` on the tagged enum rejects `retry-budget-secs` under
   `style = "mistralrs"`". That is false, and was worth measuring rather than inheriting:
   `retry-budget-secs`, `request-timeout-secs`, `base-url`, and an outright typo are all
   accepted and discarded on a mistralrs provider, while the same typo on an `openai` provider
   is a parse error. `deny_unknown_fields` has no field set to check a *unit* variant against.
   So this task did not eliminate the silent discard, it narrowed it to one path -- and made
   that path the only one. No new test was written: `config_merge.rs`'s
   `mistralrs_provider_ignores_request_timeout_secs` already pins the swallow deliberately, and
   `plan/next/mistralrs-provider-swallows-keys.md` already proposes the fix. That entry now
   records the corrected claim and the fact that 0111 left it holding the whole hazard.

7. **`public-api.txt` took this task's eight lines and left the `std::io` renderings alone.**
   Regenerating with the pinned cargo-public-api 0.52.0 also rewrites seven `std::io::error`
   paths to `core::io::error`, which is toolchain drift rather than a surface change -- 0107
   Decision 13 already declined it, and taking it here would bury this task's diff. The eight
   new lines were merged in by hand and the result diffed against a full regeneration to
   confirm nothing else moved.

8. **The CHANGELOG migration line names the setters rather than the fields.** The first draft
   said to "assign either public option field", which is what an in-crate caller does and what
   a downstream one cannot reach through a struct literal. It now gives `new()` as the
   no-override spelling and `with_*` as the override, and says outright that the setters rather
   than the fields are the downstream construction path.

9. **The test rewritten around the setters is still the only thing pinning this surface.** No
   config path calls these constructors -- serde populates the variant fields directly -- so
   `remote_provider_options_populate_remote_variants` in `tests/config_provider_enum.rs` carries
   the whole acceptance criterion. It asserts that each setter reaches its own field without
   disturbing the other, that `new` agrees with `default` on both types, and that
   `with_retry_budget_secs(0)` produces `Some(0)` rather than an absent value, since `0` means
   retries off and collapsing it to `None` would silently restore the default budget. An earlier
   draft also asserted each field of a fresh `new()` was `None`, which the review pass correctly
   read as pinning the `Default` derive rather than this surface: with `new == default` asserted
   above it, those four lines followed from the language.

10. **`new()` is the one in-tree spelling for "no overrides", `default()` is not.** Four of the
    six new call sites were written as `::default()` while the struct docs and the CHANGELOG
    migration note both name `new()` -- the convention slipping inside the very commit that
    records it as review-enforced, which Decision 2 says is the only enforcement there is. All
    six now say `new()`. `ExecOptions` is `::new()` at every in-tree call site, so this is the
    existing convention rather than a preference invented here.

11. **`Eq` was dropped from the derives; `PartialEq` stays.** `PartialEq` is exercised by the
    `new == default` assertions. `Eq` was not used by anything, and on a type whose whole
    purpose is that fields can be added freely it is a self-imposed constraint: the first field
    whose type is not `Eq` would make removing the derive a breaking change. Neither
    `ExecOptions` nor `ContainerCreateOptions` derives either. Both derives are invisible to
    `public-api.txt`, which omits auto-derived impls, so this cost nothing in the snapshot.
