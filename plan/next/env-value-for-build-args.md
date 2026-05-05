# `${VAR}` substitution for `build-args`

> **Status:** preliminary spec. Carved into a numbered task in `plan/todo/`
> when ready.

## Context

The MCP server `env` table grew `${VAR}` substitution alongside literals
(`feat/mcp-env-substitution`, on top of `plan/done/0004-api-key-syntax.md`):
each value is either a literal string or a host-env reference resolved at
MCP-startup time, with the same `^\$\{[A-Z_][A-Z0-9_]*\}$` syntax as
`api-key`.

`ContainerConfig::build_args` (`src/config/mod.rs:163`) is the same shape
-- `BTreeMap<String, String>` -- and the same logical use case. The
existing reference doc at `doc/reference/config.md:267-268` already frames
its keys as "env-var-style identifiers". Extending the same `EnvValue`
treatment is a natural follow-up; deferred from the MCP-env task to keep
that change focused.

## Goal

`build-args` values accept either a literal string (current behavior) or a
`${VAR}` reference resolved from the host environment at `outrig build`
time, surfaced to `buildah` as `--build-arg KEY=resolved-value`.

## Deliverables

- `ContainerConfig::build_args` field type: change from
  `BTreeMap<String, String>` to `BTreeMap<String, EnvValue>` (the type added
  in `src/config/env_value.rs` for MCP env).
- Resolve loop at the `outrig build` call site (the place that constructs
  `--build-arg` flags from `build_args`); on `EnvValueError`, return a
  framed `OutrigError::BuildArgResolveFailed { container, key, source }`
  variant -- mirrors the `McpEnvResolveFailed` framing.
- Doc update in `doc/reference/config.md` `[containers.<name>]` field
  table (around line 260): mention the same `${VAR}` syntax for
  `build-args` and reference the existing MCP-env subsection rather than
  duplicating the full block.
- Tests in `tests/build_args_env_value.rs` (or extend an existing
  build-args test) covering parse classification, mixed literal+ref,
  and resolve set/unset framed as a `BuildArgResolveFailed`.
- Update `tests/config_schema.rs` assertion at line 168
  (`coding_ctr.build_args["NODE_VERSION"]`) to the new `EnvValue::Literal`
  shape.

## Acceptance

- `cargo test` passes including the new build-args tests.
- A repo `config.toml` with `build-args = { GH_TOKEN = "${GITHUB_TOKEN}" }`
  builds successfully when `GITHUB_TOKEN` is exported, and fails with a
  pointed error naming the variable when it isn't.
- Backward compat: every existing `build-args = { K = "v" }` config
  continues to load and pass-through to buildah unchanged.

## Dependencies

- `feat/mcp-env-substitution` -- introduces `EnvValue`, `EnvValueError`,
  the shared `parse_env_ref`, and the framing pattern this task copies.
