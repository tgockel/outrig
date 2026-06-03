# 0075 -- `outrig design prompt --standalone`

## Context

`outrig design prompt` prints a self-contained AI design prompt for repo-local
`[images.<name>]` image-configs. Standalone image projects -- whose build output is a reusable,
labeled image -- have no equivalent prompt. This task adds one, reflecting the label-based model
settled in 0072/0073: a standalone project is authored as `image.toml` + `Dockerfile` +
`README.md`, and `outrig image build` stamps the config into OCI labels (no `COPY image.toml`).

## Goal

Add a `--standalone` mode to `outrig design prompt` that asks for a complete standalone image
project and reflects the label-based authoring model.

## Deliverables

- Add a `--standalone` flag to `design prompt`. Precedence: `--print-mcp-config` still wins
  (it emits a setup snippet); otherwise `--standalone` selects the standalone prompt, and the
  default remains the repo-local prompt.
- Keep the existing default `outrig design prompt` output unchanged.
- Factor the shared `docs::DOCS` bundle loop so both the default and standalone prompts reuse
  it without duplication.
- The standalone prompt must:
  - ask for a complete project: `Dockerfile`, `image.toml`, and `README.md`,
  - describe the standalone `image.toml` schema (required `[image].ref` + non-empty `[mcp]`;
    optional `description`/`version`/`tags`; optional `[build]`),
  - state the image conventions (`CMD ["sleep", "infinity"]`, no `USER`, install MCP binaries
    on `PATH`),
  - explain that `outrig image build` validates `image.toml` and stamps the config into OCI
    labels -- so the Dockerfile does **not** copy a config file,
  - include one worked project (Dockerfile + `image.toml` + `README.md`) showing the
    `image-name` consuming block.
- Document standalone AI-assisted design in `doc/usage/ai-assisted-design.md`.

## Acceptance

- `outrig design prompt` keeps its existing repo-local output (asserted unchanged).
- `outrig design prompt --standalone` includes the standalone conventions, the `image.toml`
  schema guidance, the label-stamping note, and a worked project example.
- Unit tests cover both render modes; an integration test asserts the standalone stdout markers.
- `cargo test`, `clippy`, `fmt`, and the doc-style audit pass.

## Dependencies

- **Hard: 0073**. The prompt describes the final no-`COPY`, label-stamped authoring model.
