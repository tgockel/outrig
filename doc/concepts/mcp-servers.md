# MCP Servers

> **TODO: Incomplete** -- these examples describe outrig's intended behavior; the implementation
> isn't ready yet.

[MCP](https://modelcontextprotocol.io/) -- Model Context Protocol -- is the wire format outrig
uses to talk to tools. Each MCP server is a child process that runs inside your container and
speaks JSON-RPC over its stdio. outrig connects to each one by `podman exec -i`'ing into the
container, hands the resulting stdio pair to the [rmcp](https://crates.io/crates/rmcp) client, and
treats every tool the server advertises as a Rig dynamic tool.

## Declaring servers

MCP servers are configured per-container, as a map keyed by the server's local name:

```toml
[containers.coding.mcp]
fs    = { command = ["mcp-server-filesystem", "/workspace"] }
shell = ["bash", "-lc", "exec mcp-server-shell"]
build = { command = ["cargo-mcp"], env = { CARGO_HOME = "/workspace/.cargo" } }
```

Each entry is one of:

- A **bare array** (short form): `["bin", "arg1", ...]`. Equivalent to
  `{ command = ["bin", "arg1", ...] }` with no extra env.
- A **table** (full form): `{ command = [...], env = { KEY = "value", ... } }`. The `env` map is
  added to the `podman exec` invocation for this server.

Server names must match `^[a-zA-Z][a-zA-Z0-9_-]*$` and must be unique within a container-config.
Names are how you reference servers elsewhere -- in `outrig logs <session> <server>`, in tool-call
traces, in the prefix that gets attached to every tool the server exposes.

## Lifecycle

When `outrig run` starts, the sequence per MCP server is:

1. `podman exec -i <container> <command>` -- outrig spawns the server as a child process inside
   the running container, with stdin/stdout piped back to outrig.
2. **Initialize handshake** -- outrig sends the MCP `initialize` request and reads the server's
   capabilities.
3. **Discover tools** -- outrig calls `tools/list` and receives the list of advertised tools, each
   with a name, description, and JSON Schema for its inputs.
4. **Register with Rig** -- each discovered tool becomes a `McpToolAdapter` that implements Rig's
   dynamic-tool trait. The agent now has access to it.

All servers come up before the REPL accepts any input. If any server fails to initialize,
`outrig run` reports the error on stderr and exits before the REPL starts -- you don't get partial
sandboxes.

When the REPL terminates (Ctrl-D, Ctrl-C, or LLM error), outrig closes each server's stdin in
turn, waits up to 5 seconds for the process to exit, then `podman stop`'s the container. Servers
don't see SIGTERM directly; they see EOF on stdin, which the MCP spec defines as the normal
shutdown signal.

## Tool name prefixing

Two different MCP servers can have a tool with the same name (e.g. both `fs` and `archive` might
define `read_file`). To avoid collisions and keep the LLM's tool-name space predictable, outrig
**always** prefixes every tool with its server name:

```
fs__list_directory
fs__read_file
fs__write_file
shell__exec
build__cargo_check
```

The separator is `__` (double underscore). The combined name is sanitized to fit OpenAI's
`^[a-zA-Z0-9_-]{1,64}$` constraint -- non-matching characters become `_`, and over-long names get
truncated with a stable hash suffix.

The LLM sees `fs__write_file` in its tool list and emits tool calls under that name. outrig's
router strips the prefix and dispatches to the correct MCP client with the original tool name.

You can list every tool currently registered with the agent from inside the REPL:

```
> /tools
[outrig] tools available (4):
  fs__list_directory   List the contents of a directory.
  fs__read_file        Read the contents of a file.
  fs__write_file       Write contents to a file (overwrites).
  shell__exec          Run a shell command and return its stdout/stderr.
> 
```

## What if a server crashes mid-session?

A crashed MCP server is surfaced as a tool-call error to the LLM, which usually causes the model
to stop calling that tool and tell you about the failure. outrig does not auto-restart MCP servers
in v0 -- the server is gone for the rest of the session.

> **TODO: Incomplete** -- auto-restart and per-server health-checking are deferred.

The server's stderr, captured to `<session_dir>/logs/<server>.stderr`, usually has the actual
error. See [Sessions](../usage/sessions.md) for how to view it.

## Picking which servers to include

Two practical guidelines:

- **Match servers to the container-config's purpose.** A `planning` container probably doesn't
  need `shell`. A `coding` container almost certainly needs both `fs` and `shell`.
- **Fewer servers is better when it's enough.** Every server is another initialize cost at
  startup, another tool list cluttering the LLM's prompt, another process to monitor. If a single
  MCP server covers your needs, use one.

## See also

- [Containers](containers.md) -- the Dockerfile that has to install the server binaries.
- [Sessions](../usage/sessions.md) -- `outrig logs <session> <server>` for stderr.
- [Reference -> Config](../reference/config.md) -- full schema for the `[containers.<name>.mcp]`
  block.
