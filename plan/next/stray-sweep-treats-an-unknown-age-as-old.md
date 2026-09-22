# A stray with no creation time is swept as if it were ancient

`parse_all_containers` (`crates/outrig-cli/src/cli/clean.rs`) decodes each `podman ps -a` row's
`Created` field and falls back to `SystemTime::UNIX_EPOCH` when it is missing or not an integer:

```rust
let created = row
    .get("Created")
    .and_then(|created| created.as_i64())
    .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64))
    .unwrap_or(SystemTime::UNIX_EPOCH);
```

`classify_strays` then asks whether `now - created >= older_than`. Against the epoch that is
true for every cutoff, so a labeled stray whose creation time podman did not report is
**removable regardless of `--older-than`** -- including one created a second ago.

The inverse of the rule the build-container sweep added in 0002-50, where an age that cannot be
established is reported and skipped precisely because it cannot be measured against the cutoff.
The two sweeps should agree, and the build-container one has the defensible half.

## Why it was not fixed there

Changing it alters what the *existing* stray sweep removes, which is outside a task about
buildah working containers. It is also not purely additive: a row with no `Created` is currently
collected, and after the fix it would be reported instead, which is a behavior change users
could notice.

## Sketch

Make `LabeledContainer::created` an `Option<SystemTime>`, have `classify_strays` route `None`
into a third bucket alongside `running`, and report it the way
`write_build_undatable` reports its own. `crates/outrig-cli/src/cli/build_containers.rs` already
has the shape to copy.

## Notes

- Worth checking first how often podman actually omits `Created` -- it may be rare enough that
  the fix is purely defensive, which is still worth having but changes the urgency.
- `crates/outrig-cli/tests/session_cli.rs` has the fixtures to extend; see
  `clean_removes_old_stopped_stray_containers`.
