# 0115 -- Whether a repo declared `[network]` is structural, not a hidden bit

## Context

`NetworkConfig` (`crates/outrig/src/config/mod.rs:1558-1569`) is four public fields plus a
private one:

```rust
pub struct NetworkConfig {
    pub mode: NetworkMode,
    pub default: NetworkAction,
    pub allow: Vec<NetworkEntry>,
    pub deny: Vec<NetworkEntry>,
    #[serde(skip)]
    #[schemars(skip)]
    declared: bool,
}
```

Public `config::merge(global, repo)` decides precedence on that private bit
(`crates/outrig/src/config/merge.rs:56-60`). `is_declared` / `set_declared` are `pub(crate)`
(`config/mod.rs:1595,1599`), and the bit is `#[serde(skip)]`, so no deserialization ever sets it.
`Config::load_from_str` repairs it by scanning the source text before parsing
(`reject_repo_network_policy`, `config/mod.rs:318,634`). Two public construction paths cannot:

- `Config::default()` followed by public field assignment;
- `toml::from_str::<Config>` directly, which is available because `Config` is `Deserialize`.

Both were tested with repo mode `Audit` and both merged to `Default`. `merge` is public API and
its behavior depends on state a public caller has no way to set. `PartialEq` deliberately ignores
the bit (`config/mod.rs:1583-1588`), so two values that merge differently compare equal -- which
is why this is easy to miss.

## The asymmetry this must not flatten

The merge is deliberately *not* "repo block wins". A repo may choose the network **mode**;
`default`, `allow`, and `deny` are operator-owned and stay global. `reject_repo_network_policy`
is the enforcement, and it runs on raw text before parsing precisely because public `merge`
returns no error and so cannot reject anything itself.

That makes this a trust boundary, not a formatting workaround. A naive `Option<NetworkConfig>`
that replaces the whole block when the repo declared one would either discard the operator's
rules or let a repo inject its own -- and a caller constructing a `Config` programmatically
bypasses the text scan entirely, so the boundary would exist only for configs that came from
files. The three obvious merge tests do not catch either failure.

This is an API-freeze item: whichever shape is chosen, changing it later is a break.

## Goal

Whether the repo config declared a `[network]` block is representable by any caller who can build
a `Config`, the mode-only trust rule survives every construction path, or `merge` stops being
public. No third state that only the file parser can reach.

## Deliverables

Pick one shape (fork 1) and carry it through:

- The declaration becomes structural -- an optional declared *mode* with an effective accessor,
  or `Option<NetworkConfig>` plus an explicit mode-only merge rule -- and the `declared` bit, its
  two `pub(crate)` accessors, and the hand-written `PartialEq` that exists to hide it all go away.
- **The trust rule is expressed in the type or in a fallible merge, not in a text scan.** Decide
  fork 2: public `merge` ignores repo policy fields, or becomes fallible and rejects them, or the
  security contract changes deliberately and is documented as changed.
- Every reader of `config.network` moves to the effective accessor. The set is small: `merge.rs`,
  `validate.rs`, `outrig_.rs`'s lowering (which 0118 rewrites), and the CLI's session setup.
- `crates/outrig/public-api.txt` regenerated. `crates/outrig/CHANGELOG.md` records the reshape as
  a break, with the one-line migration.
- `doc/reference/config.md` (a **symlink** into `crates/outrig-cli/src/mcp_self/docs/`) wherever
  it explains global-versus-repo `[network]` precedence, since the rule is now expressible rather
  than implied.

## Acceptance

Construction parity:

- A `Config` built by `Config::default()` plus public field assignment, with repo mode `Audit`,
  merges to `Audit`.
- A `Config` from bare `toml::from_str::<Config>` with the same repo mode merges to `Audit`.
- A repo config that declares no `[network]` still inherits the global block -- the behavior the
  bit exists to protect, and the test that catches a fix which always applies the repo value.

Trust boundary, which is the half the earlier version of this task missed:

- Repo `mode = "filter"` with a global `allow`/`deny` yields **exactly** the global policy: the
  mode changes, the rules do not.
- A repo value carrying its own `allow`/`deny`, built programmatically *and* by direct serde,
  cannot widen or inject policy. Two tests, because the text scan protects neither path.
- Repo `mode = "default"` changes only the mode.
- A merged config that is serialized and reparsed does not regain hidden declaration state --
  the round trip is where a `#[serde(skip)]` field's replacement can quietly reintroduce the same
  bug.

`crates/outrig/tests/config_merge.rs` carries all of these, written as an external consumer would.

## Design forks

1. **`Option<NetworkConfig>` versus an optional declared mode -- Recommended: optional mode.**
   The earlier version of this task preferred `Option<NetworkConfig>` as the honest shape. It is
   honest about *declaration* and wrong about *granularity*: the merge is per-key by design, and
   a whole-block option invites exactly the policy-injection failure above. An optional
   `mode: Option<NetworkMode>` with an `effective_mode()` accessor matches what the merge
   actually does. The cost is that "declared" is then a property of one field rather than the
   block, which must be written down so the same question is not reopened for `allow`/`deny`.

2. **What public `merge` does with repo policy fields -- Open, and it is the security call.**
   Ignoring them silently is today's effective behavior for file-loaded configs and is the least
   surprising. Making `merge` fallible is the most honest and changes a public signature. Doing
   neither -- letting a programmatic repo config carry policy -- is a deliberate weakening and
   needs to be recorded as one, not arrived at.

3. **Whether `reject_repo_network_policy`'s text scan survives -- Open, resolve while doing the
   work.** Once declaration is structural, the check may become an ordinary validation on the
   parsed value, which is better, because a text scan cannot see through formatting. Confirm
   before deleting: the scan may also be catching keys the parser accepts.

4. **The alternative: stop promising public `merge` -- Recommended only if fork 1 proves
   expensive.** Narrowing `merge` and direct `Deserialize` construction to `pub(crate)` fixes the
   inconsistency by removing the promise. It is the smaller change and the larger loss; a library
   whose config type cannot be built programmatically is a worse library. Take it only with the
   reason written down.

## Dependencies

None hard. **Sequenced before 0118**, which reads and lowers the very field this may reshape;
doing them the other way means writing the lowering twice. Land before 0125 regenerates the API
snapshot, and before 0126 writes the migration guide that has to describe this.

## See also

- `crates/outrig/src/config/mod.rs:1558-1600` -- `NetworkConfig`, its `Default`, its `PartialEq`,
  and the two `pub(crate)` accessors; `reject_repo_network_policy` at 634, called from 318.
- `crates/outrig/src/config/merge.rs:56-60` -- the branch that reads the bit.
- `plan/done/0108-global-workspace-block-dropped.md` -- the same question for `[workspace]`,
  answered with private fields and guarded setters.

## Decisions

- **Fork 1: `NetworkConfig` becomes four private fields, every one of them optional.**
  `mode: Option<NetworkMode>`, `default: Option<NetworkAction>`, and
  `allow`/`deny: Option<Vec<NetworkEntry>>`. `None` *is* "the config did not declare this
  key", carried through serde like every other key, so no state exists that only the file
  parser can reach. This is `plan/done/0108-global-workspace-block-dropped.md`'s answer for
  `[workspace]`, applied to the block that motivated 0108's rejection of a declared
  boolean in the first place -- 0108's Decisions name `NetworkConfig` as the shape it did
  not want to copy.

  Deleted with the reshape: the `declared` field, the hand-written `Default`, the
  hand-written `PartialEq` (and its `impl Eq`), `is_declared`, `set_declared`, and
  `declares_top_level_network`. All replaced by derives.

  This settles the task's note that "declared" becomes a property of one field rather than
  the block: every key answers the question for itself, and none of them answers it with a
  sentinel value an author could also have written.

  The first cut of this kept `allow`/`deny` as bare `Vec`s on the reasoning that "emptiness
  already answers it". That premise is false -- an author can write `allow = []`, and
  `Vec::is_empty` cannot tell that from an absent key -- so a repo config spelling its
  policy that way loaded clean where the deleted text scan had rejected it, and the
  documentation this task rewrote asserted the stricter rule. Caught in review. It is the
  same absence-versus-explicit-value problem `default` carries an `Option` for, and the
  lesson is that "the type already has a natural empty" is not the same claim as "the type
  cannot represent an explicit empty".

- **Every policy key is an `Option`, which is what lets fork 3 be answered.** The
  trust-boundary check has to distinguish an absent `default` from an explicit
  `default = "deny"`, and an absent `allow` from an explicit `allow = []`; bare
  `NetworkAction` and `Vec` cannot, which is precisely why the old check had to read raw
  text. Making only `mode` optional would have left the text scan alive for the rest, and
  inside the freeze. So fork 3 resolves to *both* scans deleted, not one.

  This costs nothing at the public surface: the fields are private, so `policy()`,
  `set_policy()`, and `has_policy_entries()` keep their signatures and `public-api.txt` is
  unmoved by the `Option<Vec<_>>`.

- **Fork 2: `merge` ignores repo policy structurally, and stays infallible.** The body reads
  `repo.network.declared_mode()` and nothing else, so a repo value carrying policy cannot
  widen or inject one -- not because a check rejects it, but because no code path reads it.
  A fallible `merge` was considered and declined: the safety outcome is identical, and it
  would break a public signature to tell a caller something `declared_policy()` already
  lets them ask.

  The rejection of a repo config that *carries* policy stays a per-file, load-time error,
  because the merged value has by construction taken its policy from the global side and so
  cannot say which file declared what. It moved out of mod.rs's raw-text scan and into
  `validate.rs` as `validate_as_repo`, with a typed
  `ConfigValidationError::RepoNetworkPolicy { key }` like every other config rule, reached
  publicly through `Config::validate_as_repo()`. `validate.rs` now has two entry points and
  its module doc says which kind of rule each carries: `validate` on the merged config,
  `validate_as_repo` on one unmerged file. The library therefore ships *the rule* rather
  than the ingredients -- an earlier cut exposed a public `declared_policy()` and told
  embedders to re-derive the check and its message from it, which is the shape that drifts.

- **A `[network]` table declaring no `mode` now inherits instead of resetting to
  `default`.** The old test was "does the file contain a `[network]` table", so a repo file
  consisting of the bare header counted as a declaration and overwrote the global mode with
  `NetworkMode::Default` -- the least restrictive one. Found while doing the work, not
  named in the task. Taken as a fix rather than preserved: a repo that means to opt out
  writes `mode = "default"`, which is still honored, and preserving the old behavior would
  have required keeping a table-level declared bit beside the optional mode, reintroducing
  exactly the hidden state this task deletes.

- **`mode = "default"` now survives a serialize/reparse round trip.** `is_default()`
  compared through the declaration-blind `PartialEq`, so an explicit opt-out was
  byte-identical to silence and `skip_serializing_if` dropped it. The derived `PartialEq`
  fixes this as a side effect; it is tested rather than left to be rediscovered.

- **No per-key `allow()` / `deny()` / `default_action()` getters.** `NetworkPolicy` is
  already the unit every consumer takes (`NetworkInterceptor::start_with_policy`,
  `NetworkSpec::policy`, `NetworkPolicy::validate`), so `policy()` is the one way to read
  the policy, `set_policy()` the one way to write it, and `has_policy_entries()` the cheap
  emptiness check. A second spelling would widen the surface 0124 is about to freeze for no
  capability. `set_policy()` has no in-crate caller and is kept deliberately: with the
  fields private it is the only way an embedder can give a `Config` a filter policy at all,
  which the public fields used to provide.

- **The per-key merge lives in the type, not in `merge`.** `NetworkConfig::apply_repo_overrides`
  is the counterpart to `Workspace::inherit_missing_primary_fields`, so both blocks say
  "repo declaration wins per key" in the same shape and a second repo-settable network key
  is added inside the type rather than in the file that owns the general merge algorithm.
  The direction is deliberately reversed -- global is the base, not the fallback -- because
  that asymmetry is the trust rule, which is why the method is named for the whitelist
  rather than for inheritance.

- **`outrig` no longer depends on `toml_edit`.** The two raw-text scans were its only
  consumers in the library crate. `outrig-cli` keeps its own dependency for
  `outrig mcp add`'s config editing.

- **`SECURITY.md` names the boundary.** Its in-scope list covered the interceptor failing
  to enforce a policy but not a repo config reaching that policy in the first place. Now
  that the rule is structural rather than a text scan, the document that says what counts
  as a vulnerability should state it.

- **Two doc defects fixed in passing.** `doc/reference/config.md`'s "which file wins"
  guidance ended with a sentence pasted from the map-merge paragraph that contradicted the
  two before it, and its validation-rules list still claimed `[network].mode` must be
  `default` or `audit` -- `filter` has been legal since the interceptor landed. The repo
  full example gained `[network].mode`, and `tests/fixtures/config-full.toml` with it, so
  the fixture's claim to exercise every legal section is true for this block.
