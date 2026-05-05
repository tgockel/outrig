# 0046 -- Pick a license

## Goal

Choose an open-source license for OutRig and ship it. Today the README carries a
`> TODO: Incomplete` block at line 50 saying the license isn't chosen; that needs to
become a real `LICENSE` file plus a normal short License section before we're public.

## Deliverables

- Decide on a license. Plausible options: MIT, Apache-2.0, dual MIT/Apache-2.0 (the
  Rust-ecosystem norm), MPL-2.0. The choice is the user's call.
- `LICENSE` (or `LICENSE-MIT` + `LICENSE-APACHE` for dual) at the repo root, with the
  canonical license text and a reasonable copyright line.
- Drop the `> TODO: Incomplete` block at `README.md:50` and replace it with a normal
  short "## License" section pointing at `LICENSE`. For dual licensing, the standard
  Rust-ecosystem note: "Licensed under either of MIT or Apache-2.0 at your option."
- Add `license = "..."` (or `license-file = "..."`) to `Cargo.toml` so
  `cargo publish --dry-run` is happy.

## Acceptance

- `LICENSE` (or the per-license pair) exists at the repo root.
- README has a real License section; no `> TODO: Incomplete` block remains in the file.
- `cargo publish --dry-run --allow-dirty` is clean re: license metadata.

## Dependencies

None. Can run any time.

## Notes

- Tracked here because task 0034 dropped the README's "implementation in progress"
  disclaimer but explicitly left the License `Incomplete` row alone.
