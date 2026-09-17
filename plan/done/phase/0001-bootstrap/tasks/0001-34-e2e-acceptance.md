# 0034 -- End-to-end acceptance

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
- README / `CONTRIBUTING.md` addition explaining how to run e2e tests:
  - Prerequisites: `podman` + `buildah` (rootless). Real-API variant additionally needs
    `OPENAI_API_KEY` and `OUTRIG_E2E_REAL_API=1`.
  - Command: `cargo test --features e2e quickstart_mocked` (always-on) or with the
    real-API gate to exercise the live OpenAI endpoint.
  - Expected runtime: ~30-60s for the quickstart test (image cache after first run).
- Drop the `> TODO: Incomplete` "implementation in progress" disclaimer at the top of the
  README -- v0 commands are real now.

## Acceptance

- `cargo test --features e2e quickstart_mocked` passes given the prerequisites
  (podman + buildah).
- `outrig --version` reports `0.1.0`.
- The README's "At a glance" section works as written for a new user (visual review).

## Dependencies

- 0019-agent-loop
- 0021-session-cli
- 0032-build-subcommand
- 0033-init

## Notes

- This test will be the slowest in the suite. It's gated behind `--features e2e` so default
  CI doesn't run it.
- The HOME redirection (so `~/.outrig/config.toml` lands in the tempdir) needs care: spawn the
  child `outrig` processes with `HOME=<tempdir>` in their env. Avoid mutating the test
  process's HOME.
- If a CI runner *does* have podman + an API key (rare), gate the e2e job in
  `.github/workflows/ci.yml` behind a manual-trigger workflow.

## Decisions

- **Two test variants, not one.** The original spec implied a single test using a real
  OpenAI key. We split that into `quickstart_mocked` (always runs under `--features e2e`,
  uses a hand-rolled mock OpenAI server like `tests/run_smoke.rs` -- deterministic, free,
  no network) and `quickstart_real_api` (opt-in via `OUTRIG_E2E_REAL_API=1` plus a real
  `OPENAI_API_KEY`, retries the run leg up to 2x against LLM non-determinism). The mock
  variant patches the init-generated `[providers.openai].base-url` to point at the local
  test server.
- **`doc/` `Incomplete` -> `Deferred to post-v0` pass deferred.** The original spec asked
  for a full sweep of `> TODO: Incomplete` markers in `doc/`, dropping the ones that ship
  in v0 and converting the rest to `> TODO: Deferred to post-v0`. The user pushed back:
  we're still in v0, so items that are pending should remain `Incomplete` rather than
  being relabeled. The README's top-of-file "implementation in progress" disclaimer is
  still dropped (v0 commands are real). The license `Incomplete` at `README.md:50` stays
  put -- it's tracked separately in `0046-pick-a-license.md`. The doc-content sweep itself
  becomes a separate follow-up if/when v0 actually ships.
