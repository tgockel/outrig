# `--env` flags for `outrig run` and `outrig mcp`

> **Status:** preliminary spec. Carved into a numbered task in `plan/todo/`
> when ready.

## Context

The MCP `env` table in `config.toml` now accepts both literal strings and
`${VAR}` host-env references (`feat/mcp-env-substitution`, on top of the
api-key syntax in `plan/done/0004-api-key-syntax.md`). That covers env
vars that belong to the *repo's* config -- shared with collaborators,
checked in.

What's still missing: a way to add env vars *at invocation time* without
editing `config.toml`. Common cases:

- A one-off run that needs `RUST_LOG=debug` for the `cargo-mcp` server.
- Trying out a different `${ANTHROPIC_API_KEY}`-style token without
  changing what the repo's config references.
- Ad-hoc env vars for a single MCP server during debugging, where adding
  them to the repo config would be wrong (specific to one developer's
  machine).

This applies equally to `outrig run` (today) and the planned `outrig mcp`
(`plan/todo/0040-outrig-mcp-wire-subcommand.md`) -- both spawn the same
MCP servers via `connect_via_podman_exec`, so the same flag should work
on both subcommands.

## Goal

Let the user pass env-var entries on the command line that are merged
into each MCP server's resolved env at startup. Two shapes:

- **Global**: `--env KEY=VALUE` -- applied to every MCP server.
- **Per-server**: a way to scope an entry to a single MCP server by
  name (e.g. `fs`, `build`, `shell`).

The exact CLI surface for the per-server form is an open question (see
**Sub-decisions** below); the user's initial proposal is `--env:<name>=KEY=VALUE`.

## User surface

```bash
# All MCP servers see FOO=bar:
outrig run --env FOO=bar

# Only the `fs` MCP server sees DEBUG=1:
outrig run --env:fs=DEBUG=1     # if we go with the colon-suffixed flag
# or, alternative form discussed below:
outrig run --env fs:DEBUG=1     # server-prefix in the value

# Mixed: every server gets RUST_LOG=info; cargo-mcp also gets CARGO_TERM_COLOR=always:
outrig run --env RUST_LOG=info --env:build=CARGO_TERM_COLOR=always

# Same surface on `outrig mcp`:
outrig mcp --env RUST_LOG=info
```

`--env` may be repeated. Within a single `outrig run`/`outrig mcp`
invocation, all repetitions are collected.

### Value parsing -- reuse `EnvValue`

The right-hand side of each entry runs through
`outrig::config::EnvValue::from_raw` (`src/config/env_value.rs`), so
`--env GH_TOKEN='${GITHUB_TOKEN}'` resolves from the host env at MCP
startup, identically to a config-file value. This keeps the CLI and
config-file syntaxes consistent and reuses the existing resolution
plumbing rather than introducing a parallel one.

(In practice the shell already substitutes `$GITHUB_TOKEN` before clap
sees it, so most users will write `--env GH_TOKEN=$GITHUB_TOKEN` and
never touch the `${VAR}` form on the CLI -- but the option exists, and
behaves the same as in config.)

### Precedence

Per env key, *per server*, last write wins, in this order:

1. `[containers.<name>.mcp.<server>].env` from config (already merged
   per `src/config/merge.rs`).
2. `--env KEY=VALUE` (global CLI -- applied to every server).
3. `--env:<server>=KEY=VALUE` (per-server CLI -- applied only to the
   matching server).

So a per-server CLI entry overrides a global CLI entry, which overrides
a config-file entry. Removing an env var that the config provides is
not in scope for this task (out of scope below).

### Errors

- Malformed entry (no `=` separator after `KEY`): clap-time error,
  `error: invalid value 'FOO' for '--env <KEY=VALUE>': missing '='`.
- Per-server entry naming a server that doesn't exist in the resolved
  container's MCP map: load-time error, `--env:typo=...: container
  'coding' has no MCP server 'typo'`. Caught after config validation,
  before any container starts.
- Empty key (`--env =VALUE`): error.
- Duplicate keys within the same scope: silently last-wins (matches
  POSIX `env`'s behavior, and the BTreeMap collection inside outrig).

## Architecture

### CLI parsing

Add to `RunArgs` (`src/cli/run.rs`) and `McpArgs`
(`src/cli/mcp.rs`, landing in 0040):

```rust
/// Add or override env vars for MCP servers. Repeatable.
/// `--env KEY=VAL` applies to every MCP server.
#[arg(long = "env", value_name = "KEY=VALUE", action = ArgAction::Append)]
pub env: Vec<String>,

// Per-server form: see Sub-decisions for syntax.
```

A small post-clap validator parses each `Vec<String>` entry into a
typed structure shared between the two subcommands:

```rust
// src/cli/env_arg.rs (new)
pub struct CliEnvEntries {
    pub global: BTreeMap<String, EnvValue>,
    pub per_server: BTreeMap<String, BTreeMap<String, EnvValue>>,
}
impl CliEnvEntries {
    pub fn parse(raw: &[String]) -> Result<Self, CliEnvParseError>;
    pub fn for_server(&self, name: &str) -> BTreeMap<String, EnvValue>;
}
```

`for_server` produces the global map with the per-server overlay applied
in the right order; the result is layered onto the config-file env at
the spawn site.

### Spawn-site merge

`src/mcp.rs::connect_via_podman_exec` already takes the spec and resolves
`EnvValue` entries before handing the result to `build_exec_argv`. The
merge happens *before* resolution: thread a
`&BTreeMap<String, EnvValue>` overlay into the function (or into a new
sibling that wraps it), and merge config + overlay before the resolve
loop runs. Per-server resolution still produces the same
`OutrigError::McpEnvResolveFailed { name, key, source }` framing.

The cleanest signature change is probably to add an `extra_env` parameter
to `connect_via_podman_exec` (default `&BTreeMap::new()` from existing
callers; threaded through from `cli/run.rs` and `cli/mcp.rs` for the new
behavior). An `Option<&...>` is also fine -- pick whichever leaves call
sites cleaner.

### Validation order

1. clap parses argv -> `Vec<String>` of env entries.
2. Config loads + validates as today (`Config::load`).
3. **New**: `CliEnvEntries::parse` runs; rejects malformed entries.
4. **New**: cross-check per-server entries against the resolved
   container's `mcp` map; reject unknown servers with a pointed error.
5. Container starts; for each MCP server, `connect_via_podman_exec` is
   called with the per-server overlay.

## Sub-decisions

These are deliberately left for the task author who picks this up:

- **Per-server CLI syntax.** The user's initial form is
  `--env:<name>=KEY=VALUE`. Clap's `long` names are static and registered
  at compile time, so `--env:fs` isn't a separately-recognized flag --
  the implementer has two paths:
  1. **Encode in the value**: register a single `--env`, accept either
     `KEY=VAL` (global) or `SERVER:KEY=VAL` (per-server). Trade-off:
     slightly less self-documenting, but works inside clap. Sample:
     `outrig run --env fs:DEBUG=1`.
  2. **Pre-parse argv**: before handing to clap, scan for
     `--env:<name>=...` and `--env=...`, partition them out, and feed
     clap a synthetic `--env` per entry. Trade-off: matches the user's
     literal proposal but loses clap's `--help` rendering and validation
     for the per-server form.

  Recommendation: option 1 unless the literal `--env:<name>=...` syntax
  is load-bearing for the user. The semantics are identical.
- **Removal / unset.** Should `--env KEY=` (empty value) mean "force
  empty" (current default per config) or "unset, even if config sets
  it"? Or should there be a separate `--unset-env KEY` / `--unset-env:<name> KEY`?
  Out of scope here unless a concrete use case shows up.
- **Order of repeated keys within a scope.** Within global, last
  `--env FOO=...` wins (we collect into a BTreeMap; drop earlier).
  Within per-server, same. Document this in `--help` so users don't
  rely on first-wins.
- **Whether to allow per-server entries that target the agent's *own*
  process env** (i.e., the REPL/agent process inside the container,
  not just MCP servers). v0 has no such surface, so this is theoretical
  -- skip unless the agent gains its own env-overlay surface.

## Deliverables

- `src/cli/env_arg.rs` (new) -- `CliEnvEntries` parser + tests.
- `src/cli/run.rs` -- `--env` flag added to `RunArgs`, parsed into
  `CliEnvEntries`, validated against the resolved container's MCP map,
  threaded into the connect-each-MCP loop.
- `src/cli/mcp.rs` -- same wiring on the `outrig mcp` side once 0040
  lands.
- `src/mcp.rs::connect_via_podman_exec` -- takes a per-server overlay
  and merges it onto the config-file env before resolving.
- `doc/reference/cli.md` (or wherever the `outrig run` flag table lives
  -- check after 0041 lands `outrig mcp` docs) -- new `--env` row plus
  a brief precedence note pointing back to
  `doc/reference/config.md#mcp-env-value-syntax` for the value grammar.
- `tests/cli_env.rs` -- unit tests on `CliEnvEntries::parse` (accept,
  reject, merge precedence) and an integration test that asserts the
  merged env shows up on the spawned `podman exec` argv (mirror the
  pattern in `tests/mcp_handshake.rs`).

## Acceptance

- `outrig run --env FOO=bar` adds `--env FOO=bar` to every MCP
  server's `podman exec` invocation.
- `outrig run --env <per-server-syntax>` (whichever syntax the
  sub-decision lands on) scopes the entry to one server only.
- `--env GH_TOKEN='${GITHUB_TOKEN}'` resolves from the host env at MCP
  startup, identically to the config-file form.
- `--env:typo=KEY=VAL` (or the chosen per-server syntax) referencing a
  non-existent MCP server fails fast with a clear error before any
  container starts.
- Backward compat: every existing `outrig run` invocation without
  `--env` behaves identically; config-file env continues to win when
  no CLI overlay touches that key.
- `outrig mcp --env ...` accepts the same surface once 0040 lands.

## Dependencies

- `feat/mcp-env-substitution` -- introduces `EnvValue`, the resolution
  plumbing, and the framed `McpEnvResolveFailed` error this work
  reuses.
- `plan/todo/0040-outrig-mcp-wire-subcommand.md` -- defines `McpArgs`,
  which gains the same `--env` flag once it lands. (This task can ship
  for `outrig run` first and extend to `outrig mcp` in a follow-on if
  ordering matters.)
