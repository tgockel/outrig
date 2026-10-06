//! The agent's Python: the static interpreter every session mounts, the
//! program it runs, and the host's half of the protocol between them.
//!
//! `interpreter.py` is that program: one process per session, hosting one
//! kernel per agent and answering over NDJSON by agent id. `host` starts it
//! in a session's container, submits executions, and correlates the replies.
//! `recovery` waits for an execution's outcome, and cancels or interrupts one
//! that will not end on its own -- because the user asked, or because it is
//! spinning.
//!
//! Crate-private throughout. `outrig-cli` reaches none of it directly: the
//! agent loop in `crate::agent` drives it, and `PythonAgent` is how the binary
//! reaches that.

pub(crate) mod host;
pub(crate) mod payload;
pub(crate) mod recovery;

#[cfg(test)]
pub(crate) mod relay;
#[cfg(test)]
pub(crate) mod testing;

// What `build.rs` checks the payload with before embedding it. Shared by
// `include!` rather than a module, since a build script cannot link the crate
// it builds; compiled here only for its tests.
#[cfg(test)]
mod archive {
    use crate::container::enter::elf::{ElfKind, elf_interp};
    include!("archive.rs");
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod host_tests;

#[cfg(test)]
#[path = "interpreter_tests.rs"]
mod interpreter_tests;

#[cfg(test)]
#[path = "binding_tests.rs"]
mod binding_tests;
