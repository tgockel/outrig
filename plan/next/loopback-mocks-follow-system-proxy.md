# Loopback test mocks are not isolated from a system proxy

## Context

reqwest enables automatic system-proxy detection by default (`auto_sys_proxy: true`,
`reqwest-0.13.4/src/async_impl/client.rs:309`) and has **no** loopback exemption of its own --
the only exclusion list is whatever `NO_PROXY` supplies. So a client built by
`remote_http_client` (`crates/outrig-cli/src/llm.rs:538`) sends even `http://127.0.0.1:...`
requests to `HTTP_PROXY` / `ALL_PROXY` when one is set and `NO_PROXY` does not cover loopback.

Demonstrated while landing 0109. With `HTTP_PROXY=http://127.0.0.1:9
ALL_PROXY=http://127.0.0.1:9`, `full_in_flight_tool_tree_shuts_down_within_the_grace` -- which
points a provider at a loopback mock -- had **0 of 72** requests reach the mock and failed after
its 30 s setup timeout instead of measuring anything.

0109 fixed that for the crate's own unit tests by gating `.no_proxy()` behind `#[cfg(test)]` in
`remote_http_client`. That gate does **not** cover the `tests/` binaries: they exercise outrig by
spawning the built `outrig` binary, which is compiled without `cfg(test)`, so its clients still
pick up the ambient proxy.

## Exposure

An integration test in `tests/` links the library compiled *without* `cfg(test)`, so 0109's gate
does not reach any of them.

**In the default suite** -- `crates/outrig-cli/tests/anthropic_mock.rs`. Explicitly not gated
behind `e2e` ("nothing here needs podman, a network, or a paid Anthropic account"), it builds an
agent through the real `resolve_agent` -> `build_agent` path against a loopback mock.
Reproduced, not inferred: under `HTTP_PROXY`/`ALL_PROXY` pointed at a closed port, a plain
`cargo test` stops making progress in this binary and the workspace run goes from about a minute
to over ten. This is the one that matters -- it is red, by default, on any machine behind a
proxy.

**Behind `#![cfg(feature = "e2e")]`** -- same mechanism, not separately reproduced because the
suite needs podman and a built image:

- `crates/outrig-cli/tests/run_smoke.rs` -- `run_mock_openai` (:614),
  `run_mock_openai_capturing` (:239), `run_mock_openai_resume` (:413).
- `crates/outrig-cli/tests/e2e_quickstart.rs:407-445` -- its own near-copy of the same mock.
- `crates/outrig-cli/tests/common/mod.rs:255` -- `start_mock_http`, the generic scripted
  loopback server.

The failure mode throughout is a timeout rather than a clear error, which is the expensive part
-- it reads as a flake rather than as a misconfiguration.

## Workaround that already exists

reqwest honors `NO_PROXY` (`reqwest-0.13.4/src/proxy.rs:477`), so `NO_PROXY=127.0.0.1,localhost`
in the developer's environment makes the current suite pass as-is. That is worth writing down in
`CONTRIBUTING.md` regardless of which fix below is taken, since it costs nothing and unblocks
anyone who hits this before the real fix lands.

## Shape

Two candidates, roughly in order of preference:

- Have the e2e harness set `NO_PROXY=127.0.0.1,localhost` for the child `outrig` process. It is
  the standard mechanism, needs no production change, and lives in the one place that spawns the
  binary. Note it must be set on the *child*, not the test process -- mutating the parent's
  environment races other tests in the same binary, which is why 0109 did not take that route
  for the unit tests.
- Give the binary an explicit opt-out (an `OUTRIG_TEST_NO_PROXY`-style variable, or a hidden
  flag) that `remote_http_client` honors. More invasive, but it survives a harness that forgets.

Also worth considering while here: whether `remote_http_client` should exempt loopback from
proxying in production too. A user pointing outrig at a local model server behind a corporate
proxy hits the same swallowing, and `NO_PROXY` is not obvious as the fix.

## See also

- `plan/done/0109-subagent-tree-shutdown-grace.md` -- decision 6 records the unit-test fix.
- `crates/outrig-cli/src/llm.rs:538` -- `remote_http_client` and the `#[cfg(test)]` gate.
- `plan/next/http-client-rebuilt-per-agent-build.md` -- the other finding in the same function.
