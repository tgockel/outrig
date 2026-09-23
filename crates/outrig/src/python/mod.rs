//! The agent's Python: the static interpreter every session mounts, and (in
//! later work) the process that runs under it.
//!
//! Crate-private throughout. `outrig-cli` reaches none of it directly; the
//! phase's one public entry point is what drives it.

pub(crate) mod payload;

// What `build.rs` checks the payload with before embedding it. Shared by
// `include!` rather than a module, since a build script cannot link the crate
// it builds; compiled here only for its tests.
#[cfg(test)]
mod archive {
    use crate::container::enter::elf::{ElfKind, elf_interp};
    include!("archive.rs");
}
