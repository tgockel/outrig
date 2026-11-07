# clippy's `drop_non_drop` fires on `paths.rs`'s `drop(check)`

`cargo clippy --workspace --all-targets -- -D warnings` fails as of this container's
toolchain (`rustc`/clippy 1.98) with:

```
error: call to `std::mem::drop` with a value that does not implement `Drop`. Dropping such
       a type only extends its contained lifetimes
   --> crates/outrig-cli/src/paths.rs:779:5
    |
779 |     drop(check);
    |     ^^^^^^^^^^^
    = note: `-D clippy::drop-non-drop` implied by `-D warnings`
```

`check` there is a closure, not a `Drop` type, so clippy's newer `drop_non_drop` lint (not
present, or not yet linting this shape, in whatever clippy pinned CI last ran) now flags it.
Confirmed present on `trunk` as of `a26b2efc`, unrelated to any in-flight task.

Fix options: rename the call site to make the intent (holding `check` alive to its scope's
end, not actually dropping a resource) explicit -- e.g. `let _ = check;` -- or add a scoped
`#[allow(clippy::drop_non_drop)]` with a comment saying why. Worth checking whether CI's
pinned clippy version actually lints this before spending the diff; if CI doesn't, this may
only bite local runs against a newer toolchain image.
