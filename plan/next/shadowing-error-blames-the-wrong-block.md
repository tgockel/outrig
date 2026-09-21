# A shadowed built-in default blames a sidecar block that may not exist

## Problem

`builtin_image::inject` steps aside when the config declares any of five reserved names:
`[images.outrig-default]`, `[images.outrig-default-fs]`, `[images.outrig-default-shell]`,
`[sidecars.outrig-default-fs]`, or `[sidecars.outrig-default-shell]`. Of those, only an
`[images.outrig-default]` of the user's own leaves something to resolve; the other four leave
`Injection::resolved` as `None`.

When that happens with no `--image` and no `default-image`, `session_setup` raises:

```
no --image or default-image configured, and outrig's built-in default is shadowed by a
[sidecars.<name>] block using one of its reserved names
```

Three of the four vetoing names are **image** blocks, so the message names a block the user
never wrote. A repo declaring only `[images.outrig-default-fs]` is told to go look for a
sidecar. Worse, it contradicts the note printed immediately above it, which `shadow_note`
composes from the actual shadowing name -- so the session emits two lines that disagree about
what the user did.

`doc/usage/mcp.md` and `doc/reference/cli.md` now describe the real conditions (0002-49), which
leaves the error as the only artifact still claiming the sidecar-only rule.

## Sketch

Two shapes, either fine:

- Carry the reason out of `inject` rather than re-deriving it. `shadow_note` already knows
  which name shadowed and whether it was an image or a sidecar; `Injection` could hold that
  instead of only the note string, and the error could name it directly.
- Or widen the message to cover both kinds without naming which, and let the preceding note
  carry the specifics. Cheaper, and the note is already printed.

The first is better: an error that names the block is what the user needs, and the note is a
`[outrig]` line they may not associate with the failure.

Leave the selection logic alone -- it is correct. The *behavior* of reserved names is
revisited in `plan/next/builtin-image-nameable-as-default.md`, if it ever is.

## Dependencies

- None. `crates/outrig-cli/src/cli/session_setup.rs` and `src/builtin_image/mod.rs`.
