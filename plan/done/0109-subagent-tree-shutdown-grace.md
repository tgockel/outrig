# 0109 -- The subagent shutdown grace was never measured against a full tree

## Symptom

None observed. This is an unverified bound, not a live defect.

`SubagentRegistry::shutdown` (`crates/outrig-cli/src/subagent/mod.rs`) gives the *entire* subagent
tree one `SHUTDOWN_GRACE` of five seconds to abort and join, bottom-up. Exceeding it logs a warning
and proceeds with tasks still holding clones of the session's MCP tools -- which is the shape of the
Ctrl-C hang that `shutdown_releases_the_tool_clones_subagents_hold` exists to pin.

That budget was chosen when nothing bounded how many subagents could exist. 0100 added
`subagent-width-max` and settled its default at `8`, which with the default `subagent-depth-max = 3`
admits a tree of 8 + 64 = **72** live subagents: the session registry launches 8 at depth 2, each of
those is below the depth limit so it gets its own registry and its own 8, and the depth-3 layer is
leaves. 0100's own Risks section asked for that worst case to be timed before committing to the
default. It was not; the number is arithmetic, not a measurement.

## Goal

Find out whether the five-second teardown budget fits the tree `subagent-width-max` permits, and
settle `DEFAULT_SUBAGENT_WIDTH_MAX` before 0.2.0 publishes it as working behavior. The grace is a
private constant and stays changeable; the default width is a `pub const` and lowering it after
the release breaks setups that were running fine.

## Deliverables

Stand up a tree at the permitted worst case and time `shutdown()` end to end:

- 72 live subagents (width 8, depth 3, all launched and none released), each holding tool clones.
- Both shapes that matter: every subagent idle after publishing, and every subagent mid-round with
  a tool call in flight -- the second is what the grace budget actually exists to bound.
- Report the join time, not just pass/fail, so there is a margin to reason about rather than a
  boolean.

`shutdown_reaps_nested_subagents_and_releases_their_tool_clones` is the existing nested-teardown
test to grow from; it already builds a grandchild and asserts the tool clones are released.

## If it does not fit

0100 settled the answer in advance, and it should be honored: **lower the default width, do not
raise the grace.** The grace exists to bound a wedged subagent, not to absorb a large healthy one --
stretching it to fit 72 tasks would make a genuinely hung subagent take proportionally longer to
give up on, which is the case the timeout was written for.

## Acceptance

- The worst-case tree (width 8, depth 3, 72 live subagents) tears down inside `SHUTDOWN_GRACE` in
  both shapes -- all idle, and all mid-round with a tool call in flight -- or the default width is
  lowered until it does.
- The measured join time is *reported*, not just asserted against, so the margin is a number
  someone can reason about later rather than a boolean that passed once.
- Whatever the outcome, `shutdown_releases_the_tool_clones_subagents_hold` and
  `shutdown_reaps_nested_subagents_and_releases_their_tool_clones` still pass -- no tool clone
  outlives the shutdown.
- If the default width moves, `DEFAULT_SUBAGENT_WIDTH_MAX`, the config docs, and the range check
  move together, and the CHANGELOG records the new default.

## Dependencies

- **Landed: `plan/done/0100-subagent-width-cap.md`.** It set the width cap and its decision 5
  deferred exactly this measurement; fork 2 there records why the default is `8`.

## See also

- `plan/done/0100-subagent-width-cap.md` -- decision 5 records this as deliberately deferred, and
  fork 2 records why the default is `8`.
- `crates/outrig-cli/src/subagent/mod.rs` -- `SHUTDOWN_GRACE`, `shutdown`, and `shutdown_tree`.

## Decisions

1. **Keep the default width at 8.** The full 72-subagent tree joins with roughly three orders of
   magnitude of margin against the five-second grace. Across ten runs on this runner the idle
   tree measured 3.4-7.8 ms and the in-flight-tool tree 4.7-7.5 ms; the spread is scheduling
   noise, not tree size. Raising the grace or lowering the public width default is therefore
   unnecessary. The margin is large enough that it is not worth re-measuring on every runner --
   what would change the answer is a change to teardown's shape, not to the hardware.

2. **Measure real agent tasks in both states.** The tests launch through `SubagentRegistry`, build
   the eight child registries, and launch eight leaves through each one. The idle case waits for
   every failed round to publish and reach `RunState::Idle`; the in-flight case uses a loopback
   OpenAI-compatible endpoint and a pending session tool, then waits until all 72 calls have
   entered the tool before starting the clock.

3. **Keep the measurement visible and executable.** Both tests print the elapsed join time and
   grace under `--nocapture`, and assert that no session-tool clone survives after shutdown. The
   existing direct and nested clone-release tests remain unchanged.

4. **Assert against `shutdown_tree`, not `shutdown`.** Timing `shutdown` and then asserting the
   elapsed time is under the grace is nearly self-fulfilling: `shutdown` wraps its own
   `shutdown_tree` call in `timeout(SHUTDOWN_GRACE, ..)` and swallows the expiry into a
   `tracing::warn!`, so it returns at roughly the grace however wedged the tree is, and a clock
   read afterwards can only fail by timer overshoot. The tests instead wrap the unclamped
   `shutdown_tree` in the same budget and assert on the timeout's own verdict. The elapsed time is
   still measured and printed, but it reports the margin rather than standing in for the bound.

5. **Wait on the state channel instead of polling.** The idle case originally drove the tree to
   rest with a `get_result` per subagent followed by an unbounded `yield_now` spin over
   `snapshot().state`. `SubagentShared::subscribe` already hands out the `watch::Receiver` that
   `end_round` sends through, and `watch::Receiver::wait_for` checks the current value before
   parking -- which is what makes the `publish`-then-`end_round` ordering the spin's comment
   worried about a non-issue. A fresh subagent starts `Running`, so no receiver can match before
   its round has run. This also drops the per-poll `Snapshot` clone, which copied the outcome
   `String` to read a `Copy` field.

6. **Isolate the fixtures from the ambient proxy.** The in-flight test points a provider at a
   loopback mock, and reqwest turns on system-proxy detection by default with no loopback
   exemption of its own -- the only exclusion list is `NO_PROXY`. With `HTTP_PROXY` and
   `ALL_PROXY` set, 0 of 72 requests reached the mock and the test failed on its 30 s setup
   timeout rather than measuring anything, so this was a red suite for anyone behind a corporate
   proxy. `remote_http_client` now calls `.no_proxy()` under `#[cfg(test)]`. Gating it that way
   keeps automatic proxy detection in real runs, which is how a user reaches a hosted provider
   from behind a proxy, and it avoids the alternative of mutating `HTTP_PROXY` process-wide --
   the crate's tests share a process, so that would race every other test in it.

   The gate reaches the crate's own unit tests only. An integration test in `tests/` links the
   library built *without* `cfg(test)`, so those keep the ambient proxy -- including
   `tests/anthropic_mock.rs`, which is deliberately *not* `e2e`-gated and therefore hangs a plain
   `cargo test` behind a proxy. That is pre-existing rather than introduced here, and fixing it
   needs a mechanism that survives into the binary, so it is filed as
   `plan/next/loopback-mocks-follow-system-proxy.md` rather than solved in a test-only task.
   `NO_PROXY=127.0.0.1,localhost` already works as a stopgap, since reqwest honors it.

7. **Found, but deliberately not fixed here: the HTTP client is rebuilt per launch.** `strace`
   over the idle test shows 17,784 `openat` calls into `/etc/ssl/certs` -- the whole rustls trust
   store re-parsed once per `build_agent`, about 40 ms per subagent launch. That is why these two
   tests cost ~3 s each to measure a ~5 ms quantity. It is production behavior on the launch path,
   not teardown, and it happens entirely before the clock starts, so the measurement is unaffected.
   Filed as `plan/next/http-client-rebuilt-per-agent-build.md` rather than fixed in a test-only
   task; it needs sequencing against 0113, which also touches `RetryPolicy`.
