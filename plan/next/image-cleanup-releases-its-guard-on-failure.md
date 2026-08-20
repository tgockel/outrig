# A failed image cleanup still disarms its retry guard

Pre-existing, and unchanged by 0116: `cleanup_builder` and `cleanup_temp_image`
(`crates/outrig/src/image.rs`) both discard the removal's outcome --

```rust
let _ = process::try_capture(cmd).await;
```

-- and every caller then calls `release()` on the `CleanupGuard` unconditionally
(`build_image_with_build_args`, `commit_image_with_labels`). So a `buildah rmi` or
`buildah rm` that failed for a transient reason -- a busy image, an engine hiccup, a
cleanup racing another process -- leaves the tag or the working container behind *and*
disarms the detached retry the guard exists to be.

The guard is the layer that is supposed to survive exactly this, so releasing it on the
strength of "the command returned" rather than "the resource is gone" is the one shape it
must not have.

## Sketch

Have both helpers report whether the resource is gone -- removed, or already absent, which
`buildah rmi` reports distinguishably -- and release the guard only on that. A failure
leaves it armed, and the guard's `Drop` re-issues the removal through
`supervise::detach_cleanup`, which is the fallback the arming was for.

Cheap to test with the same shell fake `tests/cancellation.rs` installs: steer `rmi` to
fail, and assert the detached removal happens anyway.
