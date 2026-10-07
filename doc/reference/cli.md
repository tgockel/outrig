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

The walk takes only a config of your own. The file, the `.agents/` and `outrig/` directories
above it, and the directory they sit in must all belong to the user running outrig; a symlink
counts as itself, not as what it points to. Anyone who owns one of them could have written the
config, and the config decides what is mounted, which commands run, and where API keys go. So a
config found anywhere else -- `/tmp/.agents/outrig/config.toml`, say, planted by another user of
a shared host -- stops the command with an error naming it, as git does for a repository another
user owns. It is not read, and not skipped in favor of a config-less run. The walk finds the
nearest config first, so `outrig init` in your own directory puts one between you and it. To
read a config you do not own on purpose, such as a checkout shared with your team, name it with
`--config`, which is never refused for its owner.

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

`--session-root` resolves in this order: this flag, then `session-root` in the global config, then
`<XDG_DATA_HOME>/outrig/sessions/`. The config key is global-only, so `ls`, `logs`, `discard`, and
`clean` read no repo config at all and ignore `--config`: they find the same root from any
directory.

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

Start an interactive agent session.

```
outrig run [--agent <name>]
           [--image <name-or-local-ref>]
           [--config <path>]
           [--device <cpu|cuda|cuda:N|metal>]
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
- `--device <cpu|cuda|cuda:N|metal>` (default: mistralrs model `device`, else `cpu`):
  **deprecated** with the in-process backend it selects hardware for; override the in-process
  mistralrs model device for this run. Rejected for OpenAI-style models, and
  for an alias spanning more than one candidate -- it selects hardware for one in-process
  model, and an alias may span several styles. A single-target alias is accepted.
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
- `--session-dir <path>` (default: `<session-root>/<sid>`): specific directory for this run;
  must be empty or not exist yet.
- `--volume <host:container[:ro|rw]>` (repeatable): bind an extra host directory into the
  container, beyond the default workspace mount. Read-only unless `:rw`; host dir must exist.
- `-v`, `--verbose` (default: off): print container lifecycle traces.

When `--session-dir` is given, outrig writes this run's `session.json`, `logs/`, and
`outrig-enter` (only for a `view = "primary"` sidecar) directly under `<path>`, and
additionally creates a symlink `<session-root>/<sid> -> <path>` so
`outrig ls`/`logs`/`discard`/`clean` still find it. A `<path>` that does not exist is created,
with any missing parents; one that already holds anything is refused. When omitted, outrig
auto-generates a session id and writes to `<session-root>/<sid>/` directly.

Reads the global and repo configs, resolves agent -> model -> provider, builds the image
(cache-hit if possible), starts the container, attaches every MCP server, opens the REPL. Exits
when stdin reaches EOF, when the user types `/quit`, or after a second Ctrl-C. A Ctrl-C before
the first prompt, or a SIGTERM or SIGHUP at any point, ends the session the same way -- its
containers stopped, its record finalized -- and exits `130`, `143`, or `129`. An interactive run
gets a line editor with in-session history; a piped or redirected one reads plain lines.

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
| `--session-dir <path>` | `<session-root>/<sid>` (auto)| New or empty dir for this server.     |
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

| Trigger or failure                                  | Exit                  |
|-----------------------------------------------------|-----------------------|
| Stdio client closes stdin after successful startup  | `0`                   |
| SIGINT, SIGTERM, or SIGHUP after successful startup | `0`                   |
| SIGINT, SIGTERM, or SIGHUP during startup           | `130`, `143`, `129`   |
| Config, image, container, or MCP startup failure    | `1`                   |
| Bad flags or missing required args                  | `2`                   |

Environment variables used by `outrig mcp`:

| Variable          | Effect                                                          |
|-------------------|-----------------------------------------------------------------|
| `OUTRIG_LOG`      | Preferred `tracing-subscriber` filter.                          |
| `RUST_LOG`        | Fallback tracing filter when `OUTRIG_LOG` is unset.             |
| `XDG_DATA_HOME`   | Default base for `session-root` if not set in global config.    |
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

Delete a session's on-disk record (`session.json`, `logs/`, `outrig-enter`) and its directory.

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
discard removes the real directory **and** the symlink; `--session-dir` given that symlink
discards the directory it names. Discard removes the record's three names and nothing else: a
directory that also holds anything else stays, and discard names what it kept. Refuses if the
session's container is still running.

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
target and the symlink under the session root. As with `discard`, a directory that holds
anything besides the record stays. Alongside the record walk, `clean` sweeps
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
| `129` | Ended by SIGHUP.                                                       |
| `130` | Ended by SIGINT.                                                       |
| `143` | Ended by SIGTERM.                                                      |

Once `outrig run` or `outrig mcp` has written its session record, it catches all three signals:
it stops the session's containers and finalizes the record, which keeps the same code, before it
exits. `outrig mcp` exits `0` on a signal once its startup has finished; see
[`outrig mcp`](#outrig-mcp). `outrig image build` catches them once it starts its validation
container, and stops that container before it exits.

`outrig clean` also exits `1` when the sweep ran but a container it tried to remove is still
there; the run is reported in full, and the container is named on stderr.

## Environment variables

- `[providers.<name>].api-key` references via `${VAR}`: provider API key.
- `OUTRIG_LOG`: preferred `tracing-subscriber` filter, e.g. `OUTRIG_LOG=debug`. When set it
  shadows `RUST_LOG` completely, including a value that filters out more than `RUST_LOG` would.
- `RUST_LOG`: fallback tracing filter when `OUTRIG_LOG` is unset. At `debug`, adds one line per
  buildah/podman invocation (target `outrig::process`) naming the command, its exit status, and
  how long it took.
- `XDG_DATA_HOME`: default base for `session-root` if not set in the global config.
- `XDG_CONFIG_HOME`: global config is checked here before `~/.outrig/config.toml`.

## See also

- [Usage](../usage/README.md) -- narrative for each subcommand.
- [Reference -> Config](config.md) -- `config.toml` schema.
