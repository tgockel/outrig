# AI-assisted design

When the built-in templates do not fit, attach `outrig mcp self` to an MCP-capable AI tool and
ask it to design an image-config. The server exposes OutRig's docs, config schema, curated
suggestions, and advisory validators over stdio. It cannot write files, run builds, or mutate your
repo; the AI can use those read-only tools before writing through its own client, or it can return
the exact file contents for you to install.

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

Regenerate any of these snippets with:

```sh
outrig design prompt --print-mcp-config <tool>
```

The valid `<tool>` names are `claude-code`, `claude-desktop`, `codex`, and `cursor`.

## What the AI sees

The server exposes these tools:

| Tool                          | What it returns                                            |
|-------------------------------|------------------------------------------------------------|
| `list_docs`                   | Embedded doc pages with titles and summaries.              |
| `get_doc`                     | Markdown for one embedded page.                            |
| `get_config_schema`           | JSON Schema plus path and image-label hints.               |
| `list_base_images`            | Base-image suggestions, explicitly non-exhaustive.         |
| `list_mcp_server_suggestions` | MCP server suggestions and shell guidance.                 |
| `validate_dockerfile`         | Advisory warnings about OutRig Dockerfile conventions.     |
| `validate_config`             | TOML parse and config validation results for image blocks. |
| `validate_image_toml`         | TOML validation for standalone image projects.             |

The suggestion tools are not a registry. The AI can pick any base image, package set, or MCP
server command that fits the job. In particular, OutRig supports shell MCP servers even when the
suggestion list does not name a single maintained package recipe. The Dockerfile validator is
advisory for the same reason: it warns about common OutRig conventions without rejecting custom
images.

## Suggested prompt

Ask for the files you want and tell the AI to validate both artifacts before it reports done:

```text
Design an OutRig image-config for a Rust and Postgres development environment.
Use the filesystem MCP server at /workspace and add a custom MCP server that runs pg-dump-mcp.
Read the OutRig docs and schema first, then validate the proposed Dockerfile and TOML.
If your client can edit this repo, write the Dockerfile and [images.<name>] block directly.
Otherwise return the exact file paths and file contents.
```

If an AI client is not allowed to write under `.agents/outrig`, have it return the exact file
contents rather than staging files elsewhere and asking for a blind copy into the repo.

## Standalone image projects

For a reusable image that other repos consume by tag, ask for a standalone project instead of a
repo-local `[images.<name>]` block. The output should be a directory with `Dockerfile`,
`image.toml`, and `README.md`.

```text
Design an OutRig standalone image project for a Rust and git development toolset.
Return complete contents for Dockerfile, image.toml, and README.md.
Use the filesystem MCP server at /workspace and the git MCP server for /workspace.
Read the OutRig docs and schema first, then validate the proposed Dockerfile and image.toml.
Explain how a repo should reference the built image with image-name.
```

The standalone `image.toml` is the authoring source. It must contain `[image].ref` and a
non-empty `[mcp]` table; optional `[image]` metadata (`description`, `version`, `tags`) and an
optional complete `[build]` section can be added when useful. `outrig image build` validates
that file and stamps its config into OCI labels, so the Dockerfile should install the tools and
MCP binaries but should not copy `image.toml` into the image.

## Trust model

OutRig expects MCP servers to be useful inside the container, not artificially narrow. A
filesystem server can point at `/workspace` or another broad container path; a shell server can
run commands inside the container. See [MCP Trust Model](../concepts/mcp-trust-model.md) for the
boundary this relies on.

## Without MCP

When your AI tool cannot attach MCP servers, print the same design context as a single prompt:

```sh
outrig design prompt | pbcopy

# Linux:
outrig design prompt | xclip -selection clipboard
```

Paste that prompt into ChatGPT, Claude.ai, or any chat UI, followed by what you want the
image-config to do. The output is intentionally self-contained: OutRig's version, embedded
docs, conventions, and worked examples are all in the prompt.

To inspect or edit the prompt before sending it:

```sh
outrig design prompt > prompt.txt
outrig design prompt --standalone > standalone-prompt.txt
```

Prefer the MCP path when your tool supports it. `outrig mcp self` lets the AI read docs and run
validators as it iterates; `outrig design prompt` is a one-shot fallback for clients that only
accept pasted text.
