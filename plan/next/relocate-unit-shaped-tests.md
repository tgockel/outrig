# Relocate unit-shaped tests out of `crates/outrig-cli/tests/`

## Context

0093 made `outrig-cli`'s module tree crate-private behind an `internal-test-api` feature, which
the integration tests enable through a dev-dependency on the crate itself. That closed the
published surface, but it preserved the reason the tree was public in the first place rather than
removing it.

Roughly 14 of the 24 files in `crates/outrig-cli/tests/` are unit tests wearing an integration
test's clothes: they exercise one module's functions directly and never spawn the binary or touch
podman. `llm_resolve.rs` (675 lines), `session_store.rs` (297), `config_init_scripted.rs` (301),
and `cli_env.rs` (229) are the largest. `cli_env.rs` is the clearest case -- it tests
`cli::env_arg` from outside while `src/cli/env_arg.rs` already carries its own
`#[cfg(test)] mod tests` doing the same kind of thing.

29 of the 50 files under `src/` already have inline `#[cfg(test)] mod tests`, so the pattern is
established; these files are the exception.

## Goal

Move the unit-shaped tests inline, so module visibility is driven by what the crate needs rather
than by what a separate test crate must reach.

## Payoff

- The `internal-test-api` feature shrinks toward the ~10 files that genuinely drive the binary
  end-to-end, and may disappear entirely.
- With it, the self dev-dependency in `crates/outrig-cli/Cargo.toml` goes away, along with the
  second feature variant of the lib that it makes Cargo build (a full extra lib compile plus bin
  relink on any dev loop that alternates `cargo build` and `cargo test`).
- `exclude = ["tests/"]` in that manifest can be revisited -- it exists because a path-only
  dev-dependency does not survive publishing, so a shipped `tests/` would not compile.
- The ~10 per-item `#[cfg_attr(not(feature = "internal-test-api"), allow(dead_code))]` annotations
  disappear as their items gain in-crate callers.

## Notes

Deliberately not folded into 0093: it is a ~4k-line test move with no bearing on what `0.2.0`
freezes, and it should not gate a release. Nothing about it is urgent -- the boundary is already
correct; this makes it cheaper.

Related: `plan/done/0093-shrink-reachable-surface.md`.
