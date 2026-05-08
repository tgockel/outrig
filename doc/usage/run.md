# `outrig run`

`outrig run` is the main subcommand. It walks up from the current directory to find
`.agents/outrig/config.toml`, builds (or cache-hits) the container image, starts the container,
attaches every MCP server defined for the selected container-config, and drops you into a
stdin/stdout REPL with the agent.

## Synopsis

```
outrig run [--agent <name>]
           [--container <name>]
           [--config <path>]
           [--max-tool-calls <n>]
           [--max-tool-result-bytes <n>]
           [--session-dir <path>]
           [--session-root <path>]
           [--verbose]
```

- `--agent <name>` (default: `default-agent`): selects an `[agents.<name>]` block.
- `--container <name>` (default: agent's `container`, else `default-container`): pick a
  container.
- `--config <path>` (default: walks up from cwd): use from outside the repo or
  non-standard locations.
- `--max-tool-calls <n>` (default: resolved `tool-call-cap`, else `50`): override the
  per-turn tool-call cap for this run.
- `--max-tool-result-bytes <n>` (default: resolved `tool-result-cap`, else `262144`):
  override the per-tool-result byte cap for this run.
- `--session-dir <path>` (default: `<session-root>/<sid>`): this run's specific session
  directory; symlinked from the root.
- `--session-root <path>` (default: `session-root` config, else XDG): root directory
  containing all sessions.
- `--verbose` (default: off): adds buildah/podman command transcripts to stderr and
  `container.log`.

Repeat `--verbose` (`-vv`) to also enable trace-level logs from outrig's own modules for that
invocation.

Normal startup progress is always printed to stderr so slow container or MCP startup is visible.
`--verbose` adds the underlying buildah/podman command transcript; it is not required for the
progress lines. Tracing filters come from `OUTRIG_LOG` first, then `RUST_LOG` if `OUTRIG_LOG`
is unset.

When `--session-dir` is given, outrig writes this run's `session.json` and `logs/` directly into
`<path>` and creates a symlink at `<session-root>/<sid> -> <path>` so `outrig ls`/`logs`/`discard`
keep working. This lets you launch with a known path and read `session.json` immediately without
looking up an auto-generated id:

```sh
$ outrig run --session-dir /tmp/my-debug-run < prompts.txt
$ cat /tmp/my-debug-run/session.json   # known location, no id lookup needed
```

`--session-dir` refuses if the path already contains a `session.json`.

## What happens, in order

1. **Locate config.** Walks up from the current directory until `.agents/outrig/config.toml` is
   found, or fails.
2. **Resolve container-config.** Uses `--container` if given, otherwise
   `default-container`. The selected block must exist.
3. **Build (or cache-hit) the image.** Runs `buildah build`. If the cache hash matches an
   existing tag, no rebuild.
4. **Start the container.** `podman run -d --rm --name outrig-<sid> -v <repo>:/workspace:rw
   --userns=keep-id ... <image> sleep infinity`.
5. **Bootstrap the user.** As in-container root, ensure a group with `$(id -g)` and a user with
   `$(id -u)` exist (creating them via `groupadd`/`useradd` if not), and that
   `/home/<user>` exists and is owned by them. See
   [Concepts -> Workspace](../concepts/workspace.md#uidgid-runtime-user-mapping) for the full
   logic.
6. **Connect MCP servers.** For each entry in `[containers.<name>.mcp]`,
   `podman exec -i --user=$(id -u):$(id -g)` the configured command, run the MCP `initialize`
   handshake, and discover tools via `tools/list`.
7. **Resolve agent -> model -> provider.** From `--agent` (or `default-agent`), look up
   `[agents.<a>].model` -- if unset, fall back to top-level `default-model`. Then
   `[models.<m>].provider`, then `[providers.<p>]`. Resolve the per-turn tool-call cap from
   `tool-call-cap` and the per-result byte cap from `tool-result-cap`, then read the API key
   from the env var named in the provider's `api-key`. Build the Rig provider client.
8. **Build the Rig agent.** Dynamic tools from every MCP server's tool list (each prefixed
   `<server>__<tool>`), the agent's `preamble` and sampling params, assembled with
   `AgentBuilder`.
9. **Open the REPL.** Banner on stderr, `> ` prompt, ready for input.

If anything before step 9 fails, `outrig run` reports the error on stderr and exits non-zero
without starting the REPL.

## REPL banner

A typical startup looks like:

```
[outrig] loading config
[outrig] config loaded (1ms)
[outrig] resolving agent and container
[outrig] agent/container resolved: agent coding, container coding (0ms)
[outrig] computing image tag
[outrig] image tag computed: outrig-cache:8c2a4f7e91d6b5a3 (32ms)
[outrig] ensuring image outrig-cache:8c2a4f7e91d6b5a3
[outrig] image ready: outrig-cache:8c2a4f7e91d6b5a3 (cache hit) (18ms)
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
[outrig] tool-call cap:     50
[outrig] tool-result cap:   262144 bytes
[outrig] container-config:  coding
[outrig] image:             outrig-cache:8c2a4f7e91d6b5a3
[outrig] container started: outrig-20260502T103412-3f2a
[outrig] mcp fs:    initialized (3 tools)
[outrig] mcp shell: initialized (1 tool)
[outrig] tools available: fs__read_file, fs__list_directory, fs__write_file, shell__exec
[outrig] session id: 20260502T103412-3f2a   (Ctrl-D to exit, /help for slash commands)
[outrig] entering REPL
>
```

All startup progress and the banner are on **stderr**. The only thing that ever goes to stdout is
the assistant's natural-language reply. This separation makes it easy to capture just the model
output:

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
traces appear on stderr; the final text reply is printed on stdout.

Each turn has a tool-call cap. The compiled-in default is `50`; a top-level
`tool-call-cap` in config changes the default, `[agents.<name>].tool-call-cap` overrides it for
one agent, and `outrig run --max-tool-calls <n>` overrides both for the current run. The cap is
per turn, so a follow-up prompt starts a fresh count.

Each individual tool result also has a byte cap before it is appended to the LLM-visible
conversation history. The compiled-in default is `262144` bytes (256 KiB); a top-level
`tool-result-cap` in config changes the default, `[agents.<name>].tool-result-cap` overrides it
for one agent, and `outrig run --max-tool-result-bytes <n>` overrides both for the current run.
If a tool returns more than the cap, outrig keeps the head of the result and appends a marker
that includes the original size, the cap, and a hint to narrow the next query.

When the tool-call cap fires, outrig ends the current turn and keeps protocol-valid partial
conversation history. If the model had already asked for the next tool call, outrig records a
tool result saying that call was not executed because the cap was reached:

```
[outrig] tool-call iteration cap (50) reached; ending turn
[outrig] partial history retained -- send another prompt (e.g. "continue")
        to keep going, or "/reset" to drop it.
```

Type `continue`, or any more specific instruction, to let the next turn pick up from the retained
tool calls with a fresh cap. If the skipped tool call is still needed, the model can ask for it
again. Use `/reset` first when you want to drop that history.

The REPL is line-buffered. Multi-line input is not supported in v0.

> **TODO: Incomplete** -- multi-line / paste-mode input is deferred.

## Slash commands

Anything starting with `/` is a REPL command, not a model prompt:

| Command  | Effect                                                                  |
|----------|-------------------------------------------------------------------------|
| `/help`  | Print the slash-command list to stderr.                                 |
| `/tools` | List every tool currently registered with the agent, with descriptions. |
| `/reset` | Clear conversation history; container and MCP servers stay up.          |
| `/quit`  | Exit cleanly (same as EOF/Ctrl-D).                                      |

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
```

You're not inside an outrig-configured repo. Either `cd` into one or pass `--config <path>`.

```
$ outrig run
[outrig] container-config: coding
error: container-config "coding" is missing required key: dockerfile
```

The selected `[containers.<name>]` block is incomplete. See
[Reference -> Config](../reference/config.md).

```
$ outrig run
[outrig] container-config: coding
[outrig] image:            outrig-cache:8c2a4f7e91d6b5a3 (cache hit)
[outrig] container started: outrig-20260501T134412-3f2a
[outrig] mcp fs: error: failed to spawn `mcp-server-filesystem` in container
[outrig] caused by: exec: "mcp-server-filesystem": executable file not found in $PATH
```

The MCP server binary isn't in the image. Install it in the Dockerfile.

```
$ outrig run
[outrig] container-config: coding
[outrig] image:            outrig-cache:8c2a4f7e91d6b5a3 (cache hit)
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
