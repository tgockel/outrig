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
