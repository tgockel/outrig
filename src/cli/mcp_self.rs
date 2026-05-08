//! CLI entry point for `outrig mcp self`.

use crate::cli::mcp::McpArgs;
use crate::error::{OutrigError, Result};

pub async fn execute(args: &McpArgs) -> Result<i32> {
    if args.container.is_some() {
        return Err(OutrigError::Configuration(
            "`outrig mcp self` does not select a container; remove --container".to_string(),
        ));
    }
    if args.session_dir.is_some() {
        return Err(OutrigError::Configuration(
            "`outrig mcp self` does not create a session; remove --session-dir".to_string(),
        ));
    }

    crate::mcp_self::serve_stdio().await
}
