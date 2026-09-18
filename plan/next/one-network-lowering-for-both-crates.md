# The CLI keeps its own copy of the `NetworkConfig` -> (mode, policy) mapping

> Found while doing `0002-41`, which gave the library
> `From<&NetworkConfig> for NetworkSpec`. That impl's doc comment says it exists so no
> caller writes its own copy of the mapping. `outrig-cli` already has one.

## Problem

`crates/outrig-cli/src/cli/session_setup.rs` never builds a `LaunchSpec`, so it never
reaches the new conversion. It re-derives the same decision across three places:

- `:291` -- `args.network_mode_override.unwrap_or(cfg.network.mode())`, the effective mode
  with the `--network` flag folded in.
- `:300` -- filter mode with no `allow`/`deny` entry is rejected up front.
- `:1015-1026` -- `attach_interceptor` dispatches on a `mode_word: &str` and reads
  `args.cfg.network.policy()` directly for the filter arm.

The two copies already disagree, in ways that are defensible individually and
indefensible as a pair:

- The CLI rejects an empty filter policy before starting anything; the library defers to
  `NetworkPolicy::validate(true)` inside `Outrig::launch` (`outrig_.rs:849`), so the same
  config fails at a different moment with a different message.
- The CLI has a `--network` override; the library documents its absence. That asymmetry is
  real and should survive, but it should be *an override applied to one lowering* rather
  than the reason two lowerings exist.

A fourth `NetworkMode`, or a fifth `[network]` key, has to be wired through both. The
library side fails to compile if it is forgotten (`From<&NetworkConfig>` matches
exhaustively, and `NetworkMode` is declared in-crate); the CLI side has an
`#[non_exhaustive]` catch-all at `:828` that refuses at runtime instead.

## Goal

One lowering, consumed by both crates, with the flag override applied to its result rather
than ahead of it. Sketch:

- `NetworkSpec::from(&cfg.network)`, then a `with_mode` style override for `--network`.
- `NetworkInterceptor` starts from a `NetworkSpec` rather than from a mode string plus a
  separately-fetched policy, which is what lets the "filter needs entries" rule live in one
  place.

## Acceptance

- The `mode_word: &str` dispatch in `attach_interceptor` is gone.
- A config with `mode = "filter"` and no entries fails identically through `outrig run` and
  through `Outrig::launch`, with the same message.
- Adding a `NetworkMode` variant breaks the build in both crates, not just the library.

## See also

- `crates/outrig/src/outrig_.rs` -- `From<&NetworkConfig> for NetworkSpec` and the
  `NetworkMode` match in `Outrig::launch`.
- `plan/next/launch-spec-security-lowering.md` -- the same "one conversion, many hand-copied
  sites" shape for `[security]`. Genuinely separate: that one targets `ContainerLaunchSpec`,
  which has no network field at all.
