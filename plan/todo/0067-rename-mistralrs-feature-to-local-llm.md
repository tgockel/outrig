# 0067 -- Rename `mistralrs` Cargo feature to `local-llm`

## Context

The `outrig-cli` crate gates its in-process LLM support behind a Cargo
feature named `mistralrs`, after the backing crate. The feature name
should describe **what the build gains** (a local, in-process LLM),
not **how** it does so. `mistralrs-core` is one implementation choice;
the public-facing knob users flip with `--features` is "do I want a
local LLM compiled in." Rename `mistralrs` -> `local-llm` everywhere.

The `style = "mistralrs"` config value is *not* renamed by this task:
it names the specific provider implementation in `models.toml` and is
a user-facing config contract. Only the build-time feature flag moves.

## Goal

Rename the `mistralrs` Cargo feature to `local-llm` in `Cargo.toml`,
all `#[cfg(feature = "...")]` sites, build script checks, and the
documentation that mentions `--features mistralrs`.

## Deliverables

- `crates/outrig-cli/Cargo.toml`: feature rename
  (`mistralrs = [...]` -> `local-llm = [...]`); update the `cuda` /
  `metal` feature definitions and the `build.rs`-driven warnings to
  refer to `local-llm`.
- `crates/outrig-cli/build.rs`: update both `cfg!(all(feature = "...",
  not(feature = "mistralrs")))` checks to use `local-llm`.
- All `#[cfg(feature = "mistralrs")]` and `cfg!(feature = "mistralrs")`
  sites in `crates/outrig-cli/src/` and `crates/outrig-cli/tests/`
  (40+ sites; see `crates/outrig-cli/src/llm.rs`,
  `crates/outrig-cli/src/hf.rs`, `crates/outrig-cli/src/cli/run.rs`,
  `crates/outrig-cli/tests/llm_resolve.rs`).
- Documentation: replace `--features mistralrs` /
  `--features "mistralrs ..."` references in `doc/`. Sites known so
  far: `doc/reference/config.md` (multiple), plus any matching strings
  in `doc/usage/*.md`. **Do not** touch the `style = "mistralrs"`
  config-value references in the same files -- those name the
  provider, not the feature.
- CI and scripts: search `scripts/`, `.github/`, `Cargo.lock` notes,
  and any release/install instructions for `mistralrs` feature
  references and update them.

## Acceptance

- `cargo build -p outrig-cli --features local-llm`,
  `cargo build -p outrig-cli --features "local-llm cuda"`, and
  `cargo build -p outrig-cli --features "local-llm metal"` all work.
- `cargo build -p outrig-cli` (default features, no `local-llm`)
  still works and emits the existing "cuda/metal without local-llm"
  build warnings.
- `cargo test --workspace` passes with and without `--features
  local-llm`.
- `git grep '"mistralrs"' -- '*.rs' '*.toml'` returns only the
  `style = "mistralrs"` provider-name references, never the Cargo
  feature.
- `python3 scripts/audit-doc-style.py doc/reference/config.md
  doc/usage/run.md doc/usage/config.md` passes.

## Dependencies

- None. Independent of the visibility/relocation sweep ahead of it;
  intentionally sequenced before the README/quickstart signposting
  task so the install snippet can ship with the final feature name.
