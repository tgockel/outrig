# AI-assisted design

When the built-in templates do not fit, attach `outrig mcp self` to an MCP-capable AI tool and
ask it to design a container-config. The server exposes OutRig's docs, config schema, curated
presets, and advisory validators over stdio. It cannot write files, run builds, or mutate your
repo; the AI proposes the Dockerfile and TOML, and you install them.

## Run the server

`outrig mcp self` is a host-side self-description server. It does not start a container, create a
session, or require a repo config.

```sh
outrig mcp self
```

The command is meant to be launched by an MCP client. Keep stdout reserved for MCP protocol
messages; status and diagnostics go to stderr.

## Client setup

Use an absolute path to `outrig` if your client starts MCP servers from a different working
directory than your shell.

Claude Code:

```sh
claude mcp add outrig-self -- outrig mcp self
```

Claude Desktop:

```json
{
  "mcpServers": {
    "outrig-self": {
      "command": "outrig",
      "args": ["mcp", "self"]
    }
  }
}
```

Codex CLI:

```toml
[mcp_servers.outrig-self]
command = "outrig"
args = ["mcp", "self"]
```

Cursor:

```json
{
  "mcpServers": {
    "outrig-self": {
      "type": "stdio",
      "command": "outrig",
      "args": ["mcp", "self"]
    }
  }
}
```

## What the AI sees

The server exposes these tools:

| Tool                  | What it returns                                                |
|-----------------------|----------------------------------------------------------------|
| `list_docs`           | Embedded doc pages with titles and summaries.                  |
| `get_doc`             | Markdown for one embedded page.                                |
| `get_config_schema`   | JSON Schema for container config and MCP server entries.       |
| `list_base_images`    | Curated base-image suggestions, explicitly non-exhaustive.     |
| `list_mcp_presets`    | Curated MCP preset suggestions, explicitly non-exhaustive.     |
| `validate_dockerfile` | Advisory warnings about OutRig Dockerfile conventions.         |
| `validate_config`     | TOML parse and config validation results for container blocks. |

The preset tools are suggestions, not a registry. The AI can pick any base image, package set, or
MCP server that fits the job. The Dockerfile validator is advisory for the same reason: it warns
about common OutRig conventions without rejecting custom images.

## Suggested prompt

Ask for the files you want and tell the AI to validate both artifacts before it reports done:

```text
Design an OutRig container-config for a Rust and Postgres development environment.
Use the filesystem MCP server at /workspace and add a custom MCP server that runs pg-dump-mcp.
Read the OutRig docs and schema first, then validate the proposed Dockerfile and TOML.
Return the Dockerfile and the [containers.<name>] block.
```

After review, place the Dockerfile under `.agents/outrig/containers/<name>/Dockerfile` and add the
matching `[containers.<name>]` block to `.agents/outrig/config.toml`.

## Trust model

OutRig expects MCP servers to be useful inside the container, not artificially narrow. A
filesystem server can point at `/workspace` or another broad container path; a shell server can
run commands inside the container. See [MCP Trust Model](../concepts/mcp-trust-model.md) for the
boundary this relies on.

## Without MCP

The no-MCP prompt path is reserved for `outrig design prompt`.
