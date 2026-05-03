# 0024 -- `outrig config init`

## Goal

Interactive write of the global config at `~/.outrig/config.toml` (providers, models,
default-model). This is the global-only half of the original all-in-one `outrig init`; the
new `outrig init` (0026) calls `config init` automatically when no global config is found.

`outrig config` is a new top-level command group; v0 ships only the `init` subcommand
(`config get`, `config set`, etc., are reserved for later).

## Deliverables

- `src/config/init.rs::run(force: bool) -> Result<()>` matching `doc/usage/config.md`.
- Prompts (using 0022's `ask_*`):
  - Provider style (default `openai`).
  - Provider name (default same as style; used as the key in `[providers.<name>]`).
  - Base URL (default by style -- `https://api.openai.com/v1` for `openai`).
  - API-key env-var name (default `OPENAI_API_KEY` for `openai`).
  - "Add another provider? [y/N]" loop.
  - "Define a model now? [Y/n]" -- on yes:
    - Model name (default `fast`).
    - Model identifier (default suggestion by style; for `openai`, `gpt-4o-mini`).
    - Provider for this model (default the just-defined provider name).
  - "Add another model? [y/N]" loop.
  - "Use this model as default-model? [Y/n]" -- if yes, set `default-model` at top level.
- Render and write `~/.outrig/config.toml` atomically via `tempfile::NamedTempFile::persist`.
- Refuses to clobber an existing global config without `--force`.
- `tests/config_init_scripted.rs`: drive `config::init::run` against a scripted stdin in a
  tempdir (with `HOME` redirected); assert the resulting config parses + validates via
  0005's loader.

## Acceptance

- `cargo test config_init_scripted` passes.
- Manual: `outrig config init` from a fresh tempdir writes a parseable
  `~/.outrig/config.toml`; re-running without `--force` errors out as documented.
- Drop the `> TODO: Incomplete` markers on `doc/usage/config.md`.

## Dependencies

- 0005-config-merge-validate
- 0022-prompt-ux

## Notes

- The path resolution honors `--global-config` and `XDG_CONFIG_HOME` per
  `doc/reference/cli.md` (the global-config search order). The default fallback is
  `~/.outrig/config.toml`.
- Atomic writes via `tempfile::NamedTempFile::persist` so an interrupted prompt never
  leaves a half-written config.
- The repo-config phase and the chain into container scaffolding live in 0026
  (`outrig init`), not here.
