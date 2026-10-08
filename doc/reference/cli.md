# CLI Reference

## Synopsis

```
outrig <SUBCOMMAND> [FLAGS] [ARGS]
outrig --version
outrig --help
```

## Global flags

These are accepted by every subcommand.

| Flag                     | Description                                                 |
|--------------------------|-------------------------------------------------------------|
| `--config <path>`        | Path to repo `config.toml`. Default: walk up from cwd.      |
| `--global-config <path>` | Path to global config. Default: `~/.outrig/config.toml`.    |
| `--session-root <path>`  | Root directory containing sessions. Overrides config + XDG. |
| `-v`, `--verbose`        | Print buildah/podman transcripts; repeat for trace logs.    |
| `--help`                 | Print subcommand help.                                      |

`--config` resolves in this order: this flag, then a walk up from cwd looking for
`.agents/outrig/config.toml`. The walk stops at the filesystem root; a `.agents/` directory
without an `outrig/config.toml` inside does not terminate the walk -- outrig keeps looking in
parents.

The flag names a file, and that file has to exist: a path that is not one is an error, not a
config-less run. Which repo the command then runs against depends on where the file is:

- `<repo>/.agents/outrig/config.toml` is that repo's own config, and the flag means what running
  from `<repo>` means. This is the way to point an MCP client, which may start outrig in any
  directory, at a repo.
- Any other file is read in place of the repo's config, for the repo found from cwd as if there
  were no flag: the walk above, else cwd itself. Relative paths in the file resolve beside it,
  as a `--global-config` file's do (see [path resolution](config.md#path-resolution)). Where the
  file sits never decides which directory is mounted as the workspace.

`--global-config` resolves in this order: this flag, then `<XDG_CONFIG_HOME>/outrig/config.toml`
when `XDG_CONFIG_HOME` is set, then `~/.outrig/config.toml` (the outrig-specific fallback --
note this is `~/.outrig/`, not `~/.config/outrig/`).

`--session-root` resolves in this order: this flag, then `session-root` in the repo or global
config, then `<XDG_DATA_HOME>/outrig/sessions/`.

`--verbose` adds buildah/podman command transcripts to stderr and to
`<session_dir>/logs/container.log` for `outrig run` / `outrig mcp`. Repeat it (`-vv`) to also
enable trace-level logs from outrig's own modules for that invocation. It does not change
container, MCP, or agent behavior. Normal startup progress is printed to stderr without
`--verbose`.

`--verbose` controls command *output*; the tracing filter controls command *lines*. At
`debug`, every buildah/podman invocation is logged before it is spawned and again on exit with
its status and elapsed time (target `outrig::process`), which identifies the responsible child
process when a startup phase stalls. The two are independent -- neither implies the other.

## Subcommands

### `outrig init`

Idempotent end-to-end setup orchestrator. Runs `outrig config init` if the global config is
missing, writes `.agents/outrig/config.toml` if it doesn't exist, then offers to call
`outrig image add` in a loop.

```
outrig init [--force]
```

| Flag      | Default | Description                                                                |
|-----------|---------|----------------------------------------------------------------------------|
| `--force` | off     | Overwrite existing files. Propagates to `config init` and `image add`.     |

See [Usage -> outrig init](../usage/init.md).

### `outrig config init`

Interactively write the global config (`~/.outrig/config.toml`): provider styles, models,
and `default-model`. The first subcommand of the `outrig config` group; future subcommands
(`config get`, `config set`, `config list`) are deferred.

```
outrig config init [--force]
```

| Flag      | Default | Description                                                        |
|-----------|---------|--------------------------------------------------------------------|
| `--force` | off     | Overwrite an existing global config; otherwise refuses to clobber. |

See [Usage -> outrig config](../usage/config.md).

### `outrig design prompt`

Print a self-contained prompt for AI-assisted image design, or print an MCP setup snippet for
tools that can attach `outrig mcp self`.

```
outrig design prompt
outrig design prompt --standalone
outrig design prompt --print-mcp-config <tool>
```

| Flag                        | Default | Description                                      |
|-----------------------------|---------|--------------------------------------------------|
| `--standalone`              | off     | Print a prompt for a standalone image project.   |
| `--print-mcp-config <tool>` | none    | Print a setup snippet; wins over `--standalone`. |

The valid `<tool>` names are `claude-code`, `claude-desktop`, `codex`, and `cursor`.

See [Usage -> AI-assisted design](../usage/ai-assisted-design.md).

### `outrig image add`

Interactively scaffold an image-config: writes a Dockerfile under
`.agents/outrig/images/<name>/Dockerfile` and adds the matching `[images.<name>]` and
`[images.<name>.mcp]` blocks to the repo config. Use `outrig image init` instead for a
standalone image project; `image ls` / `image rm` are deferred.

```
outrig image add [<name>]
                 [--force]
```

| Argument / flag | Default  | Description                             |
|-----------------|----------|-----------------------------------------|
| `<name>`        | prompted | Image-config name.                      |
| `--force`       | off      | Overwrite existing files for this name. |

See [Usage -> outrig image](../usage/image.md).

### `outrig image init`

Noninteractively scaffold a standalone image project: writes a `Dockerfile`, an authoring
`image.toml`, and a `README.md` into a target directory. The directory name becomes the image ref;
other repos consume the built image via `image-name`.

```
outrig image init [<dir>]
                  [--force]
```

| Argument / flag | Default     | Description                                          |
|-----------------|-------------|------------------------------------------------------|
| `<dir>`         | current dir | Project directory; its name becomes the image ref.  |
| `--force`       | off         | Overwrite the generated files if they already exist. |

See [Usage -> outrig image](../usage/image.md#outrig-image-init).

### `outrig image build`

Build a standalone image project (the output of `outrig image init`) with buildah, tag it by
its `image.toml` `[image].ref`, stamp its config into OCI labels, and validate the result: the
built image must carry a valid `org.outrig.mcp` label, and -- unless `--no-test` -- every
declared MCP server must start and answer `tools/list`.

```
outrig image build [<dir>]
                   [--tag <ref>]
                   [--no-test]
                   [--no-cache]
```

| Argument / flag | Default       | Description                                            |
|-----------------|---------------|--------------------------------------------------------|
| `<dir>`         | current dir   | Project directory holding `image.toml`.                |
| `--tag <ref>`   | `[image].ref` | Tag the output as `<ref>`; does not rewrite image.toml. |
| `--no-test`     | off           | Skip the live MCP test; still validates `image.toml`.  |
| `--no-cache`    | off           | Force a clean build. Passes `--no-cache` to buildah.   |

See [Usage -> outrig image](../usage/image.md#outrig-image-build).

### `outrig image inspect`

Inspect an image's OutRig OCI labels without pulling it, creating a container, or starting any MCP
server. By default this reads the local image store; `--remote` reads registry metadata with
`skopeo inspect`.

```
outrig image inspect [--remote] <ref>
```

| Argument   | Description                                   |
|------------|-----------------------------------------------|
| `<ref>`    | Image ref to inspect. Never pulled.           |
| `--remote` | Read registry metadata with `skopeo inspect`. |

See [Usage -> outrig image](../usage/image.md#outrig-image-inspect).

### `outrig run`

Start an interactive agent session. `outrig run-legacy` is another name for this command and
reaches the same code, so a script can name the MCP-tool agent explicitly before `run` moves to
the Python one.

```
outrig run [--agent <name>]
           [--image <name-or-local-ref>]
           [--config <path>]
           [--env <KEY=VALUE>]
           [--global-config <path>]
           [--max-tool-calls <n>]
           [--max-tool-result-bytes <n>]
           [--model <name>]
           [--network <default|audit|filter>]
           [--session-dir <path>]
           [--session-root <path>]
           [--volume <host:container[:ro|rw]>]
           [--verbose]
```

- `--agent <name>` (default: `default-agent`): selects an `[agents.<name>]` block. With
  neither, the session runs with no agent -- no preamble, and no `agent.image` rung in the
  image cascade.
- `--image <name-or-local-ref>` (default: from agent or `default-image`):
  image-config to launch. If an explicit value does not match config, it is
  treated as a local Podman image ref and is not pulled.
- `--env <KEY=VALUE>` (repeatable): add or override env vars for MCP servers. `KEY=VALUE`
  applies to every server; `SERVER:KEY=VALUE` targets a single server by name. Values support
  the `${VAR}` host-env-reference syntax described in
  [config.md#mcp-env-value-syntax](config.md#mcp-env-value-syntax). Within a scope, last wins
  on duplicate keys. Precedence per key: config-file env < global `--env` < per-server `--env`.
- `--max-tool-calls <n>` (default: resolved `tool-call-max`, else `50`): per-turn tool-call
  max.
- `--max-tool-result-bytes <n>` (default: resolved `tool-result-max`, else `262144`):
  per-tool-result byte max.
- `--model <name>` (default: agent's `model`, else `default-model`): configured
  `[models.<name>]` entry to use for this run. This is not a raw provider model identifier.
  An alias entry is accepted like any other name; the banner prints the hop it resolved to.
- `--network <default|audit|filter>` (default: config `[network].mode`, else `default`):
  choose Podman's default networking, network audit logging, or global network filtering for
  this session.
- `--session-dir <path>` (default: `<session-root>/<sid>`): specific directory for this run.
- `--volume <host:container[:ro|rw]>` (repeatable): bind an extra host directory into the
  container, beyond the default workspace mount. Read-only unless `:rw`; host dir must exist.
- `-v`, `--verbose` (default: off): print container lifecycle traces.

When `--session-dir` is given, outrig writes this run's `session.json` and `logs/` directly
under `<path>`, and additionally creates a symlink `<session-root>/<sid> -> <path>` so
`outrig ls`/`logs`/`discard`/`clean` still find it. When omitted, outrig auto-generates a
session id and writes to `<session-root>/<sid>/` directly.

Reads the global and repo configs, resolves agent -> model -> provider, builds the image
(cache-hit if possible), starts the container, attaches every MCP server, opens the REPL. Exits
when stdin reaches EOF, when the user types `/quit`, or after a second Ctrl-C.

With no repo config found and no `--config`, `run` and `mcp` use the current directory as the
workspace root and take all config from the global file. `run` then needs exactly one thing
from it: a resolvable **model**. The agent is optional -- without one the session sends no
preamble -- and so is the image, which falls through to outrig's built-in default. `mcp`
resolves no model either, so it needs nothing beyond a working podman. `build` still requires
a repo config and does not fall through; pass `--image outrig-default` to pre-warm the
built-in.

Whichever way the repo root is found, a workspace no config declares is never your home
directory or a directory above it: `run` and `mcp` refuse to start, naming the directory and
what chose it. Declare `[workspace] host-path` to mount it on purpose; see
[Concepts -> Workspace](../concepts/workspace.md#whats-mounted-what-isnt).

See [Usage -> outrig run](../usage/run.md) for REPL details.

### `outrig run-new`

Start an interactive session whose agent acts by writing Python. A preview: `outrig run` is
unchanged by it.

```
outrig run-new [--agent <name>]
               [--image <name>]
               [--config <path>]
               [--global-config <path>]
               [--model <name>]
               [--session-dir <path>]
               [--session-root <path>]
               [--verbose]
```

- `--agent <name>` (default: `default-agent`): selects an `[agents.<name>]` block, as for
  `run`. Its `preamble` follows OutRig's own orientation in the system prompt.
- `--image <name>` (default: from agent or `default-image`, else the built-in default): the
  `[images.<name>]` block to launch. Unlike `run`, a local image ref no block names is refused.
- `--model <name>` (default: agent's `model`, else `default-model`): as for `run`, aliases and
  their failover included.
- `--session-dir <path>` (default: `<session-root>/<sid>`): as for `run`. While another
  `run-new` holds the directory, this one is refused before it pulls or starts anything.

The model has one tool, `submit_python`, which runs source in a persistent CPython inside the
session's primary container, with the workspace as its working directory. Names the agent binds
stay bound from one message to the next. The interpreter is a static build OutRig mounts
read-only, so the image needs no Python of its own. Each submission's source is printed on stderr
as it starts running.

**What the agent's Python can import.** The standard library, and modules in the workspace, which
come after the standard library on `sys.path`. `pip install`, run from the agent's code, adds
pure-Python packages: `pip` there is the interpreter's own, not the image's, and it installs into
an environment of the interpreter's own under the container user's home, where the running
interpreter finds the package at once and no other Python in the image looks; `python3 -m pip`
installs for the image's Python. Fetching one needs the network and CA certificates the image and
`[network]` allow. Compiled code never loads in a static interpreter, so numpy and other packages
with compiled parts do not import however they are installed, and the error the model reads says
so and names the image's own Python, if it has one, as the place to run such code. In its Python,
`runtime.python` summarizes this, `runtime.names()` lists what the agent has bound, and `help(x)`
describes anything in at most 8 KiB.

**What you type is a message, not a prompt.** Each line goes onto the agent's `user` channel,
`runtime.channels["user"]` in its Python, and the model is told how many messages are waiting
there -- never what they say. It reads one by running code that receives it, and answers by
sending on the same channel. A line typed while the agent is still working is queued the same
way, and `[outrig] queued for the agent (N waiting)` says so; it does not start a second round.
The model hears of it in the next result its code returns, or in a round of its own once the
current one ends. A channel holds 256 unread messages and refuses the next, saying so; a message
is at most 1 MiB either way.

**A line typed while the agent waits ends the wait, not the work.** The agent's Python waits on
slow work with `runtime.wait`, which is Python's `asyncio.wait` except that it also watches the
agent's channels: while a message waits unread, it raises `MessageAvailable` instead of waiting.
So a line typed during a wait ends it, and the model, told a message is waiting, reads it and
decides what to do next, in the same round. What the wait was on keeps running, and a later wait
can wait on it again. Python waiting with a bare `await` is not ended by a message; Ctrl-C stops
it.

**The model is sent part of the conversation; the agent's Python holds all of it.** A round is
the agent's work on the messages waiting when it starts. Each model call is chosen the session's
first two rounds, the six before the current one, and the current one. Everything is in
`runtime.history.turns`, one entry per model call with what the model wrote and each
submission's source and result, so the agent's code can search the whole conversation without it
costing the model any context. `runtime.context.promote(turn)` adds a turn back to what the model
is sent, in its original place, from the next model call on, until `runtime.context.demote(turn)`.
When a round's first call leaves turns out, the line that opens the round says how many. A turn
the round ended during -- interrupted while its code ran -- has `incomplete` set, and each of its
calls that had not returned says so rather than claiming it never ran. The whole conversation
stays in the interpreter for the rest of the session, under the same memory ceiling as
everything else, and a warning says once when it passes an eighth of that ceiling.

**Each call is held to the model's context window.** `context-window` on the model's row sets it
(see [Remote-provider models](config.md#remote-provider-models)); without one, 128,000 tokens
is assumed and startup warns. Before each call, OutRig estimates its size -- about three bytes to
a token -- against the window, less the reply's `max-tokens` and the system prompt. When what was
chosen does not fit, the recent rounds go first, from their oldest turn, then the first rounds,
from their newest, then promotions, oldest first, then the current round's earlier turns; a turn
too large for what is left is passed over and smaller ones still go. The turn a call answers --
the model's latest code and its results -- is never left out. When that alone does not fit, the
call is not made: the round ends with `(round ended: turn N of round R ... is about X tokens, and
a call to M has room for about Y ...)`, and keeps the turn. The next round leaves it out like any
other, so the session goes on.

**What a shortened conversation looks like to a provider.** Every call keeps each tool call
beside its result, which every provider requires. What a cut can do is put two of the user's
turns or two of the model's in a row: a turn brought back without the rest of its round opens on
the model's call right after the model's own text, and a round cut short ends on results that the
next round's opening follows. Anthropic's API carries results as user messages and merges such a
pair. OpenAI's carries them as `tool` messages, so a prompt after results is no repeat there, and
it accepts two replies in a row. Both adapters are exercised against a mock, not a live
endpoint. A provider or gateway that requires turns to alternate -- a Bedrock-backed Claude
behind an OpenAI-compatible gateway is known to -- can refuse such a call, and when a refused
call carried one, the error says where.

**A failing provider is retried, then left for the next model.** A call that fails in a way that
may clear -- a rate limit, a 5xx, a dropped connection, a response OutRig cannot use -- is made
again within `retry-budget-secs`, as in `run`, and stderr says so. When the model is an alias of
several, a call that still fails moves to the next, as in `run`, and each call starts again at
the first. The call that moves is sent a conversation chosen for the window of the model it moves
to, which may leave out more than the first model's did. When every model has failed, the round
ends with each one's reason and keeps any Python it ran. Unlike `run`, no failure ends the
session, even one a resend cannot fix, such as a key every provider refuses; `/quit` does. With
`[events] mode = "record"`, the event log records each retry and each move, and names the model
that answered each call.

stdout carries only what the agent sends on the channel -- including a send from code still
running after its round ended, which is printed when it arrives, at the prompt or not. The agent
is held to the terminal's pace: its code waits once it is 16 messages ahead of what has been
printed. When input ends -- the end of piped input, or Ctrl-D -- the lines typed before it that
were refused are reported and what the agent had already sent is printed, then `run-new` exits; a
task still sending is not waited for. The
model's own text is commentary and goes to stderr with everything else, so `outrig run-new >
out.txt` keeps exactly the messages the agent meant you to have.

**No MCP server and no sidecar starts**, including servers an image declares in its
`org.outrig.mcp` label; startup names the configured ones it left out. The model could not call
them, and a server placed in the primary container would run beside the interpreter as the same
user, with whatever its `env` resolved readable from Python where `/proc` allows. So no credential
from an MCP or sidecar block reaches the session's containers.

That is the whole of the guarantee. Python can read anything else in the container, and that
includes what podman passes in on its own: by default it forwards the host's proxy variables
(`HTTP_PROXY`, `HTTPS_PROXY`, and their lowercase forms), and a proxy URL can carry a user and
password. The image's own environment, the workspace, and any configured mounts are outside it
too.

Startup checks the model before anything is pulled or started, pulls or builds the image,
starts the container and the interpreter, and prints the image, the model, and the Python
version. The session is recorded like any other, so `outrig ls`, `discard`, and `clean` see it;
its record is written once the interpreter is up, under the name of the container it runs in.

The only slash commands are `/help` and `/quit`, and a blank line is ignored. A failed round is
reported and the session carries on; the messages it had not read are still waiting, so a line
typed next is announced with them. If the interpreter exits -- Python that calls `os._exit`, say
-- nothing typed could reach the agent any more, and the session ends with exit status 1.

Ctrl-C while the agent's Python runs stops that Python, and the round carries on: the code is
cancelled, or interrupted if it has stopped yielding, and the model reads how it ended. Stopping
Python does not stop the processes it started, nor the tasks it was waiting on through
`runtime.wait`; a task it awaited directly is cancelled with it. A second Ctrl-C on the same code
stops waiting for it instead; it keeps the interpreter until it finishes, later submissions are
refused until then, and its result reaches the model with a later call. Ctrl-C while no Python
runs -- the model is being called -- ends the round and returns to the prompt. The conversation
keeps everything before the round, and what the round had already run. At the prompt, one Ctrl-C
starts a fresh line and a second exits. Python left holding the interpreter -- code that caught
its cancellation, say, after a second Ctrl-C -- is stopped by a Ctrl-C at the prompt instead:
cancelled again, and interrupted if it is blocked in a call of its own, with how it ended reaching
the model with its next call. That press counts as the first of the two, so a second Ctrl-C still
exits whether or not the code could be stopped.

Python that keeps its event loop from turning while a CPU stays busy, such as `while True: pass`,
is interrupted after about half a minute without anyone pressing anything, and the model reads
the traceback. Code waiting on a subprocess, a sleep, or a read is left to finish.

Python that allocates more memory than its ceiling -- half of what the container can see -- gets
a `MemoryError` rather than being killed, and the model reads that too. The ceiling is the whole
interpreter's: until the memory is let go, by `del` or by rebinding the name, anything else the
Python tries to allocate fails as well. Programs it starts inherit the same ceiling, each for
itself, and one that needs more can raise its own with `ulimit -d unlimited`. Python that calls
`os._exit` still ends the session.

With `[events] mode = "record"`, the session records what its agent did in
`logs/events.jsonl`: each model call and what it was sent, each submission and how it ended, each
message on the channel, and the tokens each round used (see [Event log](events.md)). At exit the
file is finished before the container stops, and a warning says how many events it could not
take, if any.

`run-new` does not yet take `run`'s `--env`, `--network`, `--volume`, `--max-tool-calls`, or
`--max-tool-result-bytes`, nor a flag for `[events]`. The config keys behind the last two,
`tool-call-max` and `tool-result-max`, apply, as do `[network]`, `[workspace]`, and `[events]`.
`-v` writes no `logs/container.log` here; `-vv` still turns on trace logging.

### `outrig mcp`

Serve the selected image's backing MCP servers as one MCP server over stdio.

```
outrig mcp [--image <name-or-local-ref>]
           [--attach <session-id-or-container-name>]
           [--listen <addr>]
           [--env <KEY=VALUE>]
           [--network <default|audit|filter>]
           [--session-dir <path>]
           [--config <path>]
           [--global-config <path>]
           [--session-root <path>]
           [--volume <host:container[:ro|rw]>]
           [--verbose]

outrig mcp show-merged [--image <name-or-local-ref>]
                       [--attach <session-id-or-container-name>]
                       [--session-dir <path>]
                       [--config <path>]
                       [--global-config <path>]
                       [--session-root <path>]
                       [--verbose]

outrig mcp self
```

| Flag                   | Default                      | Description                           |
|------------------------|------------------------------|---------------------------------------|
| `--image <name-or-ref>`| `default-image`              | Image-config or local Podman ref.     |
| `--attach <id-or-name>`| off                          | Reuse an existing container.          |
| `--listen <addr>`      | off                          | Serve Streamable HTTP at `/mcp`.      |
| `--env <KEY=VALUE>`    | --                           | Override MCP env; repeatable. As run.  |
| `--network <default|audit|filter>`| config, else `default` | Network monitoring mode.       |
| `--session-dir <path>` | `<session-root>/<sid>` (auto)| Specific directory for this server.   |
| `--volume <spec>`      | --                           | Extra bind mount; not with --attach.  |
| `-v`, `--verbose`      | off                          | Print container lifecycle traces.     |

There is no `--agent` flag. `outrig mcp` does not resolve `default-agent`, does not let
`agent.image` participate in image-config selection, and does not read provider API keys.
Image selection is explicit `--image`, then top-level `default-image`, then outrig's built-in
default image-config. Config entries win; an unknown explicit `--image` is treated as a local
Podman image ref. `default-image` remains config-only. The fallback is announced on stderr,
and declaring a reserved name yourself takes it away: `[images.outrig-default]`,
`[images.outrig-default-fs]`, `[images.outrig-default-shell]`,
`[sidecars.outrig-default-fs]`, or `[sidecars.outrig-default-shell]`. `[sidecars.outrig-default]`
is not reserved. Your own `[images.outrig-default]` still resolves after the veto; the other
four leave nothing to fall through to, which is when naming no image is still an error.

Like `outrig run`, `mcp` runs config-less when no `.agents/outrig/config.toml` is found (and no
`--config`): the current directory becomes the workspace root and config comes from the global
file. With no agent to resolve, it reads no model and no provider either, so
`--image <local-ref>` is enough -- and, with the built-in default behind it, not required.

With `--attach`, the value is resolved first as an exact session id under the resolved
session root. A session match supplies the podman container name and default
image-config or raw image ref. If there is no session match, the value is treated as a podman
container name and `--image <name-or-local-ref>` is required.

Startup builds or cache-hits the image and starts the container unless `--attach` is set.
Attach mode validates the existing container with `podman inspect` and borrows it without
stopping or removing it during teardown. Both modes initialize every entry in the merged
MCP table, list their tools, print a banner to stderr, and then speak MCP JSON-RPC on
stdout/stdin. The merged table is the image's `org.outrig.mcp` label plus
`[images.<name>.mcp]` overrides. Build-from-Dockerfile repo images stamp that merged table into
their cache tag on build misses, so `outrig image inspect <name>:<hash>` can report the same
declared servers without serving MCP. All non-protocol output stays off stdout.

When `--listen <addr>` is set, `outrig mcp` serves Streamable HTTP instead of stdio.
TCP addresses are socket addresses such as `127.0.0.1:7331` or `0.0.0.0:7331`;
Unix sockets use `unix:/tmp/outrig.sock`. The HTTP MCP endpoint is always `/mcp`.
Loopback TCP is local-only by default. Non-loopback TCP binds are allowed but print a
warning because this v1 surface has no built-in auth; use an authenticated reverse proxy
before exposing it broadly. Each HTTP MCP client gets its own rmcp session backed by the
same shared outrig proxy and backing MCP processes.

`--network audit` and `--network filter` are supported only for fresh-container `outrig mcp`
sessions. Attach mode cannot retrofit a borrowed container with a new interceptor.

`outrig mcp show-merged` uses the same image-config selection and setup path, but exits after
printing the effective `[mcp]` table to stdout. It is for debugging image-label declarations and
repo-local overrides, not for serving MCP JSON-RPC.

`outrig mcp self` serves host-side self-description tools over stdio. It does not start a
container or require a repo config. Use it from an external MCP-capable AI tool when the built-in
image templates do not fit.

| Trigger or failure                               | Exit |
|--------------------------------------------------|------|
| Stdio client closes stdin after successful startup | `0` |
| SIGINT or SIGTERM after successful startup       | `0`  |
| Config, image, container, or MCP startup failure | `1`  |
| Bad flags or missing required args               | `2`  |

Environment variables used by `outrig mcp`:

| Variable          | Effect                                                          |
|-------------------|-----------------------------------------------------------------|
| `OUTRIG_LOG`      | Preferred `tracing-subscriber` filter.                          |
| `RUST_LOG`        | Fallback tracing filter when `OUTRIG_LOG` is unset.             |
| `XDG_DATA_HOME`   | Default base for `session-root` if not set in config.           |
| `XDG_CONFIG_HOME` | Global config is checked before `~/.outrig/config.toml`.        |

See [Usage -> outrig mcp](../usage/mcp.md) for client configuration and stdio details.

### `outrig build`

Build (or cache-hit) one or more image-config images, without starting an agent.

```
outrig build [--image <name>]
             [--all]
             [--no-cache]
             [--config <path>]
```

- `--image <name>` (default: `default-image`): build a specific named
  image-config.
- `--all` (default: off): build every image-config. Mutually exclusive with
  `--image`.
- `--no-cache` (default: off): force rebuild even on cache hit. Passes `--no-cache`
  to buildah.

See [Usage -> outrig build](../usage/build.md).

### `outrig ls`

List sessions newest-first under the session root.

```
outrig ls [--session-root <path>]
```

Output columns: `ID`, `STARTED`, `DURATION`, `CONTAINER`, `EXIT`. Symlinked sessions (created
via `outrig run --session-dir`) display the symlink target as a hint.

See [Usage -> Sessions](../usage/sessions.md).

### `outrig logs`

Print or follow a session's MCP-server stderr.

```
outrig logs [<session>] [<server>]
            [--follow]
            [--session-dir <path>]
            [--session-root <path>]
```

| Argument / flag        | Default                   | Description                                 |
|------------------------|---------------------------|---------------------------------------------|
| `<session>`            | omit with `--session-dir` | Session id; resolved under session root.    |
| `<server>`             | list available logs       | Server name from `[images.<name>.mcp]`. |
| `--follow`, `-f`       | off                       | Tail the log; continue reading new lines.   |
| `--session-dir <path>` | --                        | Read directly from this session dir.        |

`<session>` and `--session-dir` are mutually exclusive: pass one or the other. Substring match
on `<session>` is allowed if unambiguous.

### `outrig discard`

Delete a session's on-disk record (logs and metadata).

```
outrig discard [<session>] [--yes]
                           [--session-dir <path>]
                           [--session-root <path>]
```

| Argument / flag        | Default                   | Description                              |
|------------------------|---------------------------|------------------------------------------|
| `<session>`            | omit with `--session-dir` | Session id; resolved under session root. |
| `--yes`, `-y`          | off                       | Skip the interactive confirmation.       |
| `--session-dir <path>` | --                        | Discard exactly this session dir.        |

`<session>` and `--session-dir` are mutually exclusive. If the session was created via
`outrig run --session-dir <path>` (i.e. lives at a user-chosen path with a symlink in the root),
discard removes the real directory **and** the symlink. Refuses if the session's container is
still running. Discards the session directory only -- your repository is untouched.

### `outrig clean`

Delete old stopped session records in bulk.

```
outrig clean [--older-than <duration>]
             [--yes]
             [--build-containers]
             [--session <id>]
             [--session-root <path>]
```

| Argument / flag           | Default | Description                             |
|---------------------------|---------|-----------------------------------------|
| `--older-than <duration>` | `30d`   | Remove sessions older than cutoff.      |
| `--yes`, `-y`             | off     | Skip the interactive confirmation.      |
| `--build-containers`      | off     | Also remove buildah working containers. |
| `--session <id>`          | all     | Restrict both sweeps to one session id. |

Durations are positive integers with `s`, `m`, `h`, or `d` units, for example `12h` or `7d`.
The command previews matching sessions and asks once before deleting unless `--yes` is set.
Running sessions are skipped. Sessions created with `--session-dir` remove both the symlink
target and the symlink under the session root. Alongside the record walk, `clean` sweeps
*stray containers*: containers carrying `org.outrig.session` whose session record is gone.
Stopped strays older than the cutoff are removed; running ones are only reported.

`--session <id>` narrows both sweeps to one session: only that session's record is considered
for removal, and only containers labeled `org.outrig.session=<id>` are stray candidates.
Without it the label sweep is machine-wide, which matters because a stray is defined by the
*absence* of a record. A container whose record lives under a different `--session-root` --
another checkout, a parallel CI job, a test suite with its own session root -- has no record
this invocation can see, so an unscoped sweep reads it as a stray and removes it.

`--build-containers` adds a third sweep, for the buildah *working containers* an interrupted
build can leave behind, and needs `buildah` on `PATH`. It removes by container id, subject to
the same `--older-than` cutoff, and previews every container before asking. It is opt-in
because it is the one sweep outrig cannot scope: buildah offers no way to mark these
containers and `buildah containers --filter` selects only on id, name, and ancestor, so the
sweep covers *every* buildah working container past the cutoff, including one you created
yourself with `buildah from`. A container whose creation time cannot be read is reported and
left alone. Do not pair it with a short cutoff while a build is running -- the cutoff is the
only thing keeping it away from a build in flight.

## Exit codes

| Code  | Meaning                                                                |
|-------|------------------------------------------------------------------------|
| `0`   | Success.                                                               |
| `1`   | Generic failure (config, image build, LLM API error, etc.).            |
| `2`   | Misuse (bad flags, missing required args). clap prints the usage line. |
| `130` | Interrupted by SIGINT before subcommand-specific handling.             |

`outrig clean` also exits `1` when the sweep ran but a container it tried to remove is still
there; the run is reported in full, and the container is named on stderr.

## Environment variables

- `[providers.<name>].api-key` references via `${VAR}`: provider API key.
- `OUTRIG_LOG`: preferred `tracing-subscriber` filter, e.g. `OUTRIG_LOG=debug`. When set it
  shadows `RUST_LOG` completely, including a value that filters out more than `RUST_LOG` would.
- `RUST_LOG`: fallback tracing filter when `OUTRIG_LOG` is unset. At `debug`, adds one line per
  buildah/podman invocation (target `outrig::process`) naming the command, its exit status, and
  how long it took.
- `XDG_DATA_HOME`: default base for `session-root` if not set in config.
- `XDG_CONFIG_HOME`: global config is checked here before `~/.outrig/config.toml`.

## See also

- [Usage](../usage/README.md) -- narrative for each subcommand.
- [Reference -> Config](config.md) -- `config.toml` schema.
