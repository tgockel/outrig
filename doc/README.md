# Introduction

`outrig` is a command-line tool for running LLM agents against your repository, with everything
the agent runs isolated inside a [podman](https://podman.io)-managed container that you describe
with a `Dockerfile`. You stay in control of the environment the agent acts in: which language
toolchains are available, which tools are wired up, what the agent's view of the filesystem looks
like.

## The problem

Modern coding agents are good at running tool calls -- reading files, editing them, executing shell
commands, hitting external APIs. But "running tool calls" on a developer's laptop means the agent
can do anything the developer can: delete files outside the project, push to remote branches, talk
to anything on the network. Most agent harnesses respond to this by gating each tool call behind a
human-approval prompt. That works, but it defeats the point of letting the agent *work* -- every
prompt is a context switch back to the human.

outrig takes a different trade. Instead of approving each call, you set up a sandbox once: a
Dockerfile, a config file, a network policy. The agent loop stays in the host outrig process, but
what the agent does runs inside that sandbox. The blast radius of a wrong action is bounded by the
container.

## Two ways an agent acts

- **`outrig run`** -- the agent calls the tools of MCP servers that run inside the container:
  filesystem access, shell execution, whatever you install in the image. `outrig run-legacy` is
  the same command.
- **`outrig run-new`**, a preview -- the agent's one tool submits Python to an interpreter
  OutRig runs inside the container, which keeps the agent's variables from one message to the
  next. Its code reads and writes files, runs commands, launches other agents, and uses objects
  that programs on the host hold for it. `outrig run-new <script.py>` runs a Python program of
  yours on the host that owns such sessions and drives their agents. A later release makes this
  system `outrig run`.

## How it fits together

```mermaid
flowchart LR
    you(["you<br/>(terminal)"])
    api(["LLM provider<br/>HTTPS API"])
    repo[("your repo<br/>on disk")]

    subgraph host["host"]
        direction TB
        outrig["outrig CLI"]
        rig["agent loop"]
        bindings["bindings<br/>(run-new)"]
        outrig --> rig
    end

    subgraph container["podman container"]
        direction TB
        mcp["MCP servers<br/>(run)"]
        python["Python interpreter<br/>(run-new)"]
    end

    you -- "prompts (stdin)" --> outrig
    rig -- "replies (stdout)" --> you
    rig <-- "chat + tool calls" --> api
    rig -- "tool calls" --> mcp
    rig -- "Python" --> python
    python -- "binding calls" --> bindings
    outrig -. "buildah build<br/>podman run" .-> container
    repo == "bind-mounted" === container
```

- **outrig** is a Rust CLI. You run it from a shell inside your repo.
- **Rig** ([rig-core](https://crates.io/crates/rig-core)) drives the agent loop: it talks to your
  configured LLM provider, decides when to call tools, feeds tool results back to the model.
- **podman** is the container runtime. We use [buildah](https://buildah.io) to build the image from
  your `Dockerfile` (buildah and podman share image storage, so anything buildah builds, podman can
  run).
- **MCP servers** ([Model Context Protocol](https://modelcontextprotocol.io/)) are `outrig run`'s
  tools -- filesystem access, shell execution, whatever you install in your container. outrig
  speaks JSON-RPC to each one over `podman exec`'d stdio and exposes them to Rig as dynamic tools.
  See [MCP Servers](concepts/mcp-servers.md).
- **The Python interpreter** is OutRig's own static CPython, which `outrig run-new` mounts
  read-only into its container, so the image needs no Python for it. `outrig run-new`'s agent
  writes the code it runs. See [Python Runtime](concepts/python-runtime.md).
- **Bindings** are objects a process on the host holds for a `run-new` agent -- a Git repository
  with your remotes and credentials, say -- which its Python uses under a name. They act on the
  host with your authority, and every call to one is recorded. See
  [Hosted Objects](concepts/hosted-objects.md).

## What outrig is not

- Not a hosted service. It's a CLI you run locally.
- Not an LLM. You bring the model -- any OpenAI-compatible endpoint, plus whatever providers Rig
  adds.
- Not opinionated about *what* the agent can do. You write the Dockerfile, you pick the MCP
  servers and the bindings. outrig is just the harness.
- Not a replacement for human-in-the-loop review of *outcomes*. The agent has full filesystem
  access to the workspace you mount; you review what it did via `git diff` after the session ends,
  not via approval prompts during it.

## What's next

- [Quickstart](quickstart.md) -- write your first Dockerfile + config and run an agent in five
  minutes.
- [Concepts -> Workspace](concepts/workspace.md) -- what the agent can and can't reach on your
  machine.
- [Concepts -> Providers, Models, and Agents](concepts/llm-providers.md) -- how to point outrig
  at a model.
- [Concepts -> Trust Model](concepts/mcp-trust-model.md) -- what the container boundary holds,
  and what crosses it.
