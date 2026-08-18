# A stale session record isn't `OutrigError::Configuration`

`read_session_json` classifies an unparseable `session.json` as
`OutrigError::Configuration`, which renders as `configuration: ...` -- i.e. "the user
misconfigured something". But a stale record is data *outrig itself wrote* under an
older schema; the user configured nothing.

`SessionStore::list` works around the wrong classification by unwrapping the variant to
strip the framing before storing a skip reason:

```rust
reason: match e {
    OutrigError::Configuration(msg) => msg,
    other => other.to_string(),
},
```

That works (and is pinned by `list_skips_unparseable_session_json`), but it couples
`list` to whichever variant `read_session_json` happens to construct, and every future
consumer of a skip reason either repeats the unwrap or leaks `configuration:` into its
output.

## Sketch

Add `OutrigError::SessionRecord { path, source }`, return it from `read_session_json`,
store it typed in `SkippedSession`, and format it at the CLI edge. Contained to
`session.rs` plus the error enum -- and it also supplies the typed error that
[unreadable-session-records-are-unremovable](unreadable-session-records-are-unremovable.md)
needs in order to act on a skip.
