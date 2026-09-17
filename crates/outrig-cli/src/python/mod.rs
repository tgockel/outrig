//! The agent's Python execution environment.
//!
//! A static CPython is bind-mounted read-only into the primary container and one kernel process
//! per session runs under it, holding the agent's globals, its event loop, and its channel
//! endpoints. The model acts by submitting source to that kernel rather than by calling MCP
//! tools.
//!
//! This is prototype work. It implements the persistent session, top-level await, bounded
//! observations, the `user` channel, and the interruptible wait; it does not implement
//! subagents, cross-VM dataclass contracts, the variable-edit interface, AArch64, or any
//! watchdog.

pub mod console;
pub mod kernel;
pub mod payload;
pub mod prompt;
pub mod tool;

pub use kernel::PythonKernel;
pub use payload::{PAYLOAD_MOUNT, payload_dir};
pub use prompt::PREAMBLE;
pub use tool::PythonExecuteTool;
