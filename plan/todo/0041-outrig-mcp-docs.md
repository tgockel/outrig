# 0041 -- Docs for `outrig mcp`

## Goal

Document the new subcommand so the mdbook reflects the shipped surface. Bulk of
the work is a new `doc/usage/mcp.md`, plus stitching it into `doc/SUMMARY.md`,
`doc/reference/cli.md`, and a forward-link in `doc/concepts/mcp-servers.md`.

## Deliverables

- `doc/usage/mcp.md` -- new page covering:
  - When to reach for `outrig mcp` vs. `outrig run` (server vs. client of an LLM).
  - The full surface: `outrig mcp [--container <name>] [--session-dir <path>]`,
    the global flags that apply, the absent `--agent` flag and the
    `default-container` fallback semantics.
  - The transport (stdio) and the load-bearing invariant: stdout is JSON-RPC,
    stderr is everything else.
  - The banner the user sees on startup (paste the sample from 0040).
  - Tool-name namespacing (`<server>__<tool>`), with a worked example showing how
    `fs.read_file` becomes `fs__read_file`.
  - Lifecycle: stdin EOF, SIGINT, SIGTERM all teardown gracefully.
  - Sessions: an `outrig mcp` session writes a row with `agent_name = null`,
    visible in `outrig ls` and `outrig logs`. Forward-link to
    `doc/usage/sessions.md`.
  - Forward-links to the `plan/next/` follow-ups (HTTP/SSE, attach-to-existing)
    framed as "future work, not in v0."
- `doc/SUMMARY.md` -- new entry under **Usage**, between `outrig run` and
  `outrig build`. Indentation matches surrounding entries; mdbook builds cleanly.
- `doc/reference/cli.md` -- new `mcp` subsection mirroring the existing `run`
  subsection (flag table, exit codes, environment variables).
- `doc/concepts/mcp-servers.md` -- one paragraph at the top noting that the same
  `[containers.<name>.mcp]` table is consumed by both `outrig run` and
  `outrig mcp`, with a forward-link to `doc/usage/mcp.md`.
- `doc/usage/sessions.md` -- mention that `outrig mcp` sessions have no
  `agent_name` and display as `-` (or whatever placeholder 0036 settled on).
- All new and modified pages pass the doc-style audit:
  ```
  python3 scripts/audit-doc-style.py doc/usage/mcp.md doc/reference/cli.md \
      doc/concepts/mcp-servers.md doc/usage/sessions.md doc/SUMMARY.md
  ```
  Width <= 100 cp, American-English spellings, ASCII em-dashes, table
  alignment, etc.

## Acceptance

- `cargo doc --no-deps` clean.
- `mdbook build` clean (and `mdbook-mermaid` clean if any new diagrams).
- The doc-style audit passes for every modified file.
- Reading the new `doc/usage/mcp.md` end-to-end is sufficient for a Claude Code /
  Cursor / Zed user to wire `outrig mcp` into their client config.

## Dependencies

- 0040-outrig-mcp-wire-subcommand

## Notes

- Open sub-decisions explicitly deferred from v0 (logged in this doc as forward-
  looking notes, not committed surface): tool-call audit log surfacing,
  `prompts/*` / `resources/*` proxying, backing-server stderr exposure as MCP
  resources, `tools/list` pagination.
- Drop the page header `> TODO: Incomplete` marker if any was added during the
  earlier outrig-mcp phases; ditto on the `doc/concepts/mcp-servers.md` header.
