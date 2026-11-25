# MCP Servers

[MCP](https://modelcontextprotocol.io/) -- Model Context Protocol -- is one of the ways an agent
reaches tools. `outrig run`'s agent acts by calling the tools of the MCP servers an image-config
declares, and `outrig mcp` serves the same tools to an external client. `outrig run-new`'s agent
acts by writing Python instead (see
[Python Runtime](https://tgockel.github.io/outrig/concepts/python-runtime.html)): its session
starts the same servers as **MCP services**, each a Python object agent code calls, under rules of
its own -- see [In a session: MCP services](#in-a-session-mcp-services). Elsewhere on this page,
"the agent" is `outrig run`'s, and a turn is one prompt and the agent's work on it.

Each MCP server is a child process that runs inside a session container and speaks JSON-RPC over
its stdio. outrig connects to each one by `podman exec -i`'ing into the container, hands the
resulting stdio pair to the [rmcp](https://crates.io/crates/rmcp) client, and treats every tool
the server advertises as a Rig dynamic tool.

By default a server runs in the session's primary workspace container. An entry can instead opt
into a **sidecar** -- an extra container owned by the session -- so the server's runtime
dependencies stay out of the workspace image and the server sees only what its container is
granted. See [Sidecar placement](#sidecar-placement) below and
[Containers](containers.md#sidecar-containers) for the container-side details.

The same `[images.<name>.mcp]` table is consumed by `outrig run`, `outrig mcp` and
`outrig run-new`. `outrig run` registers those tools with its built-in agent; `outrig mcp`
republishes them as one stdio MCP server for an external client; `outrig run-new` binds each
server to its session as a service. See
[Usage -> outrig mcp](https://tgockel.github.io/outrig/usage/mcp.html) for client setup and
transport rules.
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
  added to the `podman exec` invocation for this server. A value containing `${VAR}` references is
  passed by name,
  off podman's command line, and outrig shows it only as the reference; see
  [the value syntax](../reference/config.md#mcp-env-value-syntax). In a repo config a `${VAR}`
  may name only a variable the global config lists in
  [`[repo]` `env-allow`](../reference/config.md#repo).

Server names must match `^[a-zA-Z][a-zA-Z0-9_-]*$` and must be unique within an image-config.
Names are how you reference servers elsewhere -- in `outrig logs <session> <server>`, in tool-call
traces, in the prefix that gets attached to the tools the server exposes (see
[Tool name prefixing](#tool-name-prefixing) for the length that prefix holds to).

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
cannot be `start = "manual"`. It also skips the in-container user bootstrap: that needs a
running container, and an entrypoint host runs its server as the first process, so there is no
window for it between the container's creation and the attach that runs the entrypoint. The
image's own `USER` therefore applies to any `workspace` or `mounts` the block declares.
`view = "primary"` is the exception -- see below: its payload runs as the session user whatever
the image says, because OutRig's own launcher is the process that starts it.

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

A ref written inline like that is resolved the way `--image` is: an `[images.<name>]` block
first, else a raw podman ref that must already be present locally -- OutRig will not pull it,
so that a typo cannot reach a registry. To have OutRig pull the image, name it in a block of
its own and point the entry at that:

```toml
[images.mcp-filesystem]
image-name = "docker.io/mcp/filesystem:latest"

  [images.dev.mcp]
  fs = { image = "mcp-filesystem", view = "primary", args = ["/workspace"] }
```

**The entrypoint program must be an ELF binary.** `outrig-enter` opens it before joining the
primary's namespace and classifies it; a `#!` script is refused, because its interpreter line
would resolve against the *primary's* rootfs and quietly run something other than the image's
own. Published MCP images often ship a console script -- `docker.io/mcp/git`'s `ENTRYPOINT` is
one -- and the fix is a two-line image of your own that names the interpreter and passes the
script to it:

```dockerfile
FROM docker.io/library/python:3.13-slim
RUN pip install --no-cache-dir mcp-server-git==2026.7.10 mcp==1.29.0
ENTRYPOINT ["/usr/local/bin/python3", "/usr/local/bin/mcp-server-git"]
```

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

**`HOME` is the primary user's.** The payload runs as the session user, but the sidecar image
sets `HOME` for whoever *it* expects to run as -- usually root, whose `/root` is `0700`. So the
container gets `--env HOME=<primary's home>`, the same directory every exec-stdio server is
given, and an `env` entry of your own for `HOME` still wins. Without it the breakage is oblique
rather than obvious: `git` cannot read `core.excludesFile` under an unreadable `$HOME` and fails
rather than skipping it, so `cargo` reports a fingerprint error -- and only a tool that *writes*
to `$HOME` would have looked guilty.

**`/proc` is the payload's own.** The launcher joins the primary's *mount* namespace and
nothing else, so the primary's `/proc` -- an instance of the primary's PID namespace -- has no
entry for the payload, and `/proc/self` there resolves to nothing. The launcher mounts a fresh
`proc` over it in the private namespace it already holds, so `/proc/self` means the payload,
which is what a program asking about itself intends. Programs that read it are more common
than they look: rustup's `cargo` shim resolves its own toolchain that way. The trade is that
the primary's process list is not visible through the view -- it is a filesystem view.

**`/etc/ld.so.preload` is empty for the payload.** glibc's loader reads that file on every start,
by that absolute name, and no loader option turns it off. After the join it is the *primary's*
file, so a primary that ships one -- for an allocator, say, or a shim -- would have its library
loaded into the sidecar's server before `main`, with the server's environment and network, and one
built for a newer glibc than the sidecar's would stop the server from starting, with a loader
error naming a file the sidecar never had. So when the payload's loader is not musl's, the
launcher mounts `/dev/null` over the file in the private namespace it already holds, and the
loader finds it empty. The primary keeps its own file. The mount is in the payload's namespace,
though, so everything the server starts sees the file empty too, the primary's own binaries
included: they do not preload what the primary configured. That holds only while the primary
leaves the file alone. The mount covers the file as it is when the server starts, and the kernel
drops a mount whose file is renamed over or deleted, so when the primary replaces the file later
-- with `sed -i`, or by deleting it and writing another -- the server's later children read the
replacement, and a glibc one preloads what it lists. A write in place keeps the mask. A file the
primary creates after the server started is never masked. The server itself read the file once,
at its start, and is not affected. A musl loader has no such file and a static payload has no
loader, so none of this applies to either.

**The engine must be local.** The launcher binds the primary's `/proc/<pid>/ns` from this
machine, so the podman engine has to share this machine's kernel and PID namespace. A socket on
this host works; a remote engine (another host, a VM, another container) is refused up front.

**A dynamic payload cannot re-execute itself.** The kernel looks for a binary's loader
(its `PT_INTERP`) in the process's own root, which after the join is the *primary's* rootfs --
where the sidecar's loader is absent or is a different one. So the launcher does not exec a
dynamically linked payload directly; it runs the sidecar's loader explicitly, as
`/mnt/lib/ld-musl-x86_64.so.1 --library-path /mnt/lib:... /mnt/usr/local/bin/node ...`. The
payload then gets wrong answers to the questions a self-re-exec asks: `/proc/self/exe` names the
*loader*, so executing it runs a loader with no program, and `argv[0]` is the server's path under
the graft -- a file that exists but that the kernel cannot run by name, because it goes looking
for the loader in the primary. Failures are oblique. Python's
`subprocess.run([sys.executable, ...])`, and so `multiprocessing`'s `spawn` and `forkserver`, get
`ENOENT` for a file that is plainly there; Node's `child_process.fork()` runs `process.execPath`,
which is the loader, and the loader tries to load the script as a program; a Go `os.Executable()`
or Rust `current_exe()` re-exec fails the same way.

A static payload has no loader and is exec'd from an open file, so executing `/proc/self/exe`
itself -- the idiom of container runtimes -- does re-run it. What fails are the paths: none of the
sidecar's rootfs is grafted for it, so `argv[0]` names a file the view does not have (`ENOENT`),
and what `/proc/self/exe` reads back as -- which is what `os.Executable()` and `current_exe()`
hand over -- is `/`, which cannot be executed.

A server that does its work in one process -- the quickstart's filesystem server -- is unaffected.
One that starts copies of itself cannot take `view = "primary"` unless it is a static binary that
executes the literal path `/proc/self/exe`; run it as an entrypoint sidecar in its own image,
with the project mounted through `workspace` instead.

**A dynamic payload's libraries come from the sidecar, and only the ones it declares.** Before
joining the primary, the launcher walks the payload's `DT_NEEDED` libraries, and theirs in turn,
in the sidecar's own rootfs -- through its absolute symlinks, and the directories its own musl
`/etc/ld-musl-<arch>.path` names -- and has the loader find each one there first. A library the
sidecar does not ship fails startup, naming it and the directories searched, rather than running
on the primary's copy. The walk follows `DT_NEEDED` only, so three things can still reach the
primary's files: a library the payload loads at run time with `dlopen`, a `DT_NEEDED` name that
contains a `/`, and an absolute `DT_RPATH` entry, which the loader searches before anything the
launcher passes.

A program named without a `/` -- `ENTRYPOINT ["node", "/app/dist/index.js"]`, which is what
the quickstart image above declares -- is searched along the **sidecar image's** `PATH`, in
that same pre-join instant and for the same reason: the program is one of the sidecar's own
files, so the sidecar's `PATH` is the one that can find it. Only absolute `PATH` entries are
searched, since a relative hit could not be named under the graft afterwards. When nothing
matches, startup fails with a message naming the program and the `PATH` that was walked.

`view = "primary"` is a real posture change: the container starts with `CAP_SYS_ADMIN` and
`CAP_SYS_PTRACE` in the primary's user namespace, and the server can read the primary's whole
filesystem. See [Trust Model](mcp-trust-model.md) and `SECURITY.md`. It needs the
`outrig-enter` helper. The helper is supported on Linux x86-64 and AArch64. It is compiled for
the matching `<arch>-unknown-linux-musl` Rust target when that is installed, and otherwise
statically for the build machine's own target, which needs the host's static C library; either
build runs in any sidecar image. When neither works, a `view = "primary"` session fails at
start with a message naming the missing artifact and the specific build-time reason, and a link
to the page on building without the musl target. The helper is always a Linux binary because
it runs inside the container, not in the host process. macOS and native Windows are
unsupported; WSL2 is Linux and uses the normal path.

**The capabilities belong to the launcher, not to the server.** `outrig-enter` needs them for
the namespace join and the graft, and gives them up the moment that work is done: it becomes the
session's uid/gid immediately before exec'ing the payload, which clears its capability sets with
the uid transition, and it sets `no_new_privs` there too, so a setuid-root binary or a file with
capabilities that the server runs cannot hand them back. The server -- and anything it shells
out to -- therefore runs as the same user every other OutRig-launched server runs as, so what it
writes into the workspace is yours rather than a subuid you cannot chown back, and a root-owned
path in the primary is out of reach. The image's `USER` does not enter into it.

The launcher needs five capabilities to get that far: `CAP_SYS_ADMIN` and `CAP_SYS_PTRACE`, which
OutRig adds, and `CAP_SYS_CHROOT`, `CAP_SETUID` and `CAP_SETGID`, which podman's default set
carries. A sidecar block that takes one away -- `cap-drop` naming it or `ALL`, or
`capability-profile = "drop-all"` -- is refused when the config is read, as is
`no-new-privileges = false`, which the launcher's own `no_new_privs` would make a no-op.

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

"The image" there is the one the primary container is *running*, addressed by the engine's id
for it, not whatever the tag it was started from names at the moment of the read. A tag is a
pointer and a pull of a re-pushed ref moves it, while every container already started from the
old image goes on running that one. So the merged servers are always ones the container can
serve, even when `outrig mcp --attach` borrows a container started before the last pull. A
repo-local build image is named by a content hash, which no pull can re-point.

A label entry may carry `call-timeout-secs`, since the image's author knows how long its tools
run (see [What if a server hangs?](#what-if-a-server-hangs)). The placement keys and `args` are
rejected in a label.

A `${VAR}` in a label entry's `env` is read from your environment and handed to the server. The
label is text the image's author wrote -- a Dockerfile in the repo, or an image the repo names --
so it is held to the same [`[repo]` `env-allow`](../reference/config.md#repo) list as a repo
config's references. A reference to a variable the list omits is refused when the session
starts, naming the server, the key, the variable, and the image; the variable is never read. A
server your config declares by name replaces the label's entry and is not held by this. An image
of your own that references a variable needs it listed to start.

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
invalid server names, and empty command arrays are errors because they
mean the image metadata is broken: a startup error for an image outrig did not build, and a
build error for a repo-built image, whose build reads the label it inherited before it merges
`[images.<name>.mcp]` into it.

That build error names the image-config (`image "coding" embedded config: ...`) and ends with
the way past it. The label comes from a `LABEL` in the image's Dockerfile or from its base image,
so replace it in the Dockerfile, after the `FROM`:

```dockerfile
FROM registry.example.com/shared/tools:3
LABEL org.outrig.mcp='{}'
```

An empty table declares no servers, so the build keeps only what `[images.<name>.mcp]` adds.
Set a corrected value instead to keep the servers the base image meant to declare.

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

When `outrig run` starts, the primary container comes up first -- with network interception on,
it is created and initialized, the interceptor attaches, and only then does its process start --
then every `start = "auto"` sidecar, then interception attaches to each of those, and only then
do MCP servers connect -- primary-hosted and sidecar-hosted alike, in name order. The sequence
per MCP server is:

1. `podman exec -i <container> <command>` -- outrig spawns the server as a child process inside
   its placement's running container, with stdin/stdout piped back to outrig.
2. **Initialize handshake** -- outrig sends the MCP `initialize` request and reads the server's
   capabilities.
3. **Discover tools** -- outrig calls `tools/list` and receives the list of advertised tools, each
   with a name, description, and JSON Schema for its inputs.
4. **Register with Rig** -- each discovered tool becomes a `McpToolAdapter` that implements Rig's
   dynamic-tool trait. `outrig run`'s agent now has access to it.

All servers come up before the REPL accepts any input. If any server fails to initialize --
including one that [stops answering](#what-if-a-server-hangs) -- `outrig run` reports the error
on stderr, with the server's stderr so far, and exits before the REPL starts. You don't get
partial sandboxes.

When the REPL terminates (Ctrl-D, Ctrl-C, or LLM error), outrig closes each server's stdin in
turn, waits up to 5 seconds for the process to exit, then stops the containers -- sidecars
first, primary last. Servers don't see SIGTERM directly; they see EOF on stdin, which the MCP
spec defines as the normal shutdown signal.

In attach mode, `outrig mcp --attach` borrows a container that something else owns. It
shuts down only the MCP children it started and leaves the borrowed container running.
If the owner stops the container while the attacher is live, the attacher exits instead
of trying to relaunch it.

## In a session: MCP services

A session -- `outrig run-new`'s, a host program's `outrig.Session`, or one a Rust program starts
with `SessionBuilder` -- places, starts and stops MCP servers itself and binds each to the session
as a **service**: an object agent code reaches as `outrig.bindings[name]`, and as a global of that
name where it is free, with a method per tool by the tool's exact name. Every call is checked
against the tool's schema, held to a deadline, recorded as `service.call.*` events with
`kind: "mcp"`, and listed in the shutdown report when the close ends it, as any service's call is.
A call past its deadline, interrupted, or cut off by the close or a stop sends its server
`notifications/cancelled` and ends at once, `unknown`: the server may have done the work.

A server comes from one of three places: the configuration's `[images.<name>.mcp]` entries and the
sidecar blocks they name, and the image's `org.outrig.mcp` label under `[repo] env-allow`, as
above; a host program's `outrig.Session(mcp=[...])`; or `SessionBuilder::mcp`. A configured
server is bound under its own name, or with `-` read as `_` when that is free -- `my-fs` as
`my_fs` -- and its events keep its own name as `server`. Each runs in one of four placements:

- **command** -- exec'd in the session's container, beside the interpreter, as the session's
  user, as a primary-placed entry is.
- **sidecar** -- in a container of its own, seeing what that container is granted. A
  configuration's `[sidecars.<sc>]` block hosting several servers is one container for them all,
  as for `outrig run`; a host program's or a builder's sidecar hosts one server, in a container
  named `mcp-<name>`. A configured block of that name is never placed in its stead: its servers
  are `misconfigured`.
- **primary view** -- an entrypoint sidecar with `view = "primary"`, seeing the session container's
  whole filesystem; see [Primary filesystem view](#primary-filesystem-view).
- **url** -- a streamable-HTTP server the host dials, in no container of the session's: it sees no
  file of the session's, and is not the session's to stop.

**A URL server** is declared with a `url` and the `headers` each request carries --
`McpServiceSpec::url(..).with_header(..)`, `outrig.McpUrl(name, url, headers=...)`, or a
configuration's `url` entry:

```toml
[images.coding.mcp.docs]
url = "https://docs.example.com/mcp"
headers = { Authorization = "Bearer ${DOCS_TOKEN}" }
```

A header may combine literal text and any number of `${VAR}` references, as in `Bearer ${TOKEN}`.
The same syntax works in `env`, `build-args`, and `--env`; `$${` escapes a literal `${`.
Malformed references are refused at load. Names refer to host variables (or session `Secrets`),
not container variables: `PATH = "/opt/bin:${PATH}"` would insert the host's `PATH`. Every name
in a repository value must appear in the global `[repo] env-allow` list.

Each request carries those headers -- `initialize`, every call and listing, the server's standing
event stream, a cancel notice and the request that ends the session -- each `${VAR}` resolved once,
at start, and shown nowhere: not in an event, an outcome, the report, the service's description,
its `repr` or `help()`, or an exception. One the owner declares -- on the builder or
`outrig.Session` -- also carries the owner's identity headers (`SessionBuilder::mcp_identity`,
`outrig.Session(mcp_identity=...)`), unread and unchanged; a configured one never does, approved or
not. Its client trusts the system's roots, and a trust anchor -- a PEM certificate authority, by
path or as bytes -- beside them, or with `trust = "only"` in their place; it follows no redirect,
honors the system proxy, and takes a server that issues no session unless told
`allow-stateless = false`. A URL with a user name or password in it, a scheme other than `http` or
`https`, `trust = "only"` with no anchor, or a header that names an identity header or one HTTP or
the protocol sets itself -- `Accept`, `Content-Type`, `Mcp-Session-Id`, `Mcp-Method`, `Mcp-Name`,
any `Mcp-Param-*` and the like -- is `misconfigured` before any request, as is every server the
owner declares while one of the owner's identity headers is such a header. Plain `http` to a host
that is not loopback starts, and its `mcp.service.started` says in `detail` that the headers went
in the clear.

**A repository's URL entry needs your approval.** The host dials a URL server from its own
network, so a repository's entry would reach loopback and host-only services the session's
container cannot. It starts only once you approve that exact entry in that repository, as a
repository's [binding](../reference/config.md#bindingsname) does: `run-new` shows its URL and
its headers by name, never a value, and asks once, and any edit asks again. One nobody approved
connects to nothing and is `unapproved`. An entry in the global config asks nothing. `outrig run`
starts no URL server: it says which it leaves out. `outrig mcp` can front a URL entry for an
external client; it uses the same headers, trust settings, and remembered repository approval.

**A server that holds a credential is never placed beside the interpreter.** A `${VAR}`, even
embedded among literal text, in a
command's or a primary view's `env` would be readable from agent Python wherever `/proc` allows,
so such a server does not start; a credentialed server is a sidecar, or a URL whose headers the
host sends. A `${VAR}` under a key podman reads itself (see
[MCP `env` value syntax](../reference/config.md#mcp-env-value-syntax)) does not
start either, since its value would reach podman's command line. Each `${VAR}` is resolved once,
by the session's secret resolver -- the process environment for `run-new` -- and a value reaches
podman by name, never on a command line.

**Startup.** The servers start as the session does, beside its interpreter and concurrently with
one another: placed, `initialize` answered, tools listed. An attempt's handshake and its listing
are each bounded at two minutes, and the whole attempt, its container's preparation included, by
what is left of the **connect budget**: two minutes, or the top-level `mcp-connect-budget-secs`;
the primary image's label is read within that budget too. A failure that may pass -- the
transport failed, the handshake did not answer -- is tried again, waiting half a second and then
twice as long each time, up to eight, within that budget, and no server starts after it. A sidecar
of one server's own is stopped before each new attempt, so what a failed one left running does not
run beside the next, and a sidecar none of whose servers started is stopped once the start ends;
while that stop is not proven, its servers are still listed as writers, as is a command that was
launched and did not start, since what it ran may still run in the session's container. Each ends
with an outcome:

- `ready` -- started and bound.
- `unavailable` -- it failed in a way that may pass, every attempt the budget allowed: a URL server
  whose connection failed, or that answered `initialize` 408, 429 or a 5xx, whatever its body,
  included.
- `refused` -- it answered `initialize` with an error, or a URL server answered `initialize` or its
  listing 401 or 403.
- `misconfigured` -- it cannot start as declared: its command exited 126 or 127, it holds a
  credential beside the interpreter, a `${VAR}` has no value, a configured sidecar's image could
  not be built, pulled or found, a URL server's certificate is not one its client trusts, or its
  URL answered 404.
- `unclassified` -- any other failure, another exit status included, named so rather than guessed.
- `unapproved` -- a repository's URL entry nobody approved: nothing was connected.

A server that does not start does not stop the session: it starts with the servers that are
ready. `run-new` prints one line per server; a host program reads `session.mcp`; a Rust program
reads `Session::mcp().outcomes()`. A tool the server lists that cannot be decoded -- one whose
`inputSchema` is not an object, say -- is left out, and the server is ready with the rest; its
outcome names each one left out and why. (`outrig run` and `outrig mcp` still refuse such a
server.) The `mcp.service.*` events in
[Events](https://tgockel.github.io/outrig/reference/events.html) record each start, retry and
failure. A sidecar's `on-failure` is not consulted, and a `start = "manual"` sidecar's servers are
`misconfigured`: a session starts every server it places.

**Quiescing.** `Session::mcp()` also says which servers can write what -- a command and a primary
view the session container's filesystem, a sidecar the workspace when it mounts it `rw` and its `rw`
mounts, a URL server nothing the session mounts -- and can pause or stop one: a pause holds its new
calls, waits for those running, and freezes its container with `podman pause`; a stop tells each
call in flight to stop, closes the transport and stops the container, which stays under the
session's network policy until its stop is proven. A pause or a stop runs to its end even when its
caller stops waiting, and a resume asked after it waits for it and undoes what it did; one whose
caller stopped waiting before it began does nothing. A URL server has no container: a pause holds
its calls and waits for those running, with nothing to freeze, and a stop ends its HTTP session. A
command placement cannot be frozen apart from the interpreter, and a stopped server is not paused;
each says so. A container a configuration's block shares is paused or stopped for all of its servers
at once.

**In agent Python.** The service is an `outrig.McpService`: each tool is a method, by its exact
name where that is an identifier -- `-` read as `_`, and a trailing `_` for a keyword -- and
always through `fs.call("read-file", {...})` or `fs.tool("read-file")`, which gives the tool
with the descriptor its server listed. `fs.tools` lists those descriptors, `title`,
`annotations`, `icons` and `_meta` included, and `help(fs)` renders each tool's signature and
docstring from its schema. A call returns an `outrig.McpResult`, the result as data rather than
text:

```python
r = fs.read_file(path="data.json")
r.content          # the blocks, in order: McpText, McpImage, McpResource, ...
r.structured       # structured content, as Python data, or None
r.content[1].data  # an image's bytes, decoded on first use
r.text             # the blocks as one string -- a lossy view
```

Agent code filters a large result in Python and prints what it needs: what the model sees is
what the code prints, never the result itself. A result the server marks as an error raises
`outrig.ToolError`, carrying it. A result larger than the session's bound -- 64 MiB of JSON, or
the top-level `service-result-max-bytes` -- is not handed over: the call raises, and its event
says it returned, since the server did the work. OutRig never calls a tool again.

**Did it happen?** A call that returns no result raises an `outrig.McpError`, an
`outrig.ServiceError`, whose `state` is decided by where in the call it ended, never guessed from
what failed:

- `not_dispatched`, raised as `outrig.NotDispatched` -- nothing reached the server, so nothing
  happened.
- `may_have_dispatched`, raised as `outrig.MayHaveDispatched` -- the request was handed over and
  no answer was read, so the server may have done it.
- `response_received`, raised as `outrig.ResponseReceived` -- the server answered, and its answer
  is what happened.

A call refused before it was sent is `NotDispatched` -- `UnknownTool`, `InvalidArguments`,
`ServiceUnavailable`, `AdmissionClosed` or `Paused`. One whose transport failed, whose deadline
passed, or that was interrupted or cut off by the close is `MayHaveDispatched`, with `failure`
saying which -- `transport`, `timeout`, `canceled` -- since a failed write does not prove the
server saw nothing. An error the server answers is a `ProtocolError`, and a `ToolError` is a
`ResponseReceived` too. `except outrig.MayHaveDispatched:` is the spelling that keeps a write
from being repeated before checking whether it happened.

**Deadlines and cancellation.** A call's deadline is the service's `call-timeout-secs` -- a
builder's `with_call_timeout_secs`, a program's `call_timeout_secs` -- else the session's service
policy's ten minutes, and `timeout=` on a call lowers it, never raises it. Past it, or when its
caller is interrupted, the call ends at once and its server is sent `notifications/cancelled`,
which asks it to stop and does not prove it did: a timeout stops the wait, not the server, and its
late answer is discarded. A call made in an `asyncio.to_thread` worker is not reached by an
interrupt, and ends at its deadline.

**A server whose transport fails** is connected again for the calls after it, never for the call
that failed, which stays `MayHaveDispatched` and is never sent again. The next call -- or
`fs.reconnect()`, without a call -- places it again as it was placed at the start: a new exec in its
sidecar, a new sidecar when its container is gone, a new entrypoint container, a new exec in the
session's container, or a new HTTP session with a URL server. Its `${VAR}` values are those resolved
at the start, never resolved again; its tools are listed again, as a relist lists them, and a change
is announced as a relist's is. The call that connected it is checked again against that listing
before it is sent: a tool no longer listed is `UnknownTool`, and arguments its new schema refuses
are `InvalidArguments`, each never sent. The whole reconnect is tried within the connect budget, as
a start is; one that fails refuses the call that asked for it, `ServiceUnavailable`, and the next
call tries again, up to three reconnects a session (a Rust owner's `McpPolicy::with_reconnect_max`),
after which the service is unavailable. No reconnect is made once the session is closing. The server
the failed transport spoke to may still run until its container stops, and it is still listed among
the writers. A URL server's event stream that breaks, or does not open, and is not resumed within
about two seconds -- three tries -- disconnects the service with no call failing, since no change to
its listing would be heard; the next call connects it again.

The tools are the server's listing as it started. `fs.relist()` asks the server again: a tool it
added is a method from then on, one it removed is refused when called, and a method or tool
already held keeps calling the tool it was resolved by, never another. A server that says its
listing changed marks the service `stale`, and nothing is relisted until agent code does.
`mcp.service.relisted` and `mcp.service.list.changed` record each.

**The close.** At the session's close each server's transport is closed and its container
stopped, a paused one thawed first, one a stop left unproven stopped again -- a server that never
started included, when its container ran; a command placement ends with the session's container,
and is proven stopped only once that is -- one that did not start too, when an attempt ran it; a
URL server's HTTP session is ended, `disconnected`, the server left running.
The shutdown report's `mcp_services` lists each, with how it was stopped and whether that was
proven. A call in flight at the close drains to its deadline, and is then sent the cancel and
ends `unknown`, may have dispatched; one waiting for its server to be connected again is refused.
`mcp_uncertain` lists every call that ended may have dispatched in the session -- by call,
service, tool and agent -- and one makes the report's verdict `stopped_with_unknown`: its server
may have done what it asked. A Rust owner reads the same list at any time from
`Session::mcp().uncertain()`.

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
are errors, and there is no `/sidecar stop`. New tools become available to the agent on its next
turn. `/sidecar list` shows every declared sidecar with its status (`running`, `not started`,
`exited`) and the servers it hosts.

Dynamic-add failures are reported only to the caller -- the REPL prints the error, the library
returns `Err` -- and the session stays healthy: anything the failed add started (container,
interceptor attachment, connected servers) is torn down, and the existing tool set is untouched.
This is deliberately different from the session-start `on-failure` policy, which decides whether
a *launch-time* sidecar failure aborts the session.

There is intentionally no agent-invocable tool for adding sidecars: the agent must not grow its
own environment (see
[Trust Model](mcp-trust-model.md#the-agent-cannot-grow-its-own-environment)). Only the human at
the REPL or the embedding program can.

## Tool name prefixing

Two different MCP servers can have a tool with the same name (e.g. both `fs` and `archive` might
define `read_file`). To avoid collisions and keep the LLM's tool-name space predictable, outrig
prefixes every tool with its server name, as far as the 64-character limit on a name allows (see
[Long server names](#long-server-names)):

```
fs__list_directory
fs__read_file
fs__write_file
shell__exec
build__cargo_check
```

The separator is `__` (double underscore). A combined name that already fits OpenAI's
`^[a-zA-Z0-9_-]{1,64}$` constraint **and** splits back into its pair -- its first `__` is the
separator -- is advertised exactly as written, which is the ordinary case. Any other name is
lossy, in one of three ways: a character outside the set had to become `_`, the name was longer
than 64 characters, or the separator was ambiguous because the server name itself ended in `_`
or contained `__`. A lossy name is truncated to fit and tagged with a stable hash suffix over
the `(server, tool)` pair, so two tools that would otherwise collapse onto one name stay
distinct:

| Server | Tool        | Advertised             | Why                     |
| ------ | ----------- | ---------------------- | ----------------------- |
| `fs`   | `read_file` | `fs__read_file`        | already well-formed     |
| `fs`   | `read/file` | `fs__read_file_5d2270` | `/` had to be replaced  |
| `fs`   | `read file` | `fs__read_file_4e31b7` | so did the space        |

The suffix is derived from the server and tool names alone, so an uncontested name is the
same in every session and a client may cache it across reconnects. Sixty-four characters
cannot encode an unbounded pair of names, though, so two tools can still land on one name --
two suffixes agreeing, or a tool whose own name already ends the way a suffix does. outrig
detects that, widens one side's suffix until the two names differ, and logs both identities.

Which side moves is decided by the two tools' identities, not by the order a server listed
them in, so a restarted server that relists its tools differently cannot hand a name a client
cached to a different tool. A widened name does depend on the rest of the session's tool set,
so that one is not cacheable across a change to the servers in play.

The LLM sees `fs__write_file` in its tool list and emits tool calls under that name. outrig's
router looks that name up in a table it built at startup and dispatches to the matching MCP
client under the original tool name. It does not recover the pair by splitting the string, so a
suffixed name routes like any other.

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

### Long server names

The server-name rule bounds spelling, not length, and a rewritten name is cut at a fixed length
without regard for where the server name ends, so the prefix survives a rewrite only while the
server name leaves room for it. At the default six-character suffix, a rewritten name for a
server whose name is:

- **54 characters or fewer** reads `<server>__<start of the tool>_<hash>`.
- **55 characters** keeps `<server>__` and none of the tool's name.
- **56 characters** keeps half the separator, so it reads `<server>__<hash>`.
- **57 characters or more** is cut inside the server name: the first 57 characters of it, `_`,
  and the hash, with no `__`.

A widened suffix leaves less room, so a widened name loses the prefix at a shorter server name.
A name advertised as written is not affected, and begins `<server>__` in full. Routing is not
affected either, since a name is looked up whole rather than split. What suffers is legibility:
a person or a model reading the tool list cannot tell which server hosts such a tool, which is
why `outrig mcp` tells its client to treat names as opaque and call each exactly as listed.
Keeping server names to 54 characters or fewer avoids it for every name outrig does not have to
widen.

## Protocol revisions and list caching

Both servers outrig exposes -- the session proxy (`outrig mcp`) and the self-description server
(`outrig mcp self`) -- advertise an explicit list of MCP protocol revisions rather than deferring
to whatever the underlying SDK happens to know:

| Revision     | Requested in           |
| ------------ | ---------------------- |
| `2024-11-05` | `initialize`           |
| `2025-03-26` | `initialize`           |
| `2025-06-18` | `initialize`           |
| `2025-11-25` | `initialize`           |
| `2026-07-28` | each request's `_meta` |

The list is pinned deliberately. An SDK upgrade that teaches the library a newer revision would
otherwise widen what outrig agrees to speak without anyone checking that outrig actually meets the
new revision's requirements -- and a revision can add *mandatory* response fields, which turns a
silent widening into a client that cannot load any tools at all.

`2026-07-28` replaced the `initialize` handshake with metadata on every request: a client speaking
it names the revision in each request's `_meta` and is answered in it. An `initialize` asking for
anything the handshake cannot grant -- a revision outside this list, or `2026-07-28` and later --
is answered in `2025-11-25`, the newest revision that has a handshake. Note that this is a fallback
rather than a step down: a client asking for something older than anything listed is answered in a
*newer* revision, not an older one. A revision named per request has no fallback: one outside the
list is refused with an error that lists the revisions served.

From `2026-07-28` onward, list results must carry cache metadata (SEP-2549): how long the result
may be treated as fresh, and who may cache it. Both servers tag `tools/list` accordingly:

`outrig mcp self`
: `cacheScope: public`. The tool set is compiled into the binary, so it is identical for every
  user of a given outrig version and any intermediary may cache it.

`outrig mcp`
: `cacheScope: private`. The tool list is the union of this session's backing servers, which
  depends on the active config, image-config, and any `--env` overrides -- not shareable.

Both declare a five-minute freshness window. Neither tool list can actually change mid-connection
(the table is built once at startup, and no `listChanged` capability is advertised), so a longer
window would still be truthful; five minutes is chosen so that a config edit or a rebuilt image is
picked up promptly on the next session.

## What if a server crashes mid-session?

A crashed MCP server is surfaced as a tool-call error to the LLM, which usually causes the model
to stop calling that tool and tell you about the failure. `outrig run` does not restart MCP
servers -- the server is gone for the rest of the session.

In a session (`run-new`, a host program, `SessionBuilder`), the call the crash ended raises
`outrig.MayHaveDispatched`, its failure the transport, and the next call connects the server
again, as [In a session](#in-a-session-mcp-services) says; nothing is sent again.

A sidecar container that dies mid-session behaves the same way, uniformly and regardless of its
`on-failure` setting: outrig logs the death to stderr, every tool the sidecar hosted returns
errors, and nothing restarts. If the *primary* container dies out from under outrig (a manual
`podman kill`, the OOM killer), outrig reaps every sidecar and ends the session with an error.

> **TODO: Incomplete** -- for `outrig run`, auto-restart and per-server health-checking are
> deferred.

The server's stderr, captured to `<session_dir>/logs/<server>.stderr`, usually has the actual
error. See [Sessions](https://tgockel.github.io/outrig/usage/sessions.html) for how to view it.

Attach mode can run more than one copy of the same MCP server in one container. Prefer
servers that are reentrant-safe: no fixed listening port, global pidfile, or exclusive
lock unless the server is explicitly designed to coordinate multiple copies. If a server
cannot run twice, the second copy should fail clearly during startup and its stderr log
will show the underlying conflict.

## What if a server hangs?

Every request outrig sends a server has a deadline, so a server that stops answering costs a
bounded wait rather than the session:

- `initialize` and `tools/list` get 120 seconds each. A server that misses either fails startup
  the way a crashed one does, with the stderr it has written so far. A `tools/list` whose pages
  would never end -- one that hands back a `nextCursor` it already sent, or is still paging after
  1,000 pages -- fails startup the same way, without waiting out its 120 seconds.
- `tools/call` gets the server's own `call-timeout-secs`, else the top-level
  `mcp-call-timeout-secs`, else 10 minutes. A call that runs past it is cancelled at the server
  with `notifications/cancelled`, and the model gets a timeout error in place of a result. The
  session goes on, and the server stays connected.

A call outrig stops waiting for early is cancelled the same way: one in a turn interrupted with
Ctrl-C, or one whose `outrig mcp` client sent its own cancel. Cancellation is advisory -- the MCP
spec lets a server finish the work anyway -- so a tool with side effects may still complete them.

In a session, a call past its deadline or interrupted raises `outrig.MayHaveDispatched`, its
failure `timeout` or `canceled`, as soon as its server is sent the cancel; the server stays
connected.

Raise `call-timeout-secs` on a server whose tools legitimately run long, such as a build or a
test suite, rather than the session-wide default; see
[Reference -> Config](../reference/config.md#imagesnamemcp).

## Picking which servers to include

Two practical guidelines:

- **Match servers to the image-config's purpose.** A `planning` image probably doesn't
  need `shell`. A `coding` image almost certainly needs both `fs` and `shell`.
- **Fewer servers is better when it's enough.** Every server is another initialize cost at
  startup, another tool list cluttering the LLM's prompt, another process to monitor. If a single
  MCP server covers your needs, use one.

## See also

- [Containers](containers.md) -- the Dockerfile that has to install the server binaries.
- [Trust Model](mcp-trust-model.md) -- the container boundary that makes broad MCP tools
  practical.
- [Python Runtime](https://tgockel.github.io/outrig/concepts/python-runtime.html) -- the agent
  that acts by writing Python, which reaches MCP servers as services.
- [AI-assisted design](../usage/ai-assisted-design.md) -- use `outrig mcp self` to design custom
  MCP-enabled image-configs.
- [Sessions](https://tgockel.github.io/outrig/usage/sessions.html) --
  `outrig logs <session> <server>` for stderr.
- [Reference -> Config](../reference/config.md) -- full schema for the `[images.<name>.mcp]`
  block.
