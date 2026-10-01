//! CLI entry point for `outrig mcp self`.

use crate::cli::mcp::McpArgs;
use crate::error::{OutrigError, Result};

pub async fn execute(args: &McpArgs) -> Result<i32> {
    // No `..`, so a field added to `McpArgs` won't compile here until
    // `mcp self` refuses or honors it.
    let McpArgs {
        cmd: _,
        image,
        session_dir,
        listen,
        attach,
        env,
        network,
        volume,
    } = args;
    let refusals = [
        (image.is_some(), "does not select an image; remove --image"),
        (session_dir.is_some(), "does not create a session; remove --session-dir"),
        (attach.is_some(), "does not attach to a container; remove --attach"),
        (listen.is_some(), "serves stdio only; remove --listen"),
        (!env.is_empty(), "starts no MCP servers to configure; remove --env"),
        (network.is_some(), "starts no container; remove --network"),
        (!volume.is_empty(), "starts no container to mount into; remove --volume"),
    ];
    if let Some((_, refusal)) = refusals.into_iter().find(|&(given, _)| given) {
        return Err(OutrigError::Configuration(format!("`outrig mcp self` {refusal}")).into());
    }

    crate::mcp_self::serve_stdio().await
}
