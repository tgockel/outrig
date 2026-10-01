# The launcher declares C `open` without its variadic tail

## Symptom

`crates/outrig/src/container/enter/launcher.rs` declares

```rust
fn open(path: *const c_char, flags: c_int) -> c_int;
```

C's `open` is variadic: `int open(const char *path, int flags, ...)`. A pre-merge review of
`0003-13` reports that Rust 1.99's `invalid_runtime_symbol_definitions` check rejects the
declaration. With `OUTRIG_REQUIRE_ENTER=1`, compiling the launcher then aborts the build. That
blocks both architecture builds and the `package` and `live-e2e` CI jobs.

The review found the base commit fails the same way, so this predates `0003-13` and was left out of
it. Not reproduced here: this machine has Rust 1.98.1, which accepts the declaration.

## Shape

Declare the real signature, `fn open(path: *const c_char, flags: c_int, ...) -> c_int;`. Every
call passes `O_RDONLY` and no mode, which a variadic declaration accepts as written. The review
reports that this compiles for both supported musl targets.

## Acceptance

- The launcher builds under `OUTRIG_REQUIRE_ENTER=1` on Rust 1.99 for both musl targets, and still
  builds on the workspace's `rust-version`.
- The `live-e2e` job is green again.
