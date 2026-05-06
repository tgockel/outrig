# Clean up `cargo doc --no-deps` private-link warnings

## Goal

`cargo doc --no-deps` currently emits 5 warnings about public docs linking to
private items. These predate the outrig-mcp work and are unrelated to it; they
break the spirit of any task that lists "`cargo doc --no-deps` clean" as
acceptance.

## Warnings (as of trunk)

```
warning: public documentation for `container`           links to private item `TRACKED`
warning: public documentation for `install_panic_hook`  links to private item `TRACKED`
warning: public documentation for `shutdown`            links to private item `SHUTDOWN_GRACE`
warning: public documentation for `format_started_at`   links to private item `iso_systime`
warning: public documentation for `sanitize`            links to private item `MAX_NAME_LEN`
```

## Deliverables

For each warning, pick one of:

- Demote the doc link to plain code-formatted text (back-ticks instead of an
  intra-doc link), if the cross-reference is informational only.
- Promote the referenced item from private to `pub(crate)` and re-link, if the
  identifier is genuinely useful to a reader of the public docs.
- Reword the doc comment to drop the dangling reference.

Whichever path each warning takes, `cargo doc --no-deps` finishes clean
afterwards.

## Acceptance

- `cargo doc --no-deps` exits zero with no warnings.
- `cargo build` / `cargo test` / `cargo clippy` unchanged.

## Notes

- Discovered while landing 0038. The warnings are independent of any rmcp
  feature change.
- Touching `MAX_NAME_LEN` should not change `tool_name::sanitize` behaviour --
  it is a documentation-only edit.
