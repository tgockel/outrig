# The subagent shutdown grace was never measured against a full tree

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

## What to measure

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

## See also

- `plan/done/0100-subagent-width-cap.md` -- decision 5 records this as deliberately deferred, and
  fork 2 records why the default is `8`.
- `crates/outrig-cli/src/subagent/mod.rs` -- `SHUTDOWN_GRACE`, `shutdown`, and `shutdown_tree`.
