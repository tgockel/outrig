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
when `XDG_CONFIG_HOME` is set to an absolute path, then `~/.outrig/config.toml` (the
outrig-specific fallback -- note this is `~/.outrig/`, not `~/.config/outrig/`). As the XDG Base
Directory Specification says, an empty `XDG_CONFIG_HOME` counts as unset and a relative one is
ignored, so the global config is never looked for under the current directory. A home directory
that is not absolute is ignored the same way. With no absolute `XDG_CONFIG_HOME` and no absolute
home directory there is no default location: a command that reads the global config runs without
one, and `outrig init` and `outrig config init`, which would have nowhere to write it, stop and
ask for `--global-config`.

Only the default may be missing: a machine with no global config runs on the repo config alone. A
path the flag names has to exist wherever outrig reads the global config, and one that does not
is an error naming it, not an empty global config. To run without a global config whatever the
default holds, pass `--global-config /dev/null`. `outrig init` and `outrig config init` write the
file the flag names, so for them it need not exist yet.

`--session-root` resolves in this order: this flag, then `session-root` in the global config, then
`<XDG_DATA_HOME>/outrig/sessions/`. The config key is global-only, so `ls`, `logs`, `discard`, and
`clean` read no repo config at all and ignore `--config`: they find the same root from any
directory. Given `--session-root`, or `--session-dir` for `logs`, they read no
global config either, so a `--global-config` path is not checked.

`--verbose` adds buildah/podman command transcripts to stderr and to
`<session_dir>/logs/container.log` for `outrig run` / `outrig mcp`. Repeat it (`-vv`) to also
enable trace-level logs from outrig's own modules for that invocation. It does not change
container, MCP, or agent behavior. Normal startup progress is printed to stderr without
`--verbose`, and so is a cache-miss `buildah build` or `podman pull` during startup, as
`[buildah]` / `[podman]` lines while it runs; `--verbose` adds the command lines and the output
of every other command, and writes it all to `container.log`.

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

Start an interactive agent session. `outrig run-legacy` takes the same flags and starts the same
interactive session, so a script can name the MCP-tool agent explicitly before `run` moves to the
Python one (see [`outrig run-new`](#outrig-run-new)). A script given to `run` is refused with a
usage error that names `run-new`, the command that runs one.

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
  [config.md#mcp-env-value-syntax](config.md#mcp-env-value-syntax); you typed them, so they are
  not held to the global config's [`[repo]` `env-allow`](config.md#repo) list, which governs
  the references in a repo config and in an image's label. Within a scope, last wins on
  duplicate keys. Precedence per key: config-file env < global `--env` < per-server `--env`.
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
with any missing parents; one that already holds anything is refused, as is the second of two
launches started at once with one `<path>`. When omitted, outrig
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
directory or a directory above it, nor one that contains the global config: `run` and `mcp`
refuse to start, naming the directory and what chose it. Declare `[workspace] host-path` in the
global config to mount it on purpose; see
[Concepts -> Workspace](../concepts/workspace.md#whats-mounted-what-isnt).

See [Usage -> outrig run](../usage/run.md) for REPL details.

### `outrig run-new`

Start an interactive session whose agent acts by writing Python. A preview: `outrig run` is
unchanged by it. Given a script, it runs a Python program on the host instead: see
[`outrig run-new <script.py>`](#outrig-run-new-scriptpy).

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
  `run`. Its `preamble` follows OutRig's own orientation in the system prompt, and the
  workspace's `AGENTS.md` follows that unless `repo-instructions` turns it off (see
  [`[agents.<name>]`](config.md#agentsname)).
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
as it starts running; on a terminal, it is in the detail view instead (see **On a terminal**).

**A reply is not the work being done.** A round ends when the model yields, and Python it started
can still be running. So a round closes on a line after the model's reply that names what its code
left running -- `(still running: ci_run)`, by each task's name, as `asyncio.create_task(...,
name="ci_run")` gave it -- and says why a limit stopped it, when one did: `(round ended: ...)`. A
round that yielded with nothing left running closes on no such line. A turn that held only the
model's reasoning, which usually means it was cut off at its output ceiling, is reported as that,
with the reasoning, rather than as silence.

**What the agent's Python can import.** The standard library, and modules in the workspace, which
come after the standard library on `sys.path`. `pip install`, run from the agent's code, adds
pure-Python packages: `pip` there is the interpreter's own, not the image's, and it installs into
an environment of the interpreter's own under the container user's home, where the running
interpreter finds the package at once and no other Python in the image looks; `python3 -m pip`
installs for the image's Python. Fetching one needs the network and CA certificates the image and
`[network]` allow. Compiled code never loads in a static interpreter, so numpy and other packages
with compiled parts do not import however they are installed, and the error the model reads says
so and names the image's own Python, if it has one, as the place to run such code. The interpreter's
`pip` finds no wheel of such a package that fits, so it builds the package's source distribution
instead: that fails for numpy, and succeeds for a package such as MarkupSafe whose source builds
to pure Python, which then imports. When `pip` fails on a package that lists compiled wheels, it
ends its output with a note saying so and naming the image's own Python. Under `--only-binary`,
which leaves out a source distribution pip could otherwise build, the note says that instead and
names no Python, since whether the source builds to pure Python is not known. The note is the `pip`
command's: `sys.executable -m pip` runs the same pip without it, and `python3 -m pip` is the
image's own, unchanged. In its Python,
`runtime.python` summarizes this, `runtime.names()` lists what the agent has bound, and `help(x)`
describes anything in at most 8 KiB.

**What you type is a message, not a prompt.** Each line goes onto the agent's `user` channel,
`runtime.channels["user"]` in its Python, and the model is told how many messages are waiting
there -- never what they say. It reads one by running code that receives it, and answers by
sending on the same channel. A line typed while the agent is still working is queued the same
way, and `[outrig] queued for the agent (N waiting)` says so, or on a terminal the status line
counts it; it does not start a second round.
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

**The agent's Python can launch child agents.** `await runtime.spawn(name, prompt=...)` launches
one: an agent on its parent's model, or the one `model=` names, whose code runs in the same
interpreter in a namespace of its own, with no user channel. A work child takes
submissions, `child.submit(inputs, result=T)`, and answers each with `await
runtime.complete(value)`; a request child, launched with `requests={"fizz": outrig.Request(str,
Thing)}`, answers what is sent on its channels. Each submission and request returns a handle at
once, and awaiting it gives a value of the declared type or raises an `outrig.AgentError` saying
why there is none; `help(runtime.spawn)` has the rest. `children-max`, `model-concurrency-max`,
`requests-max`, and `token-budget` bound them (see [Top level](config.md#top-level)). A child's
code and model calls are in the event log, under the child's own subject, and are not printed
here.

**A `/name` asks the agent to run a skill.** The session's skills are the repository's
`.agents/skills` and then `~/.agents/skills`, each a directory holding a `SKILL.md` and, optionally,
a `skill.py`, and the agent is told each one's name and description. A line `/name text`, the name
right after the slash, naming one of them goes onto the agent's channel as a directive, which its
code sees naming the skill: the line as typed, or, for a skill with no `skill.py`, the skill's
instructions followed by the line. For a skill with a `skill.py`, the agent runs it with
`outrig.skills.invoke(name, text)`: a child agent reads `text` into the parameters of the skill's
entry function, or asks a question instead, which the agent puts to you, and the entry is then
called in the agent's Python, with each binding the skill names. Nothing of the skill runs until the
agent's code calls it. No line typed after a directive reaches the agent before it does. `/help` and
`/quit` are `run-new`'s own whatever the skills are named, and a skill named for one of them is said
as the session starts.

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
(see [Remote-provider models](config.md#remote-provider-models)); without one, the session asks
the model's provider for it as it starts, and if the provider reports none, 128,000 tokens is
assumed and startup warns. Before each call, OutRig estimates its size -- about three bytes to
a token -- against the window, less the reply's `max-tokens` and the system prompt. When what was
chosen does not fit, the view is cut: chosen again in half that room beside the turn the call
answers, where the first and recent rounds go first, from their oldest turn, so a long first round
keeps its end; then promotions, oldest first; then the current round's earlier turns; and last the
turn that opens each first round and the current one, where the agent read what was asked. A turn
too large for what is left is passed over and smaller ones still go. Each later call of the round is
sent what the cut kept and every turn since, until that no longer fits and the view is cut again, so
each request begins as the one before it did, which is all a provider's prompt cache can match. A
turn a cut left out stays out until the next cut; a new round, a promotion, or a demotion chooses
afresh. The turn a call answers -- the model's latest code and its results -- is never left out.
When that alone does not fit, the call is not made: the round ends with `(round ended: turn N of
round R ... is about X tokens, and a call to M has room for about Y ...)`, and keeps the turn. The
next round leaves it out like any other, so the session goes on. For an alias of several models,
each is held to its own window, the first one's included: a turn too large for the first model goes
to the next one whose window holds it, and the round ends this way only when none does, naming each
model and the room it had.

**What a shortened conversation looks like to a provider.** Every call keeps each tool call
beside its result, which every provider requires. What a cut can do is put two of the user's
turns or two of the model's in a row: a turn brought back without the rest of its round opens on
the model's call right after the model's own text, and a round cut short ends on results that the
next round's opening follows. Anthropic's API carries results as user messages and merges such a
pair. OpenAI's carries them as `tool` messages, so a prompt after results is no repeat there, and
it accepts two replies in a row. Both adapters are exercised against a mock, not a live
endpoint. A provider or gateway that requires turns to alternate -- a Bedrock-backed Claude
behind an OpenAI-compatible gateway is known to -- can refuse such a call, and when a refused
call carried one, the error says where, and what to set: `role-alternation = "strict"` on the
provider's row (see [`role-alternation`](config.md#style--openai)). For an alias of several
models, that holds for the first model that refused such a call, and names its provider's row.
With that set, each call is sent a conversation that alternates by construction, at the cost of
the turns that would not. A turn brought back without the rest of its round is left out where it
would follow the model's own reply, so promote whole rounds on such a provider. A round that
ended on results -- the tool-call cap, a turn too large to send, Ctrl-C -- is left out whole once
the next round opens, since each of its turns ends on results. What is left out stays in
`runtime.history`, the line that opens the round counts it, and the call's manifest names it. One
shape is still refused: a call whose own round's opening did not fit, so that the turn it answers
opens on the model's reply with nothing before it that ends on the user's side; only a larger
window mends that.

**A failing provider is retried, then left for the next model.** A call that fails in a way that
may clear -- a rate limit, a 5xx, a dropped connection, a response OutRig cannot use -- is made
again within `retry-budget-secs`, as in `run`, and stderr says so. A reply with nothing in it that
the provider ends with `stop` or `end_turn` is no such failure: it is a model with nothing to add,
and the round ends without an error, keeping any Python it ran. One the provider ends with
`content_filter` or `refusal` is a refusal of what the call carried: past its retries, the call is
sent to that model once more with a note in place of each result of the turn it answers -- the one
thing no earlier call carried -- saying why, and that the results are whole in `runtime.history`.
The conversation says so too, in a line beginning `[outrig]`. If that call is answered, every later
call is sent the note in their place, so one result a provider will not read does not refuse the
rest of the session; if not, nothing is left out, and the call fails as any other. A round's first
call has no results to leave out. When the model is an alias of
several, a call that still fails moves to the next, as in `run`, and each call starts again at
the first. Each model a call goes to is sent a conversation chosen for its own window, which may
leave out more than another's did. When every model has failed, the round ends with each one's
reason and keeps any Python it ran. Unlike `run`, no failure ends the session; `/quit` does.
The error says whether another message could help. If any model failed in
a way that may clear -- a rate limit that outlasted the budget, an unreachable host, a response
that could not be decoded, an error a gateway sent with a success status, a turn too large for a
later model's window, which the next call leaves out -- it says to send another. If none did,
such as a key every model refuses, it says that sending another fails the same way, and to
correct the cause and start a new session. With `[events] mode = "record"`, the event log
records each retry and each move, and names the model that answered each call.

**On a terminal.** When stdin and stdout are both the terminal `run-new` was started from, it holds
that terminal with a screen. The conversation -- each line you send, marked `›`, and each message
the agent sends you -- is printed into the terminal's own scrollback, where it stays after the
session ends, and a status line and an input box sit below it. The status line says what the session
is doing: the model working -- the latest thing it said, once it has said anything, each message
replacing the last -- Python running and for how long, a failed call and when it is retried, the
model a call moved from and why, how many of your messages the agent has yet to read, what the
last round left running, and the tokens spent. The agent's working -- the model's own text in
full, each submission's source and what it printed, retries and failovers, and log lines -- stays
out of the conversation. Ctrl-O opens it in a view of its own, scrolled with the arrow keys, PgUp
and PgDn, Home and End, and closed with Esc; with `-v` it is printed in the conversation too. A
round in which the agent sent you nothing ends on a line saying so. The agent is held to the
terminal's pace as below, and while the detail view is open, its messages wait for it to close. A
terminal that has stopped reading what is written to it holds up what is drawn there and nothing
else: what you type, Ctrl-C included, still reaches the session.

Enter sends what is in the box. Alt-Enter starts a new line in it, as does Enter after a `\` that
ends the box, and a paste is one message however many lines it has. The usual line-editing keys
work, and Up and Down recall what you sent, kept in memory and never written to a file. Ctrl-C
clears the box when something is typed in it, and is otherwise the Ctrl-C described below.
Ctrl-D at an empty box ends input, and Ctrl-Z suspends `run-new` as it would any program. A byte
that is not UTF-8 is dropped as it is typed, and becomes U+FFFD in a paste. SIGTERM and SIGHUP
end the session as they do without the screen, and the terminal is put back first, so what the
session says as it stops is written below the conversation rather than drawn over.

With stdin or stdout piped or redirected, on a terminal other than the one it was started from,
or with `TERM` set to `dumb`, `cons25`, or `emacs`, `run-new` reads and writes plain lines, as
the rest of this section describes; `outrig run-new | cat` asks for that on a terminal.

Without the screen, stdout carries only what the agent sends on the channel -- including a send from
code still running after its round ended, which is printed when it arrives, at the prompt or not.
The agent is held to the terminal's pace: its code waits once it is 21 messages ahead of what has
been printed. A stdout that is slow or has stopped, such as a paused pager, delays what is printed
and nothing else: the round, stderr, and Ctrl-C carry on. When input ends -- the end of piped input,
or Ctrl-D -- the lines typed before it that were refused are reported and what the agent had already
sent is printed, then `run-new` exits; a task still sending is not waited for. `/quit` and a second
Ctrl-C do not wait for a stdout that has stopped, so the few messages queued for it are dropped.
With stdout and stderr on one pipe, as in `outrig run-new 2>&1`, each message and each line is
written whole, one at a time, so a line of stderr's never cuts a message; a stdout that has stopped
then holds stderr up too, and `/quit` and a second Ctrl-C do not wait for either. The model's own
text is commentary and goes to stderr with everything else, so `outrig run-new > out.txt` keeps
exactly the messages the agent meant you to have.

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

Besides `/help`, `/quit`, and a directive naming one of the session's skills, a line beginning
`/` is an unknown command, and a blank line is ignored. A failed round is reported and the
session carries on; the messages it had not read are still waiting, so a line typed next is
announced with them.

**Leaving.** `/quit`, the end of input, a second Ctrl-C at the prompt, and a `SIGTERM` or
`SIGHUP` from anywhere each close the session to new work, then stop it: Python still running,
and calls into bindings, are given five seconds to finish; Python is then interrupted, the
bindings process is stopped with everything it started, and the container is stopped, which ends
whatever an interrupt could not. A round running at
`/quit` is dropped; one still running when input ends is let finish first. `run-new` then prints
what the stop found -- whether everything stopped, how each execution that was running ended,
`ok`, `error`, or `unknown` for one cut off, and how each call into a binding that ended after
the close ended -- and exits with the status it decides:

| Status | What the stop found                                                                 |
|--------|-------------------------------------------------------------------------------------|
| 0      | Everything stopped, and every outcome is known.                                     |
| 2      | Everything stopped, and some Python or binding call was cut off: it may have acted. |
| 3      | Something could not be confirmed stopped, and may still be running.                 |

A `SIGTERM` or `SIGHUP` -- a `kill`, a CI timeout, a closed terminal -- ends the session this
way wherever it lands, startup included: the session is stopped and reported as for `/quit`, its
record is finalized, and the status is the signal's own `143` or `129`, as for `run` and `mcp`,
except that a stop not proven outranks it with `3`. A signal during startup stops what had been
started and exits the same way, including at the question about a binding a repository declared,
which nobody has to answer, but with `3` if removing what it had started is not proven. Stderr
says which signal it was.

Nothing is waited for once a signal has asked for the session to end: not the agent's messages
that were queued for a stdout or stderr that has stopped, and not the end of input's own wait for
them, which a signal ends as a Ctrl-C does. A write that fails as the session closes is still
printed after the report, but the status stays the signal's.

Neither the closing line, `[outrig] closing the session`, nor the report is waited for before the
stop starts, and neither makes the session fail if it cannot be written. If nothing is reading a
pipe that carries stdout and stderr, or stderr alone, the session is still stopped and its record
finished, and `run-new` then waits five seconds for the report before it exits.

If the interpreter exits -- Python that calls `os._exit`, say -- nothing typed could reach the
agent any more, and the session ends, through the same report, with exit status 1, or 3 if the
stop could not be confirmed. A conversation that fails -- a write to a stdout whose reader went
away, say -- or a session record that cannot be written ends the same way, and its error is
printed after the report. A write the conversation queued before `/quit` counts, if it fails
before the stop is done; once the record is finished, nothing changes the status.

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

**Bindings.** Each [`[bindings.<name>]`](config.md#bindingsname) the config declares is an
object a process on the host holds for the agent, one process for all of them: `run-new`
installs their packages, starts that process before the interpreter -- `starting bindings`,
`bindings started` -- and stops it, with everything it started, as the session closes. One the
repository's config declares is shown whole before anything is installed, with a `[y/N]`
question; a yes is remembered for that exact declaration in that repository, and an edit asks
again. With no terminal on stdin -- a script, CI -- `run-new` says which binding it skipped and
how to approve it, and the session runs without it. The agent is told each binding's name and
description, and its Python uses each under that name as its library documents:
`repo.head.commit.hexsha`. Each read or call waits for the host's answer, and holds the agent's
event loop until it comes. Every call it makes is checked on the host and, with
`[events] mode = "record"`, recorded. If the bindings process dies, the session goes on without
its bindings, and a call to one raises an error saying how the process ended.

With `[events] mode = "record"`, the session records what its agent did in
`logs/events.jsonl`: each model call and what it was sent, each submission and how it ended, each
message on the channel, the tokens each request used, and the session's own state, ending with
what its stop found (see [Event log](events.md)). The agent never waits for the file. At exit the
file is finished as the session reports, and a warning says how many events it could not take,
if any.

`run-new` does not yet take `run`'s `--env`, `--network`, `--volume`, `--max-tool-calls`, or
`--max-tool-result-bytes`, nor a flag for `[events]`. The config keys behind the last two,
`tool-call-max` and `tool-result-max`, apply, as do `[network]`, `[workspace]`, and `[events]`.
`-v` writes no `logs/container.log` here; on a terminal it prints the agent's working in the
conversation instead. `-vv` still turns on trace logging.

#### `outrig run-new <script.py>`

Run a trusted Python program on the host (preview): the script as `__main__` under the Python
OutRig carries, with every argument after it as its own. It starts no container and calls no
model until the program enters a session with `outrig.Session`.

```
outrig [--config <path>] [--global-config <path>] [--session-root <path>] [--verbose]
       run-new [--grace <secs>] <script.py> [args...]
```

| Flag             | Default             | Description                                           |
|------------------|---------------------|-------------------------------------------------------|
| `--grace <secs>` | `[host] grace-secs` | How long the program has to end after a first signal. |

- The global flags apply before the script, wherever they are written there. The session flags
  above -- `--agent`, `--model`, `--image` and `--session-dir` -- are refused before it, naming
  the flag, because the program chooses its own session's inputs.
- Everything after the script is the program's `sys.argv`, unchanged: `--help`, `--`, and any
  spelling of an OutRig flag included. A script whose name starts with `-` follows `--`.
- The configuration is loaded and validated once, from the invocation's directory as for every
  command, before the program starts. The program reads it as `outrig.config()`.
- `SIGINT` and `SIGTERM` that `outrig` receives are forwarded to the program's process group, and
  so is a `SIGHUP` -- a closed terminal, a CI timeout -- which reaches the program as the
  `SIGTERM` it asks for. The first starts the program's grace, `--grace` seconds -- else
  `[host] grace-secs`, else 5, at most 3600 -- not counting time spent shutting down a session the
  program asked to shut down. Past it, or at a second signal, the program's group gets `SIGTERM`,
  then `SIGKILL` five seconds later; a further signal sends `SIGKILL` at once. None of the three
  ends `outrig` where it stands, which would leave the sessions the program opened running.
  `--grace` is refused without a script.
- Each session the program leaves open -- it exits, raises, is killed or is stopped -- is shut
  down, its report printed on stderr under a line naming it; one it was still entering has its
  start stopped, with one line saying so. `outrig` exits once each is stopped.
- The exit status is the program's, or 128 plus the number of the signal that ended it, or `3`
  when a session it left open is not proven stopped. A script that cannot be opened as a file, a
  configuration that does not load, or a declared workspace whose path is not UTF-8, which
  `outrig.config()` cannot show, is an `error:` line and status `1`, with nothing started.

A later release makes `outrig run` the command of the sessions whose agents write Python, and the
script form comes with it. See
[Usage -> outrig run](../usage/run.md#running-a-python-program-on-the-host) for what the program
sees.

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
| `XDG_CONFIG_HOME` | Absolute only: global config is checked before `~/.outrig/`.    |

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
via `outrig run --session-dir`) display the symlink target as a hint. A symlink whose directory
is gone has no record to list: `ls` reports it on stderr, and `discard` or `clean` removes it.

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
discards the directory it names. A symlink whose directory is gone is removed by
`discard <sid>`, or by `--session-dir` given the path the directory had. Discard removes the
session's directory whole, a rendered `report.html` and anything else in it included; a symlink
inside it is unlinked, not followed. A session whose `session.json` cannot be read is discarded
the same way after the confirmation, which names why it is unreadable; with no record to name
the container, the check for a running one uses `outrig-<sid>`, and `discard` warns on stderr,
`--yes` or not, when the record did not name its container. Refuses if the session's container
is still running.

### `outrig clean`

Delete old stopped session records in bulk.

```
outrig clean [--older-than <duration>]
             [--yes]
             [--build-containers]
             [--unreadable]
             [--session <id>]
             [--session-root <path>]
```

| Argument / flag           | Default | Description                             |
|---------------------------|---------|-----------------------------------------|
| `--older-than <duration>` | `30d`   | Remove sessions older than cutoff.      |
| `--yes`, `-y`             | off     | Skip the interactive confirmation.      |
| `--build-containers`      | off     | Also remove buildah working containers. |
| `--unreadable`            | off     | Also remove unreadable session records. |
| `--session <id>`          | all     | Restrict both sweeps to one session id. |

Durations are positive integers with `s`, `m`, `h`, or `d` units, for example `12h` or `7d`.
The command previews matching sessions and asks once before deleting unless `--yes` is set.
Running sessions are skipped. Symlinks whose directory is gone, older than the cutoff by the
link's own modification time, are removed too. Sessions created with `--session-dir` remove both
the symlink target and the symlink under the session root, each directory whole, as `discard`
does. A record that cannot be read is aged by the start time in
its directory name and is removed only with `--unreadable`: without it, even `--yes` leaves the
record and names it, since what is in it cannot be shown. An entry whose name is not a session
id is never removed, and an unreadable record keeps its container from being swept as a stray
until the record itself is gone. A record cut off before it names its container is kept too,
since an `outrig mcp --attach` session borrows a container that is not `outrig-<sid>`; `discard`
removes it. Alongside the record walk, `clean` sweeps
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
exits. `outrig run-new` does the same from before it starts anything, taking SIGINT only until
its conversation begins, where Ctrl-C is the conversation's own. A host program's `outrig run-new`
forwards all three to the program rather than exiting on one, and the status is the program's.
`outrig mcp` exits `0` on a signal once its startup has finished; see
[`outrig mcp`](#outrig-mcp). `outrig image build` catches them once it starts its validation
container, and stops that container before it exits.

`outrig clean` also exits `1` when the sweep ran but a container it tried to remove is still
there; the run is reported in full, and the container is named on stderr.

`outrig run-new <script.py>` exits with its program's status, whatever it is, or with 128 plus the
number of the signal that ended the program.

## Environment variables

- `[providers.<name>].api-key` references via `${VAR}`: provider API key.
- `OUTRIG_LOG`: preferred `tracing-subscriber` filter, e.g. `OUTRIG_LOG=debug`. When set it
  shadows `RUST_LOG` completely, including a value that filters out more than `RUST_LOG` would.
- `RUST_LOG`: fallback tracing filter when `OUTRIG_LOG` is unset. At `debug`, adds one line per
  buildah/podman invocation (target `outrig::process`) naming the command, its exit status, and
  how long it took.
- `XDG_DATA_HOME`: default base for `session-root` if not set in the global config.
- `XDG_CONFIG_HOME`: global config is checked here before `~/.outrig/config.toml`, when it is
  an absolute path. An empty or relative value is ignored.

These are read on the host. None of them, nor any other host variable such as `HTTPS_PROXY`,
reaches a container unless an MCP server's `env` or `--env` names it; see
[Containers -> The container's environment](../concepts/containers.md#the-containers-environment).

## See also

- [Usage](../usage/README.md) -- narrative for each subcommand.
- [Reference -> Config](config.md) -- `config.toml` schema.
