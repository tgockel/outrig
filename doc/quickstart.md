# Quickstart

This walks you from a fresh repo to your first running agent in five minutes, mostly via
`outrig init` (which orchestrates `outrig config init` and `outrig image add` under the
hood).

Crate names matter: depend on `outrig` when embedding the library, and install
`outrig-cli` when you want the command-line tool that provides the `outrig` binary. Both
crates publish from this repository.

## Prerequisites

- Linux on x86-64 or AArch64. WSL2 counts; macOS and native Windows are not supported.
- Rust toolchain (you'll need it to install outrig itself).
- [podman](https://podman.io) **4.3 or newer**, installed and working in rootless mode.
  Verify with `podman info` -- it should print without errors. The floor is 4.3 because
  outrig's cancellation cleanup removes containers with `podman rm --filter`, which older
  podman does not accept.
- [buildah](https://buildah.io) installed alongside podman. They share image storage by default.
- An API key for an OpenAI-compatible chat endpoint, exported as `OPENAI_API_KEY`.

```sh
$ podman info | grep -E "rootless|graphRoot" | head -3
  graphRoot: /home/you/.local/share/containers/storage
  rootless: true
```

If `podman info` errors out, fix that before continuing -- outrig delegates everything to it
and won't paper over a broken setup.

## Install outrig

Install the CLI package to get the `outrig` command:

```sh
$ cargo install outrig-cli
$ outrig --version
outrig 0.1.0
```

## Try it before configuring anything

You need a provider and a model, and nothing else. `outrig config init` writes the global
config -- once per machine, not per repo:

```sh
$ outrig config init
$ cd any-repo-you-like
$ OPENAI_API_KEY=sk-... outrig run
```

With no `.agents/outrig/config.toml` anywhere, outrig uses the current directory as the
workspace and falls back to its
[built-in default image-config](usage/run.md#the-built-in-default-image): a Debian container
with `git` and `curl`, plus `fs` and `shell` MCP servers. That session also
carries outrig's own documentation tools, so you can ask the agent things like "how do I pin a
Rust toolchain in my image?" and it will answer from the docs shipped with your build.

The first run pulls a couple of images, so give it a minute. This is a real session, not a demo
mode -- but the built-in default is a starting point, not a destination. When you want a
container with your project's toolchain in it, configure one:

## Initialize the configs

In a fresh git repo, run `outrig init`:

```sh
$ mkdir -p hello-outrig && cd hello-outrig
$ git init && echo "hello, world" > HELLO.txt && git add . && git commit -m initial
$ outrig init
```

`init` walks you through the global config first (providers and models -- skipped if it
already exists), then the repo config (workspace, default agent), then offers to call
`image add` for the first image-config. Every prompt shows its default in
`[default: ...]`; press Enter to accept. Type `?` and Enter at any prompt for an explanation
and the available options.

If the built-in image prompts do not cover the environment you need, still start with
`outrig init` for the provider, workspace, and agent scaffolding. Then use
[AI-assisted design](usage/ai-assisted-design.md) to have an AI tool generate or refine the
Dockerfile and `[images.<name>]` block. The MCP path gives the AI validators;
`outrig design prompt` is the no-MCP fallback.

```
[outrig] no global config found at ~/.outrig/config.toml -- let's create one.
? Provider style [default: openai]:
? Provider name [default: openai]:
? Base URL [default: https://api.openai.com/v1]:
? API key environment variable [default: OPENAI_API_KEY]:
? Define a model now? [Y/n]:
? Model name [default: fast]:
? Model identifier [default: gpt-4o-mini]:
? Provider for this model [default: openai]:
? Use this model as default-model? [Y/n]:
[outrig] wrote ~/.outrig/config.toml

[outrig] no repo config at .agents/outrig/config.toml -- let's create one.

Configuring models
[outrig] models available in your global config: fast (default: fast)
? Would you like to configure LLM models specific to this repo? [Y/n]: n
? Set a default-model for this repo? [y/N]:

Configuring your first agent
? Agent name [default: coder]:
? Preamble (one line) [default: You are a careful coding assistant.]:

Configuring your first image
? Image-config name [default: hello-outrig-standard]:
? Workspace host-path [default: .]:
? Workspace container-path [default: /workspace]:

[outrig] wrote .agents/outrig/config.toml

? Base image [default: debian:bookworm-slim]:
? Language toolchains [default: ]: node
? MCP servers [default: fs]:
[outrig] wrote .agents/outrig/images/hello-outrig-standard/Dockerfile
[outrig] added [images.hello-outrig-standard] to .agents/outrig/config.toml
```

(In the transcript above we accepted most defaults by pressing Enter; only `node` was typed
explicitly for toolchains.)

Two files were written under `.agents/outrig/`, plus your global config:

```
~/.outrig/config.toml
hello-outrig/
├── HELLO.txt
└── .agents/
    └── outrig/
        ├── config.toml
        └── images/
            └── hello-outrig-standard/
                └── Dockerfile
```

The generated MCP table starts with `fs`. The run transcript below also uses shell execution,
so the example config includes a manual `shell` entry. Replace `shell-mcp-command` with the
shell MCP server you install in the image.

Commit the agent config so it travels with the repo:

```sh
$ git add .agents/ && git commit -m "add outrig config"
```

The global file (with API keys etc.) is **not** in your repo -- it's per-user and references
secrets via env-var substitution.

## What got generated

`~/.outrig/config.toml`:

```toml
default-model = "fast"

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"
```

`.agents/outrig/config.toml`:

```toml
default-image = "hello-outrig-standard"
default-agent     = "coder"

[workspace]
host-path      = "."
container-path = "/workspace"

[agents.coder]
# model omitted -> falls back to default-model = "fast"
preamble = "You are a careful coding assistant."

[images.hello-outrig-standard]
dockerfile = ".agents/outrig/images/hello-outrig-standard/Dockerfile"
context    = ".agents/outrig/images/hello-outrig-standard"

  [images.hello-outrig-standard.mcp]
  fs    = { command = ["mcp-server-filesystem", "/workspace"] }
  shell = ["bash", "-lc", "exec shell-mcp-command"]
```

`.agents/outrig/images/hello-outrig-standard/Dockerfile` (excerpt):

```Dockerfile
FROM docker.io/library/debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git nodejs npm \
 && rm -rf /var/lib/apt/lists/* \
 && npm install -g @modelcontextprotocol/server-filesystem

# Add the shell MCP package you chose, matching shell-mcp-command in config.toml.

WORKDIR /workspace
CMD ["sleep", "infinity"]
```

The Dockerfile is yours to edit -- `image add` produces a known-good starting point, not a
finished spec. Note there's no `USER` directive and no `useradd` -- not even the package that
provides one: outrig writes a user matching your host UID/GID into the container at run time, so
the same image works for any host user without rebuilding.
See [Concepts -> Workspace](concepts/workspace.md#uidgid-runtime-user-mapping).

## Pre-build the image

The first `outrig run` would build the image automatically, but doing it explicitly is faster
to verify and keeps the first run snappy:

```sh
$ outrig build
[outrig] image-config: hello-outrig-standard
[outrig] dockerfile:       .agents/outrig/images/hello-outrig-standard/Dockerfile
[outrig] context:          .agents/outrig/images/hello-outrig-standard
[outrig] image tag:        hello-outrig-standard:8c2a4f7e91d6b5a3
[buildah] STEP 1/N: FROM docker.io/library/debian:bookworm-slim
...
[outrig] image ready: hello-outrig-standard:8c2a4f7e91d6b5a3
```

A second `outrig build` is a cache hit:

```sh
$ outrig build
[outrig] image ready (cache hit: hello-outrig-standard:8c2a4f7e91d6b5a3)
```

## Run an agent

```sh
$ OPENAI_API_KEY=sk-... outrig run
```

Diagnostics arrive on stderr:

```
[outrig] agent:             coder (model: fast / provider: openai / gpt-4o-mini)
[outrig] tool-call max:     50
[outrig] tool-result max:   262144 bytes
[outrig] image-config:  hello-outrig-standard
[outrig] image:             hello-outrig-standard:8c2a4f7e91d6b5a3 (cache hit)
[outrig] container started: outrig-20260502T103412-3f2a
[outrig] mcp fs:    initialized (3 tools)
[outrig] mcp shell: initialized (1 tool)
[outrig] tools available: fs__read_file, fs__list_directory, fs__write_file, shell__exec
[outrig] session id: 20260502T103412-3f2a   (Ctrl-D to exit, /help for slash commands)
> 
```

Anything you type at `> ` is the user turn. Replies (only the assistant text) come back on
stdout:

```
> what's in this repo and what does it do?
```

```
[outrig] tool call: fs__list_directory({"path": "/workspace"})
[outrig] tool call: fs__read_file({"path": "/workspace/HELLO.txt"})
```

```
This is a tiny git repo with one tracked file: HELLO.txt, which contains the
text "hello, world". There's no source code yet -- just the README-equivalent
and the outrig harness configuration under .agents/outrig/.
> 
```

Now ask it to do something useful:

```
> add a second line to HELLO.txt that says "from the agent",
> commit it on a new branch named agent/hello, and tell me
> what the new file looks like.
```

```
[outrig] tool call: shell__exec({"cmd": "git checkout -b agent/hello"})
[outrig] tool call: fs__write_file({"path": "/workspace/HELLO.txt",
                                    "contents": "hello, world\nfrom the agent\n"})
[outrig] tool call: shell__exec({"cmd": "git add HELLO.txt && git commit -m 'append agent line'"})
[outrig] tool call: fs__read_file({"path": "/workspace/HELLO.txt"})
```

```
Done. I created branch agent/hello, appended "from the agent" to HELLO.txt,
and committed the change. The file now reads:

    hello, world
    from the agent
> 
```

Press `Ctrl-D` to end the session. outrig stops the container, finalizes the session record,
and exits.

```
[outrig] session 20260502T103412-3f2a ended (duration 2m18s, exit 0)
$
```

## Review what happened

The agent wrote directly to your repo (we used a direct bind-mount, not a staging copy -- see
[Concepts -> Workspace](concepts/workspace.md)). Use git to inspect:

```sh
$ git status
On branch agent/hello
nothing to commit, working tree clean

$ git log --oneline -2
b41af3a append agent line
12c0a99 add outrig config

$ outrig ls
ID                       STARTED              DURATION  CONTAINER              EXIT
20260502T103412-3f2a    2026-05-02 10:34:12  2m18s     hello-outrig-standard  0
```

If you don't like what the agent did, the rollback is whatever you'd normally do with git:
`git checkout main`, `git branch -D agent/hello`, etc. outrig itself doesn't try to take that
work back from you.

## Where to go next

- [Concepts -> Providers, Models, and Agents](concepts/llm-providers.md) -- the three-layer
  LLM config and how to add more models or agents.
- [Concepts -> Containers](concepts/containers.md) -- the Dockerfile conventions in detail.
- [Concepts -> MCP Servers](concepts/mcp-servers.md) -- how to add more tools.
- [Usage -> AI-assisted design](usage/ai-assisted-design.md) -- AI-guided image-config
  design after `outrig init`.
- [Usage -> outrig run](usage/run.md) -- REPL behavior, slash commands, multiple
  image-configs.
- [Reference -> Config](reference/config.md) -- every key in `config.toml`.
