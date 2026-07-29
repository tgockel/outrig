# 0099 -- Releasing subagents is all-or-nothing in its report but not in its effect

## Context

`SubagentRegistry::release` takes a list of names and walks it, removing and aborting each entry as
it goes:

```rust
for name in names {
    let entry = entries.remove(name).ok_or_else(|| unknown_name(name, &entries))?;
    entry.abort_tree();
    released.push(name.clone());
}
```

The `?` abandons the walk on the first unknown name, discarding `released` along with it. So
`outrig__subagent_release({"names": ["audit-a", "typo"]})` stops `audit-a` -- permanently, its name
freed and its tree cancelled -- and then reports only `no subagent named "typo"; live: ...`. The
launching agent is told its release failed, and the natural next move is to retry the same call or
to go on addressing `audit-a`. Both now fail, and neither error explains that the first name was
in fact acted on.

This is not the shutdown-reap problem fixed alongside it: the released tasks are correctly reaped
either way. It is that the tool's *report* claims an atomicity its effect does not have. A model
cannot recover from a state it was told does not exist.

The same shape does not arise in the other list-taking tool. `wait_results` resolves every name
under one lock before it blocks, so an unknown name there is a pure rejection.

## Goal

Make `release` either fully atomic or honestly partial, so the agent's next move is never based on
a report that contradicts the registry.

## Deliverables

- Resolve every name before mutating anything: check the whole list against `entries` under the
  lock, return `unknown_name` for the first miss, and only then remove and abort. This is the
  `wait_results` shape, and it makes the tool atomic rather than merely better-reported.
- A test that a failed release leaves *every* named subagent live and addressable -- specifically
  that the valid name in a partly-invalid list is still collectable afterwards.
- Consider whether the error should name all unknown names rather than the first. A model that
  typed two bad handles otherwise pays two round trips to learn it.

## Acceptance

- `release` with any unknown name in the list changes nothing: every valid name in that call stays
  live, keeps its inbox and watermark, and can still be sent to and collected from.
- `release` with all names valid behaves exactly as today.
- The error still lists the live names, as the other unknown-name diagnostics do.

## Design note

Atomic-reject is the right default over report-what-happened. Release is cheap to retry once the
agent fixes the name, whereas a partial release is unrecoverable -- the subagent and its history
are gone. The asymmetry favors doing nothing on a bad list.

## Decisions

1. **A duplicate name rejects the call rather than releasing once.** Resolving names against
   `entries` before mutating is not on its own enough: `["audit", "audit"]` passes a preflight
   that only checks membership, and then the second `remove` finds nothing. Deduplicating
   silently would have worked, but a list naming the same handle twice is a mistake in the
   caller's bookkeeping, and release is exactly the operation where acting on a list the caller
   did not mean is unrecoverable. Rejecting also preserves the old behavior in kind -- a repeat
   was already an error, just a partial one -- so the only thing that changed is that it no
   longer takes the subagent down on the way out.

   That is what lets the removal loop `expect` instead of propagating: with duplicates rejected,
   the preflight is a real proof that every `remove` finds its entry, so the second loop has no
   error path to report and no half-applied state to describe.

2. **The unknown-name error still names only the first miss.** Deliverable 3 asked whether it
   should collect them all. It reuses the shared `unknown_name` helper, so a bad handle reads
   the same whether it came from `release`, `get_result`, or `send` -- one diagnostic shape to
   learn instead of three. The cost the task worried about is now much smaller than when it was
   written: because release is atomic, retrying after fixing one name is free, where before the
   retry was against a registry that had already lost a subagent.

3. **The acceptance test sends before it collects.** Acceptance asks that a valid name in a
   rejected list stay addressable, and `get_result` alone does not show that -- a result can be
   collectable from a subagent whose prompt channel is gone. `send` exercises the half that
   `abort_tree` would have taken.

## Dependencies

- **0092**, for the e2e suite to compile, so the acceptance test can actually be run.

Otherwise none. The width cap (see also) shares `crates/outrig-cli/src/subagent/mod.rs` with this
task and is queued immediately after: this one touches `release`, that one touches `launch`'s
locking discipline, so landing the smaller change first keeps the two reviewable apart.

## See also

- `crates/outrig-cli/src/subagent/mod.rs` -- `release`, and `wait_results` for the resolve-first
  pattern to copy.
- `crates/outrig-cli/src/builtin_tool.rs` -- `SubagentReleaseTool`, which surfaces the message.
- `plan/todo/0100-subagent-width-cap.md` -- also touches `launch`'s locking discipline; worth
  landing in the same pass if both are picked up.
