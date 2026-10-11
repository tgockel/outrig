# outrig

Run an LLM agent's tools inside a [podman](https://podman.io/)-managed container. This is the
library crate. It does two things:

- **Containers and their tools.** It acquires a container image, starts the container, connects
  the MCP servers running inside it, and hands you a typed surface for listing and calling their
  tools. The entry point is [`Outrig::launch`].
- **An agent that acts by writing Python** ([`harness`]). A session holds a container, a Python
  interpreter in it, and an agent whose one tool runs Python there. Your program starts it,
  drives its rounds, watches its events, and stops it with a report that says whether everything
  it started has stopped. `outrig run-new` is built on it and nothing else of the loop.

The `outrig` command-line tool lives in the companion
[`outrig-cli`](https://crates.io/crates/outrig-cli) crate.

Every session's container gets OutRig's own static CPython, mounted read-only at
`/outrig/python`, so the image needs no Python of its own. The interpreter is part of the build:
`build.rs` downloads a pinned `python-build-standalone` release once per machine, verifies its
SHA-256, and embeds it, and the first launch unpacks it under `$XDG_CACHE_HOME/outrig/python`.
For a build with no network, set `OUTRIG_PYTHON_ARCHIVE` to a copy of the archive; without either,
the build warns and every launch fails with a message saying so. The same build fetches and
embeds the pinned RPyC wheel that carries hosted-object requests between the interpreter and a
binding's process; `OUTRIG_RPYC_WHEEL` names a local copy of it for a build with no network.
OutRig reserves `/outrig` inside the container, so neither the workspace nor a mount may be
placed there.

## Example

```rust,no_run
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
use outrig::config::McpServerSpec;
use outrig::{LaunchSpec, Outrig};
use std::collections::BTreeMap;

// Describe the MCP servers to run inside the container.
let mut mcp = BTreeMap::new();
mcp.insert(
    "fs".into(),
    McpServerSpec::Short(vec!["mcp-server-filesystem".into(), "/workspace".into()]),
);

// Launch a container from an existing image and connect every MCP server.
let spec = LaunchSpec::from_image("my-image:latest", mcp, "/tmp/outrig-logs".into());
let outrig = Outrig::launch(&spec).await?;

// Enumerate the tools every connected server advertises.
for tool in outrig.tools() {
    println!("{}/{}: {}", tool.server, tool.name, tool.description);
}

// Dispatch a single `tools/call`.
let result = outrig
    .call_tool("fs", "list_directory", serde_json::json!({ "path": "/workspace" }))
    .await?;
// `result.content` is the server's blocks in order; `render_text` is the
// single-string view a language model is given.
println!("{}", result.render_text());

// Stop every server, then the container.
outrig.shutdown().await?;
# Ok(())
# }
```

`LaunchSpec` also has `LaunchSpec::build` (build an image from a `Dockerfile`) and
`LaunchSpec::from_config` (drive it from a parsed config), plus builder methods for mounts,
capability profiles, network policy, embedded MCP handling, and the session id its containers
are named and labeled for. By default, OutRig merges MCP servers from an image's
`org.outrig.mcp` label with the launch spec. Library callers that want the launch spec's MCP map
to be authoritative can opt out:

```rust,no_run
# use outrig::{EmbeddedMcpPolicy, LaunchSpec};
# use std::collections::BTreeMap;
let spec = LaunchSpec::from_image("my-image:latest", BTreeMap::new(), "/tmp/outrig-logs".into())
    .with_embedded_mcp_policy(EmbeddedMcpPolicy::Ignore);
```

## Running an agent

```rust,no_run
# async fn example(config: outrig::config::Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
use outrig::harness::{DEFAULT_DRAIN, RoundOutcome, SessionBuilder, Verdict};

// Everything the session needs from outside, given before it starts: here, each `${VAR}` its
// model's key names is read from this program's own store rather than the environment.
let mut builder = SessionBuilder::new(config, Some("coding"), None)
    .secrets(|var: &str| (var == "ANTHROPIC_API_KEY").then(|| "sk-...".to_string()));
let mut events = builder.subscribe();
let mut session = builder.start().await?;

// The user reaches the agent through its channel; a round runs on what waits there.
session.user_channel().send("summarize the README").await?;
if let RoundOutcome::Ended(end) = session.round().await? {
    println!("{}", end.reply);
}

// Stop it: nothing new is admitted, what runs gets a few seconds, then the container stops.
session.close_admission();
let report = session.shutdown(DEFAULT_DRAIN).await;
assert_eq!(report.verdict(), Verdict::Clean);
# let _ = events.try_recv();
# Ok(())
# }
```

## Documentation

Full docs render as an mdbook at <https://tgockel.github.io/outrig/>. Source lives in the
[repository](https://github.com/tgockel/outrig).

## License

Licensed under the Apache License, Version 2.0.
