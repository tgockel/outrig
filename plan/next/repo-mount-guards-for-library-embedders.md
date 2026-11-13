# The repo-declared mount guards live in the CLI, so embedders get none

## Context

`outrig run`, `outrig mcp` and `outrig run-new` -- the host-program form since #646 -- refuse a
repository's config that names the home directory, or the global config read-write, as a
workspace or a mount, and pin what passes to the path checked (#445, #604, #609). All of it lives
in `crates/outrig-cli/src/paths.rs`: `refuse_home_workspace`,
`refuse_workspace_covering_global_config`, `pin_repo_declared_mounts`, `recheck_pinned_mounts`
and `refuse_repo_declared_mounts`. A library caller that loads a repository's config with
`Config::load` and starts from it -- `SessionBuilder`, `LaunchSpec::from_config`, `HostProgram` --
is held to none of them, as #646 notes: "`outrig::harness` embedders get no such check either,
since the refusals live in the CLI".

## Shape

- The rules move into `outrig`, taking what the CLI knows and the library does not: the repo
  root, the invoking user's home directory, and the global config's path. A `Config` does not
  remember the last: one that inherited nothing from the global file carries no
  `ConfigSource::Global`, so the path is a parameter, as it is to `Config::load`.
- The pin and its recheck move with them: they are one walk over the same entries.
- `HostProgram` and `SessionBuilder` apply them to the configuration they are given, so an
  embedder that hands over a repository's config is covered where the CLI's caller is.
- The CLI keeps the diagnostics it has: the wording, the file named, and the help line.

## Acceptance

- A session an embedder starts from a repo config that mounts the home directory, or the global
  config read-write, is refused, naming the file that declared it.
- `crates/outrig-cli/tests/home_workspace.rs` passes unchanged.
