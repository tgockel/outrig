# outrig

![CI](https://github.com/tgockel/outrig/actions/workflows/ci.yml/badge.svg)

`outrig` runs an LLM agent on the host and connects it to MCP servers inside a podman-managed
container, so tools like filesystem and shell access stay inside the sandbox you've set up.

Crate names matter: depend on `outrig` when embedding the library, and install
`outrig-cli` when you want the command-line tool that provides the `outrig` binary. Both
crates publish from this repository.

You define the sandbox in your repository: a `Dockerfile` for the container, an
`.agents/outrig/config.toml` describing which MCP servers run inside it and which LLM the agent
talks to. Then you run `outrig run` from your shell, and outrig drops you into a stdin/stdout REPL
where you talk to the agent.

The agent layer is the [Rig](https://github.com/0xPlaygrounds/rig) crate -- outrig's job is to run
Rig in the host process and give it a container-isolated MCP tool set.

## Install

Install the CLI package to get the `outrig` command:

```sh
$ cargo install outrig-cli
```

For library use, depend on the `outrig` crate from your Rust package.

## Platform support

OutRig runs on Linux, x86-64 or AArch64. Install the matching musl target
(`x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`) to enable `view = "primary"`
sidecars: those need the `outrig-enter` helper, which is always a Linux binary because it runs
inside the container rather than in the host process. A build without that target still works
-- the sidecar mode reports why the helper is unavailable, before creating a container.

macOS and native Windows are not supported. WSL2 works: it is an ordinary Linux build.

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

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
