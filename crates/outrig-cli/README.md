# outrig-cli

`outrig-cli` provides the `outrig` command: it runs an LLM agent on your host and connects it to
MCP servers running inside a [podman](https://podman.io/)-managed container, so tools like
filesystem and shell access stay inside the sandbox you define. The agent layer is the
[Rig](https://github.com/0xPlaygrounds/rig) crate; outrig's job is to run Rig in the host process
and give it a container-isolated MCP tool set.

To embed the container/MCP machinery in your own Rust program without the LLM dependency graph,
depend on the [`outrig`](https://crates.io/crates/outrig) library crate instead.

## Install

```sh
$ cargo install outrig-cli
```

This installs the `outrig` binary.

## At a glance

You define the sandbox in your repository: a `Dockerfile` for the container and an
`.agents/outrig/config.toml` describing which MCP servers run inside it and which LLM the agent
talks to. Then you run `outrig run` and talk to the agent over a stdin/stdout REPL:

```sh
$ outrig run
[outrig] agent: coding (model: fast / provider: openai / gpt-4o-mini)
[outrig] container outrig-20260502T103412-3f2a started
[outrig] mcp servers: fs, shell
> what's in this repo?
This repo is a Rust project named "outrig". The top level contains Cargo.toml,
src/, and doc/. src/ has lib.rs and main.rs...
> ^D
[outrig] session 20260502T103412-3f2a ended
```

## Commands

```sh
outrig init               # set up global config and scaffold an image config
outrig config init        # configure global settings interactively
outrig run                # start an interactive agent session
outrig mcp                # serve the configured MCP servers as one MCP over stdio
outrig design             # generate AI-assisted design prompts
outrig build              # build (or cache-hit) the session image
outrig image add          # scaffold a new image config in the repo
outrig image init|build   # create / build a standalone image project
outrig image inspect      # read an image's OCI labels without starting it
outrig ls|logs|discard|clean   # inspect and prune sessions
```

## Local models (optional, deprecated)

> **Deprecated.** The `local-llm` feature is deprecated and will be removed in a future
> release. Run local models under an OpenAI-compatible server -- [Ollama](https://ollama.com),
> vLLM, or `llama.cpp`'s server -- and point a `style = "openai"` provider at its `localhost`
> base-url instead. Builds using the feature keep working for now and emit a build warning.

The `local-llm` feature adds an in-process [mistral.rs](https://github.com/EricLBuehler/mistral.rs)
backend so the agent can run a model without an external API; `cuda` and `metal` features enable
GPU acceleration. It roughly triples the dependency count, which is part of why it is going away.

```sh
$ cargo install outrig-cli --features local-llm   # deprecated
```

## Documentation

Full docs render as an mdbook at <https://tgockel.github.io/outrig/>. Source lives in the
[repository](https://github.com/tgockel/outrig).

## License

Licensed under the Apache License, Version 2.0.
