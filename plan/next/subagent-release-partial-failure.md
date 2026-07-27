# Releasing subagents is all-or-nothing in its report but not in its effect

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

## See also

- `crates/outrig-cli/src/subagent/mod.rs` -- `release`, and `wait_results` for the resolve-first
  pattern to copy.
- `crates/outrig-cli/src/builtin_tool.rs` -- `SubagentReleaseTool`, which surfaces the message.
- `plan/next/subagent-width-cap.md` -- also touches `launch`'s locking discipline; worth landing
  in the same pass if both are picked up.
