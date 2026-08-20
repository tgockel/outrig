# Private intra-doc links have crept back in

`plan/done/0052-cargo-doc-private-link-warnings.md` demoted every public-item doc
link that pointed at a private symbol to code-formatted text, on the grounds that
the targets are implementation details rather than reader-facing anchors. Three
have since reappeared. `cargo doc -p outrig` reports them as
`rustdoc::private_intra_doc_links`:

- `crates/outrig/src/error.rs:85` -- `OutrigError::Spawn` links to
  `crate::process::Cmd::render`.
- `crates/outrig/src/container/mod.rs:629` -- `userdb::home_dir` links to
  `Container::build_exec_argv`.
- `crates/outrig/src/container/mod.rs:645` -- `Container::bootstrap_user` links to
  the private `namespace` module.

Two unrelated `broken_intra_doc_links` warnings sit beside them, from prose that
means to name files rather than items:

- `crates/outrig/src/container/enter/mod.rs:7` -- `` [`launcher.rs`] `` and
  `` [`elf`] ``.

Five one-line fixes, all the same shape 0052 already settled. Noticed while
landing 0116, which added a sixth of the same kind and fixed only its own --
touching the others would have put unrelated doc edits in a cancellation diff.

Worth pairing with a CI guard, since a landed decision that nothing enforces is
one that regresses silently: `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` in
the `doc-style` job would catch all of these. That guard is the reason to do this
as its own change rather than opportunistically.
