//! CLI entry point for `outrig mcp self`.

use crate::cli::mcp::McpArgs;
use crate::error::{OutrigError, Result};

pub async fn execute(args: &McpArgs) -> Result<i32> {
    if args.container.is_some() {
        return Err(OutrigError::Configuration(
            "`outrig mcp self` does not select a container; remove --container".to_string(),
        )
        .into());
    }
    if args.session_dir.is_some() {
        return Err(OutrigError::Configuration(
            "`outrig mcp self` does not create a session; remove --session-dir".to_string(),
        )
        .into());
    }
    if args.attach.is_some() {
        return Err(OutrigError::Configuration(
            "`outrig mcp self` does not attach to a container; remove --attach".to_string(),
        )
        .into());
    }
    if args.listen.is_some() {
        return Err(OutrigError::Configuration(
            "`outrig mcp self` serves stdio only; remove --listen".to_string(),
        )
        .into());
    }

    crate::mcp_self::serve_stdio().await
}
