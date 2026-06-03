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

`--config` resolves in this order: this flag (used verbatim -- no walk-up, no existence check),
then a walk up from cwd looking for `.agents/outrig/config.toml`. The walk stops at the
filesystem root; a `.agents/` directory without an `outrig/config.toml` inside does not
terminate the walk -- outrig keeps looking in parents.

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

Noninteractively scaffold a standalone image project: writes a `Dockerfile`, an
embedded-config `image.toml`, and a `README.md` into a target directory. The directory name
becomes the image ref; other repos consume the built image via `image-name`.

```
outrig image init [<dir>]
                  [--force]
```

| Argument / flag | Default     | Description                                          |
|-----------------|-------------|------------------------------------------------------|
| `<dir>`         | current dir | Project directory; its name becomes the image ref.  |
| `--force`       | off         | Overwrite the generated files if they already exist. |

See [Usage -> outrig image](../usage/image.md#outrig-image-init).

### `outrig run`

Start an interactive agent session.

```
outrig run [--agent <name>]
           [--image <name>]
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
           [--verbose]
```

- `--agent <name>` (default: `default-agent`): selects an `[agents.<name>]` block.
- `--image <name>` (default: from agent or `default-image`): image-config to
  launch.
- `--env <KEY=VALUE>` (repeatable): add or override env vars for MCP servers. `KEY=VALUE`
  applies to every server; `SERVER:KEY=VALUE` targets a single server by name. Values support
  the `${VAR}` host-env-reference syntax described in
  [config.md#mcp-env-value-syntax](config.md#mcp-env-value-syntax). Within a scope, last wins
  on duplicate keys. Precedence per key: config-file env < global `--env` < per-server `--env`.
- `--device <cpu|cuda|cuda:N|metal>` (default: mistralrs model `device`, else `cpu`): override
  the in-process mistralrs model device for this run. Rejected for OpenAI-style models.
- `--max-tool-calls <n>` (default: resolved `tool-call-max`, else `50`): per-turn tool-call
  max.
- `--max-tool-result-bytes <n>` (default: resolved `tool-result-max`, else `262144`):
  per-tool-result byte max.
- `--model <name>` (default: agent's `model`, else `default-model`): configured
  `[models.<name>]` entry to use for this run. This is not a raw provider model identifier.
- `--network <default|audit|filter>` (default: config `[network].mode`, else `default`):
  choose Podman's default networking, network audit logging, or global network filtering for
  this session.
- `--session-dir <path>` (default: `<session-root>/<sid>`): specific directory for this run.
- `-v`, `--verbose` (default: off): print container lifecycle traces.

When `--session-dir` is given, outrig writes this run's `session.json` and `logs/` directly
under `<path>`, and additionally creates a symlink `<session-root>/<sid> -> <path>` so
`outrig ls`/`logs`/`discard`/`clean` still find it. When omitted, outrig auto-generates a
session id and writes to `<session-root>/<sid>/` directly.

Reads the global and repo configs, resolves agent -> model -> provider, builds the image
(cache-hit if possible), starts the container, attaches every MCP server, opens the REPL. Exits
when stdin reaches EOF, when the user types `/quit`, or after a second Ctrl-C.

See [Usage -> outrig run](../usage/run.md) for REPL details.

### `outrig mcp`

Serve the selected image-config's backing MCP servers as one MCP server over stdio.

```
outrig mcp [--image <name>]
           [--attach <session-id-or-container-name>]
           [--listen <addr>]
           [--env <KEY=VALUE>]
           [--network <default|audit|filter>]
           [--session-dir <path>]
           [--config <path>]
           [--global-config <path>]
           [--session-root <path>]
           [--verbose]

outrig mcp show-merged [--image <name>]
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
| `--image <name>`       | `default-image`              | Image-config to launch.               |
| `--attach <id-or-name>`| off                          | Reuse an existing container.          |
| `--listen <addr>`      | off                          | Serve Streamable HTTP at `/mcp`.      |
| `--env <KEY=VALUE>`    | --                           | Override MCP env; repeatable. As run.  |
| `--network <default|audit|filter>`| config, else `default` | Network monitoring mode.       |
| `--session-dir <path>` | `<session-root>/<sid>` (auto)| Specific directory for this server.   |
| `-v`, `--verbose`      | off                          | Print container lifecycle traces.     |

There is no `--agent` flag. `outrig mcp` does not resolve `default-agent`, does not let
`agent.image` participate in image-config selection, and does not read provider API keys.
Image-config selection is `--image`, then top-level `default-image`, then an error.

With `--attach`, the value is resolved first as an exact session id under the resolved
session root. A session match supplies the podman container name and default
image-config. If there is no session match, the value is treated as a podman
container name and `--image <name>` is required.

Startup builds or cache-hits the image and starts the container unless `--attach` is set.
Attach mode validates the existing container with `podman inspect` and borrows it without
stopping or removing it during teardown. Both modes initialize every entry in the merged
MCP table, list their tools, print a banner to stderr, and then speak MCP JSON-RPC on
stdout/stdin. The merged table is image `/etc/outrig/image.toml` plus
`[images.<name>.mcp]` overrides. All non-protocol output stays off stdout.

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
printing the effective `[mcp]` table to stdout. It is for debugging embedded image config and
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
             [--session-root <path>]
```

| Argument / flag            | Default | Description                          |
|----------------------------|---------|--------------------------------------|
| `--older-than <duration>`  | `30d`   | Remove sessions older than cutoff.   |
| `--yes`, `-y`              | off     | Skip the interactive confirmation.   |

Durations are positive integers with `s`, `m`, `h`, or `d` units, for example `12h` or `7d`.
The command previews matching sessions and asks once before deleting unless `--yes` is set.
Running sessions are skipped. Sessions created with `--session-dir` remove both the symlink
target and the symlink under the session root.

## Exit codes

| Code  | Meaning                                                                |
|-------|------------------------------------------------------------------------|
| `0`   | Success.                                                               |
| `1`   | Generic failure (config, image build, LLM API error, etc.).            |
| `2`   | Misuse (bad flags, missing required args). clap prints the usage line. |
| `130` | Interrupted by SIGINT before subcommand-specific handling.             |

## Environment variables

- `[providers.<name>].api-key` references via `${VAR}`: provider API key.
- `OUTRIG_LOG`: preferred `tracing-subscriber` filter, e.g. `OUTRIG_LOG=debug`.
- `RUST_LOG`: fallback tracing filter when `OUTRIG_LOG` is unset.
- `XDG_DATA_HOME`: default base for `session-root` if not set in config.
- `XDG_CONFIG_HOME`: global config is checked here before `~/.outrig/config.toml`.

## See also

- [Usage](../usage/README.md) -- narrative for each subcommand.
- [Reference -> Config](config.md) -- `config.toml` schema.
