//! The `IoPathExt` seal, asserted from outside the crate.
//!
//! `crates/outrig/public-api.txt` records the private supertrait bound, but a
//! snapshot line cannot tell a seal from the orphan rules: an out-of-crate
//! `impl` of a foreign trait for a foreign type is rejected either way. The
//! case below implements it for a type the case file itself defines, so the
//! orphan rules do not apply and the seal is the only thing left to reject it
//! -- which is why the golden `.stderr` is worth its maintenance.
//!
//! Behind the `seal-tests` feature, because trybuild builds its cases in a
//! scratch project with a target directory of its own -- around 2 GB, sharing
//! nothing with `target/`, and running `build.rs`'s musl launcher a second
//! time. `.github/workflows/ci.yml` runs it on the `default` row alone. Run it
//! locally, and regenerate the golden after a deliberate change to the seal or
//! a rustc release that rewords `E0277`, with:
//!
//! ```sh
//! cargo test -p outrig --features seal-tests --test compile_fail
//! TRYBUILD=overwrite cargo test -p outrig --features seal-tests --test compile_fail
//! ```

#[test]
fn io_path_ext_is_sealed_against_an_outside_impl() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
    // A seal that also breaks ordinary use is not a seal, it is a regression.
    t.pass("tests/ui/pass/*.rs");
}
