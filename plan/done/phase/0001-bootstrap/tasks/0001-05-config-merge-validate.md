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

## Decisions

- **`Config::load(repo_root: &Path, global_path: Option<&Path>)` instead of `repo_path`.**
  Task spec said `repo_path`, but the validation step needs the repo root to resolve
  `dockerfile`/`context` paths. Deriving the root from a config-file path requires
  three `parent()` calls (the standard layout is `<root>/.agents/outrig/config.toml`),
  which is fragile and breaks when `--config` overrides walk-up. Taking `repo_root`
  directly + calling `repo::repo_config_path(repo_root)` internally aligns with how
  `find_repo_root` already returns a root, and keeps the disk-path derivation in one
  place. Tests use `tmp.path()` as the root.
- **Validation lives in `src/config/validate.rs` (new module) with a free
  `pub(super) fn validate(...)` plus a `Config::validate(&self, ...)` method on
  `Config`.** The method form is what callers and tests use (`cfg.validate(repo_root)`);
  the free function is a `pub(super)` implementation detail. `ConfigValidationError` is
  re-exported through `config::ConfigValidationError` and integrated into `OutrigError`
  via `#[from]`, mirroring the `ApiKey(#[from] ApiKeyError)` pattern from 0004.
- **`merge::merge` signature consumes both inputs.** `merge(global: Config, repo: Config)
  -> Config` matches the task spec verbatim. `BTreeMap::extend` runs in linear time over
  the repo (the smaller side typically), so the move-and-extend pattern is efficient.
- **`Workspace` is taken from `repo` unconditionally.** `Workspace` derives serde
  defaults, so an absent `[workspace]` block in the repo file deserializes to the
  documented defaults rather than to a sentinel "missing" value. That makes the "repo
  wins block-level" rule unambiguous: there is no "absent" repo workspace to fall back
  from.
- **First-error-wins validation order.** `validate` short-circuits on the first
  violation via `?` returns. The check order is documented in the function: scalars
  (`default-*`), then per-agent (model/container), then per-model (provider), then
  per-container (mcp shape + disk paths), then `session-root`. Errors carry the
  offending name(s) so users get the same precision Configs gave for `api-key`.
- **Disk-existence check is gated on `repo_root: Option<&Path>`.** `None` keeps
  `validate` pure-structural so unit tests don't need a tempdir for everything; `Some`
  enables the `dockerfile.exists()` / `context.exists()` checks. The test
  `disk_checks_skipped_when_repo_root_is_none` pins this dual-mode contract.
- **`Config::load` treats absent global config via `ErrorKind::NotFound`, not via a
  prior `exists()` syscall.** Avoids the TOCTOU window between check and read, and
  removes a redundant stat. Other I/O errors propagate through `?`.
- **`mcp_server_name_re()` is its own `OnceLock<Regex>` in `validate.rs`.** Mirrors
  the `api_key.rs` pattern rather than introducing a shared `regex.rs` helper module
  for two unrelated regexes (different anchored patterns, different validation domains).
- **No shared `tests/common/` helper module.** Continues task 0003's decision:
  `tests/config_merge.rs` keeps its own `parse()` and `write_repo_cfg()`. Cross-file
  test deduplication is premature -- there are still only three integration test files,
  and their setup needs (tempdir vs inline TOML) diverge.
