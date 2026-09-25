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
- `--env` has nothing to act on while `run-new` starts no MCP server.
- `-v` writes `logs/container.log` under `run` because `session_setup` attaches a transcript to
  its containers. `Outrig` has no transcript to attach.

The REPL has no slash commands of its own either. `/reset` in particular needs deciding before it
exists: clearing the conversation while the interpreter keeps every name leaves the model with
state it has no record of making.

## Acceptance

- Each flag `run-new` gains behaves as `run`'s does, tested the way `run`'s is.
- `doc/reference/cli.md`'s `run-new` section drops the sentence listing what it does not take.
