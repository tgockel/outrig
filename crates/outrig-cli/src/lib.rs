//! Internals of the `outrig` CLI binary, exposed as a library so the
//! integration tests in `tests/` can reach them. End users should depend
//! on the [`outrig`] crate (the library) instead.

pub mod builtin_tool;
pub mod cli;
pub mod config_init;
pub mod error;
pub mod hf;
pub mod image_setup;
pub mod init;
pub mod llm;
pub mod mcp_self;
pub(crate) mod paths;
pub mod repl;
pub mod rig_tool;
pub mod session;
pub mod session_tool;
pub mod subagent;
