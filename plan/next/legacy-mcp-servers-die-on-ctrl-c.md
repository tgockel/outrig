# `outrig run`: a Ctrl-C ends every MCP server in the primary container

## Context

`0003-06` measured that a `podman exec -i` client exits on `SIGINT` (status 1), and that the
process it started then sees EOF on stdin and ends. A terminal delivers Ctrl-C to its whole
foreground process group, and outrig's children are in outrig's group, so every Ctrl-C reaches
every such client. `run-new`'s interpreter was ended this way, at the prompt as much as mid-round,
until its client was moved to a process group of its own (`Cmd::in_own_process_group`,
`Container::exec_stdio_in_own_group`).

Legacy `run` has the same shape and was not changed, since its code is shared with the 0.2.x
line. A primary-placed MCP server is started by `McpServer::connect_stdio` through
`Container::build_exec_argv` -- `podman exec -i` -- and spawned by `connect_stdio_cmd` with
`spawn_owned`, in outrig's group. `outrig run`'s REPL survives a Ctrl-C and returns to the
prompt, so after one, each in-container server should be gone and its tools failing. This is
inferred from the measurement and has not been reproduced through `run`.

## Shape

- Reproduce: `outrig run` with a primary-placed stdio server, `kill -INT -- -<pgid>` at the
  prompt, then a turn that calls the server.
- Start session-lifetime stdio exec clients -- `connect_stdio_cmd`'s, at least -- with
  `in_own_process_group`, so the REPL's own handling decides what a Ctrl-C does.

## Acceptance

- After a group SIGINT at `outrig run`'s prompt, a primary-placed server still answers.
