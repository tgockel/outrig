# `outrig mcp`

`outrig mcp` turns an outrig image-config into one MCP server for an external
client. It starts or attaches to the selected container, launches every
`[images.<name>.mcp]` backing server inside it, and republishes their tools over this
process's stdio by default, or over Streamable HTTP when `--listen` is set.

Use `outrig run` when you want outrig to be the LLM client: it resolves an agent,
builds a Rig agent, and opens the built-in REPL. Use `outrig mcp` when another
program is the LLM client -- Claude Code, Cursor, Zed, or any MCP-capable editor --
and you want that program to drive the tools inside your outrig container.

## Synopsis

```
outrig mcp [--image <name-or-local-ref>]
           [--attach <session-id-or-container-name>]
           [--listen <addr>]
           [--network <default|audit|filter>]
           [--session-dir <path>]
           [--config <path>]
           [--global-config <path>]
           [--session-root <path>]
           [--volume <host:container[:ro|rw]>]
           [--verbose]

outrig mcp show-merged [--image <name-or-local-ref>]
                       [--attach <session-id-or-container-name>]

outrig mcp self
```

- `--image <name-or-local-ref>` (default: `default-image`): selects a
  `[images.<name>]` block. If an explicit value does not match config, it is
  treated as a local Podman image ref and is never pulled. Required with
  `--attach <podman-name>`.
- `--attach <session-id-or-container-name>` (default: off): reuse an existing
  container instead of starting one.
- `--listen <addr>` (default: off): serve Streamable HTTP at `/mcp` instead of
  stdio. Accepts TCP socket addresses such as `127.0.0.1:7331` or
  `0.0.0.0:7331`, plus Unix sockets as `unix:/tmp/outrig.sock`.
- `--network <default|audit|filter>` (default: config `[network].mode`, else `default`):
  choose Podman's default networking, network audit logging, or global network filtering for
  this fresh session.
- `--session-dir <path>` (default: `<session-root>/<sid>`): writes to a known path.
- `--config <path>` (default: walks up from cwd; if not found, run config-less): path to repo
  `config.toml`.
- `--global-config <path>` (default: `~/.outrig/config.toml`): path to global config.
- `--session-root <path>` (default: config, then XDG data directory): root for all sessions.
- `--volume <host:container[:ro|rw]>` (repeatable): bind an extra host directory into the
  container, on top of the default workspace mount. Read-only unless `:rw` is given; the host
  directory must exist. Rejected with `--attach`.
- `--verbose` (default: off): adds buildah/podman command transcripts to stderr and
  `container.log`.

`outrig mcp self` is different from the session MCP server. It does not resolve a repo config,
start a container, or create a session. It serves OutRig's own docs, schema, suggestions,
and advisory validators so an external AI tool can design an image-config. See
[AI-assisted design](ai-assisted-design.md).

There is no `--agent` flag. `outrig mcp` has no agent, so it never consults
`default-agent`, `agent.image`, `[agents]`, `[models]`, `[providers]`, or provider
API keys. Image selection is only:

1. `--image <name-or-local-ref>`
2. top-level `default-image`

If neither is set, startup fails with:

```
error: no --image or default-image configured
```

`default-image` must name a config block. The raw local-image fallback applies
only to explicit `--image` values and to raw image refs saved in session records.

`outrig mcp` can also run in a directory with no `.agents/outrig/config.toml` (and no
`--config`): it uses the current directory as the workspace root and merges in the global
config. Since there is no agent, all you need is `--image <local-ref>`; the proxied MCP servers
come from the image's `org.outrig.mcp` labels.

With `--attach`, image-config selection is different:

1. If the attach value matches an exact session id under the resolved session root,
   outrig reuses that session row's `container_name` and `image_config_name`.
2. If `--image <name-or-local-ref>` is also passed, it overrides the session row's
   `image_config_name`.
3. If the attach value is not a known session id, outrig treats it as a podman
   container name and requires `--image <name-or-local-ref>`.
4. If the session row predates `image_config_name` there is nothing to inherit, so
   `--image <name-or-local-ref>` is required there too.

`--network audit` and `--network filter` are rejected with `--attach`; borrowed containers are
not retrofitted with a new interceptor.

The selected image must expose at least one backing MCP server after image
`org.outrig.mcp` label entries and `[images.<name>.mcp]` overrides are merged. Repo-local
build cache images already stamp that merged table into their labels on cache misses, but startup
still applies the same merge. An image with no merged entries has nothing to proxy, so `outrig mcp`
exits before the client sees an MCP `initialize` response.

## Minimal Config

`outrig mcp` can run from an image-only repo config:

```toml
default-image = "coding"

[workspace]
root = "."

[images.coding]
dockerfile = ".agents/outrig/images/coding/Dockerfile"
context    = ".agents/outrig/images/coding"

[images.coding.mcp]
fs    = ["mcp-server-filesystem", "/workspace"]
shell = ["bash", "-lc", "exec shell-mcp-command"]
```

The MCP server binaries still have to exist inside the image. Install them in the
container Dockerfile just as you would for `outrig run`. In the example above,
`shell-mcp-command` stands for whichever shell MCP server package you choose to install.

An image can also carry the same `[mcp]` table in its `org.outrig.mcp` OCI label.
Use that when a shared image owns the default tool set, then keep only repo-specific
overrides in `.agents/outrig/config.toml`. See
[Concepts -> MCP Servers](../concepts/mcp-servers.md#embedding-mcp-config-in-the-image).

To inspect the effective table without serving MCP:

```sh
outrig mcp show-merged --image coding
```

In fresh mode this starts the selected container, reads the `org.outrig.mcp` label off the
primary image and every named sidecar image, applies `config.toml` overrides, prints the merged
`[mcp]` table to stdout, then stops the container. Sidecar containers are not launched for the
read. Each server carries a comment naming its placement and provenance:

```toml
[mcp]
# fs: primary (image label org.outrig.mcp)
fs = ["mcp-server-filesystem", "/workspace"]
# search: sidecar "tools" (config.toml)
search = { command = ["mcp-search"], sidecar = "tools" }
```

For repo-local build images, the cache tag was already stamped with that merged label when it
was built (placement-bearing entries excluded -- they belong to other containers). With
`--attach`, it borrows the existing container for the same read and leaves it running.

## Attach Mode

Use attach mode when a container is already running and you want an external MCP client
to share its workspace, installed tools, and environment:

```sh
outrig mcp --attach 20260504T141907-a83f
outrig mcp --attach outrig-20260504T141907-a83f --image coding
```

The first form resolves an existing outrig session id. The second form borrows a
podman container directly, which is useful for containers not started by `outrig run`.

Attach mode cannot own containers, so a config that declares sidecars (or places MCP
entries in one) is rejected with `--attach`; start a fresh session instead.

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
  --image coding
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
        "--image",
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
        "--image",
        "coding"
      ],
      "env": {}
    }
  }
}
```

### Streamable HTTP

Use `--listen` when you want one long-lived `outrig mcp` process that multiple
MCP clients can connect to:

```sh
outrig mcp --listen 127.0.0.1:7331 --image coding
```

The MCP endpoint is `/mcp`, so clients should connect to:

```text
http://127.0.0.1:7331/mcp
```

Loopback TCP is the intended default deployment shape. Binding `0.0.0.0:7331`
or any other non-loopback address is allowed, but `outrig` prints a warning because
v1 has no built-in authentication and anything that can reach the port can call the
container's tools. Put an authenticated reverse proxy in front if you expose it beyond
the local machine.

For local multi-process access without a TCP port, use a Unix socket:

```sh
outrig mcp --listen unix:/tmp/outrig.sock --image coding
```

Socket filesystem permissions are the access boundary. HTTP clients still use the
Streamable HTTP protocol and the `/mcp` path over that socket.

## What Happens, in Order

1. **Locate config.** Walks up from the current directory until
   `.agents/outrig/config.toml` is found, or fails. The MCP host's `cwd` therefore
   needs to be the repo, or pass `--config <path>` explicitly.
2. **Resolve image.** Uses explicit `--image` first. Config entries win; an
   unknown explicit value is treated as a local Podman image ref. Without
   explicit `--image`, top-level `default-image` still names a config block.
3. **Prepare the container.** Fresh mode builds, cache-hits, pulls configured
   `image-name` refs, or probes raw local refs, then starts
   `podman run -d --rm --name outrig-<sid> ...`. Attach mode probes the existing
   container with `podman inspect`, verifies that it is running, and does not build,
   start, stop, or remove it.
4. **Merge MCP config and start sidecars.** Read the primary image's `org.outrig.mcp`
   label if present, then overlay `[images.<name>.mcp]` from config by server name. Each
   declared sidecar's image is resolved, its label merged (scoped to that sidecar), and
   `start = "auto"` sidecars are started as `outrig-<sid>-<sc>`. An entrypoint-stdio
   sidecar (inline `image`, no `command`) is instead created and initialized with its
   ENTRYPOINT held un-executed, env baked in via `podman create --env`. Sidecar failures
   follow the block's `on-failure` key. A `start = "manual"` sidecar stays planned but
   unstarted -- its servers are skipped with a notice. `outrig mcp` has no mid-session
   start surface; manual sidecars are started from the `outrig run` REPL
   (`/sidecar add <name>`) or the library API.
5. **Start network interception, if enabled.** The interceptor attaches to the primary
   and every sidecar -- including created-but-not-started entrypoint sidecars, whose
   first packet is therefore already subject to policy. Fresh sessions can write
   `<session_dir>/logs/network.jsonl` and filter mode can enforce global policy; attach mode
   cannot install a new interceptor.
6. **Connect MCP servers.** For each merged entry, `podman exec -i` the configured
   command in the container its placement names -- or, for entrypoint-stdio servers,
   `podman start --attach --interactive` the held container, whose lifetime now equals
   the server's -- and run the MCP `initialize` handshake.
7. **Build the proxy.** outrig advertises one merged tool list to its client, with
   each tool namespaced `<server>__<tool>`. See [Tool Names](#tool-names) below.
8. **Serve MCP.** Without `--listen`, rmcp's stdio transport reads JSON-RPC frames
   from the process stdin and writes responses to stdout. With `--listen`, rmcp's
   Streamable HTTP service accepts POST/SSE traffic at `/mcp`. The proxy dispatches
   `tools/call` to the right backing server in both modes.

If anything before step 8 fails, `outrig mcp` prints the error on stderr and exits
non-zero without ever advertising a tool list.

## Startup Banner

After the image is ready, the container is running, and all backing MCP servers have
answered `tools/list`, `outrig mcp` prints one banner to stderr:

```
[outrig] image-config:  coding
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
bytes only for stdio transport.

Attach mode prints `container attached:` in the same position.

With `--listen`, the banner says `transport: streamable-http`, then prints the bound
endpoint before the ready line:

```text
[outrig] transport: streamable-http
[outrig] listen: http://127.0.0.1:7331/mcp
[outrig] mcp server ready
```

## Transport Discipline

By default, `outrig mcp` serves MCP over stdio. Its stdout is reserved for JSON-RPC
messages to the client; all other process output goes somewhere else:

- startup banner: stderr
- outrig tracing controlled by `OUTRIG_LOG`, or `RUST_LOG` when unset: stderr
- top-level startup errors: stderr
- backing server stderr: `<session_dir>/logs/<server>.stderr`

This split is load-bearing. If a wrapper, shell hook, or debug print writes anything
non-JSON to stdout before or during the MCP exchange, the external client may fail
the handshake or drop the server.

With `--listen`, stdout is not the protocol channel, but `outrig` keeps the same
stderr-first discipline so wrapper behavior remains predictable.

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

- stdio stdin EOF, which usually means the external MCP client disconnected
- SIGINT, such as Ctrl-C in the terminal that launched the process
- SIGTERM, such as a supervisor asking the process to stop

All paths cancel the rmcp service, wait for the dispatcher to settle, shut down each
backing MCP server, and finalize the session record. Fresh-container mode then stops the
containers -- sidecars first, primary last. Attach mode leaves the borrowed container
running.

Sessions with sidecars also watch the primary container: if it dies out from under outrig
(a manual `podman kill`, the OOM killer), the sidecars are reaped and the process exits
non-zero. A sidecar dying mid-session only degrades the tool set -- its tools return
errors and the session continues.

HTTP/SSE mode is daemon-shaped: client disconnects close only that MCP session. The
`outrig mcp --listen` process stays alive until SIGINT, SIGTERM, or attached-container
shutdown.

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
ID                     STARTED              DURATION  IMAGE      EXIT
20260504T141907-a83f   2026-05-04 14:19:07  0m44s     coding     0

$ outrig logs 20260504T141907-a83f fs
[mcp-server-filesystem] starting; root=/workspace
[mcp-server-filesystem] tools/list: 3 tools advertised
```

See [Sessions](sessions.md) for session-root resolution, `--session-dir`, and log
inspection.

## Future Work

The v0 HTTP surface is intentionally unauthenticated and minimal. Deferred work includes
adding built-in auth, exposing a tool-call audit log, proxying MCP `prompts/*` and
`resources/*`, surfacing backing-server stderr as MCP resources, and paginating
`tools/list`.

## See Also

- [outrig run](run.md) -- outrig as the LLM client and REPL.
- [Sessions](sessions.md) -- session records and server stderr logs.
- [Concepts -> MCP Servers](../concepts/mcp-servers.md) -- configuration and routing.
- [Reference -> CLI](../reference/cli.md) -- all flags and exit behavior.
