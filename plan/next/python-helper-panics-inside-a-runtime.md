# The test helper `python()` panics when an async test calls it first

## Context

`crates/outrig/src/python/testing.rs`'s `python()` unpacks the embedded payload once per test
binary, on a current-thread runtime of its own built and blocked on inside a `OnceLock`. Called
first from a `#[tokio::test]` -- one already inside a runtime -- it panics with "Cannot start a
runtime from within a runtime". Which test calls it first depends on the filter and the order
tests start in: `cargo test -p outrig --features outrig/e2e --lib -- e2e` fails
`python::supervisor_tests::e2e::the_binding_cache_mounted_read_only_imports_in_a_container`
this way, while the same test passes when a synchronous test of its module runs first, as in the
full suite. Found while running `0003-24`'s e2e tests; nothing there changed the helper.

## Options

- Unpack on a thread of its own -- `std::thread::scope(...).join()` -- as `bindings_tests.rs`'s
  `wheels()` does for the same reason, so the helper works from either kind of test.
- Make the async callers use an async form of the helper.

## Acceptance

- The e2e-filtered run above passes, and so does each `#[tokio::test]` that calls `python()` run
  alone.
