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

## Decisions

- **Provider style is selected via `ask_select`, not `ask_string`.** The schema
  has exactly two valid values (`openai`, `mistralrs`) and `ask_select`'s help
  output naturally surfaces both with their blurbs from a single `Field`. Free
  text would have produced the same prompt rendering but admitted typos that
  only fail downstream.
- **Mistralrs is a first-class branch in the prompt flow.** The original task
  spec's defaults are openai-shaped, but the `LlmProvider` enum already supports
  `mistralrs` and the build-feature gate is run-time, not validate-time. Adding
  the branch now (with a `Use auto-download by model ID? [Y/n]` Y/N gate
  splitting model-id-plus-optional-`revision` from local-path, plus optional
  `context-length`) keeps `config init` from needing a follow-up the moment
  someone wants an in-process provider. `model-file` is intentionally not
  prompted -- the schema makes it optional and the typical single-file repo
  case doesn't need it; users wanting a multi-file repo edit the TOML.
- **Empty `default` in `ask_string` drops the `[default: ]` suffix.** The
  prompt UX module renders `? <name>: ` (no bracketed hint) when the default
  is empty, so prompts where empty is not a meaningful suggestion read
  cleanly. Required-non-empty fields (model-id, model-path) layer
  `ask_required` on top, which re-prompts with an `[outrig] this field
  requires a value` notice on empty input.
- **Default-model selection: Y/N for one model, free-text for many.** The doc
  walks through the single-model UX (`Use this model as default-model? [Y/n]`)
  and that's preserved verbatim. With 2+ models defined we fall through to a
  free-text `Default model name [default: <first>]` prompt that re-prompts on
  unknown names; an empty answer means "no default".
- **Render via a purpose-built `GlobalOut` newtype, not the full `Config`.**
  `Config::Workspace` has a non-`Option` default that would otherwise emit
  `[workspace]\nhost-path = "."` into a global config that should not own
  workspace state. The newtype only serializes `default-model`, `providers`,
  and `models`, all with `skip_serializing_if` for emptiness.
- **Existence check + `--force` short-circuits before any prompt.** Catching
  the conflict up front avoids burning the user through 11 prompts only to bail
  on the final write. The check is plain `path.exists()`; `tempfile::persist`
  itself clobbers on POSIX, which is fine since the upfront check already
  enforced the intent.
- **`From<tempfile::PersistError> for OutrigError` lives in `error.rs`.** Both
  `config::init::write_atomic` and the existing `session::write_session_json_atomic`
  were unwrapping `e.error` by hand; the impl drops the awkward `.map_err`
  call at both sites.
- **Field constants exported as `pub const DOC_SYNC_FIELDS`.** Per 0022's
  manual-slice convention, `tests/prompt_doc_sync.rs` extends its
  `EXAMPLE_FIELD` baseline with this slice instead of importing each Field
  individually.
- **`outrig config` is a clap subcommand group with one variant for now.**
  Adding `ConfigArgs { cmd: ConfigCmd }` matches the shape `git config` uses
  and prepares the surface for `config get` / `set` / `list` without revisiting
  the dispatch wiring.
- **Top `> TODO: Incomplete` marker dropped from `doc/usage/config.md`; the
  bottom one (non-interactive flag mode) kept.** The page describes
  `config init` end-to-end now; `config get`/`set`/`list` are explicitly noted
  as deferred in the intro paragraph. The bottom marker still applies to the
  truly-deferred `--provider ... --model ...` form.
