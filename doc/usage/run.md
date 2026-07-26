# `outrig run`

`outrig run` is the main subcommand. It walks up from the current directory to find
`.agents/outrig/config.toml`, builds (or cache-hits) the container image, starts the container,
attaches every MCP server defined for the selected image, and drops you into a
stdin/stdout REPL with the agent.

It can also run **config-less**: in a directory with no `.agents/outrig/config.toml`, it uses the
current directory as the workspace and reads the agent from the global config. See
[Config-less runs](#config-less-runs).

## Synopsis

```
outrig run [--agent <name>]
           [--image <name-or-local-ref>]
           [--config <path>]
           [--device <cpu|cuda|cuda:N|metal>]
           [--max-tool-calls <n>]
           [--max-tool-result-bytes <n>]
           [--model <name>]
           [--network <default|audit|filter>]
           [--session-dir <path>]
           [--session-root <path>]
           [--volume <host:container[:ro|rw]>]
           [--verbose]
```

- `--agent <name>` (default: `default-agent`): selects an `[agents.<name>]` block.
- `--image <name-or-local-ref>` (default: agent's `image`, else `default-image`):
  pick an image-config by name; if an explicit `--image` value does not match
  config, treat it as a local Podman image ref and run it without pulling.
- `--config <path>` (default: walks up from cwd; if not found, run config-less -- see
  [Config-less runs](#config-less-runs)): load config from a non-standard location.
- `--device <cpu|cuda|cuda:N|metal>` (default: mistralrs model `device`, else `cpu`):
  override the in-process mistralrs model device for this run.
- `--max-tool-calls <n>` (default: resolved `tool-call-max`, else `50`): override the
  per-turn tool-call max for this run.
- `--max-tool-result-bytes <n>` (default: resolved `tool-result-max`, else `262144`):
  override the per-tool-result byte max for this run.
- `--model <name>` (default: agent's `model`, else `default-model`): select an existing
  `[models.<name>]` entry for this run.
- `--network <default|audit|filter>` (default: config `[network].mode`, else `default`):
  choose Podman's default networking, network audit logging, or global network filtering for
  this run.
- `--session-dir <path>` (default: `<session-root>/<sid>`): this run's specific session
  directory; symlinked from the root.
- `--session-root <path>` (default: `session-root` config, else XDG): root directory
  containing all sessions.
- `--volume <host:container[:ro|rw]>` (repeatable): bind an extra host directory into the
  container, on top of the default workspace mount. Access defaults to read-only; append `:rw`
  for read-write. The host directory must exist; relative host paths resolve against the
  workspace root.
- `--verbose` (default: off): adds buildah/podman command transcripts to stderr and
  `container.log`.

Repeat `--verbose` (`-vv`) to also enable trace-level logs from outrig's own modules for that
invocation.

Normal startup progress is always printed to stderr so slow container or MCP startup is visible.
`--verbose` adds the underlying buildah/podman command transcript; it is not required for the
progress lines. Tracing filters come from `OUTRIG_LOG` first, then `RUST_LOG` if `OUTRIG_LOG`
is unset -- a set `OUTRIG_LOG` shadows `RUST_LOG` entirely.

Three levels of detail, in increasing order:

| Setting               | What it adds                                                     |
|-----------------------|------------------------------------------------------------------|
| (none)                | Progress lines only: one per startup phase, with elapsed time.   |
| `RUST_LOG=debug`      | Every buildah/podman command line, plus its exit code and timing. |
| `--verbose` (`-v`)    | The full command *output* transcript, also written to `container.log`. |
| `-vv`                 | Trace-level logs from outrig's own modules.                      |

A phase that stalls between progress lines is a stalled child process. `RUST_LOG=debug` names
the exact command it is waiting on, which is the fastest way to reproduce the stall by hand:

```sh
$ RUST_LOG=debug outrig run
[outrig] starting container outrig-20260726T170454-da0c
DEBUG outrig::process: spawn command=podman run -d --rm --name outrig-... sleep infinity
```

When `--session-dir` is given, outrig writes this run's `session.json` and `logs/` directly into
`<path>` and creates a symlink at `<session-root>/<sid> -> <path>` so `outrig ls`/`logs`/`discard`
keep working. This lets you launch with a known path and read `session.json` immediately without
looking up an auto-generated id:

```sh
$ outrig run --session-dir /tmp/my-debug-run < prompts.txt
$ cat /tmp/my-debug-run/session.json   # known location, no id lookup needed
```

`--session-dir` refuses if the path already contains a `session.json`.

## Config-less runs

You can run in a directory with no `.agents/outrig/config.toml` at all:

```sh
$ cd /tmp/scratch
$ outrig run --image my-tool:latest --volume "$PWD/data:/data:rw"
```

With no repo config found and no `--config`, outrig uses the current directory as the workspace
root (mounted at `/workspace`) and reads the agent, model, and provider from the global config
(`~/.outrig/config.toml`, or `$XDG_CONFIG_HOME/outrig/config.toml`). Because there is no
`default-image`, you must pass `--image`; an unknown ref is used as a local Podman image and is
never pulled. If the global config has no resolvable agent, startup fails with the usual
`no --agent and no default-agent configured` error.

## What happens, in order

1. **Locate config.** Walks up from the current directory until `.agents/outrig/config.toml` is
   found. If none is found and no `--config` is given, run config-less: the current directory
   becomes the workspace root and config comes from the global file only (see
   [Config-less runs](#config-less-runs)).
2. **Resolve image.** Uses explicit `--image` first. If that value matches
   `[images.<name>]`, OutRig uses the config block; otherwise it must already
   exist in local Podman images. Without explicit `--image`, agent `image` and
   `default-image` still name config blocks only.
3. **Build/cache/probe the image.** Config build images run `buildah build` or
   cache-hit. Config `image-name` images may be pulled. Raw `--image` refs are
   local-only and are checked with `podman image exists`.
4. **Start the container.** `podman run -d --rm --name outrig-<sid> -v <repo>:/workspace:rw
   --userns=keep-id ... <image> sleep infinity`. Any `[workspace.mounts]` and `--volume` entries
   become additional `-v` binds.
5. **Bootstrap the user.** As in-container root, ensure a group with `$(id -g)` and a user with
   `$(id -u)` exist (creating them via `groupadd`/`useradd` if not), and that
   `/home/<user>` exists and is owned by them. See
   [Concepts -> Workspace](../concepts/workspace.md#uidgid-runtime-user-mapping) for the full
   logic.
6. **Start network interception, if enabled.** `--network audit`, `--network filter`, or
   matching `[network].mode` config installs the per-session interceptor and opens
   `<session_dir>/logs/network.jsonl`. Filter mode also enforces global `[network]` policy.
   The default mode skips this step entirely.
7. **Connect MCP servers.** For each entry in `[images.<name>.mcp]`,
   `podman exec -i --user=$(id -u):$(id -g)` the configured command, run the MCP `initialize`
   handshake, and discover tools via `tools/list`.
8. **Resolve agent -> model -> provider.** From `--agent` (or `default-agent`), pick the
   model from `--model`, else `[agents.<a>].model`, else top-level `default-model`. `--model`
   must name an existing `[models.<name>]` entry; it is not a raw provider model identifier. Then
   `[models.<m>].provider`, then `[providers.<p>]`. Resolve the per-turn tool-call max from
   `tool-call-max` and the per-result byte max from `tool-result-max`. For in-process
   mistralrs models, `--device` overrides the model's configured `device` for this run. For
   OpenAI-style models, `--device` is rejected. Then read the API key from the env var named in
   the provider's `api-key`. Build the Rig provider client.
9. **Build the Rig agent.** Dynamic tools from every MCP server's tool list (each prefixed
   `<server>__<tool>`), the agent's `preamble` and sampling params, assembled with
   `AgentBuilder`.
10. **Open the REPL.** Banner on stderr, `> ` prompt, ready for input.

If anything before step 10 fails, `outrig run` reports the error on stderr and exits non-zero
without starting the REPL.

## REPL banner

A typical startup looks like:

```
[outrig] loading config
[outrig] config loaded (1ms)
[outrig] resolving agent and image-config
[outrig] agent/image-config resolved: agent coding, image-config coding (0ms)
[outrig] computing image tag
[outrig] image tag computed: coding:8c2a4f7e91d6b5a3 (32ms)
[outrig] ensuring image coding:8c2a4f7e91d6b5a3
[outrig] image ready: coding:8c2a4f7e91d6b5a3 (cache hit) (18ms)
[outrig] starting container outrig-20260502T103412-3f2a
[outrig] container ready: outrig-20260502T103412-3f2a (620ms)
[outrig] bootstrapping container user
[outrig] container user ready (141ms)
[outrig] reading and merging MCP configuration
[outrig] MCP configuration ready: 2 servers (74ms)
[outrig] MCP fs: initializing
[outrig] MCP fs: initialized (188ms)
[outrig] MCP fs: listing tools
[outrig] MCP fs: tools ready: 3 tools (11ms)
[outrig] building agent
[outrig] agent ready (0ms)
[outrig] agent:             coding (model: fast / provider: openai / gpt-4o-mini)
[outrig] tool-call max:     50
[outrig] tool-result max:   262144 bytes
[outrig] image-config:  coding
[outrig] image:             coding:8c2a4f7e91d6b5a3
[outrig] container started: outrig-20260502T103412-3f2a
[outrig] mcp fs:    initialized (3 tools)
[outrig] mcp shell: initialized (1 tool)
[outrig] tools available: fs__read_file, fs__list_directory, fs__write_file, shell__exec
[outrig] session id: 20260502T103412-3f2a   (Ctrl-D to exit, /help for slash commands)
[outrig] entering REPL
>
```

All startup progress and the banner are on **stderr**. The only thing that ever goes to stdout is
the assistant's natural-language reply. For in-process `mistralrs` models, that reply is flushed
as chunks while the model decodes; OpenAI-compatible providers print the reply when the turn
finishes. The stream separation makes it easy to capture just the model output:

```sh
$ echo "summarise this repo" | outrig run > summary.txt
```

`summary.txt` ends up with only the model's text, nothing else.

## REPL behavior

Each line you type at `>` is one user turn:

```
> add a doc comment to the public function `parse_config` and run cargo check.
[outrig] tool call: fs__read_file({"path": "/workspace/src/config.rs"})
[outrig] tool call: fs__write_file({"path": "/workspace/src/config.rs", ...})
[outrig] tool call: shell__exec({"cmd": "cargo check"})
Done. I added a `///` doc comment describing the function's inputs and the
TOML keys it expects. `cargo check` passed with no warnings. Diff:

  [diff snippet]
>
```

Behind the scenes the agent may make many tool calls per turn -- Rig drives the
model-tool-model loop until the model emits a normal text reply with no tool calls. Tool-call
traces appear on stderr; assistant text is printed on stdout.

Each turn has a tool-call max. The compiled-in default is `50`; a top-level
`tool-call-max` in config changes the default, `[agents.<name>].tool-call-max` overrides it for
one agent, and `outrig run --max-tool-calls <n>` overrides both for the current run. The max is
per turn, so a follow-up prompt starts a fresh count.

Each individual tool result also has a byte max before it is appended to the LLM-visible
conversation history. The compiled-in default is `262144` bytes (256 KiB); a top-level
`tool-result-max` in config changes the default, `[agents.<name>].tool-result-max` overrides it
for one agent, and `outrig run --max-tool-result-bytes <n>` overrides both for the current run.
If a tool returns more than the max, outrig keeps the head of the result and appends a marker
that includes the original size, the max, and a hint to narrow the next query.

When the tool-call max fires, outrig ends the current turn and keeps protocol-valid partial
conversation history. If the model had already asked for the next tool call, outrig records a
tool result saying that call was not executed because the max was reached:

```
[outrig] tool-call iteration max (50) reached; ending turn
[outrig] partial history retained -- send another prompt (e.g. "continue")
        to keep going, or "/reset" to drop it.
```

Type `continue`, or any more specific instruction, to let the next turn pick up from the retained
tool calls with a fresh max. If the skipped tool call is still needed, the model can ask for it
again. Use `/reset` first when you want to drop that history.

The REPL is line-buffered. Multi-line input is not supported in v0.

> **TODO: Incomplete** -- multi-line / paste-mode input is deferred.

## Slash commands

Anything starting with `/` is a REPL command, not a model prompt:

| Command                | Effect                                                            |
|------------------------|-------------------------------------------------------------------|
| `/help`                | Print the slash-command list to stderr.                           |
| `/tools`               | List every tool registered with the agent, with descriptions.     |
| `/reset`               | Clear conversation history; container and MCP servers stay up.    |
| `/sidecar add <name>`  | Start a config-declared `start = "manual"` sidecar.               |
| `/sidecar list`        | Show declared sidecars with status and hosted servers.            |
| `/quit`                | Exit cleanly (same as EOF/Ctrl-D).                                |

```
> /tools
[outrig] tools available (4):
  fs__list_directory   List the contents of a directory.
  fs__read_file        Read a file's contents.
  fs__write_file       Write a file (overwrites existing).
  shell__exec          Run a shell command and return stdout/stderr.
> /reset
[outrig] history cleared
>
```

### Manual sidecars

A sidecar declared with `start = "manual"` (see
[Config -> sidecars](../reference/config.md#sidecarssc)) is planned at session start
-- its image label is merged and its servers are reserved names -- but its container does not
start and its tools are absent. `/sidecar add <name>` starts it mid-session: the container
comes up with session labels, network policy attaches (in audit and filter modes), its MCP
servers connect, and the new tools become available to the agent **on the next turn**. Unknown
names and already-running sidecars are errors, and there is no `/sidecar stop` in v0.

```
> /sidecar list
[outrig] sidecars:
  tools   not started   servers: search
> /sidecar add tools
[outrig] sidecar tools started: outrig-20260504T141907-a83f-tools
[outrig] 1 MCP server connected (search); 2 tools available on the next turn
> /sidecar add tools
[outrig] sidecar "tools" is already running
```

A failed add -- image missing, server crashing on connect -- is reported on stderr and changes
nothing: the container set, tool list, and conversation stay as they were.

## Interrupting and exiting

- **Ctrl-C** during a turn cancels the in-flight LLM/tool call. The REPL prints
  `[outrig] interrupted` to stderr and returns to a `> ` prompt with conversation history intact,
  so you can redirect the agent.
- **Ctrl-D** at an empty prompt ends the session: closes MCP server stdios, stops the container,
  finalizes the session record, exits.
- A second Ctrl-C without an intervening prompt also exits.

```
> please refactor everything   ^C
[outrig] interrupted
> never mind, just summarise the file types in this repo.
...
```

## When something goes wrong

Common failures and what they look like:

```
$ outrig run
error: no .agents/outrig/config.toml found in current directory or any parent
help: run `outrig init` to initialize
```

You're not inside an outrig-configured repo. Either `cd` into one or pass `--config <path>`.

```
$ outrig run
[outrig] image-config: coding
error: image-config "coding" is missing required key: dockerfile
```

The selected `[images.<name>]` block is incomplete. See
[Reference -> Config](../reference/config.md).

```
$ outrig run
[outrig] image-config: coding
[outrig] image:            coding:8c2a4f7e91d6b5a3 (cache hit)
[outrig] container started: outrig-20260501T134412-3f2a
[outrig] mcp fs: error: failed to spawn `mcp-server-filesystem` in container
[outrig] caused by: exec: "mcp-server-filesystem": executable file not found in $PATH
```

The MCP server binary isn't in the image. Install it in the Dockerfile.

```
$ outrig run --image outrig-standard:53e082e721df8ecc
error: --image "outrig-standard:53e082e721df8ecc" did not match any [images.<name>]
       and local podman image "outrig-standard:53e082e721df8ecc" was not found
```

An explicit `--image` ref that is not an image-config is local-only. Build, tag,
or pull it with Podman first, then rerun the command.

```
$ outrig run
[outrig] image-config: coding
[outrig] image:            coding:8c2a4f7e91d6b5a3 (cache hit)
[outrig] container started: outrig-20260501T134412-3f2a
[outrig] mcp fs: initialized
[outrig] mcp shell: initialized
[outrig] model: gpt-4o-mini
[outrig] session id: 20260501T134412-3f2a
> hi
error: LLM call failed: 401 Unauthorized
caused by: provider returned: invalid_api_key
```

`OPENAI_API_KEY` is unset, expired, or pointing at the wrong endpoint.

## See also

- [outrig build](build.md) -- pre-warming the image cache.
- [Sessions](sessions.md) -- what `--session-dir` writes and how to inspect it.
- [Reference -> CLI](../reference/cli.md) -- all flags.
