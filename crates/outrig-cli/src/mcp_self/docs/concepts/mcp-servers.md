# MCP Servers

[MCP](https://modelcontextprotocol.io/) -- Model Context Protocol -- is the wire format outrig
uses to talk to tools. Each MCP server is a child process that runs inside a session container
and speaks JSON-RPC over its stdio. outrig connects to each one by `podman exec -i`'ing into the
container, hands the resulting stdio pair to the [rmcp](https://crates.io/crates/rmcp) client, and
treats every tool the server advertises as a Rig dynamic tool.

By default a server runs in the session's primary workspace container. An entry can instead opt
into a **sidecar** -- an extra container owned by the session -- so the server's runtime
dependencies stay out of the workspace image and the server sees only what its container is
granted. See [Sidecar placement](#sidecar-placement) below and
[Containers](containers.md#sidecar-containers) for the container-side details.

The same `[images.<name>.mcp]` table is consumed by both `outrig run` and
`outrig mcp`. `outrig run` registers those tools with its built-in agent; `outrig mcp`
republishes them as one stdio MCP server for an external client. See
[Usage -> outrig mcp](../usage/mcp.md) for client setup and transport rules.
When `outrig mcp --attach` points at an existing container, it still starts its own
MCP child processes inside that container; it does not share or proxy the host
session's existing MCP protocol state.

## Declaring servers

MCP servers are configured per-image, as a map keyed by the server's local name:

```toml
[images.coding.mcp]
fs    = { command = ["mcp-server-filesystem", "/workspace"] }
shell = ["bash", "-lc", "exec shell-mcp-command"]
build = { command = ["cargo-mcp"], env = { CARGO_HOME = "/workspace/.cargo" } }
```

`shell-mcp-command` is a placeholder for the shell MCP package you choose and install in the
image. OutRig supports arbitrary MCP commands; `outrig image add` only renders package recipes
for the MCP servers it can install without more input.

Each entry is one of:

- A **bare array** (short form): `["bin", "arg1", ...]`. Equivalent to
  `{ command = ["bin", "arg1", ...] }` with no extra env.
- A **table** (full form): `{ command = [...], env = { KEY = "value", ... } }`. The `env` map is
  added to the `podman exec` invocation for this server.

Server names must match `^[a-zA-Z][a-zA-Z0-9_-]*$` and must be unique within an image-config.
Names are how you reference servers elsewhere -- in `outrig logs <session> <server>`, in tool-call
traces, in the prefix that gets attached to every tool the server exposes.

## Sidecar placement

A full-form entry can name the container it runs in:

```toml
[sidecars.tools]
image     = "mcp-tools"        # sibling [images.mcp-tools] block first, else raw podman ref
workspace = "ro"               # "none" (default) | "ro" | "rw"

[sidecars.serve]
image     = "docker.io/mcp/filesystem:latest"
args      = ["/workspace"]     # argv for the image's ENTRYPOINT
workspace = "ro"

[images.dev.mcp]
# Runs in the primary container, exactly as before.
local = ["mcp-local", "--stdio"]
# exec-stdio in the named sidecar "tools".
fs    = { command = ["mcp-fs", "/workspace"], sidecar = "tools" }
# exec-stdio in a dedicated anonymous sidecar built just for this server.
grep  = { command = ["mcp-grep"], image = "ghcr.io/example/mcp-grep:1" }
# entrypoint-stdio: no command; the image ENTRYPOINT is the server.
fetch = { image = "ghcr.io/example/mcp-fetch:2", env = { TOKEN = "${FETCH_TOKEN}" } }
# entrypoint-stdio in the named sidecar "serve", which supplies the argv.
ws    = { sidecar = "serve" }
```

The placement shapes:

- **Primary (default).** No placement key. The short form (bare array) always runs in the
  primary.
- **Named sidecar:** `sidecar = "<sc>"` names a top-level `[sidecars.<sc>]` block; with a
  `command`, the server is `podman exec`'d in that container. Naming it is also what starts
  it -- blocks are shared, so a session only runs the ones its `[mcp]` entries reference.
- **Anonymous sidecar:** `image = "<ref>"` plus `command` gives this one server a dedicated
  container with all defaults (no workspace, no mounts, `on-failure = "abort"`). Anything
  fancier -- workspace access, mounts, security -- requires promoting to a named block.
  `sidecar` and `image` are mutually exclusive.
- **Entrypoint sidecar:** an entry *without* a `command` runs its container's ENTRYPOINT as the
  server over piped stdio -- the off-the-shelf MCP image pattern, zero repo-side command
  knowledge. Container lifetime equals server lifetime: the server exiting removes the
  container, surfacing exactly like a mid-session sidecar death (tools error, session
  survives). Two ways to write it:
  - `image = "<ref>"` with no `command` -- the one-liner, all defaults.
  - `sidecar = "<sc>"` with no `command` -- the named block becomes the entrypoint host, which
    is how such a server gets a workspace view, mounts, or its own security policy.

Every one of these is reachable from the library API as well as from a config file. A
`SidecarSpec` carries the same `workspace`, `view`, mounts, and security a `[sidecars.<sc>]`
block does, and hosts either exec-stdio servers (`with_server`) or a single entrypoint-stdio
one (`with_entrypoint_server`, whose `args` become the container argv).
`LaunchSpec::from_config` lowers a parsed config into those same specs, so an embedding
program and `outrig run` reach the same placements and produce the same containers.

An entrypoint server's arguments come from `args`, on the entry or on its sidecar block --
never both. Without it, `docker.io/mcp/filesystem` and most real MCP images cannot be reached
at all, since they take their served directories positionally:

```toml
[images.dev.mcp]
serve = { image = "docker.io/mcp/filesystem:latest", args = ["/workspace"] }
```

`args` is for images whose server is an `ENTRYPOINT`. podman appends trailing arguments to an
exec-form `ENTRYPOINT`, so `["node", "/app/dist/index.js"]` plus `args = ["/workspace"]` runs
`node /app/dist/index.js /workspace`. But it *replaces* `CMD` -- an image that puts its server
in `CMD` instead loses it, and nothing starts.

Because the container process is the server, an entrypoint host serves exactly one server and
cannot be `start = "manual"`. It also skips the in-container user bootstrap: that runs over
`podman exec`, and there is no window for it between the container's creation and the attach
that runs the entrypoint. The image's own `USER` therefore applies to any `workspace` or
`mounts` the block declares. `view = "primary"` is the exception -- see below: its payload runs
as the session user whatever the image says, because OutRig's own launcher is the process that
starts it.

Entrypoint sidecars never race session network policy: the container is created and initialized
with its entrypoint held un-executed, audit/filter interception attaches to its network
namespace, and only then does the entrypoint run. Its first packet is already subject to policy.

### Primary filesystem view

An entrypoint sidecar can go one step further and run against the **primary container's
filesystem view** with `view = "primary"` -- the primary's rootfs and every mount, at the
primary's paths, with the sidecar image supplying its own runtime:

```toml
[images.dev.mcp]
# The quickstart one-liner: an off-the-shelf filesystem server over the primary's view.
fs = { image = "docker.io/mcp/filesystem:latest", view = "primary", args = ["/workspace"] }
```

The sidecar's `outrig-enter` entrypoint joins the primary's mount namespace and grafts the
sidecar's own rootfs aside at `/mnt`, then execs the image's server. This is what lets an
unmodified `docker.io/mcp/filesystem` (Alpine) index a Debian project's tree -- neither image
knows about the other. `view = "primary"` is entrypoint-stdio only, and mutually exclusive with
`workspace` (the view already contains it).

**The argument asymmetry.** Because OutRig rebuilds the server's command line, two kinds of path
in it mean different things:

- Elements from the **sidecar image** -- its `ENTRYPOINT` and `CMD` -- name files in the
  sidecar's own rootfs, now under the graft. OutRig prefixes absolute ones with `/mnt`. The
  program itself is the exception: `outrig-enter` opens it *before* joining the primary's
  namespace, while the sidecar's own rootfs is still at `/`, so it stays bare and the helper
  applies the graft itself when handing the path to the loader.
- Elements **you wrote in `args`** name paths in the *primary's* view. They are passed bare.

So `args = ["/workspace"]` against an image whose entrypoint is `/usr/local/bin/node
/app/dist/index.js` runs `/usr/local/bin/node /mnt/app/dist/index.js /workspace` -- the tool
from the sidecar, the directory from the primary.

`view = "primary"` is a real posture change: the container starts with `CAP_SYS_ADMIN` and
`CAP_SYS_PTRACE` in the primary's user namespace, and the server can read the primary's whole
filesystem. See [MCP Trust Model](mcp-trust-model.md) and `SECURITY.md`. It needs the
`outrig-enter` helper, compiled when the `<arch>-unknown-linux-musl` Rust target is installed;
without it a `view = "primary"` session fails at start with a message naming the missing
artifact.

**The capabilities belong to the launcher, not to the server.** `outrig-enter` needs them for
the namespace join and the graft, and gives them up the moment that work is done: it becomes the
session's uid/gid immediately before exec'ing the payload, which clears its capability sets with
the uid transition. The server -- and anything it shells out to -- therefore runs as the same
user every other OutRig-launched server runs as, so what it writes into the workspace is yours
rather than a subuid you cannot chown back, and a root-owned path in the primary is out of
reach. The image's `USER` does not enter into it.

That posture is identical from the library. `SidecarSpec::with_view(SidecarView::Primary)`
takes the same capabilities in the same namespace, drops to the same ids, and a build without
the helper fails the same way, before any container is created. The helper is materialized into
the session's log directory, which is the one writable location a `LaunchSpec` names.

Exec-stdio named sidecars honor their image's `org.outrig.mcp` label with the usual semantics,
scoped to that sidecar: label-declared servers materialize as exec-stdio servers *in that
sidecar*, and repo config overrides by server name (the whole entry, placement included). The
label adds servers to a sidecar an `[mcp]` entry already started; it cannot start one itself.
Labels are inert on anonymous sidecars and on entrypoint hosts -- exactly the one declaring
server runs there. Labels may not carry `args` for the same reason they may not carry placement
keys: they describe exec-stdio servers in the image that carries them, whose arguments belong
in `command`.

The server-name namespace stays flat per session, across the primary and every sidecar. If two
sidecar images both advertise the same name and neither is overridden, session start fails with
an error naming both hosts; add an override by name in `[images.<name>.mcp]` to pick one.

A sidecar that fails to start, bootstrap, or connect any of its servers is handled per its
`on-failure` key: `abort` (the default) fails the session start; `warn` logs to stderr, skips
the sidecar and every server it hosts, and continues with a reduced tool set. Primary-container
and primary-hosted-server failures always fail fast.

## Embedding MCP config in the image

An image can also carry its MCP declarations in its `org.outrig.mcp` OCI label. You author them
as a `[mcp]` table in the project's `image.toml`, which `outrig image build` serializes into the
label:

```toml
# image.toml
[mcp]
fs    = { command = ["mcp-server-filesystem", "/workspace"] }
shell = ["bash", "-lc", "exec shell-mcp-command"]
build = { command = ["cargo-mcp"], env = { CARGO_HOME = "/workspace/.cargo" } }
```

The `[mcp]` table uses the same short and full entry shapes as
`[images.<name>.mcp]`. At session startup, outrig reads the `org.outrig.mcp`
label off the image, then overlays entries from `config.toml`. If both sources
define the same server name, the `config.toml` entry replaces the image entry in
full; fields are not deep-merged. Servers that appear in only one source remain
in the merged set.

For build-from-Dockerfile repo images, `outrig build` also stamps the cache image with the same
merged `org.outrig.mcp` label it would use at startup. The startup overlay still runs, but is
idempotent for those repo-local entries. This means `outrig image inspect <name>:<hash>` can show
the declared servers without starting a container.

Use embedded MCP config when a shared image owns the tool binaries and their
default commands. Use `config.toml` for repo-local additions or overrides. A
repo that wants to delegate completely to the image can omit
`[images.<name>.mcp]`.

The `org.outrig.mcp` label is not required; this allows a shared image
to be used with different configurations via `config.toml`. Malformed JSON,
invalid server names, and empty command arrays are startup errors because they
mean the image metadata is broken.

To inspect what will actually start, run:

```sh
outrig mcp show-merged --image coding
```

The command starts the selected container, reads the `org.outrig.mcp` label off the primary
image and every named sidecar image, applies `config.toml` overrides, prints the effective
`[mcp]` table to stdout (without launching sidecar containers), and then stops the container.
Each server carries a comment naming its placement and where it was declared:

```toml
[mcp]
# fs: primary (image label org.outrig.mcp)
fs = ["mcp-server-filesystem", "/workspace"]
# search: sidecar "tools" (config.toml)
search = { command = ["mcp-search"], sidecar = "tools" }
```

## Lifecycle

When `outrig run` starts, the primary container comes up first, then every `start = "auto"`
sidecar, then network interception attaches to each container, and only then do MCP servers
connect -- primary-hosted and sidecar-hosted alike, in name order. The sequence per MCP server
is:

1. `podman exec -i <container> <command>` -- outrig spawns the server as a child process inside
   its placement's running container, with stdin/stdout piped back to outrig.
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
turn, waits up to 5 seconds for the process to exit, then stops the containers -- sidecars
first, primary last. Servers don't see SIGTERM directly; they see EOF on stdin, which the MCP
spec defines as the normal shutdown signal.

In attach mode, `outrig mcp --attach` borrows a container that something else owns. It
shuts down only the MCP children it started and leaves the borrowed container running.
If the owner stops the container while the attacher is live, the attacher exits instead
of trying to relaunch it.

## Dynamic addition

Sidecars can also join a session after it starts. The primitive is the library API:
`Outrig::add_sidecar(SidecarSpec)` accepts an arbitrary spec -- a raw podman image ref, a
workspace view, a filesystem view, mounts, a security block, and either exec-stdio servers or
an entrypoint-stdio one -- starts the container with session labels and keep-id, attaches the
session's network interceptor, connects the servers, and extends `Outrig::tools`.
`LaunchSpec::with_sidecar(SidecarSpec)` declares the same thing at launch time.

The two transports differ in what "starts the container" means. An exec-stdio sidecar is
started and then exec'd into, so it outlives any one server. An entrypoint-stdio sidecar is
created with its server's environment baked in and only runs when the server does, so adding
one mid-session gives the session a container whose lifetime is that server's -- the same
coupling a config-declared entrypoint host has.

The REPL surface is narrower by design: `/sidecar add <name>` starts a sidecar the image config
declared with `start = "manual"`, and nothing else -- unknown names and already-running sidecars
are errors, and there is no `/sidecar stop`. New tools become available to the agent on the next
turn. `/sidecar list` shows every declared sidecar with its status (`running`, `not started`,
`exited`) and the servers it hosts.

Dynamic-add failures are reported only to the caller -- the REPL prints the error, the library
returns `Err` -- and the session stays healthy: anything the failed add started (container,
interceptor attachment, connected servers) is torn down, and the existing tool set is untouched.
This is deliberately different from the session-start `on-failure` policy, which decides whether
a *launch-time* sidecar failure aborts the session.

There is intentionally no agent-invocable tool for adding sidecars: the agent must not grow its
own environment. Only the human at the REPL or the embedding program can.

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

A sidecar container that dies mid-session behaves the same way, uniformly and regardless of its
`on-failure` setting: outrig logs the death to stderr, every tool the sidecar hosted returns
errors, and nothing restarts. If the *primary* container dies out from under outrig (a manual
`podman kill`, the OOM killer), outrig reaps every sidecar and ends the session with an error.

> **TODO: Incomplete** -- auto-restart and per-server health-checking are deferred.

The server's stderr, captured to `<session_dir>/logs/<server>.stderr`, usually has the actual
error. See [Sessions](../usage/sessions.md) for how to view it.

Attach mode can run more than one copy of the same MCP server in one container. Prefer
servers that are reentrant-safe: no fixed listening port, global pidfile, or exclusive
lock unless the server is explicitly designed to coordinate multiple copies. If a server
cannot run twice, the second copy should fail clearly during startup and its stderr log
will show the underlying conflict.

## Picking which servers to include

Two practical guidelines:

- **Match servers to the image-config's purpose.** A `planning` image probably doesn't
  need `shell`. A `coding` image almost certainly needs both `fs` and `shell`.
- **Fewer servers is better when it's enough.** Every server is another initialize cost at
  startup, another tool list cluttering the LLM's prompt, another process to monitor. If a single
  MCP server covers your needs, use one.

## See also

- [Containers](containers.md) -- the Dockerfile that has to install the server binaries.
- [MCP Trust Model](mcp-trust-model.md) -- the container boundary that makes broad MCP tools
  practical.
- [AI-assisted design](../usage/ai-assisted-design.md) -- use `outrig mcp self` to design custom
  MCP-enabled image-configs.
- [Sessions](../usage/sessions.md) -- `outrig logs <session> <server>` for stderr.
- [Reference -> Config](../reference/config.md) -- full schema for the `[images.<name>.mcp]`
  block.
