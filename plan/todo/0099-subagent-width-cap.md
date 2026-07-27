# 0099 -- Capping how many subagents run at once

## Context

`subagent-depth-max` bounds how deeply subagents nest. Nothing bounds how many run side by side.
`SubagentRegistry::launch` inserts into a `BTreeMap` keyed by name and spawns a task; the only
rejections are a malformed name and a name already live. An agent that emits twenty
`outrig__subagent` calls gets twenty concurrent agent loops, each holding clones of the session's
MCP tools and each calling tools into the same container.

Depth was the runaway the depth limit was designed against, and it is genuinely the more dangerous
one -- it compounds. Breadth does not compound, which is why it was reasonable to leave open, but
it is unbounded and it multiplies against everything else: provider rate limits, container load,
interleaved stderr, and spend.

The docs already lean on breadth being cheap. `doc/concepts/subagents.md` presents fan-out as the
headline pattern -- "Launch several to work in parallel" is in the tool description itself, and the
worked example launches three at once. That guidance is good and should stay; this entry is about
there being a ceiling somewhere under "however many the model felt like emitting", not about
discouraging fan-out.

Two things make the current shape worse than the count alone suggests:

- Nothing reaps idle subagents. A finished subagent stays live, with its history, until released or
  until the session ends -- by design, so it stays addressable. So the live count is the number
  ever launched minus the number explicitly released, and models are not reliable about releasing.
- `shutdown` gives the *entire tree* one five-second grace budget (`SHUTDOWN_GRACE`). That budget
  was chosen against handfuls of subagents; a wide tree makes exceeding it likelier, and exceeding
  it means teardown proceeds with tasks still holding tool clones, which is the shape of the
  original Ctrl-C hang.

## Goal

Bound the number of concurrently live subagents per session, with a limit the operator sets and a
refusal the launching agent can act on.

## Deliverables

- A `subagent-width-max` config key, top-level with an `[agents.<name>]` override, mirroring how
  `subagent-depth-max` is declared, merged, validated, and defaulted. `subagent-depth-max` is
  bounded to `1..=16`; pick and state the analogous width range rather than leaving "out of range"
  to the implementer.
- Enforcement in `SubagentRegistry::launch` -- in **both** places the entry lock is taken, not
  just the first. `launch` pre-checks for a duplicate name under the lock, drops it, awaits
  `build_subagent_agent`, then re-takes the lock and re-checks before inserting, precisely because
  an await intervened. A width check placed only at the pre-check has the identical hole. See the
  note under Risks about what that costs.
- A refusal message that tells the launching agent what to do next -- collect and release
  something -- rather than only reporting a number.
- Documentation in `doc/reference/config.md` beside `subagent-depth-max`, and a note in
  `doc/concepts/subagents.md` where fan-out is described. Both of those paths are **symlinks**
  into `crates/outrig-cli/src/mcp_self/docs/`; edit the files there. There are no second copies to
  keep in step.

## Runtime Behavior

The limit counts **live entries in one registry** -- subagents that have been launched and not
released. It is per-registry, not per-tree: each launching agent gets its own budget, the same way
each gets its own private view and its own namespace. A parent at the limit cannot launch more
until it releases some; its subagents' own budgets are unaffected.

Per-registry rather than session-wide is the cheaper and more predictable rule. A session-wide
counter shared across a tree makes one agent's fan-out fail because of a sibling's, which is not
something the launching agent can diagnose from where it stands, and it needs a shared counter
threaded through every registry. Per-registry composes with depth the way the existing design
already composes: worst case is bounded by width raised to the depth, which the operator can reason
about from two numbers.

Because idle subagents stay live by design, the limit is really a bound on *outstanding handles*,
not on running tasks. That is the correct thing to bound -- an idle subagent still holds its
history, its tool clones, and its slot in the shutdown walk -- but it means a parent that launches
up to the limit and collects everything still has to release before launching more. The refusal
message therefore names `outrig__subagent_release` explicitly, since "you are at the limit" without
the remedy is a message a model will retry rather than act on.

In the ordinary case refusal happens before the task is spawned and before the entry is inserted,
so a refused launch costs a tool call and changes nothing. The name stays free.

The post-await re-check is the exception, and it is worth being precise about because the
acceptance list would otherwise overclaim. By the time control returns from
`build_subagent_agent`, the agent has been built and the task is about to be spawned; a refusal
there has to `task.abort()` the way the existing duplicate-name path already does. Nothing is
*registered*, and nothing the subagent could do is observable, but "nothing is spawned" is only
true of the pre-check. In practice the second path should be unreachable: the concepts page states
that a parent's own tool calls stay sequential and ordered, so one registry sees one launch at a
time. That makes this defense-in-depth rather than a live race -- but match the existing duplicate
check's belt-and-braces rather than reasoning that the guarantee makes it unnecessary.

## Acceptance

- A session with the default limit behaves as today for any fan-out under it, including the
  three-way example in the concepts doc.
- Launching past the limit is refused with a message naming the limit and
  `outrig__subagent_release`; nothing is registered, and on the pre-check path nothing is spawned.
- Releasing a subagent frees a slot, and a subsequent launch succeeds.
- The limit is per-registry: a subagent at its own limit does not prevent its parent from
  launching, and vice versa.
- `subagent-width-max` is settable top-level and per-agent, with the per-agent value winning, and
  out-of-range values are a config error.
- Idle-but-uncollected subagents count against the limit -- pinned by a test, since this is the
  part that surprises.

## Design forks

Each item leads with its status: **Resolved** (committed here), **Recommended** (a lean that a
prototype should confirm), or **Open** (deferred).

1. **Scope of the count -- Resolved: per registry.** See above. Session-wide is rejected as
   undiagnosable from the launching agent's position.

2. **Default value -- Recommended: 8.** Comfortably above the documented three-way fan-out and
   above any hand-written example, low enough that a model looping on `outrig__subagent` hits it
   quickly. Worth a sanity check against a real wide-fan-out session before committing, since the
   number should be "generous but finite", and picking it too low turns a supported pattern into
   an error. As with depth, a low value should remain meaningful: `subagent-width-max = 1`
   serializes fan-out without disabling subagents.

   Multiply it out before committing, because the interaction with depth is not gentle. At width
   8 and the default `subagent-depth-max = 3`, the session registry launches at depth 2 and those
   subagents each get their own registry (`2 < 3`), while depth-3 subagents do not -- so the
   permitted tree is 8 + 64 = **72 live subagents**, each holding clones of the session's MCP
   tools and each a node in the shutdown walk. That is the number to weigh against
   `SHUTDOWN_GRACE` below, and the reason the default is a real decision rather than a formality.

3. **Refuse or queue -- Resolved: refuse.** Queueing would make `outrig__subagent` block, breaking
   the "launching is not waiting" property the tool contract is built on and the docs call out by
   name. A refusal keeps launch non-blocking and hands the decision back to the agent, which is the
   only party that knows which of its subagents is now expendable.

4. **Auto-release of collected idle subagents -- Open.** Much of the pressure comes from models
   not releasing what they have finished with. Reclaiming a subagent that is idle and whose result
   has been collected would relieve it, but it breaks the "subagents stay addressable" guarantee --
   `outrig__subagent_send` on a reaped handle would fail where the docs promise it works. Possibly
   a per-launch opt-in (`"transient": true`) rather than a global behavior change. Deferred, since
   it is a contract change and the cap is not.

5. **Counting *running* rather than *live* -- Open.** A limit on concurrently *executing* rounds
   would target provider rate limits more precisely and would not require releasing. It is a
   different feature -- concurrency throttling, not resource bounding -- and it does not bound the
   handle set that shutdown has to walk. If provider rate limiting turns out to be the real pain,
   this is the fork to revisit.

## Risks

- **A too-low default turns a documented pattern into an error.** The concepts page teaches
  fan-out; the cap must not contradict it. Whatever default is chosen, the doc's own example must
  stay comfortably under it, and the acceptance list pins that.
- **`SHUTDOWN_GRACE` is a single budget for the whole tree.** A cap makes the worst-case tree
  smaller, which helps, but the five-second budget is still shared. If the chosen width and depth
  admit a tree large enough to blow it, teardown proceeds with tasks holding tool clones -- the
  Ctrl-C hang shape. At the recommended width 8 and default depth 3 that worst case is 72
  subagents aborted and joined bottom-up inside five seconds. Measure it rather than assuming:
  if it does not fit, the answer is a lower default width, not a longer grace, since the grace
  exists to bound a wedged subagent rather than to absorb a large healthy one.
- **The limit is invisible until it is hit.** An agent has no way to ask how many slots remain.
  The refusal is the only signal, which is acceptable for a bound that is rarely reached but argues
  for the message being unusually clear about the remedy.

## Dependencies

- **0098.** Both edit `crates/outrig-cli/src/subagent/mod.rs`; that task makes `release`
  resolve-before-mutate, this one adds a check to `launch`'s two lock sites. Landing the small
  one first keeps the two locking changes reviewable apart.

Model selection (see also) is queued immediately after this task, deliberately: it makes wide
fan-out more expensive rather than merely slower, so the containment half belongs first.

## See also

- `doc/concepts/subagents.md` -- fan-out, release semantics, and the addressability guarantee
  fork 4 would bend.
- `doc/reference/config.md` -- `subagent-depth-max`, whose config, merge, and validation shape
  this key mirrors.
- `plan/todo/0100-subagent-model-selection.md` -- launching a subagent under a different model.
