# Re-check 0090's config surface against the landed `args` work

## Context

0088 widened entrypoint-stdio to cover named sidecar blocks: an entry with no `command` is the
entrypoint form whether it names an inline `image` or a `sidecar`. Two things in
`plan/todo/0090-primary-view-sidecars.md` were written before that and want a second look.

**Its `## Config surface` example is now legal**, where it previously contradicted validation:

```toml
[images.coding.sidecars.tools]
image = "docker.io/mcp/filesystem:latest"
view  = "primary"

[images.coding.mcp]
fs = { sidecar = "tools", args = ["/workspace"] }
```

`sidecar` without `command` used to be `McpSidecarRequiresCommand`; it is now the named
entrypoint-host form, and `args` on the entry is accepted (the block may declare it instead,
but not both). No change needed -- but the task text still reads as if it were describing
something 0088 would have to enable.

**Its scope note may be over-restrictive.** 0090 says exec-stdio hosts "get a validation error"
under `view = "primary"`, which was the right call when the only entrypoint form was the inline
one-liner and `view` had nowhere to live. Now that a named block can be an entrypoint host, the
restriction and the `view`-on-the-inline-form deliverable are both worth re-deriving: if `view`
only ever applies to entrypoint hosts, it may not need to exist on the inline form at all, and
the "one-liner worth putting in the quickstart" argument changes shape.

## Goal

Reconcile 0090's Config surface, Deliverables, and Validation table with the placement rules as
they now stand, before 0090 is executed.

## Notes

This is a plan-file edit, not code. Fold it into 0090 during `/groom-plan` or at the start of
0090's own branch rather than carrying it as a separate task.

## Dependencies

- 0088 (landed).
