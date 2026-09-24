//! The agent's Python: the static interpreter every session mounts, the
//! program it runs, and the host's half of the protocol between them.
//!
//! `interpreter.py` is that program: one process per session, hosting one
//! kernel per agent and answering over NDJSON by agent id. `host` starts it
//! in a session's container, submits executions, and correlates the replies.
//!
//! Crate-private throughout. `outrig-cli` reaches none of it directly; the
//! phase's one public entry point is what drives it.

// Nothing drives the interpreter until the agent loop does, and among the
// tests only the e2e ones start it in a container. Any one dead item fulfills
// the expectation, so it holds until the last of them has a caller -- and then
// fails the build rather than lingering as an `allow` would.
#[cfg_attr(
    not(all(test, feature = "e2e")),
    expect(dead_code, reason = "the agent loop is its first caller")
)]
pub(crate) mod host;
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
#[path = "host_tests.rs"]
mod host_tests;

#[cfg(test)]
#[path = "interpreter_tests.rs"]
mod interpreter_tests;
