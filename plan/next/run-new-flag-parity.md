# `run-new` takes four of `run`'s flags

## Context

`0003-05` gave `outrig run-new` `--agent`, `--model`, `--image`, and `--session-dir`. It launches
through `LaunchSpec::from_config`, which lowers the config and nothing else, so the rest of
`run`'s flags have no path in yet:

- `--network` and `--volume` would be `LaunchSpec::with_network_mode` / `with_network_filter` and
  `with_mount` on the spec, plus `run`'s checks (filter mode needs policy entries; a volume's host
  dir must exist). Config's `[network]` and `[workspace]` already apply.
- `--max-tool-calls` and `--max-tool-result-bytes` override the resolved agent, which
  `PythonAgent::start` does inside the library. They need either parameters on `start` or a
  setter, and either grows `outrig::PythonAgent`'s surface. The config keys already apply.
- `--image` with a local image ref no `[images.<name>]` block names: `from_config` refuses it.
  `LaunchSpec::from_image` plus `with_workspace` would carry it.
- `--events off|record`, which `0003-13` left out by the maintainer's choice: `[events] mode`
  is config-only for now. `run`'s `--network` is the model, and `EventsConfig::set_mode` on the
  loaded config is the whole of it, since `PythonAgent::start` reads the mode from the `Config` it
  is handed. `run` has no such flag to match, because it records nothing.
- `--env` has nothing to act on while `run-new` starts no MCP server.
- `-v` writes `logs/container.log` under `run` because `session_setup` attaches a transcript to
  its containers. `Outrig` has no transcript to attach.

The REPL had no slash commands of its own. Phase 0003's planning (2026-09-30) gives it the first
ones: `/approve <id>` and `/deny <id>` answer a boundary escalation (`0003-22`), and `/name text`
sends a skill directive to the main agent (`0003-28`). Built-in commands win over a skill of the
same name, so a built-in added later changes what `/<name>` does in a project that already has a
skill by that name. `run`'s `/tools`, `/reset` and `/sidecar add` are still absent, and `/reset`
in particular needs deciding before it exists: clearing the conversation while the interpreter
keeps every name leaves the model with state it has no record of making.

## Acceptance

- Each flag `run-new` gains behaves as `run`'s does, tested the way `run`'s is.
- `doc/reference/cli.md`'s `run-new` section drops the sentence listing what it does not take.
