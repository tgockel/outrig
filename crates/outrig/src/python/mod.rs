//! The agent's Python: the static interpreter every session mounts, and the
//! program it runs.
//!
//! `interpreter.py` is that program: one process per session, hosting one
//! kernel per agent and answering over NDJSON by agent id. The host side that
//! starts it and correlates its replies is later work, so for now only its
//! tests read it.
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

#[cfg(test)]
#[path = "interpreter_tests.rs"]
mod interpreter_tests;
