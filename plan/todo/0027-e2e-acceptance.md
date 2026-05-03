# 0027 -- End-to-end acceptance

## Goal

A single integration test demonstrating the headline behavior from `doc/quickstart.md`: from
an empty git repo, `outrig init` -> `outrig build` -> `outrig run` produces a working agent
that lists files via the filesystem MCP. This is the v0 ship gate.

## Deliverables

- `tests/e2e_quickstart.rs` (`#[cfg(feature = "e2e")]`):
  - Create a tempdir + `git init`.
  - Drive `outrig init` against scripted stdin (a sequence of newlines accepting all defaults
    plus a few explicit answers for provider/api-key-env, model, agent name, MCP servers).
  - Verify both `~/.outrig/config.toml` (point at a temp HOME for the test) and
    `<tempdir>/.agents/outrig/config.toml` exist and parse.
  - `outrig build` against the generated container-config.
  - `outrig run` driven by piped stdin: `"list every file under /workspace"`; capture stdout.
  - Assert at least one MCP tool call landed (check session logs or buildah/podman traces).
  - Assert stdout (assistant reply) mentions the test files we placed in the workspace.
- README addition explaining how to run e2e tests:
  - Prerequisites: `podman` + `buildah` (rootless), `mdbook` + `mdbook-mermaid` for docs,
    `OPENAI_API_KEY` (or compatible) in env.
  - Command: `cargo test --features e2e`.
  - Expected runtime: ~30-60s for the quickstart test (image cache after first run).
- Final pass over `doc/`: every remaining `> TODO: Incomplete` either drops (the behavior
  ships in v0) or gets restated as `> TODO: Deferred to post-v0` for things explicitly out of
  scope:
  - Multi-line REPL input.
  - Streaming LLM responses.
  - Network egress interception (CONNECT proxy + allowlist).
  - Optional staging/changeset workspace mode.
  - Per-tool human approval.
  - Concurrent tool dispatch.
  - Tighter cap-drop / seccomp.
  - Non-interactive `outrig init` flags.
  - Auto-restart of crashed MCP servers.
  - Bulk discard in `outrig discard`.

## Acceptance

- `cargo test --features e2e quickstart` passes given the prerequisites.
- All `> TODO: Incomplete` markers in `doc/` are either gone or replaced with
  `> TODO: Deferred to post-v0`. None remain in the original `Incomplete` form.
- `outrig --version` reports `0.1.0`.
- The README's "Quickstart" or "Try it" section works as written for a new user.

## Dependencies

- 0019-agent-loop
- 0021-session-cli
- 0026-init
- 0025-build-subcommand

## Notes

- This test will be the slowest in the suite. It's gated behind `--features e2e` so default
  CI doesn't run it.
- The HOME redirection (so `~/.outrig/config.toml` lands in the tempdir) needs care: spawn the
  child `outrig` processes with `HOME=<tempdir>` in their env. Avoid mutating the test
  process's HOME.
- If a CI runner *does* have podman + an API key (rare), gate the e2e job in
  `.github/workflows/ci.yml` behind a manual-trigger workflow.
