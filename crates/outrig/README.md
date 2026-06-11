# outrig

Run an LLM agent's tools inside a [podman](https://podman.io/)-managed container. This is the
library crate: it acquires a container image, starts the container, connects the MCP servers
running inside it, and hands you a typed surface for listing and calling their tools. The agent
loop, LLM providers, and the `outrig` command-line tool live in the companion
[`outrig-cli`](https://crates.io/crates/outrig-cli) crate — depend on this crate when you want to
embed container-isolated MCP tools in your own program and stay free of the LLM-side dependency
graph.

The curated entry point is [`Outrig::launch`].

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
println!("{}", result.content_text);

// Stop every server, then the container.
outrig.shutdown().await?;
# Ok(())
# }
```

`LaunchSpec` also has `LaunchSpec::build` (build an image from a `Dockerfile`) and
`LaunchSpec::from_image_config` (drive it from a parsed config), plus builder methods for mounts,
capability profiles, and network policy.

## Documentation

Full docs render as an mdbook at <https://tgockel.github.io/outrig/>. Source lives in the
[repository](https://github.com/tgockel/outrig).

## License

Licensed under the Apache License, Version 2.0.
