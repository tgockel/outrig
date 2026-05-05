# outrig

![CI](https://github.com/tgockel/outrig/actions/workflows/ci.yml/badge.svg)

`outrig` runs an LLM agent inside a podman-managed container, so the agent can use MCP tools
(filesystem, shell, etc.) without those tools touching anything outside the sandbox you've set up.

You define the sandbox in your repository: a `Dockerfile` for the container, an
`.agents/outrig/config.toml` describing which MCP servers run inside it and which LLM the agent
talks to. Then you run `outrig run` from your shell, and outrig drops you into a stdin/stdout REPL
where you talk to the agent.

The agent layer is the [Rig](https://github.com/0xPlaygrounds/rig) crate -- outrig's job is to give
Rig a containerized, MCP-equipped tool set.

## At a glance

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

## Documentation

The full docs live in [`doc/`](doc/README.md) and render as an mdbook. The latest `trunk`
build is published at <https://tgockel.github.io/outrig/>. To preview locally from the
project root:

```sh
mdbook serve   # live preview at http://localhost:3000
mdbook build   # static HTML in target/book/
```

Start at the [Introduction](doc/README.md) or jump to the [Quickstart](doc/quickstart.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for local checks, the e2e test invocation, and docs
build instructions.

## License

> **TODO: Incomplete** -- license not yet chosen.
