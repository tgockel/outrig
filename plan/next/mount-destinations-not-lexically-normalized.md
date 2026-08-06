# Mount destination collision checks don't normalize `..`

## Context

`check_mount_list` (`crates/outrig/src/config/validate.rs`) rejects duplicate container paths by
inserting each `container_path` into a `BTreeSet` verbatim. The comparison is textual, so parent
components slip through:

```toml
[workspace]
container-path = "/workspace"

[[workspace.mounts]]
host-path      = "resources"
container-path = "/workspace/../workspace"
```

Both destinations normalize to `/workspace`, but the strings differ, so outrig's validation
passes and podman fails at launch instead -- after the image is built and the session directory
is created. The same textual comparison backs the `container_path == "/"` root check, which
`/a/..` also evades.

Pre-existing; found while reviewing 0108, which touched `validate.rs:1027` and `:1076` only to
move from a field read to an accessor. Not caused or worsened by that change.

## Goal

Compare container destinations the way the container runtime will, so a collision is a config
error with a config error message.

## Deliverables

- Lexical normalization (resolve `.` and `..` textually, no filesystem access -- these are
  in-container paths that do not exist on the host) applied before the duplicate check, the
  root check, and the absoluteness check in `check_mount_list`.
- The same normalization applied to the primary `[workspace].container-path` that seeds the
  reserved set, at `validate.rs:1027` and `:1076`.
- Diagnostics keep naming the path as the user wrote it, not the normalized form -- the
  normalized value is for comparison only.

## Acceptance

- A repo config declaring `/workspace` and `/workspace/../workspace` fails
  `validate_workspace_mounts` with the existing duplicate-container-path error, naming the
  declaring file.
- `/a/..` is rejected by the root check.
- A path with no parent components produces byte-identical diagnostics to today.

## Design forks

1. **Whether to reject `..` outright instead of normalizing -- Open.** A container destination
   containing `..` has no legitimate use, so refusing it is simpler than normalizing and gives a
   clearer message. Normalizing is more permissive and matches what podman does. Rejecting is
   the smaller change and the easier rule to document; it is also a new error for configs that
   are currently accepted-then-broken, which is a strict improvement over failing at launch.

## Dependencies

None. Self-contained in `validate.rs`.

## See also

- `crates/outrig/src/config/validate.rs` -- `check_mount_list` and its two callers.
- `doc/reference/config.md` -- "Validation rules", which documents the duplicate rule and would
  gain the normalization sentence.
