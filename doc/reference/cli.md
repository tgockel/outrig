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
container, MCP, or agent behavior.

## Subcommands

### `outrig init`

Idempotent end-to-end setup orchestrator. Runs `outrig config init` if the global config is
missing, writes `.agents/outrig/config.toml` if it doesn't exist, then offers to call
`outrig container add` in a loop.

```
outrig init [--force]
```

| Flag      | Default | Description                                                                |
|-----------|---------|----------------------------------------------------------------------------|
| `--force` | off     | Overwrite existing files. Propagates to `config init` and `container add`. |

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

### `outrig container add`

Interactively scaffold a container-config: writes a Dockerfile under
`.agents/outrig/containers/<name>/Dockerfile` and adds the matching `[containers.<name>]` and
`[containers.<name>.mcp]` blocks to the repo config. The first subcommand of the `outrig
container` group; future subcommands (`container ls`, `container rm`) are deferred.

```
outrig container add [<name>]
                     [--force]
```

| Argument / flag | Default  | Description                             |
|-----------------|----------|-----------------------------------------|
| `<name>`        | prompted | Container-config name.                  |
| `--force`       | off      | Overwrite existing files for this name. |

See [Usage -> outrig container](../usage/container.md).

### `outrig run`

Start an interactive agent session.

```
outrig run [--agent <name>]
           [--container <name>]
           [--config <path>]
           [--global-config <path>]
           [--max-tool-calls <n>]
           [--session-dir <path>]
           [--session-root <path>]
           [--verbose]
```

| Flag                   | Default                           | Description                         |
|------------------------|-----------------------------------|-------------------------------------|
| `--agent <name>`       | `default-agent`                   | Selects an `[agents.<name>]` block. |
| `--container <name>`   | from agent or `default-container` | Container-config to launch.         |
| `--max-tool-calls <n>` | resolved `tool-call-cap`, else 50 | Per-turn tool-call cap.             |
| `--session-dir <path>` | `<session-root>/<sid>` (auto)     | Specific directory for this run.    |
| `-v`, `--verbose`      | off                               | Print container lifecycle traces.   |

When `--session-dir` is given, outrig writes this run's `session.json` and `logs/` directly
under `<path>`, and additionally creates a symlink `<session-root>/<sid> -> <path>` so
`outrig ls`/`logs`/`discard` still find it. When omitted, outrig auto-generates a session id
and writes to `<session-root>/<sid>/` directly.

Reads the global and repo configs, resolves agent -> model -> provider, builds the image
(cache-hit if possible), starts the container, attaches every MCP server, opens the REPL. Exits
when stdin reaches EOF, when the user types `/quit`, or after a second Ctrl-C.

See [Usage -> outrig run](../usage/run.md) for REPL details.

### `outrig mcp`

Serve the selected container-config's backing MCP servers as one MCP server over stdio.

```
outrig mcp [--container <name>]
           [--session-dir <path>]
           [--config <path>]
           [--global-config <path>]
           [--session-root <path>]
           [--verbose]
```

| Flag                   | Default                       | Description                         |
|------------------------|-------------------------------|-------------------------------------|
| `--container <name>`   | `default-container`           | Container-config to launch.         |
| `--session-dir <path>` | `<session-root>/<sid>` (auto) | Specific directory for this server. |
| `-v`, `--verbose`      | off                           | Print container lifecycle traces.   |

There is no `--agent` flag. `outrig mcp` does not resolve `default-agent`, does not let
`agent.container` participate in container selection, and does not read provider API keys.
Container selection is `--container`, then top-level `default-container`, then an error.

Startup builds or cache-hits the image, starts the container, initializes every entry in
`[containers.<name>.mcp]`, lists their tools, prints a banner to stderr, and then speaks MCP
JSON-RPC on stdout/stdin. All non-protocol output stays off stdout.

| Trigger or failure                               | Exit |
|--------------------------------------------------|------|
| Client closes stdin after successful startup     | `0`  |
| SIGINT or SIGTERM after successful startup       | `0`  |
| Config, image, container, or MCP startup failure | `1`  |
| Bad flags or missing required args               | `2`  |

Environment variables used by `outrig mcp`:

| Variable          | Effect                                                          |
|-------------------|-----------------------------------------------------------------|
| `OUTRIG_LOG`      | `tracing-subscriber` filter. Trace output still goes to stderr. |
| `XDG_DATA_HOME`   | Default base for `session-root` if not set in config.           |
| `XDG_CONFIG_HOME` | Global config is checked before `~/.outrig/config.toml`.        |

See [Usage -> outrig mcp](../usage/mcp.md) for client configuration and stdio details.

### `outrig build`

Build (or cache-hit) one or more container-config images, without starting an agent.

```
outrig build [--container <name>]
             [--all]
             [--no-cache]
             [--config <path>]
```

- `--container <name>` (default: `default-container`): build a specific named
  container-config.
- `--all` (default: off): build every container-config. Mutually exclusive with
  `--container`.
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
| `<server>`             | list available logs       | Server name from `[containers.<name>.mcp]`. |
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

## Exit codes

| Code  | Meaning                                                                |
|-------|------------------------------------------------------------------------|
| `0`   | Success.                                                               |
| `1`   | Generic failure (config, image build, LLM API error, etc.).            |
| `2`   | Misuse (bad flags, missing required args). clap prints the usage line. |
| `130` | Interrupted by SIGINT before subcommand-specific handling.             |

## Environment variables

- `[providers.<name>].api-key` references via `${VAR}`: provider API key.
- `OUTRIG_LOG`: `tracing-subscriber` filter, e.g. `OUTRIG_LOG=debug`.
- `XDG_DATA_HOME`: default base for `session-root` if not set in config.
- `XDG_CONFIG_HOME`: global config is checked here before `~/.outrig/config.toml`.

## See also

- [Usage](../usage/README.md) -- narrative for each subcommand.
- [Reference -> Config](config.md) -- `config.toml` schema.
