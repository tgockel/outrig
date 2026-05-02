# 0005 -- Config merge and validate

## Goal

Merge global + repo configs (repo wins by name), then validate every cross-reference
documented in `doc/reference/config.md`'s "Validation rules" section.

## Deliverables

- `src/config/merge.rs::merge(global: Config, repo: Config) -> Config` with these rules:
  - For every map (`[providers]`, `[models]`, `[agents]`, `[containers]`): repo entries
    replace global entries with the same key. No per-key merging within an entry.
  - For top-level scalars (`default-container`, `default-agent`, `default-model`,
    `session-root`): repo value wins if present; else global value.
  - `[workspace]` is repo-only (rare to set globally; if present in global, repo still wins
    block-level).
- `Config::validate(&self) -> Result<()>` enforcing every rule from the docs:
  - `default-container` (if set) names an existing `[containers.<name>]`.
  - `default-agent` (if set) names an existing `[agents.<name>]`.
  - `default-model` (if set) names an existing `[models.<name>]`.
  - For every agent: either `model` is set and resolves, or `default-model` is set and
    resolves (else error).
  - Every `[models.<name>].provider` resolves to `[providers.<name>]`.
  - Every `[agents.<name>].container` (if set) resolves to `[containers.<name>]`.
  - Every MCP server name matches `^[a-zA-Z][a-zA-Z0-9_-]*$`.
  - Every MCP `command` is non-empty.
  - `dockerfile` and `context` paths exist on disk relative to the repo root (validated only
    when a repo root is provided -- pass it as a separate arg to keep the type pure).
  - `session-root` is absolute.
- `Config::load(repo_path: &Path, global_path: Option<&Path>) -> Result<Config>` doing the
  full pipeline: read both files (global may not exist), parse each via `load_from_str`, merge,
  validate.
- `tests/config_merge.rs` covering:
  - Missing `default-container` when `outrig run` would need it -> error.
  - Dangling `agents.<a>.model` -> error pointing at the agent.
  - Dangling `models.<m>.provider` -> error.
  - Dangling `agents.<a>.container` -> error.
  - Agent omits `model` and no `default-model` -> error.
  - Agent omits `model`, `default-model` is set -> resolves.
  - All-good case.
  - Repo overrides global by name (verify the repo entry won).

## Acceptance

- `cargo test config_merge` passes every case.
- End-to-end load of `tests/fixtures/config-full.toml` succeeds.
- Drop the `> TODO: Incomplete` marker at the top of `doc/reference/config.md` -- the config
  schema is fully implemented after this task.

## Dependencies

- 0002-repo-and-config-paths
- 0003-config-schema
- 0004-api-key-syntax

## Notes

- Errors should be specific: `"agent 'review' has model='claude' which does not match any
  [models.<name>]"` rather than `"validation failed"`.
- `validate` takes `repo_root` as an argument so the same `Config` value can be parsed without
  hitting the disk for tests of pure structural validity.
