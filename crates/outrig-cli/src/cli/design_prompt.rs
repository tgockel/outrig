//! `outrig design prompt` -- print a one-shot AI design prompt or MCP setup
//! snippets for OutRig's self-description server.

use std::fmt::Write as _;
use std::io::Write as _;

use clap::{Args, Subcommand, ValueEnum};

use crate::error::Result;
use crate::mcp_self::docs;

#[derive(Debug, Args)]
pub struct DesignArgs {
    #[command(subcommand)]
    pub cmd: DesignCommand,
}

#[derive(Debug, Subcommand)]
pub enum DesignCommand {
    /// Print a self-contained prompt for AI-assisted image-config design.
    Prompt(PromptArgs),
}

#[derive(Debug, Args)]
pub struct PromptArgs {
    /// Print a copy-pasteable MCP config snippet for the named AI tool.
    #[arg(long = "print-mcp-config", value_name = "TOOL")]
    pub print_mcp_config: Option<McpConfigTool>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum McpConfigTool {
    ClaudeCode,
    ClaudeDesktop,
    Codex,
    Cursor,
}

pub fn execute(args: &DesignArgs) -> Result<i32> {
    let output = match &args.cmd {
        DesignCommand::Prompt(args) => match args.print_mcp_config {
            Some(tool) => mcp_config_snippet(tool).to_string(),
            None => render_prompt(),
        },
    };
    write_stdout(&output)?;
    Ok(0)
}

fn write_stdout(output: &str) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(output.as_bytes())?;
    if !output.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    stdout.flush()?;
    Ok(())
}

pub(crate) fn render_prompt() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# OutRig Container-Config Design Prompt");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "You are designing an image-config for OutRig version {}.",
        env!("CARGO_PKG_VERSION")
    );
    out.push_str(
        "\n\
         OutRig runs LLM agents with MCP servers inside podman containers. \
         Produce a Dockerfile and a matching `.agents/outrig/config.toml` \
         `[images.<name>]` block. Respect these rules:\n\
         \n\
         - Keep the container alive with `CMD [\"sleep\", \"infinity\"]`.\n\
         - Do not add a Dockerfile `USER`; OutRig maps the host UID/GID at runtime.\n\
         - Install every MCP server binary in the image or ensure it is on `PATH`.\n\
         - Prefer `/workspace` as the mounted repo path unless the request says otherwise.\n\
         - Return exact file paths and complete file contents.\n\
         - Check the proposed Dockerfile and TOML against the documentation below.\n\
         \n\
         Read the bundled OutRig documentation and examples before designing.\n",
    );

    out.push_str("\n## Bundled OutRig Docs\n\n");
    for doc in docs::DOCS {
        let _ = writeln!(out, "### doc/{}.md", doc.page);
        let _ = writeln!(out);
        out.push_str(doc.markdown.trim_end());
        out.push_str("\n\n");
    }

    out.push_str("## Worked Examples\n\n");
    out.push_str(RUST_EXAMPLE.trim());
    out.push_str("\n\n");
    out.push_str(NODE_EXAMPLE.trim());
    out.push_str("\n\n");
    out.push_str(MULTI_MCP_EXAMPLE.trim());
    out.push('\n');
    out
}

fn mcp_config_snippet(tool: McpConfigTool) -> &'static str {
    match tool {
        McpConfigTool::ClaudeCode => "claude mcp add outrig-self -- outrig mcp self\n",
        McpConfigTool::ClaudeDesktop => {
            r#"{
  "mcpServers": {
    "outrig-self": {
      "command": "outrig",
      "args": ["mcp", "self"]
    }
  }
}
"#
        }
        McpConfigTool::Codex => {
            r#"[mcp_servers.outrig-self]
command = "outrig"
args = ["mcp", "self"]
"#
        }
        McpConfigTool::Cursor => {
            r#"{
  "mcpServers": {
    "outrig-self": {
      "type": "stdio",
      "command": "outrig",
      "args": ["mcp", "self"]
    }
  }
}
"#
        }
    }
}

const RUST_EXAMPLE: &str = r#"
### Worked example: Rust container

User request: Rust development with filesystem and shell tools.

```Dockerfile
FROM docker.io/library/debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git build-essential nodejs npm passwd \
 && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
 | sh -s -- -y --default-toolchain stable --profile default
ENV PATH=/root/.cargo/bin:$PATH
RUN npm install -g @modelcontextprotocol/server-filesystem
WORKDIR /workspace
CMD ["sleep", "infinity"]
```

```toml
[images.rust-dev]
dockerfile = ".agents/outrig/images/rust-dev/Dockerfile"
context = ".agents/outrig/images/rust-dev"

  [images.rust-dev.mcp]
  fs = { command = ["mcp-server-filesystem", "/workspace"] }
  shell = ["bash", "-lc", "exec shell-mcp-command"]
```
"#;

const NODE_EXAMPLE: &str = r#"
### Worked example: Node container

User request: Node 20 container with repo filesystem access.

```Dockerfile
FROM docker.io/library/node:20-bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends git passwd \
 && rm -rf /var/lib/apt/lists/*
RUN npm install -g @modelcontextprotocol/server-filesystem
WORKDIR /workspace
CMD ["sleep", "infinity"]
```

```toml
[images.node-dev]
dockerfile = ".agents/outrig/images/node-dev/Dockerfile"
context = ".agents/outrig/images/node-dev"

  [images.node-dev.mcp]
  fs = { command = ["mcp-server-filesystem", "/workspace"] }
```
"#;

const MULTI_MCP_EXAMPLE: &str = r#"
### Worked example: Multi-MCP container

User request: Filesystem, git, and a project-specific build MCP.

```Dockerfile
FROM docker.io/library/python:3.12-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends git nodejs npm passwd \
 && rm -rf /var/lib/apt/lists/*
RUN npm install -g @modelcontextprotocol/server-filesystem
RUN pip install --break-system-packages mcp-server-git
RUN pip install --break-system-packages project-build-mcp
WORKDIR /workspace
CMD ["sleep", "infinity"]
```

```toml
[images.tools]
dockerfile = ".agents/outrig/images/tools/Dockerfile"
context = ".agents/outrig/images/tools"

  [images.tools.mcp]
  fs = { command = ["mcp-server-filesystem", "/workspace"] }
  git = { command = ["mcp-server-git", "--repository", "/workspace"] }
  build = { command = ["project-build-mcp"], env = { CARGO_HOME = "/workspace/.cargo" } }
```
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_version_docs_and_examples() {
        let prompt = render_prompt();
        assert!(prompt.contains(env!("CARGO_PKG_VERSION")));
        assert!(prompt.contains("# Containers"));
        assert!(prompt.contains("# Config Reference"));
        assert!(prompt.contains("### Worked example: Rust container"));
        assert!(prompt.contains("### Worked example: Multi-MCP container"));
    }

    #[test]
    fn snippets_are_newline_terminated() {
        for tool in [
            McpConfigTool::ClaudeCode,
            McpConfigTool::ClaudeDesktop,
            McpConfigTool::Codex,
            McpConfigTool::Cursor,
        ] {
            assert!(mcp_config_snippet(tool).ends_with('\n'));
        }
    }
}
