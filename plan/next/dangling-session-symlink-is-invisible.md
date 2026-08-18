# A dangling session symlink is invisible to every command

`SessionStore::list` skips an entry whose `session.json` read fails with
`ErrorKind::NotFound`:

```rust
Err(OutrigError::Path { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => continue,
```

That arm is meant for foreign content under the session root (a stray directory that
isn't a session at all). But it also swallows a genuinely broken case: a
`<root>/<sid>` symlink created by `outrig run --session-dir` whose target directory
has since been deleted. The read of `<target>/session.json` fails with `NotFound`, so
the entry produces no row *and* no skip:

- `outrig ls` shows nothing.
- `outrig clean` never sweeps the dangling link.
- `rm` by hand is the only recovery.

## Sketch

`resolve_entry` already distinguishes the two shapes -- it returns
`(target, Some(target))` for a symlink and `(path, None)` for a directory. Use that:
when the entry is a symlink and its target is missing, report it through
`SessionListing::skipped` (reason: dangling link, naming the target) instead of
silently continuing. A plain directory with no `session.json` keeps skipping quietly.

## Status

Pre-existing, not introduced by the `skipped` work -- this arm is byte-for-byte
identical to what shipped before, and `ls` printed `[outrig] no sessions` with exit 0
in this case then too. Filed here because the skip channel that would carry it now
exists, which makes the fix cheap. Pairs with
[unreadable-session-records-are-unremovable](unreadable-session-records-are-unremovable.md):
both are "the store knows about an entry the user can't act on."
