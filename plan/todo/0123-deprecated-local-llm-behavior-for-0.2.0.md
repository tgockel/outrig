# 0123 -- Decide what the deprecated local-LLM surface does in 0.2.0

## Context

`e9847bb3` deprecated the `local-llm` feature, the `style = "mistralrs"` provider, and the
in-process backend: warnings only, nothing removed, no config broken.
`plan/next/remove-deprecated-local-llm.md` holds the removal, correctly filed rather than queued
-- deprecating and removing in the same release is not a deprecation, so the removal waits for a
release after this one.

That leaves a question this release cannot avoid: **0.2.0 ships the deprecated provider, and it
ships two proven defects.** The audit's position is that either is acceptable -- fix them, or
document a deliberately narrower compatibility path -- but reshaping or removing the surface
silently at final is not.

The two defects, both verified code paths with their own buffer entries:

- **`style = "mistralrs"` accepts and discards any key.** `LlmProvider` is internally tagged with
  `deny_unknown_fields`, which bites on the two remote variants. `Mistralrs` is a *unit* variant,
  so there is no field set to check against, and `retry-budget-secs`, `request-timeout-secs`,
  `base-url` and outright typos all parse clean and vanish. `doc/reference/config.md` opens by
  promising "Unknown keys are an error"; this is the one place it does not hold. Full analysis in
  `plan/next/mistralrs-provider-swallows-keys.md`.
- **A relative `model-path` is validated against one base and opened against another.**
  `validate_mistralrs_model` (`crates/outrig/src/config/validate.rs`) joins it to the repo root
  and checks existence; `crates/outrig-cli/src/llm.rs` passes `model.model_path.as_deref()`
  through to `llm/mistralrs.rs` verbatim, which opens it relative to the process's cwd. They
  agree only when `outrig` is invoked from the repo root. Both docs describe the validating half.
  Full analysis in `plan/next/model-path-runtime-unjoined.md`.

## Goal

One recorded decision about what `style = "mistralrs"` means in 0.2.0, and whatever work that
decision implies -- executed, or explicitly closed unfixed with the reason.

## The decision

**Does `style = "mistralrs"` keep working in 0.2.0, or become a parse error?** This is the open
question `plan/next/remove-deprecated-local-llm.md` names, pulled forward because the two defects
above have different answers depending on it.

- **Keep it operational -- recommended, and the other two are not equal alternatives.** The
  deprecation has not shipped in a stable release. Users have had no version in which to see the
  warning, so 0.2.0 is the release that *carries* the deprecation, not the one that acts on it.
  Both defects are then shipping defects and both get fixed.
- **Parse error now -- effectively removal, and it contradicts the deferral.** Every committed
  config declaring a mistralrs provider stops loading, including ones whose agents never used it,
  since providers validate independently of whether anything resolves to them. That is the
  operational effect of removing the feature, delivered in the same release that first announces
  the deprecation -- which is exactly what `plan/next/remove-deprecated-local-llm.md` deferred
  removal to avoid. Take it only as a recorded override of the compatibility recommendation,
  with the reason written into this task's `## Decisions`.
- **Keep parsing, fail at resolve.** Today's feature-off behavior, whose rationale is portability
  of a shared checked-in config. This is not a third disposition so much as a variant of the
  first: it says nothing about either defect. Taking it means also saying what an unknown key
  does and what a relative `model-path` does, at which point it *is* the first option with a
  narrower runtime. Choose it only with both answers attached.

Record the answer in this task's `## Decisions`, since `plan/next/remove-deprecated-local-llm.md`
depends on it and currently reads as undecided.

## Deliverables

Conditional on the decision:

- **If the provider stays operational:** give the variant a field set so `deny_unknown_fields`
  bites -- `Mistralrs {}` is the sketch, but verify serde's internally-tagged handling actually
  enforces an empty field set rather than assuming it; the fallback is a custom `Deserialize` or
  a deny-list check beside `reject_repo_network_policy`. Note this is a **source break** for
  downstream matchers: `Mistralrs` is matched as a unit variant in `validate.rs`'s two exhaustive
  matches, `llm.rs`, `config_init.rs`, and several tests, all of which become `Mistralrs { .. }`.
  That is exactly why it belongs in the pre-freeze window. Separately, resolve `model-path` once
  against the base it is validated against and pass the resolved path to the loader; the
  resolution belongs in the library beside the other config path helpers, not in the CLI.
  **The base must be named explicitly** -- see fork 2 below; "resolve it once" is not a design.
- **If it becomes a parse error:** close both buffer entries unfixed, say so in their bodies
  rather than leaving them looking outstanding, and pin the new error message in a test.
- **Either way:** `doc/reference/config.md` (a **symlink** into
  `crates/outrig-cli/src/mcp_self/docs/` -- edit the target) and
  `doc/concepts/in-process-llm.md` state the 0.2.0 behavior and the migration path, and
  `crates/outrig-cli/CHANGELOG.md` says which it is. A user hitting the deprecation warning needs
  to know what the next release will do.

## Design forks

1. **The 0.2.0 disposition -- Recommended: operational.** See The decision above. Recorded here
   as a fork so that choosing otherwise leaves a trace rather than looking like the default.

2. **What base a relative `model-path` resolves against -- Open, and it decides scope.**
   - **Repo root.** The minimal fix: open the same path validation already checks
     (`validate_mistralrs_model` joins to the repo root). No new machinery, no new public
     surface, and the docs already describe this base. Recommended for a deprecated surface.
   - **Declaring-file provenance.** Consistent with what 0097 did for images and mounts, and with
     what a global `[models.<n>]` would want. But it gives `Model` a `ConfigSource`, which
     immediately inherits 0119's stale-public-field problem -- `Model`'s path fields are public
     -- so it must adopt 0119's chosen shape rather than inventing a second one. Coordinate with
     0119 fork 3 or do not take this branch.
   - **Reject relative paths.** Smallest possible change, most disruptive; the only in-tree
     relative `model-path` is `crates/outrig/tests/fixtures/config-full.toml`.

## Acceptance

- A config with `style = "mistralrs"` and an unknown key behaves as the decision says, with a
  test pinning the message either way.
- **The unknown-key behavior is proven at the serde level, not only through the loader.** Drive
  `toml::from_str::<Config>` directly as well as `Config::load_from_str`: a braced-empty variant
  either makes `deny_unknown_fields` bite or it does not, and only the direct path shows which.
  This is the check the sketch says not to assume.
- `style = "mistralrs"` alone behaves as the decision says, and if it still parses it round-trips
  to an equal `Config` -- semantic equality, not byte-identical serializer output.
- `crates/outrig/tests/config_merge.rs`'s `mistralrs_provider_ignores_request_timeout_secs`
  is updated. It was written deliberately against the bug and its doc comment points at the
  buffer entry; under the fix it flips to asserting rejection, under a parse error it goes away.
- If `model-path` is fixed: a regression that runs from a directory that is **not** the repo
  root, since the bug is invisible from the repo root. `crates/outrig-cli/tests/llm_resolve.rs`
  is the closest existing home.

## Dependencies

None hard. Must precede 0126, which writes the 0.1 -> 0.2 migration guide and has to state this
behavior, and 0128, which cuts the release that ships it.

## See also

- `plan/next/remove-deprecated-local-llm.md` -- the removal, still post-0.2.0. This task settles
  its open question; the deletion work stays there.
- `plan/next/mistralrs-provider-swallows-keys.md`, `plan/next/model-path-runtime-unjoined.md` --
  the two defects, with the analysis this task does not repeat.
- `plan/next/ci-configuration-coverage.md` -- its `macos-latest` x `local-llm,metal` item is
  contingent on this decision.
