# `outrig run`

`outrig run` is the main subcommand. It walks up from the current directory to find
`.agents/outrig/config.toml`, builds (or cache-hits) the container image, starts the container,
attaches every MCP server defined for the selected image, and drops you into a
stdin/stdout REPL with the agent.

It can also run **config-less**: in a directory with no `.agents/outrig/config.toml`, it uses the
current directory as the workspace and reads the agent from the global config. See
[Config-less runs](#config-less-runs).

`outrig run` takes no script: `outrig run-new <script.py> [args...]` runs a Python program on the
host, a preview of a second way to drive OutRig. See
[Running a Python program on the host](#running-a-python-program-on-the-host).

## Synopsis

```
outrig run [--agent <name>]
           [--image <name-or-local-ref>]
           [--config <path>]
           [--env <KEY=VALUE>]
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
  neither, the session runs with no agent: no preamble, and the image comes from `--image`,
  `default-image`, or [the built-in default](#the-built-in-default-image).
- `--image <name-or-local-ref>` (default: agent's `image`, else `default-image`):
  pick an image-config by name; if an explicit `--image` value does not match
  config, treat it as a local Podman image ref and run it without pulling.
- `--config <path>` (default: walks up from cwd; if not found, run config-less -- see
  [Config-less runs](#config-less-runs)): read the repo config from this file instead. See
  [Reference -> CLI](../reference/cli.md#global-flags) for which repo the session runs against.
- `--env <KEY=VALUE>` (repeatable): add or override env vars for the MCP servers this session
  starts. `KEY=VALUE` applies to every server; `SERVER:KEY=VALUE` targets one server by name.
  Values take the same `${VAR}` host-env-reference syntax config files use, described in
  [Reference -> Config](../reference/config.md#mcp-env-value-syntax). You typed them, so
  they are not held to the global config's [`[repo]` `env-allow`](../reference/config.md#repo)
  list, which governs the references in a repo config and in an image's label. Within a scope
  the last value given for a key wins; across scopes the per-key precedence is config-file env <
  global `--env` < per-server `--env`. A `SERVER:` prefix naming a server the image does not
  declare is an error.
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
  directory, which must be empty or not exist yet; symlinked from the root.
- `--session-root <path>` (default: global config's `session-root`, else XDG): root directory
  containing all sessions.
- `--volume <host:container[:ro|rw]>` (repeatable): bind an extra host directory into the
  container, on top of the default workspace mount. Access defaults to read-only; append `:rw`
  for read-write. The host directory must exist; relative host paths resolve against the
  workspace root, and a leading `~` is your home directory.
- `--verbose` (default: off): adds buildah/podman command transcripts to stderr and
  `container.log`.

Repeat `--verbose` (`-vv`) to also enable trace-level logs from outrig's own modules for that
invocation.

Normal startup progress is always printed to stderr so slow container or MCP startup is visible.
`--verbose` adds the underlying buildah/podman command transcript; it is not required for the
progress lines. Tracing filters come from `OUTRIG_LOG` first, then `RUST_LOG` if `OUTRIG_LOG`
is unset -- a set `OUTRIG_LOG` shadows `RUST_LOG` entirely.

When the image is not cached, the `buildah build` (or the `podman pull` for an `image-name`
config) that produces it is shown as it runs, as `[buildah]` / `[podman]` lines between
`ensuring image` and `image ready`, so a first run does not look hung. `--verbose` is not
required for that either; it adds the command line itself and writes everything to
`container.log`. Sidecar images report the same way, one at a time.

Three levels of detail, in increasing order:

| Setting               | What it adds                                                           |
|-----------------------|------------------------------------------------------------------------|
| (none)                | Progress lines, plus the output of a build or pull on a cache miss.    |
| `RUST_LOG=debug`      | Every buildah/podman command line, plus its exit code and timing.      |
| `--verbose` (`-v`)    | The full command *output* transcript, also written to `container.log`. |
| `-vv`                 | Trace-level logs from outrig's own modules.                            |

A phase that stalls between progress lines is a stalled child process. `RUST_LOG=debug` names
the exact command it is waiting on, which is the fastest way to reproduce the stall by hand:

```sh
$ RUST_LOG=debug outrig run
[outrig] starting container outrig-20260726T170454-da0c
DEBUG outrig::process: spawn command=podman run -d --rm --name outrig-... sleep infinity
```

An env value that came from a `${VAR}` reference appears in that line as `'KEY=${VAR}'`, never
as its value. Pasted as shown, the single quotes would hand podman that literal text; to rerun
the command by hand, write it as `"KEY=${VAR}"` so your shell fills the value in.

When `--session-dir` is given, outrig writes this run's `session.json`, `logs/`, and (for a
`view = "primary"` sidecar) `outrig-enter` directly into `<path>` and creates a symlink at
`<session-root>/<sid> -> <path>` so `outrig ls`/`logs`/`discard` keep working. This lets you
launch with a known path and read `session.json` immediately without looking up an
auto-generated id:

```sh
$ outrig run --session-dir /tmp/my-debug-run < prompts.txt
$ cat /tmp/my-debug-run/session.json   # known location, no id lookup needed
```

`<path>` has to be empty or not exist yet; outrig creates it, with any missing parents. A path
that already holds anything -- even a `.gitkeep`, or an earlier run's `session.json` -- is
refused, because `outrig discard` removes the directory along with the session. See
[Sessions](sessions.md#outrig-discard).

## Config-less runs

You can run in a directory with no `.agents/outrig/config.toml` at all:

```sh
$ cd /tmp/scratch
$ outrig run --image my-tool:latest --volume "$PWD/data:/data:rw"
```

With no repo config found and no `--config`, outrig uses the current directory as the workspace
root (mounted at `/workspace`) and reads the agent, model, and provider from the global config
(`~/.outrig/config.toml`, or `$XDG_CONFIG_HOME/outrig/config.toml` when that is an absolute
path). You need not pass `--image`: with nothing else naming one, the session falls through to
[the built-in default image-config](#the-built-in-default-image). If you do pass `--image`, an
unknown ref is used as a local Podman image and is never pulled. The current directory can't be
your home directory, one above it, or one that contains the global config, unless the *global*
config declares `[workspace] host-path`; see
[Concepts -> Workspace](../concepts/workspace.md#whats-mounted-what-isnt).

What the global config must still supply is a **model**. The session does not need an agent --
without one it runs with no preamble -- but `--model` alone is not enough on its own: the name
is looked up in `[models.<name>]`, and that entry's provider in `[providers.<name>]`. So the
supported shape is *global config, no repo config*, which is exactly where
[`outrig config init`](../quickstart.md) leaves you:

```sh
$ outrig config init     # once per machine: provider, model, API key
$ cd ~/some/other/repo   # no .agents/outrig/ here
$ outrig run
```

### The built-in default image

The image-config outrig falls back to is `outrig-default`: a
`docker.io/library/buildpack-deps:bookworm-scm` container (Debian with `git`, `curl`, and
`ca-certificates`) with two MCP servers attached as `view = "primary"` sidecars -- `fs` for
reading and writing files, and `shell` for running commands. Both run against the primary
container's filesystem, so the shell sees the same tree `fs` edits. The banner marks it:

```
[outrig] no --image, agent image, or default-image configured; using outrig's built-in default
[outrig] image-config:  outrig-default (built-in default)
```

A session on the built-in default also gets outrig's own documentation tools
(`outrig__list_docs`, `outrig__get_doc`, `outrig__validate_config`, and others), so you can ask
the agent how to configure outrig and it will answer from the shipped docs for your build. A
repo with its own image-config does not get them, and its tool list is unchanged.

`outrig-default` is also usable as an explicit `--image`, so you can opt into it from a repo
that has its own `default-image`. The first such run pulls two images and builds a third; to
pay that up front rather than inside your first session, see
[`outrig build`](build.md#synopsis).

Two caveats. `outrig-default`, `outrig-default-fs`, and `outrig-default-shell` are reserved
names -- declare any of them yourself and outrig steps aside entirely, with a note saying so.
And if outrig was built without the `<arch>-unknown-linux-musl` target, the `outrig-enter`
launcher is missing: `fs` then falls back to a read-write bind mount of the workspace (same
tools) and `shell` is unavailable. Full details in
[Reference -> Config](../reference/config.md#the-built-in-default-image-config).

## What happens, in order

1. **Locate config.** Walks up from the current directory until `.agents/outrig/config.toml` is
   found. If none is found and no `--config` is given, run config-less: the current directory
   becomes the workspace root and config comes from the global file only (see
   [Config-less runs](#config-less-runs)). A config found that another user owns is refused, not
   read (see [Reference -> CLI](../reference/cli.md#global-flags)). Startup names the config
   file it read and the workspace it will mount before anything is built or started. A key this
   outrig does not know -- a typo, or one a newer outrig reads from the same global config -- is
   ignored with a `[outrig] warning:` line naming the file and line (see
   [Reference -> Config](../reference/config.md#what-a-load-sets-aside)).
2. **Resolve image.** Uses explicit `--image` first. If that value matches
   `[images.<name>]`, OutRig uses the config block; otherwise it must already
   exist in local Podman images. Without explicit `--image`, agent `image` and
   `default-image` still name config blocks only. With none of the three set,
   the session falls through to
   [the built-in default](#the-built-in-default-image).
3. **Build/cache/probe the image.** Config build images run `buildah build` or
   cache-hit. Config `image-name` images may be pulled. Raw `--image` refs are
   local-only and are checked with `podman image exists`. A build or pull streams
   its output to stderr while it runs.
4. **Start the container.** `podman run -d --rm --name outrig-<sid> -v <repo>:/workspace:rw
   --userns=keep-id ... <image> sleep infinity`. Any `[workspace.mounts]` and `--volume` entries
   become additional `-v` binds. The trailing `sleep infinity` is the container's command and
   overrides the image's `CMD`; an image that sets an `ENTRYPOINT` gets it appended instead, so
   a primary image should not set one (see
   [Concepts -> Containers](../concepts/containers.md#dont-set-an-entrypoint)).
5. **Bootstrap the user.** From the host, inside the container's namespaces: ensure a group with
   `$(id -g)` and a user with `$(id -u)` exist -- appending the entries to `/etc/group` and
   `/etc/passwd` if not -- and that `/home/<user>` exists, is owned by them, and is the home
   their `/etc/passwd` entry names. The image needs no `useradd`/`groupadd` for this. See
   [Concepts -> Workspace](../concepts/workspace.md#uidgid-runtime-user-mapping) for the full
   logic.
6. **Merge MCP config and start sidecars.** Read the primary image's `org.outrig.mcp` label,
   then overlay `[images.<name>.mcp]` from config by server name. Each declared sidecar's image is
   ensured and its label merged, and the `start = "auto"` sidecars are started. A sidecar failure
   follows the block's `on-failure` key; a malformed label, two labels claiming one server
   name, or a label whose `env` references a host variable the global config's
   [`[repo]` `env-allow`](../reference/config.md#repo) does not list, is a startup error. See
   [Config -> sidecars](../reference/config.md#sidecarssc).
7. **Start network interception, if enabled.** `--network audit`, `--network filter`, or
   matching `[network].mode` config installs the per-session interceptor and opens
   `<session_dir>/logs/network.jsonl`. Filter mode also enforces global `[network]` policy.
   The default mode skips this step entirely.
8. **Connect MCP servers.** For each entry in `[images.<name>.mcp]`,
   `podman exec -i --user=$(id -u):$(id -g)` the configured command, run the MCP `initialize`
   handshake, and discover tools via `tools/list`. Each of those two gets 120 seconds; a server
   that misses either fails startup, with its stderr in the error.
9. **Resolve agent -> model -> provider.** From `--agent` (or `default-agent`, or neither --
   an agentless session resolves as though the agent had no keys set), pick the
   model from `--model`, else `[agents.<a>].model`, else top-level `default-model`. `--model`
   must name an existing `[models.<name>]` entry; it is not a raw provider model identifier. Then
   `[models.<m>].provider`, then `[providers.<p>]`. Resolve the per-turn tool-call max from
   `tool-call-max` and the per-result byte max from `tool-result-max`. Then read the API key
   from the env var named in the provider's `api-key`. Build the Rig provider client.
10. **Build the Rig agent.** Dynamic tools from every MCP server's tool list (each prefixed
    `<server>__<tool>`), the agent's `preamble` and sampling params, assembled with
    `AgentBuilder`. An unset `preamble` -- including every agentless session -- sends no
    system prompt at all.
11. **Open the REPL.** Banner on stderr, `> ` prompt on the terminal, ready for input.

If anything before step 11 fails, `outrig run` reports the error on stderr and exits non-zero
without starting the REPL.

## REPL banner

A typical startup looks like:

```
[outrig] loading config
[outrig] config loaded: /home/you/coding/.agents/outrig/config.toml (1ms)
[outrig] workspace: /home/you/coding
[outrig] resolving agent and container
[outrig] agent/container resolved: agent coding, container coding (0ms)
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
[outrig] base-url:          https://api.openai.com/v1
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

When the image is not cached, the build runs between `ensuring image` and `image ready`, and its
output is shown as it happens:

```
[outrig] ensuring image coding:8c2a4f7e91d6b5a3
[buildah] STEP 1/6: FROM docker.io/library/node:24-bookworm-slim
[buildah] STEP 2/6: RUN apt-get update && apt-get install -y git
...
[outrig] image ready: coding:8c2a4f7e91d6b5a3 (built) (4m12s)
```

The lines are buildah's own (`podman pull`'s, as `[podman]`, for an `image-name` config). Without
`--verbose` the command line itself is not shown and nothing is written to `container.log`; with
it, each line still appears once. A sidecar image that needs building shows the same, under its
own `ensuring sidecar image <name>` / `sidecar image ready` pair.

Two config keys are read only by `outrig run-new`: `context-window` on a `[models.<name>]` row,
and `[events] mode = "record"`. `outrig run` parses them and does nothing with them, so when a
config sets either, it says so with one line per key before it pulls or starts anything:

```
[outrig] warning: context-window on [models.fast] is ignored by `run`; only `run-new` reads it
[outrig] warning: [events] mode = "record" is ignored by `run`; only `run-new` reads it
```

When the session's model is an alias for several, the `context-window` line names each model
that sets one. Without either key, startup is as above. See
[Config](../reference/config.md#remote-provider-models) for `context-window` and
[Config](../reference/config.md#events) for the event log.

All startup progress and the banner are on **stderr**. The only thing that ever goes to stdout is
the assistant's natural-language reply, printed when the turn finishes. On an interactive terminal
the `> ` prompt and the echo of what you type go to the terminal itself rather than to either
stream, so neither `outrig run > out.txt` nor `outrig run 2> err.txt` captures them, and the
prompt stays visible in both. The stream separationmakes it easy to capture just the model output:

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

A rate-limited or briefly unreachable provider does not end the session either. outrig retries
the call, honoring the server's own `Retry-After` when it sends one, and prints its progress:

```
[outrig] LLM call failed (HTTP 429 Too Many Requests); retry in 21.0s (Retry-After; 63s/600s spent)
```

If the endpoint is still failing when the budget runs out, the turn ends and you are back at
`>`, with the containers still up. What the conversation keeps depends on how far the turn got.
A turn whose first model call failed has done nothing, and the conversation is unchanged:

```
[outrig] LLM endpoint failed and did not recover (HTTP 429 Too Many Requests); ending turn
[outrig] history unchanged -- send the prompt again to retry, or "/quit" to stop.
```

Resend the prompt itself once the window clears. A later model call comes after tool calls the
turn has already run, and those stay in the conversation with their results, as they do when the
tool-call max fires:

```
[outrig] LLM endpoint failed and did not recover (HTTP 429 Too Many Requests); ending turn
[outrig] partial history retained -- send another prompt (e.g. "continue")
        to keep going, or "/reset" to drop it.
```

Once the window clears, send `continue` rather than the prompt again: the model sees the tool
calls that already ran, where a resent prompt could have it run them a second time. The budget is
`retry-budget-secs`; see [config reference](../reference/config.md) to change it, and
[LLM providers](../concepts/llm-providers.md) for which failures count as transient.

A provider that answers with nothing usable is handled the same way, down to what the
conversation keeps. The call is retried twice before outrig gives up:

```
[outrig] model returned an unusable response (Response contained no message or tool call
        (empty)); retry in 1.4s (attempt 2/3)
```

```
[outrig] the model returned a response outrig could not use (Response contained no message
        or tool call (empty)); ending turn
[outrig] history unchanged -- send the prompt again to retry, or "/quit" to stop.
```

## Line editing and history

On an interactive terminal the `>` prompt is a full line editor. Arrow keys and the usual
readline bindings move and edit within the line -- `Ctrl-A`/`Ctrl-E` for the ends, `Ctrl-W` and
`Ctrl-K` to cut a word or the tail, `Alt-B`/`Alt-F` to move by word. `Up` and `Down` recall the
prompts you typed earlier in this session, and `Ctrl-R` searches them.

That history lives in memory for the length of the session. **outrig writes no history file**:
nothing you type at the prompt is persisted anywhere, and a new `outrig run` starts with an empty
recall list. `/reset` clears the *conversation* history the model sees; it does not touch the
prompts the editor recalls.

Pasting several lines at once sends them as a single prompt, with the line breaks intact, rather
than as one turn per line. Typing multi-line input is still not supported -- `Enter` always ends
the prompt.

The editor needs a terminal it can drive. When stdin is a pipe or a file, or a terminal other
than the one you are typing at (`outrig run < /dev/pts/7`), or `TERM` is `dumb`, `cons25`, or
`emacs`, the prompt falls back to reading one line at a time with no editing and no recall, which
is what keeps scripted use (`echo "..." | outrig run`) working unchanged.

Input is read as UTF-8. A line from a pipe or a file with bytes that are not UTF-8 -- a Latin-1
`é` in a prompts file -- is still sent, with U+FFFD (`�`) in place of each bad sequence. The line
editor does not do this yet: at the first such byte, from an `Alt` key that sets the eighth bit or
a paste out of a Latin-1 source, its read fails and the session ends with an error. Redirect stdin
to send such text (#516).

`RUST_LOG=debug` and `OUTRIG_LOG=debug` do not record what you type. The editor's own debug
output names every keystroke, so it stays at `warn` unless the filter mentions `rustyline`.

> **TODO: Incomplete** -- typed multi-line input is deferred.

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
servers connect, and the new tools become available to the agent **on the next turn**. Subagents
launched from then on get them too; one already running keeps the tools it was launched with
(see [Concepts -> Subagents](../concepts/subagents.md)). Unknown names and already-running
sidecars are errors, and there is no `/sidecar stop` in v0.

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
nothing: the container set, tool list, and conversation stay as they were. An add interrupted
with Ctrl-C changes nothing either, and either one can simply be run again.

## Subagents

The agent can hand scoped work to subagents that share this session's container and tools, using
the built-in `outrig__` tools. They are on by default; `subagents = false` on the
`[agents.<name>]` block leaves them out. Their tool calls appear on stderr labeled by name, and
each one's transcript is written to `<session_dir>/logs/subagent-<name>.log` -- or, for one a
subagent launched, under a `subagent-<parent>/` directory there.

```
> audit the config and the mcp wiring in parallel
[outrig] tool call: outrig__subagent({"name": "audit-config", ...})
[outrig] subagent audit-config started
[outrig] tool call: outrig__subagent({"name": "audit-mcp", ...})
[outrig] subagent audit-mcp started
[outrig]   [audit-config] tool call: fs__read_file(...)
[outrig]   [audit-mcp] tool call: shell__exec(...)
[outrig] tool call: outrig__wait_results({"names": [...], "min_count": 1})
[outrig]   [audit-mcp] tool call: outrig__set_result(...)
[outrig] tool call: outrig__get_result({"name": "audit-mcp"})
```

A subagent round that ends early says why on a line labeled the same way, without the advice
shown above for your own turns: `continue` or `/reset` would act on the primary's conversation,
and what the subagent does next is up to the agent that launched it.

```
[outrig]   [audit-mcp] tool-call iteration max (50) reached; ending turn
```

Nothing a subagent produces reaches stdout, so `outrig run > out.txt` still captures only the
primary agent's reply. See [Concepts -> Subagents](../concepts/subagents.md).

## Interrupting and exiting

- **Ctrl-C** during a turn cancels the in-flight LLM/tool call. The REPL prints
  `[outrig] interrupted` to stderr and returns to a `> ` prompt with conversation history intact,
  so you can redirect the agent. A turn interrupted before it finished keeps the tool calls whose
  results had already gone back to the model, with those results and the prompt that led to
  them, so the next turn knows those calls ran. The rest of the turn is not kept: a tool call
  still running when Ctrl-C lands, any the model asked for alongside it, and the whole of a turn
  interrupted before its first results went back, which leaves the conversation as it stood
  before that prompt. Say what you still need rather than referring back to them. It stops the
  agent *waiting*, not work already handed to the container: a
  `shell__exec` that started a build may run to completion, and any
  [subagents](../concepts/subagents.md) keep working and stay collectable on the next turn, as
  does a result the turn read from one of them but did not keep.

  Nothing is killed. The MCP servers, and the `podman exec` transports outrig talks to them
  over, belong to the session and not to the turn, so they stay up for the next prompt; what
  Ctrl-C abandons is the request in flight over one of them. outrig sends that server
  `notifications/cancelled` for it, but cancellation is advisory: a server that ignores it runs
  the work to completion and simply has no one to answer. Ending the session, with Ctrl-D, is
  what stops those. A tool call that nobody interrupts is bounded too: past its
  [`call-timeout-secs`](../reference/config.md#imagesnamemcp) (10 minutes unless configured), it
  is cancelled the same way and the model is told it timed out.
- **Ctrl-C** during a slash command cancels it the same way, with the same `[outrig] interrupted`.
  A `/sidecar add` waiting on an image or on a server that never answers `initialize` stops
  there, and whatever it had started goes: the sidecar container is removed in the background
  and its tools never appear. The session is left as a failed add leaves it.
- **Ctrl-D** at an empty prompt ends the session: closes MCP server stdios, stops the container,
  finalizes the session record, exits.
- **Ctrl-C** at the prompt discards whatever is typed on that line and draws a fresh `> `. A
  second Ctrl-C, with nothing entered in between, exits.
- **Ctrl-C before the first prompt** -- while the image builds, the container starts, or an MCP
  server initializes -- ends the session the way Ctrl-D does: whatever had started is stopped,
  and the session record is finalized. So does **SIGTERM** or **SIGHUP** at any point, prompt
  and turn included: `kill <pid>`, a closed terminal, a service manager stopping the process, a
  CI job timing out. outrig prints `[outrig] SIGTERM received; ending the session` (or the
  signal's own name) and exits with 128 plus the signal number -- `130` for SIGINT, `143` for
  SIGTERM, `129` for SIGHUP -- which is also the exit code the session record keeps.
- A SIGINT or SIGTERM while that shutdown is still stopping containers stops the waiting:
  outrig finalizes the record and exits, and the remaining containers are force-removed in the
  background. A SIGHUP does not hurry it, since a closed terminal can deliver one twice.

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
error: configuration: refusing /tmp/.agents/outrig/config.toml: it is owned by uid 1001,
       not by you (uid 1000), so another user could have written it
help: run `outrig init` where you are to start a config of your own; outrig reads a
      config you do not own only when --config names it
```

The nearest config above the current directory belongs to someone else, so outrig will not run
on it. If it should not be there, `outrig init` gives the current directory a config of its
own, which the walk finds first. If you trust it -- a checkout shared with your team -- pass it
with `--config`.

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
error: configuration: image "outrig-standard:53e082e721df8ecc" (from --image) is not an
       [images.<name>] and is not in podman's local image store; outrig does not pull a raw
       image ref, so run `podman pull outrig-standard:53e082e721df8ecc` (or build and tag it)
       first
```

An explicit `--image` ref that is not an image-config is local-only. Build, tag,
or pull it with Podman first, then rerun the command. A sidecar's `image` that is
not an image-config is refused the same way, with `(from [sidecars.<name>])` in
place of `(from --image)`.

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

## Running a Python program on the host

> Preview. The script form is `outrig run-new`'s, the command of the sessions whose agents write
> Python; a later release makes that `outrig run`, and the script form comes with it. Until then
> `outrig run` and `outrig run-legacy` are the interactive session alone, and refuse a script.

```sh
$ outrig run-new worker.py ./workspace "Fix the tests"
```

runs `worker.py` on the host as `python3 worker.py ./workspace "Fix the tests"` would, under the
Python OutRig carries rather than any on your system. Starting it starts no container and calls
no model; a [session](#sessions) the program enters does.

- **What it sees.** `sys.argv` is the script as you gave it and every argument after it,
  unchanged -- `--help` and OutRig's own flag names included -- and `__name__` is `"__main__"`,
  `__file__` the script, and the working directory, environment, stdin, stdout and stderr yours.
  OutRig reads none of what it prints.
- **What it imports.** The standard library, OutRig's own `outrig` package, and modules beside
  the script. The interpreter runs isolated: neither the working directory, `PYTHONPATH`, nor
  your user site is on `sys.path`. Like the agent's Python, it is a static build, so a package
  with compiled parts does not load.
- **The configuration.** Loaded and validated once, from the directory you ran `outrig` in --
  with `--config` and the global file as for every command -- before the program starts, and
  never again: not from the script's directory. `outrig.config()` is a read-only view of it: each
  model with its provider, the identifier OutRig sends and its declared window, each alias with
  its candidates in order, each provider's style and base URL, `default-model`, the resolved
  `[workspace] host-path`, and `[events] mode`. No API key and no `${VAR}` value is in it.
- **Flags.** `--config`, `--global-config`, `--session-root` and `-v` go before the script, and
  so does `--grace`, which is the program's alone (see [Signals and the
  grace](#signals-and-the-grace)). The interactive session's flags -- `--agent`, `--model` and
  the rest -- are refused there, since the program chooses its own session's inputs; written after
  the script, they are the program's arguments.
- **The terminal.** The program does not read the terminal: because its process group is not the
  terminal's foreground group, a read from it fails at once with `EIO` (`input()` raises
  `EOFError`). Input piped or redirected into `outrig run` reads as usual, and writing to the
  terminal works.
- **The exit status** is the program's: `0` off the end, `sys.exit`'s value, `1` with Python's
  traceback for an uncaught exception, and 128 plus the signal's number for one a signal ended. A
  session OutRig had to stop for the program that is not proven stopped makes it `3`, whatever the
  program's was: something may still be running (see [What OutRig stops for the
  program](#what-outrig-stops-for-the-program)). A script that cannot be opened as a file, or a
  configuration that does not load, is an `error:` line and status `1`, with nothing started.

### Sessions

A program enters a session with `outrig.Session`, and OutRig starts it then: a container, the
Python interpreter in it, and the bindings the configuration declares, with no agent of its own.
OutRig owns each of those and stops them; the program owns its own control flow.

```python
async with outrig.Session(workspace="./repo") as session:
    ...                         # outrig.current_session() is session here

print(session.shutdown_report)
```

- **Making one starts nothing.** `outrig.Session(workspace=None, mounts=(), grace=None)` checks
  its inputs and returns. `workspace` replaces the configured `[workspace] host-path`; when it is
  `None` the configured one is mounted if one is declared, and otherwise no workspace is.
  `mounts`, each an `outrig.Mount` or a path, are added to `[[workspace.mounts]]`; a host or
  container path mounted twice fails the entry, and so, with a binding configured, does a
  directory mounted anywhere but at its host path, as in a configuration. Relative paths are taken
  from the program's working directory when the session is made.
- **Entering it starts it.** It runs in the image `default-image` names -- or OutRig's built-in
  default, as for `outrig run-new` -- with the configured bindings; one a repository's config
  declares is refused, since no one is there to approve it. An entry that fails raises
  `RuntimeError` saying why, once what the start had made is stopped, and the body does not run.
- **Its events.** `session.events()` is the session's event stream, read with `async for`: each
  `outrig.Event` has the `type`, `subject`, `time` and `data` that [the event
  log](../reference/events.md) records, and an `outrig.Missed` says how many a slow reader lost.
  Reading is never required and never slows the session. With `[events] mode = "record"` the
  stream is also written to `logs/events.jsonl` in the session's directory under the session
  root, `<session-root>/<id>/`. That directory holds no `session.json`, so `outrig ls` does not
  list it.
- **Closing it.** `await session.close_admission()` returns once the session refuses new work: a
  call an agent makes of a binding from then on is refused. The `async with` exit closes
  admission, lets what is running finish for `grace` seconds -- five when it is `None`, an hour at
  most -- and stops the rest. It then sets `session.shutdown_report`, whose `verdict` is `"clean"`,
  `"stopped_with_unknown"` or `"not_proven_stopped"`. For the last, the exit raises
  `outrig.ShutdownException`, its `report` the same report and an exception the body raised its
  `__context__`: something the session started may still be running.

### Agent classes

A program drives agents by declaring them as classes, as an agent's own Python does: a subclass
of `outrig.Agent` whose docstring is the agent's instructions, and whose `async def` methods
marked `@outrig.agent` are the requests it answers. An instance constructed inside a session's
block is one agent of that session, whose parent is the program. It keeps its names and its
conversation from one request to the next, and its replies are the program's own classes.

```python
import asyncio
from dataclasses import dataclass
import outrig

@dataclass
class Summary:
    purpose: str
    languages: list[str]

class ProjectReader(outrig.Agent):
    """You read the project in /workspace, and answer questions about it."""

    @outrig.agent
    async def explain(self, question: str) -> Summary:
        """Answer `question` about the project."""
        ...

async def main():
    async with outrig.Session(workspace="./repo"):
        async with ProjectReader() as agent:
            summary = await agent.explain("What does this project do?")
    print(summary.purpose)

asyncio.run(main())
```

- **Construction starts it in the background.** `ProjectReader(**inputs, model=None,
  session=None)` returns at once. It runs on the event loop that is running, which its requests'
  handles belong to, in `session`, or else the session whose block it runs in; with no session,
  or no running loop, it raises. Each input must be data -- a str, number, bool, `None`, a list
  or str-keyed dict of these, or a dataclass instance -- and is a variable of that name in the
  agent's Python. The agent's first round, in which it reads its instructions, follows:
  `await agent.ready()` waits for it, and `async with` waits for it on entry and releases the
  agent on exit.
- **Its model.** `model=` names a configured model or alias; without it, `default-model`. No
  `[agents.<name>]` table, `default-agent`, image or preamble applies: the class's docstring is
  the agent's instructions. An unknown model, a full `children-max`, or a session that has closed
  raises at `ready()` and at the first request, not in the constructor.
- **A call sends a request.** `agent.explain(...)` checks its arguments against their types --
  `explain(1)` raises `TypeError` and sends nothing -- and returns a handle at once: `await` it
  for the reply, or pass `handle.future` to `asyncio.wait`. Cancelling the code that awaits it
  leaves the request running; `handle.cancel()` cancels the request. `done()`, `result()` and
  `status` read it without waiting. A request made before the agent exists waits for it, and one
  made while `requests-max` requests are open on the agent raises `outrig.AgentLimitReached`.
- **Replies are your classes.** The types an agent class names reach the agent as descriptions --
  fields, types, `doc()` and `alias()` markers, defaults -- never as code. Each reply comes back
  as JSON and is decoded in your program, into your class. One that does not decode goes back to
  the agent with what is wrong, each problem with its path, and the agent corrects it without
  running anything again; after three, the request raises `outrig.CompletionRejected`.
- **Releasing it.** `await agent.release()`, or leaving `async with`, ends the agent, each request
  still open on it raising `outrig.AgentReleased`. Construct agents inside the session's block:
  leaving the session releases each one still alive, ends its open requests with
  `outrig.SessionClosing`, and lists them in `shutdown_report.requests`.

The session's events record such an agent under a subject of its own, `agent/c1` and on, its
parent `owner`, the program (see [the event log](../reference/events.md)). The calls an agent's
own Python makes to launch children and give them work by hand -- `runtime.spawn`, `submit` --
have no counterpart in a program: it constructs agent classes.

### Signals and the grace

The program runs in a process group of its own. Ctrl-C, and `SIGINT` or `SIGTERM` sent to
`outrig`, are forwarded to that group, so the program sees them as it would from a terminal. So is
`SIGHUP` -- a closed terminal, a CI timeout -- which the program sees as the `SIGTERM` it asks
for. None of the three ends `outrig` where it stands: were one to, the sessions the program opened
would be left running, with no one to stop them. If `outrig` itself dies anyway -- `SIGKILL`, say
-- the program and everything it started end with it, and its sessions' containers remain.

The first signal also starts the program's grace: the time it has to end on its own, five seconds
unless `--grace <SECS>` or `[host] grace-secs` says otherwise. Time OutRig spends shutting down a
session the program asked it to shut down does not count against it, so a program that leaves its
sessions when signaled is not cut short by their shutdown. Past the grace, or at a second signal,
OutRig stops the program's process group -- `SIGTERM`, five seconds, `SIGKILL` -- and a further
signal sends `SIGKILL` at once. The grace bounds the program's own response, not the whole
shutdown: OutRig still stops whatever the program left open afterward.

A program that leaves its session when interrupted, and prints the report itself:

```python
import asyncio, signal
import outrig

async def main():
    leave = asyncio.Event()
    asyncio.get_running_loop().add_signal_handler(signal.SIGINT, leave.set)
    async with outrig.Session() as session:
        print(f"session {session.id} started", flush=True)
        await leave.wait()
    print(f"session {session.id}: {session.shutdown_report.verdict}")

asyncio.run(main())
```

Ctrl-C ends it with status `0`, once its session is shut down.

### What OutRig stops for the program

A program can end with a session still open: it returns, raises, is killed, or is stopped after
its grace. OutRig then closes the session's admission, shuts it down with the default five-second
drain, and prints its report on stderr, under a line naming the session. A session the program
was still entering has its start stopped where it stood, and what that start had made removed;
OutRig says so in one line. Either way `outrig` exits only once each of them is stopped. If
`outrig` itself is killed, none of this runs, and the sessions' containers remain.

The program is trusted code running with your authority, not a sandbox: it reads your files,
holds your environment, and runs host commands. Keep its imports and any credentials it reads
outside paths an agent can write.

## See also

- [outrig build](build.md) -- pre-warming the image cache.
- [Sessions](sessions.md) -- what `--session-dir` writes and how to inspect it.
- [Reference -> CLI](../reference/cli.md) -- all flags.
