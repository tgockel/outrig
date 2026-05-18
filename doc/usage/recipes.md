# Recipes

> **TODO: Incomplete** -- every command and behavior on this page describes outrig's intended
> behavior; the implementation isn't ready yet.

A handful of common patterns. None of them require anything outside the documented commands and
config -- they're just compositions worth knowing.

## Recipe: scripted single-prompt runs

Pipe a single prompt in, capture only the assistant's reply:

```sh
$ echo "list every Cargo.toml in this repo and summarise what it builds" \
    | outrig run > summary.txt 2> outrig.stderr
$ cat summary.txt
This repo is a Cargo workspace with one crate: outrig (a Rust binary). The
single Cargo.toml at the root declares dependencies on clap, tokio, ...
```

Because outrig keeps tool-call traces, container-startup messages, and slash-command output
strictly on stderr, redirecting stdout gives you only model output.

This is the pattern for embedding outrig in larger shell pipelines (CI checks, batch generation,
summarization jobs).

## Recipe: separate container-configs for different tasks

A `coding` config has the full toolchain. A `planning` config has only research-style MCPs and
runs against a cheaper model. Use distinct agents (each pointing at its own container) and pick
with `--agent`:

```toml
default-container = "coding"
default-agent     = "coding"

[agents.coding]
model     = "fast"
container = "coding"
preamble  = "You are a careful coding assistant."

[agents.planning]
model     = "fast"
container = "planning"
preamble  = "You are a research planner. Read, summarise, do not execute."

[containers.coding]
dockerfile = ".agents/outrig/containers/coding/Dockerfile"
context    = ".agents/outrig/containers/coding"

  [containers.coding.mcp]
  fs    = { command = ["mcp-server-filesystem", "/workspace"] }
  shell = ["bash", "-lc", "exec shell-mcp-command"]

[containers.planning]
dockerfile = ".agents/outrig/containers/planning/Dockerfile"
context    = ".agents/outrig/containers/planning"

  [containers.planning.mcp]
  fs       = { command = ["mcp-server-filesystem", "/workspace"] }
  research = { command = ["mcp-research"] }
```

```sh
$ outrig run                  # default-agent = "coding"
$ outrig run --agent planning # different agent, different image, different MCPs
```

Because each container-config caches independently, switching is fast after the first build of
each.

## Recipe: pre-warming images in CI

Catch a broken Dockerfile or dependency bump before anyone tries to use the agent:

```sh
# .github/workflows/outrig-images.yml (sketch)
- run: cargo install outrig-cli
- run: outrig build --all
```

`outrig build --all` rebuilds every container-config, returns non-zero on the first failure, and
prints the failing buildah step.

## Recipe: capturing what the agent did

Two complementary approaches.

**Tool-call traces via `--verbose`:**

```sh
$ outrig run --verbose 2> trace.log
> add unit tests for parse_config
...
> ^D
$ less trace.log
```

`trace.log` contains every buildah/podman invocation, every MCP tool call with its arguments, and
every assistant turn delineator.

**git as the source of truth for filesystem changes:**

```sh
$ git checkout -b agent/test-coverage
$ outrig run
> add unit tests for parse_config
...
> ^D
$ git status
$ git diff
$ git log --oneline main..HEAD     # what the agent committed (if anything)
```

Combined: `--verbose` for *what the agent decided*, git for *what landed on disk*.

## Recipe: testing a config from outside the repo

If you're iterating on the config and don't want to `cd` every time:

```sh
$ outrig run --config ~/work/some-repo/.agents/outrig/config.toml
```

`--config` overrides the default upward-walk. Pair it with `--session-dir` to write this run's
session into a known directory you can read directly:

```sh
$ outrig run \
    --config ~/work/some-repo/.agents/outrig/config.toml \
    --session-dir /tmp/outrig-experiment

$ cat /tmp/outrig-experiment/session.json   # no id lookup needed
$ tail -f /tmp/outrig-experiment/logs/shell.stderr
```

`--session-dir` writes the session content under `/tmp/outrig-experiment/` and adds a symlink
`<session-root>/<sid> -> /tmp/outrig-experiment/` so the run still appears in `outrig ls`. To
move the *root* (where new auto-generated sessions live) for the duration of one command, use
`--session-root` instead:

```sh
$ outrig run --session-root /var/lib/outrig/sessions
```

## Recipe: sharing a config across repositories

The config file is just TOML -- symlink or copy it. A common pattern is a single template repo
with `.agents/outrig/config.toml` and a Dockerfile, then `git subtree add` (or simply copy) into
per-project repos. You can keep the Dockerfile centralized too if it doesn't need per-repo
customization.

> **TODO: Incomplete** -- outrig has no built-in mechanism for sharing configs; this is plain
> filesystem composition.

## Recipe: limiting the agent's filesystem reach

Mount a subset of the repo by setting `[workspace] host-path` to something narrower than `.`:

```toml
[workspace]
host-path      = "src"
container-path = "/workspace"
```

The agent now sees only `src/`. It can't read `Cargo.toml`, `.git`, `target/`, etc. Useful when
you want a tightly scoped sandbox for a refactor that shouldn't need to touch build
configuration.

## See also

- [outrig run](run.md) -- full REPL reference.
- [Concepts -> Containers](../concepts/containers.md) -- the Dockerfile conventions every recipe
  assumes.
- [Reference -> Config](../reference/config.md) -- every key these recipes use.
