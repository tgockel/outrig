# 0002-46 -- Decide what the deprecated local-LLM surface does in 0.2.0

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
   - **Declaring-file provenance.** Consistent with what 0002-20 did for images and mounts, and with
     what a global `[models.<n>]` would want. But it gives `Model` a `ConfigSource`, which
     immediately inherits 0002-42's stale-public-field problem -- `Model`'s path fields are public
     -- so it must adopt 0002-42's chosen shape rather than inventing a second one. Coordinate with
     0002-42 fork 3 or do not take this branch.
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

None hard. Must precede 0002-49, which writes the 0.1 -> 0.2 migration guide and has to state this
behavior, and 0002-52, which cuts the release that ships it.

## See also

- `plan/next/remove-deprecated-local-llm.md` -- the removal, still post-0.2.0. This task settles
  its open question; the deletion work stays there.
- `plan/next/mistralrs-provider-swallows-keys.md`, `plan/next/model-path-runtime-unjoined.md` --
  the two defects, with the analysis this task does not repeat. **Both were executed here and
  deleted**; read them at the commit before this one if that analysis is wanted.
- `plan/next/ci-configuration-coverage.md` -- its `macos-latest` x `local-llm,metal` item is
  contingent on this decision, and survives it.

## Decisions

1. **`style = "mistralrs"` stays operational in 0.2.0, and both defects are fixed -- fork 1, taken
   as recommended.** The deprecation has not shipped in a stable release, so no user has had a
   version in which to see the warning; 0.2.0 is the release that *carries* it. Both defects are
   therefore shipping defects rather than blemishes on something already on its way out. The
   parse-error branch was not taken and left no work behind: the removal stays in
   `plan/next/remove-deprecated-local-llm.md`, and its own open question is now *unblocked* rather
   than answered -- by the time it runs there will be a released version carrying the warning, so
   parse-error versus fail-at-resolve becomes a straight API-cleanup-against-soft-landing trade
   rather than a question of whether the deprecation counted.

2. **A relative `model-path` resolves against the repo root, not the declaring file -- fork 2, and
   it closes 0002-42's fork 3 by declining it.** Giving `Model` a `ConfigSource` was the
   consistent answer and was rejected on three counts. It would force `Model` to adopt 0002-42's
   shape -- `model_path` private behind a getter and a source-clearing setter -- which by that
   task's own Decision 2 means taking all ten fields, on a surface scheduled for deletion.
   `Model::source()` already exists and means provider-versus-alias, so `source` would name two
   unrelated things on one type. And the repo root is what `validate_mistralrs_model` has always
   checked and what the reference has always documented, so it closes the split without moving any
   existing user's path. The cost is stated in the method's own doc comment and on both affected
   pages: a global `[models.<n>]` with a relative `model-path` follows whichever repo is current.
   `Config::stamp_source` still skips `models`; its comment now says why that is the end state
   rather than a deferral.

3. **The variant is `Mistralrs {}` *without* `#[non_exhaustive]`, contrary to the buffer entry's
   sketch.** `LlmProvider::Mistralrs` is constructed from outside the crate that defines it --
   `config_init.rs` and `subagent/mod.rs`'s fixtures in `outrig-cli`, and `config_merge.rs`'s
   `assert_eq!` in `outrig`'s own integration tests, which are separate crates. `#[non_exhaustive]`
   on a struct variant forbids exactly that, so it would turn a one-token migration into
   "unconstructible, add a `LlmProvider::mistralrs()`" -- new published surface on a surface being
   deleted. Non-exhaustiveness exists to allow adding fields later, and this variant will never
   gain one.

4. **serde's handling of an empty braced variant was read, not assumed.** The entry said to check,
   and the check is worth recording. In `serde_derive` 1.0.229, `internals/ast.rs` maps
   `syn::Fields::Named(_)` to `Style::Struct` regardless of emptiness, so the variant does not take
   the unit path whose `InternallyTaggedUnitVisitor` ignores everything left in the map; and
   `de/struct_.rs` computes `all_skipped` over an empty field list as vacuously true, which under
   `deny_unknown_fields` generates a `visit_map` that reads `next_key` into an uninhabited
   `__Field`. Any surviving key is an error. The reading only predicted the outcome --
   `mistralrs_provider_rejects_unknown_keys` drives `toml::from_str::<Config>` alongside
   `Config::load_from_str` because only the bare `Deserialize` path shows that the derive is what
   refuses the key.

5. **The join happens in `resolve_candidate`, not in the loader it feeds.** `mistralrs_model` and
   `mistralrs::load` are `#[cfg(feature = "local-llm")]`, so a fix living there would be invisible
   to a default build and unassertable in the default CI row -- the one that runs on every push.
   `resolve_candidate`'s mistralrs arm is ungated and is where the config's text becomes
   `MistralrsWeights`, so the value leaving it is absolute in every build and the regression needs
   no feature to observe it. The cost is `repo_root: &Path` threaded through the three
   `resolve_agent*` entry points and a field on `SubagentContext`. That is not a published break:
   `crates/outrig-cli/public-api.txt` publishes exactly `outrig_cli::run()`, and the module tree is
   reachable only under `internal-test-api`.

6. **The regression proves "not from the repo root" by supplying a root the cwd is not, rather than
   by calling `set_current_dir`.** The only in-tree precedent for moving the process cwd,
   `config_merge.rs`'s `relative_global_config_path_resolves_independently_of_cwd`, notes that cwd
   is process-global and unguarded, and nothing serializes tests against it. After this fix
   resolution never reads the cwd at all, so a tempdir root plus `assert_ne!(current_dir(),
   repo_root)` is both the stronger claim and the safer one: it states the precondition the test
   depends on instead of assuming it.

7. **`llm_resolve.rs` got three same-named private wrappers instead of forty-odd edited call
   sites.** The wrappers shadow the library functions and supply one named constant,
   `UNUSED_REPO_ROOT`, whose name says the thing worth knowing: every other config in that file
   sets no `model-path` or an absolute one, so the base is immaterial to them. The one test the
   base matters to bypasses the wrappers and calls `outrig_cli::llm` directly with a root of its
   own, so the shadowing cannot hide the property under test.

8. **`MistralrsModelPathMissing` still names the path the row declared, not the resolved one.**
   Switching it to the join was tempting -- it would say which file was looked for -- but it
   changes a user-facing diagnostic this task did not ask about, and the base is now stated in both
   docs. Left alone deliberately rather than by oversight.

9. **`crates/outrig/CHANGELOG.md` gained the `Deprecated` entry it never had.** `e9847bb3` recorded
   the deprecation only in the CLI's changelog, but every deprecated *public item* --
   `LlmProvider::Mistralrs`, `MistralrsDeviceSpec`, the six `Model` weight fields,
   `Config::model_cache_root`, nine `ConfigValidationError` variants -- is published by the
   library. Without it, this task's `Changed` entry would cite a deprecation that crate had never
   announced. A small deliberate addition to the task's scope.

10. **Both buffer entries were deleted, not annotated.** `mistralrs-provider-swallows-keys.md` and
    `model-path-runtime-unjoined.md` were executed in full here, so there is nothing left in either
    to close; annotating them would leave finished work looking outstanding, which is the failure
    mode the task named for the branch it did not take. Their analysis survives in git history and
    in these Decisions. `remove-deprecated-local-llm.md` absorbed both dispositions and the one
    surface item this task added (`Model::resolved_model_path`), and
    `ci-configuration-coverage.md` now records that its `macos-latest` x `local-llm,metal` item
    survives 0.2.0 rather than evaporating.

11. **Both fixes were checked against a disabled version of themselves.** With the variant reverted
    to unit form, `mistralrs_provider_rejects_unknown_keys` and the flipped
    `mistralrs_provider_rejects_request_timeout_secs` both fail. With
    `model.resolved_model_path(repo_root)` reverted to `model.model_path.clone()`,
    `mistralrs_relative_model_path_resolves_against_the_repo_root` fails with
    `left: Some("models/local.gguf")` against the tempdir-absolute right. A test that passes either
    way asserts nothing.

## Decisions from the /simplify pass

- **The rationale had been written five times; four were cut.** "Validation and the loader used to
  join independently and agreed only from the repo root" appeared on `Model::resolved_model_path`,
  at `validate_mistralrs_model`'s call site, in `resolve_agent`'s doc, beside the
  `MistralrsWeights` construction, and again in two test doc comments. The canonical statement
  stays on the method; the call-site comment beside `MistralrsWeights` went entirely, since the
  field's own doc already says the value is absolute. Long-form history belongs in this file.

- **`UNUSED_REPO_ROOT` became `ANY_REPO_ROOT`.** The constant is used three times -- it is the
  *value* that is immaterial, and the old name spent five lines of doc explaining that it did not
  mean what it said.

- **`validate_mistralrs_model` keeps `!model.resolved_model_path(root).is_some_and(..exists())`.**
  The proposed simplification reached for `super::resolve_against(root, declared)` directly, which
  is exactly the second base-choosing site this task exists to delete. The alternatives that keep
  the method need a `clone().expect(..)` to recover the declared path for the error, which is
  worse than an `is_some_and` that is total either way.

- **The `config_merge.rs` test stays despite overlapping `config_provider_enum.rs`.** The
  acceptance names it and requires it to flip to asserting rejection. Its doc now cites the
  general proof next door rather than retelling it, and the half only it covers -- a bare provider
  reaching the `(None, None)` validation arm -- is stated as such.

- **Collapsing the three `resolve_agent*` entry points behind a `ResolveOptions` struct: seen,
  declined.** Two of the three exist only for tests, and adding a required parameter to all three
  is what made `llm_resolve.rs` need wrappers to take it back off. But `repo_root` is required
  input, not an option -- `validate_with_options(cfg, repo_root, ValidationOptions { .. })` is the
  repo's own shape for exactly that -- so the parameter's placement is right and only the
  pre-existing entry-point family is redundant. It is out of this task's scope and the whole
  surface is queued for deletion; not filed separately for that reason.

- **One gap was filed rather than fixed: `plan/next/config-init-model-path-has-no-base.md`.**
  `outrig init` stores the answer to `model-path` verbatim and its field description names no
  base, so an answer relative to the user's working directory writes a config that then fails
  validation naming a path that exists. It is the mirror of the defect this task fixed, it is
  pre-existing, and it is on the same deprecated surface.

- **A side effect worth recording: a bare-filename `model-path` no longer hands the GGUF loader an
  empty directory.** `crates/outrig-cli/src/llm/mistralrs.rs` splits the path into `parent()` plus
  `file_name()`, and `Path::new("model.gguf").parent()` is `Some("")`. The join makes it the repo
  root.

- **`cargo public-api` cannot see this task's source break, and the CHANGELOG is the only record.**
  A zero-field struct variant renders identically to a unit variant, so `public-api.txt` line 303
  is unchanged by `Mistralrs` -> `Mistralrs {}`; the snapshot's only movement is the new
  `Model::resolved_model_path` line. Worth knowing for 0002-48, which turns that file into a gate:
  it is a gate with this blind spot in it.

- **`cargo fmt --check` passes on a 105-column line.** `cli/run.rs`'s
  `resolve_agent_with_overrides` call reached 105 columns against the repo's `max_width = 100`,
  and rustfmt accepted it rather than rewriting -- it gives up on that shape. Wrapped by hand to
  match the identical call in `session_setup.rs`.
