# `outrig mcp`

`outrig mcp` turns an outrig container-config into one MCP server for an external
client. It starts or attaches to the selected container, launches every
`[containers.<name>.mcp]` backing server inside it, and republishes their tools over this
process's stdio.

Use `outrig run` when you want outrig to be the LLM client: it resolves an agent,
builds a Rig agent, and opens the built-in REPL. Use `outrig mcp` when another
program is the LLM client -- Claude Code, Cursor, Zed, or any MCP-capable editor --
and you want that program to drive the tools inside your outrig container.

## Synopsis

```
outrig mcp [--container <name>]
           [--attach <session-id-or-container-name>]
           [--network <default|audit|filter>]
           [--session-dir <path>]
           [--config <path>]
           [--global-config <path>]
           [--session-root <path>]
           [--verbose]

outrig mcp show-merged [--container <name>]
                       [--attach <session-id-or-container-name>]

outrig mcp self
```

- `--container <name>` (default: `default-container`): selects a
  `[containers.<name>]` block. Required with `--attach <podman-name>`.
- `--attach <session-id-or-container-name>` (default: off): reuse an existing
  container instead of starting one.
- `--network <default|audit|filter>` (default: config `[network].mode`, else `default`):
  choose Podman's default networking, network audit logging, or global network filtering for
  this fresh session.
- `--session-dir <path>` (default: `<session-root>/<sid>`): writes to a known path.
- `--config <path>` (default: walks up from cwd): path to repo `config.toml`.
- `--global-config <path>` (default: `~/.outrig/config.toml`): path to global config.
- `--session-root <path>` (default: config, then XDG data directory): root for all sessions.
- `--verbose` (default: off): adds buildah/podman command transcripts to stderr and
  `container.log`.

`outrig mcp self` is different from the session MCP server. It does not resolve a repo config,
start a container, or create a session. It serves OutRig's own docs, schema, suggestions,
and advisory validators so an external AI tool can design a container-config. See
[AI-assisted design](ai-assisted-design.md).

There is no `--agent` flag. `outrig mcp` has no agent, so it never consults
`default-agent`, `agent.container`, `[agents]`, `[models]`, `[providers]`, or provider
API keys. Container selection is only:

1. `--container <name>`
2. top-level `default-container`

If neither is set, startup fails with:

```
error: no --container or default-container configured
```

With `--attach`, container-config selection is different:

1. If the attach value matches an exact session id under the resolved session root,
   outrig reuses that session row's `container_name` and `container_config_name`.
2. If `--container <name>` is also passed, it overrides the session row's
   `container_config_name`.
3. If the attach value is not a known session id, outrig treats it as a podman
   container name and requires `--container <name>`.

`--network audit` and `--network filter` are rejected with `--attach`; borrowed containers are
not retrofitted with a new interceptor.

The selected container must expose at least one backing MCP server after image
`/etc/outrig/container.toml` entries and `[containers.<name>.mcp]` overrides are merged. A
container-config with no merged entries has nothing to proxy, so `outrig mcp` exits before the
client sees an MCP `initialize` response.

## Minimal Config

`outrig mcp` can run from a container-only repo config:

```toml
default-container = "coding"

[workspace]
root = "."

[containers.coding]
dockerfile = ".agents/outrig/containers/coding/Dockerfile"
context    = ".agents/outrig/containers/coding"

[containers.coding.mcp]
fs    = ["mcp-server-filesystem", "/workspace"]
shell = ["bash", "-lc", "exec shell-mcp-command"]
```

The MCP server binaries still have to exist inside the image. Install them in the
container Dockerfile just as you would for `outrig run`. In the example above,
`shell-mcp-command` stands for whichever shell MCP server package you choose to install.

An image can also carry the same `[mcp]` table at `/etc/outrig/container.toml`.
Use that when a shared image owns the default tool set, then keep only repo-specific
overrides in `.agents/outrig/config.toml`. See
[Concepts -> MCP Servers](../concepts/mcp-servers.md#embedding-mcp-config-in-the-image).

To inspect the effective table without serving MCP:

```sh
outrig mcp show-merged --container coding
```

In fresh mode this starts the selected container, reads `/etc/outrig/container.toml`,
applies `config.toml` overrides, prints the merged `[mcp]` table to stdout, then stops
the container. With `--attach`, it borrows the existing container for the same read and
leaves it running.

## Attach Mode

Use attach mode when a container is already running and you want an external MCP client
to share its workspace, installed tools, and environment:

```sh
outrig mcp --attach 20260504T141907-a83f
outrig mcp --attach outrig-20260504T141907-a83f --container coding
```

The first form resolves an existing outrig session id. The second form borrows a
podman container directly, which is useful for containers not started by `outrig run`.

Attach mode shares the container, not the MCP protocol processes. The attacher starts
its own `podman exec -i` children for each merged MCP server, so the external client
has independent MCP state and independent stderr logs. Existing MCP children from the
host session keep running.

Every attach invocation writes its own fresh session row and log directory. `outrig logs
<attached-session> <server>` therefore works the same as it does for a fresh-container
`outrig mcp` session.

MCP servers used with attach mode should be reentrant-safe. Servers that bind a fixed
port, write a global pidfile, or take an exclusive lock can conflict with the host
session's copy or with another attacher.

## Client Configuration

Put `outrig` on `PATH`, or use an absolute path to the binary in each client config.
MCP clients may start servers with a different working directory than your shell, so
passing an absolute `--config` path is the least surprising setup.

Claude Code can add a stdio server from the command line:

```sh
claude mcp add --transport stdio outrig -- outrig mcp \
  --config /path/to/repo/.agents/outrig/config.toml \
  --container coding
```

Claude Code and Cursor both understand an `mcpServers` JSON shape. For Cursor, place
this in `.cursor/mcp.json` for project scope or `~/.cursor/mcp.json` for global scope.
For Claude Code project scope, use `.mcp.json` in the project root.

```json
{
  "mcpServers": {
    "outrig": {
      "type": "stdio",
      "command": "outrig",
      "args": [
        "mcp",
        "--config",
        "/path/to/repo/.agents/outrig/config.toml",
        "--container",
        "coding"
      ],
      "env": {}
    }
  }
}
```

Zed uses `context_servers` in its settings:

```json
{
  "context_servers": {
    "outrig": {
      "command": "outrig",
      "args": [
        "mcp",
        "--config",
        "/path/to/repo/.agents/outrig/config.toml",
        "--container",
        "coding"
      ],
      "env": {}
    }
  }
}
```

## What Happens, in Order

1. **Locate config.** Walks up from the current directory until
   `.agents/outrig/config.toml` is found, or fails. The MCP host's `cwd` therefore
   needs to be the repo, or pass `--config <path>` explicitly.
2. **Resolve container-config.** Uses `--container` if given, otherwise top-level
   `default-container`. There is no `agent.container` step -- this subcommand has no
   agent.
3. **Prepare the container.** Fresh mode builds or cache-hits the image and starts
   `podman run -d --rm --name outrig-<sid> ...`. Attach mode probes the existing
   container with `podman inspect`, verifies that it is running, and does not build,
   start, stop, or remove it.
4. **Start network interception, if enabled.** Fresh sessions can write
   `<session_dir>/logs/network.jsonl` and filter mode can enforce global policy; attach mode
   cannot install a new interceptor.
5. **Merge MCP config.** Read `/etc/outrig/container.toml` from the image if present,
   then overlay `[containers.<name>.mcp]` from config by server name.
6. **Connect MCP servers.** For each merged entry, `podman exec -i` the configured
   command and run the MCP `initialize` handshake.
7. **Build the proxy.** outrig advertises one merged tool list to its client, with
   each tool namespaced `<server>__<tool>`. See [Tool Names](#tool-names) below.
8. **Serve JSON-RPC over stdio.** rmcp's stdio transport reads JSON-RPC frames from
   the process stdin and writes responses to stdout. The proxy dispatches `tools/call`
   to the right backing server.

If anything before step 8 fails, `outrig mcp` prints the error on stderr and exits
non-zero without ever advertising a tool list.

## Startup Banner

After the image is ready, the container is running, and all backing MCP servers have
answered `tools/list`, `outrig mcp` prints one banner to stderr:

```
[outrig] container-config:  coding
[outrig] image:             outrig:coding-1f3a2b
[outrig] container started: outrig-coding-2026-05-04-a83f
[outrig] mcp fs:    initialized (3 tools)
[outrig] mcp shell: initialized (1 tool)
[outrig] tools available: fs__list_directory, fs__read_file, fs__write_file, shell__exec
[outrig] session id: 20260504T141907-a83f
[outrig] transport: stdio
[outrig] mcp server ready
```

Everything in that banner is on stderr. The client should treat stdout as protocol
bytes only.

Attach mode prints `container attached:` in the same position.

## Transport Discipline

`outrig mcp` serves MCP over stdio. Its stdout is reserved for JSON-RPC messages to
the client; all other process output goes somewhere else:

- startup banner: stderr
- outrig tracing controlled by `OUTRIG_LOG`, or `RUST_LOG` when unset: stderr
- top-level startup errors: stderr
- backing server stderr: `<session_dir>/logs/<server>.stderr`

This split is load-bearing. If a wrapper, shell hook, or debug print writes anything
non-JSON to stdout before or during the MCP exchange, the external client may fail
the handshake or drop the server.

## Tool Names

Outrig publishes tools under `<server>__<tool>` names. If the backing `fs` server
advertises a tool named `read_file`, the external client sees:

```
fs__read_file
```

Some MCP clients display that relationship as `fs.read_file` in their UI. The actual
tool name on the wire is still `fs__read_file`; outrig strips the `fs__` prefix and
dispatches the call to the original `read_file` tool on the `fs` backing server.

See [Concepts -> MCP Servers](../concepts/mcp-servers.md#tool-name-prefixing) for the
collision and sanitization rules.

## Lifecycle

`outrig mcp` has three graceful shutdown triggers:

- stdin EOF, which usually means the external MCP client disconnected
- SIGINT, such as Ctrl-C in the terminal that launched the process
- SIGTERM, such as a supervisor asking the process to stop

All three paths cancel the rmcp service, wait for the dispatcher to settle, shut down
each backing MCP server, and finalize the session record. Fresh-container mode then stops
the container. Attach mode leaves the borrowed container running.

If an attached host session stops the container while `outrig mcp --attach` is live, the
attacher cancels its proxy, shuts down its MCP children, finalizes its session with a
non-zero exit, and reports that the attached container stopped.

## Sessions and Logs

Every `outrig mcp` invocation creates a normal outrig session, including attach mode.
It appears in `outrig ls`, its backing-server stderr is readable with `outrig logs`, and
`outrig discard` removes it like any other session.

Because no agent participates, the in-memory session row has `agent_name = None`.
On disk, new `outrig mcp` session JSON omits `agent_name`; older JSON with
`"agent_name": null` still represents the same no-agent state when read.

```sh
$ outrig ls
ID                     STARTED              DURATION  CONTAINER  EXIT
20260504T141907-a83f   2026-05-04 14:19:07  0m44s     coding     0

$ outrig logs 20260504T141907-a83f fs
[mcp-server-filesystem] starting; root=/workspace
[mcp-server-filesystem] tools/list: 3 tools advertised
```

See [Sessions](sessions.md) for session-root resolution, `--session-dir`, and log
inspection.

## Future Work

The v0 surface is intentionally minimal. One transport extension is tracked but deferred:

- **HTTP / SSE transport** (`--listen <addr>`) -- run as a long-lived daemon serving
  multiple clients over TCP or Unix-socket MCP. v0 is stdio-only; one process per client.

Also deferred: exposing a tool-call audit log, proxying MCP `prompts/*` and
`resources/*`, surfacing backing-server stderr as MCP resources, and paginating
`tools/list`.

## See Also

- [outrig run](run.md) -- outrig as the LLM client and REPL.
- [Sessions](sessions.md) -- session records and server stderr logs.
- [Concepts -> MCP Servers](../concepts/mcp-servers.md) -- configuration and routing.
- [Reference -> CLI](../reference/cli.md) -- all flags and exit behavior.
